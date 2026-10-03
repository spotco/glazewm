use anyhow::Context;
#[cfg(target_os = "windows")]
use wm_common::WindowEffectConfig;
use wm_common::{
  CursorJumpTrigger, DisplayState, HideCorner, HideMethod, UniqueExt,
  WindowState, WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::begin_z_order_batch;
#[cfg(target_os = "windows")]
use wm_platform::reorder_z_order;
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

/// Whether per-window `bring_to_front` `set_z_order` must defer to the
/// workspace layer chain apply.
///
/// Tiling focus, targeted floating focus, and any focus that already
/// queued `workspace_to_reorder` (including Super+arrow) rely on
/// `reorder_focused_workspace_layers`. Skipping the legacy `AfterWindow`
/// swarm prevents floaters from flashing between tiles / peer floaters
/// under `SWP_ASYNCWINDOWPOS` (layout.log 2026-10-02 ~00:33 ET; floating
/// Super+arrow dual-applicator race).
#[cfg(target_os = "windows")]
fn should_defer_bring_to_front_to_workspace_reorder(
  has_targeted_floating_focus: bool,
  focused_is_tiling: bool,
  workspace_reorder_queued: bool,
) -> bool {
  has_targeted_floating_focus
    || focused_is_tiling
    || workspace_reorder_queued
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
  #[cfg(target_os = "windows")]
  let _ = begin_z_order_batch();

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

    // Set the z-order of the window.
    //
    // NOTE: macOS doesn't have a robust public API for setting the z-order
    // of a window. See `NativeWindow::raise` for more details.
    //
    // On Windows, skip per-window bring_to_front `set_z_order` when a
    // workspace layer reorder will own the outcome. `AfterWindow(focused)`
    // under `SWP_ASYNCWINDOWPOS` races `SetForegroundWindow` and
    // `reorder_z_order`, leaving detached windows between tiles (confirmed
    // layout.log 2026-10-02 ~00:33 ET). Targeted floating focus already
    // skipped this path; tiling group promotion must skip too. Geometry
    // redraws still pass `z_order` through `reposition_window`.
    #[cfg(target_os = "windows")]
    if !preserve_z_order
      && should_bring_to_front
      && !windows_to_redraw.contains(window)
    {
      let has_targeted_floating_focus = state
        .pending_sync
        .focused_window_to_bring_to_front(&workspace)
        .is_some_and(|window_id| window_id == window.id());

      let focused_is_tiling = workspace
        .descendant_focus_order()
        .next()
        .and_then(|container| container.as_window_container().ok())
        .is_some_and(|focused| {
          matches!(focused.state(), WindowState::Tiling)
        });

      let workspace_reorder_queued = state
        .pending_sync
        .workspaces_to_reorder()
        .iter()
        .any(|queued| queued.id() == workspace.id())
        || state.pending_sync.needs_focus_update();

      // Defer to `reorder_focused_workspace_layers` whenever that chain
      // owns the outcome (tiling group, targeted detached, or any
      // Super+arrow / queued workspace reorder). Avoid dual applicators.
      let defer_to_workspace_reorder =
        should_defer_bring_to_front_to_workspace_reorder(
          has_targeted_floating_focus,
          focused_is_tiling,
          workspace_reorder_queued,
        );

      if defer_to_workspace_reorder {
        if verbose_z_order_enabled() {
          let reason = if has_targeted_floating_focus {
            "targeted_floating"
          } else if focused_is_tiling {
            "tiling_group_defer_workspace_reorder"
          } else {
            "workspace_reorder_queued"
          };
          crate::commands::general::layout_debug_log(format!(
            "verbose z-order skip bring_to_front hwnd={:?} z_order={z_order:?} reason={reason}",
            window.native().id(),
          ));
        }
      } else {
        if verbose_z_order_enabled() {
          tracing::info!("Updating window z-order: {window}");
          crate::commands::general::layout_debug_log(format!(
            "verbose z-order bring_to_front hwnd={:?} z_order={z_order:?}",
            window.native().id(),
          ));
        } else {
          tracing::debug!("Updating window z-order: {window}");
        }

        if let Err(err) = window.native().set_z_order(&z_order) {
          tracing::warn!("Failed to set window z-order: {}", err);
        }
      }
    }

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
  let Some(workspace) = focused_container.workspace() else {
    return;
  };

  if !workspace.is_displayed() {
    return;
  }

  let Some(focused_window_id) = workspace
    .descendant_focus_order()
    .next()
    .and_then(|container| container.as_window_container().ok())
    .map(|window| window.native().id())
  else {
    return;
  };

  reorder_focused_workspace_layers_in_workspace(
    &workspace,
    focused_window_id,
    state,
  );
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

  reorder_focused_workspace_layers_in_workspace(
    &workspace,
    native_window.id(),
    state,
  );
}

#[cfg(not(target_os = "windows"))]
pub fn promote_ignored_window(
  _state: &mut WmState,
  _native_window: &wm_platform::NativeWindow,
) {
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_lines)]
fn reorder_focused_workspace_layers_in_workspace(
  workspace: &Workspace,
  focused_window_id: WindowId,
  state: &mut WmState,
) {
  let mut normal_windows = workspace
    .descendant_focus_order()
    .filter_map(|container| container.as_window_container().ok())
    .filter(|window| {
      matches!(
        window.display_state(),
        DisplayState::Showing | DisplayState::Shown
      )
    })
    .filter(|window| !state.is_show_desktop_minimized(window.id()))
    .filter_map(|window| {
      let layer = match window.state() {
        WindowState::Tiling => NormalZOrderLayer::Tiling,
        WindowState::Floating(config) if !config.shown_on_top => {
          NormalZOrderLayer::Floating
        }
        _ => return None,
      };

      Some((window.native().id(), layer))
    })
    .collect::<Vec<_>>();

  let workspace_monitor_id =
    workspace.monitor().map(|monitor| monitor.id());
  let ignored_window_ids = state
    .ignored_windows
    .iter()
    .filter(|window| {
      window.is_valid()
        && window.is_visible().unwrap_or(false)
        && state.nearest_monitor(window).is_some_and(|monitor| {
          workspace_monitor_id == Some(monitor.id())
        })
    })
    .map(wm_platform::NativeWindow::id)
    .collect::<Vec<_>>();

  // Ignored windows are native detached windows from the z-order manager's
  // perspective. Their native focus events are intentionally no-ops, so
  // they only enter this chain when a managed window owns focus. They
  // share the Floating layer with detached managed windows and therefore
  // retain the same relative-order rules.
  normal_windows.extend(
    ignored_window_ids
      .iter()
      .copied()
      .map(|window_id| (window_id, NormalZOrderLayer::Ignored)),
  );
  let focused_layer = normal_windows
    .iter()
    .find(|(window_id, _)| *window_id == focused_window_id)
    .map(|(_, layer)| *layer);
  let previous_order = state
    .normal_z_order_for_workspace(workspace.id())
    .map(<[WindowId]>::to_vec);

  // Owner links among Floating/Ignored windows in this chain. Focusing an
  // owned caption popup must also raise its managed GW_OWNER (and owned
  // siblings) above the tiled block. Tiling owners are omitted so the
  // tiled block stays contiguous without reshuffling unrelated floaters.
  let chain_ids: std::collections::HashSet<WindowId> =
    normal_windows.iter().map(|(id, _)| *id).collect();
  let layer_of = |id: WindowId| -> Option<NormalZOrderLayer> {
    normal_windows
      .iter()
      .find(|(window_id, _)| *window_id == id)
      .map(|(_, layer)| *layer)
  };
  let mut owner_by_window: std::collections::HashMap<WindowId, WindowId> =
    std::collections::HashMap::new();

  for window in workspace
    .descendant_focus_order()
    .filter_map(|container| container.as_window_container().ok())
  {
    let window_id = window.native().id();
    if !chain_ids.contains(&window_id) {
      continue;
    }
    let Some(owner_handle) = window.native().debug_info().owner_handle
    else {
      continue;
    };
    let owner_id = WindowId(owner_handle);
    if !chain_ids.contains(&owner_id) {
      continue;
    }
    if matches!(
      layer_of(owner_id),
      Some(NormalZOrderLayer::Floating | NormalZOrderLayer::Ignored)
    ) {
      owner_by_window.insert(window_id, owner_id);
    }
  }

  for ignored in &state.ignored_windows {
    let window_id = ignored.id();
    if !chain_ids.contains(&window_id) {
      continue;
    }
    let Some(owner_handle) = ignored.debug_info().owner_handle else {
      continue;
    };
    let owner_id = WindowId(owner_handle);
    if !chain_ids.contains(&owner_id) {
      continue;
    }
    if matches!(
      layer_of(owner_id),
      Some(NormalZOrderLayer::Floating | NormalZOrderLayer::Ignored)
    ) {
      owner_by_window.insert(window_id, owner_id);
    }
  }

  let owner_of = |id: WindowId| -> Option<WindowId> {
    owner_by_window.get(&id).copied()
  };

  let Some(window_ids) = normal_z_order_chain(
    normal_windows,
    focused_window_id,
    previous_order.as_deref(),
    &owner_of,
  ) else {
    return;
  };

  state.set_normal_z_order_for_workspace(workspace.id(), window_ids);
  let native_window_ids = state
    .normal_z_order_for_workspace(workspace.id())
    .map_or_default(<[WindowId]>::to_vec);

  if verbose_z_order_enabled() {
    crate::commands::general::layout_debug_log(format!(
      "normal z reconcile workspace={:?} focused={:?} layer={:?} chain={:?}",
      workspace.id(),
      focused_window_id,
      focused_layer,
      native_window_ids,
    ));
    log_verbose_native_z_order("pre-reorder", &native_window_ids);
  }

  if let Err(err) = reorder_z_order(&native_window_ids) {
    tracing::warn!(
      "Failed to reorder focused workspace window layers: {}",
      err
    );
  }

  if verbose_z_order_enabled() {
    log_verbose_native_z_order("post-reorder", &native_window_ids);
    schedule_verbose_z_order_recheck(
      "post-reorder".into(),
      native_window_ids.clone(),
    );
  }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NormalZOrderLayer {
  Tiling,
  Floating,
  /// Ignored windows such as Snipping Tool. Same promotion rule as
  /// `Floating`: only the focused one moves.
  Ignored,
}

/// Builds the normal-window z-order chain while treating all tiled windows
/// as one group.
///
/// `current_windows` is in most-recent-focus order, while `previous_order`
/// is the last intended native order from top to bottom. Focus promotion
/// is applied to the previous order so non-selected detached windows
/// retain their existing relationship to the tiled group. Only tiled
/// windows are grouped together; detached windows remain individual
/// z-order items, except a focused Floating/Ignored window also raises its
/// managed Win32 owner group (see `floating_focus_owner_group`).
///
/// `owner_of` maps a managed window to its managed `GW_OWNER` when that
/// owner is also present in this workspace chain (Floating/Ignored owners
/// only — tiling owners stay in the tiled block).
#[cfg(target_os = "windows")]
fn normal_z_order_chain(
  current_windows: Vec<(WindowId, NormalZOrderLayer)>,
  focused_window_id: WindowId,
  previous_order: Option<&[WindowId]>,
  owner_of: &dyn Fn(WindowId) -> Option<WindowId>,
) -> Option<Vec<WindowId>> {
  let focused_layer = current_windows
    .iter()
    .find(|(window_id, _)| *window_id == focused_window_id)
    .map(|(_, layer)| *layer)?;

  // On the first reconciliation there is no prior native order to modify.
  // Preserve the WM's current focus order as the initial order, then use
  // that saved order for subsequent focus promotions.
  let Some(previous_order) = previous_order else {
    return Some(
      current_windows
        .into_iter()
        .map(|(window_id, _)| window_id)
        .collect(),
    );
  };

  let mut previous_or_focus_order = Vec::new();
  for window_id in previous_order {
    if current_windows
      .iter()
      .any(|(current_id, _)| current_id == window_id)
      && !previous_or_focus_order.contains(window_id)
    {
      previous_or_focus_order.push(*window_id);
    }
  }

  for (window_id, _) in &current_windows {
    if !previous_or_focus_order.contains(window_id) {
      previous_or_focus_order.push(*window_id);
    }
  }

  let mut chain = match focused_layer {
    NormalZOrderLayer::Tiling => {
      let mut tiled_windows = current_windows
        .iter()
        .filter(|(_, layer)| *layer == NormalZOrderLayer::Tiling)
        .map(|(window_id, _)| *window_id)
        .collect::<Vec<_>>();

      let other_windows =
        previous_or_focus_order.into_iter().filter(|window_id| {
          current_windows
            .iter()
            .find(|(current_id, _)| current_id == window_id)
            .is_some_and(|(_, layer)| *layer != NormalZOrderLayer::Tiling)
        });

      tiled_windows.extend(other_windows);
      Some(tiled_windows)
    }
    // Floating and Ignored share the same promotion rule: raise the
    // focused hwnd together with its managed Win32 owner group (owned
    // siblings + owners). Unrelated detached peers keep prior slots
    // relative to the tiled block.
    NormalZOrderLayer::Floating | NormalZOrderLayer::Ignored => {
      Some(floating_focus_owner_group(
        &previous_or_focus_order,
        focused_window_id,
        owner_of,
      ))
    }
  }?;

  // Keep the tiled windows as one contiguous block at the slot of the
  // first tile. This repairs a split block without moving a detached
  // or ignored window past another one.
  let tiled_window_ids = current_windows
    .iter()
    .filter(|(_, layer)| *layer == NormalZOrderLayer::Tiling)
    .map(|(window_id, _)| *window_id)
    .collect::<Vec<_>>();

  normalize_tiled_window_block(&mut chain, &tiled_window_ids);

  Some(chain)
}

#[cfg(target_os = "windows")]
fn normalize_tiled_window_block(
  chain: &mut Vec<WindowId>,
  tiled_window_ids: &[WindowId],
) {
  let Some(first_tiled_index) = chain
    .iter()
    .position(|window_id| tiled_window_ids.contains(window_id))
  else {
    return;
  };

  let tiled_windows = chain
    .iter()
    .filter(|window_id| tiled_window_ids.contains(window_id))
    .copied()
    .collect::<Vec<_>>();

  chain.retain(|window_id| !tiled_window_ids.contains(window_id));
  let insertion_index = first_tiled_index.min(chain.len());
  chain.splice(insertion_index..insertion_index, tiled_windows);
}

/// Pure helper for the owned-popup Alt-Tab fix (wired into
/// `normal_z_order_chain`). Covers both **modal** (parent disabled) and
/// **non-modal** owned caption children.
///
/// When focusing a floating/ignored HWND that has a managed `GW_OWNER`,
/// the intended normal-layer chain must promote the focused window **and**
/// its managed owner chain as one contiguous group (focused first, then
/// other owned siblings in prior order, then the owner). Peers outside the
/// group keep their relative order — same spirit as tiling-as-one-group,
/// but for Win32 owner relationships.
///
/// `owner_of(id)` returns the managed owner `WindowId` when
/// `GetWindow(GW_OWNER)` points at another window in the workspace chain;
/// `None` otherwise.
#[cfg(target_os = "windows")]
fn floating_focus_owner_group(
  previous_order: &[WindowId],
  focused: WindowId,
  owner_of: &dyn Fn(WindowId) -> Option<WindowId>,
) -> Vec<WindowId> {
  // Walk owner links from focused until we leave the managed set / hit
  // root.
  let mut owner_chain = Vec::new();
  let mut cursor = focused;
  while let Some(owner) = owner_of(cursor) {
    if owner_chain.contains(&owner) || owner == focused {
      break;
    }
    owner_chain.push(owner);
    cursor = owner;
  }

  let in_group = |id: WindowId| -> bool {
    if id == focused || owner_chain.contains(&id) {
      return true;
    }
    // Owned sibling: its owner walk intersects this owner_chain / focused.
    let mut c = id;
    let mut seen = Vec::new();
    while let Some(owner) = owner_of(c) {
      if owner == focused || owner_chain.contains(&owner) {
        return true;
      }
      if seen.contains(&owner) {
        break;
      }
      seen.push(owner);
      c = owner;
    }
    false
  };

  let mut group: Vec<WindowId> = Vec::new();
  group.push(focused);
  for id in previous_order {
    if *id != focused && in_group(*id) && !group.contains(id) {
      group.push(*id);
    }
  }
  // Ensure every owner-chain member is present even if missing from
  // previous.
  for owner in &owner_chain {
    if !group.contains(owner) {
      group.push(*owner);
    }
  }

  let mut rest: Vec<WindowId> = previous_order
    .iter()
    .copied()
    .filter(|id| !group.contains(id))
    .collect();

  group.append(&mut rest);
  group
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use super::*;

  fn window(
    id: isize,
    layer: NormalZOrderLayer,
  ) -> (WindowId, NormalZOrderLayer) {
    (WindowId(id), layer)
  }

  fn no_owner(_: WindowId) -> Option<WindowId> {
    None
  }

  fn chain(
    current_windows: Vec<(WindowId, NormalZOrderLayer)>,
    focused_window_id: WindowId,
    previous_order: Option<&[WindowId]>,
  ) -> Option<Vec<WindowId>> {
    normal_z_order_chain(
      current_windows,
      focused_window_id,
      previous_order,
      &no_owner,
    )
  }

  #[test]
  fn detached_focus_only_promotes_selected_window() {
    let d1 = window(1, NormalZOrderLayer::Floating);
    let d2 = window(2, NormalZOrderLayer::Floating);
    let w3 = window(3, NormalZOrderLayer::Tiling);
    let w4 = window(4, NormalZOrderLayer::Tiling);

    // Bootstrap the saved order from the initial stack:
    // Detached1 > Detached2 > GlazeWM.
    assert_eq!(
      chain(vec![d1, d2, w3, w4], d1.0, None),
      Some(vec![d1.0, d2.0, w3.0, w4.0])
    );

    // Alt-Tab to detached Window2: only that detached window is promoted.
    assert_eq!(
      chain(vec![d2, d1, w3, w4], d2.0, Some(&[d1.0, d2.0, w3.0, w4.0]),),
      Some(vec![d2.0, d1.0, w3.0, w4.0])
    );

    // Alt-Tab to tiled Window3: the complete tiled group is promoted.
    assert_eq!(
      chain(vec![w3, w4, d2, d1], w3.0, Some(&[d2.0, d1.0, w3.0, w4.0]),),
      Some(vec![w3.0, w4.0, d2.0, d1.0])
    );
  }

  #[test]
  fn alt_tab_to_snipping_then_visual_studio_keeps_snipping_above_tiles() {
    let visual_studio = window(1, NormalZOrderLayer::Floating);
    let snipping = window(2, NormalZOrderLayer::Ignored);
    let tiled_a = window(3, NormalZOrderLayer::Tiling);
    let tiled_b = window(4, NormalZOrderLayer::Tiling);
    let windows = vec![tiled_a, tiled_b, visual_studio, snipping];

    // Tiles are focused. Snipping Tool is behind the tiled group.
    let tiles_focused =
      [tiled_a.0, tiled_b.0, visual_studio.0, snipping.0];

    // Alt-Tab to Snipping Tool promotes only that window.
    let snipping_focused =
      chain(windows.clone(), snipping.0, Some(&tiles_focused))
        .expect("snipping focus");
    assert_eq!(
      snipping_focused,
      vec![snipping.0, tiled_a.0, tiled_b.0, visual_studio.0]
    );

    // The next Alt-Tab lands on Visual Studio. Only Visual Studio
    // moves. Snipping Tool stays above the tiled group.
    let visual_studio_focused =
      chain(windows, visual_studio.0, Some(&snipping_focused))
        .expect("visual studio focus");
    assert_eq!(
      visual_studio_focused,
      vec![visual_studio.0, snipping.0, tiled_a.0, tiled_b.0]
    );
  }

  #[test]
  fn focusing_a_tiled_window_raises_the_group_without_reordering_peers() {
    let visual_studio = window(1, NormalZOrderLayer::Floating);
    let snipping = window(2, NormalZOrderLayer::Ignored);
    let tiled_a = window(3, NormalZOrderLayer::Tiling);
    let tiled_b = window(4, NormalZOrderLayer::Tiling);

    // Visual Studio is in front, Snipping Tool is still above the tiles.
    let before = [visual_studio.0, snipping.0, tiled_a.0, tiled_b.0];
    assert_eq!(
      chain(
        vec![tiled_a, tiled_b, visual_studio, snipping],
        tiled_a.0,
        Some(&before),
      ),
      Some(vec![tiled_a.0, tiled_b.0, visual_studio.0, snipping.0])
    );
  }

  /// layout.log 2026-10-02 22:35:50 ET: after an ignored toast
  /// (`WindowId(984154)`, absent from `EnumWindows`) has been promoted,
  /// tiling focus on Grok keeps that toast in the detached tail,
  /// immediately before Notepad. `apply_z_order_chain` then uses the
  /// toast as Notepad's insert-after hwnd.
  #[test]
  fn tiling_focus_leaves_unenumerated_ignored_window_directly_before_notepad(
  ) {
    let grok = window(131_888, NormalZOrderLayer::Tiling);
    let peer = window(67_798, NormalZOrderLayer::Tiling);
    let other = window(1_575_680, NormalZOrderLayer::Tiling);
    let floater = window(3_803_134, NormalZOrderLayer::Floating);
    let notification = window(984_154, NormalZOrderLayer::Ignored);
    let notepad = window(67_996, NormalZOrderLayer::Floating);

    // Saved order after the toast was focused, then the floater above it
    // (layout.log 02:35:47Z chain).
    let previous = [
      floater.0,
      notification.0,
      grok.0,
      peer.0,
      other.0,
      notepad.0,
    ];
    let chain = chain(
      vec![grok, peer, other, floater, notepad, notification],
      grok.0,
      Some(&previous),
    )
    .expect("tiling focus chain");

    assert_eq!(
      chain,
      vec![
        grok.0,
        peer.0,
        other.0,
        floater.0,
        notification.0,
        notepad.0,
      ]
    );
    let notepad_at = chain.iter().position(|id| *id == notepad.0).unwrap();
    assert_eq!(
      chain[notepad_at - 1],
      notification.0,
      "Notepad's chain predecessor is the ignored toast, not a tiled window"
    );
  }

  #[test]
  fn e2e_leaving_ignored_focus_rejoins_ignored_before_detached_and_after_tiled(
  ) {
    let ignored = window(1, NormalZOrderLayer::Ignored);
    let tiled_a = window(2, NormalZOrderLayer::Tiling);
    let tiled_b = window(3, NormalZOrderLayer::Tiling);
    let detached = window(4, NormalZOrderLayer::Floating);

    // Snipping Tool is above the detached window. Raising the tiled
    // group keeps that peer order behind the group.
    assert_eq!(
      chain(
        vec![ignored, tiled_a, tiled_b, detached],
        tiled_a.0,
        Some(&[ignored.0, detached.0, tiled_a.0, tiled_b.0]),
      ),
      Some(vec![tiled_a.0, tiled_b.0, ignored.0, detached.0])
    );
  }

  #[test]
  fn e2e_ignored_focus_preserves_detached_peer_order() {
    let notepad_one = window(1, NormalZOrderLayer::Floating);
    let notepad_two = window(2, NormalZOrderLayer::Floating);
    let ignored = window(3, NormalZOrderLayer::Ignored);
    let tiled_a = window(4, NormalZOrderLayer::Tiling);
    let tiled_b = window(5, NormalZOrderLayer::Tiling);

    let initial = [
      notepad_one.0,
      notepad_two.0,
      ignored.0,
      tiled_a.0,
      tiled_b.0,
    ];

    // Alt-Tab to the ignored window promotes only that window. Neither
    // Notepad moves relative to the tiled group.
    let after_ignored_focus = chain(
      vec![notepad_one, notepad_two, ignored, tiled_a, tiled_b],
      ignored.0,
      Some(&initial),
    )
    .expect("ignored focus chain");
    assert_eq!(
      after_ignored_focus,
      vec![
        ignored.0,
        notepad_one.0,
        notepad_two.0,
        tiled_a.0,
        tiled_b.0,
      ]
    );

    // Returning to either detached window promotes only that window.
    let after_notepad_one = chain(
      vec![notepad_one, notepad_two, ignored, tiled_a, tiled_b],
      notepad_one.0,
      Some(&after_ignored_focus),
    )
    .expect("detached focus chain");
    assert_eq!(
      after_notepad_one,
      vec![
        notepad_one.0,
        ignored.0,
        notepad_two.0,
        tiled_a.0,
        tiled_b.0,
      ]
    );

    let after_notepad_two = chain(
      vec![notepad_two, notepad_one, ignored, tiled_a, tiled_b],
      notepad_two.0,
      Some(&after_notepad_one),
    )
    .expect("second detached focus chain");
    assert_eq!(
      after_notepad_two,
      vec![
        notepad_two.0,
        notepad_one.0,
        ignored.0,
        tiled_a.0,
        tiled_b.0,
      ]
    );

    // Focusing a tiled window raises the group as one window. The
    // detached and ignored windows keep the order they already had.
    let after_tiled = chain(
      vec![tiled_a, tiled_b, notepad_two, notepad_one, ignored],
      tiled_a.0,
      Some(&after_notepad_two),
    )
    .expect("tiled focus chain");
    assert_eq!(
      after_tiled,
      vec![
        tiled_a.0,
        tiled_b.0,
        notepad_two.0,
        notepad_one.0,
        ignored.0,
      ]
    );
  }

  #[test]
  fn detaching_a_tiled_focus_preserves_the_group_position() {
    let d1 = window(1, NormalZOrderLayer::Floating);
    let d2 = window(2, NormalZOrderLayer::Floating);
    let d3 = window(3, NormalZOrderLayer::Floating);
    let w4 = window(4, NormalZOrderLayer::Tiling);

    // Window3 was the focused tiled window and is now detached. It moves
    // to the front, while the remaining tiled group stays ahead of old
    // peers.
    assert_eq!(
      chain(vec![d3, w4, d2, d1], d3.0, Some(&[d3.0, w4.0, d2.0, d1.0]),),
      Some(vec![d3.0, w4.0, d2.0, d1.0])
    );

    // Reattaching Window3 makes it part of the tiled group again.
    let w3 = window(3, NormalZOrderLayer::Tiling);
    assert_eq!(
      chain(vec![w3, w4, d2, d1], w3.0, Some(&[d3.0, w4.0, d2.0, d1.0]),),
      Some(vec![w3.0, w4.0, d2.0, d1.0])
    );
  }

  #[test]
  fn nonfocused_layer_change_keeps_tiled_windows_contiguous() {
    let d1 = window(1, NormalZOrderLayer::Floating);
    let d2 = window(2, NormalZOrderLayer::Floating);
    let t1 = window(3, NormalZOrderLayer::Tiling);
    let t2 = window(4, NormalZOrderLayer::Tiling);

    // The old order has one tiled block followed by two detached windows.
    // Detached2 becomes tiled while Detached1 remains focused. Without
    // normalization this would leave Detached1 between the tiled windows.
    assert_eq!(
      chain(
        vec![d1, t1, t2, window(2, NormalZOrderLayer::Tiling)],
        d1.0,
        Some(&[t1.0, t2.0, d1.0, d2.0]),
      ),
      Some(vec![d1.0, t1.0, t2.0, d2.0])
    );
  }

  #[test]
  fn tiling_focus_defers_bring_to_front_to_workspace_reorder() {
    assert!(should_defer_bring_to_front_to_workspace_reorder(
      false, true, false
    ));
    assert!(
      should_defer_bring_to_front_to_workspace_reorder(true, false, false),
      "targeted floating also defers"
    );
    assert!(
      should_defer_bring_to_front_to_workspace_reorder(false, false, true),
      "queued workspace reorder (Super+arrow) must defer even without targeted mark"
    );
    assert!(
      !should_defer_bring_to_front_to_workspace_reorder(
        false, false, false
      ),
      "without tiling/targeted/reorder, legacy set_z_order path remains"
    );
  }

  #[test]
  fn floating_directional_focus_chain_promotes_only_selected_floater() {
    // Regression: 3 floaters + tiles, Super+Left/Right among floaters.
    // Focusing a detached window must promote ONLY that window; peer
    // floaters keep their relation to the tiled block (f1 stays above
    // tiles; f3 stays below).
    let f1 = window(1, NormalZOrderLayer::Floating);
    let f2 = window(2, NormalZOrderLayer::Floating);
    let f3 = window(3, NormalZOrderLayer::Floating);
    let t1 = window(10, NormalZOrderLayer::Tiling);
    let t2 = window(11, NormalZOrderLayer::Tiling);

    // Prior: f1 on top of tiles, f2/f3 below the tiled block.
    let previous = [f1.0, t1.0, t2.0, f2.0, f3.0];
    assert_eq!(
      chain(vec![f2, f1, f3, t1, t2], f2.0, Some(&previous),),
      Some(vec![f2.0, f1.0, t1.0, t2.0, f3.0]),
      "only f2 moves to front; f1 stays above tiles; f3 stays below"
    );

    // Second Super+arrow to f3: only f3 rises; f2/f1 keep peer order
    // relative to the tiled block.
    assert_eq!(
      chain(
        vec![f3, f2, f1, t1, t2],
        f3.0,
        Some(&[f2.0, f1.0, t1.0, t2.0, f3.0]),
      ),
      Some(vec![f3.0, f2.0, f1.0, t1.0, t2.0]),
      "only f3 moves; peers and tiled block undisturbed"
    );
  }

  #[test]
  fn tiling_focus_chain_keeps_floaters_below_contiguous_tiles() {
    // Bug A regression: Super+arrow tiling focus must not leave floaters
    // between tiles. The chain builder places the whole tiled group first.
    let tile_a = window(10, NormalZOrderLayer::Tiling);
    let tile_b = window(11, NormalZOrderLayer::Tiling);
    let tile_c = window(12, NormalZOrderLayer::Tiling);
    let steam = window(20, NormalZOrderLayer::Floating);
    let grok = window(21, NormalZOrderLayer::Floating);

    let previous = [steam.0, tile_a.0, grok.0, tile_b.0, tile_c.0];
    let chain = chain(
      vec![tile_b, tile_a, tile_c, steam, grok],
      tile_b.0,
      Some(&previous),
    )
    .expect("tiling focus chain");

    assert_eq!(
      &chain[..3],
      &[tile_b.0, tile_a.0, tile_c.0],
      "tiled block must be contiguous at the front"
    );
    assert!(
      chain[3..].iter().all(|id| *id == steam.0 || *id == grok.0),
      "floaters must stay below the tiled block: {chain:?}"
    );
    assert!(
      !chain.windows(3).any(|w| {
        let is_tile = |id: WindowId| {
          id == tile_a.0 || id == tile_b.0 || id == tile_c.0
        };
        let is_floater = |id: WindowId| id == steam.0 || id == grok.0;
        is_tile(w[0]) && is_floater(w[1]) && is_tile(w[2])
      }),
      "no floater may sit between tiles: {chain:?}"
    );
  }

  /// **Non-modal** owned popup Alt-Tab (`GW_OWNER` + caption +
  /// `WS_EX_APPWINDOW`, parent still enabled): floating focus promotes the
  /// popup and its managed owner as one group above tiles (Asus helper
  /// hwnd 4720436 / owner 330456).
  #[test]
  fn non_modal_owned_focus_raises_managed_owner_above_tiles() {
    let popup = window(4_720_436, NormalZOrderLayer::Floating);
    let owner = window(330_456, NormalZOrderLayer::Floating);
    let tile_a = window(67_798, NormalZOrderLayer::Tiling);
    let tile_b = window(131_888, NormalZOrderLayer::Tiling);

    let previous = [tile_a.0, tile_b.0, popup.0, owner.0];
    let owner_of = |id: WindowId| -> Option<WindowId> {
      if id == popup.0 {
        Some(owner.0)
      } else {
        None
      }
    };
    let result = normal_z_order_chain(
      vec![popup, owner, tile_a, tile_b],
      popup.0,
      Some(&previous),
      &owner_of,
    )
    .expect("chain");

    assert_eq!(
      result,
      vec![popup.0, owner.0, tile_a.0, tile_b.0],
      "non-modal owned Alt-Tab must raise parent+popup as one group"
    );
  }

  /// **Modal** owned dialog Alt-Tab (EnableWindow(parent, FALSE) +
  /// `WS_EX_DLGMODALFRAME` | `WS_EX_APPWINDOW`). Same owner-group raise as
  /// non-modal — both are managed Floating peers linked by `GW_OWNER`.
  /// Asus layout.log modal 724112 / owner 330456.
  #[test]
  fn modal_owned_focus_raises_managed_owner_above_tiles() {
    let popup = window(724_112, NormalZOrderLayer::Floating);
    let owner = window(330_456, NormalZOrderLayer::Floating);
    let tile_a = window(67_798, NormalZOrderLayer::Tiling);
    let tile_b = window(131_888, NormalZOrderLayer::Tiling);

    let previous = [tile_a.0, tile_b.0, popup.0, owner.0];
    let owner_of = |id: WindowId| -> Option<WindowId> {
      if id == popup.0 {
        Some(owner.0)
      } else {
        None
      }
    };
    let result = normal_z_order_chain(
      vec![popup, owner, tile_a, tile_b],
      popup.0,
      Some(&previous),
      &owner_of,
    )
    .expect("chain");

    assert_eq!(
      result,
      vec![popup.0, owner.0, tile_a.0, tile_b.0],
      "modal owned Alt-Tab must raise parent+dialog as one group"
    );
  }

  /// Helper-level check for **non-modal** owned popup grouping.
  #[test]
  fn non_modal_owner_group_raises_popup_and_owner_above_tiles() {
    let popup = WindowId(4_720_436);
    let owner = WindowId(330_456);
    let tile_a = WindowId(67_798);
    let tile_b = WindowId(131_888);
    let previous = [tile_a, tile_b, popup, owner];

    let owner_of = |id: WindowId| -> Option<WindowId> {
      if id == popup {
        Some(owner)
      } else {
        None
      }
    };

    assert_eq!(
      floating_focus_owner_group(&previous, popup, &owner_of),
      vec![popup, owner, tile_a, tile_b],
      "non-modal owned Alt-Tab must raise parent+popup as one group"
    );
  }

  /// Helper-level check for **modal** owned dialog grouping.
  #[test]
  fn modal_owner_group_raises_dialog_and_owner_above_tiles() {
    let popup = WindowId(724_112);
    let owner = WindowId(330_456);
    let tile_a = WindowId(67_798);
    let tile_b = WindowId(131_888);
    let previous = [tile_a, tile_b, popup, owner];

    let owner_of = |id: WindowId| -> Option<WindowId> {
      if id == popup {
        Some(owner)
      } else {
        None
      }
    };

    assert_eq!(
      floating_focus_owner_group(&previous, popup, &owner_of),
      vec![popup, owner, tile_a, tile_b],
      "modal owned Alt-Tab must raise parent+dialog as one group"
    );
  }

  #[test]
  fn floating_focus_owner_group_preserves_owned_sibling_order() {
    let popup = WindowId(100);
    let sibling = WindowId(101);
    let owner = WindowId(200);
    let other_float = WindowId(300);
    let tile = WindowId(400);
    // Prior: tiles, then other floater, then owner group (sibling above
    // popup).
    let previous = [tile, other_float, sibling, popup, owner];

    let owner_of = |id: WindowId| -> Option<WindowId> {
      if id == popup || id == sibling {
        Some(owner)
      } else {
        None
      }
    };

    assert_eq!(
      floating_focus_owner_group(&previous, popup, &owner_of),
      vec![popup, sibling, owner, tile, other_float],
      "focused owned first; other owned siblings keep prior relative order; owner last in group"
    );
  }

  #[test]
  fn floating_focus_owner_group_noop_without_owner() {
    let floater = WindowId(1);
    let tile = WindowId(2);
    let previous = [tile, floater];
    let owner_of = |_id: WindowId| -> Option<WindowId> { None };

    assert_eq!(
      floating_focus_owner_group(&previous, floater, &owner_of),
      vec![floater, tile],
      "unowned floater still promotes alone"
    );
  }

  #[test]
  fn hung_focused_tile_chain_still_intends_tiles_then_floaters() {
    // Bug B intent: even when the focused tile is a hung debug helper, the
    // *intended* chain remains tiles-then-floaters. Native apply must then
    // raise responsive peers without using the hung hwnd as an anchor.
    let hung_helper = window(2_168_898, NormalZOrderLayer::Tiling);
    let tile = window(265_086, NormalZOrderLayer::Tiling);
    let steam = window(330_202, NormalZOrderLayer::Floating);

    let previous = [steam.0, tile.0, hung_helper.0];
    assert_eq!(
      chain(
        vec![hung_helper, tile, steam],
        hung_helper.0,
        Some(&previous),
      ),
      Some(vec![hung_helper.0, tile.0, steam.0])
    );
  }
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
      if preserve_z_order {
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
