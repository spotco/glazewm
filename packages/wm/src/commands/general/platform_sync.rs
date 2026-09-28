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
use wm_platform::SWP_NOZORDER;
#[cfg(target_os = "windows")]
use wm_platform::{CornerStyle, OpacityValue};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, WindowId};
use wm_platform::{Rect, WindowZOrder};

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
    tracing::info!("Setting focus to window: {window}");
    window.native().focus()
  } else {
    tracing::info!("Setting focus to the desktop window.");
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
    #[cfg(target_os = "windows")]
    if !preserve_z_order
      && should_bring_to_front
      && !windows_to_redraw.contains(window)
    {
      let has_targeted_floating_focus = state
        .pending_sync
        .focused_window_to_bring_to_front(&workspace)
        .is_some_and(|window_id| window_id == window.id());

      if !has_targeted_floating_focus {
        tracing::info!("Updating window z-order: {window}");

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

    if let Err(err) = reposition_window(
      window,
      *hide_corner,
      &z_order,
      is_visible,
      is_show_desktop_minimized,
      preserve_z_order,
      config,
    ) {
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
    #[cfg(target_os = "windows")]
    if config.value.general.hide_method == HideMethod::Cloak
      && !config.value.general.show_all_in_taskbar
      && matches!(
        window.display_state(),
        DisplayState::Showing | DisplayState::Hiding
      )
    {
      if let Err(err) = window.native().set_taskbar_visibility(is_visible)
      {
        tracing::warn!("Failed to set taskbar visibility: {}", err);
      }
    }
  }

  #[cfg(target_os = "windows")]
  if !preserve_z_order {
    reorder_focused_workspace_layers(focused_container, state);
  }

  Ok(())
}

/// Restores the normal floating/tiling layers after a focus or state
/// change.
///
/// Normal detached windows are individual z-order items. Tiled windows are
/// treated as one z-order group. Focusing a detached window promotes only
/// that window, while focusing a tiled window promotes the complete tiled
/// group. This prevents a detached focus event from pushing its peers
/// behind the tiled layer.
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

#[cfg(target_os = "windows")]
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
    .map(|window| window.id())
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
      .map(|window_id| (window_id, NormalZOrderLayer::Floating)),
  );
  let focused_layer = normal_windows
    .iter()
    .find(|(window_id, _)| *window_id == focused_window_id)
    .map(|(_, layer)| *layer);
  let previous_order = state
    .normal_z_order_for_workspace(workspace.id())
    .map(<[WindowId]>::to_vec);

  let Some(window_ids) = normal_z_order_chain(
    normal_windows,
    focused_window_id,
    previous_order.as_deref(),
  ) else {
    return;
  };

  state.set_normal_z_order_for_workspace(workspace.id(), window_ids);
  let native_window_ids = state
    .normal_z_order_for_workspace(workspace.id())
    .map(<[WindowId]>::to_vec)
    .unwrap_or_default();

  crate::commands::general::layout_debug_log(format!(
    "normal z reconcile workspace={:?} focused={:?} layer={:?} chain={:?}",
    workspace.id(),
    focused_window_id,
    focused_layer,
    native_window_ids,
  ));

  if let Err(err) = reorder_z_order(&native_window_ids) {
    tracing::warn!(
      "Failed to reorder focused workspace window layers: {}",
      err
    );
  }
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NormalZOrderLayer {
  Tiling,
  Floating,
}

/// Builds the normal-window z-order chain while treating all tiled windows
/// as one group.
///
/// `current_windows` is in most-recent-focus order, while `previous_order`
/// is the last intended native order from top to bottom. Focus promotion
/// is applied to the previous order so non-selected detached windows
/// retain their existing relationship to the tiled group. Only tiled
/// windows are grouped together; detached windows remain individual
/// z-order items.
#[cfg(target_os = "windows")]
fn normal_z_order_chain(
  current_windows: Vec<(WindowId, NormalZOrderLayer)>,
  focused_window_id: WindowId,
  previous_order: Option<&[WindowId]>,
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
    NormalZOrderLayer::Floating => {
      previous_or_focus_order
        .retain(|window_id| *window_id != focused_window_id);
      previous_or_focus_order.insert(0, focused_window_id);
      Some(previous_or_focus_order)
    }
  }?;

  // A non-focused window can change layers while another window has focus.
  // Normalize the result in that case too, otherwise a floating window
  // that used to sit inside the tiled block can split the remaining
  // tiled windows.
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use super::*;

  fn window(
    id: isize,
    layer: NormalZOrderLayer,
  ) -> (WindowId, NormalZOrderLayer) {
    (WindowId(id), layer)
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
      normal_z_order_chain(vec![d1, d2, w3, w4], d1.0, None),
      Some(vec![d1.0, d2.0, w3.0, w4.0])
    );

    // Alt-Tab to detached Window2: only that detached window is promoted.
    assert_eq!(
      normal_z_order_chain(
        vec![d2, d1, w3, w4],
        d2.0,
        Some(&[d1.0, d2.0, w3.0, w4.0]),
      ),
      Some(vec![d2.0, d1.0, w3.0, w4.0])
    );

    // Alt-Tab to tiled Window3: the complete tiled group is promoted.
    assert_eq!(
      normal_z_order_chain(
        vec![w3, w4, d2, d1],
        w3.0,
        Some(&[d2.0, d1.0, w3.0, w4.0]),
      ),
      Some(vec![w3.0, w4.0, d2.0, d1.0])
    );
  }

  #[test]
  fn e2e_ignored_window_stays_between_detached_and_tiled_after_alt_tab() {
    let detached = window(1, NormalZOrderLayer::Floating);
    let ignored = window(2, NormalZOrderLayer::Floating);
    let tiled_a = window(3, NormalZOrderLayer::Tiling);
    let tiled_b = window(4, NormalZOrderLayer::Tiling);

    // Native state immediately before Alt-Tab: Snipping Tool was focused
    // over the tiled group. The detached window is then selected by
    // Alt-Tab. The required final native order is:
    //
    //   detached > ignored > tiled group
    assert_eq!(
      normal_z_order_chain(
        vec![ignored, tiled_a, tiled_b, detached],
        detached.0,
        Some(&[ignored.0, tiled_a.0, tiled_b.0, detached.0]),
      ),
      Some(vec![detached.0, ignored.0, tiled_a.0, tiled_b.0])
    );
  }

  #[test]
  fn e2e_returning_from_ignored_focus_keeps_ignored_above_tiled_group() {
    let detached = window(1, NormalZOrderLayer::Floating);
    let ignored = window(2, NormalZOrderLayer::Floating);
    let tiled_a = window(3, NormalZOrderLayer::Tiling);
    let tiled_b = window(4, NormalZOrderLayer::Tiling);

    // Snipping Tool is foreground immediately before Alt-Tab returns to
    // detached Notepad. The ignored window must remain between Notepad and
    // the tiled group after the managed focus event is processed.
    assert_eq!(
      normal_z_order_chain(
        vec![ignored, detached, tiled_a, tiled_b],
        detached.0,
        Some(&[ignored.0, detached.0, tiled_a.0, tiled_b.0]),
      ),
      Some(vec![detached.0, ignored.0, tiled_a.0, tiled_b.0])
    );
  }

  #[test]
  fn e2e_leaving_ignored_focus_rejoins_ignored_before_detached_and_after_tiled(
  ) {
    let ignored = window(1, NormalZOrderLayer::Floating);
    let tiled_a = window(2, NormalZOrderLayer::Tiling);
    let tiled_b = window(3, NormalZOrderLayer::Tiling);
    let detached = window(4, NormalZOrderLayer::Floating);

    // Snipping Tool was foreground and may have acquired a native TOPMOST
    // bit. Once Alt-Tab returns to a tiled window, the ignored HWND must
    // be present in the reconciliation chain so the native reorder
    // clears that bit and leaves the ignored window below the tiled
    // group.
    assert_eq!(
      normal_z_order_chain(
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
    let ignored = window(3, NormalZOrderLayer::Floating);
    let tiled_a = window(4, NormalZOrderLayer::Tiling);
    let tiled_b = window(5, NormalZOrderLayer::Tiling);

    let initial = [
      notepad_one.0,
      notepad_two.0,
      ignored.0,
      tiled_a.0,
      tiled_b.0,
    ];

    // The ignored focus event is a native-only transition, so it does not
    // call normal_z_order_chain and leaves both Notepads untouched.
    let after_ignored_focus = initial.to_vec();
    assert_eq!(after_ignored_focus, initial);

    // Returning to either detached window promotes only that window. The
    // other detached/ignored windows retain their existing relative order.
    let after_notepad_one = normal_z_order_chain(
      vec![notepad_one, notepad_two, ignored, tiled_a, tiled_b],
      notepad_one.0,
      Some(&after_ignored_focus),
    )
    .expect("detached focus chain");
    assert_eq!(after_notepad_one, initial);

    let after_notepad_two = normal_z_order_chain(
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
        tiled_b.0
      ]
    );

    // Focusing a tiled window promotes the tiled group as one unit and
    // preserves the detached/ignored order behind it.
    let after_tiled = normal_z_order_chain(
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
      normal_z_order_chain(
        vec![d3, w4, d2, d1],
        d3.0,
        Some(&[d3.0, w4.0, d2.0, d1.0]),
      ),
      Some(vec![d3.0, w4.0, d2.0, d1.0])
    );

    // Reattaching Window3 makes it part of the tiled group again.
    let w3 = window(3, NormalZOrderLayer::Tiling);
    assert_eq!(
      normal_z_order_chain(
        vec![w3, w4, d2, d1],
        w3.0,
        Some(&[d3.0, w4.0, d2.0, d1.0]),
      ),
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
      normal_z_order_chain(
        vec![d1, t1, t2, window(2, NormalZOrderLayer::Tiling)],
        d1.0,
        Some(&[t1.0, t2.0, d1.0, d2.0]),
      ),
      Some(vec![d1.0, t1.0, t2.0, d2.0])
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
