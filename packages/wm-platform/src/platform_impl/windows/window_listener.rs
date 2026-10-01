use std::sync::OnceLock;

use tokio::sync::mpsc;
use windows::Win32::{
  Foundation::HWND,
  UI::{
    Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    WindowsAndMessaging::{
      EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
      EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
      EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
      EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
      EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, OBJID_WINDOW,
      WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
    },
  },
};

use super::NativeWindow;
use crate::{Dispatcher, WindowEvent, WindowId};

thread_local! {
  /// Sender for window events. For use with hook procedure.
  static EVENT_TX: OnceLock<mpsc::UnboundedSender<WindowEvent>> = const { OnceLock::new() };
}

/// Platform-specific implementation of [`WindowEventNotification`].
#[derive(Clone, Debug)]
pub struct WindowEventNotificationInner;

/// Platform-specific implementation of [`WindowListener`].
#[derive(Debug)]
pub(crate) struct WindowListener {
  hook_handles: Vec<HWINEVENTHOOK>,
}

impl WindowListener {
  /// Implements [`WindowListener::new`].
  pub(crate) fn new(
    event_tx: mpsc::UnboundedSender<WindowEvent>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let hook_handles = dispatcher.dispatch_sync(move || {
      EVENT_TX.with(|lock| lock.set(event_tx)).map_err(|_| {
        crate::Error::Platform(
          "Window event sender already set.".to_string(),
        )
      })?;

      Self::hook_win_events()
    })??;

    Ok(Self { hook_handles })
  }

  /// Implements [`WindowListener::terminate`].
  pub(crate) fn terminate(&mut self) {
    for handle in self.hook_handles.drain(..) {
      let _ = unsafe { UnhookWinEvent(handle) };
    }
  }

  /// Creates several window event hooks via `SetWinEventHook`.
  ///
  /// Separate hooks are created per event range, which is more performant
  /// than a single hook covering all events.
  fn hook_win_events() -> crate::Result<Vec<HWINEVENTHOOK>> {
    let event_ranges = [
      (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
      (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
      (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
      (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
      (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE),
      (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];

    event_ranges
      .iter()
      .try_fold(Vec::new(), |mut handles, (min, max)| {
        // Create a window hook for the event range.
        let hook_handle = unsafe {
          SetWinEventHook(
            *min,
            *max,
            None,
            Some(Self::window_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
          )
        };

        if hook_handle.is_invalid() {
          return Err(crate::Error::Platform(
            "Failed to set window event hook.".to_string(),
          ));
        }

        handles.push(hook_handle);
        Ok(handles)
      })
  }

  /// Callback passed to `SetWinEventHook`.
  ///
  /// This function is called on selected window events, and forwards them
  /// through an MPSC channel.
  extern "system" fn window_event_proc(
    _hook: HWINEVENTHOOK,
    event_type: u32,
    handle: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
  ) {
    let Some(event) =
      classify_win_event(event_type, handle, id_object, id_child)
    else {
      return;
    };

    let Some(event_tx) = EVENT_TX.with(|lock| lock.get().cloned()) else {
      return;
    };

    if let Err(err) = event_tx.send(event) {
      tracing::warn!("Failed to send window event: {}.", err);
    }
  }
}

/// Classifies one `WinEvent` callback.
///
/// `EVENT_OBJECT_DESTROY` drops cached foreign style before the event
/// is returned. The hook is not filtered by whether GlazeWM manages
/// the window, so an ignored or unmanaged `HWND` is cleared too. A
/// child-control event returns `None` and does not touch that cache.
pub(super) fn classify_win_event(
  event_type: u32,
  handle: HWND,
  id_object: i32,
  id_child: i32,
) -> Option<WindowEvent> {
  let is_window_event =
    id_object == OBJID_WINDOW.0 && id_child == 0 && handle != HWND(0);
  if !is_window_event {
    return None;
  }

  if event_type == EVENT_OBJECT_DESTROY {
    super::native_window::invalidate_foreign_style_state(handle);
  }

  let notification = crate::WindowEventNotification(None);
  match event_type {
    EVENT_OBJECT_DESTROY => Some(WindowEvent::Destroyed {
      window_id: WindowId(handle.0),
      notification,
    }),
    EVENT_SYSTEM_FOREGROUND => Some(WindowEvent::Focused {
      window: NativeWindow::new(handle.0).into(),
      notification,
    }),
    EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
      Some(WindowEvent::Hidden {
        window: NativeWindow::new(handle.0).into(),
        notification,
      })
    }
    EVENT_OBJECT_LOCATIONCHANGE => Some(WindowEvent::MovedOrResized {
      window: NativeWindow::new(handle.0).into(),
      is_interactive_start: false,
      is_interactive_end: false,
      notification,
    }),
    EVENT_SYSTEM_MINIMIZESTART => Some(WindowEvent::Minimized {
      window: NativeWindow::new(handle.0).into(),
      notification,
    }),
    EVENT_SYSTEM_MINIMIZEEND => Some(WindowEvent::MinimizeEnded {
      window: NativeWindow::new(handle.0).into(),
      notification,
    }),
    EVENT_SYSTEM_MOVESIZESTART => Some(WindowEvent::MovedOrResized {
      window: NativeWindow::new(handle.0).into(),
      is_interactive_start: true,
      is_interactive_end: false,
      notification,
    }),
    EVENT_SYSTEM_MOVESIZEEND => Some(WindowEvent::MovedOrResized {
      window: NativeWindow::new(handle.0).into(),
      is_interactive_start: false,
      is_interactive_end: true,
      notification,
    }),
    EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => {
      Some(WindowEvent::Shown {
        window: NativeWindow::new(handle.0).into(),
        notification,
      })
    }
    EVENT_OBJECT_NAMECHANGE => Some(WindowEvent::TitleChanged {
      window: NativeWindow::new(handle.0).into(),
      notification,
    }),
    _ => None,
  }
}

impl Drop for WindowListener {
  fn drop(&mut self) {
    self.terminate();
  }
}
