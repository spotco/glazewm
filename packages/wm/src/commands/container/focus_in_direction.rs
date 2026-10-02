use anyhow::Context;
use uuid::Uuid;
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
    // Queue layer reorder in the same platform_sync as SetForegroundWindow
    // so tiling/floating directional focus does not briefly rely on
    // per-window bring_to_front SWPs before the workspace chain apply.
    if let Some(workspace) = focus_target.workspace() {
      state
        .pending_sync
        .queue_workspace_to_reorder(workspace.clone());
      // Super+arrow between normal floaters must promote ONLY the
      // selected window (invariant). Mark as targeted so
      // windows_to_bring_to_front does not raise every peer floater and
      // so bring_to_front defers to reorder_focused_workspace_layers -
      // matching the native floating focus path. Without this mark,
      // focus_in_direction queued workspace reorder while defer still
      // only covered tiling / focused_window_to_bring_to_front, so both
      // applicators raced (3 floaters + tiles Super+Left/Right).
      if let Ok(window) = focus_target.as_window_container() {
        if matches!(
          window.state(),
          WindowState::Floating(config) if !config.shown_on_top
        ) {
          state.pending_sync.queue_focused_window_to_bring_to_front(
            &workspace,
            window.id(),
          );
        }
      }
    }
  }

  Ok(())
}

/// Super+arrow cycle among detached/floating windows on the focused
/// workspace.
///
/// Membership is workspace-scoped (not merely immediate siblings) so a
/// floater stays in the cycle after detach/reattach and after transfers
/// between workspaces. Order follows workspace `descendant_focus_order`
/// among floating windows is *not* used — child-tree order under the
/// workspace (stable across focus) matches the historical sibling walk.
///
/// Wrap is a true ring: Left = next, Right = previous, modulo len. The
/// old sibling wrap used `floating_siblings.last()` / `.next()`, which
/// failed to reach the opposite end. After `move --workspace`, the
/// transferred floater is appended last, so Super+Right from an older
/// floater never landed on it (Steam WKS2→back repro).
fn floating_focus_target(
  origin_container: &Container,
  direction: &Direction,
) -> Option<Container> {
  let workspace = origin_container.workspace()?;
  let floating = floating_windows_on_workspace(&workspace);

  // Need at least one peer to cycle to.
  if floating.len() < 2 {
    return None;
  }

  let current_index = floating
    .iter()
    .position(|container| container.id() == origin_container.id())?;

  let target_index = match direction {
    // Preserve historical left=next / right=prev relative to child order.
    Direction::Left => (current_index + 1) % floating.len(),
    Direction::Right => {
      (current_index + floating.len() - 1) % floating.len()
    }
    // Cannot focus vertically from a floating window.
    _ => return None,
  };

  floating.into_iter().nth(target_index)
}

/// Floating windows that belong to `workspace`, in stable cycle order.
///
/// Prefers direct workspace children (the invariant for non-tiling
/// windows). Also includes any nested floating descendants so a floater
/// that landed under a split still joins the Super+arrow cycle until it
/// is reparented.
fn floating_windows_on_workspace(
  workspace: &crate::models::Workspace,
) -> Vec<Container> {
  let is_floating = |container: &Container| {
    container.as_non_tiling_window().is_some_and(|window| {
      matches!(window.state(), WindowState::Floating(_))
    })
  };

  let mut floating = workspace
    .children()
    .into_iter()
    .filter(is_floating)
    .collect::<Vec<_>>();

  // Nested floaters (should not happen if invariants hold) still count.
  for descendant in workspace.descendants() {
    if !is_floating(&descendant) {
      continue;
    }
    if floating
      .iter()
      .any(|existing| existing.id() == descendant.id())
    {
      continue;
    }
    floating.push(descendant);
  }

  floating
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
  geometric_tiling_focus_target_in_workspace(
    origin_window.id(),
    &origin_rect,
    &workspace,
    direction,
  )
}

fn geometric_tiling_focus_target_in_workspace(
  origin_id: Uuid,
  origin_rect: &Rect,
  workspace: &crate::models::Workspace,
  direction: &Direction,
) -> anyhow::Result<Option<Container>> {
  let candidates = workspace
    .descendants()
    .filter_map(|container| match container {
      Container::TilingWindow(window) if window.id() != origin_id => {
        Some(window)
      }
      _ => None,
    })
    .collect::<Vec<_>>();
  let candidate_rects = candidates
    .iter()
    .map(TilingWindow::to_rect)
    .collect::<anyhow::Result<Vec<Rect>>>()?;
  let target_index =
    geometric_focus_target_index(origin_rect, direction, &candidate_rects);

  let candidate_summary = candidates
    .iter()
    .zip(candidate_rects.iter())
    .map(|(window, rect)| format!("{}={rect:?}", window.id()))
    .collect::<Vec<_>>()
    .join(", ");
  if crate::commands::general::verbose_z_order_enabled() {
    layout_debug_log(format!(
      "geometric focus: origin={} rect={origin_rect:?} dir={direction:?} candidates=[{candidate_summary}] target={}",
      origin_id,
      target_index
        .and_then(|index| candidates.get(index))
        .map_or_else(|| "none".into(), |window| window.id().to_string()),
    ));
  }

  Ok(
    target_index
      .and_then(|index| candidates.get(index).cloned())
      .map(Into::into),
  )
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

  let geometric_target = target_workspace
    .as_ref()
    .map(|workspace| {
      let origin_rect = origin_container.to_rect()?;
      geometric_tiling_focus_target_in_workspace(
        origin_container.id(),
        &origin_rect,
        workspace,
        direction,
      )
    })
    .transpose()?
    .flatten();

  let focus_target = focused_fullscreen
    .or(geometric_target)
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use uuid::Uuid;
  use wm_common::{
    FloatingStateConfig, GapsConfig, TilingDirection, WindowState,
    WorkspaceConfig,
  };
  use wm_platform::{
    Direction, EventLoop, NativeWindow, NativeWindowWindowsExt, Rect,
    RectDelta,
  };

  use super::*;
  use crate::{
    commands::{
      container::{attach_container, set_focused_descendant},
      monitor::add_monitor,
      window::{move_window_to_workspace, update_window_state},
    },
    models::{
      Container, NativeMonitorProperties, NativeWindowProperties,
      NonTilingWindow, SplitContainer, TilingWindow, Workspace,
      WorkspaceTarget,
    },
    traits::{CommonGetters, TilingSizeGetters},
    user_config::UserConfig,
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

  fn floating_window(id: u128, title: &str) -> NonTilingWindow {
    NonTilingWindow::new(
      Some(Uuid::from_u128(id)),
      NativeWindow::from_handle(id as isize),
      NativeWindowProperties {
        title: title.into(),
        class_name: "test".into(),
        process_name: title.into(),
        process_path: None,
        frame: Rect::from_xy(0, 0, 100, 100),
        is_minimized: false,
        is_maximized: false,
        is_resizable: true,
        shadow_borders: RectDelta::zero(),
      },
      WindowState::Floating(FloatingStateConfig {
        centered: false,
        shown_on_top: false,
      }),
      Some(WindowState::Tiling),
      RectDelta::zero(),
      None,
      Rect::from_xy(0, 0, 100, 100),
      false,
      Vec::new(),
      None,
    )
  }

  fn test_config() -> UserConfig {
    let path = std::env::temp_dir().join(format!(
      "glazewm-floating-cycle-test-{}.yaml",
      Uuid::new_v4()
    ));
    UserConfig::new(Some(path)).expect("test config")
  }

  fn cycle_ids(
    origin: &Container,
    direction: &Direction,
    steps: usize,
  ) -> Vec<Uuid> {
    let mut current = origin.clone();
    let mut ids = Vec::new();
    for _ in 0..steps {
      let next = floating_focus_target(&current, direction)
        .expect("floating cycle peer");
      ids.push(next.id());
      current = next;
    }
    ids
  }

  #[test]
  #[allow(clippy::too_many_lines)]
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

  #[test]
  fn floating_cycle_is_full_ring_including_appended_peer() {
    // Old wrap used siblings.last()/next() and skipped the opposite end.
    // A floater appended last (typical after move --workspace) was
    // unreachable via Super+Right from the first floater.
    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "1".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    let tiling = test_window(10);
    let floater_a = floating_window(1, "floater-a");
    let floater_b = floating_window(2, "floater-b");
    let steam = floating_window(3, "Steam");

    attach_container(&tiling.clone().into(), &workspace_container, None)
      .expect("attach tiling");
    attach_container(
      &floater_a.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach a");
    attach_container(
      &floater_b.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach b");
    // Appended last — same as move_window_to_workspace target index.
    attach_container(&steam.clone().into(), &workspace_container, None)
      .expect("attach steam");

    let from_a = floater_a.clone().into();
    let right_cycle = cycle_ids(&from_a, &Direction::Right, 3);
    assert_eq!(
      right_cycle,
      vec![steam.id(), floater_b.id(), floater_a.id()],
      "Super+Right from first floater must wrap to appended Steam"
    );

    let left_cycle = cycle_ids(&from_a, &Direction::Left, 3);
    assert_eq!(
      left_cycle,
      vec![floater_b.id(), steam.id(), floater_a.id()],
      "Super+Left must visit every floater including Steam"
    );
  }

  #[test]
  #[allow(clippy::too_many_lines)]
  fn floating_cycle_includes_window_after_workspace_transfer_both_ways() {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);
    let mut config = test_config();

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

    let workspace_one = Workspace::new(
      WorkspaceConfig {
        name: "1".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: true,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_two = Workspace::new(
      WorkspaceConfig {
        name: "2".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: true,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    attach_container(
      &workspace_one.clone().into(),
      &monitor.clone().into(),
      None,
    )
    .expect("attach wks1");
    attach_container(
      &workspace_two.clone().into(),
      &monitor.clone().into(),
      None,
    )
    .expect("attach wks2");

    let peer = floating_window(1, "peer");
    let steam = floating_window(2, "Steam");
    attach_container(
      &peer.clone().into(),
      &workspace_one.clone().into(),
      None,
    )
    .expect("attach peer");
    attach_container(
      &steam.clone().into(),
      &workspace_one.clone().into(),
      None,
    )
    .expect("attach steam");
    set_focused_descendant(&steam.clone().into(), None);

    move_window_to_workspace(
      steam.clone().into(),
      WorkspaceTarget::Name("2".into()),
      &mut state,
      &config,
    )
    .expect("move steam to wks2");

    assert!(
      floating_focus_target(&peer.clone().into(), &Direction::Left)
        .is_none(),
      "peer alone on wks1 has no floating cycle peer"
    );
    assert_eq!(
      steam.workspace().map(|ws| ws.config().name),
      Some("2".into())
    );

    // Focus follows to wks2 in the user repro; focus steam then move back.
    set_focused_descendant(&steam.clone().into(), None);
    move_window_to_workspace(
      steam.clone().into(),
      WorkspaceTarget::Name("1".into()),
      &mut state,
      &config,
    )
    .expect("move steam back to wks1");

    assert_eq!(
      steam.workspace().map(|ws| ws.config().name),
      Some("1".into())
    );

    let from_peer = peer.clone().into();
    let right_ids = cycle_ids(&from_peer, &Direction::Right, 2);
    assert!(
      right_ids.contains(&steam.id()),
      "after WKS2→back, Super+Right cycle from peer must include Steam: {right_ids:?}"
    );
    let left_ids = cycle_ids(&from_peer, &Direction::Left, 2);
    assert!(
      left_ids.contains(&steam.id()),
      "after WKS2→back, Super+Left cycle from peer must include Steam: {left_ids:?}"
    );

    // Floaters on the other workspace must not enter this cycle.
    let foreign = floating_window(9, "foreign");
    attach_container(
      &foreign.clone().into(),
      &workspace_two.clone().into(),
      None,
    )
    .expect("attach foreign on wks2");
    let cycle_after_foreign = cycle_ids(&from_peer, &Direction::Left, 4);
    assert!(
      !cycle_after_foreign.contains(&foreign.id()),
      "cycle must stay on focused workspace: {cycle_after_foreign:?}"
    );

    let _ = &mut config;
    drop(event_loop);
  }

  #[test]
  fn floating_cycle_survives_detach_reattach_and_nested_floater() {
    let (event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);
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
        name: "1".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: true,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    attach_container(
      &workspace.clone().into(),
      &monitor.clone().into(),
      None,
    )
    .expect("attach workspace");

    let peer = floating_window(1, "peer");
    let steam = floating_window(2, "Steam");
    attach_container(
      &peer.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .expect("attach peer");
    attach_container(
      &steam.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .expect("attach steam");

    // Detach (tile) then reattach (float) Steam.
    let tiled = update_window_state(
      steam.clone().into(),
      WindowState::Tiling,
      &mut state,
      &config,
    )
    .expect("tile steam");
    let floated = update_window_state(
      tiled,
      WindowState::Floating(FloatingStateConfig {
        centered: false,
        shown_on_top: false,
      }),
      &mut state,
      &config,
    )
    .expect("float steam again");

    let from_peer = peer.clone().into();
    let after_reattach = cycle_ids(&from_peer, &Direction::Left, 2);
    assert!(
      after_reattach.contains(&floated.id()),
      "detach/reattach must keep Steam in cycle: {after_reattach:?}"
    );

    // Nested floater under a split still belongs to the workspace cycle.
    let nested_split = SplitContainer::new(
      TilingDirection::Horizontal,
      GapsConfig::default(),
    );
    let nested = floating_window(3, "nested");
    attach_container(
      &nested_split.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .expect("attach split");
    attach_container(
      &nested.clone().into(),
      &nested_split.clone().into(),
      None,
    )
    .expect("attach nested floater");

    let with_nested = floating_windows_on_workspace(&workspace);
    assert!(
      with_nested.iter().any(|c| c.id() == nested.id()),
      "nested floater must be in workspace cycle membership"
    );
    let cycle_with_nested = cycle_ids(&from_peer, &Direction::Left, 3);
    assert!(
      cycle_with_nested.contains(&nested.id()),
      "Super+arrow must reach nested floater on same workspace: {cycle_with_nested:?}"
    );

    drop(event_loop);
  }
}
