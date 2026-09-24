use std::collections::{HashMap, HashSet};

use anyhow::Context;
use tracing::info;
use uuid::Uuid;
use wm_common::{
  match_windows, LayoutSnapshot, MatchableIdentity, MatchableWindow,
  SnapshotBounds, SnapshotMonitor, SnapshotNode, SnapshotNodeKind,
  SnapshotWindow, WindowState, LAYOUT_SNAPSHOT_VERSION,
};

use crate::{
  commands::{
    container::{
      move_container_within_tree, resize_tiling_container,
      set_focused_descendant,
    },
    window::{
      move_window_to_workspace, set_window_position, update_window_state,
      WindowPositionTarget,
    },
    workspace::focus_workspace,
  },
  models::{
    Monitor, TilingContainer, WindowContainer, WorkspaceTarget,
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Summary of a best-effort layout restore.
#[derive(Clone, Debug, Default)]
pub struct LoadLayoutSummary {
  pub matched: usize,
  pub unmatched_snapshot: usize,
  pub unmatched_live: usize,
  pub state_updates: usize,
  pub workspace_moves: usize,
}

/// Placement metadata for a snapshot window leaf.
#[derive(Clone, Debug)]
struct SnapshotLeaf {
  key: String,
  window: SnapshotWindow,
  workspace_name: String,
  /// DFS order among window leaves in the workspace.
  order_index: usize,
  tiling_size: Option<f32>,
}

/// Best-effort restore of `snapshot` onto the current desktop.
///
/// Does **not** launch missing applications. Ignored snapshot windows are
/// left alone. Nested-split isomorphism is not required — workspace
/// assignment and window state are prioritized.
pub fn load_layout_snapshot(
  snapshot: &LayoutSnapshot,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<LoadLayoutSummary> {
  if snapshot.version != LAYOUT_SNAPSHOT_VERSION {
    anyhow::bail!(
      "Unsupported layout snapshot version {} (expected {}).",
      snapshot.version,
      LAYOUT_SNAPSHOT_VERSION
    );
  }

  let monitor_map = match_monitors(&snapshot.monitors, &state.monitors());
  let leaves = collect_snapshot_leaves(snapshot, &monitor_map);

  let live_windows = state.windows();
  let live_matchables: Vec<MatchableWindow> = live_windows
    .iter()
    .map(|window| MatchableWindow {
      key: window.id().to_string(),
      identity: live_identity(window),
    })
    .collect();

  let snap_matchables: Vec<MatchableWindow> = leaves
    .iter()
    .map(|leaf| MatchableWindow {
      key: leaf.key.clone(),
      identity: MatchableIdentity::from(&leaf.window),
    })
    .collect();

  let pairs = match_windows(&snap_matchables, &live_matchables);
  let matched_keys: HashSet<_> =
    pairs.iter().map(|(s, _)| s.clone()).collect();
  let matched_live: HashSet<_> =
    pairs.iter().map(|(_, l)| l.clone()).collect();

  let leaf_by_key: HashMap<_, _> =
    leaves.iter().map(|l| (l.key.clone(), l)).collect();

  let mut summary = LoadLayoutSummary {
    matched: pairs.len(),
    unmatched_snapshot: leaves
      .iter()
      .filter(|l| !matched_keys.contains(&l.key))
      .count(),
    unmatched_live: live_matchables
      .iter()
      .filter(|l| !matched_live.contains(&l.key))
      .count(),
    ..Default::default()
  };

  // Sort matched pairs by workspace + order so tiling inserts are stabler.
  let mut ordered_pairs = pairs;
  ordered_pairs.sort_by(|(a, _), (b, _)| {
    let la = leaf_by_key.get(a);
    let lb = leaf_by_key.get(b);
    match (la, lb) {
      (Some(a), Some(b)) => a
        .workspace_name
        .cmp(&b.workspace_name)
        .then(a.order_index.cmp(&b.order_index)),
      _ => std::cmp::Ordering::Equal,
    }
  });

  for (snap_key, live_key) in &ordered_pairs {
    let Some(leaf) = leaf_by_key.get(snap_key) else {
      continue;
    };
    let Ok(live_id) = Uuid::parse_str(live_key) else {
      continue;
    };

    let Some(window) =
      state.windows().into_iter().find(|w| w.id() == live_id)
    else {
      continue;
    };

    if let Err(err) =
      restore_window(leaf, window, state, config, &mut summary)
    {
      tracing::warn!(
        "Failed to restore window '{}' -> {}: {err:#}",
        snap_key,
        live_key
      );
    }
  }

  if let Err(err) = apply_tiling_sizes(&ordered_pairs, &leaf_by_key, state)
  {
    tracing::warn!("Failed to apply tiling sizes: {err:#}");
  }

  // Activate focused workspaces from snapshot where monitors matched.
  for (snap_mon, live_mon) in &monitor_map {
    if let Some(name) = &snap_mon.focused_workspace_name {
      if let Err(err) = focus_workspace(
        WorkspaceTarget::Name(name.clone()),
        state,
        config,
      ) {
        tracing::warn!(
          "Failed to focus workspace '{}' on monitor {}: {err:#}",
          name,
          live_mon.native_properties().device_name
        );
      }
    }
  }

  info!(
    "Layout snapshot load summary: matched={}, unmatched_snapshot={}, unmatched_live={}, workspace_moves={}, state_updates={}",
    summary.matched,
    summary.unmatched_snapshot,
    summary.unmatched_live,
    summary.workspace_moves,
    summary.state_updates
  );

  Ok(summary)
}

fn restore_window(
  leaf: &SnapshotLeaf,
  window: WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
  summary: &mut LoadLayoutSummary,
) -> anyhow::Result<()> {
  let window_id = window.id();
  let current_ws = window
    .workspace()
    .map(|ws| ws.config().name)
    .unwrap_or_default();

  if current_ws != leaf.workspace_name {
    move_window_to_workspace(
      window,
      WorkspaceTarget::Name(leaf.workspace_name.clone()),
      state,
      config,
    )?;
    summary.workspace_moves += 1;
  }

  let window = state
    .windows()
    .into_iter()
    .find(|w| w.id() == window_id)
    .context("Window disappeared during restore.")?;

  let target_state = leaf.window.state.clone();
  if !window.state().is_same_state(&target_state) {
    let window =
      update_window_state(window, target_state.clone(), state, config)?;
    summary.state_updates += 1;

    if matches!(target_state, WindowState::Floating(_)) {
      if let Some(placement) = &leaf.window.floating_placement {
        set_window_position(
          window,
          &WindowPositionTarget::Coordinates(
            Some(placement.x()),
            Some(placement.y()),
          ),
          state,
        )?;
      }
    }
  } else if matches!(target_state, WindowState::Floating(_)) {
    if let Some(placement) = &leaf.window.floating_placement {
      set_window_position(
        window,
        &WindowPositionTarget::Coordinates(
          Some(placement.x()),
          Some(placement.y()),
        ),
        state,
      )?;
    }
  }

  Ok(())
}

fn apply_tiling_sizes(
  pairs: &[(String, String)],
  leaf_by_key: &HashMap<String, &SnapshotLeaf>,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let mut by_ws: HashMap<String, Vec<(Uuid, f32, usize)>> = HashMap::new();

  for (snap_key, live_key) in pairs {
    let Some(leaf) = leaf_by_key.get(snap_key) else {
      continue;
    };
    let Some(size) = leaf.tiling_size else {
      continue;
    };
    if !matches!(leaf.window.state, WindowState::Tiling) {
      continue;
    }
    let Ok(id) = Uuid::parse_str(live_key) else {
      continue;
    };
    by_ws
      .entry(leaf.workspace_name.clone())
      .or_default()
      .push((id, size, leaf.order_index));
  }

  for (ws_name, mut entries) in by_ws {
    entries.sort_by_key(|e| e.2);

    let Some(workspace) = state.workspace_by_name(&ws_name) else {
      continue;
    };

    // Direct tiling window children of the workspace (best-effort; ignores
    // nested split structure).
    let tiling_windows: Vec<_> = workspace
      .tiling_children()
      .filter_map(|c| match c {
        TilingContainer::TilingWindow(w) => Some(w),
        TilingContainer::Split(_) => None,
      })
      .collect();

    for (target_index, (id, _size, _)) in entries.iter().enumerate() {
      let Some(window) =
        tiling_windows.iter().find(|w| w.id() == *id).cloned()
      else {
        continue;
      };

      let parent = window.parent().context("No parent.")?;
      let desired =
        target_index.min(parent.child_count().saturating_sub(1));
      if window.index() != desired {
        let _ = move_container_within_tree(
          &window.clone().into(),
          &parent,
          desired,
          state,
        );
      }
    }

    for (id, size, _) in &entries {
      let Some(window) =
        state.windows().into_iter().find(|w| w.id() == *id)
      else {
        continue;
      };
      let WindowContainer::TilingWindow(tw) = window else {
        continue;
      };
      let tiling: TilingContainer = tw.into();
      resize_tiling_container(&tiling, *size);
      let redraw: Vec<TilingContainer> =
        tiling.tiling_siblings().chain([tiling.clone()]).collect();
      state.pending_sync.queue_containers_to_redraw(redraw);
    }

    if let Some((id, _, _)) = entries.first() {
      if let Some(window) =
        state.windows().into_iter().find(|w| w.id() == *id)
      {
        set_focused_descendant(&window.into(), None);
        state.pending_sync.queue_focus_change();
      }
    }
  }

  Ok(())
}

fn live_identity(window: &WindowContainer) -> MatchableIdentity {
  let props = window.native_properties();
  MatchableIdentity {
    process_path: props.process_path,
    process_name: props.process_name,
    class_name: if props.class_name.is_empty() {
      None
    } else {
      Some(props.class_name)
    },
    title: if props.title.is_empty() {
      None
    } else {
      Some(props.title)
    },
  }
}

/// Match snapshot monitors to live monitors.
fn match_monitors(
  snapshot: &[SnapshotMonitor],
  live: &[Monitor],
) -> Vec<(SnapshotMonitor, Monitor)> {
  let mut used = vec![false; live.len()];
  let mut result = Vec::new();

  for snap in snapshot {
    let best = live
      .iter()
      .enumerate()
      .filter(|(i, _)| !used[*i])
      .map(|(i, m)| (monitor_score(snap, m), i, m))
      .max_by_key(|(s, _, _)| *s);

    if let Some((score, i, m)) = best {
      if score > 0 {
        used[i] = true;
        result.push((snap.clone(), m.clone()));
      }
    }
  }

  // If nothing matched but counts align, zip by index as last resort.
  if result.is_empty()
    && !snapshot.is_empty()
    && snapshot.len() == live.len()
  {
    for (snap, mon) in snapshot.iter().zip(live.iter()) {
      result.push((snap.clone(), mon.clone()));
    }
  }

  result
}

fn monitor_score(snap: &SnapshotMonitor, live: &Monitor) -> u32 {
  let props = live.native_properties();
  let mut score = 0u32;

  #[cfg(target_os = "windows")]
  {
    if let (Some(sh), Some(lh)) = (&snap.hardware_id, &props.hardware_id) {
      if sh.eq_ignore_ascii_case(lh) {
        score += 100;
      }
    }
    if let (Some(sp), Some(lp)) = (&snap.device_path, &props.device_path) {
      if sp.eq_ignore_ascii_case(lp) {
        score += 80;
      }
    }
  }

  if snap.device_name.eq_ignore_ascii_case(&props.device_name) {
    score += 40;
  }
  if bounds_equal(&snap.bounds, &props.bounds) {
    score += 20;
  } else if bounds_overlap(&snap.bounds, &props.bounds) {
    score += 5;
  }

  score
}

fn bounds_equal(a: &SnapshotBounds, b: &wm_platform::Rect) -> bool {
  a.x == b.x()
    && a.y == b.y()
    && a.width == b.width()
    && a.height == b.height()
}

fn bounds_overlap(a: &SnapshotBounds, b: &wm_platform::Rect) -> bool {
  let ax2 = a.x + a.width;
  let ay2 = a.y + a.height;
  let bx2 = b.x() + b.width();
  let by2 = b.y() + b.height();
  a.x < bx2 && ax2 > b.x() && a.y < by2 && ay2 > b.y()
}

fn collect_snapshot_leaves(
  snapshot: &LayoutSnapshot,
  monitor_map: &[(SnapshotMonitor, Monitor)],
) -> Vec<SnapshotLeaf> {
  let mut leaves = Vec::new();
  let mut seen_workspace_keys: HashSet<String> = HashSet::new();

  for (snap_mon, _live_mon) in monitor_map {
    for workspace in &snap_mon.workspaces {
      let ws_key = format!("{}::{}", snap_mon.device_name, workspace.name);
      if !seen_workspace_keys.insert(ws_key) {
        continue;
      }
      let mut order = 0usize;
      collect_node_windows(
        &workspace.root,
        &workspace.name,
        &mut order,
        &mut leaves,
      );
    }
  }

  // Also include windows from unmatched snapshot monitors (workspace name
  // only — still useful when monitor hardware changed).
  for snap_mon in &snapshot.monitors {
    for workspace in &snap_mon.workspaces {
      let ws_key = format!("{}::{}", snap_mon.device_name, workspace.name);
      if !seen_workspace_keys.insert(ws_key) {
        continue;
      }
      let mut order = 0usize;
      collect_node_windows(
        &workspace.root,
        &workspace.name,
        &mut order,
        &mut leaves,
      );
    }
  }

  leaves
}

fn collect_node_windows(
  node: &SnapshotNode,
  workspace_name: &str,
  order: &mut usize,
  out: &mut Vec<SnapshotLeaf>,
) {
  match node.kind {
    SnapshotNodeKind::Window => {
      if let Some(window) = &node.window {
        let key =
          format!("{}::{}::{}", workspace_name, node.local_id, *order);
        out.push(SnapshotLeaf {
          key,
          window: window.clone(),
          workspace_name: workspace_name.to_string(),
          order_index: *order,
          tiling_size: node.tiling_size,
        });
        *order += 1;
      }
    }
    SnapshotNodeKind::Split => {
      if let Some(children) = &node.children {
        for child in children {
          collect_node_windows(child, workspace_name, order, out);
        }
      }
    }
  }
}
