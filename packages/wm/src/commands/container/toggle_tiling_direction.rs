use wm_common::{TilingDirection, WmEvent};

use crate::{
  models::Container,
  user_config::UserConfig,
  wm_state::WmState,
};

/// Spotcobuild: flip the WM-wide global_tiling_direction only.
///
/// Does not wrap/flatten the focused window or mutate per-split directions.
pub fn toggle_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
) -> anyhow::Result<()> {
  let new_dir = state.global_tiling_direction.inverse();
  set_global_tiling_direction(state, new_dir);
  Ok(())
}

/// Spotcobuild: set the WM-wide global_tiling_direction only.
pub fn set_tiling_direction(
  _container: Container,
  state: &mut WmState,
  _config: &UserConfig,
  tiling_direction: &TilingDirection,
) -> anyhow::Result<()> {
  if state.global_tiling_direction != *tiling_direction {
    set_global_tiling_direction(state, tiling_direction.clone());
  }
  Ok(())
}

fn set_global_tiling_direction(
  state: &mut WmState,
  new_tiling_direction: TilingDirection,
) {
  state.global_tiling_direction = new_tiling_direction.clone();
  state.emit_event(WmEvent::GlobalTilingDirectionChanged {
    new_tiling_direction,
  });
}

#[cfg(test)]
mod tests {
  use super::*;
  use wm_platform::Dispatcher;

  // Lightweight check that inverse toggles Horizontal <-> Vertical on the
  // enum itself (WmState construction needs a real dispatcher in full tests).
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
