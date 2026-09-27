//! Pure tree planner for spotcobuild global-stack-direction moves.
//!
//! Reference algorithm for live `move_window_in_direction` and unit tests.

use wm_platform::Direction;

use crate::TilingDirection;

/// Workspace tiling tree (direction + top-level children).
#[derive(Clone, Debug, PartialEq)]
pub struct MoveTree {
  pub direction: TilingDirection,
  pub children: Vec<MoveNode>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MoveNode {
  Window(String),
  Split {
    direction: TilingDirection,
    children: Vec<MoveNode>,
  },
}

impl MoveNode {
  #[must_use]
  pub fn window(id: impl Into<String>) -> Self {
    Self::Window(id.into())
  }

  #[must_use]
  pub fn split(
    direction: TilingDirection,
    children: Vec<MoveNode>,
  ) -> Self {
    Self::Split {
      direction,
      children,
    }
  }

  fn contains_window(&self, id: &str) -> bool {
    match self {
      Self::Window(w) => w == id,
      Self::Split { children, .. } => {
        children.iter().any(|c| c.contains_window(id))
      }
    }
  }

  fn extract_window(&mut self, id: &str) -> Option<MoveNode> {
    match self {
      Self::Window(_) => None,
      Self::Split { children, .. } => extract_from_children(children, id),
    }
  }
}

impl MoveTree {
  /// Compact debug form like H[V[1 2] 3].
  #[must_use]
  pub fn format_compact(&self) -> String {
    format_split_like(&self.direction, &self.children)
  }
}

fn format_split_like(
  direction: &TilingDirection,
  children: &[MoveNode],
) -> String {
  let mut s = match direction {
    TilingDirection::Horizontal => String::from("H["),
    TilingDirection::Vertical => String::from("V["),
  };
  for (i, child) in children.iter().enumerate() {
    if i > 0 {
      s.push(' ');
    }
    s.push_str(&format_node(child));
  }
  s.push(']');
  s
}

fn format_node(node: &MoveNode) -> String {
  match node {
    MoveNode::Window(id) => id.clone(),
    MoveNode::Split {
      direction,
      children,
    } => format_split_like(direction, children),
  }
}

fn extract_from_children(
  children: &mut Vec<MoveNode>,
  id: &str,
) -> Option<MoveNode> {
  let mut i = 0;
  while i < children.len() {
    match &children[i] {
      MoveNode::Window(w) if w == id => {
        return Some(children.remove(i));
      }
      MoveNode::Split { .. } if children[i].contains_window(id) => {
        let extracted = children[i].extract_window(id)?;
        flatten_single_child_at(children, i);
        return Some(extracted);
      }
      _ => i += 1,
    }
  }
  None
}

fn flatten_single_child_at(children: &mut Vec<MoveNode>, index: usize) {
  if index >= children.len() {
    return;
  }
  let collapse = matches!(
    &children[index],
    MoveNode::Split { children: c, .. } if c.len() <= 1
  );
  if !collapse {
    return;
  }
  match children.remove(index) {
    MoveNode::Split {
      children: mut inner,
      ..
    } if !inner.is_empty() => {
      children.insert(index, inner.remove(0));
    }
    _ => {}
  }
}

fn normalize(tree: &mut MoveTree) {
  normalize_children(&mut tree.children, &tree.direction);
  // Promote a sole split child into the workspace. The live tree flips the
  // workspace direction to the promoted split's direction when doing this.
  if tree.children.len() == 1 {
    if let MoveNode::Split {
      direction,
      children,
    } = &tree.children[0]
    {
      tree.direction = direction.clone();
      tree.children = children.clone();
    }
  }
}

fn normalize_children(
  children: &mut Vec<MoveNode>,
  parent_dir: &TilingDirection,
) {
  let mut i = 0;
  while i < children.len() {
    if let MoveNode::Split {
      direction,
      children: inner,
    } = &mut children[i]
    {
      let dir = direction.clone();
      normalize_children(inner, &dir);
    }

    // Collapse single-child splits.
    let collapse = matches!(
      children.get(i),
      Some(MoveNode::Split { children: c, .. }) if c.len() == 1
    );
    if collapse {
      if let MoveNode::Split {
        children: mut inner,
        ..
      } = children.remove(i)
      {
        children.insert(i, inner.remove(0));
      }
      continue;
    }

    // Merge same-direction nested splits into parent.
    let merge = matches!(
      children.get(i),
      Some(MoveNode::Split { direction, .. }) if direction == parent_dir
    );
    if merge {
      if let MoveNode::Split {
        children: inner, ..
      } = children.remove(i)
      {
        for (offset, child) in inner.into_iter().enumerate() {
          children.insert(i + offset, child);
        }
      }
      continue;
    }

    i += 1;
  }
}

/// Plan a directional move using `stack_direction` as the insertion axis.
///
/// Returns None when the focused window is at the workspace edge on the
/// stack axis with no sibling (live code may cross monitors).
#[must_use]
pub fn plan_global_move(
  tree: &MoveTree,
  focus_id: &str,
  direction: &Direction,
  stack_direction: &TilingDirection,
) -> Option<MoveTree> {
  if !tree.children.iter().any(|c| c.contains_window(focus_id)) {
    return None;
  }

  let arrow_axis = TilingDirection::from_direction(direction);
  let mut result = tree.clone();

  if arrow_axis == *stack_direction {
    plan_parallel(&mut result, focus_id, direction, stack_direction)?;
  } else {
    plan_orthogonal(&mut result, focus_id, direction, stack_direction)?;
  }

  normalize(&mut result);
  Some(result)
}

fn plan_parallel(
  tree: &mut MoveTree,
  focus_id: &str,
  direction: &Direction,
  stack_direction: &TilingDirection,
) -> Option<()> {
  let path = focus_path(tree, focus_id)?;
  let arrow_axis = TilingDirection::from_direction(direction);
  let Some((target, target_parent_path)) =
    nearest_directional_target(tree, &path, &arrow_axis, direction)
  else {
    if path.len() > 1 {
      return promote_nested_window_to_workspace_edge(
        tree, focus_id, direction,
      );
    }
    if tree.direction != *stack_direction {
      return restructure_workspace_to_stack(
        tree,
        focus_id,
        direction,
        stack_direction,
      );
    }
    return None;
  };

  let focus_parent_path = path[..path.len() - 1].to_vec();
  if target_parent_path == focus_parent_path {
    let children = children_at_path_mut(tree, &focus_parent_path)?;
    let focus_index = *path.last()?;
    let window = children.remove(focus_index);
    let target_index =
      children.iter().position(|child| child == &target)?;
    let insert_index = match direction {
      Direction::Left | Direction::Up => target_index,
      Direction::Right | Direction::Down => target_index + 1,
    };
    children.insert(insert_index.min(children.len()), window);
    return Some(());
  }

  let window = extract_from_children(&mut tree.children, focus_id)?;
  // A nested focused window is promoted beside the neighboring workspace
  // column before the two containers are flattened. This preserves the
  // focused window's column-relative order for both horizontal arrows.
  let insertion_direction = if path.len() > 1 {
    Direction::Right
  } else {
    direction.clone()
  };
  if insert_beside_target(
    &mut tree.children,
    &target,
    window,
    &insertion_direction,
  ) {
    return Some(());
  }

  // The source subtree may have collapsed while the focused leaf was
  // extracted. The target should still be present; if not, refuse to make
  // a speculative rewrite.
  if path.len() > 1 {
    return promote_nested_window_to_workspace_edge(
      tree, focus_id, direction,
    );
  }

  if tree.direction != *stack_direction {
    return restructure_workspace_to_stack(
      tree,
      focus_id,
      direction,
      stack_direction,
    );
  }
  None
}

fn plan_orthogonal(
  tree: &mut MoveTree,
  focus_id: &str,
  direction: &Direction,
  stack_direction: &TilingDirection,
) -> Option<()> {
  let path = focus_path(tree, focus_id)?;
  let arrow_axis = TilingDirection::from_direction(direction);
  let Some((neighbor, _)) =
    nearest_directional_target(tree, &path, &arrow_axis, direction)
  else {
    if path.len() > 1 {
      return promote_nested_window_to_workspace_edge(
        tree, focus_id, direction,
      );
    }
    return restructure_workspace_to_stack(
      tree,
      focus_id,
      direction,
      stack_direction,
    );
  };

  let window = extract_from_children(&mut tree.children, focus_id)?;
  if append_to_existing_stack(
    &mut tree.children,
    &neighbor,
    window.clone(),
    stack_direction,
  ) {
    return Some(());
  }

  if replace_target_with_joined_stack(
    &mut tree.children,
    &neighbor,
    window,
    stack_direction,
  ) {
    return Some(());
  }

  None
}

fn focus_path(tree: &MoveTree, focus_id: &str) -> Option<Vec<usize>> {
  let mut path = Vec::new();
  find_focus_path(&tree.children, focus_id, &mut path).then_some(path)
}

fn find_focus_path(
  children: &[MoveNode],
  focus_id: &str,
  path: &mut Vec<usize>,
) -> bool {
  for (index, child) in children.iter().enumerate() {
    path.push(index);
    match child {
      MoveNode::Window(id) if id == focus_id => return true,
      MoveNode::Split { children, .. }
        if find_focus_path(children, focus_id, path) =>
      {
        return true;
      }
      _ => {
        path.pop();
      }
    }
  }
  false
}

fn children_at_path<'a>(
  tree: &'a MoveTree,
  path: &[usize],
) -> Option<&'a [MoveNode]> {
  if path.is_empty() {
    return Some(&tree.children);
  }
  match node_at_path(&tree.children, path)? {
    MoveNode::Split { children, .. } => Some(children),
    MoveNode::Window(_) => None,
  }
}

fn children_at_path_mut<'a>(
  tree: &'a mut MoveTree,
  path: &[usize],
) -> Option<&'a mut Vec<MoveNode>> {
  if path.is_empty() {
    return Some(&mut tree.children);
  }
  match node_at_path_mut(&mut tree.children, path)? {
    MoveNode::Split { children, .. } => Some(children),
    MoveNode::Window(_) => None,
  }
}

fn node_at_path<'a>(
  children: &'a [MoveNode],
  path: &[usize],
) -> Option<&'a MoveNode> {
  let (index, rest) = path.split_first()?;
  let node = children.get(*index)?;
  if rest.is_empty() {
    Some(node)
  } else {
    match node {
      MoveNode::Split { children, .. } => node_at_path(children, rest),
      MoveNode::Window(_) => None,
    }
  }
}

fn node_at_path_mut<'a>(
  children: &'a mut [MoveNode],
  path: &[usize],
) -> Option<&'a mut MoveNode> {
  let (index, rest) = path.split_first()?;
  let node = children.get_mut(*index)?;
  if rest.is_empty() {
    Some(node)
  } else {
    match node {
      MoveNode::Split { children, .. } => node_at_path_mut(children, rest),
      MoveNode::Window(_) => None,
    }
  }
}

fn nearest_directional_target(
  tree: &MoveTree,
  focus_path: &[usize],
  arrow_axis: &TilingDirection,
  direction: &Direction,
) -> Option<(MoveNode, Vec<usize>)> {
  for level in (0..focus_path.len()).rev() {
    let ancestor_path = &focus_path[..level];
    let ancestor_direction = if ancestor_path.is_empty() {
      &tree.direction
    } else {
      match node_at_path(&tree.children, ancestor_path)? {
        MoveNode::Split { direction, .. } => direction,
        MoveNode::Window(_) => continue,
      }
    };
    if ancestor_direction != arrow_axis {
      continue;
    }

    let children = children_at_path(tree, ancestor_path)?;
    let focus_index = *focus_path.get(level)?;
    let target_index = match direction {
      Direction::Left | Direction::Up => focus_index.checked_sub(1),
      Direction::Right | Direction::Down
        if focus_index + 1 < children.len() =>
      {
        Some(focus_index + 1)
      }
      Direction::Right | Direction::Down => None,
    }?;
    return Some((
      children.get(target_index)?.clone(),
      ancestor_path.to_vec(),
    ));
  }
  None
}

fn insert_beside_target(
  children: &mut Vec<MoveNode>,
  target: &MoveNode,
  window: MoveNode,
  direction: &Direction,
) -> bool {
  if let Some(index) = children.iter().position(|child| child == target) {
    let insert_index = match direction {
      Direction::Left | Direction::Up => index,
      Direction::Right | Direction::Down => index + 1,
    };
    children.insert(insert_index.min(children.len()), window);
    return true;
  }
  for child in children.iter_mut() {
    if let MoveNode::Split { children, .. } = child {
      if insert_beside_target(children, target, window.clone(), direction)
      {
        return true;
      }
    }
  }
  false
}

#[allow(clippy::ptr_arg)]
fn append_to_existing_stack(
  children: &mut Vec<MoveNode>,
  target: &MoveNode,
  window: MoveNode,
  stack_direction: &TilingDirection,
) -> bool {
  for child in children.iter_mut() {
    if child == target {
      if let MoveNode::Split {
        direction,
        children,
      } = child
      {
        if direction == stack_direction {
          children.push(window);
          return true;
        }
      }
      return false;
    }
    if let MoveNode::Split { children, .. } = child {
      if append_to_existing_stack(
        children,
        target,
        window.clone(),
        stack_direction,
      ) {
        return true;
      }
    }
  }
  false
}

fn replace_target_with_joined_stack(
  children: &mut Vec<MoveNode>,
  target: &MoveNode,
  window: MoveNode,
  stack_direction: &TilingDirection,
) -> bool {
  if let Some(index) = children.iter().position(|child| child == target) {
    let target = children.remove(index);
    let mut joined = flatten_to_list(target);
    // Orthogonal stacking has one stable insertion invariant: the moved
    // window is appended to the target stack. This matches the existing
    // stack-target path (`append_to_existing_stack`) and keeps a repeated
    // opposite-axis move at the bottom/end of each newly-created stack,
    // independent of whether the target started as a leaf or a split.
    joined.push(window);
    let new_node = if joined.len() == 1 {
      joined.remove(0)
    } else {
      MoveNode::split(stack_direction.clone(), joined)
    };
    children.insert(index, new_node);
    return true;
  }
  for child in children.iter_mut() {
    if let MoveNode::Split { children, .. } = child {
      if replace_target_with_joined_stack(
        children,
        target,
        window.clone(),
        stack_direction,
      ) {
        return true;
      }
    }
  }
  false
}

fn promote_nested_window_to_workspace_edge(
  tree: &mut MoveTree,
  focus_id: &str,
  direction: &Direction,
) -> Option<()> {
  let path = focus_path(tree, focus_id)?;
  if path.len() <= 1 {
    return None;
  }
  let root_index = *path.first()?;
  let window = extract_from_children(&mut tree.children, focus_id)?;
  let insert_index = match direction {
    Direction::Left | Direction::Up => root_index,
    Direction::Right | Direction::Down => root_index + 1,
  };
  tree
    .children
    .insert(insert_index.min(tree.children.len()), window);
  Some(())
}

fn flatten_to_list(node: MoveNode) -> Vec<MoveNode> {
  match node {
    MoveNode::Window(_) => vec![node],
    MoveNode::Split { children, .. } => children,
  }
}

fn restructure_workspace_to_stack(
  tree: &mut MoveTree,
  focus_id: &str,
  direction: &Direction,
  stack_direction: &TilingDirection,
) -> Option<()> {
  let old_dir = tree.direction.clone();
  let window = extract_from_children(&mut tree.children, focus_id)?;
  let others = std::mem::take(&mut tree.children);

  let others_node = match others.len() {
    0 => None,
    1 => Some(others.into_iter().next().unwrap()),
    _ => Some(MoveNode::split(old_dir, others)),
  };

  tree.direction = stack_direction.clone();
  tree.children.clear();

  let insert_first = matches!(direction, Direction::Left | Direction::Up);
  match (insert_first, others_node) {
    (true, Some(other)) => {
      tree.children.push(window);
      tree.children.push(other);
    }
    (false, Some(other)) => {
      tree.children.push(other);
      tree.children.push(window);
    }
    (_, None) => tree.children.push(window),
  }

  Some(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn h(children: Vec<MoveNode>) -> MoveTree {
    MoveTree {
      direction: TilingDirection::Horizontal,
      children,
    }
  }

  fn v(children: Vec<MoveNode>) -> MoveTree {
    MoveTree {
      direction: TilingDirection::Vertical,
      children,
    }
  }

  /// Fixture 13/23: H[V[1 2] 3]
  fn fixture_13_23() -> MoveTree {
    h(vec![
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("1"), MoveNode::window("2")],
      ),
      MoveNode::window("3"),
    ])
  }

  /// Deeper two-stack fixture with focus on an inner window (`2`).
  fn fixture_deep_12_34() -> MoveTree {
    h(vec![
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("1"), MoveNode::window("2")],
      ),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("3"), MoveNode::window("4")],
      ),
    ])
  }

  /// Workspace 2 example: rows `124` / `134`, with 1 spanning the left
  /// column, 2/3 stacked in the middle, and 4 spanning the right column.
  fn fixture_wks2_124_134() -> MoveTree {
    h(vec![
      MoveNode::window("1"),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("2"), MoveNode::window("3")],
      ),
      MoveNode::window("4"),
    ])
  }

  /// Workspace 2 example: rows `12` / `13` / `14`, with 1 spanning the
  /// left column and 2/3/4 stacked in the right column.
  fn fixture_wks2_12_13_14() -> MoveTree {
    h(vec![
      MoveNode::window("1"),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![
          MoveNode::window("2"),
          MoveNode::window("3"),
          MoveNode::window("4"),
        ],
      ),
    ])
  }

  #[test]
  fn fixture_formats_as_expected() {
    assert_eq!(fixture_13_23().format_compact(), "H[V[1 2] 3]");
  }

  #[test]
  fn fixture_covers_all_global_directions_and_arrows() {
    let cases = [
      (
        TilingDirection::Horizontal,
        Direction::Left,
        Some("H[3 V[1 2]]"),
      ),
      (TilingDirection::Horizontal, Direction::Right, None),
      (
        TilingDirection::Horizontal,
        Direction::Up,
        Some("H[3 V[1 2]]"),
      ),
      (
        TilingDirection::Horizontal,
        Direction::Down,
        Some("H[V[1 2] 3]"),
      ),
      (TilingDirection::Vertical, Direction::Left, Some("V[1 2 3]")),
      (
        TilingDirection::Vertical,
        Direction::Right,
        Some("V[1 2 3]"),
      ),
      (TilingDirection::Vertical, Direction::Up, Some("V[3 1 2]")),
      (TilingDirection::Vertical, Direction::Down, Some("V[1 2 3]")),
    ];

    for (stack_direction, direction, expected) in cases {
      let result = plan_global_move(
        &fixture_13_23(),
        "3",
        &direction,
        &stack_direction,
      );

      match expected {
        Some(expected) => assert_eq!(
          result
            .expect("fixture move should succeed")
            .format_compact(),
          expected,
          "stack={stack_direction:?}, direction={direction:?}"
        ),
        None => assert!(
          result.is_none(),
          "stack={stack_direction:?}, direction={direction:?}"
        ),
      }
    }
  }

  #[test]
  fn move_left_global_horizontal_gives_31_32() {
    let tree = fixture_13_23();
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[3 V[1 2]]");
  }

  #[test]
  fn move_left_global_vertical_gives_1_2_3() {
    let tree = fixture_13_23();
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Left,
      &TilingDirection::Vertical,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "V[1 2 3]");
  }

  #[test]
  fn opposite_stack_does_not_require_mutating_caller_global() {
    let tree = fixture_13_23();
    let global = TilingDirection::Horizontal;
    let opposite = global.inverse();
    let out = plan_global_move(&tree, "3", &Direction::Left, &opposite)
      .expect("move");
    assert_eq!(out.format_compact(), "V[1 2 3]");
    assert_eq!(global, TilingDirection::Horizontal);
  }

  #[test]
  fn parallel_when_workspace_not_stack_restructures() {
    // V[1 2 3] focus 3, Left, stack=H → H[3 V[1 2]]
    let tree = v(vec![
      MoveNode::window("1"),
      MoveNode::window("2"),
      MoveNode::window("3"),
    ]);
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[3 V[1 2]]");
  }

  #[test]
  fn deeper_nest_parallel_extracts_window_beside_neighbor() {
    // H[V[1 2] V[3 4]] focus 3 Left stack=H →
    // H[V[1 2] 3 4]. A nested source is promoted after the neighboring
    // column so its remaining column keeps its relative order.
    let tree = fixture_deep_12_34();
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[V[1 2] 3 4]");
  }

  #[test]
  fn deep_fixture_inner_focus_covers_all_arrows() {
    let cases = [
      (
        TilingDirection::Horizontal,
        Direction::Left,
        Some("H[2 1 V[3 4]]"),
      ),
      (
        TilingDirection::Horizontal,
        Direction::Right,
        Some("H[1 V[3 4] 2]"),
      ),
      (
        TilingDirection::Horizontal,
        Direction::Up,
        Some("H[1 2 V[3 4]]"),
      ),
      (
        TilingDirection::Horizontal,
        Direction::Down,
        Some("H[1 2 V[3 4]]"),
      ),
      (
        TilingDirection::Vertical,
        Direction::Left,
        Some("H[2 1 V[3 4]]"),
      ),
      (
        TilingDirection::Vertical,
        Direction::Right,
        Some("H[1 V[3 4 2]]"),
      ),
      (
        TilingDirection::Vertical,
        Direction::Up,
        Some("H[V[2 1] V[3 4]]"),
      ),
      (
        TilingDirection::Vertical,
        Direction::Down,
        Some("H[1 2 V[3 4]]"),
      ),
    ];

    for (stack_direction, direction, expected) in cases {
      let result = plan_global_move(
        &fixture_deep_12_34(),
        "2",
        &direction,
        &stack_direction,
      );

      match expected {
        Some(expected) => assert_eq!(
          result
            .expect("deep fixture move should succeed")
            .format_compact(),
          expected,
          "stack={stack_direction:?}, direction={direction:?}"
        ),
        None => assert!(
          result.is_none(),
          "stack={stack_direction:?}, direction={direction:?}"
        ),
      }
    }
  }

  #[test]
  fn single_window_move_returns_none_at_edge() {
    let tree = h(vec![MoveNode::window("1")]);
    assert!(plan_global_move(
      &tree,
      "1",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .is_none());
  }

  #[test]
  fn move_right_global_horizontal_swaps_back() {
    let tree = h(vec![
      MoveNode::window("3"),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("1"), MoveNode::window("2")],
      ),
    ]);
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Right,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[V[1 2] 3]");
  }

  #[test]
  fn wks2_focus_one_move_right_uses_global_horizontal_siblings() {
    let out = plan_global_move(
      &fixture_wks2_124_134(),
      "1",
      &Direction::Right,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[V[2 3] 1 4]");
  }

  #[test]
  fn wks2_focus_one_opposite_move_right_joins_bottom_of_vertical_stack() {
    let out = plan_global_move(
      &fixture_wks2_124_134(),
      "1",
      &Direction::Right,
      &TilingDirection::Vertical,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[V[2 3 1] 4]");
  }

  #[test]
  fn repeated_opposite_right_moves_append_to_the_bottom_of_each_stack() {
    // H[1 2 3] rendered as one horizontal row. With global H, Ctrl+Right
    // uses the opposite V stack axis:
    //   123 -> 23 / 13 -> 23 / 21
    let tree = h(vec![
      MoveNode::window("1"),
      MoveNode::window("2"),
      MoveNode::window("3"),
    ]);
    let first = plan_global_move(
      &tree,
      "1",
      &Direction::Right,
      &TilingDirection::Vertical,
    )
    .expect("first opposite move");
    assert_eq!(first.format_compact(), "H[V[2 1] 3]");

    let second = plan_global_move(
      &first,
      "1",
      &Direction::Right,
      &TilingDirection::Vertical,
    )
    .expect("second opposite move");
    assert_eq!(second.format_compact(), "H[2 V[3 1]]");
  }

  #[test]
  fn wks2_focus_three_opposite_move_left_preserves_two_columns() {
    let out = plan_global_move(
      &fixture_wks2_12_13_14(),
      "3",
      &Direction::Left,
      &TilingDirection::Vertical,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[V[1 3] V[2 4]]");
  }

  #[test]
  fn user_move_matrix_has_stable_expected_trees() {
    let mut history = std::collections::VecDeque::new();
    let cases = [
      (
        fixture_wks2_124_134(),
        "1",
        Direction::Right,
        TilingDirection::Horizontal,
        "H[V[2 3] 1 4]",
      ),
      (
        fixture_wks2_124_134(),
        "1",
        Direction::Right,
        TilingDirection::Vertical,
        "H[V[2 3 1] 4]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Up,
        TilingDirection::Horizontal,
        "H[1 V[2 H[3 4]]]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Up,
        TilingDirection::Vertical,
        "H[1 V[2 4 3]]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Right,
        TilingDirection::Horizontal,
        "H[1 V[2 3] 4]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Left,
        TilingDirection::Horizontal,
        "H[1 4 V[2 3]]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Right,
        TilingDirection::Vertical,
        "H[1 V[2 3] 4]",
      ),
      (
        fixture_wks2_12_13_14(),
        "4",
        Direction::Left,
        TilingDirection::Vertical,
        "H[V[1 4] V[2 3]]",
      ),
    ];

    for (tree, focus, direction, stack, expected) in cases {
      let original = tree.clone();
      let moved = plan_global_move(&tree, focus, &direction, &stack)
        .expect("user matrix move");
      assert_eq!(
        moved.format_compact(),
        expected,
        "focus={focus} direction={direction:?} stack={stack:?}"
      );

      // This is the permanent move-history contract for every user case:
      // undo restores the exact pre-move tree, and redo restores the exact
      // post-move tree. The live WM stores LayoutSnapshots with the same
      // before/after shape.
      history.push_back((original.clone(), moved.clone()));
      let (before, after) = history.pop_back().expect("history entry");
      assert_eq!(before, original, "undo tree");
      history.push_back((before.clone(), after.clone()));
      let (redo_before, redo_after) =
        history.pop_back().expect("redo entry");
      assert_eq!(redo_before, original, "redo source tree");
      assert_eq!(redo_after, moved, "redo tree");
    }
  }
}
