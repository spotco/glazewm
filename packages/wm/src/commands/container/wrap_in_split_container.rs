use std::collections::VecDeque;

use anyhow::{bail, Context};

use super::{attach_container, detach_container};
use crate::{
  models::{Container, SplitContainer, TilingContainer},
  traits::{CommonGetters, TilingSizeGetters},
};

pub fn wrap_in_split_container(
  split_container: &SplitContainer,
  target_parent: &Container,
  target_children: &[TilingContainer],
) -> anyhow::Result<()> {
  // Callers normally pass direct children, but an orthogonal move can pair
  // a workspace child with a window nested below a different split. Detach
  // those mixed-parent children through the normal tree helper before
  // rewriting parent pointers below. Otherwise the old parent keeps the
  // child while the new split also claims it.
  for target_child in target_children {
    let target_child_container: Container = target_child.clone().into();
    if target_child_container.parent() != Some(target_parent.clone()) {
      detach_container(target_child_container.clone())?;
      attach_container(&target_child_container, target_parent, None)?;
    }
    if target_child_container.parent() != Some(target_parent.clone()) {
      bail!("Target child is not attached to target parent.");
    }
  }

  let starting_index = target_children
    .iter()
    .map(CommonGetters::index)
    .min()
    .context("Failed to get starting index.")?;

  target_parent
    .borrow_children_mut()
    .insert(starting_index, split_container.clone().into());

  let starting_focus_index = target_children
    .iter()
    .map(CommonGetters::focus_index)
    .min()
    .context("Failed to get starting focus index.")?;

  target_parent
    .borrow_child_focus_order_mut()
    .insert(starting_focus_index, split_container.id());

  // Get the total tiling size amongst all children.
  let total_tiling_size = target_children
    .iter()
    .map(TilingSizeGetters::tiling_size)
    .sum::<f32>();

  let target_children_ids = target_children
    .iter()
    .map(CommonGetters::id)
    .collect::<Vec<_>>();

  let sorted_focus_ids = target_parent
    .borrow_child_focus_order()
    .iter()
    .filter(|id| target_children_ids.contains(id))
    .copied()
    .collect::<VecDeque<_>>();

  // Set the split container's parent and tiling size.
  *split_container.borrow_parent_mut() = Some(target_parent.clone());
  split_container.set_tiling_size(total_tiling_size);

  // Move the children from their original parent to the split container.
  for target_child in target_children {
    *target_child.borrow_parent_mut() =
      Some(split_container.clone().into());

    split_container
      .borrow_children_mut()
      .push_back(target_child.clone().into());

    target_parent
      .borrow_children_mut()
      .retain(|child| child != &target_child.clone().into());

    target_parent
      .borrow_child_focus_order_mut()
      .retain(|id| id != &target_child.id());

    // Scale the tiling size to the new split container.
    target_child
      .set_tiling_size(target_child.tiling_size() / total_tiling_size);
  }

  // Add original focus order to split container.
  *split_container.borrow_child_focus_order_mut() = sorted_focus_ids;

  Ok(())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use std::collections::HashMap;

  use uuid::Uuid;
  use wm_common::{GapsConfig, TilingDirection, WorkspaceConfig};
  use wm_platform::{
    NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
  };

  use super::*;
  use crate::{
    commands::container::attach_container,
    models::{NativeWindowProperties, TilingWindow, Workspace},
    traits::CommonGetters,
  };

  fn test_window(id: u128) -> TilingWindow {
    TilingWindow::new(
      Some(Uuid::from_u128(id)),
      NativeWindow::from_handle(0),
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

  fn assert_tree_integrity(root: &Container) {
    fn visit(node: &Container, counts: &mut HashMap<Uuid, usize>) {
      *counts.entry(node.id()).or_default() += 1;
      if let Some(parent) = node.parent() {
        assert!(
          parent
            .children()
            .iter()
            .any(|child| child.id() == node.id()),
          "parent does not contain child {}",
          node.id()
        );
      }
      for child in node.children() {
        assert_eq!(child.parent(), Some(node.clone()));
        visit(&child, counts);
      }
    }

    let mut counts = HashMap::new();
    visit(root, &mut counts);
    assert!(counts.values().all(|count| *count == 1));
  }

  #[test]
  fn mixed_parent_wrap_detaches_nested_window_once() {
    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "test".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    let old_split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    let window_one = test_window(1);
    let window_two = test_window(2);
    let neighbor = test_window(3);

    attach_container(
      &old_split.clone().into(),
      &workspace_container,
      Some(0),
    )
    .expect("attach old split");
    attach_container(&neighbor.clone().into(), &workspace_container, None)
      .expect("attach neighbor");
    attach_container(
      &window_one.clone().into(),
      &old_split.clone().into(),
      None,
    )
    .expect("attach nested window");
    attach_container(
      &window_two.clone().into(),
      &old_split.clone().into(),
      None,
    )
    .expect("attach sibling window");

    let new_split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    wrap_in_split_container(
      &new_split,
      &workspace_container,
      &[neighbor.clone().into(), window_one.clone().into()],
    )
    .expect("wrap mixed-parent children");

    assert_eq!(
      window_one.parent().map(|parent| parent.id()),
      Some(new_split.id())
    );
    assert_eq!(window_two.parent(), Some(old_split.clone().into()));
    assert_tree_integrity(&workspace_container);
  }
}
