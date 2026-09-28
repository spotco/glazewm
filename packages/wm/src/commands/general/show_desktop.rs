use std::time::Instant;

use wm_common::WindowState;
use wm_platform::NativeWindow;
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  commands::{
    container::set_focused_descendant, window::update_window_state,
  },
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Minimizes the windows belonging to the currently focused workspace.
///
/// Managed windows are selected from the workspace tree, so this includes
/// tiled and detached windows even when their native window is not
/// currently visible. Visible unmanaged windows are selected from the
/// monitor displaying the focused workspace. Desktop and shell helper
/// windows are excluded.
pub fn show_desktop(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let current_workspace = state
    .focused_container()
    .and_then(|container| container.workspace())
    .ok_or_else(|| anyhow::anyhow!("No focused workspace."))?;
  let current_monitor = current_workspace
    .monitor()
    .ok_or_else(|| anyhow::anyhow!("Focused workspace has no monitor."))?;

  let managed_windows = current_workspace
    .descendants()
    .filter_map(|container| container.as_window_container().ok())
    .collect::<Vec<_>>();

  let mut unmanaged_windows: Vec<NativeWindow> = Vec::new();

  // visible_windows contains top-level windows that are not necessarily
  // managed by GlazeWM. On a displayed workspace, an unmanaged visible
  // window on the workspace's monitor is part of the current desktop
  // too.
  for window in state.dispatcher.visible_windows()? {
    if state.window_from_native(&window).is_some()
      || is_desktop_or_shell_window(&window)
      || is_input_method_window(&window)
      || is_zebar_window(&window)
      || state
        .nearest_monitor(&window)
        .is_none_or(|monitor| monitor.id() != current_monitor.id())
    {
      continue;
    }

    if !unmanaged_windows
      .iter()
      .any(|candidate| candidate.id() == window.id())
    {
      unmanaged_windows.push(window);
    }
  }

  if !managed_windows.is_empty() || !unmanaged_windows.is_empty() {
    // Windows can emit a focus event for the next z-order window before
    // its minimize event reaches the WM. Reuse the normal short focus
    // override window so that event cannot switch to another workspace
    // mid-action.
    state.unmanaged_or_minimized_timestamp = Some(Instant::now());
  }

  if !managed_windows.is_empty() && !state.show_desktop_session_active() {
    state.begin_show_desktop_session(super::build_layout_snapshot(state)?);
  }

  let mut minimized = 0;
  let mut failed = 0;
  for window in managed_windows {
    match minimize_managed_window(&window, state, config) {
      Ok(true) => minimized += 1,
      Ok(false) => {}
      Err(err) => {
        failed += 1;
        tracing::warn!(
          "Show desktop failed to minimize window {:?}: {}",
          window.native().id(),
          err
        );
      }
    }
  }

  for window in unmanaged_windows {
    if window.is_minimized().unwrap_or(false) {
      continue;
    }

    match window.minimize() {
      Ok(()) => minimized += 1,
      Err(err) => {
        failed += 1;
        tracing::warn!(
          "Show desktop failed to minimize window {:?}: {}",
          window.id(),
          err
        );
      }
    }
  }

  tracing::info!(
    "Show desktop minimized {minimized} window(s); {failed} failed."
  );

  if state.show_desktop_minimized_count() == 0 {
    state.clear_show_desktop_session();
  }

  // Minimizing the final foreground window can make Windows activate a
  // window from another workspace. Keep the WM on the desktop instead of
  // allowing that fallback activation to switch workspaces.
  if minimized > 0 {
    let workspace_container = current_workspace.clone().into();
    set_focused_descendant(&workspace_container, None);
    state.pending_sync.queue_focus_change();
    super::platform_sync::platform_sync(state, config)?;
  }

  Ok(())
}

/// Applies GlazeWM's normal logical minimized transition to a managed
/// window while a Show Desktop topology snapshot is active.
///
/// The snapshot, rather than detached insertion-target references, is the
/// source of truth for rebuilding the original tree after the session
/// ends.
fn minimize_managed_window(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let was_minimized = window.native().is_minimized()?;

  if was_minimized || window.state() == WindowState::Minimized {
    window.update_native_properties(|properties| {
      properties.is_minimized = was_minimized;
    });
    return Ok(false);
  }

  // Mark before the native call so the asynchronous minimize event cannot
  // destructively update the tree before the session applies its logical
  // minimized state.
  state.mark_show_desktop_minimized(window.id());
  if let Err(err) = window.native().minimize() {
    state.clear_show_desktop_minimized(window.id());
    return Err(err.into());
  }

  window.update_native_properties(|properties| {
    properties.is_minimized = true;
  });

  if let Err(err) = update_window_state(
    window.clone(),
    WindowState::Minimized,
    state,
    config,
  ) {
    state.clear_show_desktop_minimized(window.id());
    return Err(err);
  }

  Ok(!was_minimized)
}

/// Restores the exact pre-Show-Desktop layout after the final managed
/// window leaves the temporary minimized state.
pub fn restore_show_desktop_layout(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(snapshot) = state.take_show_desktop_snapshot() else {
    return Ok(());
  };

  let summary = super::load_layout_snapshot(&snapshot, state, config)?;
  tracing::info!(
    "Restored Show Desktop layout: matched={}, tiling trees={}, windows placed={}",
    summary.matched,
    summary.tiling_trees_restored,
    summary.tiling_windows_placed,
  );

  Ok(())
}

fn is_desktop_or_shell_window(window: &NativeWindow) -> bool {
  if window.is_desktop_window().unwrap_or(false) {
    return true;
  }

  #[cfg(target_os = "windows")]
  {
    // Explorer owns both real folder windows and the shell's
    // desktop/taskbar helper HWNDs. Only the former should participate
    // in show-desktop.
    if window
      .process_name()
      .is_ok_and(|name| name.eq_ignore_ascii_case("explorer"))
    {
      return !window.class_name().is_ok_and(|class_name| {
        matches!(class_name.as_str(), "CabinetWClass" | "ExploreWClass")
      });
    }
  }

  false
}

fn is_zebar_window(window: &NativeWindow) -> bool {
  window
    .process_name()
    .is_ok_and(|name| name.eq_ignore_ascii_case("zebar"))
}

fn is_input_method_window(window: &NativeWindow) -> bool {
  #[cfg(target_os = "windows")]
  {
    window.class_name().is_ok_and(|class_name| {
      matches!(class_name.as_str(), "MSCTFIME UI" | "IME")
    })
  }

  #[cfg(not(target_os = "windows"))]
  {
    let _ = window;
    false
  }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use uuid::Uuid;
  use wm_common::{
    ContainerDto, GapsConfig, TilingDirection, WindowState,
    WorkspaceConfig,
  };
  use wm_platform::{EventLoop, NativeWindowWindowsExt, Rect, RectDelta};

  use super::*;
  use crate::{
    commands::{
      container::attach_container, monitor::add_monitor,
      window::update_window_state,
    },
    events::handle_window_minimize_ended,
    models::{
      Container, NativeMonitorProperties, NativeWindowProperties,
      SplitContainer, TilingWindow, Workspace,
    },
    traits::{CommonGetters, PositionGetters, WindowGetters},
    user_config::UserConfig,
  };

  fn test_config() -> UserConfig {
    let path = std::env::temp_dir()
      .join(format!("glazewm-show-desktop-test-{}.yaml", Uuid::new_v4()));
    UserConfig::new(Some(path)).expect("test config")
  }

  fn test_window(id: u128) -> TilingWindow {
    TilingWindow::new(
      Some(Uuid::from_u128(id)),
      NativeWindow::from_handle(id as isize),
      NativeWindowProperties {
        title: format!("window-{id}"),
        class_name: "test".into(),
        process_name: "test".into(),
        process_path: None,
        frame: Rect::from_xy(0, 0, 100, 100),
        is_minimized: false,
        is_maximized: false,
        is_resizable: true,
        shadow_borders: RectDelta::zero(),
      },
      None,
      RectDelta::zero(),
      Rect::from_xy(0, 0, 100, 100),
      false,
      GapsConfig::default(),
      Vec::new(),
      None,
    )
  }

  #[test]
  #[allow(clippy::too_many_lines)]
  fn show_desktop_restore_uses_single_window_then_rebuilds_snapshot() {
    let (_event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);
    let config = test_config();

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
        name: "show-desktop-test".into(),
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

    let window_one = test_window(1);
    let split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    let window_two = test_window(2);
    let window_three = test_window(3);

    attach_container(
      &window_one.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach window one");
    attach_container(&split.clone().into(), &workspace_container, None)
      .expect("attach split");
    attach_container(
      &window_two.clone().into(),
      &split.clone().into(),
      None,
    )
    .expect("attach window two");
    attach_container(
      &window_three.clone().into(),
      &split.clone().into(),
      None,
    )
    .expect("attach window three");

    let snapshot = crate::commands::general::build_layout_snapshot(&state)
      .expect("capture Show Desktop snapshot");
    state.begin_show_desktop_session(snapshot);

    let managed_windows = state.windows();
    for window in &managed_windows {
      state.mark_show_desktop_minimized(window.id());
      window.update_native_properties(|properties| {
        // Avoid a native call in the model test; this is the state
        // observed after Show Desktop has minimized the window.
        properties.is_minimized = true;
      });
      update_window_state(
        window.clone(),
        WindowState::Minimized,
        &mut state,
        &config,
      )
      .expect("apply logical minimized state");
    }

    assert_eq!(workspace.tiling_children().count(), 0);

    // Restoring one managed window must make it the only active tiled
    // window, so it receives the full workspace rectangle.
    let restored_id = Uuid::from_u128(2);
    let restored = state
      .windows()
      .into_iter()
      .find(|window| window.id() == restored_id)
      .expect("restored window");
    restored.update_native_properties(|properties| {
      properties.is_minimized = false;
    });
    let restored_native = restored.native().clone();
    handle_window_minimize_ended(&restored_native, &mut state, &config)
      .expect("restore one window");

    assert_eq!(workspace.tiling_children().count(), 1);
    let restored = state
      .windows()
      .into_iter()
      .find(|window| window.id() == restored_id)
      .expect("restored tiled window");
    assert_eq!(
      restored.to_rect().expect("restored rectangle"),
      workspace.to_rect().expect("workspace rectangle")
    );

    // Restore the remaining windows and use the real session completion
    // path.
    for id in [Uuid::from_u128(1), Uuid::from_u128(3)] {
      let window = state
        .windows()
        .into_iter()
        .find(|window| window.id() == id)
        .expect("remaining window");
      window.update_native_properties(|properties| {
        properties.is_minimized = false;
      });
      let native = window.native().clone();
      handle_window_minimize_ended(&native, &mut state, &config)
        .expect("restore remaining window");
    }

    assert!(!state.show_desktop_session_active());
    assert_eq!(workspace.tiling_children().count(), 2);
    let children = workspace.children();
    assert!(matches!(children[0], Container::TilingWindow(_)));
    let restored_split =
      children[1].as_split().expect("nested split restored");
    assert_eq!(restored_split.child_count(), 2);

    let dto = workspace.to_dto().expect("workspace dto");
    let ContainerDto::Workspace(dto) = dto else {
      panic!("expected workspace dto");
    };
    assert_eq!(dto.children.len(), 2);
    assert!(matches!(dto.children[1], ContainerDto::Split(_)));
  }
}
