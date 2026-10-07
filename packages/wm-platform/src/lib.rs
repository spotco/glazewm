#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![feature(iterator_try_collect)]

mod dispatcher;
mod display;
mod display_listener;
mod error;
mod event_loop;
mod keybinding_listener;
mod models;
mod mouse_listener;
mod native_window;
mod platform_event;
mod platform_impl;
mod single_instance;
mod thread_bound;
mod window_listener;

pub use dispatcher::*;
pub use display::*;
pub use display_listener::*;
pub use error::*;
pub use event_loop::*;
pub use keybinding_listener::*;
pub use models::*;
pub use mouse_listener::*;
pub use native_window::*;
pub use platform_event::*;
pub use single_instance::*;
pub use thread_bound::*;
pub use window_listener::*;

/// Registers a sink for foreign-window operation timing lines.
///
/// Windows emits `native-op begin` / `native-op end` through this sink.
/// An unmatched begin identifies the call that blocked the WM thread.
/// Other platforms accept the registration and do not emit lines.
pub fn set_native_op_logger(logger: fn(&str)) {
  #[cfg(target_os = "windows")]
  platform_impl::set_native_op_logger(logger);
  #[cfg(not(target_os = "windows"))]
  let _ = logger;
}

/// Returns whether the current Windows process has a full elevated token.
#[cfg(target_os = "windows")]
pub fn is_process_elevated() -> Result<bool> {
  platform_impl::is_process_elevated()
}

/// Starts the current executable again with the Windows `runas` verb.
///
/// Returns `true` when the elevated process was launched, and `false` when
/// the user dismissed the UAC prompt.
#[cfg(target_os = "windows")]
pub fn relaunch_current_process_as_admin(args: &[String]) -> Result<bool> {
  platform_impl::relaunch_current_process_as_admin(args)
}
// TODO: Avoid exposing `windows` crate types in the public API.
#[cfg(target_os = "windows")]
pub use windows::Win32::UI::WindowsAndMessaging::{
  SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED,
  SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOSENDCHANGING, SWP_NOZORDER,
  WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE,
  WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX,
};
