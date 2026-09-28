use tracing::info;
use wm_common::{try_warn, WindowState};
use wm_platform::NativeWindow;

use crate::{
  commands::{
    general::restore_show_desktop_layout, window::update_window_state,
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_minimize_ended(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  // Update the window's state to not be minimized.
  if let Some(window) = found_window {
    let is_minimized = try_warn!(window.native().is_minimized());

    window.update_native_properties(|properties| {
      properties.is_minimized = is_minimized;
    });

    if !is_minimized && state.is_show_desktop_minimized(window.id()) {
      state.clear_show_desktop_minimized(window.id());

      let target_state = window
        .prev_state()
        .filter(|state| *state != WindowState::Minimized)
        .unwrap_or(WindowState::default_from_config(&config.value));

      if window.state() == WindowState::Minimized {
        info!("Show Desktop minimize ended: {window}");
        update_window_state(window.clone(), target_state, state, config)?;
      }

      if state.show_desktop_minimized_count() == 0 {
        restore_show_desktop_layout(state, config)?;
      }
    } else if !is_minimized && window.state() == WindowState::Minimized {
      info!("Window minimize ended: {window}");

      let target_state = window
        .prev_state()
        .unwrap_or(WindowState::default_from_config(&config.value));

      update_window_state(window.clone(), target_state, state, config)?;
    }
  }

  Ok(())
}
