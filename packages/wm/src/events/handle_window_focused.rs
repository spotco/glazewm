use anyhow::Context;
use tracing::info;
use wm_common::{DisplayState, WindowRuleEvent, WmEvent};
use wm_platform::NativeWindow;
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  commands::{
    container::set_focused_descendant, window::run_window_rules,
    workspace::focus_workspace,
  },
  models::WorkspaceTarget,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

#[allow(clippy::too_many_lines)]
pub fn handle_window_focused(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);
  #[cfg(target_os = "windows")]
  let found_window =
    found_window.filter(|window| window.native().identity_is_current());
  let is_ignored_window =
    state.ignored_windows.iter().any(|ignored_window| {
      ignored_window == native_window && {
        #[cfg(all(target_os = "windows", test))]
        {
          ignored_window.identity_is_current()
            || (!ignored_window.is_valid() && !native_window.is_valid())
        }
        #[cfg(all(target_os = "windows", not(test)))]
        {
          ignored_window.identity_is_current()
        }
        #[cfg(not(target_os = "windows"))]
        {
          true
        }
      }
    });

  // Ignored windows stay out of logical focus. Record the promotion
  // immediately so a later Alt-Tab does not replay a chain that still
  // has this window behind the tiled group.
  if is_ignored_window {
    if crate::commands::general::verbose_z_order_enabled() {
      let native_debug = native_window.debug_info();
      crate::commands::general::layout_debug_log(format!(
        "ignored window focused handle={:?} title={:?} class={:?} z_order={:?}",
        native_window.id(),
        native_debug.title,
        native_debug.class_name,
        native_debug.z_order_index,
      ));
    }
    state.suspend_z_order_for_ignored_foreground(native_window);
    record_focus_diagnostic(
      state,
      native_window,
      crate::diagnostic_history::FocusOutcome::IgnoredSuspend,
      None,
    );
    return Ok(());
  }

  state.ignored_native_foreground = None;
  let lifecycle_recovery =
    state.consume_lifecycle_focus_recovery(native_window);

  let focused_container =
    state.focused_container().context("No focused container.")?;

  // Update the focus sync state. If the OS focused window is not same as
  // the WM's focused container, then the focus is not synced.
  state.is_focus_synced = match focused_container.as_window_container() {
    Ok(window) => *window.native() == *native_window,
    _ => native_window.is_desktop_window().unwrap_or(false),
  };

  // Focus effect should be updated for any change in focus that shouldn't
  // be overwritten. The incoming focus event at this point is either:
  //  1. WM's focus container (window or workspace). This is the desktop
  //     window in the case of a workspace.
  //  2. An ignored window.
  //  3. A window that received manual focus.
  state.pending_sync.queue_focused_effect_update();

  // Unknown foreground windows (the Alt-Tab switcher, for example) must
  // not reconcile z-order. Doing so raises the logical tiled focus and
  // pulls every other window behind the tiled group mid-gesture.
  if found_window.is_none() {
    record_focus_diagnostic(
      state,
      native_window,
      crate::diagnostic_history::FocusOutcome::UnknownForeground,
      None,
    );
    return Ok(());
  }

  if let Some(window) = found_window {
    let workspace = window.workspace().context("No workspace")?;
    let already_requested_activation = state
      .pending_sync
      .stack_activation(&workspace)
      .is_some_and(|activation| activation.window_id == window.id());
    if !lifecycle_recovery {
      // A different live native foreground transition supersedes the
      // automatic fallback request. This preserves a real owner/app focus
      // transition that races with unmanage processing.
      state.clear_lifecycle_focus_recovery();
    }

    // Native focus has been synced to the WM's focused container.
    if focused_container == window.clone().into() {
      state.is_focus_synced = true;
      state
        .pending_sync
        .queue_workspace_to_reorder(workspace.clone());
      if lifecycle_recovery {
        record_focus_diagnostic(
          state,
          native_window,
          crate::diagnostic_history::FocusOutcome::LifecycleRecovery,
          Some(&window),
        );
      } else if !already_requested_activation {
        state.pending_sync.queue_stack_activation(
          &workspace,
          window.id(),
          crate::pending_sync::StackActivationReason::NativeActivation,
        );
        record_focus_diagnostic(
          state,
          native_window,
          crate::diagnostic_history::FocusOutcome::AlreadySynced,
          Some(&window),
        );
      }
      return Ok(());
    }

    info!("Window manually focused: {window}");

    // Handle focus events from windows on hidden workspaces. For example,
    // if Discord is forcefully shown by the OS when it's on a hidden
    // workspace, switch focus to Discord's workspace.
    // `Hiding` is included so a window stuck there (reposition failed
    // before cloak, so the state never reached `Hidden`) can still be
    // recovered from Alt-Tab instead of the focus event being dropped.
    if matches!(
      window.display_state(),
      DisplayState::Hidden | DisplayState::Hiding
    ) {
      info!("Focusing off-screen window: {window}");

      focus_workspace(
        WorkspaceTarget::Name(workspace.config().name),
        state,
        config,
      )?;
    }

    // Update the WM's focus state.
    set_focused_descendant(&window.clone().into(), None);

    // Run window rules for focus events.
    run_window_rules(
      window.clone(),
      &WindowRuleEvent::Focus,
      state,
      config,
    )?;

    state.is_focus_synced = true;
    let pending_sync = state
      .pending_sync
      .queue_workspace_to_reorder(workspace.clone());
    if !lifecycle_recovery && !already_requested_activation {
      pending_sync.queue_stack_activation(
        &workspace,
        window.id(),
        crate::pending_sync::StackActivationReason::NativeActivation,
      );
    }

    // Broadcast the focus change event.
    state.emit_event(WmEvent::FocusChanged {
      focused_container: window.to_dto()?,
    });
    record_focus_diagnostic(
      state,
      native_window,
      if lifecycle_recovery {
        crate::diagnostic_history::FocusOutcome::LifecycleRecovery
      } else {
        crate::diagnostic_history::FocusOutcome::ManualFocus
      },
      Some(&window),
    );
  }

  Ok(())
}

fn record_focus_diagnostic(
  state: &mut WmState,
  native_window: &NativeWindow,
  outcome: crate::diagnostic_history::FocusOutcome,
  window: Option<&crate::models::WindowContainer>,
) {
  use crate::diagnostic_history::{
    hwnd_i64, truncate_title, DiagnosticBody,
  };

  let props = window.map(WindowGetters::native_properties);
  let title = props
    .as_ref()
    .map(|props| truncate_title(&props.title))
    .or_else(|| {
      native_window
        .debug_info()
        .title
        .map(|title| truncate_title(&title))
    });
  #[cfg(target_os = "windows")]
  let class_name = props
    .as_ref()
    .map(|props| truncate_title(&props.class_name))
    .or_else(|| {
      native_window
        .debug_info()
        .class_name
        .map(|class_name| truncate_title(&class_name))
    });
  #[cfg(not(target_os = "windows"))]
  let class_name = None;
  let workspace = window.and_then(CommonGetters::workspace);
  let shown_on_top = window.and_then(|window| match window.state() {
    wm_common::WindowState::Floating(config) => Some(config.shown_on_top),
    wm_common::WindowState::Fullscreen(config) => {
      Some(config.shown_on_top)
    }
    wm_common::WindowState::Minimized | wm_common::WindowState::Tiling => {
      None
    }
  });
  state.push_diagnostic(DiagnosticBody::Focus {
    hwnd: hwnd_i64(native_window.id().0),
    title,
    class_name,
    outcome,
    wm_container_id: window.map(CommonGetters::id),
    workspace_id: workspace.as_ref().map(CommonGetters::id),
    workspace_name: workspace.map(|workspace| workspace.config().name),
    display_state: window.map(|window| {
      display_state_name(&window.display_state()).to_string()
    }),
    window_state: window
      .map(|window| window_state_name(&window.state()).to_string()),
    shown_on_top,
    focus_synced: state.is_focus_synced,
  });
}

fn display_state_name(state: &wm_common::DisplayState) -> &'static str {
  match state {
    wm_common::DisplayState::Shown => "shown",
    wm_common::DisplayState::Showing => "showing",
    wm_common::DisplayState::Hidden => "hidden",
    wm_common::DisplayState::Hiding => "hiding",
  }
}

fn window_state_name(state: &wm_common::WindowState) -> &'static str {
  match state {
    wm_common::WindowState::Tiling => "tiling",
    wm_common::WindowState::Floating(_) => "floating",
    wm_common::WindowState::Fullscreen(_) => "fullscreen",
    wm_common::WindowState::Minimized => "minimized",
  }
}

/// Returns true if focus should be reassigned to the WM's focus container.
#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use tokio::sync::mpsc;
  use wm_common::{
    GapsConfig, TilingDirection, WindowState, WorkspaceConfig,
  };
  use wm_platform::{EventLoop, Rect, RectDelta};

  use super::*;
  use crate::{
    commands::{container::attach_container, monitor::add_monitor},
    models::{
      Container, NativeMonitorProperties, NativeWindowProperties,
      NonTilingWindow, TilingWindow, Workspace,
    },
    traits::CommonGetters,
  };

  fn test_properties(title: &str) -> NativeWindowProperties {
    NativeWindowProperties {
      title: title.into(),
      class_name: "test".into(),
      process_name: "test".into(),
      process_path: None,
      frame: Rect::from_xy(0, 0, 100, 100),
      is_minimized: false,
      is_maximized: false,
      is_resizable: true,
      shadow_borders: RectDelta::zero(),
    }
  }

  #[test]
  #[allow(clippy::too_many_lines)]
  fn e2e_ignored_window_alt_tab_cancels_stack_and_focus_work() {
    let (_event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "popup-focus-test".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");

    let tiled_native = crate::test_utils::TestNativeWindow::new();
    let detached_native = crate::test_utils::TestNativeWindow::new();
    let ignored_native = crate::test_utils::TestNativeWindow::new();
    let tiled_window = TilingWindow::new(
      None,
      tiled_native.native().clone(),
      test_properties("managed owner"),
      None,
      RectDelta::zero(),
      Rect::from_xy(0, 0, 100, 100),
      false,
      GapsConfig::default(),
      Vec::new(),
      None,
    );
    let detached_window = NonTilingWindow::new(
      None,
      detached_native.native().clone(),
      test_properties("detached peer"),
      WindowState::Floating(wm_common::FloatingStateConfig::default()),
      Some(WindowState::Tiling),
      RectDelta::zero(),
      None,
      Rect::from_xy(0, 0, 100, 100),
      false,
      Vec::new(),
      None,
    );

    attach_container(
      &tiled_window.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach tiled window");
    attach_container(
      &detached_window.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach detached window");
    crate::commands::container::set_focused_descendant(
      &tiled_window.clone().into(),
      None,
    );
    state.pending_sync.clear();

    let config_path = std::env::temp_dir().join(format!(
      "glazewm-popup-focus-test-{}.yaml",
      uuid::Uuid::new_v4()
    ));
    let mut config =
      UserConfig::new(Some(config_path)).expect("test config");

    handle_window_focused(
      ignored_native.native(),
      &mut state,
      &mut config,
    )
    .expect("handle unmanaged popup focus");

    assert!(
      state.pending_sync.workspaces_to_reorder().is_empty(),
      "an unknown foreground window must not reorder z-order"
    );
    assert!(!state.is_focus_synced);

    // An explicitly ignored window is allowed to own native focus. Work
    // queued before that event must be dropped, and draining it must not
    // rebuild the normal z-order chain over the ignored foreground.
    state.pending_sync.clear();
    state.ignored_windows.push(ignored_native.native().clone());
    state
      .pending_sync
      .queue_workspace_to_reorder(workspace.clone());
    state.pending_sync.queue_focus_change();
    state.pending_sync.queue_focused_effect_update();
    state.pending_sync.queue_cursor_jump();
    state.pending_sync.queue_stack_activation(
      &workspace,
      detached_window.id(),
      crate::pending_sync::StackActivationReason::NativeActivation,
    );
    state
      .pending_sync
      .queue_container_to_redraw(tiled_window.clone());
    let sentinel =
      vec![detached_native.native().id(), tiled_native.native().id()];
    let expected_after_ignored_activation =
      vec![ignored_native.native().id(), sentinel[0], sentinel[1]];
    state.apply_normal_stack_operation(
      workspace.id(),
      crate::normal_window_stack::StackOperation::Reconcile {
        tiled_window_ids: vec![tiled_native.native().id()],
        detached_window_ids: vec![detached_native.native().id()],
        owner_by_window: HashMap::new(),
        native_order: sentinel.clone(),
      },
    );

    handle_window_focused(
      ignored_native.native(),
      &mut state,
      &mut config,
    )
    .expect("handle ignored window focus");

    assert!(state.pending_sync.workspaces_to_reorder().is_empty());
    assert!(state
      .pending_sync
      .focused_window_to_bring_to_front(&workspace)
      .is_none());
    assert!(!state.pending_sync.needs_focus_update());
    assert!(!state.pending_sync.needs_focused_effect_update());
    assert!(!state.pending_sync.needs_cursor_jump());
    assert!(!state.pending_sync.containers_to_redraw().is_empty());
    assert_eq!(
      state.ignored_native_foreground,
      Some(ignored_native.native().id())
    );
    assert!(!state.is_focus_synced);

    // A redraw queued alongside the stale reorder still drains. The
    // ignored activation stays above the saved managed chain, whose
    // relative order must not change or promote the tiled window.
    crate::commands::general::platform_sync(&mut state, &config)
      .expect("drain pending work after ignored focus");
    assert_eq!(
      state.normal_z_order_for_workspace(workspace.id()),
      Some(expected_after_ignored_activation.clone())
    );
    assert_eq!(
      state.ignored_native_foreground,
      Some(ignored_native.native().id())
    );
    assert!(state.pending_sync.containers_to_redraw().is_empty());

    // Work queued while the ignored window still owns foreground is
    // dropped again on the next drain.
    state
      .pending_sync
      .queue_workspace_to_reorder(workspace.clone());
    state.pending_sync.queue_focus_change();
    crate::commands::general::platform_sync(&mut state, &config)
      .expect("drain work queued during ignored foreground");
    assert_eq!(
      state.normal_z_order_for_workspace(workspace.id()),
      Some(expected_after_ignored_activation)
    );
    assert!(!state.pending_sync.needs_focus_update());
    assert!(state.pending_sync.workspaces_to_reorder().is_empty());

    handle_window_focused(tiled_native.native(), &mut state, &mut config)
      .expect("return focus to the tiled window");
    assert_eq!(state.ignored_native_foreground, None);
    assert_eq!(state.pending_sync.workspaces_to_reorder().len(), 1);
  }
}
