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
    Monitor, NonTilingWindow, SplitContainer, TilingContainer,
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

  layout_debug_log(format!(
    "move tiling: dir={direction:?} stack={stack_direction:?} ws_dir={:?} arrow_axis={arrow_axis:?}",
    workspace.tiling_direction()
  ));

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

/// Move along the global stack axis: reorder as siblings (splits opaque).
fn move_parallel(
  window_to_move: TilingWindow,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
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

  let anchor = workspace_child_containing(workspace, &window_to_move)
    .context("No workspace child containing window.")?;
  let neighbor = tiling_sibling_of(&anchor, direction);

  let Some(neighbor) = neighbor else {
    // Edge of workspace on stack axis → cross-monitor (existing behavior).
    return move_to_workspace_in_direction(
      &window_to_move.into(),
      direction,
      state,
    );
  };

  let insert_index = match direction {
    Direction::Left | Direction::Up => neighbor.index(),
    Direction::Right | Direction::Down => neighbor.index() + 1,
  };

  move_container_within_tree(
    &window_to_move.clone().into(),
    &workspace.clone().into(),
    insert_index,
    state,
  )?;

  flatten_child_split_containers(&workspace.clone().into())?;
  equalize_tiling_children(workspace);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(())
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
  let anchor = workspace_child_containing(workspace, &window_to_move)
    .context("No workspace child containing window.")?;
  let neighbor = tiling_sibling_of(&anchor, direction);

  if let Some(neighbor) = neighbor {
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

  restructure_workspace_to_stack(
    window_to_move,
    direction,
    stack_direction,
    workspace,
    state,
    config,
  )
}

fn join_with_neighbor_on_stack(
  window_to_move: TilingWindow,
  neighbor: TilingContainer,
  direction: &Direction,
  stack_direction: &TilingDirection,
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  match neighbor {
    TilingContainer::Split(split) => {
      if split.tiling_direction() != *stack_direction {
        split.set_tiling_direction(stack_direction.clone());
      }
      let target_index = match direction {
        Direction::Left | Direction::Up => split.child_count(),
        Direction::Right | Direction::Down => 0,
      };
      move_container_within_tree(
        &window_to_move.clone().into(),
        &split.clone().into(),
        target_index,
        state,
      )?;
    }
    TilingContainer::TilingWindow(neighbor_window) => {
      let split = SplitContainer::new(
        stack_direction.clone(),
        config.value.gaps.clone(),
      );
      let wrap_kids: Vec<TilingContainer> = match direction {
        Direction::Left | Direction::Up => {
          vec![neighbor_window.into(), window_to_move.clone().into()]
        }
        Direction::Right | Direction::Down => {
          vec![window_to_move.clone().into(), neighbor_window.into()]
        }
      };
      wrap_in_split_container(
        &split,
        &workspace.clone().into(),
        &wrap_kids,
      )?;
    }
  }

  workspace.set_tiling_direction(stack_direction.clone());
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

fn split_contains_window(split: &SplitContainer, window_id: uuid::Uuid) -> bool {
  split.tiling_children().any(|child| match child {
    TilingContainer::TilingWindow(w) => w.id() == window_id,
    TilingContainer::Split(inner) => split_contains_window(&inner, window_id),
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
