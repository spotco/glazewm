use anyhow::Context;
use tracing::info;
use wm_common::WindowState;

use crate::{
  commands::{
    container::{
      move_container_within_tree, set_focused_descendant,
      wrap_in_split_container,
    },
    workspace::activate_workspace,
  },
  models::{
    Container, SplitContainer, TilingContainer, WindowContainer,
    Workspace, WorkspaceTarget,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn move_window_to_workspace(
  window: WindowContainer,
  target: WorkspaceTarget,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let current_workspace = window.workspace().context("No workspace.")?;
  let current_monitor =
    current_workspace.monitor().context("No monitor.")?;

  let (target_workspace_name, target_workspace) =
    state.workspace_by_target(&current_workspace, target, config)?;

  // Retrieve or activate the target workspace by its name.
  let target_workspace = match target_workspace {
    Some(_) => anyhow::Ok(target_workspace),
    _ => match target_workspace_name {
      Some(name) => {
        activate_workspace(Some(&name), None, state, config)?;

        Ok(state.workspace_by_name(&name))
      }
      _ => Ok(None),
    },
  }?;

  if let Some(target_workspace) = target_workspace {
    if target_workspace.id() == current_workspace.id() {
      return Ok(());
    }

    info!(
      "Moving window to workspace: '{}'.",
      target_workspace.config().name
    );

    let target_monitor =
      target_workspace.monitor().context("No monitor.")?;

    // Since target workspace could be on a different monitor, adjustments
    // might need to be made because of DPI.
    if current_monitor
      .has_dpi_difference(&target_monitor.clone().into())?
    {
      window.set_has_pending_dpi_adjustment(true);
    }

    // Update floating placement if the window has to cross monitors.
    if target_monitor.id() != current_monitor.id() {
      window.set_floating_placement(
        window
          .floating_placement()
          .translate_to_center(&target_workspace.to_rect()?),
      );
    }

    if let WindowContainer::NonTilingWindow(window) = &window {
      window.set_insertion_target(None);
    }

    // Focus target is `None` if the window is not focused.
    let focus_target = state.focus_target_after_removal(&window);

    let focus_reset_target = if target_workspace.is_displayed() {
      None
    } else {
      target_monitor.descendant_focus_order().next()
    };

    let insertion_sibling = target_workspace
      .descendant_focus_order()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| descendant.state() == WindowState::Tiling);

    // Insert the window into the target workspace.
    match (window.is_tiling_window(), insertion_sibling.is_some()) {
      (true, true) => {
        if let Some(insertion_sibling) = insertion_sibling {
          move_container_within_tree(
            &window.clone().into(),
            &insertion_sibling.clone().parent().context("No parent.")?,
            insertion_sibling.index() + 1,
            state,
          )?;
        }
      }
      _ => {
        move_container_within_tree(
          &window.clone().into(),
          &target_workspace.clone().into(),
          target_workspace.child_count(),
          state,
        )?;
      }
    }

    // When moving a focused window within the tree to another workspace,
    // the target workspace will get displayed. If moving the window e.g.
    // from monitor 1 -> 2, and the target workspace is hidden on that
    // monitor, we want to reset focus to the workspace that was displayed
    // on that monitor.
    if let Some(focus_reset_target) = focus_reset_target {
      set_focused_descendant(
        &focus_reset_target,
        Some(&target_monitor.into()),
      );
    }

    // Retain focus within the workspace from where the window was moved.
    if let Some(focus_target) = focus_target {
      set_focused_descendant(&focus_target, None);
      state.pending_sync.queue_focus_change();
    }

    match window {
      WindowContainer::NonTilingWindow(_) => {
        state.pending_sync.queue_container_to_redraw(window);
      }
      WindowContainer::TilingWindow(_) => {
        state
          .pending_sync
          .queue_containers_to_redraw(current_workspace.tiling_children())
          .queue_containers_to_redraw(target_workspace.tiling_children());
      }
    }

    state
      .pending_sync
      .queue_workspace_to_reorder(target_workspace);
  }

  Ok(())
}

/// Moves every window in `current_workspace` to another workspace while
/// preserving the source workspace's tiling tree.
///
/// When the target already contains tiled windows, the source tree is kept
/// as one nested split. This preserves the source windows' relative sizes
/// and layout while allowing the target's existing layout to remain in
/// place.
pub fn move_all_windows_to_workspace(
  current_workspace: &Workspace,
  target: WorkspaceTarget,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let has_windows = current_workspace
    .descendants()
    .any(|descendant| descendant.as_window_container().is_ok());
  if !has_windows {
    return Ok(());
  }

  // Moving all windows to the current workspace is always a no-op. Avoid
  // workspace target resolution here because it can intentionally toggle a
  // workspace when refocus toggling is enabled.
  if let WorkspaceTarget::Name(name) = &target {
    if current_workspace.config().name == *name {
      return Ok(());
    }
  }

  let (target_workspace_name, target_workspace) =
    state.workspace_by_target(current_workspace, target, config)?;

  let target_workspace = match target_workspace {
    Some(workspace) => workspace,
    None => match target_workspace_name {
      Some(name) => {
        activate_workspace(Some(&name), None, state, config)?;
        state
          .workspace_by_name(&name)
          .context("Target workspace was not activated.")?
      }
      None => return Ok(()),
    },
  };

  if target_workspace.id() == current_workspace.id() {
    return Ok(());
  }

  let current_monitor =
    current_workspace.monitor().context("No monitor.")?;
  let target_monitor =
    target_workspace.monitor().context("No monitor.")?;
  let has_dpi_difference =
    current_monitor.has_dpi_difference(&target_monitor.clone().into())?;

  let source_tiling_children: Vec<TilingContainer> =
    current_workspace.tiling_children().collect();

  // Keep multiple root tiling children together so their original root
  // ratios remain meaningful after they are inserted into the target tree.
  let tiling_group: Option<Container> = match source_tiling_children
    .as_slice()
  {
    [] => None,
    [child] => Some(child.clone().into()),
    _ => {
      let split = SplitContainer::new(
        current_workspace.tiling_direction(),
        config.value.gaps.clone(),
      );
      let current_container: Container = current_workspace.clone().into();
      wrap_in_split_container(
        &split,
        &current_container,
        &source_tiling_children,
      )?;
      Some(split.into())
    }
  };

  if let Some(tiling_group) = tiling_group {
    if has_dpi_difference {
      for window in tiling_group
        .self_and_descendants()
        .filter_map(|descendant| descendant.as_window_container().ok())
      {
        window.set_has_pending_dpi_adjustment(true);
      }
    }

    move_container_within_tree(
      &tiling_group,
      &target_workspace.clone().into(),
      target_workspace.child_count(),
      state,
    )?;
  }

  // Floating, fullscreen, and minimized windows are direct workspace
  // children. Reuse the regular move path so their placement, focus
  // fallback, and cross-monitor behavior stay consistent.
  let non_tiling_windows = current_workspace
    .children()
    .into_iter()
    .filter_map(|child| child.as_window_container().ok())
    .collect::<Vec<_>>();
  let target_name = target_workspace.config().name;
  for window in non_tiling_windows {
    move_window_to_workspace(
      window,
      WorkspaceTarget::Name(target_name.clone()),
      state,
      config,
    )?;
  }

  // The source workspace remains displayed, so keep focus there after its
  // windows have been moved rather than focusing a hidden target
  // workspace.
  set_focused_descendant(&current_workspace.clone().into(), None);
  state.pending_sync.queue_focus_change();
  state
    .pending_sync
    .queue_containers_to_redraw(target_workspace.tiling_children())
    .queue_workspace_to_reorder(target_workspace);

  Ok(())
}
