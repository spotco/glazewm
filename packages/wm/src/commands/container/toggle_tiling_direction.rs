use wm_common::{TilingDirection, WmEvent};

use crate::{
  models::Container, traits::CommonGetters, user_config::UserConfig,
  wm_state::WmState,
};

/// Spotcobuild: flip the WM-wide `global_tiling_direction` only.
///
/// Does not wrap/flatten the focused window or mutate per-split
/// directions.
pub fn toggle_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
) -> anyhow::Result<()> {
  let new_dir = state.global_tiling_direction.inverse();
  set_global_tiling_direction(state, new_dir)
}

/// Spotcobuild: set the WM-wide `global_tiling_direction` only.
pub fn set_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
  tiling_direction: &TilingDirection,
) -> anyhow::Result<()> {
  if state.global_tiling_direction != *tiling_direction {
    set_global_tiling_direction(state, tiling_direction.clone())?;
  }
  Ok(())
}

fn set_global_tiling_direction(
  state: &mut WmState,
  new_tiling_direction: TilingDirection,
) -> anyhow::Result<()> {
  state.global_tiling_direction = new_tiling_direction.clone();
  state.emit_event(WmEvent::GlobalTilingDirectionChanged {
    new_tiling_direction: new_tiling_direction.clone(),
  });

  // Compat: existing Zebar chips listen for TilingDirectionChanged.
  if let Some(direction_container) = state
    .focused_container()
    .and_then(|focused| focused.direction_container())
  {
    state.emit_event(WmEvent::TilingDirectionChanged {
      direction_container: direction_container.to_dto()?,
      new_tiling_direction,
    });
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tiling_direction_inverse_round_trips() {
    assert_eq!(
      TilingDirection::Horizontal.inverse(),
      TilingDirection::Vertical
    );
    assert_eq!(
      TilingDirection::Vertical.inverse(),
      TilingDirection::Horizontal
    );
  }
}
