use wm_platform::NativeWindow;
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  traits::{CommonGetters, WindowGetters},
  wm_state::WmState,
};

/// Minimizes the windows belonging to the currently focused workspace.
///
/// Managed windows are selected from the workspace tree, so this includes
/// tiled and detached windows even when their native window is not
/// currently visible. Visible unmanaged windows are selected from the
/// monitor displaying the focused workspace. Desktop and shell helper
/// windows are excluded.
pub fn show_desktop(state: &WmState) -> anyhow::Result<()> {
  let current_workspace = state
    .focused_container()
    .and_then(|container| container.workspace())
    .ok_or_else(|| anyhow::anyhow!("No focused workspace."))?;
  let current_monitor = current_workspace
    .monitor()
    .ok_or_else(|| anyhow::anyhow!("Focused workspace has no monitor."))?;

  let mut windows = current_workspace
    .descendants()
    .filter_map(|container| container.as_window_container().ok())
    .map(|window| window.native().clone())
    .collect::<Vec<_>>();

  // visible_windows contains top-level windows that are not necessarily
  // managed by GlazeWM. On a displayed workspace, an unmanaged visible
  // window on the workspace's monitor is part of the current desktop
  // too.
  for window in state.dispatcher.visible_windows()? {
    if state.window_from_native(&window).is_some()
      || is_desktop_or_shell_window(&window)
      || state
        .nearest_monitor(&window)
        .is_none_or(|monitor| monitor.id() != current_monitor.id())
    {
      continue;
    }

    if !windows
      .iter()
      .any(|candidate| candidate.id() == window.id())
    {
      windows.push(window);
    }
  }

  let mut minimized = 0;
  let mut failed = 0;
  for window in windows {
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
