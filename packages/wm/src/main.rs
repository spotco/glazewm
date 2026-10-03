// The `windows` or `console` subsystem (default is `console`) determines
// whether a console window is spawned on launch, if not already ran
// through a console. The following prevents this additional console window
// in release mode.
#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]
#![warn(clippy::all, clippy::pedantic)]
#![feature(iterator_try_collect)]

#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::{env, path::PathBuf, process, time::Duration};

use anyhow::{Context, Error};
use tokio::{process::Command, signal};
use tracing::Level;
use tracing_subscriber::{
  fmt::{self, writer::MakeWriterExt},
  layer::SubscriberExt,
};
use wm_common::{AppCommand, InvokeCommand, Verbosity, WmEvent};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  Dispatcher, DisplayListener, EventLoop, KeybindingListener,
  MouseEventKind, MouseListener, PlatformEvent, SingleInstance,
  WindowListener,
};

use crate::{
  commands::general::{
    dump_wm_state_with_dialog, layout_debug_log_path,
    layout_snapshot_path, load_layout_snapshot, pick_layout_snapshot_path,
    platform_sync, read_layout_snapshot_file,
    save_layout_snapshot_with_dialog, set_layout_debug_log_path,
    set_verbose_z_order_from_config, show_desktop,
    try_load_persisted_layout_snapshot, verbose_z_order_enabled,
    wm_event_affects_layout_snapshot, LayoutAutoSave,
  },
  ipc_server::IpcServer,
  sys_tray::SystemTray,
  user_config::UserConfig,
  wm::WindowManager,
};

mod commands;
mod diagnostic_history;
mod events;
mod ipc_conflict;
mod ipc_server;
mod models;
mod pending_sync;
mod sys_tray;
mod traits;

use crate::traits::WindowGetters;
mod user_config;
mod wm;
mod wm_state;

/// Main entry point for the application.
///
/// Conditionally starts the WM or runs a CLI command based on the given
/// subcommand.
fn main() -> anyhow::Result<()> {
  let args = std::env::args().collect::<Vec<_>>();
  let app_command = AppCommand::parse_with_default(&args);

  if let AppCommand::Start {
    config_path,
    verbosity,
  } = app_command
  {
    let rt = tokio::runtime::Runtime::new()?;
    let (event_loop, dispatcher) = EventLoop::new()?;

    let task_handle = std::thread::spawn(move || {
      rt.block_on(async {
        let start_res =
          start_wm(config_path, verbosity, &dispatcher).await;

        if let Err(err) = &start_res {
          // If unable to start the WM, the error is fatal and a message
          // dialog is shown.
          tracing::error!("{:?}", err);
          dispatcher.show_error_dialog("Fatal error", &err.to_string());
        }

        if let Err(err) = dispatcher.stop_event_loop() {
          // Forcefully exit the process to ensure the event loop is
          // stopped.
          tracing::error!("Failed to stop event loop gracefully: {}", err);
          process::exit(1);
        }

        start_res
      })
    });

    // Run event loop (blocks until shutdown). This must be on the main
    // thread for macOS compatibility.
    event_loop.run()?;

    // Wait for clean exit of the WM.
    task_handle.join().unwrap()
  } else {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wm_cli::start(args))
  }
}

#[allow(clippy::too_many_lines)]
async fn start_wm(
  config_path: Option<PathBuf>,
  verbosity: Verbosity,
  dispatcher: &Dispatcher,
) -> anyhow::Result<()> {
  setup_logging(&verbosity)?;
  tracing::info!("startup: logging ready");

  // Ensure that only one instance of the WM is running.
  let _single_instance = SingleInstance::new()?;
  tracing::info!("startup: single instance lock acquired");

  #[cfg(target_os = "macos")]
  {
    if !dispatcher.has_ax_permission(true) {
      anyhow::bail!(
        "Accessibility permissions are not granted. In System Preferences, \
         go to Privacy & Security > Accessibility and enable GlazeWM."
      );
    }
  }

  // Parse and validate user config.
  let mut config = UserConfig::new(config_path)?;

  // Debug trail: layout.log beside config.yaml / layout.json.
  // Set early so IPC AddrInUse recovery can log before the listener binds.
  let layout_path = layout_snapshot_path(&config);
  let layout_log_path = layout_debug_log_path(&config);
  set_layout_debug_log_path(layout_log_path.clone());
  set_verbose_z_order_from_config(config.value.general.verbose_z_order);
  #[cfg(target_os = "windows")]
  wm_platform::set_native_op_logger(log_native_op_to_layout_log);
  tracing::info!(
    "Layout persistence debug log -> {}",
    layout_log_path.display()
  );
  startup_log(format!(
    "startup: config loaded from {}",
    config.path.display()
  ));
  startup_log(format!(
    "startup: layout log -> {}",
    layout_log_path.display()
  ));
  startup_log(format!(
    "startup: verbose_z_order={} (config={}, env_override={})",
    verbose_z_order_enabled(),
    config.value.general.verbose_z_order,
    std::env::var("GLAZEWM_VERBOSE_Z_ORDER").ok().as_deref() == Some("1"),
  ));

  #[cfg(target_os = "windows")]
  {
    startup_log("startup: cleaning taskbar helper windows");
    match dispatcher.cleanup_taskbar_helper_windows() {
      Ok(count) => {
        startup_log(format!(
          "startup: taskbar helper cleanup removed {count} window(s)"
        ));
      }
      Err(err) => {
        tracing::warn!(
          "Failed to clean shell/input-helper taskbar tabs: {err:?}"
        );
        startup_log(format!(
          "startup: taskbar helper cleanup failed: {err:?}"
        ));
      }
    }
  }

  // Add application icon to system tray.
  startup_log("startup: creating system tray");
  let mut tray = SystemTray::new(&config.path, dispatcher.clone())?;
  startup_log("startup: system tray ready");

  startup_log("startup: populating window manager state");
  let mut wm = WindowManager::new(&mut config, dispatcher.clone())?;
  startup_log(format!(
    "startup: window manager ready monitors={} windows={}",
    wm.state.monitors().len(),
    wm.state.windows().len()
  ));

  let mut ipc_server = IpcServer::start(dispatcher).await;
  if ipc_server.is_enabled() {
    startup_log("startup: IPC listener is accepting localhost clients");
  } else {
    startup_log("startup: IPC listener is disabled");
  }

  // On Windows, start watcher process for restoring hidden windows on
  // crash. macOS' hidden windows are always accessible. The watcher is
  // an IPC client, so it cannot run when IPC failed to bind.
  #[cfg(target_os = "windows")]
  if ipc_server.is_enabled() {
    startup_log("startup: launching watcher");
    if let Err(err) = start_watcher_process() {
      tracing::warn!(
        "Failed to start watcher process: {err}{}",
        cfg!(debug_assertions)
          .then_some(".\n Run `cargo build -p wm-watcher` to build it.")
          .unwrap_or_default()
      );
      startup_log(format!("startup: watcher failed: {err}"));
    } else {
      startup_log("startup: watcher launched");
    }
  } else {
    startup_log("startup: watcher skipped because IPC is disabled");
  }

  // On macOS, update the current process' PATH variable so that
  // `shell-exec` can resolve programs defined in the shell's PATH. Skip if
  // running via a terminal.
  #[cfg(target_os = "macos")]
  if !std::io::stdin().is_terminal() {
    update_path_env();
  }

  // Start listening for platform events after populating initial state.
  startup_log("startup: creating platform listeners");
  let mut window_listener = WindowListener::new(dispatcher)?;
  let mut display_listener = DisplayListener::new(dispatcher)?;
  let mut mouse_listener = MouseListener::new(
    if config.value.general.focus_follows_cursor {
      &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
    } else {
      &[MouseEventKind::LeftButtonUp]
    },
    dispatcher,
  )?;
  let mut keybinding_listener = KeybindingListener::new(
    &config
      .active_keybinding_configs(&[], false)
      .flat_map(|kb| kb.bindings)
      .collect::<Vec<_>>(),
    dispatcher,
  )?;
  startup_log("startup: platform listeners ready");

  // Run user's startup commands.
  startup_log(format!(
    "startup: running {} startup command(s)",
    config.value.general.startup_commands.len()
  ));
  if let Err(err) = wm.process_commands(
    &config.value.general.startup_commands.clone(),
    None,
    &mut config,
  ) {
    tracing::error!("{:?}", err);
    dispatcher.show_error_dialog("Non-fatal error", &err.to_string());
    startup_log(format!("startup: startup commands failed: {err:#}"));
  } else {
    startup_log("startup: startup commands finished");
  }

  // Best-effort restore of persisted layout.json (beside config.yaml).
  // Runs after initial populate + startup commands so windows/workspaces
  // exist. Windows still cloaked / not yet in visible_windows() at
  // populate() are absent from the first pass — schedule deferred
  // retries after the event loop can manage late arrivals
  // (WindowManaged), plus timed retries.
  startup_log("startup: layout restore starting");
  let first_load = try_load_persisted_layout_snapshot(
    &layout_path,
    &mut wm.state,
    &config,
  );
  let mut startup_layout_retries_left: u32 = if first_load
    .as_ref()
    .is_some_and(|s| s.unmatched_snapshot > 0)
  {
    4
  } else {
    0
  };
  if startup_layout_retries_left > 0 {
    crate::commands::general::layout_debug_log(format!(
      "startup load left unmatched_snapshot={}; scheduling up to {startup_layout_retries_left} retries (2s / WindowManaged)",
      first_load.as_ref().map_or(0, |s| s.unmatched_snapshot)
    ));
  }
  let startup_layout_retry_delay =
    tokio::time::sleep(Duration::from_secs(2));
  tokio::pin!(startup_layout_retry_delay);

  // Drain events emitted by startup restore so IPC clients see them, but
  // do not arm auto-save yet (avoids thrashing a rewrite of the file we
  // just loaded).
  while let Ok(wm_event) = wm.event_rx.try_recv() {
    if let WmEvent::PauseChanged { is_paused } = wm_event {
      let _ = mouse_listener.enable(!is_paused);
    }
    if matches!(
      wm_event,
      WmEvent::UserConfigChanged { .. }
        | WmEvent::BindingModesChanged { .. }
        | WmEvent::PauseChanged { .. }
    ) {
      keybinding_listener.update(
        &config
          .active_keybinding_configs(&wm.state.binding_modes, false)
          .flat_map(|kb| kb.bindings)
          .collect::<Vec<_>>(),
      );
      let _ = mouse_listener.set_enabled_events(
        if config.value.general.focus_follows_cursor {
          &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
        } else {
          &[MouseEventKind::LeftButtonUp]
        },
      );
    }
    if let Err(err) = ipc_server.process_event(wm_event) {
      tracing::error!("{:?}", err);
    }
  }

  // Startup event drain finished; allow layout auto-save scheduling.
  // Keep auto-save off until pending startup layout retries finish so we
  // do not persist the incomplete pre-retry arrangement over layout.json.
  crate::commands::general::layout_debug_log(
    "startup event drain complete; enabling layout auto-save",
  );
  let layout_path_for_retry = layout_path.clone();
  let mut layout_auto_save = LayoutAutoSave::new(layout_path);
  if startup_layout_retries_left == 0 {
    layout_auto_save.enable();
  } else {
    crate::commands::general::layout_debug_log(
      "auto-save deferred until startup layout retries complete",
    );
  }

  // Create an interval for periodically cleaning up invalid windows.
  let mut cleanup_interval = tokio::time::interval(Duration::from_secs(5));

  startup_log("startup: entering event loop");

  loop {
    let res = tokio::select! {
      _ = signal::ctrl_c() => {
        tracing::info!("Received SIGINT signal.");
        break;
      },
      Some(()) = wm.exit_rx.recv() => {
        tracing::info!("Exiting through WM command.");
        break;
      },
      Some(()) = tray.exit_rx.recv() => {
        tracing::info!("Exiting through system tray.");
        break;
      },
      Some(()) = tray.uncloak_non_tracked_rx.recv() => {
        (|| -> anyhow::Result<()> {
          tracing::info!("Tray: Uncloak all non-tracked windows");
          #[cfg(target_os = "windows")]
          {
            // Skip handles GlazeWM already manages; only orphan cloaked HWNDs.
            let skip: Vec<isize> = wm
              .state
              .windows()
              .into_iter()
              .map(|w| w.native().id().0)
              .collect();
            match dispatcher.unhide_all_cloaked_windows(&skip) {
              Ok(n) => {
                let msg = format!(
                  "tray uncloak-non-tracked: uncloaked {n} orphan window(s)"
                );
                tracing::info!("{msg}");
                crate::commands::general::layout_debug_log(msg);
              }
              Err(err) => {
                let msg = format!(
                  "tray uncloak-non-tracked: failed: {err:?}"
                );
                tracing::warn!("{msg}");
                crate::commands::general::layout_debug_log(msg);
              }
            }
            if let Err(err) = wm.state.manage_new_visible_windows(&mut config) {
              tracing::warn!("post-uncloak manage scan failed: {err:?}");
              crate::commands::general::layout_debug_log(format!(
                "tray uncloak-non-tracked: manage scan failed: {err:?}"
              ));
            }
            if wm.state.pending_sync.has_changes() {
              platform_sync(&mut wm.state, &config)?;
            }
          }
          Ok(())
        })()
      },
      Some(()) = tray.show_desktop_rx.recv() => {
        show_desktop(&mut wm.state, &config)
      },
      Some(event) = mouse_listener.next_event() => {
        tracing::debug!("Received mouse event: {:?}", event);
        wm.process_event(PlatformEvent::Mouse(event), &mut config)
      },
      Some(event) = window_listener.next_event() => {
        tracing::debug!("Received window event: {:?}", event);
        wm.process_event(PlatformEvent::Window(event), &mut config)
      },
      Some(()) = display_listener.next_event() => {
        tracing::debug!("Received display settings changed event.");
        wm.process_event(PlatformEvent::DisplaySettingsChanged, &mut config)
      },
      Some(event) = keybinding_listener.next_event() => {
        tracing::debug!("Received keyboard event: {:?}", event);
        wm.process_event(PlatformEvent::Keybinding(event), &mut config)
      }
      _ = cleanup_interval.tick() => {
        if wm.state.is_paused {
          Ok(())
        } else {
          wm.state.cleanup_invalid_windows()
        }
      },
      () = &mut startup_layout_retry_delay, if startup_layout_retries_left > 0 => {
        crate::commands::general::layout_debug_log(format!(
          "startup layout timed retry firing ({startup_layout_retries_left} left)",
        ));
        let summary = try_load_persisted_layout_snapshot(
          &layout_path_for_retry,
          &mut wm.state,
          &config,
        );
        let still = summary.as_ref().is_some_and(|s| s.unmatched_snapshot > 0);
        if still && startup_layout_retries_left > 1 {
          startup_layout_retries_left -= 1;
          startup_layout_retry_delay
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_secs(2));
          crate::commands::general::layout_debug_log(format!(
            "startup layout still unmatched; next timed retry in 2s ({startup_layout_retries_left} left)",
          ));
        } else {
          startup_layout_retries_left = 0;
          layout_auto_save.enable();
          crate::commands::general::layout_debug_log(
            "startup layout retries done; auto-save enabled",
          );
        }
        Ok(())
      },
      Some((
        message,
        response_tx,
        disconnection_tx
      )) = ipc_server.message_rx.recv() => {
        tracing::info!("Received IPC message: {:?}", message);

        if let Err(err) = ipc_server.process_message(
          message,
          &response_tx,
          &disconnection_tx,
          &mut wm,
          &mut config,
        ) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      },
      Some(wm_event) = wm.event_rx.recv() => {
        tracing::debug!("Received WM event: {:?}", wm_event);

        // Disable mouse listener when the WM is paused.
        if let WmEvent::PauseChanged { is_paused } = wm_event {
          let _ = mouse_listener.enable(!is_paused);
        }

        // Update keybinding and mouse listeners on config changes.
        if matches!(
          wm_event,
          WmEvent::UserConfigChanged { .. }
            | WmEvent::BindingModesChanged { .. }
            | WmEvent::PauseChanged { .. }
        ) {
          keybinding_listener.update(
            &config
              .active_keybinding_configs(&wm.state.binding_modes, false)
              .flat_map(|kb| kb.bindings)
              .collect::<Vec<_>>(),
          );

          mouse_listener.set_enabled_events(
            if config.value.general.focus_follows_cursor {
              &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
            } else {
              &[MouseEventKind::LeftButtonUp]
            },
          )?;
        }

        // Event-driven layout retry: when a late window is managed during the
        // startup retry window, re-run load immediately (debounced by resetting
        // the timer after a successful settle).
        if startup_layout_retries_left > 0
          && matches!(wm_event, WmEvent::WindowManaged { .. })
        {
          crate::commands::general::layout_debug_log(
            "startup layout WindowManaged-driven retry",
          );
          let summary = try_load_persisted_layout_snapshot(
            &layout_path_for_retry,
            &mut wm.state,
            &config,
          );
          if summary.as_ref().is_some_and(|s| s.unmatched_snapshot == 0) {
            startup_layout_retries_left = 0;
            layout_auto_save.enable();
            crate::commands::general::layout_debug_log(
              "startup layout fully matched after WindowManaged; auto-save enabled",
            );
          } else {
            // Nudge the timed retry so we keep trying briefly.
            startup_layout_retry_delay.as_mut().reset(
              tokio::time::Instant::now() + Duration::from_secs(2),
            );
          }
        }

        if wm_event_affects_layout_snapshot(&wm_event) {
          layout_auto_save.schedule(&wm_event);
        }

        if let Err(err) = ipc_server.process_event(wm_event) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      },
      () = layout_auto_save.sleep_mut(), if layout_auto_save.is_armed() => {
        layout_auto_save.flush(&wm.state)
      },
      Some(()) = tray.dump_state_rx.recv() => {
        dump_wm_state_with_dialog(&wm.state, &config, dispatcher)
      },
      Some(()) = tray.config_reload_rx.recv() => {
        wm.process_commands(
          &vec![InvokeCommand::WmReloadConfig],
          None,
          &mut config,
        ).map(|_| ())
      },
      Some(()) = tray.save_layout_snapshot_rx.recv() => {
        save_layout_snapshot_with_dialog(&wm.state, dispatcher)
      },
      Some(()) = tray.load_layout_snapshot_rx.recv() => {
        (|| -> anyhow::Result<()> {
          let Some(path) = pick_layout_snapshot_path(dispatcher)? else {
            tracing::info!("Load layout snapshot cancelled.");
            return Ok(());
          };
          let snapshot = read_layout_snapshot_file(&path)?;
          let summary =
            load_layout_snapshot(&snapshot, &mut wm.state, &config)?;
          if wm.state.pending_sync.has_changes() {
            platform_sync(&mut wm.state, &config)?;
          }
          tracing::info!(
            "Loaded layout snapshot from {}: matched={}",
            path.display(),
            summary.matched
          );
          Ok(())
        })()
      },
    };

    if let Err(err) = res {
      tracing::error!("{:?}", err);
      dispatcher.show_error_dialog("Non-fatal error", &err.to_string());
    }
  }

  // Flush pending layout autosave before teardown mutates
  // visibility/state. A structural move arms a 5s debounce; exiting
  // within that window would otherwise drop the pending write and
  // restore a stale layout.json on restart.
  if layout_auto_save.is_armed() {
    match layout_auto_save.flush(&wm.state) {
      Ok(()) => tracing::info!("Flushed pending layout autosave on exit."),
      Err(err) => tracing::warn!(
        "Failed to flush pending layout autosave on exit: {err:#}"
      ),
    }
  }

  tracing::info!("Window manager shutting down.");
  // Close IPC listener first (sync signal + short wait) before other
  // teardown.
  ipc_server.stop_and_wait().await;
  wm.cleanup(&mut config, &mut ipc_server);

  Ok(())
}

/// Writes one native-operation timing line to `layout.log`.
#[cfg(target_os = "windows")]
fn log_native_op_to_layout_log(message: &str) {
  if verbose_z_order_enabled() {
    crate::commands::general::layout_debug_log(message);
  }
}

/// Writes one startup step to tracing and `layout.log`.
fn startup_log(message: impl AsRef<str>) {
  let message = message.as_ref();
  tracing::info!("{message}");
  crate::commands::general::layout_debug_log(message);
}

/// Initialize logging with the specified verbosity level.
///
/// Error logs are saved to `~/.glzr/glazewm/errors.log`.
fn setup_logging(verbosity: &Verbosity) -> anyhow::Result<()> {
  let error_log_dir = home::home_dir()
    .context("Unable to get home directory.")?
    .join(".glzr/glazewm/");

  let error_writer =
    tracing_appender::rolling::never(error_log_dir, "errors.log");

  let subscriber = tracing_subscriber::registry()
    .with(
      // Output to stdout with specified verbosity level.
      fmt::Layer::new()
        .with_writer(std::io::stdout.with_max_level(verbosity.level())),
    )
    .with(
      // Output to error log file.
      fmt::Layer::new()
        .with_writer(error_writer.with_max_level(Level::ERROR)),
    );

  tracing::subscriber::set_global_default(subscriber)?;

  tracing::info!(
    "Starting WM with log level {:?}.",
    verbosity.level().to_string()
  );

  Ok(())
}

/// Launches watcher binary (Windows-only). This is a separate process that
/// is responsible for restoring hidden windows in case the main WM process
/// crashes.
///
/// This assumes the watcher binary exists in the same directory as the
/// WM binary.
#[allow(unused)]
fn start_watcher_process() -> anyhow::Result<tokio::process::Child, Error>
{
  let watcher_path = env::current_exe()?
    .parent()
    .context("Failed to resolve path to the watcher process.")?
    .join("glazewm-watcher");

  Command::new(&watcher_path)
    .spawn()
    .context("Failed to start watcher process.")
}

/// Updates the current process' PATH by querying the login shell.
///
/// Apps launched outside a terminal (Spotlight, Finder, login items)
/// inherit a PATH that only contains `/usr/bin:/bin:/usr/sbin:/sbin`. This
/// causes `shell-exec` to fail for binaries that aren't in the system
/// PATH.
#[cfg(target_os = "macos")]
fn update_path_env() {
  let shell =
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

  // Use `-l` and `-i` (login + interactive) so that both profile and rc
  // files are sourced.
  let path_var = match std::process::Command::new(&shell)
    .args(["-lic", "printf '%s' \"$PATH\""])
    .output()
  {
    Ok(output) if output.status.success() => {
      String::from_utf8(output.stdout)
        .ok()
        .filter(|path| !path.is_empty())
    }
    _ => None,
  };

  if let Some(path) = path_var {
    std::env::set_var("PATH", path);
  } else {
    tracing::warn!(
      "Failed to query login shell for PATH. Keeping existing PATH."
    );
  }
}
