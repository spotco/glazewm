use anyhow::Context;
#[cfg(target_os = "windows")]
use wm_common::WindowEffectConfig;
use wm_common::{
  CursorJumpTrigger, DisplayState, HideCorner, HideMethod, UniqueExt,
  WindowState, WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::sample_z_order_ranks;
#[cfg(target_os = "windows")]
use wm_platform::SWP_NOZORDER;
#[cfg(target_os = "windows")]
use wm_platform::{CornerStyle, OpacityValue};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, WindowId};
use wm_platform::{Rect, WindowZOrder};

use super::verbose_z_order_enabled;
#[cfg(target_os = "windows")]
use crate::normal_window_stack::{StackAtomId, StackOperation};
use crate::{
  models::{Container, WindowContainer, Workspace},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn platform_sync(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let suspend_z_order = state.ignored_foreground_suspends_z_order();
  let focused_container = state
    .native_focus_target()
    .context("No focused container.")?;

  if state.pending_sync.needs_focus_update() && !suspend_z_order {
    sync_focus(&focused_container, state)?;
  }

  if !state.pending_sync.containers_to_redraw().is_empty()
    || !state.pending_sync.workspaces_to_reorder().is_empty()
    || state.pending_sync.needs_focus_update()
    || state.pending_sync.has_stack_activations()
  {
    redraw_containers(&focused_container, state, config, suspend_z_order)?;
  }

  if state.pending_sync.needs_cursor_jump()
    && config.value.general.cursor_jump.enabled
    && !suspend_z_order
  {
    jump_cursor(focused_container.clone(), state, config)?;
  }

  if state.pending_sync.needs_focused_effect_update()
    || state.pending_sync.needs_all_effects_update()
  {
    // Keep reference to the previous window that had focus effects
    // applied.
    let prev_effects_window = state.prev_effects_window.clone();

    if let Ok(window) = focused_container.as_window_container() {
      apply_window_effects(&window, true, config);
      state.prev_effects_window = Some(window.clone());
    } else {
      state.prev_effects_window = None;
    }

    // Get windows that should have the unfocused border applied to them.
    // For the sake of performance, we only update the border of the
    // previously focused window. If the `reset_window_effects` flag is
    // passed, the unfocused border is applied to all unfocused windows.
    let unfocused_windows =
      if state.pending_sync.needs_all_effects_update() {
        state.windows()
      } else {
        prev_effects_window.into_iter().collect()
      }
      .into_iter()
      .filter(|window| window.id() != focused_container.id());

    for window in unfocused_windows {
      apply_window_effects(&window, false, config);
    }
  }

  state.pending_sync.clear();

  Ok(())
}

fn sync_focus(
  focused_container: &Container,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let native_window = focused_container.as_window_container().ok();

  // Sets focus to the appropriate target:
  // - If the container is a window, focuses that window.
  // - If the container is a workspace, "resets" focus by focusing the
  //   desktop window.
  //
  // In either case, a `PlatformEvent::WindowFocused` event is subsequently
  // triggered.
  let result = if let Some(window) = native_window {
    if verbose_z_order_enabled() {
      tracing::info!("Setting focus to window: {window}");
      #[cfg(target_os = "windows")]
      crate::commands::general::layout_debug_log(format!(
        "verbose z-order SetForegroundWindow hwnd={:?}",
        window.native().id(),
      ));
    } else {
      tracing::debug!("Setting focus to window: {window}");
    }
    window.native().focus()
  } else {
    if verbose_z_order_enabled() {
      tracing::info!("Setting focus to the desktop window.");
    } else {
      tracing::debug!("Setting focus to the desktop window.");
    }
    state.dispatcher.reset_focus()
  };

  if let Err(err) = result {
    tracing::warn!("Failed to set focus: {}", err);
  }

  state.emit_event(WmEvent::FocusChanged {
    focused_container: focused_container.to_dto()?,
  });

  Ok(())
}

/// Finds windows that should be brought to the top of their workspace's
/// z-order.
///
/// Windows are brought to front if they match the focused window's state
/// (floating/tiling) and any of these conditions are met:
///  * Focus has changed to a different window.
///  * Focused window's state has changed (e.g. tiling -> floating).
///  * Focused window has moved to a different workspace.
fn windows_to_bring_to_front(
  focused_container: &Container,
  state: &WmState,
) -> anyhow::Result<Vec<WindowContainer>> {
  let focused_workspace =
    focused_container.workspace().context("No workspace.")?;

  // Add focused workspace if there's been a focus change.
  let workspaces_to_reorder = state
    .pending_sync
    .workspaces_to_reorder()
    .iter()
    .filter(|workspace| workspace.is_displayed())
    .chain(
      state
        .pending_sync
        .needs_focus_update()
        .then_some(&focused_workspace),
    )
    .unique_by(|workspace| workspace.id());

  // Bring forward windows that match the focused state. Only do this for
  // tiling/floating windows.
  let windows_to_bring_to_front = workspaces_to_reorder
    .flat_map(|workspace| {
      let focused_descendant = workspace
        .descendant_focus_order()
        .next()
        .and_then(|container| container.as_window_container().ok());

      match focused_descendant {
        Some(focused_descendant)
          if state
            .pending_sync
            .focused_window_to_bring_to_front(workspace)
            .is_some_and(|window_id| {
              window_id == focused_descendant.id()
                && matches!(
                  focused_descendant.state(),
                  WindowState::Floating(config) if !config.shown_on_top
                )
            }) =>
        {
          // A native focus event for a normal floating window should only
          // promote the selected window. The workspace-wide state policy
          // is intentionally bypassed for this case.
          workspace
            .descendants()
            .filter_map(|descendant| descendant.as_window_container().ok())
            .filter(|window| window.id() == focused_descendant.id())
            .collect()
        }
        Some(focused_descendant) => workspace
          .descendants()
          .filter_map(|descendant| descendant.as_window_container().ok())
          .filter(|window| {
            let is_floating_or_tiling = matches!(
              window.state(),
              WindowState::Floating(_) | WindowState::Tiling
            );

            is_floating_or_tiling
              && window.state().is_same_state(&focused_descendant.state())
          })
          .collect(),
        None => vec![],
      }
    })
    .filter(|window| !state.is_show_desktop_minimized(window.id()))
    .collect::<Vec<_>>();

  Ok(windows_to_bring_to_front)
}

#[allow(clippy::too_many_lines)]
fn redraw_containers(
  focused_container: &Container,
  state: &mut WmState,
  config: &UserConfig,
  preserve_z_order: bool,
) -> anyhow::Result<()> {
  let windows_to_redraw = state.windows_to_redraw();
  let windows_to_bring_to_front =
    windows_to_bring_to_front(focused_container, state)?;

  let windows_to_update = {
    let mut windows = windows_to_redraw
      .iter()
      .chain(&windows_to_bring_to_front)
      .unique_by(|window| window.id())
      .collect::<Vec<_>>();

    let descendant_focus_order = state
      .root_container
      .descendant_focus_order()
      .collect::<Vec<_>>();

    // Sort the windows to update by their focus order. The most recently
    // focused window will be updated first.
    // TODO: To reduce flicker, redraw windows that will be shown first,
    // then redraw the ones to be hidden last.
    windows.sort_by_key(|window| {
      descendant_focus_order
        .iter()
        .position(|order| order.id() == window.id())
    });

    windows
  };

  // Get monitors by their optimal hide corner.
  let monitors_by_hide_corner = state.monitors_by_hide_corner();

  for window in windows_to_update.iter().rev() {
    let should_bring_to_front = windows_to_bring_to_front.contains(window);

    let workspace =
      window.workspace().context("Window has no workspace.")?;

    let monitor = window.monitor().context("No monitor.")?;
    let hide_corner = monitors_by_hide_corner
      .iter()
      .find(|(m, _)| m.id() == monitor.id())
      .map(|(_, hide_corner)| hide_corner)
      .context("Monitor not found in hide corner map.")?;

    // Whether the window should be shown above all other windows.
    let z_order = match window.state() {
      WindowState::Floating(config) if config.shown_on_top => {
        WindowZOrder::TopMost
      }
      WindowState::Fullscreen(config) if config.shown_on_top => {
        WindowZOrder::TopMost
      }
      _ if should_bring_to_front => {
        let focused_descendant = workspace
          .descendant_focus_order()
          .next()
          .and_then(|container| container.as_window_container().ok());

        if let Some(focused_descendant) = focused_descendant {
          if window.id() == focused_descendant.id() {
            WindowZOrder::Normal
          } else {
            WindowZOrder::AfterWindow(focused_descendant.native().id())
          }
        } else {
          WindowZOrder::Normal
        }
      }
      _ => WindowZOrder::Normal,
    };

    // Skip updating the window's position if it only required a z-order
    // change.
    if !windows_to_redraw.contains(window) {
      continue;
    }

    // Transition display state depending on whether window will be
    // shown or hidden.
    window.set_display_state(
      match (window.display_state(), workspace.is_displayed()) {
        (DisplayState::Hidden | DisplayState::Hiding, true) => {
          DisplayState::Showing
        }
        (DisplayState::Shown | DisplayState::Showing, false) => {
          DisplayState::Hiding
        }
        _ => window.display_state(),
      },
    );

    let is_visible = matches!(
      window.display_state(),
      DisplayState::Showing | DisplayState::Shown
    );

    let is_show_desktop_minimized =
      state.is_show_desktop_minimized(window.id());

    // `SetWindowPos` failure (maximized fullscreen) returns before
    // `set_cloaked`. Remember that so the caller can cloak before any
    // taskbar `DeleteTab`.
    let repositioned = reposition_window(
      window,
      *hide_corner,
      &z_order,
      is_visible,
      is_show_desktop_minimized,
      preserve_z_order,
      config,
    );
    if let Err(err) = &repositioned {
      tracing::warn!("Failed to set window position: {}", err);
    }

    // Whether the window is either transitioning to or from fullscreen.
    // TODO: This check can be improved since `prev_state` can be
    // fullscreen without it needing to be marked as not fullscreen.
    #[cfg(target_os = "windows")]
    {
      let is_transitioning_fullscreen =
        match (window.prev_state(), window.state()) {
          (Some(_), WindowState::Fullscreen(s)) if !s.maximized => true,
          (Some(WindowState::Fullscreen(_)), _) => true,
          _ => false,
        };

      if is_transitioning_fullscreen {
        if let Err(err) = window.native().mark_fullscreen(matches!(
          window.state(),
          WindowState::Fullscreen(_)
        )) {
          tracing::warn!("Failed to mark window as fullscreen: {}", err);
        }
      }
    }

    // Skip setting taskbar visibility if the window is hidden (has no
    // effect). Since cloaked windows are normally always visible in the
    // taskbar, we only need to set visibility if `show_all_in_taskbar` is
    // `false`.
    //
    // A failed reposition returns before `set_cloaked`. Cloak here before
    // any taskbar change. Hiding must not `DeleteTab` unless that cloak
    // has begun, or the window stays `Hiding` and drops out of Alt-Tab.
    #[cfg(target_os = "windows")]
    {
      let hide_with_cloak =
        config.value.general.hide_method == HideMethod::Cloak;
      let mut cloak_begun = hide_with_cloak && repositioned.is_ok();
      if hide_with_cloak && repositioned.is_err() {
        match window.native().set_cloaked(!is_visible) {
          Ok(()) => cloak_begun = true,
          Err(err) => {
            tracing::warn!(
              "Failed to cloak window after reposition: {}",
              err
            );
          }
        }
      }

      if hide_with_cloak
        && !config.value.general.show_all_in_taskbar
        && matches!(
          window.display_state(),
          DisplayState::Showing | DisplayState::Hiding
        )
        && (is_visible || cloak_begun)
      {
        if let Err(err) =
          window.native().set_taskbar_visibility(is_visible)
        {
          tracing::warn!("Failed to set taskbar visibility: {}", err);
        }
      }
    }
  }

  #[cfg(target_os = "windows")]
  if !preserve_z_order {
    reorder_focused_workspace_layers(focused_container, state);
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn log_verbose_native_z_order(label: &str, intended: &[WindowId]) {
  if !verbose_z_order_enabled() || intended.is_empty() {
    return;
  }

  let mut ranks = sample_z_order_ranks(intended);
  ranks.sort_by_key(|(_, rank)| *rank);
  let native_order: Vec<WindowId> =
    ranks.iter().map(|(window_id, _)| *window_id).collect();
  let native_top_to_bottom = ranks
    .iter()
    .map(|(window_id, rank)| format!("{window_id:?}@{rank}"))
    .collect::<Vec<_>>()
    .join(", ");
  let rank_by_id: std::collections::HashMap<WindowId, u32> =
    ranks.iter().copied().collect();
  let intended_ranks = intended
    .iter()
    .map(|window_id| {
      rank_by_id.get(window_id).map_or_else(
        || format!("{window_id:?}=?"),
        |rank| format!("{window_id:?}@{rank}"),
      )
    })
    .collect::<Vec<_>>()
    .join(", ");
  let sorted_ok = native_order == intended;

  crate::commands::general::layout_debug_log(format!(
    "verbose z-order {label}: match_intended={sorted_ok} native_top_to_bottom=[{native_top_to_bottom}] intended_ranks=[{intended_ranks}]"
  ));
}

#[cfg(target_os = "windows")]
fn schedule_verbose_z_order_recheck(
  label: String,
  intended: Vec<WindowId>,
) {
  if !verbose_z_order_enabled() || intended.is_empty() {
    return;
  }
  if let Ok(runtime) = tokio::runtime::Handle::try_current() {
    runtime.spawn(async move {
      tokio::time::sleep(std::time::Duration::from_millis(25)).await;
      log_verbose_native_z_order(&format!("{label}+25ms"), &intended);
    });
  }
}

/// Restores the normal floating/tiling layers after a focus or state
/// change.
///
/// Normal detached windows are individual z-order items. Tiled windows are
/// treated as one z-order group. Focusing a detached window promotes that
/// window and its managed Win32 owner group (owned above owner); unrelated
/// detached peers keep their position relative to the tiled block.
/// Focusing a tiled window promotes the complete tiled group.
///
/// When a focused tiled window is detached, the newly detached window is
/// promoted while the remaining tiled group keeps its position relative to
/// the other detached windows.
#[cfg(target_os = "windows")]
fn reorder_focused_workspace_layers(
  focused_container: &Container,
  state: &mut WmState,
) {
  let mut workspaces = Vec::new();
  let mut seen = std::collections::HashSet::new();
  for workspace in state
    .pending_sync
    .workspaces_to_reorder()
    .iter()
    .filter(|workspace| workspace.is_displayed())
    .cloned()
    .chain(
      focused_container
        .workspace()
        .filter(Workspace::is_displayed),
    )
  {
    if seen.insert(workspace.id()) {
      workspaces.push(workspace);
    }
  }

  for workspace in workspaces {
    reconcile_workspace_stack(&workspace, state, None);
  }
}

/// Records an ignored window as the top of the normal z-order.
///
/// Promotes that window and its managed Win32 owner group (when any).
/// The tiled group stays one block, and unrelated detached or ignored
/// peers keep their position relative to the block. Applied immediately
/// so a later managed focus cannot replay the pre-Alt-Tab chain.
#[cfg(target_os = "windows")]
pub fn promote_ignored_window(
  state: &mut WmState,
  native_window: &wm_platform::NativeWindow,
) {
  let Some(workspace) = state
    .nearest_monitor(native_window)
    .and_then(|monitor| monitor.displayed_workspace())
  else {
    crate::commands::general::layout_debug_log(format!(
      "ignored promote skipped; no workspace for {:?}",
      native_window.id()
    ));
    return;
  };

  reconcile_workspace_stack(&workspace, state, Some(native_window.id()));
}

/// Reconciles one displayed workspace from its logical atom stack.
///
/// `forced_activation` is used only for an observed ignored-window
/// activation. Managed activations are recorded as explicit stack intent
/// in `PendingSync`; an ordinary focus/layout recovery only reconciles.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_lines)] // Builds one workspace snapshot and
                                 // records its native reconciliation.
fn reconcile_workspace_stack(
  workspace: &Workspace,
  state: &mut WmState,
  forced_activation: Option<WindowId>,
) {
  if !workspace.is_displayed() {
    return;
  }

  let mut tiled_ids = Vec::new();
  let mut detached_ids = Vec::new();
  let mut layers = Vec::new();
  for window in workspace
    .descendant_focus_order()
    .filter_map(|container| container.as_window_container().ok())
    .filter(|window| {
      matches!(
        window.display_state(),
        DisplayState::Showing | DisplayState::Shown
      ) && !state.is_show_desktop_minimized(window.id())
        && window.native().identity_is_current()
    })
  {
    match window.state() {
      WindowState::Tiling => {
        tiled_ids.push(window.native().id());
        layers.push((window.native().id(), NormalZOrderLayer::Tiling));
      }
      WindowState::Floating(config) if !config.shown_on_top => {
        detached_ids.push(window.native().id());
        layers.push((window.native().id(), NormalZOrderLayer::Floating));
      }
      _ => {}
    }
  }

  let workspace_monitor_id =
    workspace.monitor().map(|monitor| monitor.id());
  let ignored = state
    .ignored_windows
    .iter()
    .filter(|window| {
      window.is_valid()
        && window.identity_is_current()
        && window.is_visible().unwrap_or(false)
        && window.debug_info().is_topmost != Some(true)
        && state.nearest_monitor(window).is_some_and(|monitor| {
          workspace_monitor_id == Some(monitor.id())
        })
    })
    .map(wm_platform::NativeWindow::id)
    .collect::<Vec<_>>();
  detached_ids.extend(ignored.iter().copied());
  layers.extend(
    ignored
      .iter()
      .copied()
      .map(|window_id| (window_id, NormalZOrderLayer::Ignored)),
  );

  let detached_set: std::collections::HashSet<WindowId> =
    detached_ids.iter().copied().collect();
  let stack_window_set: std::collections::HashSet<WindowId> =
    tiled_ids.iter().chain(&detached_ids).copied().collect();
  let mut owner_by_window = std::collections::HashMap::new();
  for window in workspace
    .descendant_focus_order()
    .filter_map(|container| container.as_window_container().ok())
  {
    let child = window.native().id();
    if !detached_set.contains(&child) {
      continue;
    }
    if let Some(owner) = window.native().debug_info().owner_handle {
      let owner = WindowId(owner);
      if stack_window_set.contains(&owner) {
        owner_by_window.insert(child, owner);
      }
    }
  }
  for window in &state.ignored_windows {
    let child = window.id();
    if !detached_set.contains(&child) {
      continue;
    }
    if let Some(owner) = window.debug_info().owner_handle {
      let owner = WindowId(owner);
      if stack_window_set.contains(&owner) {
        owner_by_window.insert(child, owner);
      }
    }
  }

  let all_ids = tiled_ids
    .iter()
    .chain(&detached_ids)
    .copied()
    .collect::<Vec<_>>();
  let mut observed = sample_z_order_ranks(&all_ids);
  observed.sort_by_key(|(_, rank)| *rank);
  let native_order = observed
    .iter()
    .map(|(window_id, _)| *window_id)
    .collect::<Vec<_>>();
  let previous_atoms = state
    .normal_stack_for_workspace(workspace.id())
    .map_or_else(Vec::new, |stack| stack.atoms().to_vec());

  state.apply_normal_stack_operation(
    workspace.id(),
    StackOperation::Reconcile {
      tiled_window_ids: tiled_ids.clone(),
      detached_window_ids: detached_ids.clone(),
      owner_by_window: owner_by_window.clone(),
      native_order: native_order.clone(),
    },
  );

  let queued_activation = state
    .pending_sync
    .stack_activation(workspace)
    .and_then(|activation| {
      workspace
        .descendant_focus_order()
        .filter_map(|container| container.as_window_container().ok())
        .find(|window| window.id() == activation.window_id)
        .map(|window| window.native().id())
    });
  let activation_reason = if forced_activation.is_some() {
    Some("ignored_native_activation".to_string())
  } else {
    state
      .pending_sync
      .stack_activation(workspace)
      .map(|activation| format!("{:?}", activation.reason))
  };
  let activation = forced_activation.or(queued_activation);
  if let Some(window_id) = activation {
    let atom = if tiled_ids.contains(&window_id) {
      Some(StackAtomId::TiledGroup)
    } else if detached_ids.contains(&window_id) {
      Some(StackAtomId::DetachedWindow(window_id))
    } else {
      None
    };
    if let Some(atom) = atom {
      state.apply_normal_stack_operation(
        workspace.id(),
        StackOperation::Activate {
          atom,
          owner_by_window: owner_by_window.clone(),
        },
      );
    }
  }

  let Some(stack) = state.normal_stack_for_workspace(workspace.id())
  else {
    return;
  };
  let native_window_ids = stack.materialize(&tiled_ids);
  let atom_order = stack.atoms().to_vec();
  let promoted_atom_ids = activation.map_or_else(Vec::new, |window_id| {
    stack.activation_group(window_id, &tiled_ids)
  });
  let focused_window_id = activation.or_else(|| {
    workspace
      .descendant_focus_order()
      .next()
      .and_then(|container| container.as_window_container().ok())
      .map(|window| window.native().id())
  });
  let focused_layer = focused_window_id.and_then(|focused| {
    layers
      .iter()
      .find(|(window_id, _)| *window_id == focused)
      .map(|(_, layer)| *layer)
  });
  let request = if activation.is_some() {
    wm_platform::ZOrderIntent::Promote
  } else {
    wm_platform::ZOrderIntent::Preserve
  };
  let mut pre_ranks = sample_z_order_ranks(&native_window_ids);
  pre_ranks.sort_by_key(|(_, rank)| *rank);
  if verbose_z_order_enabled() {
    crate::commands::general::layout_debug_log(format!(
      "normal z reconcile workspace={:?} intent={:?} activation={:?} atoms_before={:?} atoms_after={:?} native_before={:?} desired={:?}",
      workspace.id(),
      request,
      activation,
      previous_atoms,
      atom_order,
      pre_ranks,
      native_window_ids,
    ));
  }

  let (
    reorder_error,
    native_writes,
    skipped_native_windows,
    retry_scheduled,
    reconcile_status,
  ) = match wm_platform::reconcile_z_order_for_workspace(
    workspace.id().as_u128(),
    &native_window_ids,
    request,
    &promoted_atom_ids,
  ) {
    Ok(outcome) => (
      None,
      outcome.native_writes,
      outcome.skipped_windows,
      outcome.retry_scheduled,
      if outcome.superseded {
        "superseded"
      } else if outcome.native_writes == 0 && outcome.skipped_windows == 0
      {
        "already_satisfied"
      } else if outcome.skipped_windows > 0 {
        "pending_skipped_windows"
      } else if outcome.retry_scheduled {
        "pending_async_convergence"
      } else {
        "requests_issued_without_retry_worker"
      },
    ),
    Err(err) => {
      tracing::warn!("Failed to reconcile normal-window z-order: {err}");
      (
        Some(crate::diagnostic_history::truncate_error(&err.to_string())),
        0,
        0,
        false,
        "native_reconcile_error",
      )
    }
  };
  let mut owner_constraints = owner_by_window
    .iter()
    .map(|(child, owner)| {
      crate::diagnostic_history::NativeOwnerConstraint {
        child_hwnd: crate::diagnostic_history::hwnd_i64(child.0),
        owner_hwnd: crate::diagnostic_history::hwnd_i64(owner.0),
      }
    })
    .collect::<Vec<_>>();
  owner_constraints.sort_by_key(|constraint| constraint.child_hwnd);
  let mut native_ranks = sample_z_order_ranks(&native_window_ids)
    .into_iter()
    .map(|(window_id, z_order_index)| {
      crate::diagnostic_history::NativeZRank {
        hwnd: crate::diagnostic_history::hwnd_i64(window_id.0),
        z_order_index,
      }
    })
    .collect::<Vec<_>>();
  native_ranks.sort_by_key(|rank| rank.z_order_index);
  state.push_diagnostic(
    crate::diagnostic_history::DiagnosticBody::ZOrderReconcile {
      workspace_id: workspace.id(),
      workspace_name: workspace.config().name,
      focused_hwnd: focused_window_id.map_or(0, |window_id| {
        crate::diagnostic_history::hwnd_i64(window_id.0)
      }),
      focused_layer: focused_layer.map(NormalZOrderLayer::as_str),
      stack_intent: activation_reason
        .clone()
        .unwrap_or_else(|| "preserve_reconcile".into()),
      stack_atoms_before: previous_atoms
        .iter()
        .map(|atom| format!("{atom:?}"))
        .collect(),
      stack_atoms_after: atom_order
        .iter()
        .map(|atom| format!("{atom:?}"))
        .collect(),
      owner_constraints,
      native_order_before: pre_ranks
        .iter()
        .map(|(window_id, z_order_index)| {
          crate::diagnostic_history::NativeZRank {
            hwnd: crate::diagnostic_history::hwnd_i64(window_id.0),
            z_order_index: *z_order_index,
          }
        })
        .collect(),
      intended_top_to_bottom: native_window_ids
        .iter()
        .map(|window_id| crate::diagnostic_history::hwnd_i64(window_id.0))
        .collect(),
      native_ranks,
      native_writes,
      skipped_native_windows,
      retry_scheduled,
      reconcile_status,
      native_sample_timing: "immediate_after_reconcile_request",
      reorder_error,
    },
  );
  if verbose_z_order_enabled() {
    log_verbose_native_z_order("post-reconcile", &native_window_ids);
    schedule_verbose_z_order_recheck(
      "post-reconcile".into(),
      native_window_ids,
    );
  }
}

#[cfg(not(target_os = "windows"))]
pub fn promote_ignored_window(
  _state: &mut WmState,
  _native_window: &wm_platform::NativeWindow,
) {
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NormalZOrderLayer {
  Tiling,
  Floating,
  /// Ignored windows such as Snipping Tool remain independent detached
  /// atoms and are promoted only after an observed explicit activation.
  Ignored,
}

impl NormalZOrderLayer {
  const fn as_str(self) -> &'static str {
    match self {
      Self::Tiling => "tiling",
      Self::Floating => "floating",
      Self::Ignored => "ignored",
    }
  }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use super::*;

  #[test]
  #[allow(clippy::too_many_lines)]
  fn workspace_hide_taskbar_delete_requires_prior_cloak() {
    use std::sync::Mutex;

    use tokio::sync::mpsc;
    use wm_common::{
      FullscreenStateConfig, GapsConfig, TilingDirection, WindowState,
      WorkspaceConfig,
    };
    use wm_platform::{
      EventLoop, NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
    };

    use crate::{
      commands::{container::attach_container, monitor::add_monitor},
      models::{
        NativeMonitorProperties, NativeWindowProperties, NonTilingWindow,
        TilingWindow, Workspace,
      },
      traits::WindowGetters,
    };

    static NATIVE_OPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record_native_op(line: &str) {
      if let Ok(mut lines) = NATIVE_OPS.lock() {
        lines.push(line.to_string());
      }
    }

    // Invalid HWND: SetWindowPos fails inside reposition_window before
    // set_cloaked. Same early return as a fullscreen maximize /
    // SetWindowPos error. Live windows are refused below.
    const GAME_HWND: isize = 0x0EE0_EE01;
    const ANCHOR_HWND: isize = 0x0EE0_EE02;

    let game_native = NativeWindow::from_handle(GAME_HWND);
    let anchor_native = NativeWindow::from_handle(ANCHOR_HWND);
    assert!(
      !game_native.is_valid() && !anchor_native.is_valid(),
      "refusing to run hide sync against a live HWND"
    );

    let (_event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let mut state =
      crate::wm_state::WmState::new(dispatcher, event_tx, exit_tx);

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("displays")
      .into_iter()
      .next()
      .expect("display");
    let monitor_properties =
      NativeMonitorProperties::try_from(&display).expect("monitor");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach monitor");

    let visible_workspace = Workspace::new(
      WorkspaceConfig {
        name: "visible-ws".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: true,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let hidden_workspace = Workspace::new(
      WorkspaceConfig {
        name: "hidden-ws".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: true,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    attach_container(
      &visible_workspace.clone().into(),
      &monitor.clone().into(),
      None,
    )
    .expect("attach visible workspace");
    attach_container(
      &hidden_workspace.clone().into(),
      &monitor.into(),
      None,
    )
    .expect("attach hidden workspace");

    let properties = |title: &str| NativeWindowProperties {
      title: title.into(),
      class_name: "test".into(),
      process_name: "test".into(),
      process_path: None,
      frame: Rect::from_xy(0, 0, 100, 100),
      is_minimized: false,
      is_maximized: true,
      is_resizable: true,
      shadow_borders: RectDelta::zero(),
    };

    let anchor = TilingWindow::new(
      None,
      anchor_native,
      properties("anchor"),
      None,
      RectDelta::zero(),
      Rect::from_xy(0, 0, 100, 100),
      false,
      GapsConfig::default(),
      Vec::new(),
      None,
    );
    let game = NonTilingWindow::new(
      None,
      game_native,
      properties("SoC"),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        shown_on_top: false,
      }),
      Some(WindowState::Tiling),
      RectDelta::zero(),
      None,
      Rect::from_xy(0, 0, 1920, 1080),
      false,
      Vec::new(),
      None,
    );
    attach_container(
      &anchor.clone().into(),
      &visible_workspace.clone().into(),
      None,
    )
    .expect("attach anchor");
    attach_container(
      &game.clone().into(),
      &hidden_workspace.clone().into(),
      None,
    )
    .expect("attach fullscreen window");
    crate::commands::container::set_focused_descendant(
      &anchor.into(),
      None,
    );

    assert!(visible_workspace.is_displayed());
    assert!(
      !hidden_workspace.is_displayed(),
      "fullscreen window must sit on a hidden workspace"
    );
    assert_eq!(game.display_state(), wm_common::DisplayState::Shown);

    state.pending_sync.clear();
    state.pending_sync.queue_container_to_redraw(game.clone());

    let config_path = std::env::temp_dir().join(format!(
      "glazewm-hide-cloak-order-{}.yaml",
      uuid::Uuid::new_v4()
    ));
    let config = crate::user_config::UserConfig::new(Some(config_path))
      .expect("config");
    assert_eq!(
      config.value.general.hide_method,
      wm_common::HideMethod::Cloak
    );
    assert!(!config.value.general.show_all_in_taskbar);

    NATIVE_OPS.lock().expect("ops").clear();
    wm_platform::set_native_op_logger(record_native_op);

    platform_sync(&mut state, &config).expect("sync");

    assert_eq!(
      game.display_state(),
      wm_common::DisplayState::Hiding,
      "a failed reposition must leave the hide transition in place"
    );

    #[allow(clippy::cast_sign_loss)]
    let hwnd_token = format!("hwnd={:#x}", GAME_HWND as usize);
    let lines = NATIVE_OPS.lock().expect("ops").clone();
    let began = |op: &str| {
      lines.iter().position(|line| {
        line.contains("native-op begin")
          && line.contains(op)
          && line.contains(&hwnd_token)
      })
    };
    let cloak_at = began("op=set_cloaked");
    let taskbar_at = began("op=set_taskbar_visibility");

    // Contract: if taskbar hide runs, cloak must already have begun.
    // Current code returns from reposition_window on SetWindowPos
    // failure, then the caller still calls DeleteTab.
    let taskbar_before_cloak = match (cloak_at, taskbar_at) {
      (None, Some(_)) => true,
      (Some(cloak), Some(taskbar)) => cloak > taskbar,
      _ => false,
    };
    assert!(
      !taskbar_before_cloak,
      "taskbar hide ran before cloak for {hwnd_token}; window stays \
       Hiding and drops out of Alt-Tab: {lines:?}"
    );
  }
}

#[allow(clippy::too_many_lines)] // The display transition and Win32
                                 // geometry path stay together.
fn reposition_window(
  window: &WindowContainer,
  hide_corner: HideCorner,
  // LINT: `z_order` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  z_order: &WindowZOrder,
  is_visible: bool,
  is_show_desktop_minimized: bool,
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  preserve_z_order: bool,
  config: &UserConfig,
) -> anyhow::Result<()> {
  #[cfg(target_os = "windows")]
  if !window.native().identity_is_current() {
    return Ok(());
  }

  // Show Desktop minimizes windows natively without changing their WM tree
  // state. A redraw must not restore or reposition those windows, or the
  // next focus/layout sync would undo Show Desktop.
  if is_show_desktop_minimized {
    return Ok(());
  }

  let rect = window
    .to_rect()?
    .apply_delta(&window.total_border_delta()?, None);

  // For `HideMethod::PlaceInCorner`, we need to reposition hidden windows
  // to the corner of the monitor.
  if config.value.general.hide_method == HideMethod::PlaceInCorner
    && !is_visible
  {
    const VISIBLE_SLIVER: i32 = 1;

    let monitor_rect = window
      .monitor()
      .context("No monitor.")?
      .native_properties()
      .working_area;

    let frame = window.native_properties().frame;

    let position_y = monitor_rect.bottom - VISIBLE_SLIVER;
    let position_x = match hide_corner {
      HideCorner::BottomLeft => {
        monitor_rect.left + VISIBLE_SLIVER - frame.width()
      }
      HideCorner::BottomRight => monitor_rect.right - VISIBLE_SLIVER,
    };

    // Even though the window size is unchanged, `NativeWindow::set_frame`
    // is used instead of `NativeWindow::reposition` because the latter
    // resulted in occasional incorrect positionings on macOS.
    window.native().set_frame(&Rect::from_xy(
      position_x,
      position_y,
      frame.width(),
      frame.height(),
    ))?;

    return Ok(());
  }

  if window.active_drag().is_some() {
    window.native().resize(rect.width(), rect.height())?;
  } else {
    #[cfg(target_os = "macos")]
    window.native().set_frame(&rect)?;

    #[cfg(target_os = "windows")]
    {
      use wm_platform::{
        SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOCOPYBITS, SWP_NOSENDCHANGING, WS_MAXIMIZEBOX,
      };

      // Restore window if it's minimized/maximized and shouldn't be. This
      // is needed to be able to move and resize it.
      let should_restore = match &window.state() {
        // Need to restore window if transitioning from maximized
        // fullscreen to non-maximized fullscreen.
        WindowState::Fullscreen(fullscreen) => {
          !fullscreen.maximized && window.native().is_maximized()?
        }
        // No need to restore window if it'll be minimized. Transitioning
        // from maximized to minimized works without having to
        // restore.
        WindowState::Minimized => false,
        _ => {
          window.native().is_minimized()?
            || window.native().is_maximized()?
        }
      };

      if should_restore {
        // Restoring to position has the same effect as `ShowWindow` with
        // `SW_RESTORE`, but doesn't cause a flicker.
        window.native().restore(Some(&rect))?;
      }

      let mut swp_flags = SWP_NOACTIVATE
        | SWP_NOCOPYBITS
        | SWP_NOSENDCHANGING
        | SWP_ASYNCWINDOWPOS;
      let needs_z_order_change = matches!(z_order, WindowZOrder::TopMost)
        || (window.native().debug_info().is_topmost == Some(true)
          && !preserve_z_order);
      if preserve_z_order || !needs_z_order_change {
        swp_flags |= SWP_NOZORDER;
      }

      match &window.state() {
        WindowState::Minimized => {
          if !window.native().is_minimized()? {
            window.native().minimize()?;
          }
        }
        WindowState::Fullscreen(fullscreen)
          if fullscreen.maximized
            && window.native().has_window_style(WS_MAXIMIZEBOX) =>
        {
          if !window.native().is_maximized()? {
            window.native().maximize()?;
          }

          window.native().set_window_pos(z_order, &rect, swp_flags)?;
        }
        _ => {
          swp_flags |= SWP_FRAMECHANGED;

          window.native().set_window_pos(z_order, &rect, swp_flags)?;

          // When there's a mismatch between the DPI of the monitor and the
          // window, the window might be sized incorrectly after the first
          // move. If we set the position twice, inconsistencies after the
          // first move are resolved.
          if window.has_pending_dpi_adjustment() {
            window.native().set_window_pos(z_order, &rect, swp_flags)?;
          }
        }
      }

      // Set visibility based on the hide method.
      if config.value.general.hide_method == HideMethod::Cloak {
        window.native().set_cloaked(!is_visible)?;
      } else if is_visible {
        window.native().show()?;
      } else {
        window.native().hide()?;
      }
    }
  }

  Ok(())
}

fn jump_cursor(
  focused_container: Container,
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let cursor_jump = &config.value.general.cursor_jump;

  let jump_target = match cursor_jump.trigger {
    CursorJumpTrigger::WindowFocus => Some(focused_container),
    CursorJumpTrigger::MonitorFocus => {
      let target_monitor =
        focused_container.monitor().context("No monitor.")?;

      let cursor_monitor = state
        .dispatcher
        .cursor_position()
        .ok()
        .and_then(|pos| state.monitor_at_point(&pos));

      // Jump to the target monitor if the cursor is not already on it.
      cursor_monitor
        .filter(|monitor| monitor.id() != target_monitor.id())
        .map(|_| target_monitor.into())
    }
  };

  if let Some(jump_target) = jump_target {
    let center = jump_target.to_rect()?.center_point();

    if let Err(err) = state.dispatcher.set_cursor_position(&center) {
      tracing::warn!("Failed to set cursor position: {}", err);
    }
  }

  Ok(())
}

fn apply_window_effects(
  // LINT: `window` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  window: &WindowContainer,
  is_focused: bool,
  config: &UserConfig,
) {
  let window_effects = &config.value.window_effects;

  // LINT: `effect_config` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  let effect_config = if is_focused {
    &window_effects.focused_window
  } else {
    &window_effects.other_windows
  };

  // Skip if both focused + non-focused window effects are disabled.
  #[cfg(target_os = "windows")]
  if window_effects.focused_window.border.enabled
    || window_effects.other_windows.border.enabled
  {
    apply_border_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.hide_title_bar.enabled
    || window_effects.other_windows.hide_title_bar.enabled
  {
    apply_hide_title_bar_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.corner_style.enabled
    || window_effects.other_windows.corner_style.enabled
  {
    apply_corner_effect(window, effect_config);
  }

  #[cfg(target_os = "windows")]
  if window_effects.focused_window.transparency.enabled
    || window_effects.other_windows.transparency.enabled
  {
    apply_transparency_effect(window, effect_config);
  }
}

#[cfg(target_os = "windows")]
fn apply_border_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  let border_color = if effect_config.border.enabled {
    Some(&effect_config.border.color)
  } else {
    None
  };

  _ = window.native().set_border_color(border_color);

  let native = window.native().clone();
  let border_color = border_color.cloned();

  // Re-apply border color after a short delay to better handle
  // windows that change it themselves.
  tokio::task::spawn(async move {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    _ = native.set_border_color(border_color.as_ref());
  });
}

#[cfg(target_os = "windows")]
fn apply_hide_title_bar_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  _ = window
    .native()
    .set_title_bar_visibility(!effect_config.hide_title_bar.enabled);
}

#[cfg(target_os = "windows")]
fn apply_corner_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  let corner_style = if effect_config.corner_style.enabled {
    &effect_config.corner_style.style
  } else {
    &CornerStyle::Default
  };

  _ = window.native().set_corner_style(corner_style);
}

#[cfg(target_os = "windows")]
fn apply_transparency_effect(
  window: &WindowContainer,
  effect_config: &WindowEffectConfig,
) {
  let transparency = if effect_config.transparency.enabled {
    &effect_config.transparency.opacity
  } else {
    // Reset the transparency to default.
    &OpacityValue::from_alpha(u8::MAX)
  };

  _ = window.native().set_transparency(transparency);
}
