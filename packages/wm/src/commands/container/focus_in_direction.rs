use anyhow::Context;
use wm_common::{geometric_focus_target_index, WindowState};
use wm_platform::{Direction, Rect};

use super::set_focused_descendant;
use crate::{
  commands::general::layout_debug_log,
  models::{Container, TilingWindow},
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  wm_state::WmState,
};

pub fn focus_in_direction(
  origin_container: &Container,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let focus_target = match origin_container {
    Container::TilingWindow(_) => {
      // If a suitable focus target isn't found in the current workspace,
      // attempt to find a workspace in the given direction.
      geometric_tiling_focus_target(origin_container, direction)?
        .map_or_else(
          || workspace_focus_target(origin_container, direction, state),
          |container| Ok(Some(container)),
        )?
    }
    Container::NonTilingWindow(ref non_tiling_window) => {
      match non_tiling_window.state() {
        WindowState::Floating(_) => {
          floating_focus_target(origin_container, direction)
        }
        WindowState::Fullscreen(_) => {
          workspace_focus_target(origin_container, direction, state)?
        }
        _ => None,
      }
    }
    Container::Workspace(_) => {
      workspace_focus_target(origin_container, direction, state)?
    }
    _ => None,
  };

  // Set focus to the target container.
  if let Some(focus_target) = focus_target {
    set_focused_descendant(&focus_target, None);
    state.pending_sync.queue_focus_change().queue_cursor_jump();
  }

  Ok(())
}

fn floating_focus_target(
  origin_container: &Container,
  direction: &Direction,
) -> Option<Container> {
  let is_floating = |sibling: &Container| {
    sibling.as_non_tiling_window().is_some_and(|window| {
      matches!(window.state(), WindowState::Floating(_))
    })
  };

  let mut floating_siblings =
    origin_container.siblings().filter(is_floating);

  // Wrap if next/previous floating window is not found.
  match direction {
    Direction::Left => origin_container
      .next_siblings()
      .find(is_floating)
      .or_else(|| floating_siblings.last()),
    Direction::Right => origin_container
      .prev_siblings()
      .find(is_floating)
      .or_else(|| floating_siblings.next()),
    // Cannot focus vertically from a floating window.
    _ => None,
  }
}

/// Gets a focus target within the current workspace using the focused
/// window's directional edge midpoint rather than the tiling tree.
fn geometric_tiling_focus_target(
  origin_container: &Container,
  direction: &Direction,
) -> anyhow::Result<Option<Container>> {
  let origin_window = origin_container
    .as_tiling_window()
    .context("Geometric focus requires a tiling window.")?;
  let origin_rect = origin_window.to_rect()?;
  let workspace = origin_window.workspace().context("No workspace.")?;
  let candidates = workspace
    .descendants()
    .filter_map(|container| match container {
      Container::TilingWindow(window)
        if window.id() != origin_window.id() =>
      {
        Some(window)
      }
      _ => None,
    })
    .collect::<Vec<_>>();
  let candidate_rects = candidates
    .iter()
    .map(TilingWindow::to_rect)
    .collect::<anyhow::Result<Vec<Rect>>>()?;
  let target_index = geometric_focus_target_index(
    &origin_rect,
    direction,
    &candidate_rects,
  );

  let candidate_summary = candidates
    .iter()
    .zip(candidate_rects.iter())
    .map(|(window, rect)| format!("{}={rect:?}", window.id()))
    .collect::<Vec<_>>()
    .join(", ");
  layout_debug_log(format!(
    "geometric focus: origin={} rect={origin_rect:?} dir={direction:?} candidates=[{candidate_summary}] target={}",
    origin_window.id(),
    target_index
      .and_then(|index| candidates.get(index))
      .map_or_else(|| "none".into(), |window| window.id().to_string()),
  ));

  Ok(
    target_index
      .and_then(|index| candidates.get(index).cloned())
      .map(Into::into),
  )
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use uuid::Uuid;
  use wm_common::{GapsConfig, TilingDirection, WorkspaceConfig};
  use wm_platform::{
    EventLoop, NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
  };

  use super::*;
  use crate::{
    commands::{container::attach_container, monitor::add_monitor},
    models::{
      Container, NativeMonitorProperties, NativeWindowProperties,
      SplitContainer, Workspace,
    },
    traits::{CommonGetters, TilingSizeGetters},
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

  #[test]
  fn live_tree_geometry_selects_window_containing_edge_midpoint() {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);
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
        name: "geometric-focus".into(),
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
    let window_four = test_window(4);
    let window_five = test_window(5);
    let window_six = test_window(6);
    let middle_split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );
    let right_split = SplitContainer::new(
      TilingDirection::Vertical,
      GapsConfig::default(),
    );

    // Visual layout:
    //
    // 124
    // 125
    // 136
    attach_container(
      &window_one.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach window 1");
    attach_container(
      &middle_split.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach middle split");
    attach_container(
      &right_split.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach right split");
    attach_container(
      &window_two.clone().into(),
      &middle_split.clone().into(),
      None,
    )
    .expect("attach window 2");
    attach_container(
      &window_three.clone().into(),
      &middle_split.clone().into(),
      None,
    )
    .expect("attach window 3");
    attach_container(
      &window_four.clone().into(),
      &right_split.clone().into(),
      None,
    )
    .expect("attach window 4");
    attach_container(
      &window_five.clone().into(),
      &right_split.clone().into(),
      None,
    )
    .expect("attach window 5");
    attach_container(
      &window_six.clone().into(),
      &right_split.clone().into(),
      None,
    )
    .expect("attach window 6");

    window_two.set_tiling_size(2.0 / 3.0);
    window_three.set_tiling_size(1.0 / 3.0);

    let target = geometric_tiling_focus_target(
      &window_one.clone().into(),
      &Direction::Right,
    )
    .expect("geometric focus target")
    .expect("window to the right");

    assert_eq!(target.id(), window_two.id());
    drop(event_loop);
  }
}

/// Gets a focus target outside of the current workspace in the given
/// direction.
///
/// This will descend into the workspace in the given direction, and will
/// always return a tiling container. This makes it different from the
/// `focus_workspace` command with `FocusWorkspaceTarget::Direction`.
fn workspace_focus_target(
  origin_container: &Container,
  direction: &Direction,
  state: &WmState,
) -> anyhow::Result<Option<Container>> {
  let monitor = origin_container.monitor().context("No monitor.")?;

  let target_workspace = state
    .monitor_in_direction(&monitor, direction)?
    .and_then(|monitor| monitor.displayed_workspace());

  let focused_fullscreen = target_workspace
    .as_ref()
    .and_then(|workspace| workspace.descendant_focus_order().next())
    .filter(|focused| match focused {
      Container::NonTilingWindow(window) => {
        matches!(window.state(), WindowState::Fullscreen(_))
      }
      _ => false,
    });

  let focus_target = focused_fullscreen
    .or_else(|| {
      target_workspace.as_ref().and_then(|workspace| {
        workspace
          .descendant_in_direction(&direction.inverse())
          .map(Into::into)
      })
    })
    .or(target_workspace.map(Into::into));

  Ok(focus_target)
}
