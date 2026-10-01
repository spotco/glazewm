use std::{
  cell::RefCell,
  collections::HashSet,
  sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Mutex, OnceLock,
  },
  thread,
  time::{Duration, Instant},
};

use tokio::runtime::Handle;
use tracing::{debug, warn};
use windows::{
  core::PWSTR,
  Win32::{
    Foundation::{CloseHandle, BOOL, HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Dwm::{
      DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
      DWMWA_CLOAK, DWMWA_CLOAKED, DWMWA_COLOR_NONE,
      DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE,
      DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
    },
    System::Threading::{
      GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
      PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::{
      Input::KeyboardAndMouse::{
        IsWindowEnabled, SendInput, INPUT, INPUT_0, INPUT_MOUSE,
        MOUSEINPUT,
      },
      WindowsAndMessaging::{
        EnumWindows, GetAncestor, GetClassNameW, GetDesktopWindow,
        GetForegroundWindow, GetLayeredWindowAttributes, GetParent,
        GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, IsZoomed, SendMessageTimeoutW,
        SendNotifyMessageW, SetForegroundWindow,
        SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPlacement,
        SetWindowPos, ShowWindowAsync, WindowFromPoint, GA_ROOT,
        GWL_EXSTYLE, GWL_STYLE, GW_OWNER, HWND_NOTOPMOST, HWND_TOP,
        HWND_TOPMOST, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA,
        LWA_COLORKEY, SET_WINDOW_POS_FLAGS, SMTO_ABORTIFHUNG, SMTO_NORMAL,
        SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOCOPYBITS, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,
        SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE,
        SW_RESTORE, SW_SHOWNA, WINDOWPLACEMENT, WINDOW_EX_STYLE,
        WINDOW_STYLE, WM_CLOSE, WM_NULL, WPF_ASYNCWINDOWPLACEMENT,
        WS_CHILD, WS_DLGFRAME, WS_EX_APPWINDOW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        WS_THICKFRAME,
      },
    },
  },
};

use super::com::{IApplicationView, COM_INIT};
use crate::{
  Color, CornerStyle, Delta, Dispatcher, LengthValue,
  NativeWindowDebugInfo, OpacityValue, Point, Rect, RectDelta, WindowId,
  WindowZOrder,
};

/// Magic number used to identify programmatic mouse inputs from our own
/// process.
pub(crate) const FOREGROUND_INPUT_IDENTIFIER: u32 = 6379;

static Z_ORDER_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bound for the `WM_NULL` probe used before a synchronous style write.
///
/// `IsHungAppWindow` stays false for several seconds after a debugger
/// suspends a thread. `SendMessageTimeoutW` returns when this elapses.
const FOREIGN_GUI_PROBE_TIMEOUT_MS: u32 = 50;

/// How long the caller waits for a foreign style or layered write.
///
/// The write itself runs on another thread. A `WM_NULL` probe is not
/// this bound: the target can answer `WM_NULL` and then stall in
/// `WM_STYLECHANGING`.
const FOREIGN_GUI_CALL_TIMEOUT: Duration = Duration::from_millis(50);

static NATIVE_OP_LOGGER: Mutex<Option<fn(&str)>> = Mutex::new(None);

/// HWNDs with a style or layered write that has not returned yet.
///
/// A timed-out worker stays here until the foreign call finishes, so
/// a later attempt does not start a second blocked thread.
fn foreign_gui_calls_in_flight() -> &'static Mutex<HashSet<isize>> {
  static CALLS: OnceLock<Mutex<HashSet<isize>>> = OnceLock::new();
  CALLS.get_or_init(|| Mutex::new(HashSet::new()))
}

thread_local! {
  static NATIVE_OP_RESULTS: RefCell<Vec<Option<&'static str>>> =
    const { RefCell::new(Vec::new()) };
}

/// Registers the sink for `native-op` timing lines.
///
/// The WM points this at `layout.log`. An unmatched `native-op begin`
/// is the call that is still blocked on a foreign window.
pub(crate) fn set_native_op_logger(logger: fn(&str)) {
  if let Ok(mut guard) = NATIVE_OP_LOGGER.lock() {
    *guard = Some(logger);
  }
}

fn log_native_op(line: &str) {
  debug!("{line}");
  let logger = NATIVE_OP_LOGGER.lock().ok().and_then(|guard| *guard);
  if let Some(logger) = logger {
    logger(line);
  }
}

fn hwnd_token(hwnd: HWND) -> String {
  #[allow(clippy::cast_sign_loss)]
  let value = hwnd.0 as usize;
  format!("{value:#x}")
}

struct NativeOpSpan {
  hwnd: String,
  op: &'static str,
  started: Instant,
}

impl Drop for NativeOpSpan {
  fn drop(&mut self) {
    let label = NATIVE_OP_RESULTS
      .with(|stack| stack.borrow_mut().pop().flatten().unwrap_or("ok"));
    log_native_op(&format!(
      "native-op end hwnd={} op={} elapsed_ms={} result={label}",
      self.hwnd,
      self.op,
      self.started.elapsed().as_millis()
    ));
  }
}

/// Logs a paired begin/end line around `body`.
///
/// The end line is written when `body` returns. A missing end in
/// `layout.log` means `body` is still inside a foreign call.
fn timed_native_op<T>(
  hwnd: HWND,
  op: &'static str,
  body: impl FnOnce() -> T,
) -> T {
  let hwnd = hwnd_token(hwnd);
  log_native_op(&format!("native-op begin hwnd={hwnd} op={op}"));
  NATIVE_OP_RESULTS.with(|stack| stack.borrow_mut().push(None));
  let _span = NativeOpSpan {
    hwnd,
    op,
    started: Instant::now(),
  };
  body()
}

fn mark_native_op_result(label: &'static str) {
  NATIVE_OP_RESULTS.with(|stack| {
    if let Some(slot) = stack.borrow_mut().last_mut() {
      *slot = Some(label);
    }
  });
}

/// Clears the in-flight slot when a foreign style write returns.
struct ForeignGuiCallGuard(isize);

impl Drop for ForeignGuiCallGuard {
  fn drop(&mut self) {
    if let Ok(mut in_flight) = foreign_gui_calls_in_flight().lock() {
      in_flight.remove(&self.0);
    }
  }
}

/// Runs `work` away from the caller and waits at most
/// [`FOREIGN_GUI_CALL_TIMEOUT`].
///
/// Returns `None` when `work` does not finish in time, or when this
/// `HWND` already has a call in flight. The caller, which is the WM
/// thread in production, does not stay inside `SetWindowLongPtrW` or
/// `SetLayeredWindowAttributes`.
fn run_bounded_foreign_gui_call<T>(
  hwnd: isize,
  work: impl FnOnce() -> T + Send + 'static,
) -> Option<T>
where
  T: Send + 'static,
{
  {
    let Ok(mut in_flight) = foreign_gui_calls_in_flight().lock() else {
      return None;
    };
    if !in_flight.insert(hwnd) {
      return None;
    }
  }

  let (sender, receiver) = mpsc::channel();
  let spawned = thread::Builder::new()
    .name("glazewm-foreign-gui".to_string())
    .spawn(move || {
      let _guard = ForeignGuiCallGuard(hwnd);
      let value = work();
      let _ = sender.send(value);
    });
  if spawned.is_err() {
    if let Ok(mut in_flight) = foreign_gui_calls_in_flight().lock() {
      in_flight.remove(&hwnd);
    }
    return None;
  }

  receiver.recv_timeout(FOREIGN_GUI_CALL_TIMEOUT).ok()
}

/// Platform-specific implementation of [`NativeWindow`].
#[derive(Clone, Debug)]
pub(crate) struct NativeWindow {
  pub(crate) handle: isize,
}

impl NativeWindow {
  /// Creates an instance of `NativeWindow`.
  #[must_use]
  pub(crate) fn new(handle: isize) -> Self {
    Self { handle }
  }

  /// Implements [`NativeWindow::id`].
  #[must_use]
  pub(crate) fn id(&self) -> WindowId {
    WindowId(self.handle)
  }

  /// Implements [`NativeWindow::title`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn title(&self) -> crate::Result<String> {
    let mut text: [u16; 512] = [0; 512];
    let length = unsafe { GetWindowTextW(self.hwnd(), &mut text) };

    #[allow(clippy::cast_sign_loss)]
    Ok(String::from_utf16_lossy(&text[..length as usize]))
  }

  /// Implements [`NativeWindow::process_path`].
  pub(crate) fn process_path(&self) -> crate::Result<String> {
    let mut process_id = 0u32;
    unsafe {
      GetWindowThreadProcessId(self.hwnd(), Some(&raw mut process_id));
    }

    let process_handle = unsafe {
      OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
    }?;

    let mut buffer = [0u16; 256];
    let mut length = u32::try_from(buffer.len())?;

    unsafe {
      let query_res = QueryFullProcessImageNameW(
        process_handle,
        PROCESS_NAME_WIN32,
        PWSTR(buffer.as_mut_ptr()),
        &raw mut length,
      );

      // Always close the process handle regardless of the query result.
      CloseHandle(process_handle)?;

      query_res
    }?;

    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
  }

  /// Implements [`NativeWindow::process_name`].
  pub(crate) fn process_name(&self) -> crate::Result<String> {
    let exe_path = self.process_path()?;

    exe_path
      .split('\\')
      .next_back()
      .map(|file_name| {
        file_name.split('.').next().unwrap_or(file_name).to_string()
      })
      .ok_or_else(|| {
        crate::Error::Platform("Failed to parse process name.".to_string())
      })
  }

  /// Implements [`NativeWindow::frame`].
  pub(crate) fn frame(&self) -> crate::Result<Rect> {
    let mut rect = RECT::default();

    let dwm_res = unsafe {
      #[allow(clippy::cast_possible_truncation)]
      DwmGetWindowAttribute(
        self.hwnd(),
        DWMWA_EXTENDED_FRAME_BOUNDS,
        std::ptr::from_mut(&mut rect).cast(),
        std::mem::size_of::<RECT>() as u32,
      )
    };

    if let Ok(()) = dwm_res {
      Ok(Rect::from_ltrb(
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
      ))
    } else {
      warn!("Failed to get window's frame position. Falling back to border position.");
      self.frame_with_shadows()
    }
  }

  /// Implements [`NativeWindow::position`].
  pub(crate) fn position(&self) -> crate::Result<(f64, f64)> {
    let frame = self.frame()?;
    Ok((f64::from(frame.left), f64::from(frame.top)))
  }

  /// Implements [`NativeWindow::size`].
  pub(crate) fn size(&self) -> crate::Result<(f64, f64)> {
    let frame = self.frame()?;
    Ok((f64::from(frame.width()), f64::from(frame.height())))
  }

  /// Implements [`NativeWindow::is_valid`].
  pub(crate) fn is_valid(&self) -> bool {
    unsafe { IsWindow(self.hwnd()) }.as_bool()
  }

  /// Implements [`NativeWindow::is_visible`].
  pub(crate) fn is_visible(&self) -> crate::Result<bool> {
    let is_visible = unsafe { IsWindowVisible(self.hwnd()) }.as_bool();

    Ok(is_visible && !self.is_cloaked()?)
  }

  /// Implements [`NativeWindow::is_minimized`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_minimized(&self) -> crate::Result<bool> {
    Ok(unsafe { IsIconic(self.hwnd()) }.as_bool())
  }

  /// Implements [`NativeWindow::is_maximized`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_maximized(&self) -> crate::Result<bool> {
    Ok(unsafe { IsZoomed(self.hwnd()) }.as_bool())
  }

  /// Implements [`NativeWindow::is_resizable`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_resizable(&self) -> crate::Result<bool> {
    Ok(self.has_window_style(WS_THICKFRAME))
  }

  /// Implements [`NativeWindow::is_desktop_window`].
  #[allow(clippy::unnecessary_wraps)]
  pub(crate) fn is_desktop_window(&self) -> crate::Result<bool> {
    Ok(*self == desktop_window())
  }

  /// Implements [`NativeWindow::set_frame`].
  pub(crate) fn set_frame(&self, rect: &Rect) -> crate::Result<()> {
    unsafe {
      SetWindowPos(
        self.hwnd(),
        HWND_NOTOPMOST,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        SWP_NOACTIVATE
          | SWP_NOZORDER
          | SWP_NOCOPYBITS
          | SWP_NOSENDCHANGING
          | SWP_ASYNCWINDOWPOS
          | SWP_FRAMECHANGED,
      )
    }?;

    Ok(())
  }

  /// Implements [`NativeWindow::resize`].
  pub(crate) fn resize(
    &self,
    width: i32,
    height: i32,
  ) -> crate::Result<()> {
    unsafe {
      SetWindowPos(
        self.hwnd(),
        HWND_NOTOPMOST,
        0,
        0,
        width,
        height,
        SWP_NOACTIVATE
          | SWP_NOZORDER
          | SWP_NOMOVE
          | SWP_NOCOPYBITS
          | SWP_NOSENDCHANGING
          | SWP_ASYNCWINDOWPOS
          | SWP_FRAMECHANGED,
      )
    }?;

    Ok(())
  }

  /// Implements [`NativeWindow::reposition`].
  pub(crate) fn reposition(&self, x: i32, y: i32) -> crate::Result<()> {
    unsafe {
      SetWindowPos(
        self.hwnd(),
        HWND_NOTOPMOST,
        x,
        y,
        0,
        0,
        SWP_NOACTIVATE
          | SWP_NOZORDER
          | SWP_NOSIZE
          | SWP_NOCOPYBITS
          | SWP_NOSENDCHANGING
          | SWP_ASYNCWINDOWPOS
          | SWP_FRAMECHANGED,
      )
    }?;

    Ok(())
  }

  /// Implements [`NativeWindow::minimize`].
  pub(crate) fn minimize(&self) -> crate::Result<()> {
    unsafe { ShowWindowAsync(self.hwnd(), SW_MINIMIZE).ok() }?;
    Ok(())
  }

  /// Implements [`NativeWindow::maximize`].
  pub(crate) fn maximize(&self) -> crate::Result<()> {
    unsafe { ShowWindowAsync(self.hwnd(), SW_MAXIMIZE).ok() }?;
    Ok(())
  }

  /// Implements [`NativeWindow::focus`].
  pub(crate) fn focus(&self) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "focus", || self.focus_inner())
  }

  fn focus_inner(&self) -> crate::Result<()> {
    let input = [INPUT {
      r#type: INPUT_MOUSE,
      Anonymous: INPUT_0 {
        mi: MOUSEINPUT {
          dwExtraInfo: FOREGROUND_INPUT_IDENTIFIER as usize,
          ..Default::default()
        },
      },
    }];

    // Bypass restriction for setting the foreground window by sending an
    // input to our own process first.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    unsafe {
      SendInput(&input, std::mem::size_of::<INPUT>() as i32)
    };

    // Set as the foreground window.
    unsafe { SetForegroundWindow(self.hwnd()) }.ok()?;

    Ok(())
  }

  /// Implements [`NativeWindow::close`].
  pub(crate) fn close(&self) -> crate::Result<()> {
    unsafe { SendNotifyMessageW(self.hwnd(), WM_CLOSE, None, None) }?;
    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::hwnd`].
  pub(crate) fn hwnd(&self) -> HWND {
    HWND(self.handle)
  }

  /// Implements [`NativeWindowWindowsExt::class_name`].
  pub(crate) fn class_name(&self) -> crate::Result<String> {
    let mut buffer = [0u16; 256];
    let result = unsafe { GetClassNameW(self.hwnd(), &mut buffer) };

    if result == 0 {
      return Err(windows::core::Error::from_win32().into());
    }

    #[allow(clippy::cast_sign_loss)]
    let class_name = String::from_utf16_lossy(&buffer[..result as usize]);
    Ok(class_name)
  }

  /// Implements [`NativeWindowWindowsExt::frame_with_shadows`].
  pub(crate) fn frame_with_shadows(&self) -> crate::Result<Rect> {
    let mut rect = RECT::default();

    unsafe {
      GetWindowRect(self.hwnd(), std::ptr::from_mut(&mut rect).cast())
    }?;

    Ok(Rect::from_ltrb(
      rect.left,
      rect.top,
      rect.right,
      rect.bottom,
    ))
  }

  /// Implements [`NativeWindowWindowsExt::shadow_borders`].
  // TODO: Return tuple of (left, top, right, bottom) instead of
  // `RectDelta`.
  pub(crate) fn shadow_borders(&self) -> crate::Result<RectDelta> {
    let border_pos = self.frame_with_shadows()?;
    let frame_pos = self.frame()?;

    Ok(RectDelta::new(
      LengthValue::from_px(frame_pos.left - border_pos.left),
      LengthValue::from_px(frame_pos.top - border_pos.top),
      LengthValue::from_px(border_pos.right - frame_pos.right),
      LengthValue::from_px(border_pos.bottom - frame_pos.bottom),
    ))
  }

  /// Implements [`NativeWindowWindowsExt::has_owner_window`].
  pub(crate) fn has_owner_window(&self) -> bool {
    unsafe { GetWindow(self.hwnd(), GW_OWNER) }.0 != 0
  }

  /// Implements [`NativeWindowWindowsExt::has_window_style`].
  pub(crate) fn has_window_style(&self, style: WINDOW_STYLE) -> bool {
    let current_style =
      unsafe { GetWindowLongPtrW(self.hwnd(), GWL_STYLE) };

    #[allow(clippy::cast_possible_wrap)]
    let style = style.0 as isize;
    (current_style & style) != 0
  }

  /// Implements [`NativeWindowWindowsExt::has_window_style_ex`].
  pub(crate) fn has_window_style_ex(
    &self,
    style: WINDOW_EX_STYLE,
  ) -> bool {
    let current_style =
      unsafe { GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) };

    #[allow(clippy::cast_possible_wrap)]
    let style = style.0 as isize;
    (current_style & style) != 0
  }

  /// Implements [`NativeWindowWindowsExt::set_window_pos`].
  pub(crate) fn set_window_pos(
    &self,
    z_order: &WindowZOrder,
    rect: &Rect,
    flags: SET_WINDOW_POS_FLAGS,
  ) -> crate::Result<()> {
    self.set_window_pos_inner(z_order, rect, flags)
  }

  fn set_window_pos_inner(
    &self,
    z_order: &WindowZOrder,
    rect: &Rect,
    flags: SET_WINDOW_POS_FLAGS,
  ) -> crate::Result<()> {
    let z_order_hwnd = match z_order {
      WindowZOrder::TopMost => HWND_TOPMOST,
      WindowZOrder::Top => HWND_TOP,
      WindowZOrder::Normal => HWND_NOTOPMOST,
      WindowZOrder::AfterWindow(window_id) => HWND(window_id.0),
    };

    unsafe {
      SetWindowPos(
        self.hwnd(),
        z_order_hwnd,
        rect.x(),
        rect.y(),
        rect.width(),
        rect.height(),
        flags,
      )
    }?;

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::show`].
  pub(crate) fn show(&self) -> crate::Result<()> {
    unsafe { ShowWindowAsync(self.hwnd(), SW_SHOWNA) }.ok()?;
    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::hide`].
  pub(crate) fn hide(&self) -> crate::Result<()> {
    unsafe { ShowWindowAsync(self.hwnd(), SW_HIDE) }.ok()?;
    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::restore`].
  pub(crate) fn restore(
    &self,
    outer_frame: Option<&Rect>,
  ) -> crate::Result<()> {
    self.restore_inner(outer_frame)
  }

  fn restore_inner(
    &self,
    outer_frame: Option<&Rect>,
  ) -> crate::Result<()> {
    match outer_frame {
      None => {
        unsafe { ShowWindowAsync(self.hwnd(), SW_RESTORE) }.ok()?;
        Ok(())
      }
      Some(rect) => {
        let placement = WINDOWPLACEMENT {
          #[allow(clippy::cast_possible_truncation)]
          length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
          flags: WPF_ASYNCWINDOWPLACEMENT,
          showCmd: SW_RESTORE.0 as u32,
          rcNormalPosition: RECT {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
          },
          ..Default::default()
        };

        unsafe { SetWindowPlacement(self.hwnd(), &raw const placement) }?;
        Ok(())
      }
    }
  }

  /// Implements [`NativeWindowWindowsExt::set_cloaked`].
  ///
  /// # Shell COM
  ///
  /// `set_cloaked`, `mark_fullscreen`, and `set_taskbar_visibility`
  /// are synchronous RPC into Explorer / Immersive Shell. They do
  /// not wait on the target window's GUI thread, so a
  /// debugger-suspended debuggee does not block them. A wedged
  /// Explorer still can, and that is not closed here. Isolating the
  /// RPC would only move the stall. It is a separate follow-up. The
  /// `native-op` lines identify it.
  pub(crate) fn set_cloaked(&self, cloaked: bool) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_cloaked", || {
      self.set_cloaked_inner(cloaked)
    })
  }

  fn set_cloaked_inner(&self, cloaked: bool) -> crate::Result<()> {
    COM_INIT.with(|com_init| -> crate::Result<()> {
      com_init.borrow_mut().with_retry(|com| {
        let view_collection = com.application_view_collection()?;

        let mut view: Option<IApplicationView> = None;
        unsafe {
          view_collection.get_view_for_hwnd(self.hwnd().0, &raw mut view)
        }
        .ok()?;

        let view = view.ok_or_else(|| {
          crate::Error::Platform(
            "Unable to get application view by window handle.".to_string(),
          )
        })?;

        // Ref: https://github.com/Ciantic/AltTabAccessor/issues/1#issuecomment-1426877843
        unsafe { view.set_cloak(1, if cloaked { 2 } else { 0 }) }
          .ok()
          .map_err(|_| {
            crate::Error::Platform("Failed to cloak window.".to_string())
          })
      })
    })
  }

  /// Implements [`NativeWindowWindowsExt::mark_fullscreen`].
  pub(crate) fn mark_fullscreen(
    &self,
    fullscreen: bool,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "mark_fullscreen", || {
      self.mark_fullscreen_inner(fullscreen)
    })
  }

  /// See [`NativeWindow::set_cloaked`] for why this Shell RPC stays
  /// on the WM thread.
  fn mark_fullscreen_inner(&self, fullscreen: bool) -> crate::Result<()> {
    COM_INIT.with(|com_init| -> crate::Result<()> {
      com_init.borrow_mut().with_retry(|com| {
        let taskbar_list = com.taskbar_list()?;

        unsafe {
          taskbar_list.MarkFullscreenWindow(self.hwnd(), fullscreen)
        }?;

        Ok(())
      })
    })
  }

  /// Implements [`NativeWindowWindowsExt::set_taskbar_visibility`].
  pub(crate) fn set_taskbar_visibility(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_taskbar_visibility", || {
      self.set_taskbar_visibility_inner(visible)
    })
  }

  /// See [`NativeWindow::set_cloaked`] for why this Shell RPC stays
  /// on the WM thread.
  fn set_taskbar_visibility_inner(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    // Input-method helper HWNDs are not user windows. Never register them
    // as explicit taskbar tabs, regardless of which cleanup/restore
    // path calls this shared API.
    let visible = visible && !is_taskbar_helper_window(self);

    COM_INIT.with(|com_init| -> crate::Result<()> {
      com_init.borrow_mut().with_retry(|com| {
        let taskbar_list = com.taskbar_list()?;

        if visible {
          unsafe { taskbar_list.AddTab(self.hwnd())? };
        } else {
          unsafe { taskbar_list.DeleteTab(self.hwnd())? };
        }

        Ok(())
      })
    })
  }

  /// Whether `hwnd` belongs to this thread.
  ///
  /// Same-thread style writes do not send `WM_STYLECHANGING` across
  /// threads, so they stay inline. A worker would deadlock if this
  /// thread waited on it.
  fn gui_thread_is_current(&self) -> bool {
    let hwnd = self.hwnd();
    if !unsafe { IsWindow(hwnd) }.as_bool() {
      return false;
    }
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
    thread_id != 0 && thread_id == unsafe { GetCurrentThreadId() }
  }

  /// Whether the window's GUI thread can accept a synchronous message.
  ///
  /// Same-thread windows are treated as responsive. A cross-thread
  /// target must answer `WM_NULL` within
  /// [`FOREIGN_GUI_PROBE_TIMEOUT_MS`]. This is only a fast skip for a
  /// thread that is already not pumping. It is not the bound on
  /// `SetWindowLongPtrW` or `SetLayeredWindowAttributes`.
  fn foreign_gui_responsive(&self) -> bool {
    let hwnd = self.hwnd();
    // SAFETY: `IsWindow` accepts any bit pattern and reports whether
    // it is still a window.
    if !unsafe { IsWindow(hwnd) }.as_bool() {
      return false;
    }

    let mut process_id = 0u32;
    // SAFETY: `process_id` is a valid out-parameter.
    let thread_id =
      unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)) };
    if thread_id == 0 {
      return false;
    }
    // SAFETY: `GetCurrentThreadId` has no parameters.
    if thread_id == unsafe { GetCurrentThreadId() } {
      return true;
    }

    let mut result = 0usize;
    // SAFETY: `WM_NULL` carries no payload. The timeout keeps a
    // suspended foreign GUI thread from stalling this thread.
    let returned = unsafe {
      SendMessageTimeoutW(
        hwnd,
        WM_NULL,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_NORMAL,
        FOREIGN_GUI_PROBE_TIMEOUT_MS,
        Some(&raw mut result),
      )
    };
    returned.0 != 0
  }

  /// Implements [`NativeWindowWindowsExt::add_window_style_ex`].
  ///
  /// A foreign write runs off this thread. `SetWindowLongPtrW` sends
  /// `WM_STYLECHANGING` to the target and would freeze the WM thread
  /// if that handler stalls, even after `WM_NULL` succeeded.
  pub(crate) fn add_window_style_ex(&self, style: WINDOW_EX_STYLE) {
    timed_native_op(self.hwnd(), "add_window_style_ex", || {
      self.add_window_style_ex_inner(style);
    });
  }

  fn add_window_style_ex_inner(&self, style: WINDOW_EX_STYLE) {
    if self.has_window_style_ex(style) {
      mark_native_op_result("unchanged");
      return;
    }

    #[allow(clippy::cast_possible_wrap)]
    let bit = style.0 as isize;
    let hwnd = self.hwnd();
    let apply = move || {
      let current_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
      if current_style & bit == 0 {
        // SAFETY: Cross-thread `SetWindowLongPtrW` sends
        // `WM_STYLECHANGING` to the window thread. Callers that are
        // not that thread use [`run_bounded_foreign_gui_call`].
        unsafe {
          SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current_style | bit)
        };
      }
    };

    if self.gui_thread_is_current() {
      apply();
      return;
    }
    if !self.foreign_gui_responsive()
      || run_bounded_foreign_gui_call(hwnd.0, apply).is_none()
    {
      mark_native_op_result("skip-unresponsive");
      warn!(
        "Skipped extended style {:#x} for unresponsive hwnd={}.",
        style.0,
        hwnd_token(hwnd)
      );
    }
  }

  /// Implements [`NativeWindowWindowsExt::set_z_order`].
  pub(crate) fn set_z_order(
    &self,
    z_order: &WindowZOrder,
  ) -> crate::Result<()> {
    let z_order_hwnd = match z_order {
      WindowZOrder::TopMost => HWND_TOPMOST,
      WindowZOrder::Top => HWND_TOP,
      WindowZOrder::Normal => HWND_NOTOPMOST,
      WindowZOrder::AfterWindow(window_id) => HWND(window_id.0),
    };

    let flags = SWP_NOACTIVATE
      | SWP_NOCOPYBITS
      | SWP_ASYNCWINDOWPOS
      | SWP_NOOWNERZORDER
      | SWP_NOMOVE
      | SWP_NOSIZE;

    // A cross-process z-order request must not wait for a foreign GUI
    // thread. Keep the retry generation-aware so an older focus transition
    // cannot replay after a newer z-order repair has completed.
    let generation = current_or_new_z_order_generation();
    let expected_foreground = (!matches!(z_order, WindowZOrder::TopMost))
      .then(|| unsafe { GetForegroundWindow() });
    unsafe { SetWindowPos(self.hwnd(), z_order_hwnd, 0, 0, 0, 0, flags) }?;

    let handle = self.handle;
    if let Ok(runtime) = Handle::try_current() {
      runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if Z_ORDER_GENERATION.load(Ordering::SeqCst) != generation
          || expected_foreground.is_some_and(|foreground| {
            (unsafe { GetForegroundWindow() }) != foreground
          })
        {
          return;
        }
        let _ = unsafe {
          SetWindowPos(HWND(handle), z_order_hwnd, 0, 0, 0, 0, flags)
        };
      });
    }

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_title_bar_visibility`].
  ///
  /// The style write is bounded the same way as
  /// [`NativeWindow::add_window_style_ex`]. The following
  /// `SetWindowPos` stays asynchronous.
  pub(crate) fn set_title_bar_visibility(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_title_bar_visibility", || {
      self.set_title_bar_visibility_inner(visible)
    })
  }

  fn set_title_bar_visibility_inner(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    let style = unsafe { GetWindowLongPtrW(self.hwnd(), GWL_STYLE) };

    #[allow(clippy::cast_possible_wrap)]
    let new_style = if visible {
      style | (WS_DLGFRAME.0 as isize)
    } else {
      style & !(WS_DLGFRAME.0 as isize)
    };

    if new_style == style {
      mark_native_op_result("unchanged");
      return Ok(());
    }

    let hwnd = self.hwnd();
    let wrote = if self.gui_thread_is_current() {
      unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new_style) };
      true
    } else if !self.foreign_gui_responsive() {
      false
    } else {
      run_bounded_foreign_gui_call(hwnd.0, move || {
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new_style) };
      })
      .is_some()
    };
    if !wrote {
      mark_native_op_result("skip-unresponsive");
      warn!(
        "Skipped title-bar style for unresponsive hwnd={}.",
        hwnd_token(hwnd)
      );
      return Ok(());
    }

    // SAFETY: `SWP_ASYNCWINDOWPOS` keeps the frame refresh off this
    // thread. The style bits were already written.
    unsafe {
      SetWindowPos(
        hwnd,
        HWND_NOTOPMOST,
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED
          | SWP_NOMOVE
          | SWP_NOSIZE
          | SWP_NOZORDER
          | SWP_NOOWNERZORDER
          | SWP_NOACTIVATE
          | SWP_NOCOPYBITS
          | SWP_NOSENDCHANGING
          | SWP_ASYNCWINDOWPOS,
      )?;
    }

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_border_color`].
  pub(crate) fn set_border_color(
    &self,
    color: Option<&Color>,
  ) -> crate::Result<()> {
    self.set_border_color_inner(color)
  }

  fn set_border_color_inner(
    &self,
    color: Option<&Color>,
  ) -> crate::Result<()> {
    let bgr = match color {
      Some(color) => color.to_bgr(),
      None => DWMWA_COLOR_NONE,
    };

    unsafe {
      #[allow(clippy::cast_possible_truncation)]
      DwmSetWindowAttribute(
        self.hwnd(),
        DWMWA_BORDER_COLOR,
        std::ptr::from_ref(&bgr).cast(),
        std::mem::size_of::<u32>() as u32,
      )?;
    }

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_corner_style`].
  pub(crate) fn set_corner_style(
    &self,
    corner_style: &CornerStyle,
  ) -> crate::Result<()> {
    self.set_corner_style_inner(corner_style)
  }

  fn set_corner_style_inner(
    &self,
    corner_style: &CornerStyle,
  ) -> crate::Result<()> {
    let corner_preference = match corner_style {
      CornerStyle::Default => DWMWCP_DEFAULT,
      CornerStyle::Square => DWMWCP_DONOTROUND,
      CornerStyle::Rounded => DWMWCP_ROUND,
      CornerStyle::SmallRounded => DWMWCP_ROUNDSMALL,
    };

    unsafe {
      #[allow(clippy::cast_possible_truncation)]
      DwmSetWindowAttribute(
        self.hwnd(),
        DWMWA_WINDOW_CORNER_PREFERENCE,
        std::ptr::from_ref(&(corner_preference.0)).cast(),
        std::mem::size_of::<i32>() as u32,
      )?;
    }

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_transparency`].
  ///
  /// A fully opaque request does not add `WS_EX_LAYERED`. A window
  /// that is already layered still receives `opacity_value`. Both
  /// the style bit and `SetLayeredWindowAttributes` run off this
  /// thread when the target GUI thread is foreign, so a stall in
  /// either call cannot freeze the WM thread.
  pub(crate) fn set_transparency(
    &self,
    opacity_value: &OpacityValue,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_transparency", || {
      self.set_transparency_inner(opacity_value)
    })
  }

  fn set_transparency_inner(
    &self,
    opacity_value: &OpacityValue,
  ) -> crate::Result<()> {
    let alpha = opacity_value.to_alpha();
    let already_layered = self.has_window_style_ex(WS_EX_LAYERED);
    // 100% on a non-layered window is a no-op. Do not add the bit.
    if alpha == u8::MAX && !already_layered {
      mark_native_op_result("unchanged");
      return Ok(());
    }

    if !already_layered {
      self.add_window_style_ex(WS_EX_LAYERED);
      if !self.has_window_style_ex(WS_EX_LAYERED) {
        mark_native_op_result("skip-unresponsive");
        warn!(
          "Skipped transparency for unresponsive hwnd={}.",
          hwnd_token(self.hwnd())
        );
        return Ok(());
      }
    }

    self.set_layered_alpha_bounded(alpha)
  }

  /// Applies `alpha` with `SetLayeredWindowAttributes`.
  ///
  /// The foreign call is bounded by [`run_bounded_foreign_gui_call`].
  /// A `WM_NULL` probe only avoids starting that call when the thread
  /// is already not answering.
  fn set_layered_alpha_bounded(&self, alpha: u8) -> crate::Result<()> {
    let hwnd = self.hwnd();
    let apply = move || unsafe {
      SetLayeredWindowAttributes(hwnd, None, alpha, LWA_ALPHA)
    };

    if self.gui_thread_is_current() {
      apply()?;
      return Ok(());
    }
    if !self.foreign_gui_responsive() {
      mark_native_op_result("skip-unresponsive");
      warn!(
        "Skipped layered alpha for unresponsive hwnd={}.",
        hwnd_token(hwnd)
      );
      return Ok(());
    }

    if let Some(result) = run_bounded_foreign_gui_call(hwnd.0, apply) {
      result?;
      Ok(())
    } else {
      mark_native_op_result("skip-unresponsive");
      warn!(
        "Skipped layered alpha for unresponsive hwnd={}.",
        hwnd_token(hwnd)
      );
      Ok(())
    }
  }

  /// Implements [`NativeWindowWindowsExt::adjust_transparency`].
  pub(crate) fn adjust_transparency(
    &self,
    opacity_delta: &Delta<OpacityValue>,
  ) -> crate::Result<()> {
    let mut alpha = u8::MAX;
    let mut flag = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();

    unsafe {
      GetLayeredWindowAttributes(
        self.hwnd(),
        None,
        Some(&raw mut alpha),
        Some(&raw mut flag),
      )?;
    }

    if flag.contains(LWA_COLORKEY) {
      return Err(crate::Error::Platform(
        "Window uses color key for its transparency and cannot be adjusted."
          .to_string(),
      ));
    }

    let target_alpha = if opacity_delta.is_negative {
      alpha.saturating_sub(opacity_delta.inner.to_alpha())
    } else {
      alpha.saturating_add(opacity_delta.inner.to_alpha())
    };

    self.set_transparency(&OpacityValue::from_alpha(target_alpha))
  }

  /// Whether the window is cloaked. For some UWP apps, `WS_VISIBLE` will
  /// be present even if the window isn't actually visible. The
  /// `DWMWA_CLOAKED` attribute is used to check whether these apps are
  /// visible.
  fn is_cloaked(&self) -> crate::Result<bool> {
    let mut cloaked = 0u32;

    unsafe {
      #[allow(clippy::cast_possible_truncation)]
      DwmGetWindowAttribute(
        self.hwnd(),
        DWMWA_CLOAKED,
        std::ptr::from_mut::<u32>(&mut cloaked).cast(),
        std::mem::size_of::<u32>() as u32,
      )
    }?;

    Ok(cloaked != 0)
  }
}

fn capture_debug_value<T>(
  errors: &mut Vec<String>,
  name: &str,
  result: crate::Result<T>,
) -> Option<T> {
  match result {
    Ok(value) => Some(value),
    Err(error) => {
      errors.push(format!("{name}: {error}"));
      None
    }
  }
}

/// Captures best-effort diagnostics for one native window.
pub(crate) fn debug_info(window: &NativeWindow) -> NativeWindowDebugInfo {
  let hwnd = window.hwnd();
  let mut errors = Vec::new();
  let is_valid = window.is_valid();
  let title = capture_debug_value(&mut errors, "title", window.title());
  let class_name =
    capture_debug_value(&mut errors, "className", window.class_name());
  let process_path =
    capture_debug_value(&mut errors, "processPath", window.process_path());
  let process_name =
    capture_debug_value(&mut errors, "processName", window.process_name());
  let frame = capture_debug_value(&mut errors, "frame", window.frame());
  let frame_with_shadows = capture_debug_value(
    &mut errors,
    "frameWithShadows",
    window.frame_with_shadows(),
  );
  let is_cloaked =
    capture_debug_value(&mut errors, "isCloaked", window.is_cloaked());
  let is_minimized =
    capture_debug_value(&mut errors, "isMinimized", window.is_minimized());
  let is_maximized =
    capture_debug_value(&mut errors, "isMaximized", window.is_maximized());
  #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
  let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
  #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
  let extended_style =
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
  let is_window_visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
  let foreground_handle = unsafe { GetForegroundWindow() };
  let owner_handle = unsafe { GetWindow(hwnd, GW_OWNER) }.0;
  let parent_handle = unsafe { GetParent(hwnd) }.0;
  let process_id_and_thread_id = {
    let mut process_id = 0u32;
    let thread_id =
      unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)) };
    (process_id != 0).then_some((process_id, thread_id))
  };

  NativeWindowDebugInfo {
    handle: window.handle,
    title,
    class_name,
    process_name,
    process_path,
    process_id: process_id_and_thread_id.map(|(process_id, _)| process_id),
    thread_id: process_id_and_thread_id.map(|(_, thread_id)| thread_id),
    frame,
    frame_with_shadows,
    is_valid,
    is_window_visible: Some(is_window_visible),
    is_visible: is_cloaked.map(|cloaked| is_window_visible && !cloaked),
    is_cloaked,
    is_minimized,
    is_maximized,
    is_enabled: Some(unsafe { IsWindowEnabled(hwnd) }.as_bool()),
    is_foreground: hwnd == foreground_handle,
    owner_handle: (owner_handle != 0).then_some(owner_handle),
    parent_handle: (parent_handle != 0).then_some(parent_handle),
    style: Some(style),
    extended_style: Some(extended_style),
    is_topmost: Some((extended_style & WS_EX_TOPMOST.0) != 0),
    is_tool_window: Some((extended_style & WS_EX_TOOLWINDOW.0) != 0),
    is_app_window: Some((extended_style & WS_EX_APPWINDOW.0) != 0),
    is_no_activate: Some((extended_style & WS_EX_NOACTIVATE.0) != 0),
    is_child: Some((style & WS_CHILD.0) != 0),
    is_popup: Some((style & WS_POPUP.0) != 0),
    z_order_index: None,
    errors,
  }
}

/// Reorders a normal-window chain without changing focus or visibility.
///
/// The first window is placed at the top of the normal z-order. Each
/// subsequent window is placed immediately after the previous one. Native
/// requests are asynchronous so a suspended foreign GUI thread cannot
/// block the WM loop; the generation-aware retry reconciles ordering after
/// the target threads process their queued requests.
pub(crate) fn reorder_z_order(
  window_ids: &[WindowId],
) -> crate::Result<()> {
  if window_ids.is_empty() {
    return Ok(());
  }

  let generation = next_z_order_generation();
  apply_z_order_chain(window_ids)?;

  let window_ids = window_ids.to_vec();
  let focused_window = window_ids[0];
  if let Ok(runtime) = Handle::try_current() {
    runtime.spawn(async move {
      tokio::time::sleep(Duration::from_millis(10)).await;

      if Z_ORDER_GENERATION.load(Ordering::SeqCst) != generation
        || unsafe { GetForegroundWindow() } != HWND(focused_window.0)
      {
        return;
      }

      let _ = apply_z_order_chain(&window_ids);
    });
  }

  Ok(())
}

fn next_z_order_generation() -> u64 {
  Z_ORDER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

pub(crate) fn begin_z_order_batch() -> u64 {
  next_z_order_generation()
}

fn current_or_new_z_order_generation() -> u64 {
  let generation = Z_ORDER_GENERATION.load(Ordering::SeqCst);
  if generation == 0 {
    next_z_order_generation()
  } else {
    generation
  }
}

fn apply_z_order_chain(window_ids: &[WindowId]) -> crate::Result<()> {
  // Always use asynchronous cross-thread requests. A debugger-suspended or
  // otherwise hung target GUI thread must not block GlazeWM's event loop.
  let flags = SWP_NOACTIVATE
    | SWP_NOCOPYBITS
    | SWP_NOMOVE
    | SWP_NOSIZE
    | SWP_ASYNCWINDOWPOS
    | SWP_NOOWNERZORDER;

  for (index, window_id) in window_ids.iter().enumerate() {
    let insert_after = index
      .checked_sub(1)
      .and_then(|previous| window_ids.get(previous))
      .map_or(HWND_TOP, |previous| HWND(previous.0));

    // HWND_TOP does not clear TOPMOST. Drop that bit, then immediately
    // place the window. HWND_NOTOPMOST alone would leave it above every
    // non-topmost window, which is the wrong band for an ignored window.
    unsafe {
      SetWindowPos(HWND(window_id.0), HWND_NOTOPMOST, 0, 0, 0, 0, flags)?;
      SetWindowPos(HWND(window_id.0), insert_after, 0, 0, 0, 0, flags)?;
    }
  }

  Ok(())
}

impl PartialEq for NativeWindow {
  fn eq(&self, other: &Self) -> bool {
    self.handle == other.handle
  }
}

impl Eq for NativeWindow {}

impl From<NativeWindow> for crate::NativeWindow {
  fn from(window: NativeWindow) -> Self {
    crate::NativeWindow { inner: window }
  }
}

/// Implements [`Dispatcher::visible_windows`].
pub(crate) fn visible_windows(
  _: &Dispatcher,
) -> crate::Result<Vec<crate::NativeWindow>> {
  let mut handles: Vec<isize> = Vec::new();

  #[allow(clippy::items_after_statements)]
  extern "system" fn visible_windows_proc(
    handle: HWND,
    data: LPARAM,
  ) -> BOOL {
    let handles = data.0 as *mut Vec<isize>;
    unsafe { (*handles).push(handle.0) };
    true.into()
  }

  unsafe {
    EnumWindows(
      Some(visible_windows_proc),
      LPARAM(std::ptr::from_mut(&mut handles) as _),
    )
  }?;

  Ok(
    handles
      .into_iter()
      .map(NativeWindow::new)
      .filter(|window| window.is_visible().unwrap_or(false))
      .map(Into::into)
      .collect(),
  )
}

/// Implements [`Dispatcher::debug_windows`].
pub(crate) fn debug_windows(
  _: &Dispatcher,
) -> crate::Result<Vec<NativeWindowDebugInfo>> {
  let mut handles: Vec<isize> = Vec::new();

  #[allow(clippy::items_after_statements)]
  extern "system" fn debug_windows_proc(
    handle: HWND,
    data: LPARAM,
  ) -> BOOL {
    let handles = data.0 as *mut Vec<isize>;
    unsafe { (*handles).push(handle.0) };
    true.into()
  }

  unsafe {
    EnumWindows(
      Some(debug_windows_proc),
      LPARAM(std::ptr::from_mut(&mut handles) as _),
    )
  }?;

  Ok(
    handles
      .into_iter()
      .enumerate()
      .map(|(index, handle)| {
        let window = NativeWindow::new(handle);
        let mut info = debug_info(&window);
        #[allow(clippy::cast_possible_truncation)]
        {
          info.z_order_index = Some(index as u32);
        }
        info
      })
      .collect(),
  )
}

/// Uncloak + show top-level DWM-cloaked windows, skipping `skip_handles`
/// (typically currently managed `GlazeWM` window handles).
///
/// Unlike `visible_windows` / managed-container restore, this uses raw
/// `EnumWindows` and does **not** filter out cloaked HWNDs - so orphaned
/// windows left cloaked by a prior `GlazeWM` session (Brave/Edge/Terminal)
/// are included.
///
/// Returns how many windows were successfully unhidden.
pub(crate) fn unhide_all_cloaked_windows(
  skip_handles: &[isize],
  _: &Dispatcher,
) -> crate::Result<usize> {
  let handles = top_level_window_handles()?;

  let mut unhidden = 0usize;
  for handle in handles {
    let window = NativeWindow::new(handle);
    if !window.is_valid() {
      continue;
    }
    if skip_handles.contains(&handle) {
      continue;
    }

    // explorer.exe owns both real File Explorer windows and a large number
    // of shell/desktop helper HWNDs (WorkerW, Progman, taskbar internals,
    // etc.). The latter are not user windows, but AddTab below would
    // create a blank, uncloseable taskbar entry for them. Keep real
    // Explorer folder windows eligible while cleaning up any stale
    // shell tabs left by older versions of this command.
    if is_taskbar_helper_window(&window) {
      let _ = window.set_taskbar_visibility(false);
      continue;
    }

    let cloaked = match window.is_cloaked() {
      Ok(true) => true,
      Ok(false) => false,
      Err(_) => continue,
    };
    if !cloaked {
      continue;
    }

    // Primary path: same ApplicationView cloak API GlazeWM uses to hide.
    let uncloak_ok = window.set_cloaked(false).is_ok();
    if !uncloak_ok {
      // Fallback: DWMWA_CLOAK = FALSE (attribute 13).
      let mut cloak_flag: i32 = 0;
      let _ = unsafe {
        #[allow(clippy::cast_possible_truncation)]
        DwmSetWindowAttribute(
          window.hwnd(),
          DWMWA_CLOAK,
          std::ptr::from_mut(&mut cloak_flag).cast(),
          std::mem::size_of::<i32>() as u32,
        )
      };
    }

    let _ = window.show();
    let _ = window.set_taskbar_visibility(true);
    unhidden += 1;
  }

  Ok(unhidden)
}

/// Removes taskbar tabs left behind by broad uncloak operations that
/// treated shell/input helper HWNDs as user windows.
pub(crate) fn cleanup_taskbar_helper_windows(
  _: &Dispatcher,
) -> crate::Result<usize> {
  let mut cleaned = 0usize;
  for handle in top_level_window_handles()? {
    let window = NativeWindow::new(handle);
    if !window.is_valid() || !is_taskbar_helper_window(&window) {
      continue;
    }

    if window.set_taskbar_visibility(false).is_ok() {
      cleaned += 1;
    }
  }

  Ok(cleaned)
}

fn top_level_window_handles() -> crate::Result<Vec<isize>> {
  let mut handles: Vec<isize> = Vec::new();

  #[allow(clippy::items_after_statements)]
  extern "system" fn enum_proc(handle: HWND, data: LPARAM) -> BOOL {
    let handles = data.0 as *mut Vec<isize>;
    unsafe { (*handles).push(handle.0) };
    true.into()
  }

  unsafe {
    EnumWindows(
      Some(enum_proc),
      LPARAM(std::ptr::from_mut(&mut handles) as _),
    )
  }?;

  Ok(handles)
}

fn is_taskbar_helper_window(window: &NativeWindow) -> bool {
  is_explorer_shell_window(window) || is_input_method_window(window)
}

fn is_input_method_window(window: &NativeWindow) -> bool {
  window.class_name().is_ok_and(|class_name| {
    matches!(class_name.as_str(), "MSCTFIME UI" | "IME")
  })
}

fn is_explorer_shell_window(window: &NativeWindow) -> bool {
  let Ok(process_name) = window.process_name() else {
    return false;
  };

  if !process_name.eq_ignore_ascii_case("explorer") {
    return false;
  }

  let Ok(class_name) = window.class_name() else {
    return false;
  };

  !matches!(class_name.as_str(), "CabinetWClass" | "ExploreWClass")
}

/// Implements [`Dispatcher::focused_window`].
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn focused_window(
  _: &Dispatcher,
) -> crate::Result<crate::NativeWindow> {
  let handle = unsafe { GetForegroundWindow() };
  Ok(NativeWindow::new(handle.0).into())
}

/// Implements [`Dispatcher::window_from_point`].
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn window_from_point(
  point: &Point,
  _: &Dispatcher,
) -> crate::Result<Option<crate::NativeWindow>> {
  let point = POINT {
    x: point.x,
    y: point.y,
  };

  let handle = unsafe { WindowFromPoint(point) };
  if handle.0 == 0 {
    return Ok(None);
  }

  let root = unsafe { GetAncestor(handle, GA_ROOT) };
  if root.0 == 0 {
    return Ok(None);
  }

  Ok(Some(NativeWindow::new(root.0).into()))
}

/// Implements [`Dispatcher::reset_focus`].
pub(crate) fn reset_focus(_dispatcher: &Dispatcher) -> crate::Result<()> {
  desktop_window().focus()
}

/// Gets the `NativeWindow` instance of the desktop window.
///
/// This is the explorer.exe wallpaper window (i.e. "Progman"). If
/// explorer.exe isn't running, then default to the desktop window below
/// the wallpaper window.
#[must_use]
fn desktop_window() -> NativeWindow {
  let handle = match unsafe { GetShellWindow() } {
    HWND(0) => unsafe { GetDesktopWindow() },
    handle => handle,
  };

  NativeWindow::new(handle.0)
}

#[cfg(test)]
mod reorder_z_order_tests {
  use std::os::windows::ffi::OsStrExt;

  use windows::{
    core::PCWSTR,
    Win32::{
      Foundation::{HWND, LPARAM, LRESULT, WPARAM},
      UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetLayeredWindowAttributes, GetMessageW, GetTopWindow, GetWindow,
        GetWindowLongPtrW, InSendMessage, RegisterClassW,
        TranslateMessage, UnregisterClassW, GWL_EXSTYLE, GWL_STYLE,
        GW_HWNDNEXT, LAYERED_WINDOW_ATTRIBUTES_FLAGS, MSG,
        WINDOW_EX_STYLE, WM_NULL, WM_WINDOWPOSCHANGING, WNDCLASSW,
        WS_DLGFRAME, WS_EX_LAYERED, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
      },
    },
  };

  use super::reorder_z_order;
  use crate::{
    Color, CornerStyle, OpacityValue, Rect, WindowId, WindowZOrder,
  };

  unsafe extern "system" fn reorder_test_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
  }

  fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
      .encode_wide()
      .chain(Some(0))
      .collect()
  }

  /// Creates a visible top-level window. `topmost` reproduces a Snipping
  /// Tool window left `WS_EX_TOPMOST` by a previous GlazeWM session.
  fn create_test_window(class: &[u16], topmost: bool) -> HWND {
    let title = wide("glazewm-z-order-test");
    let ex_style = if topmost {
      WS_EX_TOPMOST
    } else {
      WINDOW_EX_STYLE::default()
    };
    let hwnd = unsafe {
      CreateWindowExW(
        ex_style,
        PCWSTR(class.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        40,
        40,
        160,
        80,
        None,
        None,
        None,
        None,
      )
    };
    assert_ne!(hwnd.0, 0, "create z-order test window");
    hwnd
  }

  fn relative_order(targets: &[HWND]) -> Vec<isize> {
    let mut order = Vec::new();
    let mut hwnd = unsafe { GetTopWindow(None) };
    while hwnd.0 != 0 {
      if targets.iter().any(|target| target.0 == hwnd.0) {
        order.push(hwnd.0);
      }
      hwnd = unsafe { GetWindow(hwnd, GW_HWNDNEXT) };
    }
    order
  }

  fn helper_mode(mode: &str) -> bool {
    std::env::var("GLAZEWM_Z_ORDER_HELPER").ok().as_deref() == Some(mode)
  }

  fn create_non_pumping_window() {
    use std::{io::Write, thread, time::Duration};

    let class_name =
      wide(&format!("GlazeWmNonPumpingHelper{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register non-pumping helper class");

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
    if std::env::var("GLAZEWM_HELPER_LAYERED").ok().as_deref() == Some("1")
    {
      // Same thread, so the probe does not apply. Seeds the
      // already-layered case from issue #10 before the thread stops.
      super::NativeWindow::new(hwnd.0)
        .set_transparency(&OpacityValue::from_alpha(200))
        .expect("seed layered alpha");
    }
    println!("GLAZEWM_Z_ORDER_HWND:{}", hwnd.0);
    std::io::stdout().flush().expect("flush helper HWND");
    if std::env::var("GLAZEWM_HELPER_STOP").ok().as_deref()
      == Some("suspend")
    {
      use windows::Win32::System::Threading::{
        GetCurrentThread, SuspendThread,
      };
      // Debugger Break All suspends the GUI thread. Sleep does not.
      unsafe {
        SuspendThread(GetCurrentThread());
      }
    }
    loop {
      thread::sleep(Duration::from_secs(60));
    }
  }

  /// Armed after the helper has created and, when requested, layered
  /// the window. Posted messages still run. A cross-thread send other
  /// than `WM_NULL` stalls, including `WM_STYLECHANGING`.
  static STYLE_STALL_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

  unsafe extern "system" fn style_stall_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    if STYLE_STALL_ARMED.load(std::sync::atomic::Ordering::SeqCst)
      && unsafe { InSendMessage() }.as_bool()
      && msg != WM_NULL
    {
      // Longer than the production foreign-call bound and the
      // parent's 2 second deadline.
      std::thread::sleep(std::time::Duration::from_secs(30));
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
  }

  /// Pumps messages, answers `WM_NULL`, and stalls in the style-change
  /// handler. This is the case a `WM_NULL` probe cannot see.
  fn create_style_stall_window() {
    use std::io::Write;

    STYLE_STALL_ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
    let class_name =
      wide(&format!("GlazeWmStyleStallHelper{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(style_stall_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register style-stall helper class");

    let title = wide("GlazeWM style-stall helper");
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
    assert_ne!(hwnd.0, 0, "create style-stall helper window");
    if std::env::var("GLAZEWM_HELPER_LAYERED").ok().as_deref() == Some("1")
    {
      super::NativeWindow::new(hwnd.0)
        .set_transparency(&OpacityValue::from_alpha(200))
        .expect("seed layered alpha");
    }
    STYLE_STALL_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
    println!("GLAZEWM_Z_ORDER_HWND:{}", hwnd.0);
    std::io::stdout().flush().expect("flush helper HWND");

    let mut message = MSG::default();
    while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool() {
      unsafe {
        TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }
    std::thread::sleep(std::time::Duration::from_secs(60));
  }

  fn create_pumping_window_pair() {
    use std::{io::Write, time::Duration};

    let class_name =
      wide(&format!("GlazeWmPumpingHelper{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register pumping helper class");

    let topmost = create_test_window(&class_name, true);
    let peer = create_test_window(&class_name, false);
    println!("GLAZEWM_Z_ORDER_PUMPING_HWND:{}:{}", topmost.0, peer.0);
    std::io::stdout()
      .flush()
      .expect("flush pumping helper HWNDs");

    let mut message = MSG::default();
    while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool() {
      unsafe {
        TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }

    // Keep the helper alive if it receives a quit message before the
    // parent has finished polling the windows.
    std::thread::sleep(Duration::from_secs(60));
  }

  unsafe extern "system" fn delayed_reorder_test_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    if msg == WM_WINDOWPOSCHANGING {
      let delay_ms = std::env::var("GLAZEWM_Z_ORDER_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default();
      std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
  }

  fn create_single_pumping_window() {
    use std::io::Write;

    let class_name =
      wide(&format!("GlazeWmSinglePumpingHelper{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(delayed_reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register single pumping helper class");

    let topmost = std::env::var("GLAZEWM_Z_ORDER_TOPMOST").ok().as_deref()
      == Some("1");
    let ex_style = if topmost {
      WS_EX_TOPMOST
    } else {
      WINDOW_EX_STYLE::default()
    };
    let title = wide("GlazeWM independent z-order helper");
    let hwnd = unsafe {
      CreateWindowExW(
        ex_style,
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
    assert_ne!(hwnd.0, 0, "create single pumping helper window");
    println!("GLAZEWM_Z_ORDER_SINGLE_HWND:{}", hwnd.0);
    std::io::stdout()
      .flush()
      .expect("flush single pumping helper HWND");

    let mut message = MSG::default();
    while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool() {
      unsafe {
        TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }
  }

  fn run_z_order_helper_call(mode: &str) {
    let hwnd = std::env::var("GLAZEWM_Z_ORDER_HWND")
      .expect("helper HWND")
      .parse::<isize>()
      .expect("parse helper HWND");
    match mode {
      "set-z-order" => {
        super::NativeWindow::new(hwnd)
          .set_z_order(&WindowZOrder::Normal)
          .expect("set z-order");
      }
      "reorder-z-order" => {
        reorder_z_order(&[WindowId(hwnd)]).expect("reorder z-order");
      }
      _ => panic!("unknown z-order helper mode: {mode}"),
    }
  }

  #[test]
  fn z_order_test_helper_window() {
    if helper_mode("window") {
      create_non_pumping_window();
    }
  }

  #[test]
  fn style_stall_helper_window() {
    if helper_mode("style-stall") {
      create_style_stall_window();
    }
  }

  #[test]
  fn z_order_test_helper_pumping_window() {
    if helper_mode("pumping-window") {
      create_pumping_window_pair();
    }
  }

  #[test]
  fn z_order_test_helper_single_pumping_window() {
    if helper_mode("single-pumping-window") {
      create_single_pumping_window();
    }
  }

  #[test]
  fn z_order_test_helper_set_z_order() {
    if helper_mode("set-z-order") {
      run_z_order_helper_call("set-z-order");
    }
  }

  #[test]
  fn z_order_test_helper_reorder_z_order() {
    if helper_mode("reorder-z-order") {
      run_z_order_helper_call("reorder-z-order");
    }
  }

  #[test]
  fn reorder_z_order_sinks_a_topmost_window_to_the_bottom_of_the_chain() {
    let class_name =
      wide(&format!("GlazeWmZOrderTest{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register z-order test class");

    let visual_studio = create_test_window(&class_name, false);
    let tiled = create_test_window(&class_name, false);
    // Start topmost, which is the bad post-restart Snipping Tool state.
    let snipping = create_test_window(&class_name, true);

    let chain = [
      WindowId(visual_studio.0),
      WindowId(tiled.0),
      WindowId(snipping.0),
    ];
    reorder_z_order(&chain).expect("reorder");

    let order = relative_order(&[visual_studio, tiled, snipping]);
    unsafe {
      let _ = DestroyWindow(visual_studio);
      let _ = DestroyWindow(tiled);
      let _ = DestroyWindow(snipping);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }

    assert_eq!(
      order,
      vec![visual_studio.0, tiled.0, snipping.0],
      "chain must be applied top-to-bottom, with the topmost ignored window sunk"
    );
  }

  fn assert_foreign_z_order_call_is_bounded(mode: &str) {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
      time::{Duration, Instant},
    };

    let current_exe = std::env::current_exe().expect("test executable");
    let mut window_helper = Command::new(&current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "window")
      .arg("z_order_test_helper_window")
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn non-pumping window helper");
    let stdout = window_helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let hwnd = loop {
      let line = lines
        .next()
        .expect("window helper HWND")
        .expect("read window helper HWND");
      if let Some(hwnd) = line.strip_prefix("GLAZEWM_Z_ORDER_HWND:") {
        break hwnd.parse::<isize>().expect("parse window helper HWND");
      }
    };

    let mut z_order_call = Command::new(&current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", mode)
      .env("GLAZEWM_Z_ORDER_HWND", hwnd.to_string())
      .arg(format!("z_order_test_helper_{mode}"))
      .arg("--nocapture")
      .stdout(Stdio::null())
      .spawn()
      .expect("spawn bounded z-order caller");

    let deadline = Instant::now() + Duration::from_secs(2);
    let completed = loop {
      if z_order_call
        .try_wait()
        .expect("poll z-order caller")
        .is_some()
      {
        break true;
      }
      if Instant::now() >= deadline {
        break false;
      }
      std::thread::sleep(Duration::from_millis(10));
    };

    if !completed {
      let _ = z_order_call.kill();
      let _ = z_order_call.wait();
    }
    let _ = window_helper.kill();
    let _ = window_helper.wait();

    assert!(completed, "{mode} blocked on a non-pumping foreign HWND");
  }

  fn foreign_windows_have_order(expected: &[HWND]) -> bool {
    let order = relative_order(expected);
    #[allow(clippy::cast_possible_wrap)]
    let topmost_style = WS_EX_TOPMOST.0 as isize;
    let all_not_topmost = expected.iter().all(|window| {
      let style = unsafe { GetWindowLongPtrW(*window, GWL_EXSTYLE) };
      style & topmost_style == 0
    });
    let expected_order =
      expected.iter().map(|window| window.0).collect::<Vec<_>>();
    order == expected_order && all_not_topmost
  }

  fn assert_foreign_z_order_converges(expected: &[HWND]) {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
      if foreign_windows_have_order(expected) {
        return;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    let order = relative_order(expected);
    #[allow(clippy::cast_possible_wrap)]
    let topmost_style = WS_EX_TOPMOST.0 as isize;
    let topmost_states = expected
      .iter()
      .map(|window| {
        (unsafe { GetWindowLongPtrW(*window, GWL_EXSTYLE) }
          & topmost_style)
          != 0
      })
      .collect::<Vec<_>>();
    let expected_order =
      expected.iter().map(|window| window.0).collect::<Vec<_>>();
    panic!("foreign z-order did not converge: expected={expected_order:?}, actual={order:?}, topmost={topmost_states:?}");
  }

  fn spawn_pumping_window_helper() -> (std::process::Child, [HWND; 2]) {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
    };

    let current_exe = std::env::current_exe().expect("test executable");
    let mut helper = Command::new(current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "pumping-window")
      .arg("z_order_test_helper_pumping_window")
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn pumping window helper");
    let stdout = helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let handles = loop {
      let line = lines
        .next()
        .expect("pumping helper HWNDs")
        .expect("read pumping helper HWNDs");
      if let Some(handles) =
        line.strip_prefix("GLAZEWM_Z_ORDER_PUMPING_HWND:")
      {
        break handles
          .split(':')
          .map(|handle| {
            handle.parse::<isize>().expect("parse pumping helper HWND")
          })
          .collect::<Vec<_>>();
      }
    };
    assert_eq!(handles.len(), 2, "pumping helper must expose two HWNDs");
    (helper, [HWND(handles[0]), HWND(handles[1])])
  }

  fn spawn_single_pumping_window_helper(
    topmost: bool,
    delay_ms: u64,
  ) -> (std::process::Child, HWND) {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
    };

    let current_exe = std::env::current_exe().expect("test executable");
    let mut helper = Command::new(current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "single-pumping-window")
      .env("GLAZEWM_Z_ORDER_TOPMOST", if topmost { "1" } else { "0" })
      .env("GLAZEWM_Z_ORDER_DELAY_MS", delay_ms.to_string())
      .arg("z_order_test_helper_single_pumping_window")
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn single pumping window helper");
    let stdout = helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let hwnd = loop {
      let line = lines
        .next()
        .expect("single pumping helper HWND")
        .expect("read single pumping helper HWND");
      if let Some(hwnd) = line.strip_prefix("GLAZEWM_Z_ORDER_SINGLE_HWND:")
      {
        break hwnd
          .parse::<isize>()
          .expect("parse single pumping helper HWND");
      }
    };
    (helper, HWND(hwnd))
  }

  #[test]
  fn reorder_z_order_converges_for_foreign_pumping_windows() {
    let (mut helper, [topmost, peer]) = spawn_pumping_window_helper();

    reorder_z_order(&[WindowId(topmost.0), WindowId(peer.0)])
      .expect("reorder foreign pumping windows");
    assert_foreign_z_order_converges(&[topmost, peer]);

    // Queue two successive desired chains before polling. The final chain
    // must win without restoring TOPMOST on the foreign window.
    reorder_z_order(&[WindowId(peer.0), WindowId(topmost.0)])
      .expect("queue first rapid foreign reorder");
    reorder_z_order(&[WindowId(topmost.0), WindowId(peer.0)])
      .expect("queue second rapid foreign reorder");
    assert_foreign_z_order_converges(&[topmost, peer]);

    let _ = helper.kill();
    let _ = helper.wait();
  }

  #[test]
  fn reorder_z_order_retries_across_independent_foreign_gui_queues() {
    use std::time::Duration;

    // The first process deliberately stalls its queue while the second
    // process services its requests immediately. This forces the
    // production retry to run after the initial requests have been
    // serviced out of order.
    let (mut delayed_helper, delayed_topmost) =
      spawn_single_pumping_window_helper(true, 100);
    let (mut prompt_helper, prompt_peer) =
      spawn_single_pumping_window_helper(false, 0);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
      reorder_z_order(&[
        WindowId(delayed_topmost.0),
        WindowId(prompt_peer.0),
      ])
      .expect("reorder independently serviced foreign windows");

      // Let the generation-aware 10 ms retry execute while the delayed
      // foreign queue is still processing its first request.
      tokio::time::sleep(Duration::from_millis(500)).await;
    });

    assert_foreign_z_order_converges(&[delayed_topmost, prompt_peer]);

    let _ = delayed_helper.kill();
    let _ = delayed_helper.wait();
    let _ = prompt_helper.kill();
    let _ = prompt_helper.wait();
  }

  #[test]
  fn set_z_order_does_not_wait_for_a_non_pumping_foreign_window() {
    assert_foreign_z_order_call_is_bounded("set-z-order");
  }

  #[test]
  fn reorder_z_order_does_not_wait_for_a_non_pumping_foreign_window() {
    assert_foreign_z_order_call_is_bounded("reorder-z-order");
  }

  fn run_hung_style_helper_call(mode: &str) {
    let hwnd = std::env::var("GLAZEWM_Z_ORDER_HWND")
      .expect("helper HWND")
      .parse::<isize>()
      .expect("parse helper HWND");
    let window = super::NativeWindow::new(hwnd);
    match mode {
      "set-transparency" => {
        window
          .set_transparency(&OpacityValue::from_alpha(180))
          .expect("set transparency");
      }
      "set-transparency-opaque" => {
        window
          .set_transparency(&OpacityValue::from_alpha(u8::MAX))
          .expect("set opaque transparency");
      }
      "set-title-bar" => {
        window
          .set_title_bar_visibility(false)
          .expect("hide title bar");
      }
      _ => panic!("unknown hung style helper mode: {mode}"),
    }
  }

  #[test]
  fn hung_style_helper_set_transparency() {
    if helper_mode("set-transparency") {
      run_hung_style_helper_call("set-transparency");
    }
  }

  #[test]
  fn hung_style_helper_set_transparency_opaque() {
    if helper_mode("set-transparency-opaque") {
      run_hung_style_helper_call("set-transparency-opaque");
    }
  }

  #[test]
  fn hung_style_helper_set_title_bar() {
    if helper_mode("set-title-bar") {
      run_hung_style_helper_call("set-title-bar");
    }
  }

  /// Style bits of the helper window after a bounded call.
  struct HungStyleOutcome {
    layered: bool,
    has_dlg_frame: bool,
    alpha: Option<u8>,
  }

  /// Runs `mode` against a sleeping, non-pumping helper.
  fn assert_hung_style_call_is_bounded(mode: &str) -> HungStyleOutcome {
    assert_stopped_style_call(mode, "sleep", false)
  }

  /// Runs `mode` against a helper that has stopped its GUI thread.
  ///
  /// `stop` is `sleep` or `suspend`. `layered` seeds `WS_EX_LAYERED`
  /// and alpha 200 before the thread stops. The caller must finish
  /// within 2 seconds. Style is read before the helper exits.
  fn assert_stopped_style_call(
    mode: &str,
    stop: &str,
    layered: bool,
  ) -> HungStyleOutcome {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
      time::{Duration, Instant},
    };

    let current_exe = std::env::current_exe().expect("test executable");
    let (helper_test, helper_kind) = if stop == "style-stall" {
      ("style_stall_helper_window", "style-stall")
    } else {
      ("z_order_test_helper_window", "window")
    };
    let mut window_helper = Command::new(&current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", helper_kind)
      .env("GLAZEWM_HELPER_STOP", stop)
      .env("GLAZEWM_HELPER_LAYERED", if layered { "1" } else { "0" })
      .arg(helper_test)
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn non-pumping window helper");
    let stdout = window_helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let hwnd = loop {
      let line = lines
        .next()
        .expect("window helper HWND")
        .expect("read window helper HWND");
      if let Some(hwnd) = line.strip_prefix("GLAZEWM_Z_ORDER_HWND:") {
        break hwnd.parse::<isize>().expect("parse window helper HWND");
      }
    };
    if stop == "suspend" || stop == "style-stall" {
      // Suspend: the HWND line is printed just before SuspendThread.
      // Style stall: the stall is armed before the HWND line, and the
      // pump starts after it. Wait until that pump is running.
      std::thread::sleep(Duration::from_millis(100));
    }

    let (test_name, helper_mode_name) = match mode {
      "set-transparency" if stop == "sleep" && !layered => {
        ("hung_style_helper_set_transparency", mode)
      }
      "set-transparency-opaque" if stop == "sleep" && !layered => {
        ("hung_style_helper_set_transparency_opaque", mode)
      }
      "set-title-bar" if stop == "sleep" && !layered => {
        ("hung_style_helper_set_title_bar", mode)
      }
      _ => ("foreign_hwnd_call", "foreign-call"),
    };
    let mut style_call = Command::new(&current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", helper_mode_name)
      .env("GLAZEWM_FOREIGN_OP", mode)
      .env("GLAZEWM_Z_ORDER_HWND", hwnd.to_string())
      .arg(test_name)
      .arg("--nocapture")
      .stdout(Stdio::null())
      .spawn()
      .expect("spawn hung style caller");

    let deadline = Instant::now() + Duration::from_secs(2);
    let completed = loop {
      if style_call.try_wait().expect("poll style caller").is_some() {
        break true;
      }
      if Instant::now() >= deadline {
        break false;
      }
      std::thread::sleep(Duration::from_millis(10));
    };

    if !completed {
      let _ = style_call.kill();
      let _ = style_call.wait();
    }
    let outcome = HungStyleOutcome {
      layered: ex_style_bit(hwnd, WS_EX_LAYERED.0),
      has_dlg_frame: {
        #[allow(clippy::cast_possible_wrap)]
        let frame = WS_DLGFRAME.0 as isize;
        (unsafe { GetWindowLongPtrW(HWND(hwnd), GWL_STYLE) }) & frame != 0
      },
      alpha: layered_alpha(hwnd),
    };
    let _ = window_helper.kill();
    let _ = window_helper.wait();

    assert!(completed, "{mode} blocked on a non-pumping foreign HWND");
    outcome
  }

  fn ex_style_bit(hwnd: isize, bit: u32) -> bool {
    #[allow(clippy::cast_possible_wrap)]
    let bit = bit as isize;
    (unsafe { GetWindowLongPtrW(HWND(hwnd), GWL_EXSTYLE) }) & bit != 0
  }

  fn layered_alpha(hwnd: isize) -> Option<u8> {
    if !ex_style_bit(hwnd, WS_EX_LAYERED.0) {
      return None;
    }
    let mut alpha = 0u8;
    let mut flag = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
    unsafe {
      GetLayeredWindowAttributes(
        HWND(hwnd),
        None,
        Some(&raw mut alpha),
        Some(&raw mut flag),
      )
      .ok()?;
    }
    Some(alpha)
  }

  /// Dispatches one production call named by `GLAZEWM_FOREIGN_OP`.
  ///
  /// Errors are ignored. The parent only requires the process to exit.
  /// A hang fails the parent's 2 second deadline.
  fn run_foreign_hwnd_call() {
    use windows::Win32::UI::WindowsAndMessaging::{
      SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
      SWP_NOCOPYBITS, SWP_NOSENDCHANGING,
    };

    let hwnd = std::env::var("GLAZEWM_Z_ORDER_HWND")
      .expect("helper HWND")
      .parse::<isize>()
      .expect("parse helper HWND");
    let window = super::NativeWindow::new(hwnd);
    let op = std::env::var("GLAZEWM_FOREIGN_OP").unwrap_or_default();
    let color = Color {
      r: 1,
      g: 2,
      b: 3,
      a: 255,
    };
    let rect = Rect::from_ltrb(60, 60, 260, 180);
    match op.as_str() {
      "set-transparency" => {
        window
          .set_transparency(&OpacityValue::from_alpha(180))
          .expect("set transparency");
      }
      "set-transparency-opaque" => {
        window
          .set_transparency(&OpacityValue::from_alpha(u8::MAX))
          .expect("set opaque transparency");
      }
      "set-title-bar" => {
        window
          .set_title_bar_visibility(false)
          .expect("hide title bar");
      }
      "focus" => {
        let _ = window.focus();
      }
      "set-border-color" => {
        let _ = window.set_border_color(Some(&color));
      }
      "set-corner-style" => {
        let _ = window.set_corner_style(&CornerStyle::Square);
      }
      "restore" => {
        let _ = window.restore(Some(&rect));
      }
      "set-window-pos" => {
        let _ = window.set_window_pos(
          &WindowZOrder::Normal,
          &rect,
          SWP_NOACTIVATE
            | SWP_NOCOPYBITS
            | SWP_NOSENDCHANGING
            | SWP_ASYNCWINDOWPOS
            | SWP_FRAMECHANGED,
        );
      }
      "show" => {
        let _ = window.show();
      }
      "hide" => {
        let _ = window.hide();
      }
      "minimize" => {
        let _ = window.minimize();
      }
      "maximize" => {
        let _ = window.maximize();
      }
      "set-cloaked" => {
        let _ = window.set_cloaked(false);
      }
      "mark-fullscreen" => {
        let _ = window.mark_fullscreen(false);
      }
      "set-taskbar-visibility" => {
        let _ = window.set_taskbar_visibility(true);
      }
      "focus-transition" => {
        let class_name =
          wide(&format!("GlazeWmFocusPeer{}", std::process::id()));
        let class = WNDCLASSW {
          lpfnWndProc: Some(reorder_test_wnd_proc),
          lpszClassName: PCWSTR(class_name.as_ptr()),
          ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&raw const class) };
        assert_ne!(atom, 0, "register focus peer class");
        let peer = create_test_window(&class_name, false);
        let _ = super::NativeWindow::new(peer.0).focus();
        let _ = window.set_transparency(&OpacityValue::from_alpha(180));
        let _ = window.set_title_bar_visibility(false);
        let _ = window.set_border_color(Some(&color));
        let _ = window.set_corner_style(&CornerStyle::Square);
      }
      other => panic!("unknown foreign op: {other}"),
    }
  }

  #[test]
  fn foreign_hwnd_call() {
    if helper_mode("foreign-call") {
      run_foreign_hwnd_call();
    }
  }

  #[test]
  fn set_transparency_does_not_block_or_layer_a_non_pumping_window() {
    let outcome = assert_hung_style_call_is_bounded("set-transparency");
    assert!(
      !outcome.layered,
      "set_transparency added WS_EX_LAYERED on a non-pumping window"
    );
  }

  #[test]
  fn opaque_transparency_does_not_block_or_layer_a_non_pumping_window() {
    let outcome =
      assert_hung_style_call_is_bounded("set-transparency-opaque");
    assert!(
      !outcome.layered,
      "opaque set_transparency added WS_EX_LAYERED on a non-pumping window"
    );
  }

  #[test]
  fn set_title_bar_visibility_does_not_block_on_a_non_pumping_window() {
    let outcome = assert_hung_style_call_is_bounded("set-title-bar");
    assert!(
      outcome.has_dlg_frame,
      "set_title_bar_visibility cleared the title bar on a non-pumping window"
    );
  }

  #[test]
  fn set_transparency_still_layers_a_responsive_window() {
    let class_name = wide(&format!(
      "GlazeWmResponsiveTransparency{}",
      std::process::id()
    ));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register responsive transparency class");
    let hwnd = create_test_window(&class_name, false);
    let window = super::NativeWindow::new(hwnd.0);

    window
      .set_transparency(&OpacityValue::from_alpha(u8::MAX))
      .expect("opaque transparency");
    assert!(
      !ex_style_bit(hwnd.0, WS_EX_LAYERED.0),
      "a 100% request must not add WS_EX_LAYERED"
    );

    window
      .set_transparency(&OpacityValue::from_alpha(80))
      .expect("partial transparency");
    let mut alpha = 0u8;
    let mut flag = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
    unsafe {
      GetLayeredWindowAttributes(
        hwnd,
        None,
        Some(&raw mut alpha),
        Some(&raw mut flag),
      )
      .expect("read partial alpha");
    }
    assert_eq!(alpha, 80);
    assert!(ex_style_bit(hwnd.0, WS_EX_LAYERED.0));

    unsafe {
      let _ = DestroyWindow(hwnd);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
  }

  #[test]
  fn suspended_thread_transparency_does_not_layer() {
    let outcome =
      assert_stopped_style_call("set-transparency", "suspend", false);
    assert!(!outcome.layered);
    assert!(outcome.has_dlg_frame);
  }

  #[test]
  fn suspended_thread_opaque_transparency_does_not_layer() {
    let outcome = assert_stopped_style_call(
      "set-transparency-opaque",
      "suspend",
      false,
    );
    assert!(!outcome.layered);
  }

  #[test]
  fn suspended_thread_title_bar_is_not_cleared() {
    let outcome =
      assert_stopped_style_call("set-title-bar", "suspend", false);
    assert!(outcome.has_dlg_frame);
  }

  #[test]
  fn layered_sleeping_window_keeps_its_alpha() {
    let outcome =
      assert_stopped_style_call("set-transparency", "sleep", true);
    assert!(outcome.layered);
    assert_eq!(outcome.alpha, Some(200));
  }

  #[test]
  fn layered_suspended_window_keeps_its_alpha() {
    let outcome =
      assert_stopped_style_call("set-transparency", "suspend", true);
    assert!(outcome.layered);
    assert_eq!(outcome.alpha, Some(200));
  }

  #[test]
  fn focus_returns_for_a_sleeping_foreign_window() {
    assert_stopped_style_call("focus", "sleep", false);
  }

  #[test]
  fn focus_returns_for_a_suspended_foreign_window() {
    assert_stopped_style_call("focus", "suspend", false);
  }

  #[test]
  fn style_changing_stall_does_not_block_or_layer() {
    let outcome =
      assert_stopped_style_call("set-transparency", "style-stall", false);
    assert!(
      !outcome.layered,
      "set_transparency layered a window stalled in WM_STYLECHANGING"
    );
    assert!(outcome.has_dlg_frame);
  }

  #[test]
  fn style_changing_stall_does_not_clear_title_bar() {
    let outcome =
      assert_stopped_style_call("set-title-bar", "style-stall", false);
    assert!(
      outcome.has_dlg_frame,
      "set_title_bar_visibility cleared a window stalled in WM_STYLECHANGING"
    );
  }

  #[test]
  fn layered_attributes_stay_bounded_during_style_stall() {
    let outcome =
      assert_stopped_style_call("set-transparency", "style-stall", true);
    assert!(outcome.layered);
    assert_eq!(
      outcome.alpha,
      Some(200),
      "SetLayeredWindowAttributes changed alpha while the GUI thread was stalled"
    );
  }

  #[test]
  fn focus_transition_effects_do_not_restyle_a_suspended_window() {
    let outcome =
      assert_stopped_style_call("focus-transition", "suspend", false);
    assert!(
      !outcome.layered,
      "focus transition layered the suspended window"
    );
    assert!(
      outcome.has_dlg_frame,
      "focus transition cleared the suspended window frame"
    );
  }

  #[test]
  fn remaining_native_calls_return_for_a_suspended_window() {
    for op in [
      "set-border-color",
      "set-corner-style",
      "restore",
      "set-window-pos",
      "show",
      "hide",
      "minimize",
      "maximize",
      "set-cloaked",
      "mark-fullscreen",
      "set-taskbar-visibility",
    ] {
      assert_stopped_style_call(op, "suspend", false);
    }
  }

  #[test]
  fn native_op_log_brackets_a_transparency_call() {
    use std::sync::Mutex;

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    fn record(line: &str) {
      if let Ok(mut lines) = LINES.lock() {
        lines.push(line.to_string());
      }
    }

    let class_name =
      wide(&format!("GlazeWmNativeOpLog{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register native-op log class");
    let hwnd = create_test_window(&class_name, false);
    LINES.lock().expect("log lines").clear();
    super::set_native_op_logger(record);
    super::NativeWindow::new(hwnd.0)
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("responsive transparency");
    let lines = LINES.lock().expect("log lines").clone();
    unsafe {
      let _ = DestroyWindow(hwnd);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
    assert!(
      lines.iter().any(|line| {
        line.contains("native-op begin")
          && line.contains("op=set_transparency")
      }),
      "missing begin line: {lines:?}"
    );
    assert!(
      lines.iter().any(|line| {
        line.contains("native-op end")
          && line.contains("op=set_transparency")
          && line.contains("result=ok")
      }),
      "missing end line: {lines:?}"
    );
  }
}
