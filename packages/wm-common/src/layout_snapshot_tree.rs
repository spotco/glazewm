//! Pure tiling-tree planning for layout snapshot restore.
//!
//! Prunes missing / non-tiling leaves, collapses redundant splits, and
//! renormalizes sibling `tiling_size` ratios so the WM can rebuild nested
//! geometry best-effort among windows that are still alive.

use std::collections::HashSet;

use crate::{
  SnapshotNode, SnapshotNodeKind, SnapshotWorkspace, SnapshotWindow,
  TilingDirection, WindowState,
};

/// A window leaf skipped while planning (missing live match or non-tiling).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedSnapshotLeaf {
  pub local_id: String,
  pub process_name: String,
  pub reason: SkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
  /// Snapshot leaf had no live window match.
  MissingLiveMatch,
  /// Non-tiling windows are restored via state/floating paths, not the tree.
  NonTiling,
}

/// Planned tiling node after prune + renormalize.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutPlanNode {
  Window {
    local_id: String,
    tiling_size: f32,
  },
  Split {
    local_id: String,
    tiling_direction: TilingDirection,
    tiling_size: f32,
    children: Vec<LayoutPlanNode>,
    /// Surviving child local_ids in snapshot focus order.
    child_focus_order: Vec<String>,
  },
}

impl LayoutPlanNode {
  #[must_use]
  pub fn local_id(&self) -> &str {
    match self {
      Self::Window { local_id, .. } | Self::Split { local_id, .. } => {
        local_id
      }
    }
  }

  #[must_use]
  pub fn tiling_size(&self) -> f32 {
    match self {
      Self::Window { tiling_size, .. } | Self::Split { tiling_size, .. } => {
        *tiling_size
      }
    }
  }

  fn set_tiling_size(&mut self, tiling_size: f32) {
    match self {
      Self::Window {
        tiling_size: size, ..
      }
      | Self::Split {
        tiling_size: size, ..
      } => *size = tiling_size,
    }
  }

  /// DFS window local_ids under this node.
  #[must_use]
  pub fn window_local_ids(&self) -> Vec<String> {
    let mut out = Vec::new();
    collect_window_local_ids(self, &mut out);
    out
  }
}

fn collect_window_local_ids(node: &LayoutPlanNode, out: &mut Vec<String>) {
  match node {
    LayoutPlanNode::Window { local_id, .. } => out.push(local_id.clone()),
    LayoutPlanNode::Split { children, .. } => {
      for child in children {
        collect_window_local_ids(child, out);
      }
    }
  }
}

/// Best-effort tiling layout for one workspace after matching.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceLayoutPlan {
  pub workspace_name: String,
  pub tiling_direction: TilingDirection,
  pub children: Vec<LayoutPlanNode>,
  pub child_focus_order: Vec<String>,
  pub skipped: Vec<SkippedSnapshotLeaf>,
}

impl WorkspaceLayoutPlan {
  #[must_use]
  pub fn window_local_ids(&self) -> Vec<String> {
    let mut out = Vec::new();
    for child in &self.children {
      collect_window_local_ids(child, &mut out);
    }
    out
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.children.is_empty()
  }
}

/// Build a tiling restore plan for `workspace`.
///
/// `matched_tiling_local_ids` must contain only snapshot window `local_id`s
/// that (1) matched a live window and (2) are `WindowState::Tiling` in the
/// snapshot. Missing ids are skipped; relative structure among survivors is
/// preserved and sibling sizes are renormalized to sum to 1.
#[must_use]
pub fn plan_workspace_tiling_layout(
  workspace: &SnapshotWorkspace,
  matched_tiling_local_ids: &HashSet<String>,
) -> WorkspaceLayoutPlan {
  let mut skipped = Vec::new();
  let root_children = workspace
    .root
    .children
    .as_deref()
    .unwrap_or(&[]);

  let mut children = Vec::new();
  for child in root_children {
    if let Some(node) =
      prune_tiling_node(child, matched_tiling_local_ids, &mut skipped)
    {
      children.push(node);
    }
  }

  if !children.is_empty() {
    renormalize_sibling_sizes(&mut children);
  }

  let surviving: HashSet<String> = children
    .iter()
    .flat_map(LayoutPlanNode::window_local_ids)
    .chain(children.iter().map(|c| c.local_id().to_string()))
    .collect();

  // Focus order may reference split local_ids at the workspace root.
  let child_focus_order = filter_focus_order(
    &workspace.child_focus_order,
    &children,
    &surviving,
  );

  WorkspaceLayoutPlan {
    workspace_name: workspace.name.clone(),
    tiling_direction: workspace.tiling_direction.clone(),
    children,
    child_focus_order,
    skipped,
  }
}

fn prune_tiling_node(
  node: &SnapshotNode,
  matched_tiling_local_ids: &HashSet<String>,
  skipped: &mut Vec<SkippedSnapshotLeaf>,
) -> Option<LayoutPlanNode> {
  match node.kind {
    SnapshotNodeKind::Window => {
      prune_window_node(node, matched_tiling_local_ids, skipped)
    }
    SnapshotNodeKind::Split => {
      prune_split_node(node, matched_tiling_local_ids, skipped)
    }
  }
}

fn prune_window_node(
  node: &SnapshotNode,
  matched_tiling_local_ids: &HashSet<String>,
  skipped: &mut Vec<SkippedSnapshotLeaf>,
) -> Option<LayoutPlanNode> {
  let window = node.window.as_ref()?;

  if !matches!(window.state, WindowState::Tiling) {
    skipped.push(SkippedSnapshotLeaf {
      local_id: node.local_id.clone(),
      process_name: window.identity.process_name.clone(),
      reason: SkipReason::NonTiling,
    });
    return None;
  }

  if !matched_tiling_local_ids.contains(&node.local_id) {
    skipped.push(SkippedSnapshotLeaf {
      local_id: node.local_id.clone(),
      process_name: window.identity.process_name.clone(),
      reason: SkipReason::MissingLiveMatch,
    });
    return None;
  }

  Some(LayoutPlanNode::Window {
    local_id: node.local_id.clone(),
    tiling_size: node.tiling_size.unwrap_or(1.0).max(0.0),
  })
}

fn prune_split_node(
  node: &SnapshotNode,
  matched_tiling_local_ids: &HashSet<String>,
  skipped: &mut Vec<SkippedSnapshotLeaf>,
) -> Option<LayoutPlanNode> {
  let raw_children = node.children.as_deref().unwrap_or(&[]);
  let mut children = Vec::new();
  for child in raw_children {
    if let Some(pruned) =
      prune_tiling_node(child, matched_tiling_local_ids, skipped)
    {
      children.push(pruned);
    }
  }

  let outer_size = node.tiling_size.unwrap_or(1.0).max(0.0);

  match children.len() {
    0 => None,
    1 => {
      // Collapse redundant single-child split; the survivor occupies this
      // node's slot in the parent (outer tiling size).
      let mut only = children.pop().unwrap();
      only.set_tiling_size(outer_size);
      Some(only)
    }
    _ => {
      renormalize_sibling_sizes(&mut children);
      let child_ids: HashSet<String> =
        children.iter().map(|c| c.local_id().to_string()).collect();
      let child_focus_order = node
        .child_focus_order
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|id| child_ids.contains(*id))
        .cloned()
        .collect();

      Some(LayoutPlanNode::Split {
        local_id: node.local_id.clone(),
        tiling_direction: node
          .tiling_direction
          .clone()
          .unwrap_or(TilingDirection::Horizontal),
        tiling_size: outer_size,
        children,
        child_focus_order,
      })
    }
  }
}

fn renormalize_sibling_sizes(nodes: &mut [LayoutPlanNode]) {
  if nodes.is_empty() {
    return;
  }

  let total: f32 = nodes.iter().map(LayoutPlanNode::tiling_size).sum();
  if total <= f32::EPSILON {
    #[allow(clippy::cast_precision_loss)]
    let equal = 1.0 / nodes.len() as f32;
    for node in nodes.iter_mut() {
      node.set_tiling_size(equal);
    }
    return;
  }

  for node in nodes.iter_mut() {
    node.set_tiling_size(node.tiling_size() / total);
  }
}

fn filter_focus_order(
  focus_order: &[String],
  children: &[LayoutPlanNode],
  surviving: &HashSet<String>,
) -> Vec<String> {
  let child_ids: HashSet<&str> =
    children.iter().map(LayoutPlanNode::local_id).collect();

  let mut ordered: Vec<String> = focus_order
    .iter()
    .filter(|id| child_ids.contains(id.as_str()) && surviving.contains(*id))
    .cloned()
    .collect();

  for child in children {
    let id = child.local_id();
    if !ordered.iter().any(|existing| existing == id) {
      ordered.push(id.to_string());
    }
  }
  ordered
}

/// Convenience: process names of tiling leaves under a snapshot node (DFS).
#[must_use]
pub fn snapshot_tiling_process_names(node: &SnapshotNode) -> Vec<String> {
  let mut out = Vec::new();
  walk_tiling_windows(node, &mut |w| {
    out.push(w.identity.process_name.clone());
  });
  out
}

fn walk_tiling_windows(
  node: &SnapshotNode,
  visit: &mut dyn FnMut(&SnapshotWindow),
) {
  match node.kind {
    SnapshotNodeKind::Window => {
      if let Some(window) = &node.window {
        if matches!(window.state, WindowState::Tiling) {
          visit(window);
        }
      }
    }
    SnapshotNodeKind::Split => {
      if let Some(children) = &node.children {
        for child in children {
          walk_tiling_windows(child, visit);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    SnapshotWindowIdentity, LAYOUT_SNAPSHOT_VERSION,
  };
  use wm_platform::Rect;

  fn tiling_window(local_id: &str, name: &str, size: f32) -> SnapshotNode {
    SnapshotNode {
      local_id: local_id.into(),
      kind: SnapshotNodeKind::Window,
      tiling_size: Some(size),
      tiling_direction: None,
      children: None,
      child_focus_order: None,
      window: Some(SnapshotWindow {
        identity: SnapshotWindowIdentity {
          process_path: Some(format!("C:\\Apps\\{name}.exe")),
          process_name: name.into(),
          class_name: Some(format!("{name}Class")),
          title_hint: Some(name.into()),
        },
        state: WindowState::Tiling,
        prev_state: None,
        floating_placement: Some(Rect::from_xy(0, 0, 100, 100)),
        id: None,
        handle: None,
      }),
      id: None,
    }
  }

  fn floating_window(local_id: &str, name: &str) -> SnapshotNode {
    let mut node = tiling_window(local_id, name, 1.0);
    if let Some(window) = node.window.as_mut() {
      window.state = WindowState::Floating(crate::FloatingStateConfig {
        centered: false,
        shown_on_top: false,
      });
    }
    node.tiling_size = None;
    node
  }

  fn split(
    local_id: &str,
    direction: TilingDirection,
    size: f32,
    children: Vec<SnapshotNode>,
    focus: Vec<&str>,
  ) -> SnapshotNode {
    SnapshotNode {
      local_id: local_id.into(),
      kind: SnapshotNodeKind::Split,
      tiling_size: Some(size),
      tiling_direction: Some(direction),
      children: Some(children),
      child_focus_order: Some(focus.into_iter().map(str::to_string).collect()),
      window: None,
      id: None,
    }
  }

  fn workspace_with_root(
    name: &str,
    direction: TilingDirection,
    children: Vec<SnapshotNode>,
    focus: Vec<&str>,
  ) -> SnapshotWorkspace {
    SnapshotWorkspace {
      name: name.into(),
      tiling_direction: direction.clone(),
      child_focus_order: focus.into_iter().map(str::to_string).collect(),
      root: SnapshotNode {
        local_id: "root".into(),
        kind: SnapshotNodeKind::Split,
        tiling_size: None,
        tiling_direction: Some(direction),
        children: Some(children),
        child_focus_order: None,
        window: None,
        id: None,
      },
      id: None,
    }
  }

  #[test]
  fn plan_preserves_nested_split_order_and_sizes() {
    // H[ a(0.3) V[ b(0.4) c(0.6) ](0.7) ]
    let ws = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("n1", "alpha", 0.3),
        split(
          "n2",
          TilingDirection::Vertical,
          0.7,
          vec![
            tiling_window("n3", "beta", 0.4),
            tiling_window("n4", "gamma", 0.6),
          ],
          vec!["n4", "n3"],
        ),
      ],
      vec!["n2", "n1"],
    );

    let matched: HashSet<String> =
      ["n1", "n3", "n4"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.children.len(), 2);
    assert!(matches!(
      &plan.children[0],
      LayoutPlanNode::Window { local_id, tiling_size }
        if local_id == "n1" && (*tiling_size - 0.3).abs() < 1e-5
    ));
    match &plan.children[1] {
      LayoutPlanNode::Split {
        local_id,
        tiling_direction,
        tiling_size,
        children,
        child_focus_order,
      } => {
        assert_eq!(local_id, "n2");
        assert_eq!(*tiling_direction, TilingDirection::Vertical);
        assert!((*tiling_size - 0.7).abs() < 1e-5);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].local_id(), "n3");
        assert_eq!(children[1].local_id(), "n4");
        assert!((children[0].tiling_size() - 0.4).abs() < 1e-5);
        assert!((children[1].tiling_size() - 0.6).abs() < 1e-5);
        assert_eq!(
          child_focus_order,
          &vec!["n4".to_string(), "n3".to_string()]
        );
      }
      other => panic!("expected split, got {other:?}"),
    }
    assert_eq!(
      plan.child_focus_order,
      vec!["n2".to_string(), "n1".to_string()]
    );
    assert_eq!(plan.tiling_direction, TilingDirection::Horizontal);
    assert_eq!(plan.workspace_name, "1");
    assert!(plan.skipped.is_empty());
  }

  #[test]
  fn plan_best_effort_when_middle_window_missing() {
    // H[ a(0.2) b(0.3) c(0.5) ] with b missing → H[ a(0.286) c(0.714) ]
    let ws = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("n1", "alpha", 0.2),
        tiling_window("n2", "beta", 0.3),
        tiling_window("n3", "gamma", 0.5),
      ],
      vec!["n1", "n2", "n3"],
    );

    let matched: HashSet<String> =
      ["n1", "n3"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.children.len(), 2);
    assert_eq!(plan.children[0].local_id(), "n1");
    assert_eq!(plan.children[1].local_id(), "n3");
    let s0 = plan.children[0].tiling_size();
    let s1 = plan.children[1].tiling_size();
    assert!((s0 + s1 - 1.0).abs() < 1e-5);
    assert!((s0 - 0.2 / 0.7).abs() < 1e-5);
    assert!((s1 - 0.5 / 0.7).abs() < 1e-5);
    assert_eq!(
      plan.skipped,
      vec![SkippedSnapshotLeaf {
        local_id: "n2".into(),
        process_name: "beta".into(),
        reason: SkipReason::MissingLiveMatch,
      }]
    );
  }

  #[test]
  fn plan_collapses_split_when_only_one_child_survives() {
    // H[ a(0.4) V[ b c ](0.6) ] with c missing and b only survivor of V
    // → H[ a(0.4) b(0.6) ] (vertical split collapses)
    let ws = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("n1", "alpha", 0.4),
        split(
          "n2",
          TilingDirection::Vertical,
          0.6,
          vec![
            tiling_window("n3", "beta", 0.5),
            tiling_window("n4", "gamma", 0.5),
          ],
          vec!["n3", "n4"],
        ),
      ],
      vec!["n1", "n2"],
    );

    let matched: HashSet<String> =
      ["n1", "n3"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.children.len(), 2);
    assert!(matches!(
      &plan.children[0],
      LayoutPlanNode::Window { local_id, .. } if local_id == "n1"
    ));
    assert!(matches!(
      &plan.children[1],
      LayoutPlanNode::Window { local_id, tiling_size }
        if local_id == "n3" && (*tiling_size - 0.6).abs() < 1e-5
    ));
    assert!(plan.skipped.iter().any(|s| s.local_id == "n4"));
  }

  #[test]
  fn plan_ignores_floating_siblings_in_tree() {
    let ws = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("n1", "alpha", 0.5),
        floating_window("n2", "floater"),
        tiling_window("n3", "beta", 0.5),
      ],
      vec!["n1", "n2", "n3"],
    );

    let matched: HashSet<String> =
      ["n1", "n3"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.window_local_ids(), vec!["n1".to_string(), "n3".to_string()]);
    assert!(plan.skipped.iter().any(|s| {
      s.local_id == "n2" && s.reason == SkipReason::NonTiling
    }));
    let _ = LAYOUT_SNAPSHOT_VERSION; // keep import used if clippy pedantic
  }

  #[test]
  fn plan_renormalizes_nested_sizes_after_sibling_loss() {
    // V[ H[a(0.5) b(0.5)](0.3)  c(0.7) ] with a missing
    // → V[ b(0.3) c(0.7) ]  (inner H collapses to b occupying 0.3 slot)
    let ws = workspace_with_root(
      "2",
      TilingDirection::Vertical,
      vec![
        split(
          "n0",
          TilingDirection::Horizontal,
          0.3,
          vec![
            tiling_window("n1", "alpha", 0.5),
            tiling_window("n2", "beta", 0.5),
          ],
          vec!["n1", "n2"],
        ),
        tiling_window("n3", "gamma", 0.7),
      ],
      vec!["n0", "n3"],
    );

    let matched: HashSet<String> =
      ["n2", "n3"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.children.len(), 2);
    assert_eq!(plan.children[0].local_id(), "n2");
    assert!((plan.children[0].tiling_size() - 0.3).abs() < 1e-5);
    assert_eq!(plan.children[1].local_id(), "n3");
    assert!((plan.children[1].tiling_size() - 0.7).abs() < 1e-5);
  }

  /// Collect split directions in DFS attach order (workspace children first,
  /// then nested). Mirrors what `attach_plan_nodes` walks when creating
  /// `SplitContainer`s during load.
  fn collect_split_directions(nodes: &[LayoutPlanNode]) -> Vec<TilingDirection> {
    let mut out = Vec::new();
    for node in nodes {
      if let LayoutPlanNode::Split {
        tiling_direction,
        children,
        ..
      } = node
      {
        out.push(tiling_direction.clone());
        out.extend(collect_split_directions(children));
      }
    }
    out
  }

  #[test]
  fn plan_preserves_nested_vertical_then_horizontal_directions() {
    // V[ a(0.4) H[ b(0.5) c(0.5) ](0.6) ]
    let ws = workspace_with_root(
      "code",
      TilingDirection::Vertical,
      vec![
        tiling_window("n1", "top", 0.4),
        split(
          "n2",
          TilingDirection::Horizontal,
          0.6,
          vec![
            tiling_window("n3", "left", 0.5),
            tiling_window("n4", "right", 0.5),
          ],
          vec!["n3", "n4"],
        ),
      ],
      vec!["n1", "n2"],
    );

    let matched: HashSet<String> =
      ["n1", "n3", "n4"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);

    assert_eq!(plan.workspace_name, "code");
    assert_eq!(plan.tiling_direction, TilingDirection::Vertical);
    assert_eq!(plan.children.len(), 2);
    assert_eq!(
      collect_split_directions(&plan.children),
      vec![TilingDirection::Horizontal],
      "load attach must see nested Horizontal under Vertical workspace"
    );
    match &plan.children[1] {
      LayoutPlanNode::Split {
        tiling_direction,
        children,
        ..
      } => {
        assert_eq!(*tiling_direction, TilingDirection::Horizontal);
        assert_eq!(children[0].local_id(), "n3");
        assert_eq!(children[1].local_id(), "n4");
      }
      other => panic!("expected horizontal split, got {other:?}"),
    }
  }

  #[test]
  fn plan_preserves_nested_horizontal_then_vertical_directions_for_attach() {
    // Explicit H-then-V direction list that load's SplitContainer::new walks.
    let ws = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("n1", "alpha", 0.3),
        split(
          "n2",
          TilingDirection::Vertical,
          0.7,
          vec![
            tiling_window("n3", "beta", 0.4),
            tiling_window("n4", "gamma", 0.6),
          ],
          vec!["n3", "n4"],
        ),
      ],
      vec!["n1", "n2"],
    );
    let matched: HashSet<String> =
      ["n1", "n3", "n4"].into_iter().map(str::to_string).collect();
    let plan = plan_workspace_tiling_layout(&ws, &matched);
    assert_eq!(plan.tiling_direction, TilingDirection::Horizontal);
    assert_eq!(
      collect_split_directions(&plan.children),
      vec![TilingDirection::Vertical]
    );
  }

  #[test]
  fn plans_keep_windows_on_their_snapshot_workspaces() {
    // Two workspaces; matched tiling ids per workspace must not cross-contaminate.
    let ws1 = workspace_with_root(
      "1",
      TilingDirection::Horizontal,
      vec![
        tiling_window("a1", "editor", 0.5),
        tiling_window("a2", "term", 0.5),
      ],
      vec!["a1", "a2"],
    );
    let ws2 = workspace_with_root(
      "2",
      TilingDirection::Vertical,
      vec![
        tiling_window("b1", "browser", 0.6),
        floating_window("b2", "chat"),
      ],
      vec!["b1", "b2"],
    );

    // Best-effort across workspaces: ws1 keeps both tiling leaves; ws2 keeps browser,
    // drops floating chat from the tiling plan.
    let plan1 = plan_workspace_tiling_layout(
      &ws1,
      &["a1", "a2"].into_iter().map(str::to_string).collect(),
    );
    let plan2 = plan_workspace_tiling_layout(
      &ws2,
      &["b1"].into_iter().map(str::to_string).collect(),
    );

    assert_eq!(plan1.workspace_name, "1");
    assert_eq!(plan1.window_local_ids(), vec!["a1".to_string(), "a2".to_string()]);
    assert_eq!(plan1.tiling_direction, TilingDirection::Horizontal);

    assert_eq!(plan2.workspace_name, "2");
    assert_eq!(plan2.window_local_ids(), vec!["b1".to_string()]);
    assert_eq!(plan2.tiling_direction, TilingDirection::Vertical);
    assert!(
      plan2.skipped.iter().any(|s| {
        s.local_id == "b2" && s.reason == SkipReason::NonTiling
      }),
      "floating leaf stays out of tiling plan but ws membership is via leaf path"
    );
    // No cross-workspace leakage of local ids.
    assert!(!plan1.window_local_ids().iter().any(|id| id.starts_with('b')));
    assert!(!plan2.window_local_ids().iter().any(|id| id.starts_with('a')));
  }
}
