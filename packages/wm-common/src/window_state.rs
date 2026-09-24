use serde::{Deserialize, Serialize};

use crate::{
  parsed_config::{
    FloatingStateConfig, FullscreenStateConfig, InitialWindowState,
  },
  ParsedConfig,
};

/// Represents the possible states a window can have.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WindowState {
  Floating(FloatingStateConfig),
  Fullscreen(FullscreenStateConfig),
  Minimized,
  Tiling,
}

impl WindowState {
  #[must_use]
  pub fn default_from_config(config: &ParsedConfig) -> Self {
    match config.window_behavior.initial_state {
      InitialWindowState::Tiling => Self::Tiling,
      InitialWindowState::Floating => Self::Floating(
        config.window_behavior.state_defaults.floating.clone(),
      ),
    }
  }

  #[must_use]
  pub fn is_same_state(&self, other: &Self) -> bool {
    std::mem::discriminant(self) == std::mem::discriminant(other)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{FloatingStateConfig, FullscreenStateConfig};

  #[test]
  fn same_variant_config_differs_by_full_equality() {
    let a = WindowState::Floating(FloatingStateConfig {
      centered: true,
      shown_on_top: false,
    });
    let b = WindowState::Floating(FloatingStateConfig {
      centered: false,
      shown_on_top: true,
    });
    assert!(a.is_same_state(&b), "discriminant-only helper still true");
    assert_ne!(a, b, "PartialEq must see centered/shown_on_top");

    let c = WindowState::Fullscreen(FullscreenStateConfig {
      maximized: true,
      shown_on_top: false,
    });
    let d = WindowState::Fullscreen(FullscreenStateConfig {
      maximized: false,
      shown_on_top: true,
    });
    assert!(c.is_same_state(&d));
    assert_ne!(c, d);
  }

  #[test]
  fn load_guard_should_use_full_equality_not_is_same_state() {
    // Documents the restore contract: same-variant config changes must apply.
    let live = WindowState::Floating(FloatingStateConfig {
      centered: true,
      shown_on_top: false,
    });
    let target = WindowState::Floating(FloatingStateConfig {
      centered: false,
      shown_on_top: true,
    });
    // Old (buggy) guard:
    let old_would_skip = live.is_same_state(&target);
    // New guard:
    let new_should_update = live != target;
    assert!(old_would_skip);
    assert!(new_should_update);
  }
}

