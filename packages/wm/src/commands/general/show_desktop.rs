use std::time::Instant;

use wm_platform::NativeWindow;
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;

use crate::{
  commands::container::set_focused_descendant,
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

  let mut minimized = 0;
  let mut failed = 0;
  for window in managed_windows {
    match minimize_managed_window(&window, state) {
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

/// Minimizes a managed window without changing its WM tree state.
///
/// Show Desktop is transient: the native window is minimized, but a tiled
/// window remains in its split tree so restoring it cannot depend on a
/// stale insertion target pointing at a detached split container.
fn minimize_managed_window(
  window: &WindowContainer,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  let was_minimized = window.native().is_minimized()?;

  if !was_minimized {
    state.mark_show_desktop_minimized(window.id());
    if let Err(err) = window.native().minimize() {
      state.clear_show_desktop_minimized(window.id());
      return Err(err.into());
    }
  }

  window.update_native_properties(|properties| {
    properties.is_minimized = true;
  });

  Ok(!was_minimized)
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
