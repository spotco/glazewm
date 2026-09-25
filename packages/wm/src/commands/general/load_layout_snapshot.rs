use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Context;
use tracing::info;

use super::layout_debug_log;
use uuid::Uuid;
use wm_common::{
  match_windows, plan_workspace_tiling_layout, LayoutPlanNode,
  LayoutSnapshot, MatchableIdentity, MatchableWindow, SkipReason,
  SnapshotBounds, SnapshotMonitor, SnapshotNode, SnapshotNodeKind,
  SnapshotWindow, WindowState, WorkspaceLayoutPlan,
};

use crate::{
  commands::{
    container::{
      attach_container, detach_container, flatten_split_container,
      set_focused_descendant,
    },
    monitor::move_workspace_to_monitor,
    window::{move_window_to_workspace, update_window_state},
    workspace::{activate_workspace, focus_workspace},
  },
  models::{
    Container, Monitor, SplitContainer, TilingContainer, WindowContainer,
    WorkspaceTarget,
  },
  traits::{CommonGetters, TilingDirectionGetters, TilingSizeGetters, WindowGetters},
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
  pub tiling_trees_restored: usize,
  pub tiling_windows_placed: usize,
}

/// Placement metadata for a snapshot window leaf.
#[derive(Clone, Debug)]
struct SnapshotLeaf {
  key: String,
  local_id: String,
  window: SnapshotWindow,
  workspace_name: String,
  /// DFS order among window leaves in the workspace.
  order_index: usize,
}

/// Best-effort restore of `snapshot` onto the current desktop.
///
/// Does **not** launch missing applications. Ignored snapshot windows are
/// left alone. Matched tiling windows are re-shaped into the snapshot's
/// nested split tree (order, sizes, directions) with missing leaves pruned.
pub fn load_layout_snapshot(
  snapshot: &LayoutSnapshot,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<LoadLayoutSummary> {
  if let Err(msg) = wm_common::validate_layout_snapshot_version(snapshot) {
    anyhow::bail!("{msg}");
  }

  layout_debug_log(format!(
    "restore begin: snapshot monitors={}, version={}",
    snapshot.monitors.len(),
    snapshot.version
  ));

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

  // Workspace names are global/unique: move (or activate) each named
  // workspace onto its matched live monitor before placing windows.
  if let Err(err) =
    ensure_workspaces_on_matched_monitors(&monitor_map, state, config)
  {
    tracing::warn!(
      "Failed to reassign workspaces to matched monitors: {err:#}"
    );
  }

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
      let msg = format!(
        "Failed to restore window '{}' -> {}: {err:#}",
        snap_key,
        live_key
      );
      tracing::warn!("{msg}");
      layout_debug_log(&msg);
    }
  }

  // Rebuild nested tiling geometry per workspace (order, sizes, splits).
  if let Err(err) = restore_tiling_layouts(
    snapshot,
    &monitor_map,
    &ordered_pairs,
    &leaf_by_key,
    state,
    config,
    &mut summary,
  ) {
    tracing::warn!("Failed to restore tiling layouts: {err:#}");
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

  let msg = format!(
    "Layout snapshot load summary: matched={}, unmatched_snapshot={}, unmatched_live={}, workspace_moves={}, state_updates={}, tiling_trees_restored={}, tiling_windows_placed={}",
    summary.matched,
    summary.unmatched_snapshot,
    summary.unmatched_live,
    summary.workspace_moves,
    summary.state_updates,
    summary.tiling_trees_restored,
    summary.tiling_windows_placed
  );
  info!("{msg}");
  layout_debug_log(&msg);

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

  // For Minimized restore, apply snapshot prev_state first (so the later
  // minimize event records it), then minimize; also set_prev_state so
  // unminimize returns to Floating/Tiling/etc. from the snapshot.
  //
  // For non-minimized targets, compare full WindowState (not just the
  // discriminant) so Floating.centered/shown_on_top and
  // Fullscreen.maximized/shown_on_top round-trip. Always re-apply snapshot
  // prev_state so exiting fullscreen/unminimize uses the saved previous
  // state rather than the live pre-load state.
  let window = if matches!(target_state, WindowState::Minimized) {
    let mut window = window;
    if let Some(prev) = &leaf.window.prev_state {
      if !window.state().is_same_state(prev)
        && !matches!(window.state(), WindowState::Minimized)
      {
        window =
          update_window_state(window, prev.clone(), state, config)?;
        summary.state_updates += 1;
      }
    }

    if !matches!(window.state(), WindowState::Minimized) {
      window = update_window_state(
        window,
        WindowState::Minimized,
        state,
        config,
      )?;
      summary.state_updates += 1;
    }

    if let Some(prev) = &leaf.window.prev_state {
      window.set_prev_state(prev.clone());
    }
    window
  } else {
    let window = if window.state() != target_state {
      let window =
        update_window_state(window, target_state.clone(), state, config)?;
      summary.state_updates += 1;
      window
    } else {
      window
    };

    if let Some(prev) = &leaf.window.prev_state {
      window.set_prev_state(prev.clone());
    }
    window
  };

  if matches!(target_state, WindowState::Floating(_)) {
    if let Some(placement) = &leaf.window.floating_placement {
      // Full Rect restore (X/Y/W/H). set_window_position alone preserves
      // live width/height and cannot round-trip a resized float.
      window.set_floating_placement(placement.clone());
      window.set_has_custom_floating_placement(true);
      state.pending_sync.queue_container_to_redraw(window);
    }
  }

  Ok(())
}

/// Rebuild nested tiling trees for every snapshot workspace that has at
/// least one matched tiling window.
fn restore_tiling_layouts(
  snapshot: &LayoutSnapshot,
  monitor_map: &[(SnapshotMonitor, Monitor)],
  pairs: &[(String, String)],
  leaf_by_key: &HashMap<String, &SnapshotLeaf>,
  state: &mut WmState,
  config: &UserConfig,
  summary: &mut LoadLayoutSummary,
) -> anyhow::Result<()> {
  // snap leaf key -> live window uuid
  let mut live_by_snap_key: HashMap<String, Uuid> = HashMap::new();
  for (snap_key, live_key) in pairs {
    if let Ok(id) = Uuid::parse_str(live_key) {
      live_by_snap_key.insert(snap_key.clone(), id);
    }
  }

  // Per workspace: matched tiling local_id -> live uuid
  let mut matched_tiling_by_ws: HashMap<String, HashMap<String, Uuid>> =
    HashMap::new();

  for (snap_key, live_id) in &live_by_snap_key {
    let Some(leaf) = leaf_by_key.get(snap_key) else {
      continue;
    };
    if !matches!(leaf.window.state, WindowState::Tiling) {
      continue;
    }
    matched_tiling_by_ws
      .entry(leaf.workspace_name.clone())
      .or_default()
      .insert(leaf.local_id.clone(), *live_id);
  }

  let mut seen_workspaces: HashSet<String> = HashSet::new();
  let mut workspace_plans: Vec<(WorkspaceLayoutPlan, HashMap<String, Uuid>)> =
    Vec::new();

  let mut consider_workspace =
    |ws: &wm_common::SnapshotWorkspace,
     matched: &HashMap<String, Uuid>| {
      if !seen_workspaces.insert(ws.name.clone()) {
        return;
      }
      let matched_ids: HashSet<String> = matched.keys().cloned().collect();
      let plan = plan_workspace_tiling_layout(ws, &matched_ids);
      for skip in &plan.skipped {
        if skip.reason == SkipReason::MissingLiveMatch {
          let msg = format!(
            "Layout restore: skipping missing tiling window '{}' ({}) on workspace '{}'",
            skip.process_name,
            skip.local_id,
            ws.name
          );
          tracing::info!("{msg}");
          layout_debug_log(&msg);
        }
      }
      if !plan.is_empty() {
        workspace_plans.push((plan, matched.clone()));
      }
    };

  for (snap_mon, _) in monitor_map {
    for workspace in &snap_mon.workspaces {
      let matched = matched_tiling_by_ws
        .get(&workspace.name)
        .cloned()
        .unwrap_or_default();
      consider_workspace(workspace, &matched);
    }
  }
  for snap_mon in &snapshot.monitors {
    for workspace in &snap_mon.workspaces {
      let matched = matched_tiling_by_ws
        .get(&workspace.name)
        .cloned()
        .unwrap_or_default();
      consider_workspace(workspace, &matched);
    }
  }

  for (plan, local_to_live) in workspace_plans {
    match apply_workspace_tiling_plan(&plan, &local_to_live, state, config)
    {
      Ok(placed) => {
        summary.tiling_trees_restored += 1;
        summary.tiling_windows_placed += placed;
        let msg = format!(
          "Restored tiling tree on workspace '{}': {} windows, {} root children, direction={:?}",
          plan.workspace_name,
          placed,
          plan.children.len(),
          plan.tiling_direction
        );
        info!("{msg}");
        layout_debug_log(&msg);
      }
      Err(err) => {
        let msg = format!(
          "Failed to restore tiling tree on workspace '{}': {err:#}",
          plan.workspace_name
        );
        tracing::warn!("{msg}");
        layout_debug_log(&msg);
      }
    }
  }

  Ok(())
}

fn apply_workspace_tiling_plan(
  plan: &WorkspaceLayoutPlan,
  local_to_live: &HashMap<String, Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<usize> {
  let workspace = state
    .workspace_by_name(&plan.workspace_name)
    .with_context(|| {
      format!("Workspace '{}' not found during tiling restore.", plan.workspace_name)
    })?;

  // Detach planned tiling windows so we can rebuild the tree cleanly.
  let mut detached: HashMap<String, WindowContainer> = HashMap::new();
  for local_id in plan.window_local_ids() {
    let Some(live_id) = local_to_live.get(&local_id).copied() else {
      tracing::warn!(
        "Plan window local_id '{}' has no live match on '{}'",
        local_id,
        plan.workspace_name
      );
      continue;
    };
    let Some(mut window) =
      state.windows().into_iter().find(|w| w.id() == live_id)
    else {
      continue;
    };

    // Should already be tiling after restore_window; force if needed.
    if !matches!(window, WindowContainer::TilingWindow(_)) {
      let _ = update_window_state(
        window,
        WindowState::Tiling,
        state,
        config,
      )?;
      window = state
        .windows()
        .into_iter()
        .find(|w| w.id() == live_id)
        .context("Window missing after forcing tiling.")?;
    }

    if !matches!(window, WindowContainer::TilingWindow(_)) {
      tracing::warn!(
        "Could not convert window {} to tiling for layout restore",
        live_id
      );
      continue;
    }

    if window.parent().is_some() {
      detach_container(window.clone().into())?;
    }
    detached.insert(local_id, window);
  }

  // Flatten any split leftovers (e.g. from unmatched live windows).
  flatten_all_splits(&workspace.clone().into())?;

  workspace.set_tiling_direction(plan.tiling_direction.clone());

  let mut local_to_container: HashMap<String, Uuid> = HashMap::new();
  attach_plan_nodes(
    &workspace.clone().into(),
    &plan.children,
    &mut detached,
    &mut local_to_container,
    config,
    0,
  )?;

  if !detached.is_empty() {
    let msg = format!(
      "Tiling restore on '{}': {} planned windows were not re-attached",
      plan.workspace_name,
      detached.len()
    );
    tracing::warn!("{msg}");
    layout_debug_log(&msg);
  }

  apply_plan_sizes(&plan.children, &local_to_container, state)?;
  apply_child_focus_order(
    &workspace.clone().into(),
    &plan.child_focus_order,
    &local_to_container,
  );
  apply_focus_orders_recursive(&plan.children, &local_to_container, state)?;

  // Focus the first window in workspace focus order when available.
  if let Some(first_local) = plan.child_focus_order.first() {
    if let Some(id) = local_to_container.get(first_local) {
      if let Some(window) =
        state.windows().into_iter().find(|w| w.id() == *id)
      {
        set_focused_descendant(&window.into(), None);
        state.pending_sync.queue_focus_change();
      } else if let Some(split) = find_split_by_id(state, *id) {
        if let Some(focus_win) = split.descendant_focus_order().next() {
          set_focused_descendant(&focus_win, None);
          state.pending_sync.queue_focus_change();
        }
      }
    }
  }

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(plan.window_local_ids().len())
}

fn attach_plan_nodes(
  parent: &Container,
  nodes: &[LayoutPlanNode],
  detached: &mut HashMap<String, WindowContainer>,
  local_to_container: &mut HashMap<String, Uuid>,
  config: &UserConfig,
  start_index: usize,
) -> anyhow::Result<()> {
  let mut index = start_index;
  for node in nodes {
    match node {
      LayoutPlanNode::Window { local_id, .. } => {
        let window = detached.remove(local_id).with_context(|| {
          format!("Detached window '{local_id}' missing during attach.")
        })?;
        let window_id = window.id();
        attach_container(
          &window.clone().into(),
          parent,
          Some(index),
        )?;
        local_to_container.insert(local_id.clone(), window_id);
        index += 1;
      }
      LayoutPlanNode::Split {
        local_id,
        tiling_direction,
        children,
        ..
      } => {
        let split = SplitContainer::new(
          tiling_direction.clone(),
          config.value.gaps.clone(),
        );
        let split_id = split.id();
        attach_container(&split.clone().into(), parent, Some(index))?;
        local_to_container.insert(local_id.clone(), split_id);
        attach_plan_nodes(
          &split.into(),
          children,
          detached,
          local_to_container,
          config,
          0,
        )?;
        index += 1;
      }
    }
  }
  Ok(())
}

fn apply_plan_sizes(
  nodes: &[LayoutPlanNode],
  local_to_container: &HashMap<String, Uuid>,
  state: &WmState,
) -> anyhow::Result<()> {
  // Direct set (not resize_tiling_container) so sibling ratios match the
  // plan exactly without fighting proportional redistribution.
  for node in nodes {
    let Some(id) = local_to_container.get(node.local_id()) else {
      continue;
    };
    if let Some(tiling) = find_tiling_by_id(state, *id) {
      tiling.set_tiling_size(node.tiling_size());
    }
  }

  for node in nodes {
    if let LayoutPlanNode::Split { children, .. } = node {
      apply_plan_sizes(children, local_to_container, state)?;
    }
  }
  Ok(())
}

fn apply_focus_orders_recursive(
  nodes: &[LayoutPlanNode],
  local_to_container: &HashMap<String, Uuid>,
  state: &WmState,
) -> anyhow::Result<()> {
  for node in nodes {
    if let LayoutPlanNode::Split {
      local_id,
      children,
      child_focus_order,
      ..
    } = node
    {
      if let Some(id) = local_to_container.get(local_id) {
        if let Some(split) = find_split_by_id(state, *id) {
          apply_child_focus_order(
            &split.into(),
            child_focus_order,
            local_to_container,
          );
        }
      }
      apply_focus_orders_recursive(
        children,
        local_to_container,
        state,
      )?;
    }
  }
  Ok(())
}

fn apply_child_focus_order(
  parent: &Container,
  focus_local_ids: &[String],
  local_to_container: &HashMap<String, Uuid>,
) {
  let child_ids: HashSet<Uuid> =
    parent.children().iter().map(CommonGetters::id).collect();

  let mut new_order: VecDeque<Uuid> = VecDeque::new();
  for local_id in focus_local_ids {
    if let Some(id) = local_to_container.get(local_id) {
      if child_ids.contains(id) && !new_order.contains(id) {
        new_order.push_back(*id);
      }
    }
  }
  for child in parent.children() {
    if !new_order.contains(&child.id()) {
      new_order.push_back(child.id());
    }
  }
  *parent.borrow_child_focus_order_mut() = new_order;
}

fn flatten_all_splits(parent: &Container) -> anyhow::Result<()> {
  loop {
    let mut splits: Vec<SplitContainer> = parent
      .descendants()
      .filter_map(|c| c.as_split().cloned())
      .collect();
    if splits.is_empty() {
      break;
    }
    splits.sort_by_key(|s| std::cmp::Reverse(s.ancestors().count()));
    let before = splits.len();
    for split in splits {
      if split.parent().is_some() {
        flatten_split_container(split)?;
      }
    }
    // Safety: avoid infinite loop if something fails to detach.
    let remaining = parent
      .descendants()
      .filter(|c| c.is_split())
      .count();
    if remaining >= before {
      tracing::warn!(
        "flatten_all_splits made no progress ({} splits remain)",
        remaining
      );
      break;
    }
  }
  Ok(())
}

fn find_tiling_by_id(
  state: &WmState,
  id: Uuid,
) -> Option<TilingContainer> {
  for window in state.windows() {
    if window.id() == id {
      return window.as_tiling_container().ok();
    }
  }
  for monitor in state.monitors() {
    for descendant in monitor.descendants() {
      if descendant.id() == id {
        return descendant.as_tiling_container().ok();
      }
    }
  }
  None
}

fn find_split_by_id(state: &WmState, id: Uuid) -> Option<SplitContainer> {
  for monitor in state.monitors() {
    for descendant in monitor.descendants() {
      if descendant.id() == id {
        return descendant.as_split().cloned();
      }
    }
  }
  None
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

/// Ensure each snapshot workspace lands on its matched live monitor.
///
/// GlazeWM workspace names are globally unique (`workspace_by_name`), so
/// `WorkspaceTarget::Name` alone cannot express "workspace X on monitor A".
/// We therefore reassign (or activate) the named workspace onto the matched
/// monitor before `move_window_to_workspace` places windows.
///
/// Placement is derived from the snapshot monitor→workspace structure (via
/// `monitor_map`), **including empty workspaces** that produce no window
/// leaves. Leaf-driven planning alone would skip those.
fn ensure_workspaces_on_matched_monitors(
  monitor_map: &[(SnapshotMonitor, Monitor)],
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut planned: HashMap<String, Monitor> = HashMap::new();
  for (snap_mon, live_mon) in monitor_map {
    for workspace in &snap_mon.workspaces {
      planned
        .entry(workspace.name.clone())
        .or_insert_with(|| live_mon.clone());
    }
  }

  for (ws_name, target_mon) in planned {
    if let Some(workspace) = state.workspace_by_name(&ws_name) {
      let current_mon =
        workspace.monitor().context("Workspace has no monitor.")?;
      if current_mon.id() != target_mon.id() {
        let msg = format!(
          "Moving workspace '{}' to matched monitor '{}'.",
          ws_name,
          target_mon.native_properties().device_name
        );
        info!("{msg}");
        layout_debug_log(&msg);
        move_workspace_to_monitor(
          &workspace,
          &target_mon,
          state,
          config,
        )?;
      }
    } else {
      let msg = format!(
        "Activating workspace '{}' on matched monitor '{}'.",
        ws_name,
        target_mon.native_properties().device_name
      );
      info!("{msg}");
      layout_debug_log(&msg);
      activate_workspace(
        Some(&ws_name),
        Some(target_mon),
        state,
        config,
      )?;
    }
  }

  Ok(())
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
          local_id: node.local_id.clone(),
          window: window.clone(),
          workspace_name: workspace_name.to_string(),
          order_index: *order,
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

