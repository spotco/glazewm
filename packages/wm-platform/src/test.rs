#![feature(iterator_try_collect)]

#[macro_use]
extern crate libtest_mimic_collect;

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

pub fn main() {
  #[cfg(target_os = "windows")]
  if let Some(mode) = std::env::var_os("GLAZEWM_Z_ORDER_HELPER") {
    run_z_order_helper(&mode.to_string_lossy());
    return;
  }

  // Due to macOS requiring the main thread for some UI APIs, these
  // tests must execute on the main thread. Until this is natively
  // supported via cargo's test harness, we use `libtest_mimic_collect`.
  //
  // To run these tests, run `cargo test <...args> -- --test-threads=1`.
  //
  // Ref: https://github.com/rust-lang/rust/issues/104053
  libtest_mimic_collect::TestCollection::run();
}

#[cfg(target_os = "windows")]
fn run_z_order_helper(mode: &str) {
  use std::{
    io::Write, os::windows::ffi::OsStrExt, thread, time::Duration,
  };

  use windows::{
    core::PCWSTR,
    Win32::{
      Foundation::{HWND, LPARAM, LRESULT, WPARAM},
      UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WINDOW_EX_STYLE,
        WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
      },
    },
  };

  fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
      .encode_wide()
      .chain(Some(0))
      .collect()
  }

  unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
  }

  let class_name =
    wide(&format!("GlazeWmNonPumpingHelper{}", std::process::id()));
  let class = WNDCLASSW {
    lpfnWndProc: Some(window_proc),
    lpszClassName: PCWSTR(class_name.as_ptr()),
    ..Default::default()
  };
  let atom = unsafe { RegisterClassW(&class) };
  assert_ne!(atom, 0, "register non-pumping helper class");

  if mode == "window" {
    let title = wide("GlazeWM non-pumping z-order helper");
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        80,
        80,
        240,
        120,
        None,
        None,
        None,
        None,
      )
    };
    assert_ne!(hwnd.0, 0, "create non-pumping helper window");
    println!("{}", hwnd.0);
    std::io::stdout().flush().expect("flush helper HWND");
    loop {
      thread::sleep(Duration::from_secs(60));
    }
  }

  let hwnd = std::env::var("GLAZEWM_Z_ORDER_HWND")
    .expect("helper HWND")
    .parse::<isize>()
    .expect("parse helper HWND");
  match mode {
    "set-z-order" => {
      crate::platform_impl::NativeWindow::new(hwnd)
        .set_z_order(&crate::WindowZOrder::Normal)
        .expect("set z-order");
    }
    "reorder-z-order" => {
      crate::platform_impl::reorder_z_order(&[crate::WindowId(hwnd)])
        .expect("reorder z-order");
    }
    _ => panic!("unknown z-order helper mode: {mode}"),
  }
}
