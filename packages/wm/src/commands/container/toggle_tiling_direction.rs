use wm_common::{TilingDirection, WmEvent};

use crate::{
  models::Container, user_config::UserConfig, wm_state::WmState,
};

/// Spotcobuild: flip the WM-wide `global_tiling_direction` only.
///
/// Does not wrap/flatten the focused window or mutate per-split
/// directions.
#[allow(clippy::unnecessary_wraps)]
pub fn toggle_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
) -> anyhow::Result<()> {
  let new_dir =
    toggle_global_tiling_direction(&state.global_tiling_direction);
  set_global_tiling_direction(state, &new_dir);
  Ok(())
}

/// Spotcobuild: set the WM-wide `global_tiling_direction` only.
#[allow(clippy::unnecessary_wraps)]
pub fn set_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
  tiling_direction: &TilingDirection,
) -> anyhow::Result<()> {
  if state.global_tiling_direction != *tiling_direction {
    set_global_tiling_direction(state, tiling_direction);
  }
  Ok(())
}

fn set_global_tiling_direction(
  state: &mut WmState,
  new_tiling_direction: &TilingDirection,
) {
  if !apply_global_tiling_direction(
    &mut state.global_tiling_direction,
    new_tiling_direction,
  ) {
    return;
  }

  state.emit_event(global_tiling_direction_changed_event(
    new_tiling_direction.clone(),
  ));
}

fn toggle_global_tiling_direction(
  current: &TilingDirection,
) -> TilingDirection {
  current.inverse()
}

fn apply_global_tiling_direction(
  current: &mut TilingDirection,
  requested: &TilingDirection,
) -> bool {
  if *current == *requested {
    return false;
  }
  *current = requested.clone();
  true
}

fn global_tiling_direction_changed_event(
  new_tiling_direction: TilingDirection,
) -> WmEvent {
  WmEvent::GlobalTilingDirectionChanged {
    new_tiling_direction,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::wm_state::DEFAULT_GLOBAL_TILING_DIRECTION;

  #[test]
  fn default_global_direction_is_horizontal() {
    assert_eq!(
      DEFAULT_GLOBAL_TILING_DIRECTION,
      TilingDirection::Horizontal
    );
  }

  #[test]
  fn toggle_global_direction_round_trips() {
    assert_eq!(
      toggle_global_tiling_direction(&TilingDirection::Horizontal),
      TilingDirection::Vertical
    );
    assert_eq!(
      toggle_global_tiling_direction(&TilingDirection::Vertical),
      TilingDirection::Horizontal
    );
  }

  #[test]
  fn set_global_direction_changes_only_when_needed() {
    let mut current = TilingDirection::Horizontal;
    assert!(apply_global_tiling_direction(
      &mut current,
      &TilingDirection::Vertical,
    ));
    assert_eq!(current, TilingDirection::Vertical);
    assert!(!apply_global_tiling_direction(
      &mut current,
      &TilingDirection::Vertical,
    ));
  }

  #[test]
  fn global_direction_event_contains_new_direction() {
    let event =
      global_tiling_direction_changed_event(TilingDirection::Vertical);
    assert!(matches!(
      event,
      WmEvent::GlobalTilingDirectionChanged {
        new_tiling_direction: TilingDirection::Vertical
      }
    ));
  }
}
