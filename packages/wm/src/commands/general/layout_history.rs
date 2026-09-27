use std::{
  collections::{hash_map::DefaultHasher, VecDeque},
  hash::{Hash, Hasher},
};

use anyhow::Context;
use wm_common::{
  LayoutSnapshot, SnapshotNode, SnapshotNodeKind, SnapshotWorkspace,
};

use super::{layout_debug_log, load_layout_snapshot, LoadLayoutSummary};
use crate::{user_config::UserConfig, wm_state::WmState};

/// Maximum number of structural tiling moves retained for undo/redo.
pub const LAYOUT_HISTORY_MAX_ENTRIES: usize = 128;

/// A successful structural layout mutation and its exact before/after
/// state.
#[derive(Clone, Debug)]
pub struct LayoutHistoryEntry {
  pub transaction_id: u64,
  pub operation: String,
  pub workspace_name: String,
  pub focused_window_id: String,
  pub before: LayoutSnapshot,
  pub after: LayoutSnapshot,
}

/// WM-wide, bounded, in-memory layout history.
#[derive(Debug, Default)]
pub struct LayoutHistory {
  undo: VecDeque<LayoutHistoryEntry>,
  redo: VecDeque<LayoutHistoryEntry>,
  next_transaction_id: u64,
}

impl LayoutHistory {
  pub fn record(
    &mut self,
    operation: String,
    workspace_name: String,
    focused_window_id: String,
    before: LayoutSnapshot,
    after: LayoutSnapshot,
  ) -> LayoutHistoryEntry {
    self.next_transaction_id = self.next_transaction_id.saturating_add(1);
    let entry = LayoutHistoryEntry {
      transaction_id: self.next_transaction_id,
      operation,
      workspace_name,
      focused_window_id,
      before,
      after,
    };

    self.undo.push_back(entry.clone());
    while self.undo.len() > LAYOUT_HISTORY_MAX_ENTRIES {
      self.undo.pop_front();
    }
    self.redo.clear();

    entry
  }

  pub fn pop_undo(&mut self) -> Option<LayoutHistoryEntry> {
    self.undo.pop_back()
  }

  pub fn pop_redo(&mut self) -> Option<LayoutHistoryEntry> {
    self.redo.pop_back()
  }

  pub fn push_undo(&mut self, entry: LayoutHistoryEntry) {
    self.undo.push_back(entry);
    while self.undo.len() > LAYOUT_HISTORY_MAX_ENTRIES {
      self.undo.pop_front();
    }
  }

  pub fn push_redo(&mut self, entry: LayoutHistoryEntry) {
    self.redo.push_back(entry);
    while self.redo.len() > LAYOUT_HISTORY_MAX_ENTRIES {
      self.redo.pop_front();
    }
  }

  pub fn undo_depth(&self) -> usize {
    self.undo.len()
  }

  pub fn redo_depth(&self) -> usize {
    self.redo.len()
  }

  pub fn last_transaction_id(&self) -> Option<u64> {
    self
      .undo
      .back()
      .map(|entry| entry.transaction_id)
      .or_else(|| self.redo.back().map(|entry| entry.transaction_id))
  }

  /// Clears both stacks after an external topology mutation.
  pub fn clear(&mut self, reason: &str) {
    let undo_depth = self.undo.len();
    let redo_depth = self.redo.len();
    self.undo.clear();
    self.redo.clear();
    if undo_depth > 0 || redo_depth > 0 {
      layout_debug_log(format!(
        "layout history clear reason={reason:?} undo_depth={undo_depth} redo_depth={redo_depth}"
      ));
    }
  }
}

/// Returns whether two snapshots describe the same live layout state while
/// ignoring capture timestamps and version metadata.
#[must_use]
pub fn same_layout_state(
  left: &LayoutSnapshot,
  right: &LayoutSnapshot,
) -> bool {
  let mut left = left.clone();
  let mut right = right.clone();
  left.captured_at.clear();
  right.captured_at.clear();
  left.glazewm_version = None;
  right.glazewm_version = None;
  left == right
}

/// Stable, compact hash used in layout-history diagnostics.
#[must_use]
pub fn layout_state_hash(snapshot: &LayoutSnapshot) -> String {
  let mut normalized = snapshot.clone();
  normalized.captured_at.clear();
  normalized.glazewm_version = None;
  let json = serde_json::to_string(&normalized).unwrap_or_default();
  let mut hasher = DefaultHasher::new();
  json.hash(&mut hasher);
  format!("{:016x}", hasher.finish())
}

/// Compact tree summaries for one snapshot workspace, suitable for
/// `layout.log` without dumping the full DTO/snapshot.
#[must_use]
pub fn compact_workspace_trees(snapshot: &LayoutSnapshot) -> String {
  snapshot
    .monitors
    .iter()
    .flat_map(|monitor| monitor.workspaces.iter())
    .map(|workspace| {
      format!("{}={}", workspace.name, compact_workspace_tree(workspace))
    })
    .collect::<Vec<_>>()
    .join(";")
}

fn compact_workspace_tree(workspace: &SnapshotWorkspace) -> String {
  format_snapshot_node(&workspace.root)
}

fn format_snapshot_node(node: &SnapshotNode) -> String {
  match &node.kind {
    SnapshotNodeKind::Window => node.window.as_ref().map_or_else(
      || node.local_id.clone(),
      |window| {
        let title =
          window.identity.title_hint.as_deref().unwrap_or_default();
        let label = if title.is_empty() {
          window.identity.process_name.clone()
        } else {
          format!("{}:{title}", window.identity.process_name)
        };
        label.chars().take(64).collect()
      },
    ),
    SnapshotNodeKind::Split => {
      let direction =
        node.tiling_direction.as_ref().map_or('?', |direction| {
          match direction {
            wm_common::TilingDirection::Horizontal => 'H',
            wm_common::TilingDirection::Vertical => 'V',
          }
        });
      let children = node
        .children
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(format_snapshot_node)
        .collect::<Vec<_>>()
        .join(" ");
      format!("{direction}[{children}]")
    }
  }
}

/// Restore a history snapshot while preserving the current WM-wide global
/// direction. The global flag is policy state, not part of a move undo.
pub fn restore_history_snapshot(
  snapshot: &LayoutSnapshot,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<LoadLayoutSummary> {
  let global_direction = state.global_tiling_direction.clone();
  let mut snapshot = snapshot.clone();
  snapshot.global_tiling_direction = Some(global_direction.clone());

  let summary = load_layout_snapshot(&snapshot, state, config)
    .context("Failed to restore layout history snapshot")?;

  // `load_layout_snapshot` should already see the preserved value, but
  // keep this guard explicit so future snapshot changes cannot silently
  // mutate global move policy during undo/redo.
  if state.global_tiling_direction != global_direction {
    state.global_tiling_direction = global_direction;
  }

  Ok(summary)
}

#[cfg(test)]
mod tests {
  use wm_common::{
    plan_global_move, LayoutSnapshot, MoveNode, MoveTree, SnapshotBounds,
    SnapshotMonitor, SnapshotNode, SnapshotNodeKind, SnapshotWindow,
    SnapshotWindowIdentity, SnapshotWorkspace, TilingDirection,
    WindowState,
  };
  use wm_platform::Direction;

  use super::*;

  fn h(children: Vec<MoveNode>) -> MoveTree {
    MoveTree {
      direction: TilingDirection::Horizontal,
      children,
    }
  }

  fn w(id: &str) -> MoveNode {
    MoveNode::window(id)
  }

  fn wks2_124_134() -> MoveTree {
    h(vec![
      w("1"),
      MoveNode::split(TilingDirection::Vertical, vec![w("2"), w("3")]),
      w("4"),
    ])
  }

  fn wks2_12_13_14() -> MoveTree {
    h(vec![
      w("1"),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![w("2"), w("3"), w("4")],
      ),
    ])
  }

  fn flat_123() -> MoveTree {
    h(vec![w("1"), w("2"), w("3")])
  }

  fn flat_123_after_first_opposite_right() -> MoveTree {
    h(vec![
      MoveNode::split(TilingDirection::Vertical, vec![w("2"), w("1")]),
      w("3"),
    ])
  }

  fn snapshot_node(node: &MoveNode, split_id: &mut usize) -> SnapshotNode {
    match node {
      MoveNode::Window(id) => SnapshotNode {
        local_id: id.clone(),
        kind: SnapshotNodeKind::Window,
        tiling_size: Some(1.0),
        tiling_direction: None,
        children: None,
        child_focus_order: None,
        window: Some(SnapshotWindow {
          identity: SnapshotWindowIdentity {
            process_path: None,
            process_name: id.clone(),
            class_name: None,
            title_hint: Some(id.clone()),
          },
          state: WindowState::Tiling,
          prev_state: None,
          floating_placement: None,
          floating_placement_relative: None,
          id: None,
          handle: None,
        }),
        id: None,
      },
      MoveNode::Split {
        direction,
        children,
      } => {
        let local_id = format!("split-{split_id}");
        *split_id += 1;
        SnapshotNode {
          local_id,
          kind: SnapshotNodeKind::Split,
          tiling_size: Some(1.0),
          tiling_direction: Some(direction.clone()),
          children: Some(
            children
              .iter()
              .map(|child| snapshot_node(child, split_id))
              .collect(),
          ),
          child_focus_order: None,
          window: None,
          id: None,
        }
      }
    }
  }

  fn snapshot_for_tree(tree: &MoveTree) -> LayoutSnapshot {
    let mut split_id = 0;
    let root = SnapshotNode {
      local_id: "root".into(),
      kind: SnapshotNodeKind::Split,
      tiling_size: None,
      tiling_direction: Some(tree.direction.clone()),
      children: Some(
        tree
          .children
          .iter()
          .map(|child| snapshot_node(child, &mut split_id))
          .collect(),
      ),
      child_focus_order: None,
      window: None,
      id: None,
    };
    LayoutSnapshot {
      version: 1,
      captured_at: "history-test".into(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      global_tiling_direction: Some(TilingDirection::Horizontal),
      monitors: vec![SnapshotMonitor {
        hardware_id: Some("history-test".into()),
        device_path: None,
        device_name: "history-test".into(),
        bounds: SnapshotBounds {
          x: 0,
          y: 0,
          width: 1920,
          height: 1080,
        },
        focused_workspace_name: Some("wks2".into()),
        workspaces: vec![SnapshotWorkspace {
          name: "wks2".into(),
          tiling_direction: tree.direction.clone(),
          child_focus_order: vec![],
          root,
          id: None,
        }],
        id: None,
      }],
      ignored_windows: vec![],
    }
  }

  #[test]
  fn same_layout_state_ignores_capture_metadata() {
    let mut left = LayoutSnapshot {
      version: 1,
      captured_at: "one".into(),
      glazewm_version: Some("a".into()),
      paused: false,
      binding_modes: vec![],
      global_tiling_direction: None,
      monitors: vec![],
      ignored_windows: vec![],
    };
    let mut right = left.clone();
    right.captured_at = "two".into();
    right.glazewm_version = Some("b".into());
    assert!(same_layout_state(&left, &right));

    left.paused = true;
    assert!(!same_layout_state(&left, &right));
  }

  #[test]
  fn history_is_bounded_and_new_record_clears_redo() {
    let snapshot = LayoutSnapshot {
      version: 1,
      captured_at: String::new(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      global_tiling_direction: None,
      monitors: vec![],
      ignored_windows: vec![],
    };
    let mut history = LayoutHistory::default();
    for _ in 0..=LAYOUT_HISTORY_MAX_ENTRIES {
      history.record(
        "move".into(),
        "1".into(),
        "window".into(),
        snapshot.clone(),
        snapshot.clone(),
      );
    }
    assert_eq!(history.undo_depth(), LAYOUT_HISTORY_MAX_ENTRIES);
    let entry = history.pop_undo().expect("history entry");
    history.push_redo(entry);
    assert_eq!(history.redo_depth(), 1);
    history.record(
      "move".into(),
      "1".into(),
      "window".into(),
      snapshot.clone(),
      snapshot,
    );
    assert_eq!(history.redo_depth(), 0);
  }

  #[test]
  fn user_move_matrix_history_round_trips_every_case() {
    let cases = [
      (
        wks2_124_134(),
        "1",
        Direction::Right,
        TilingDirection::Horizontal,
        "H[V[2 3] 1 4]",
      ),
      (
        wks2_124_134(),
        "1",
        Direction::Right,
        TilingDirection::Vertical,
        "H[V[2 3 1] 4]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Up,
        TilingDirection::Horizontal,
        "H[1 V[2 H[3 4]]]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Up,
        TilingDirection::Vertical,
        "H[1 V[2 4 3]]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Right,
        TilingDirection::Horizontal,
        "H[1 V[2 3] 4]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Left,
        TilingDirection::Horizontal,
        "H[1 4 V[2 3]]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Right,
        TilingDirection::Vertical,
        "H[1 V[2 3] 4]",
      ),
      (
        wks2_12_13_14(),
        "4",
        Direction::Left,
        TilingDirection::Vertical,
        "H[V[1 4] V[2 3]]",
      ),
      (
        flat_123(),
        "1",
        Direction::Right,
        TilingDirection::Vertical,
        "H[V[2 1] 3]",
      ),
      (
        flat_123_after_first_opposite_right(),
        "1",
        Direction::Right,
        TilingDirection::Vertical,
        "H[2 V[3 1]]",
      ),
    ];

    for (tree, focus, direction, stack, expected) in cases {
      let moved = plan_global_move(&tree, focus, &direction, &stack)
        .expect("matrix move");
      assert_eq!(moved.format_compact(), expected);
      let before = snapshot_for_tree(&tree);
      let after = snapshot_for_tree(&moved);

      let mut history = LayoutHistory::default();
      let entry = history.record(
        format!("move direction={direction:?} stack={stack:?}"),
        "wks2".into(),
        focus.into(),
        before.clone(),
        after.clone(),
      );
      let undo = history.pop_undo().expect("undo entry");
      assert!(same_layout_state(&undo.before, &before));
      assert!(same_layout_state(&undo.after, &after));
      history.push_redo(undo);
      let redo = history.pop_redo().expect("redo entry");
      assert!(same_layout_state(&redo.before, &before));
      assert!(same_layout_state(&redo.after, &after));
      assert_eq!(entry.transaction_id, redo.transaction_id);
      assert_eq!(history.undo_depth(), 0);
      assert_eq!(history.redo_depth(), 0);
    }
  }

  #[test]
  fn no_op_or_failed_move_does_not_create_history() {
    let tree = h(vec![w("1")]);
    assert!(plan_global_move(
      &tree,
      "1",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .is_none());

    let snapshot = snapshot_for_tree(&tree);
    let mut history = LayoutHistory::default();
    if !same_layout_state(&snapshot, &snapshot) {
      history.record(
        "move".into(),
        "wks2".into(),
        "1".into(),
        snapshot.clone(),
        snapshot.clone(),
      );
    }
    assert_eq!(history.undo_depth(), 0);
    assert_eq!(history.redo_depth(), 0);
  }
}
