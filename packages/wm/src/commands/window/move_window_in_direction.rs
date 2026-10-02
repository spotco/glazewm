use anyhow::Context;
use wm_common::{TilingDirection, WindowState};
use wm_platform::{Direction, Rect};

use crate::{
  commands::{
    container::{
      flatten_child_split_containers, flatten_split_container,
      move_container_within_tree, set_focused_descendant,
      wrap_in_split_container,
    },
    general::layout_debug_log,
  },
  models::{
    Container, Monitor, NonTilingWindow, SplitContainer, TilingContainer,
    TilingWindow, WindowContainer, Workspace,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// The distance in pixels to snap the window to the monitor's edge.
const SNAP_DISTANCE: i32 = 15;

pub fn move_window_in_direction(
  window: WindowContainer,
  direction: &Direction,
  stack_direction: &TilingDirection,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  match window {
    WindowContainer::TilingWindow(window) => {
      move_tiling_window(window, direction, stack_direction, state, config)
    }
    WindowContainer::NonTilingWindow(non_tiling_window) => {
      match non_tiling_window.state() {
        WindowState::Floating(_) => {
          move_floating_window(non_tiling_window, direction, state)
        }
        WindowState::Fullscreen(_) => move_to_workspace_in_direction(
          &non_tiling_window.into(),
          direction,
          state,
        ),
        _ => Ok(()),
      }
    }
  }
}

fn move_tiling_window(
  window_to_move: TilingWindow,
  direction: &Direction,
  stack_direction: &TilingDirection,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  // Flatten the parent split container if it only contains the window.
  if let Some(split_parent) = window_to_move
    .parent()
    .and_then(|parent| parent.as_split().cloned())
  {
    if split_parent.child_count() == 1 {
      flatten_split_container(split_parent)?;
    }
  }

  let workspace = window_to_move.workspace().context("No workspace.")?;
  let arrow_axis = TilingDirection::from_direction(direction);

  if crate::commands::general::verbose_z_order_enabled() {
    layout_debug_log(format!(
      "move tiling: dir={direction:?} stack={stack_direction:?} ws_dir={:?} arrow_axis={arrow_axis:?}",
      workspace.tiling_direction()
    ));
  }

  if arrow_axis == *stack_direction {
    move_parallel(
      window_to_move,
      direction,
      stack_direction,
      &workspace,
      state,
      config,
    )
  } else {
    move_orthogonal(
      window_to_move,
      direction,
      stack_direction,
      &workspace,
      state,
      config,
    )
  }
}

/// Move along the global stack axis: reorder at the nearest matching
/// split.
fn move_parallel(
  window_to_move: TilingWindow,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  if let Some((neighbor, target_parent)) =
    find_directional_target(&window_to_move, direction)
  {
    let window_parent = window_to_move.parent();
    let target_parent_is_source =
      window_parent == Some(target_parent.clone());
    let nested_source = window_parent != Some(workspace.clone().into());

    // `move_container_within_tree` detaches the focused window before
    // using the target index. A nested window is promoted after its
    // neighboring column, preserving the column-relative order
    // expected by the user matrix (`... 1 4 ...` for focus 4 moving
    // left). Direct workspace children use the normal directional
    // before/after rule.
    let insert_index = if target_parent_is_source {
      directional_insert_index(&window_to_move, &neighbor, direction)
    } else if nested_source {
      neighbor.index() + 1
    } else {
      match direction {
        Direction::Left | Direction::Up => neighbor.index(),
        Direction::Right | Direction::Down => neighbor.index() + 1,
      }
    };

    move_container_within_tree(
      &window_to_move.clone().into(),
      &target_parent,
      insert_index,
      state,
    )?;
    finalize_tiling_move(workspace, state)?;
    return Ok(());
  }

  let workspace_container: Container = workspace.clone().into();
  if window_to_move.parent() != Some(workspace_container.clone()) {
    let anchor = workspace_child_containing(workspace, &window_to_move)
      .context("No workspace child containing window.")?;
    let target_index = match direction {
      Direction::Left | Direction::Up => anchor.index(),
      Direction::Right | Direction::Down => anchor.index() + 1,
    };
    move_container_within_tree(
      &window_to_move.clone().into(),
      &workspace_container,
      target_index,
      state,
    )?;
    finalize_tiling_move(workspace, state)?;
    return Ok(());
  }

  if workspace.tiling_direction() != *stack_direction {
    return restructure_workspace_to_stack(
      window_to_move,
      direction,
      stack_direction,
      workspace,
      state,
      config,
    );
  }

  // Edge of workspace on stack axis → cross-monitor (existing behavior).
  move_to_workspace_in_direction(&window_to_move.into(), direction, state)
}

/// Move on the axis orthogonal to the global stack: join neighbor into
/// stack-direction split, or restructure the workspace.
fn move_orthogonal(
  window_to_move: TilingWindow,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  if let Some((neighbor, _target_parent)) =
    find_directional_target(&window_to_move, direction)
  {
    return join_with_neighbor_on_stack(
      window_to_move,
      neighbor,
      direction,
      stack_direction,
      workspace,
      state,
      config,
    );
  }

  let workspace_container: Container = workspace.clone().into();
  if window_to_move.parent() != Some(workspace_container.clone()) {
    let anchor = workspace_child_containing(workspace, &window_to_move)
      .context("No workspace child containing window.")?;
    let target_index = match direction {
      Direction::Left | Direction::Up => anchor.index(),
      Direction::Right | Direction::Down => anchor.index() + 1,
    };
    move_container_within_tree(
      &window_to_move.clone().into(),
      &workspace_container,
      target_index,
      state,
    )?;
    finalize_tiling_move(workspace, state)?;
    return Ok(());
  }

  restructure_workspace_to_stack(
    window_to_move,
    direction,
    stack_direction,
    workspace,
    state,
    config,
  )
}

#[allow(clippy::needless_pass_by_value)]
fn join_with_neighbor_on_stack(
  window_to_move: TilingWindow,
  neighbor: TilingContainer,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let source_split = window_to_move
    .parent()
    .and_then(|parent| parent.as_split().cloned());

  match neighbor {
    TilingContainer::Split(split) => {
      if split.tiling_direction() != *stack_direction {
        split.set_tiling_direction(stack_direction.clone());
      }
      // An orthogonal move into an existing global-direction stack appends
      // the focused window and keeps the workspace axis unchanged. For
      // example, H[1 V[2 3] 4] + opposite-right becomes H[V[2 3 1] 4].
      let target_index = split.child_count();
      move_container_within_tree(
        &window_to_move.clone().into(),
        &split.clone().into(),
        target_index,
        state,
      )?;
    }
    TilingContainer::TilingWindow(neighbor_window) => {
      let target_parent = neighbor_window
        .parent()
        .context("Neighbor window has no parent.")?;

      // The focused window can be nested below the workspace child that
      // contains it. Promote it to the workspace beside the neighbor first
      // so wrapping it cannot reorder the surviving sibling subtree.
      if window_to_move.parent() != Some(target_parent.clone()) {
        let target_index = match direction {
          Direction::Left | Direction::Up => neighbor_window.index() + 1,
          Direction::Right | Direction::Down => neighbor_window.index(),
        };
        move_container_within_tree(
          &window_to_move.clone().into(),
          &target_parent,
          target_index,
          state,
        )?;
      }

      let split = SplitContainer::new(
        stack_direction.clone(),
        config.value.gaps.clone(),
      );
      // Orthogonal stacking always appends the moved window to the target
      // stack. Keep this leaf-target path consistent with the existing
      // split-target append path, including repeated Ctrl+Right moves.
      let wrap_kids: Vec<TilingContainer> =
        vec![neighbor_window.into(), window_to_move.clone().into()];
      wrap_in_split_container(&split, &target_parent, &wrap_kids)?;
    }
  }

  // Keep the live tree canonical with the pure planner: extracting the
  // only window other than the focused one collapses the
  // now-single-child source split instead of leaving a redundant
  // V[2]/H[2] container behind.
  if let Some(source_split) = source_split {
    if source_split.child_count() == 1 {
      flatten_split_container(source_split)?;
    }
  }

  // Preserve the workspace axis whenever a neighboring container existed.
  // If the new stack is the sole workspace child, the normal flattening
  // below promotes its direction instead.
  flatten_child_split_containers(&workspace.clone().into())?;

  // Promote a lone stack-direction split to be the workspace children.
  if workspace.tiling_children().count() == 1 {
    if let Some(TilingContainer::Split(only)) =
      workspace.tiling_children().next()
    {
      if only.tiling_direction() == *stack_direction {
        flatten_split_container(only)?;
      }
    }
  }

  equalize_tiling_children(workspace);
  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use std::collections::HashMap;

  use uuid::Uuid;
  use wm_common::{
    plan_global_move, GapsConfig, MoveNode, MoveTree, TilingDirection,
    WorkspaceConfig,
  };
  use wm_platform::{
    EventLoop, NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
  };

  use super::*;
  use crate::{
    commands::{container::attach_container, monitor::add_monitor},
    models::{Container, NativeMonitorProperties, NativeWindowProperties},
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

  fn test_config() -> UserConfig {
    let path = std::env::temp_dir()
      .join(format!("glazewm-global-move-test-{}.yaml", Uuid::new_v4()));
    UserConfig::new(Some(path)).expect("test config")
  }

  fn live_node(container: &TilingContainer) -> MoveNode {
    match container {
      TilingContainer::TilingWindow(window) => MoveNode::window(
        window
          .native_properties()
          .title
          .strip_prefix("window-")
          .expect("test window title")
          .to_string(),
      ),
      TilingContainer::Split(split) => MoveNode::split(
        split.tiling_direction(),
        split
          .tiling_children()
          .map(|child| live_node(&child))
          .collect(),
      ),
    }
  }

  fn live_tree(workspace: &Workspace) -> MoveTree {
    MoveTree {
      direction: workspace.tiling_direction(),
      children: workspace
        .tiling_children()
        .map(|child| live_node(&child))
        .collect(),
    }
  }

  fn wks2_fixture() -> (
    EventLoop,
    WmState,
    UserConfig,
    Workspace,
    TilingWindow,
    SplitContainer,
  ) {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    let config = test_config();

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "wks2".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");

    let window_one = test_window(1);
    let middle = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    let window_two = test_window(2);
    let window_three = test_window(3);
    let window_four = test_window(4);

    attach_container(
      &window_one.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach window one");
    attach_container(&middle.clone().into(), &workspace_container, None)
      .expect("attach middle split");
    attach_container(
      &window_four.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach window four");
    attach_container(
      &window_two.clone().into(),
      &middle.clone().into(),
      None,
    )
    .expect("attach window two");
    attach_container(
      &window_three.clone().into(),
      &middle.clone().into(),
      None,
    )
    .expect("attach window three");

    (event_loop, state, config, workspace, window_one, middle)
  }

  fn flat_three_fixture(
  ) -> (EventLoop, WmState, UserConfig, Workspace, TilingWindow) {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    let config = test_config();

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "flat-three".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");

    let window_one = test_window(1);
    let window_two = test_window(2);
    let window_three = test_window(3);
    for window in [window_one.clone(), window_two, window_three] {
      attach_container(&window.into(), &workspace_container, None)
        .expect("attach flat window");
    }

    (event_loop, state, config, workspace, window_one)
  }

  fn wks2_tall_stack_fixture() -> (
    EventLoop,
    WmState,
    UserConfig,
    Workspace,
    TilingWindow,
    TilingWindow,
  ) {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    let config = test_config();

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "wks2-tall".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");

    let window_one = test_window(1);
    let middle = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    let window_two = test_window(2);
    let window_three = test_window(3);
    let window_four = test_window(4);

    attach_container(
      &window_one.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach window one");
    attach_container(&middle.clone().into(), &workspace_container, None)
      .expect("attach middle split");
    for window in [window_two, window_three.clone(), window_four.clone()] {
      attach_container(&window.into(), &middle.clone().into(), None)
        .expect("attach stack window");
    }

    (
      event_loop,
      state,
      config,
      workspace,
      window_three,
      window_four,
    )
  }

  fn assert_tree_integrity(root: &Container) {
    fn visit(node: &Container, counts: &mut HashMap<Uuid, usize>) {
      *counts.entry(node.id()).or_default() += 1;
      if let Some(parent) = node.parent() {
        assert!(parent
          .children()
          .iter()
          .any(|child| child.id() == node.id()));
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
  fn nested_orthogonal_move_matches_global_move_planner() {
    let (_event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    let config = test_config();

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

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
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");
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
    .expect("attach focused window");
    attach_container(
      &window_two.clone().into(),
      &old_split.clone().into(),
      None,
    )
    .expect("attach sibling window");
    old_split
      .borrow_child_focus_order_mut()
      .make_contiguous()
      .reverse();

    let planner_input = MoveTree {
      direction: TilingDirection::Horizontal,
      children: vec![
        MoveNode::split(
          TilingDirection::Vertical,
          vec![MoveNode::window("1"), MoveNode::window("2")],
        ),
        MoveNode::window("3"),
      ],
    };
    let expected = plan_global_move(
      &planner_input,
      "1",
      &Direction::Right,
      &TilingDirection::Vertical,
    )
    .expect("planner move");

    join_with_neighbor_on_stack(
      window_one,
      neighbor.into(),
      &Direction::Right,
      &TilingDirection::Vertical,
      &workspace,
      &mut state,
      &config,
    )
    .expect("live move");

    assert_eq!(live_tree(&workspace), expected);
    assert_eq!(live_tree(&workspace).format_compact(), "H[2 V[3 1]]");
    assert_tree_integrity(&workspace_container);
  }

  #[test]
  fn wks2_normal_right_moves_focus_one_as_workspace_sibling() {
    let (_event_loop, mut state, config, workspace, window_one, _middle) =
      wks2_fixture();

    move_tiling_window(
      window_one,
      &Direction::Right,
      &TilingDirection::Horizontal,
      &mut state,
      &config,
    )
    .expect("move");

    let workspace_container: Container = workspace.clone().into();
    assert_eq!(live_tree(&workspace).format_compact(), "H[V[2 3] 1 4]");
    assert_tree_integrity(&workspace_container);
  }

  #[test]
  fn wks2_opposite_right_appends_focus_one_to_vertical_stack() {
    let (_event_loop, mut state, config, workspace, window_one, middle) =
      wks2_fixture();

    join_with_neighbor_on_stack(
      window_one,
      middle.into(),
      &Direction::Right,
      &TilingDirection::Vertical,
      &workspace,
      &mut state,
      &config,
    )
    .expect("move");

    let workspace_container: Container = workspace.clone().into();
    assert_eq!(live_tree(&workspace).format_compact(), "H[V[2 3 1] 4]");
    assert_tree_integrity(&workspace_container);
  }

  #[test]
  fn repeated_opposite_right_moves_append_to_the_bottom_of_each_stack() {
    let (_event_loop, mut state, config, workspace, window_one) =
      flat_three_fixture();

    move_tiling_window(
      window_one.clone(),
      &Direction::Right,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("first opposite move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[V[2 1] 3]");

    move_tiling_window(
      window_one,
      &Direction::Right,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("second opposite move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[2 V[3 1]]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn wks2_opposite_left_wraps_focus_three_without_flattening_columns() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      window_three,
      _window_four,
    ) = wks2_tall_stack_fixture();

    move_tiling_window(
      window_three,
      &Direction::Left,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("move");

    let workspace_container: Container = workspace.clone().into();
    assert_eq!(live_tree(&workspace).format_compact(), "H[V[1 3] V[2 4]]");
    assert_tree_integrity(&workspace_container);
  }

  #[test]
  fn user_matrix_focus_four_normal_up_stacks_horizontally() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Up,
      &TilingDirection::Horizontal,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[1 V[2 H[3 4]]]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn user_matrix_focus_four_opposite_up_stacks_vertically() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Up,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[1 V[2 4 3]]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn user_matrix_focus_four_normal_right_creates_new_column() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Right,
      &TilingDirection::Horizontal,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[1 V[2 3] 4]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn user_matrix_focus_four_normal_left_preserves_column_order() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Left,
      &TilingDirection::Horizontal,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[1 4 V[2 3]]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn user_matrix_focus_four_opposite_right_creates_new_column() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Right,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[1 V[2 3] 4]");
    assert_tree_integrity(&workspace.clone().into());
  }

  #[test]
  fn user_matrix_focus_four_opposite_left_stacks_with_left_column() {
    let (
      _event_loop,
      mut state,
      config,
      workspace,
      _window_three,
      window_four,
    ) = wks2_tall_stack_fixture();
    move_tiling_window(
      window_four,
      &Direction::Left,
      &TilingDirection::Vertical,
      &mut state,
      &config,
    )
    .expect("move");
    assert_eq!(live_tree(&workspace).format_compact(), "H[V[1 4] V[2 3]]");
    assert_tree_integrity(&workspace.clone().into());
  }
}

#[allow(clippy::needless_pass_by_value)]
fn restructure_workspace_to_stack(
  window_to_move: TilingWindow,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let old_dir = workspace.tiling_direction();

  let workspace_children = workspace
    .tiling_children()
    .filter(|container| container.id() != window_to_move.id())
    .collect::<Vec<_>>();

  if workspace_children.len() > 1 {
    let split_container =
      SplitContainer::new(old_dir, config.value.gaps.clone());
    wrap_in_split_container(
      &split_container,
      &workspace.clone().into(),
      &workspace_children,
    )?;
  }

  workspace.set_tiling_direction(stack_direction.clone());

  let target_index = match direction {
    Direction::Left | Direction::Up => 0,
    Direction::Right | Direction::Down => workspace.child_count(),
  };

  move_container_within_tree(
    &window_to_move.clone().into(),
    &workspace.clone().into(),
    target_index,
    state,
  )?;

  flatten_child_split_containers(&workspace.clone().into())?;
  equalize_tiling_children(workspace);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(())
}

fn workspace_child_containing(
  workspace: &Workspace,
  window: &TilingWindow,
) -> Option<TilingContainer> {
  workspace.tiling_children().find(|child| match child {
    TilingContainer::TilingWindow(w) => w.id() == window.id(),
    TilingContainer::Split(split) => {
      split_contains_window(split, window.id())
    }
  })
}

fn split_contains_window(
  split: &SplitContainer,
  window_id: uuid::Uuid,
) -> bool {
  split.tiling_children().any(|child| match child {
    TilingContainer::TilingWindow(w) => w.id() == window_id,
    TilingContainer::Split(inner) => {
      split_contains_window(&inner, window_id)
    }
  })
}

fn tiling_sibling_of(
  container: &TilingContainer,
  direction: &Direction,
) -> Option<TilingContainer> {
  match direction {
    Direction::Up | Direction::Left => container
      .prev_siblings()
      .find_map(|sibling| sibling.as_tiling_container().ok()),
    _ => container
      .next_siblings()
      .find_map(|sibling| sibling.as_tiling_container().ok()),
  }
}

/// Find the nearest sibling in the requested arrow direction whose parent
/// uses the same axis as the arrow. Walking from the focused window upward
/// is what makes nested moves target the inner stack before the workspace
/// root.
fn find_directional_target(
  window: &TilingWindow,
  direction: &Direction,
) -> Option<(TilingContainer, Container)> {
  let arrow_axis = TilingDirection::from_direction(direction);
  let mut current: Container = window.clone().into();

  loop {
    let parent = current.parent()?;
    if let Ok(direction_container) = parent.as_direction_container() {
      if direction_container.tiling_direction() == arrow_axis {
        if let Ok(current_tiling) = current.as_tiling_container() {
          if let Some(neighbor) =
            tiling_sibling_of(&current_tiling, direction)
          {
            return Some((neighbor, parent));
          }
        }
      }
    }
    current = parent;
  }
}

fn directional_insert_index(
  window_to_move: &TilingWindow,
  neighbor: &TilingContainer,
  direction: &Direction,
) -> usize {
  let window_index = window_to_move.index();
  let neighbor_index = neighbor.index();
  match direction {
    Direction::Left | Direction::Up if window_index < neighbor_index => {
      neighbor_index.saturating_sub(1)
    }
    Direction::Left | Direction::Up => neighbor_index,
    Direction::Right | Direction::Down
      if window_index < neighbor_index =>
    {
      neighbor_index
    }
    Direction::Right | Direction::Down => neighbor_index + 1,
  }
}

fn finalize_tiling_move(
  workspace: &Workspace,
  state: &mut WmState,
) -> anyhow::Result<()> {
  flatten_child_split_containers(&workspace.clone().into())?;
  equalize_tiling_children(workspace);
  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());
  Ok(())
}

fn equalize_tiling_children(parent: &Workspace) {
  let children: Vec<TilingContainer> = parent.tiling_children().collect();
  let count = children.len();
  if count == 0 {
    return;
  }
  #[allow(clippy::cast_precision_loss)]
  let size = 1.0 / count as f32;
  for child in children {
    child.set_tiling_size(size);
  }
}

fn move_to_workspace_in_direction(
  window_to_move: &WindowContainer,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let parent = window_to_move.parent().context("No parent.")?;
  let workspace = window_to_move.workspace().context("No workspace.")?;
  let monitor = parent.monitor().context("No monitor.")?;

  let target_workspace = state
    .monitor_in_direction(&monitor, direction)?
    .and_then(|monitor| monitor.displayed_workspace());

  if let Some(target_workspace) = target_workspace {
    if monitor.has_dpi_difference(&target_workspace.clone().into())? {
      window_to_move.set_has_pending_dpi_adjustment(true);
    }

    window_to_move.set_floating_placement(
      window_to_move
        .floating_placement()
        .translate_to_center(&target_workspace.to_rect()?),
    );

    if let WindowContainer::NonTilingWindow(window_to_move) =
      &window_to_move
    {
      window_to_move.set_insertion_target(None);
    }

    let target_index = match direction {
      Direction::Down | Direction::Right => 0,
      _ => target_workspace.child_count(),
    };

    let focus_target = state.focus_target_after_removal(window_to_move);

    move_container_within_tree(
      &window_to_move.clone().into(),
      &target_workspace.clone().into(),
      target_index,
      state,
    )?;

    if let Some(focus_target) = focus_target {
      set_focused_descendant(
        &focus_target,
        Some(&workspace.clone().into()),
      );
    }

    state
      .pending_sync
      .queue_container_to_redraw(window_to_move.clone())
      .queue_containers_to_redraw(target_workspace.tiling_children())
      .queue_containers_to_redraw(parent.tiling_children())
      .queue_cursor_jump()
      .queue_workspace_to_reorder(target_workspace);
  }

  Ok(())
}

fn move_floating_window(
  window_to_move: NonTilingWindow,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let new_position =
    new_floating_position(&window_to_move, direction, state)?;

  if let Some((position_rect, target_monitor)) = new_position {
    let monitor = window_to_move.monitor().context("No monitor.")?;

    if monitor.id() != target_monitor.id()
      && monitor.has_dpi_difference(&target_monitor.into())?
    {
      window_to_move.set_has_pending_dpi_adjustment(true);
    }

    window_to_move.set_floating_placement(position_rect);
    state.pending_sync.queue_container_to_redraw(window_to_move);
  }

  Ok(())
}

fn new_floating_position(
  window_to_move: &NonTilingWindow,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<Option<(Rect, Monitor)>> {
  let monitor = window_to_move.monitor().context("No monitor.")?;
  let monitor_rect = monitor.native_properties().working_area;
  let window_pos = window_to_move.native_properties().frame;

  let is_on_monitor_edge = match direction {
    Direction::Up => window_pos.top == monitor_rect.top,
    Direction::Down => window_pos.bottom == monitor_rect.bottom,
    Direction::Left => window_pos.left == monitor_rect.left,
    Direction::Right => window_pos.right == monitor_rect.right,
  };

  if is_on_monitor_edge {
    let next_monitor = state.monitor_in_direction(&monitor, direction)?;

    if let Some(next_monitor) = next_monitor {
      let monitor_rect = next_monitor.native().working_area()?.clone();

      let position = snap_to_monitor_edge(
        &window_pos,
        &monitor_rect,
        &direction.inverse(),
      )
      .clamp(&monitor_rect);

      return Ok(Some((position, next_monitor)));
    }

    return Ok(None);
  }

  let (monitor_length, window_length) = match direction {
    Direction::Up | Direction::Down => {
      (monitor_rect.height(), window_pos.height())
    }
    _ => (monitor_rect.width(), window_pos.width()),
  };

  let length_delta = monitor_length - window_length;

  #[allow(clippy::cast_precision_loss)]
  let move_distance = match window_length as f32 / monitor_length as f32 {
    x if (0.0..0.2).contains(&x) => length_delta / 5,
    x if (0.2..0.4).contains(&x) => length_delta / 4,
    x if (0.4..0.6).contains(&x) => length_delta / 3,
    _ => length_delta / 2,
  };

  let should_snap_to_edge = match direction {
    Direction::Up => {
      window_pos.top - move_distance - SNAP_DISTANCE < monitor_rect.top
    }
    Direction::Down => {
      window_pos.bottom + move_distance + SNAP_DISTANCE
        > monitor_rect.bottom
    }
    Direction::Left => {
      window_pos.left - move_distance - SNAP_DISTANCE < monitor_rect.left
    }
    Direction::Right => {
      window_pos.right + move_distance + SNAP_DISTANCE > monitor_rect.right
    }
  };

  if should_snap_to_edge {
    let position =
      snap_to_monitor_edge(&window_pos, &monitor_rect, direction);

    return Ok(Some((position, monitor)));
  }

  let should_snap_to_inverse_edge = match direction {
    Direction::Up => window_pos.bottom > monitor_rect.bottom,
    Direction::Down => window_pos.top < monitor_rect.top,
    Direction::Left => window_pos.right > monitor_rect.right,
    Direction::Right => window_pos.left < monitor_rect.left,
  };

  let position = if should_snap_to_inverse_edge {
    snap_to_monitor_edge(&window_pos, &monitor_rect, &direction.inverse())
  } else {
    window_pos.translate_in_direction(direction, move_distance)
  };

  Ok(Some((position, monitor)))
}

fn snap_to_monitor_edge(
  window_pos: &Rect,
  monitor_rect: &Rect,
  edge: &Direction,
) -> Rect {
  let (x, y) = match edge {
    Direction::Up => (window_pos.x(), monitor_rect.top),
    Direction::Down => {
      (window_pos.x(), monitor_rect.bottom - window_pos.height())
    }
    Direction::Left => (monitor_rect.left, window_pos.y()),
    Direction::Right => {
      (monitor_rect.right - window_pos.width(), window_pos.y())
    }
  };

  window_pos.translate_to_coordinates(x, y)
}
