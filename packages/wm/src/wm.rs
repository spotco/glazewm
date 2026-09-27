use anyhow::{bail, Context};
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::TitleBarVisibility;
use wm_common::{
  FloatingStateConfig, FullscreenStateConfig, HideMethod, InvokeCommand,
  WindowState, WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{
  Dispatcher, LengthValue, PlatformEvent, RectDelta, WindowEvent,
};

use crate::{
  commands::{
    container::{
      focus_container_by_id, focus_in_direction, set_tiling_direction,
      toggle_tiling_direction,
    },
    general::{
      build_layout_snapshot, compact_workspace_trees, cycle_focus,
      disable_binding_mode, enable_binding_mode, layout_debug_log,
      layout_state_hash, platform_sync, reload_config,
      restore_history_snapshot, same_layout_state, shell_exec,
      toggle_pause,
    },
    monitor::focus_monitor,
    window::{
      ignore_window, move_all_windows_to_workspace,
      move_window_in_direction, move_window_to_workspace, resize_window,
      set_window_position, set_window_size, update_window_state,
      WindowPositionTarget,
    },
    workspace::{
      focus_workspace, move_workspace_in_direction,
      update_workspace_config,
    },
  },
  events::{
    handle_display_settings_changed, handle_mouse_move,
    handle_window_destroyed, handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_moved_or_resized, handle_window_shown,
    handle_window_title_changed,
  },
  ipc_server::IpcServer,
  models::{Container, WorkspaceTarget},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub struct WindowManager {
  pub event_rx: mpsc::UnboundedReceiver<WmEvent>,
  pub exit_rx: mpsc::UnboundedReceiver<()>,
  pub state: WmState,
}

impl WindowManager {
  pub fn new(
    config: &mut UserConfig,
    dispatcher: Dispatcher,
  ) -> anyhow::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = mpsc::unbounded_channel();

    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    state.populate(config)?;

    Ok(Self {
      event_rx,
      exit_rx,
      state,
    })
  }

  pub fn process_event(
    &mut self,
    event: PlatformEvent,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    match event {
      PlatformEvent::DisplaySettingsChanged => {
        handle_display_settings_changed(state, config)
      }
      PlatformEvent::Keybinding(keybinding_event) => {
        // Find the keybinding config that matches this keybinding.
        let commands = config
          .active_keybinding_configs(
            &self.state.binding_modes,
            self.state.is_paused,
          )
          .find(|kb_config| {
            kb_config.bindings.contains(&keybinding_event.0)
          })
          .map(|kb_config| kb_config.commands.clone());

        if let Some(commands) = commands {
          self.process_commands(&commands, None, config)?;
        }

        // Return early since we don't want to redraw twice.
        return Ok(());
      }
      PlatformEvent::Mouse(event) => {
        handle_mouse_move(&event, state, config)
      }
      PlatformEvent::Window(window_event) => match window_event {
        WindowEvent::Focused { window, .. } => {
          handle_window_focused(&window, state, config)
        }
        WindowEvent::Shown { window, .. } => {
          handle_window_shown(window, state, config)
        }
        WindowEvent::Hidden { window, .. } => {
          handle_window_hidden(&window, state, config)
        }
        WindowEvent::MovedOrResized {
          window,
          is_interactive_start,
          is_interactive_end,
          ..
        } => handle_window_moved_or_resized(
          &window,
          is_interactive_start,
          is_interactive_end,
          state,
          config,
        ),
        WindowEvent::Minimized { window, .. } => {
          handle_window_minimized(&window, state, config)
        }
        WindowEvent::MinimizeEnded { window, .. } => {
          handle_window_minimize_ended(&window, state, config)
        }
        WindowEvent::TitleChanged { window, .. } => {
          handle_window_title_changed(&window, state, config)
        }
        WindowEvent::Destroyed { window_id, .. } => {
          handle_window_destroyed(window_id, state)
        }
      },
    }?;

    if !state.is_paused && state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    Ok(())
  }

  pub fn process_commands(
    &mut self,
    commands: &Vec<InvokeCommand>,
    subject_container_id: Option<Uuid>,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let state = &mut self.state;

    // Get the container to run WM commands with.
    let subject_container = match subject_container_id {
      Some(id) => state.container_by_id(id).with_context(|| {
        format!("No container found with the given ID '{id}'.")
      })?,
      None => state
        .focused_container()
        .context("No subject container for command.")?,
    };

    let new_subject_container_id = WindowManager::run_commands(
      commands,
      subject_container,
      state,
      config,
    )?;

    if state.pending_sync.has_changes() {
      platform_sync(state, config)?;
    }

    Ok(new_subject_container_id)
  }

  pub fn run_commands(
    commands: &Vec<InvokeCommand>,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<Uuid> {
    let mut current_subject_container = subject_container;

    for command in commands {
      WindowManager::run_command(
        command,
        current_subject_container.clone(),
        state,
        config,
      )?;

      // Update the subject container in case the container type changes.
      // For example, when going from a tiling to a floating window.
      current_subject_container =
        if current_subject_container.is_detached() {
          match state.container_by_id(current_subject_container.id()) {
            Some(container) => container,
            None => break,
          }
        } else {
          current_subject_container
        }
    }

    Ok(current_subject_container.id())
  }

  #[allow(clippy::too_many_lines)]
  pub fn run_command(
    command: &InvokeCommand,
    subject_container: Container,
    state: &mut WmState,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // No-op if WM is currently paused.
    if state.is_paused && *command != InvokeCommand::WmTogglePause {
      return Ok(());
    }

    if subject_container.is_detached() {
      bail!("Cannot run command because subject container is detached.");
    }

    match &command {
      InvokeCommand::AdjustBorders(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            let args = args.clone();
            let border_delta = RectDelta::new(
              args.left.unwrap_or(LengthValue::from_px(0)),
              args.top.unwrap_or(LengthValue::from_px(0)),
              args.right.unwrap_or(LengthValue::from_px(0)),
              args.bottom.unwrap_or(LengthValue::from_px(0)),
            );

            window.set_border_delta(border_delta);
            state.pending_sync.queue_container_to_redraw(window);

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Close => {
        match subject_container.as_window_container() {
          Ok(window) => {
            state.layout_history.clear("close command");
            // Window handle might no longer be valid here.
            if let Err(err) = window.native().close() {
              warn!("Failed to close window: {:?}", err);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::Focus(args) => {
        if let Some(direction) = &args.direction {
          focus_in_direction(&subject_container, direction, state)?;
        }

        if let Some(direction) = &args.workspace_in_direction {
          focus_workspace(
            WorkspaceTarget::Direction(direction.clone()),
            state,
            config,
          )?;
        }

        if let Some(container_id) = &args.container_id {
          focus_container_by_id(container_id, state)?;
        }

        if let Some(name) = &args.workspace {
          focus_workspace(
            WorkspaceTarget::Name(name.clone()),
            state,
            config,
          )?;
        }

        if let Some(monitor_index) = &args.monitor {
          focus_monitor(*monitor_index, state, config)?;
        }

        if args.next_active_workspace {
          focus_workspace(WorkspaceTarget::NextActive, state, config)?;
        }

        if args.prev_active_workspace {
          focus_workspace(WorkspaceTarget::PreviousActive, state, config)?;
        }

        if args.next_workspace {
          focus_workspace(WorkspaceTarget::Next, state, config)?;
        }

        if args.prev_workspace {
          focus_workspace(WorkspaceTarget::Previous, state, config)?;
        }

        if args.recent_workspace {
          focus_workspace(WorkspaceTarget::Recent, state, config)?;
        }

        if args.next_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::NextActiveInMonitor,
            state,
            config,
          )?;
        }

        if args.prev_active_workspace_on_monitor {
          focus_workspace(
            WorkspaceTarget::PreviousActiveInMonitor,
            state,
            config,
          )?;
        }

        Ok(())
      }
      InvokeCommand::Ignore => {
        match subject_container.as_window_container() {
          Ok(window) => ignore_window(window, state),
          _ => Ok(()),
        }
      }
      InvokeCommand::Move(args) => {
        if args.all {
          if args.target.direction.is_some() {
            bail!(
              "The --all flag can only be used with a workspace move target."
            );
          }

          let target =
            if let Some(direction) = &args.target.workspace_in_direction {
              WorkspaceTarget::Direction(direction.clone())
            } else if let Some(name) = &args.target.workspace {
              WorkspaceTarget::Name(name.clone())
            } else if args.target.next_active_workspace {
              WorkspaceTarget::NextActive
            } else if args.target.prev_active_workspace {
              WorkspaceTarget::PreviousActive
            } else if args.target.next_workspace {
              WorkspaceTarget::Next
            } else if args.target.prev_workspace {
              WorkspaceTarget::Previous
            } else if args.target.next_active_workspace_on_monitor {
              WorkspaceTarget::NextActiveInMonitor
            } else if args.target.prev_active_workspace_on_monitor {
              WorkspaceTarget::PreviousActiveInMonitor
            } else if args.target.recent_workspace {
              WorkspaceTarget::Recent
            } else {
              bail!("The --all flag requires a workspace move target.");
            };

          state.layout_history.clear("move all windows command");
          let workspace =
            subject_container.workspace().context("No workspace.")?;
          return move_all_windows_to_workspace(
            &workspace, target, state, config,
          );
        }

        if args.target.direction.is_none() {
          state.layout_history.clear("workspace move command");
        }
        let history_context =
          args.target.direction.as_ref().and_then(|_| {
            subject_container
              .as_window_container()
              .ok()
              .filter(|window| window.state() == WindowState::Tiling)
              .and_then(|window| {
                window
                  .workspace()
                  .map(|workspace| (window.id(), workspace.config().name))
              })
          });
        let history_before = history_context
          .as_ref()
          .map(|_| build_layout_snapshot(state))
          .transpose()?;
        let history_operation =
          args.target.direction.as_ref().map(|direction| {
            let stack_direction = if args.opposite_tiling_direction {
              state.global_tiling_direction.inverse()
            } else {
              state.global_tiling_direction.clone()
            };
            format!(
              "move direction={direction:?} stack={stack_direction:?}"
            )
          });

        match subject_container.as_window_container() {
          Ok(window) => {
            if let Some(direction) = &args.target.direction {
              let stack_direction = if args.opposite_tiling_direction {
                state.global_tiling_direction.inverse()
              } else {
                state.global_tiling_direction.clone()
              };
              move_window_in_direction(
                window.clone(),
                direction,
                &stack_direction,
                state,
                config,
              )?;
            }

            if let Some(direction) = &args.target.workspace_in_direction {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Direction(direction.clone()),
                state,
                config,
              )?;
            }

            if let Some(name) = &args.target.workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Name(name.clone()),
                state,
                config,
              )?;
            }

            if args.target.next_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActive,
                state,
                config,
              )?;
            }

            if args.target.prev_active_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::PreviousActive,
                state,
                config,
              )?;
            }

            if args.target.next_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Next,
                state,
                config,
              )?;
            }

            if args.target.prev_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Previous,
                state,
                config,
              )?;
            }

            if args.target.recent_workspace {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::Recent,
                state,
                config,
              )?;
            }

            if args.target.next_active_workspace_on_monitor {
              move_window_to_workspace(
                window.clone(),
                WorkspaceTarget::NextActiveInMonitor,
                state,
                config,
              )?;
            }

            if args.target.prev_active_workspace_on_monitor {
              move_window_to_workspace(
                window,
                WorkspaceTarget::PreviousActiveInMonitor,
                state,
                config,
              )?;
            }

            if let (
              Some((window_id, before_workspace)),
              Some(before),
              Some(operation),
            ) = (history_context, history_before, history_operation)
            {
              let after = build_layout_snapshot(state)?;
              let after_workspace = state
                .container_by_id(window_id)
                .and_then(|container| {
                  container
                    .as_window_container()
                    .ok()
                    .and_then(|window| window.workspace())
                })
                .map(|workspace| workspace.config().name);

              if after_workspace.as_deref()
                != Some(before_workspace.as_str())
              {
                state
                  .layout_history
                  .clear("directional move changed workspace");
              } else if !same_layout_state(&before, &after) {
                let entry = state.layout_history.record(
                  operation,
                  before_workspace.clone(),
                  window_id.to_string(),
                  before.clone(),
                  after.clone(),
                );
                layout_debug_log(format!(
                  "layout history commit id={} op={:?} workspace={:?} focus={} before_hash={} after_hash={} undo_depth={} redo_depth=0 before_tree={:?} after_tree={:?}",
                  entry.transaction_id,
                  entry.operation,
                  entry.workspace_name,
                  entry.focused_window_id,
                  layout_state_hash(&before),
                  layout_state_hash(&after),
                  state.layout_history.undo_depth(),
                  compact_workspace_trees(&before),
                  compact_workspace_trees(&after),
                ));
              } else {
                layout_debug_log(format!(
                  "layout history skip op={:?} workspace={:?} focus={} reason=no-op state_hash={} undo_depth={} redo_depth={}",
                  operation,
                  before_workspace,
                  window_id,
                  layout_state_hash(&before),
                  state.layout_history.undo_depth(),
                  state.layout_history.redo_depth(),
                ));
              }
            }
            Ok(())
          }

          _ => Ok(()),
        }
      }
      InvokeCommand::Undo => {
        let Some(entry) = state.layout_history.pop_undo() else {
          layout_debug_log("layout history undo no-op: undo stack empty");
          return Ok(());
        };
        let transaction_id = entry.transaction_id;
        let result =
          restore_history_snapshot(&entry.before, state, config);
        match result {
          Ok(_) => {
            state.layout_history.push_redo(entry.clone());
            layout_debug_log(format!(
              "layout history undo id={} op={:?} workspace={:?} focus={} before_hash={} after_hash={} undo_depth={} redo_depth={} before_tree={:?} after_tree={:?}",
              transaction_id,
              entry.operation,
              entry.workspace_name,
              entry.focused_window_id,
              layout_state_hash(&entry.after),
              layout_state_hash(&entry.before),
              state.layout_history.undo_depth(),
              state.layout_history.redo_depth(),
              compact_workspace_trees(&entry.after),
              compact_workspace_trees(&entry.before),
            ));
            Ok(())
          }
          Err(err) => {
            state.layout_history.push_undo(entry);
            Err(err)
          }
        }
      }
      InvokeCommand::Redo => {
        let Some(entry) = state.layout_history.pop_redo() else {
          layout_debug_log("layout history redo no-op: redo stack empty");
          return Ok(());
        };
        let transaction_id = entry.transaction_id;
        let result = restore_history_snapshot(&entry.after, state, config);
        match result {
          Ok(_) => {
            state.layout_history.push_undo(entry.clone());
            layout_debug_log(format!(
              "layout history redo id={} op={:?} workspace={:?} focus={} before_hash={} after_hash={} undo_depth={} redo_depth={} before_tree={:?} after_tree={:?}",
              transaction_id,
              entry.operation,
              entry.workspace_name,
              entry.focused_window_id,
              layout_state_hash(&entry.before),
              layout_state_hash(&entry.after),
              state.layout_history.undo_depth(),
              state.layout_history.redo_depth(),
              compact_workspace_trees(&entry.before),
              compact_workspace_trees(&entry.after),
            ));
            Ok(())
          }
          Err(err) => {
            state.layout_history.push_redo(entry);
            Err(err)
          }
        }
      }
      InvokeCommand::MoveWorkspace { direction } => {
        state.layout_history.clear("workspace move command");
        let workspace =
          subject_container.workspace().context("No workspace.")?;

        move_workspace_in_direction(&workspace, direction, state, config)
      }
      InvokeCommand::Position(args) => {
        match subject_container.as_window_container() {
          Ok(window) => {
            if args.centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )
            } else {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(args.x_pos, args.y_pos),
                state,
              )
            }
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::UpdateWorkspaceConfig {
        workspace,
        new_config,
      } => {
        let workspace = if let Some(workspace_name) = workspace {
          state
            .workspace_by_name(workspace_name)
            .context("Workspace doesn't exist.")?
        } else {
          subject_container.workspace().context("No workspace.")?
        };
        update_workspace_config(&workspace, state, config, new_config)
      }
      InvokeCommand::Resize(args) => {
        state.layout_history.clear("resize command");
        match subject_container.as_window_container() {
          Ok(window) => resize_window(
            &window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::SetFloating {
        centered,
        shown_on_top,
        x_pos,
        y_pos,
        width,
        height,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          state.layout_history.clear("set floating command");
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;
          let centered = centered.unwrap_or(floating_defaults.centered);

          let window = update_window_state(
            window.clone(),
            WindowState::Floating(FloatingStateConfig {
              centered,
              shown_on_top: shown_on_top
                .unwrap_or(floating_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          // Allow size and position to be set if window has not previously
          // been manually placed.
          if !window.has_custom_floating_placement() {
            if width.is_some() || height.is_some() {
              set_window_size(
                window.clone(),
                width.clone(),
                height.clone(),
                state,
              )?;
            }

            if centered {
              set_window_position(
                window,
                &WindowPositionTarget::Centered,
                state,
              )?;
            } else if x_pos.is_some() || y_pos.is_some() {
              set_window_position(
                window,
                &WindowPositionTarget::Coordinates(*x_pos, *y_pos),
                state,
              )?;
            }
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          state.layout_history.clear("set fullscreen command");
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          update_window_state(
            window.clone(),
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            }),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::SetMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            state.layout_history.clear("set minimized command");
            update_window_state(
              window.clone(),
              WindowState::Minimized,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            state.layout_history.clear("set tiling command");
            update_window_state(
              window,
              WindowState::Tiling,
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::SetTitleBarVisibility {
        // LINT: `visibility` is only used on Windows.
        #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
        visibility,
      } => match subject_container.as_window_container() {
        #[cfg(target_os = "windows")]
        Ok(window) => {
          _ = window.native().set_title_bar_visibility(
            *visibility == TitleBarVisibility::Shown,
          );
          Ok(())
        }
        _ => Ok(()),
      },
      // LINT: `args` is only used on Windows.
      #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
      InvokeCommand::SetTransparency(args) => {
        match subject_container.as_window_container() {
          #[cfg(target_os = "windows")]
          Ok(window) => {
            if let Some(opacity) = &args.opacity {
              _ = window.native().set_transparency(opacity);
            }

            if let Some(opacity_delta) = &args.opacity_delta {
              _ = window.native().adjust_transparency(opacity_delta);
            }

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ShellExec {
        hide_window,
        command,
      } =>
      // Re-quote after split_ipc_args so paths with spaces stay one arg
      // for ShellExec.
      {
        shell_exec(&wm_common::join_ipc_args(command), *hide_window, state)
      }
      InvokeCommand::Size(args) => {
        match subject_container.as_window_container() {
          Ok(window) => set_window_size(
            window,
            args.width.clone(),
            args.height.clone(),
            state,
          ),
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleFloating {
        centered,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          state.layout_history.clear("toggle floating command");
          let floating_defaults =
            &config.value.window_behavior.state_defaults.floating;

          let centered = centered.unwrap_or(floating_defaults.centered);
          let target_state = WindowState::Floating(FloatingStateConfig {
            centered,
            shown_on_top: shown_on_top
              .unwrap_or(floating_defaults.shown_on_top),
          });

          let window = update_window_state(
            window.clone(),
            window.toggled_floating_state(target_state),
            state,
            config,
          )?;

          if !window.has_custom_floating_placement() && centered {
            set_window_position(
              window,
              &WindowPositionTarget::Centered,
              state,
            )?;
          }

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleFullscreen {
        maximized,
        shown_on_top,
      } => match subject_container.as_window_container() {
        Ok(window) => {
          state.layout_history.clear("toggle fullscreen command");
          let fullscreen_defaults =
            &config.value.window_behavior.state_defaults.fullscreen;

          let target_state =
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: maximized
                .unwrap_or(fullscreen_defaults.maximized),
              shown_on_top: shown_on_top
                .unwrap_or(fullscreen_defaults.shown_on_top),
            });

          update_window_state(
            window.clone(),
            window.toggled_state(target_state, config),
            state,
            config,
          )?;

          Ok(())
        }
        _ => Ok(()),
      },
      InvokeCommand::ToggleMinimized => {
        match subject_container.as_window_container() {
          Ok(window) => {
            state.layout_history.clear("toggle minimized command");
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Minimized, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTiling => {
        match subject_container.as_window_container() {
          Ok(window) => {
            state.layout_history.clear("toggle tiling command");
            update_window_state(
              window.clone(),
              window.toggled_state(WindowState::Tiling, config),
              state,
              config,
            )?;

            Ok(())
          }
          _ => Ok(()),
        }
      }
      InvokeCommand::ToggleTilingDirection => {
        toggle_tiling_direction(subject_container, state, config)
      }
      InvokeCommand::SetTilingDirection { tiling_direction } => {
        set_tiling_direction(
          subject_container,
          state,
          config,
          tiling_direction,
        )
      }
      InvokeCommand::WmCycleFocus {
        omit_floating,
        omit_fullscreen,
        omit_minimized,
        omit_tiling,
      } => cycle_focus(
        *omit_floating,
        *omit_fullscreen,
        *omit_minimized,
        *omit_tiling,
        state,
        config,
      ),
      InvokeCommand::WmDisableBindingMode { name } => {
        disable_binding_mode(name, state);
        Ok(())
      }
      InvokeCommand::WmEnableBindingMode { name } => {
        enable_binding_mode(name, state, config)
      }
      InvokeCommand::WmExit => state.emit_exit(),
      InvokeCommand::WmUncloakNonTracked => {
        #[cfg(target_os = "windows")]
        {
          let skip: Vec<isize> = state
            .windows()
            .into_iter()
            .map(|w| w.native().id().0)
            .collect();
          match state.dispatcher.unhide_all_cloaked_windows(&skip) {
            Ok(n) => {
              let msg = format!(
                "wm-uncloak-non-tracked: uncloaked {n} orphan window(s)"
              );
              tracing::info!("{msg}");
              crate::commands::general::layout_debug_log(msg);
            }
            Err(err) => {
              let msg = format!("wm-uncloak-non-tracked: failed: {err:?}");
              tracing::warn!("{msg}");
              crate::commands::general::layout_debug_log(msg);
            }
          }
          if let Err(err) = state.manage_new_visible_windows(config) {
            tracing::warn!("post-uncloak manage scan failed: {err:?}");
          }
        }
        Ok(())
      }
      InvokeCommand::WmRedraw => {
        state
          .pending_sync
          .queue_container_to_redraw(state.root_container.clone());

        Ok(())
      }
      InvokeCommand::WmReloadConfig => reload_config(state, config),
      InvokeCommand::WmTogglePause => {
        toggle_pause(state);
        Ok(())
      }
      InvokeCommand::LoadLayout { path } => {
        let snapshot =
          crate::commands::general::read_layout_snapshot_file(path)?;
        let _summary = crate::commands::general::load_layout_snapshot(
          &snapshot, state, config,
        )?;
        state.layout_history.clear("load layout command");
        Ok(())
      }
    }
  }

  /// Runs cleanup tasks when the WM is exiting.
  pub(crate) fn cleanup(
    &mut self,
    config: &mut UserConfig,
    ipc_server: &mut IpcServer,
  ) {
    self.state.emit_event(WmEvent::ApplicationExiting);

    // Ensure that the WM is unpaused, otherwise, shutdown commands won't
    // get executed.
    self.state.is_paused = false;

    // Run user's shutdown commands.
    if let Err(err) = self.process_commands(
      &config.value.general.shutdown_commands.clone(),
      None,
      config,
    ) {
      tracing::warn!("Failed to run shutdown commands: {:?}", err);
    }

    // Uncloak / show every managed window BEFORE exit.
    // With hide_method=cloak, inactive-workspace windows stay DWM-cloaked.
    // Watcher skips restore on clean ApplicationExiting, so without this
    // those windows vanish from visible_windows() on next start and layout
    // restore cannot match them (Terminal/Code "disappeared" bug).
    self.restore_visibility_on_exit(config);

    // Emit remaining WM events before exiting.
    while let Ok(wm_event) = self.event_rx.try_recv() {
      tracing::info!(
        "Emitting WM event before shutting down: {:?}",
        wm_event
      );

      if let Err(err) = ipc_server.process_event(wm_event) {
        tracing::warn!("{:?}", err);
      }
    }

    // Drop the IPC TcpListener before process exit so Windows does not
    // leave a ghost LISTENING socket. Also stop the watcher so it does
    // not restart us. Prefer stop_and_wait from main; this sync stop
    // is a safety net for Drop.
    ipc_server.stop();
    let watcher_report = crate::ipc_conflict::kill_watcher_on_exit();
    tracing::info!("Exit watcher cleanup: {watcher_report}");
    crate::commands::general::layout_debug_log(format!(
      "wm-exit: IPC stopped; watcher cleanup: {watcher_report}"
    ));
  }

  /// Best-effort uncloak/show of all managed windows (Windows cloak
  /// `hide_method`).
  fn restore_visibility_on_exit(&self, config: &UserConfig) {
    let cloak = config.value.general.hide_method == HideMethod::Cloak;
    let mut restored = 0usize;
    for window in self.state.windows() {
      let native = window.native();
      #[cfg(target_os = "windows")]
      if cloak {
        if let Err(err) = native.set_cloaked(false) {
          tracing::warn!("Exit uncloak failed: {err:?}");
          continue;
        }
      }
      if let Err(err) = native.show() {
        tracing::warn!("Exit show failed: {err:?}");
        continue;
      }
      let _ = native.set_taskbar_visibility(true);
      restored += 1;
    }
    let msg = format!(
      "wm-exit: restored visibility on {restored} window(s) (cloak={cloak})"
    );
    tracing::info!("{msg}");
    crate::commands::general::layout_debug_log(msg);
  }
}
