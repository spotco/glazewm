use std::{
  sync::atomic::{AtomicU64, Ordering},
  time::Duration,
};

use tokio::task;
use tracing::warn;
use windows::{
  core::PWSTR,
  Win32::{
    Foundation::{CloseHandle, BOOL, HWND, LPARAM, POINT, RECT},
    Graphics::Dwm::{
      DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
      DWMWA_CLOAK, DWMWA_CLOAKED, DWMWA_COLOR_NONE,
      DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_WINDOW_CORNER_PREFERENCE,
      DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
    },
    System::Threading::{
      OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
      PROCESS_QUERY_LIMITED_INFORMATION,
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
        IsWindowVisible, IsZoomed, SendNotifyMessageW,
        SetForegroundWindow, SetLayeredWindowAttributes,
        SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
        ShowWindowAsync, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GWL_STYLE,
        GW_OWNER, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
        LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, LWA_COLORKEY,
        SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOMOVE, SWP_NOOWNERZORDER,
        SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
        SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNA,
        WINDOWPLACEMENT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE,
        WPF_ASYNCWINDOWPLACEMENT, WS_CHILD, WS_DLGFRAME, WS_EX_APPWINDOW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_POPUP, WS_THICKFRAME,
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
  pub(crate) fn set_cloaked(&self, cloaked: bool) -> crate::Result<()> {
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

  /// Implements [`NativeWindowWindowsExt::add_window_style_ex`].
  pub(crate) fn add_window_style_ex(&self, style: WINDOW_EX_STYLE) {
    let current_style =
      unsafe { GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) };

    #[allow(clippy::cast_possible_wrap)]
    if current_style & style.0 as isize == 0 {
      let new_style = current_style | style.0 as isize;

      unsafe { SetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE, new_style) };
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
      | SWP_SHOWWINDOW
      | SWP_NOMOVE
      | SWP_NOSIZE;

    // Z-order can sometimes still be incorrect after the above call. Make
    // the retry generation-aware so an older focus transition cannot
    // replay after a newer z-order repair has completed.
    let generation = next_z_order_generation();
    let expected_foreground = (!matches!(z_order, WindowZOrder::TopMost))
      .then(|| unsafe { GetForegroundWindow() });
    unsafe { SetWindowPos(self.hwnd(), z_order_hwnd, 0, 0, 0, 0, flags) }?;

    let handle = self.handle;
    task::spawn(async move {
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

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_title_bar_visibility`].
  pub(crate) fn set_title_bar_visibility(
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

    if new_style != style {
      unsafe {
        SetWindowLongPtrW(self.hwnd(), GWL_STYLE, new_style);
        SetWindowPos(
          self.hwnd(),
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
    }

    Ok(())
  }

  /// Implements [`NativeWindowWindowsExt::set_border_color`].
  pub(crate) fn set_border_color(
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
  pub(crate) fn set_transparency(
    &self,
    opacity_value: &OpacityValue,
  ) -> crate::Result<()> {
    // Make the window layered if it isn't already.
    self.add_window_style_ex(WS_EX_LAYERED);

    unsafe {
      SetLayeredWindowAttributes(
        self.hwnd(),
        None,
        opacity_value.to_alpha(),
        LWA_ALPHA,
      )?;
    }

    Ok(())
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
/// subsequent window is placed immediately after the previous one. The
/// delayed retry is generation-aware so a fast native-focus transition
/// cannot replay an obsolete chain after a newer one has been requested.
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
  task::spawn(async move {
    tokio::time::sleep(Duration::from_millis(10)).await;

    if Z_ORDER_GENERATION.load(Ordering::SeqCst) != generation
      || unsafe { GetForegroundWindow() } != HWND(focused_window.0)
    {
      return;
    }

    let _ = apply_z_order_chain(&window_ids);
  });

  Ok(())
}

fn next_z_order_generation() -> u64 {
  Z_ORDER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

fn apply_z_order_chain(window_ids: &[WindowId]) -> crate::Result<()> {
  let flags = SWP_NOACTIVATE
    | SWP_NOCOPYBITS
    | SWP_NOMOVE
    | SWP_NOSIZE
    | SWP_NOOWNERZORDER;

  // A normal WM window must not retain a native TOPMOST bit from a
  // previous state or an application-side z-order change. Demote the
  // complete chain before rebuilding its order; HWND_TOP alone does not
  // clear TOPMOST.
  for window_id in window_ids {
    unsafe {
      SetWindowPos(HWND(window_id.0), HWND_NOTOPMOST, 0, 0, 0, 0, flags)
    }?;
  }

  for (index, window_id) in window_ids.iter().enumerate() {
    let insert_after = index
      .checked_sub(1)
      .and_then(|previous| window_ids.get(previous))
      .map_or(HWND_TOP, |previous| HWND(previous.0));

    unsafe {
      SetWindowPos(HWND(window_id.0), insert_after, 0, 0, 0, 0, flags)
    }?;
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
