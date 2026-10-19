//! Helpers for tests that exercise Windows-native window identity.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::{
  core::PCWSTR,
  Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
      UnregisterClassW, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
      WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    },
  },
};
use wm_platform::{NativeWindow, NativeWindowWindowsExt};

static NEXT_TEST_WINDOW: AtomicU64 = AtomicU64::new(1);

unsafe extern "system" fn test_window_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  DefWindowProcW(hwnd, message, wparam, lparam)
}

/// A real off-screen HWND for tests that need current Win32 identity.
///
/// The window is visible without activating and uses tool-window styling,
/// so it remains discoverable by monitor and owner APIs without appearing
/// in the taskbar or covering the user's desktop.
pub(crate) struct TestNativeWindow {
  hwnd: HWND,
  class_name: Vec<u16>,
  native: NativeWindow,
}

impl TestNativeWindow {
  pub(crate) fn new() -> Self {
    let id = NEXT_TEST_WINDOW.fetch_add(1, Ordering::Relaxed);
    let class = format!("GlazeWmNativeTest{}-{id}", std::process::id());
    let class_name =
      class.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let class = WNDCLASSW {
      lpfnWndProc: Some(test_window_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register native test window class");

    let title = format!("GlazeWM native test window {id}")
      .encode_utf16()
      .chain(Some(0))
      .collect::<Vec<_>>();
    let hwnd = unsafe {
      CreateWindowExW(
        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        -32_000,
        -32_000,
        160,
        100,
        None,
        None,
        None,
        None,
      )
    };
    assert_ne!(hwnd.0, 0, "create native test window");

    Self {
      hwnd,
      class_name,
      native: NativeWindow::from_handle(hwnd.0),
    }
  }

  pub(crate) fn native(&self) -> &NativeWindow {
    &self.native
  }
}

impl Drop for TestNativeWindow {
  fn drop(&mut self) {
    unsafe {
      let _ = DestroyWindow(self.hwnd);
      let _ = UnregisterClassW(PCWSTR(self.class_name.as_ptr()), None);
    }
  }
}
