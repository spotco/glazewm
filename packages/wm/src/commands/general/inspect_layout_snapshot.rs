use wm_common::{
  build_layout_match_report, collect_snapshot_windows_for_match,
  LayoutMatchReport, LayoutMatchWindow, LayoutSnapshot, MatchableIdentity,
  LAYOUT_SNAPSHOT_VERSION,
};

use crate::{
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  wm_state::WmState,
};

/// Dry-run match of `snapshot` against current managed windows.
///
/// Does **not** mutate WM state. Reuses the same scoring as restore.
pub fn inspect_layout_snapshot(
  snapshot: &LayoutSnapshot,
  state: &WmState,
) -> anyhow::Result<LayoutMatchReport> {
  if snapshot.version != LAYOUT_SNAPSHOT_VERSION {
    anyhow::bail!(
      "Unsupported layout snapshot version {} (expected {}).",
      snapshot.version,
      LAYOUT_SNAPSHOT_VERSION
    );
  }

  let snapshot_windows = collect_snapshot_windows_for_match(snapshot);
  let live_windows: Vec<LayoutMatchWindow> = state
    .windows()
    .iter()
    .map(|window| live_match_window(window))
    .collect();

  Ok(build_layout_match_report(&snapshot_windows, &live_windows))
}

fn live_match_window(window: &WindowContainer) -> LayoutMatchWindow {
  let identity = live_identity(window);
  let workspace = window.workspace().map(|ws| ws.config().name);
  LayoutMatchWindow {
    key: window.id().to_string(),
    process_name: identity.process_name,
    process_path: identity.process_path,
    class_name: identity.class_name,
    title: identity.title,
    workspace,
  }
}

fn live_identity(window: &WindowContainer) -> MatchableIdentity {
  let props = window.native_properties();
  MatchableIdentity {
    process_path: props.process_path,
    process_name: props.process_name,
    #[cfg(target_os = "windows")]
    class_name: if props.class_name.is_empty() {
      None
    } else {
      Some(props.class_name)
    },
    #[cfg(not(target_os = "windows"))]
    class_name: None,
    title: if props.title.is_empty() {
      None
    } else {
      Some(props.title)
    },
  }
}

