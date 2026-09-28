use anyhow::Context;
use tracing::info;
use wm_common::{DisplayState, WindowRuleEvent, WmEvent};
use wm_platform::NativeWindow;

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

pub fn handle_window_focused(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);
  let is_ignored_window = state
    .ignored_windows
    .iter()
    .any(|ignored_window| ignored_window == native_window);

  // Ignored windows are outside the WM's logical focus model. Native focus
  // is already bringing them to the foreground. Cancel focus and z-order
  // work that was queued before this event; a later drain must not rebuild
  // the chain under the previous logical focus. The next managed focus
  // event clears the suspension and reconciles.
  if is_ignored_window {
    let native_debug = native_window.debug_info();
    crate::commands::general::layout_debug_log(format!(
      "ignored window focused handle={:?} title={:?} class={:?} z_order={:?}",
      native_window.id(),
      native_debug.title,
      native_debug.class_name,
      native_debug.z_order_index,
    ));
    state.suspend_z_order_for_ignored_foreground(native_window);
    return Ok(());
  }

  state.ignored_native_foreground = None;

  let focused_container =
    state.focused_container().context("No focused container.")?;

  // Update the focus sync state. If the OS focused window is not same as
  // the WM's focused container, then the focus is not synced.
  state.is_focus_synced = match focused_container.as_window_container() {
    Ok(window) => *window.native() == *native_window,
    _ => native_window.is_desktop_window().unwrap_or(false),
  };

  // Handle overriding focus on close/minimize. After a window is closed
  // or minimized, the OS or the closed application might automatically
  // switch focus to a different window. To force focus to go to the WM's
  // target focus container, we reassign any focus events 100ms after
  // close/minimize. This will cause focus to briefly flicker to the OS
  // focus target and then to the WM's focus target.
  if should_override_focus(state) {
    state.pending_sync.queue_focus_change();
    return Ok(());
  }

  // Ignore the focus event if window is being hidden by the WM.
  if let Some(window) = &found_window {
    if window.display_state() == DisplayState::Hiding {
      return Ok(());
    }
  }

  // Focus effect should be updated for any change in focus that shouldn't
  // be overwritten. The incoming focus event at this point is either:
  //  1. WM's focus container (window or workspace). This is the desktop
  //     window in the case of a workspace.
  //  2. An ignored window.
  //  3. A window that received manual focus.
  state.pending_sync.queue_focused_effect_update();

  // An owned dialog or other unmanaged popup can temporarily become the
  // native foreground window without changing GlazeWM's logical focus. Its
  // activation may disturb the normal-window z-order, and some
  // applications do not emit a reliable focus event for the owner when
  // the popup closes. Reconcile the focused workspace while the logical
  // focus is still known so detached windows cannot remain above the
  // tiled layer.
  if found_window.is_none() {
    if let Some(workspace) = focused_container.workspace() {
      state.pending_sync.queue_workspace_to_reorder(workspace);
    }
  }

  if let Some(window) = found_window {
    let workspace = window.workspace().context("No workspace")?;

    // Native focus has been synced to the WM's focused container.
    if focused_container == window.clone().into() {
      state.is_focus_synced = true;
      state.pending_sync.queue_workspace_to_reorder(workspace);
      return Ok(());
    }

    info!("Window manually focused: {window}");

    // Handle focus events from windows on hidden workspaces. For example,
    // if Discord is forcefully shown by the OS when it's on a hidden
    // workspace, switch focus to Discord's workspace.
    if window.display_state() == DisplayState::Hidden {
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

    if matches!(
      window.state(),
      wm_common::WindowState::Floating(config) if !config.shown_on_top
    ) {
      pending_sync
        .queue_focused_window_to_bring_to_front(&workspace, window.id());
    }

    // Broadcast the focus change event.
    state.emit_event(WmEvent::FocusChanged {
      focused_container: window.to_dto()?,
    });
  }

  Ok(())
}

/// Returns true if focus should be reassigned to the WM's focus container.
fn should_override_focus(state: &WmState) -> bool {
  let has_recent_unmanage = state
    .unmanaged_or_minimized_timestamp
    .is_some_and(|time| time.elapsed().as_millis() < 100);

  has_recent_unmanage && !state.is_focus_synced
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::{
    GapsConfig, TilingDirection, WindowState, WorkspaceConfig,
  };
  use wm_platform::{
    EventLoop, NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
    WindowId,
  };

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
  fn e2e_ignored_window_alt_tab_is_not_overridden_after_recent_unmanage() {
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

    let tiled_window = TilingWindow::new(
      None,
      NativeWindow::from_handle(1),
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
      NativeWindow::from_handle(2),
      test_properties("detached peer"),
      WindowState::Floating(Default::default()),
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
      &NativeWindow::from_handle(3),
      &mut state,
      &mut config,
    )
    .expect("handle unmanaged popup focus");

    assert_eq!(state.pending_sync.workspaces_to_reorder().len(), 1);
    assert!(!state.is_focus_synced);

    // An explicitly ignored window is allowed to own native focus. Work
    // queued before that event must be dropped, and draining it must not
    // rebuild the normal z-order chain over the ignored foreground.
    state.pending_sync.clear();
    state.ignored_windows.push(NativeWindow::from_handle(3));
    state.unmanaged_or_minimized_timestamp =
      Some(std::time::Instant::now());
    state
      .pending_sync
      .queue_workspace_to_reorder(workspace.clone());
    state.pending_sync.queue_focus_change();
    state.pending_sync.queue_focused_effect_update();
    state.pending_sync.queue_cursor_jump();
    state.pending_sync.queue_focused_window_to_bring_to_front(
      &workspace,
      detached_window.id(),
    );
    state
      .pending_sync
      .queue_container_to_redraw(tiled_window.clone());
    let sentinel = vec![WindowId(9)];
    state
      .set_normal_z_order_for_workspace(workspace.id(), sentinel.clone());

    handle_window_focused(
      &NativeWindow::from_handle(3),
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
    assert_eq!(state.ignored_native_foreground, Some(WindowId(3)));
    assert!(!state.is_focus_synced);

    // A redraw queued alongside the stale reorder still drains, but it
    // must not replace the saved chain or focus the tiled window.
    crate::commands::general::platform_sync(&mut state, &config)
      .expect("drain pending work after ignored focus");
    assert_eq!(
      state.normal_z_order_for_workspace(workspace.id()),
      Some(sentinel.as_slice())
    );
    assert_eq!(state.ignored_native_foreground, Some(WindowId(3)));
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
      Some(sentinel.as_slice())
    );
    assert!(!state.pending_sync.needs_focus_update());
    assert!(state.pending_sync.workspaces_to_reorder().is_empty());

    state.unmanaged_or_minimized_timestamp = None;
    handle_window_focused(
      &NativeWindow::from_handle(1),
      &mut state,
      &mut config,
    )
    .expect("return focus to the tiled window");
    assert_eq!(state.ignored_native_foreground, None);
    assert_eq!(state.pending_sync.workspaces_to_reorder().len(), 1);
  }
}
