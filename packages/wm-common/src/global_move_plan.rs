//! Pure tree planner for spotcobuild global-stack-direction moves.
//!
//! Reference algorithm for live move_window_in_direction and unit tests.

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
      children: mut inner, ..
    } if !inner.is_empty() => {
      children.insert(index, inner.remove(0));
    }
    _ => {}
  }
}

fn normalize(tree: &mut MoveTree) {
  normalize_children(&mut tree.children, &tree.direction);
  // Unwrap single same-direction split at root.
  if tree.children.len() == 1 {
    if let MoveNode::Split {
      direction,
      children,
    } = &tree.children[0]
    {
      if direction == &tree.direction {
        tree.children = children.clone();
      }
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
        children: mut inner, ..
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

/// Plan a directional move using stack_direction as the insertion axis.
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
  if tree.direction != *stack_direction {
    return restructure_workspace_to_stack(
      tree,
      focus_id,
      direction,
      stack_direction,
    );
  }

  let anchor_idx = tree
    .children
    .iter()
    .position(|c| c.contains_window(focus_id))?;

  let neighbor_idx = match direction {
    Direction::Left | Direction::Up if anchor_idx > 0 => {
      Some(anchor_idx - 1)
    }
    Direction::Right | Direction::Down
      if anchor_idx + 1 < tree.children.len() =>
    {
      Some(anchor_idx + 1)
    }
    _ => None,
  };

  let Some(neighbor_idx) = neighbor_idx else {
    return None;
  };

  let neighbor_node = tree.children[neighbor_idx].clone();
  let window = extract_from_children(&mut tree.children, focus_id)?;

  let new_neighbor_idx = tree
    .children
    .iter()
    .position(|c| c == &neighbor_node)
    .unwrap_or_else(|| neighbor_idx.min(tree.children.len()));

  let insert_at = match direction {
    Direction::Left | Direction::Up => new_neighbor_idx,
    Direction::Right | Direction::Down => new_neighbor_idx + 1,
  };

  tree.children.insert(insert_at.min(tree.children.len()), window);
  Some(())
}

fn plan_orthogonal(
  tree: &mut MoveTree,
  focus_id: &str,
  direction: &Direction,
  stack_direction: &TilingDirection,
) -> Option<()> {
  let anchor_idx = tree
    .children
    .iter()
    .position(|c| c.contains_window(focus_id))?;

  let neighbor_idx = match direction {
    Direction::Left | Direction::Up if anchor_idx > 0 => {
      Some(anchor_idx - 1)
    }
    Direction::Right | Direction::Down
      if anchor_idx + 1 < tree.children.len() =>
    {
      Some(anchor_idx + 1)
    }
    _ => None,
  };

  if let Some(neighbor_idx) = neighbor_idx {
    let neighbor = tree.children[neighbor_idx].clone();
    let window = extract_from_children(&mut tree.children, focus_id)?;

    let neighbor_idx = tree
      .children
      .iter()
      .position(|c| c == &neighbor)
      .unwrap_or_else(|| {
        neighbor_idx.min(tree.children.len().saturating_sub(1))
      });

    let neighbor = tree.children.remove(neighbor_idx);
    let mut joined = flatten_to_list(neighbor);
    match direction {
      Direction::Left | Direction::Up => joined.push(window),
      Direction::Right | Direction::Down => joined.insert(0, window),
    }

    let new_node = if joined.len() == 1 {
      joined.remove(0)
    } else {
      MoveNode::split(stack_direction.clone(), joined)
    };

    tree
      .children
      .insert(neighbor_idx.min(tree.children.len()), new_node);
    tree.direction = stack_direction.clone();
    return Some(());
  }

  restructure_workspace_to_stack(
    tree,
    focus_id,
    direction,
    stack_direction,
  )
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

  #[test]
  fn fixture_formats_as_expected() {
    assert_eq!(fixture_13_23().format_compact(), "H[V[1 2] 3]");
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
    // H[V[1 2] V[3 4]] focus 3 Left stack=H → H[3 V[1 2] 4]
    let tree = h(vec![
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("1"), MoveNode::window("2")],
      ),
      MoveNode::split(
        TilingDirection::Vertical,
        vec![MoveNode::window("3"), MoveNode::window("4")],
      ),
    ]);
    let out = plan_global_move(
      &tree,
      "3",
      &Direction::Left,
      &TilingDirection::Horizontal,
    )
    .expect("move");
    assert_eq!(out.format_compact(), "H[3 V[1 2] 4]");
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
}
