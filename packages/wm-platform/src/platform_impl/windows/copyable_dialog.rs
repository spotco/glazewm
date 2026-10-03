use std::{cell::Cell, ffi::c_void};

use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT},
    System::{
      DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
      },
      Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    },
    UI::{
      Input::KeyboardAndMouse::SetFocus,
      WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetClientRect, GetDlgItem, GetMessageW, GetSystemMetrics,
        GetWindowTextLengthW, GetWindowTextW, MessageBoxW,
        PostQuitMessage, RegisterClassW, SendMessageW, SetWindowTextW,
        TranslateMessage, ES_AUTOHSCROLL, ES_READONLY, HMENU,
        MB_ICONINFORMATION, MB_OK, MB_SYSTEMMODAL, MSG, SM_CXSCREEN,
        SM_CYSCREEN, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
        WM_DESTROY, WM_QUIT, WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION,
        WS_CHILD, WS_EX_DLGMODALFRAME, WS_EX_TOPMOST, WS_OVERLAPPED,
        WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
      },
    },
  },
};

const ID_LABEL: i32 = 100;
const ID_EDIT: i32 = 101;
const ID_COPY: i32 = 102;
const ID_CLOSE: i32 = 103;
const EM_SETSEL: u32 = 0x00B1;
const CF_UNICODETEXT: u32 = 13;

thread_local! {
  static DIALOG_ALIVE: Cell<bool> = const { Cell::new(false) };
}

/// Modal dialog with a read-only path field, Copy, and Close.
///
/// Must run on a thread that already pumps window messages. Does not post
/// `WM_QUIT` of its own; a quit meant for the outer loop is reposted.
pub(crate) fn show_copyable_text(owner: isize, title: &str, text: &str) {
  let _ = set_clipboard_text(text);
  if !show_dialog(owner, title, text) {
    fallback_message_box(title, text);
  }
}

fn show_dialog(owner: isize, title: &str, text: &str) -> bool {
  ensure_class();
  let title_wide = wide(title);
  let text_wide = wide(text);
  let owner_hwnd = HWND(owner);

  let width = 640i32;
  let height = 168i32;
  let (x, y) = centered_origin(width, height);
  let hwnd = unsafe {
    CreateWindowExW(
      WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
      w!("GlazeWM.StateDumpDialog"),
      PCWSTR(title_wide.as_ptr()),
      WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
      x,
      y,
      width,
      height,
      owner_hwnd,
      HMENU(0),
      HINSTANCE(0),
      None,
    )
  };
  if hwnd.0 == 0 {
    return false;
  }

  let mut client = RECT::default();
  unsafe {
    let _ = GetClientRect(hwnd, std::ptr::from_mut(&mut client));
  }
  let client_width = (client.right - client.left).max(320);
  let client_height = (client.bottom - client.top).max(80);
  let edit = create_child(
    hwnd,
    w!("EDIT"),
    edit_style(),
    12,
    36,
    client_width - 24,
    24,
    ID_EDIT,
  );
  let label = create_child(
    hwnd,
    w!("STATIC"),
    WS_CHILD | WS_VISIBLE,
    12,
    12,
    client_width - 24,
    18,
    ID_LABEL,
  );
  let copy = create_child(
    hwnd,
    w!("BUTTON"),
    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
    client_width - 184,
    client_height - 40,
    80,
    26,
    ID_COPY,
  );
  let close = create_child(
    hwnd,
    w!("BUTTON"),
    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
    client_width - 96,
    client_height - 40,
    84,
    26,
    ID_CLOSE,
  );
  if edit.0 == 0 || copy.0 == 0 || close.0 == 0 || label.0 == 0 {
    unsafe {
      let _ = DestroyWindow(hwnd);
    }
    return false;
  }

  let prompt = wide("Saved. Select the path, or use Copy.");
  let copy_label = wide("Copy");
  let close_label = wide("Close");
  unsafe {
    let _ = SetWindowTextW(label, PCWSTR(prompt.as_ptr()));
    let _ = SetWindowTextW(edit, PCWSTR(text_wide.as_ptr()));
    let _ = SetWindowTextW(copy, PCWSTR(copy_label.as_ptr()));
    let _ = SetWindowTextW(close, PCWSTR(close_label.as_ptr()));
    apply_font(label);
    apply_font(edit);
    apply_font(copy);
    apply_font(close);
    SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
    let _ = SetFocus(edit);
  }

  DIALOG_ALIVE.set(true);
  pump_until_destroyed();
  true
}

fn pump_until_destroyed() {
  let mut msg = MSG::default();
  loop {
    let got = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
    if !got.as_bool() || msg.message == WM_QUIT {
      let code = i32::try_from(msg.wParam.0).unwrap_or(0);
      unsafe { PostQuitMessage(code) };
      break;
    }
    unsafe {
      let _ = TranslateMessage(&raw const msg);
      DispatchMessageW(&raw const msg);
    }
    if !DIALOG_ALIVE.get() {
      break;
    }
  }
}

fn edit_style() -> WINDOW_STYLE {
  WINDOW_STYLE(
    (WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER).0
      | u32::try_from(ES_READONLY).unwrap_or(0)
      | u32::try_from(ES_AUTOHSCROLL).unwrap_or(0),
  )
}

#[allow(clippy::too_many_arguments)]
fn create_child(
  parent: HWND,
  class_name: PCWSTR,
  style: WINDOW_STYLE,
  x: i32,
  y: i32,
  width: i32,
  height: i32,
  id: i32,
) -> HWND {
  unsafe {
    CreateWindowExW(
      WINDOW_EX_STYLE::default(),
      class_name,
      w!(""),
      style,
      x,
      y,
      width,
      height,
      parent,
      HMENU(id as isize),
      HINSTANCE(0),
      None,
    )
  }
}

fn apply_font(hwnd: HWND) {
  if hwnd.0 == 0 {
    return;
  }
  let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
  unsafe {
    SendMessageW(
      hwnd,
      WM_SETFONT,
      WPARAM(usize::try_from(font.0).unwrap_or(0)),
      LPARAM(1),
    );
  }
}

fn centered_origin(width: i32, height: i32) -> (i32, i32) {
  let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(width);
  let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(height);
  ((screen_w - width) / 2, (screen_h - height) / 2)
}

fn ensure_class() {
  use std::sync::Once;
  static REGISTER: Once = Once::new();
  REGISTER.call_once(|| {
    let class = WNDCLASSW {
      lpszClassName: w!("GlazeWM.StateDumpDialog"),
      lpfnWndProc: Some(dialog_proc),
      ..Default::default()
    };
    unsafe {
      RegisterClassW(std::ptr::from_ref(&class));
    }
  });
}

unsafe extern "system" fn dialog_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match msg {
    WM_COMMAND => {
      let id = i32::try_from(wparam.0 & 0xFFFF).unwrap_or(0);
      if id == ID_COPY {
        copy_edit(hwnd);
      } else if id == ID_CLOSE {
        let _ = DestroyWindow(hwnd);
      }
      LRESULT(0)
    }
    WM_CLOSE => {
      let _ = DestroyWindow(hwnd);
      LRESULT(0)
    }
    WM_DESTROY => {
      DIALOG_ALIVE.set(false);
      LRESULT(0)
    }
    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
  }
}

fn copy_edit(parent: HWND) {
  let edit = unsafe { GetDlgItem(parent, ID_EDIT) };
  if edit.0 == 0 {
    return;
  }
  let len = unsafe { GetWindowTextLengthW(edit) };
  if len < 0 {
    return;
  }
  let mut buf = vec![0u16; usize::try_from(len).unwrap_or(0) + 1];
  let copied = unsafe { GetWindowTextW(edit, &mut buf) };
  if copied < 0 {
    return;
  }
  let end = usize::try_from(copied).unwrap_or(0).min(buf.len());
  let text = String::from_utf16_lossy(&buf[..end]);
  let _ = set_clipboard_text(&text);
}

fn set_clipboard_text(text: &str) -> bool {
  let wide_text = wide(text);
  let bytes = wide_text.len().saturating_mul(2);
  unsafe {
    if OpenClipboard(None).is_err() {
      return false;
    }
    let _ = EmptyClipboard();
    let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
      let _ = CloseClipboard();
      return false;
    };
    let locked = GlobalLock(handle);
    if locked.is_null() {
      let _ = CloseClipboard();
      return false;
    }
    std::ptr::copy_nonoverlapping(
      wide_text.as_ptr().cast::<c_void>(),
      locked,
      bytes,
    );
    let _ = GlobalUnlock(handle);
    let clipboard_handle = HANDLE(handle.0 as isize);
    if SetClipboardData(CF_UNICODETEXT, clipboard_handle).is_err() {
      let _ = CloseClipboard();
      return false;
    }
    CloseClipboard().is_ok()
  }
}

fn fallback_message_box(title: &str, text: &str) {
  let title_wide = wide(title);
  let message = format!(
    "Saved state dump.\n\n{text}\n\nThe path was copied to the clipboard."
  );
  let message_wide = wide(&message);
  unsafe {
    MessageBoxW(
      None,
      PCWSTR(message_wide.as_ptr()),
      PCWSTR(title_wide.as_ptr()),
      MB_OK | MB_ICONINFORMATION | MB_SYSTEMMODAL,
    );
  }
}

fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(Some(0)).collect()
}
