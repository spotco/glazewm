#[cfg(test)]
use std::sync::atomic::AtomicIsize;
use std::{
  cell::RefCell,
  collections::{HashMap, HashSet},
  sync::{
    atomic::{AtomicU64, Ordering},
    Condvar, Mutex, OnceLock,
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
        GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow,
        IsIconic, IsWindow, IsWindowVisible, IsZoomed,
        SendMessageTimeoutW, SendNotifyMessageW, SetForegroundWindow,
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

#[cfg(test)]
static TEST_FOREGROUND_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// Foreground state used by z-order tests that exercise delayed retries.
///
/// Windows may redirect focus to unrelated desktop windows while test
/// helpers are created or destroyed. Keeping this state controllable makes
/// the retry policy tests independent of the interactive desktop.
#[cfg(test)]
struct TestForegroundWindowOverride {
  previous: isize,
}

#[cfg(test)]
impl TestForegroundWindowOverride {
  fn new(hwnd: HWND) -> Self {
    Self {
      previous: TEST_FOREGROUND_WINDOW.swap(hwnd.0, Ordering::SeqCst),
    }
  }
}

#[cfg(test)]
impl Drop for TestForegroundWindowOverride {
  fn drop(&mut self) {
    TEST_FOREGROUND_WINDOW.store(self.previous, Ordering::SeqCst);
  }
}

fn z_order_foreground_window() -> HWND {
  #[cfg(test)]
  {
    let overridden = TEST_FOREGROUND_WINDOW.load(Ordering::SeqCst);
    if overridden != 0 {
      return HWND(overridden);
    }
  }

  unsafe { GetForegroundWindow() }
}

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

/// Most foreign GUI threads that may each keep one blocked worker.
///
/// A native call already inside another process cannot be cancelled.
/// The worker stays alive until that call returns, then reapplies the
/// latest desired style. One permanently hung `HWND` therefore retains
/// one `glazewm-foreign-gui` thread. Distinct HWNDs do not share that
/// thread. A thread whose `HWND` was destroyed or reused stays counted
/// until that thread returns, so a recycled handle cannot borrow it.
/// Past this cap, further HWNDs keep their desired state but do not
/// start another thread until a slot is free and a later call drives
/// it. This bounds thread growth; it does not make a wedged GUI thread
/// apply the new state.
const FOREIGN_GUI_MAX_BLOCKED_THREADS: usize = 32;

/// Process and GUI thread that owned an `HWND` when its intent was stored.
///
/// Windows reuses numeric handle values. The pair distinguishes the
/// window that created the cache from a later window that received the
/// same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct ForeignWindowOwner {
  process_id: u32,
  thread_id: u32,
}

/// Latest style a foreign worker must leave on an `HWND`.
///
/// `generation` advances on every requested change. `applied_generation`
/// advances only after a worker finishes a native apply of that
/// generation. A worker that was blocked inside an older apply uses the
/// mismatch to reapply the current values. `owner` is the process and
/// GUI thread captured with the entry. A changed or invalid owner drops
/// the entry instead of replaying it.
#[derive(Clone, Default)]
struct ForeignStyleIntent {
  generation: u64,
  applied_generation: u64,
  owner: ForeignWindowOwner,
  /// `Some` once transparency has an opinion about `WS_EX_LAYERED`.
  layered: Option<bool>,
  /// Other extended-style bits that must be present.
  ex_style_or: isize,
  title_bar_visible: Option<bool>,
  alpha: Option<u8>,
}

/// Worker currently applying style for one `HWND`.
struct ForeignStyleWorker {
  ticket: u64,
  owner: ForeignWindowOwner,
}

/// In-flight workers and the desired style they should converge to.
struct ForeignGuiState {
  in_flight: HashMap<isize, ForeignStyleWorker>,
  intents: HashMap<isize, ForeignStyleIntent>,
  next_ticket: u64,
  /// Tickets for workers detached after their `HWND` died or was reused.
  detached_tickets: HashSet<u64>,
}

/// Shared desired-style table.
///
/// The mutex is not held across `SetWindowLongPtrW` or
/// `SetLayeredWindowAttributes`.
fn foreign_gui_sync() -> &'static (Mutex<ForeignGuiState>, Condvar) {
  static STATE: OnceLock<(Mutex<ForeignGuiState>, Condvar)> =
    OnceLock::new();
  STATE.get_or_init(|| {
    (
      Mutex::new(ForeignGuiState {
        in_flight: HashMap::new(),
        intents: HashMap::new(),
        next_ticket: 1,
        detached_tickets: HashSet::new(),
      }),
      Condvar::new(),
    )
  })
}

/// Locks the desired-style table, recovering from a poisoned lock.
fn lock_foreign_gui() -> std::sync::MutexGuard<'static, ForeignGuiState> {
  foreign_gui_sync()
    .0
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// Outcome of driving the desired foreign style.
enum ForeignStyleDrive {
  /// The desired generation was applied before the wait elapsed.
  Applied,
  /// A worker for this `HWND` is already blocked. It reapplies the
  /// new generation when the foreign call returns.
  Deferred,
  /// The worker was still inside the native call when the wait elapsed.
  TimedOut,
  /// No worker was started. The desired state remains recorded.
  Skipped,
}

/// `WS_EX_LAYERED` as a signed extended-style bit.
fn layered_ex_bit() -> isize {
  #[allow(clippy::cast_possible_wrap)]
  {
    WS_EX_LAYERED.0 as isize
  }
}

/// `WS_DLGFRAME` as a signed style bit.
fn dlg_frame_bit() -> isize {
  #[allow(clippy::cast_possible_wrap)]
  {
    WS_DLGFRAME.0 as isize
  }
}

/// Reads the process and GUI thread that currently own `hwnd`.
///
/// Returns `None` when `hwnd` is not a window. A destroyed handle and
/// a handle reused by another window do not report the stored owner.
fn foreign_window_owner(hwnd: HWND) -> Option<ForeignWindowOwner> {
  if !unsafe { IsWindow(hwnd) }.as_bool() {
    return None;
  }
  let mut process_id = 0u32;
  let thread_id =
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)) };
  if thread_id == 0 || process_id == 0 {
    return None;
  }
  Some(ForeignWindowOwner {
    process_id,
    thread_id,
  })
}

/// Workers that still occupy a thread, including ones detached from a
/// dead or reused `HWND`.
fn foreign_style_thread_count(state: &ForeignGuiState) -> usize {
  state.in_flight.len() + state.detached_tickets.len()
}

/// Forgets a worker's claim on `hwnd` without assuming the thread has
/// returned. The thread keeps its ticket until it exits.
fn detach_foreign_style_worker(state: &mut ForeignGuiState, hwnd: isize) {
  if let Some(worker) = state.in_flight.remove(&hwnd) {
    state.detached_tickets.insert(worker.ticket);
  }
}

/// Records that the worker for `ticket` has left its native call.
fn release_foreign_style_worker(
  state: &mut ForeignGuiState,
  hwnd: isize,
  ticket: u64,
) {
  if state
    .in_flight
    .get(&hwnd)
    .is_some_and(|worker| worker.ticket == ticket)
  {
    state.in_flight.remove(&hwnd);
  } else {
    state.detached_tickets.remove(&ticket);
  }
}

/// Drops cached foreign style for a destroyed `HWND`.
///
/// `WindowListener` calls this from `EVENT_OBJECT_DESTROY` before the
/// event is forwarded. That includes ignored and unmanaged windows.
/// The numeric handle can be reused by the same process and GUI
/// thread, which process id and thread id do not distinguish. An
/// in-flight worker is detached, not dropped: its ticket still counts
/// toward [`FOREIGN_GUI_MAX_BLOCKED_THREADS`] until that thread
/// returns, and it must not clear a later entry for the same handle.
/// Process id and thread id checks remain as a backstop when this
/// event is missed.
pub(super) fn invalidate_foreign_style_state(hwnd: HWND) {
  let mut state = lock_foreign_gui();
  state.intents.remove(&hwnd.0);
  detach_foreign_style_worker(&mut state, hwnd.0);
  foreign_gui_sync().1.notify_all();
}

/// Drops cached style when `hwnd` is gone or belongs to another window.
///
/// Returns the live owner. A detached worker remains counted until its
/// thread returns, and it no longer applies this `HWND`. Destroy
/// invalidation is the primary reset. This check covers a missed
/// destroy event when the new window has a different owner.
fn reclaim_foreign_style_owner(
  state: &mut ForeignGuiState,
  hwnd: isize,
) -> Option<ForeignWindowOwner> {
  let owner = foreign_window_owner(HWND(hwnd));
  let intent_stale = state
    .intents
    .get(&hwnd)
    .is_some_and(|intent| owner.is_none_or(|owner| intent.owner != owner));
  if intent_stale {
    state.intents.remove(&hwnd);
  }
  let worker_stale = state
    .in_flight
    .get(&hwnd)
    .is_some_and(|worker| owner.is_none_or(|owner| worker.owner != owner));
  if worker_stale {
    detach_foreign_style_worker(state, hwnd);
  }
  owner
}

/// Whether `intent` is still the desired style for the same window.
fn foreign_style_snapshot_is_current(
  hwnd: HWND,
  intent: &ForeignStyleIntent,
) -> bool {
  let stored_matches = lock_foreign_gui()
    .intents
    .get(&hwnd.0)
    .is_some_and(|stored| {
      stored.generation == intent.generation
        && stored.owner == intent.owner
    });
  stored_matches && foreign_window_owner(hwnd) == Some(intent.owner)
}

/// Applies one snapshot. Returns early when a newer request arrived.
///
/// A call already blocked in the foreign window procedure cannot be
/// cancelled. The caller loops and applies the newer snapshot after
/// this one returns.
fn apply_foreign_style_intent(hwnd: HWND, intent: &ForeignStyleIntent) {
  if !foreign_style_snapshot_is_current(hwnd, intent) {
    return;
  }

  let current_ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
  let mut desired_ex = current_ex | intent.ex_style_or;
  if let Some(layered) = intent.layered {
    let bit = layered_ex_bit();
    if layered {
      desired_ex |= bit;
    } else {
      desired_ex &= !bit;
    }
  }
  if desired_ex != current_ex {
    // SAFETY: Cross-thread `SetWindowLongPtrW` sends
    // `WM_STYLECHANGING` to the window thread. This runs on the
    // worker, not the WM thread.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired_ex) };
  }
  if !foreign_style_snapshot_is_current(hwnd, intent) {
    return;
  }

  let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
  let layered_now = (ex_style & layered_ex_bit()) != 0;
  if layered_now && intent.layered != Some(false) {
    if let Some(alpha) = intent.alpha {
      // `SetLayeredWindowAttributes` does not send to the foreign
      // window procedure, so a `WM_STYLECHANGING` stall cannot hold
      // it. It runs only while this snapshot is still current, which
      // keeps a worker that was blocked in `SetWindowLongPtrW` from
      // writing an older alpha after the GUI thread resumes.
      // SAFETY: `LWA_ALPHA` writes only the opacity byte.
      let _ = unsafe {
        SetLayeredWindowAttributes(hwnd, None, alpha, LWA_ALPHA)
      };
    }
  }
  if !foreign_style_snapshot_is_current(hwnd, intent) {
    return;
  }

  let Some(visible) = intent.title_bar_visible else {
    return;
  };
  let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
  let frame = dlg_frame_bit();
  let new_style = if visible {
    style | frame
  } else {
    style & !frame
  };
  if new_style == style {
    return;
  }
  unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, new_style) };
  // SAFETY: `SWP_NOZORDER` keeps this frame refresh from moving the
  // window in z-order. `SWP_ASYNCWINDOWPOS` does not wait for the
  // foreign queue.
  let _ = unsafe {
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
    )
  };
}

/// Reapplies desired style until the snapshot it finished is current.
///
/// `ticket` is the worker's claim. After the `HWND` is destroyed or
/// reused, the claim is detached and this thread must not apply style
/// or clear a newer worker.
fn foreign_style_worker(hwnd: isize, ticket: u64) {
  let (_lock, cvar) = foreign_gui_sync();
  loop {
    let snapshot = {
      let mut state = lock_foreign_gui();
      let owner = reclaim_foreign_style_owner(&mut state, hwnd);
      let current = state
        .in_flight
        .get(&hwnd)
        .is_some_and(|worker| worker.ticket == ticket);
      let snapshot = state.intents.get(&hwnd).cloned().filter(|intent| {
        current && owner.is_some_and(|owner| intent.owner == owner)
      });
      if snapshot.is_none() {
        release_foreign_style_worker(&mut state, hwnd, ticket);
        cvar.notify_all();
      }
      snapshot
    };
    let Some(snapshot) = snapshot else {
      break;
    };
    apply_foreign_style_intent(HWND(hwnd), &snapshot);
    let mut state = lock_foreign_gui();
    let owns_hwnd = state
      .in_flight
      .get(&hwnd)
      .is_some_and(|worker| worker.ticket == ticket);
    if !owns_hwnd {
      release_foreign_style_worker(&mut state, hwnd, ticket);
      cvar.notify_all();
      break;
    }
    let still_current = state.intents.get(&hwnd).is_some_and(|intent| {
      intent.generation == snapshot.generation
        && intent.owner == snapshot.owner
    }) && foreign_window_owner(HWND(hwnd))
      == Some(snapshot.owner);
    if still_current {
      if let Some(intent) = state.intents.get_mut(&hwnd) {
        intent.applied_generation = snapshot.generation;
      }
      release_foreign_style_worker(&mut state, hwnd, ticket);
      cvar.notify_all();
      break;
    }
    cvar.notify_all();
  }
}

/// Starts or joins the worker for `hwnd` and waits at most
/// [`FOREIGN_GUI_CALL_TIMEOUT`].
///
/// Same-thread writes stay inline. A second request while the worker
/// is blocked only updates the desired generation and returns
/// [`ForeignStyleDrive::Deferred`].
fn drive_foreign_style(window: &NativeWindow) -> ForeignStyleDrive {
  let hwnd = window.hwnd().0;
  if window.gui_thread_is_current() {
    let snapshot = {
      let mut state = lock_foreign_gui();
      let _ = reclaim_foreign_style_owner(&mut state, hwnd);
      state.intents.get(&hwnd).cloned()
    };
    if let Some(snapshot) = snapshot {
      apply_foreign_style_intent(window.hwnd(), &snapshot);
      let mut state = lock_foreign_gui();
      if let Some(intent) = state.intents.get_mut(&hwnd) {
        if intent.generation == snapshot.generation
          && intent.owner == snapshot.owner
        {
          intent.applied_generation = snapshot.generation;
        }
      }
    }
    return ForeignStyleDrive::Applied;
  }
  if !window.foreign_gui_responsive() {
    return ForeignStyleDrive::Skipped;
  }

  let (generation, ticket) = {
    let mut state = lock_foreign_gui();
    let Some(owner) = reclaim_foreign_style_owner(&mut state, hwnd) else {
      return ForeignStyleDrive::Skipped;
    };
    if state.in_flight.contains_key(&hwnd) {
      return ForeignStyleDrive::Deferred;
    }
    if foreign_style_thread_count(&state)
      >= FOREIGN_GUI_MAX_BLOCKED_THREADS
    {
      warn!(
        "Foreign style worker cap ({FOREIGN_GUI_MAX_BLOCKED_THREADS}) is full; hwnd={} keeps its desired state until a worker returns.",
        hwnd_token(HWND(hwnd))
      );
      return ForeignStyleDrive::Skipped;
    }
    let Some(intent) = state.intents.get(&hwnd) else {
      return ForeignStyleDrive::Skipped;
    };
    if intent.owner != owner {
      return ForeignStyleDrive::Skipped;
    }
    let generation = intent.generation;
    let ticket = state.next_ticket;
    state.next_ticket = state.next_ticket.wrapping_add(1);
    state
      .in_flight
      .insert(hwnd, ForeignStyleWorker { ticket, owner });
    (generation, ticket)
  };

  let spawned = thread::Builder::new()
    .name("glazewm-foreign-gui".to_string())
    .spawn(move || foreign_style_worker(hwnd, ticket));
  if spawned.is_err() {
    let mut state = lock_foreign_gui();
    release_foreign_style_worker(&mut state, hwnd, ticket);
    foreign_gui_sync().1.notify_all();
    return ForeignStyleDrive::Skipped;
  }

  let (_lock, cvar) = foreign_gui_sync();
  let guard = lock_foreign_gui();
  let finished =
    cvar.wait_timeout_while(guard, FOREIGN_GUI_CALL_TIMEOUT, |state| {
      let pending = state
        .intents
        .get(&hwnd)
        .is_some_and(|intent| intent.applied_generation != generation);
      pending && state.in_flight.contains_key(&hwnd)
    });
  match finished {
    Ok((_, wait)) if !wait.timed_out() => ForeignStyleDrive::Applied,
    _ => ForeignStyleDrive::TimedOut,
  }
}

/// Records `label` when the WM-facing call did not observe a finished
/// apply.
fn finish_foreign_style_drive(drive: &ForeignStyleDrive) {
  match drive {
    ForeignStyleDrive::Applied => {}
    ForeignStyleDrive::Deferred => mark_native_op_result("deferred"),
    ForeignStyleDrive::TimedOut | ForeignStyleDrive::Skipped => {
      mark_native_op_result("skip-unresponsive");
    }
  }
}

/// Whether a foreign style worker has not yet returned for `hwnd`.
#[cfg(test)]
fn foreign_style_in_flight(hwnd: isize) -> bool {
  lock_foreign_gui().in_flight.contains_key(&hwnd)
}

/// Waits until the worker for `hwnd` has finished, including a stale
/// apply that resumed and reconciled.
#[cfg(test)]
fn wait_until_foreign_style_idle(hwnd: isize, timeout: Duration) -> bool {
  let (lock, cvar) = foreign_gui_sync();
  let guard = lock
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  match cvar.wait_timeout_while(guard, timeout, |state| {
    state.in_flight.contains_key(&hwnd)
  }) {
    Ok((guard, wait)) => {
      !wait.timed_out() && !guard.in_flight.contains_key(&hwnd)
    }
    Err(poisoned) => {
      !poisoned.into_inner().0.in_flight.contains_key(&hwnd)
    }
  }
}

/// Copies the cached style for `hwnd`.
#[cfg(test)]
fn foreign_style_intent_clone(hwnd: isize) -> Option<ForeignStyleIntent> {
  lock_foreign_gui().intents.get(&hwnd).cloned()
}

/// Stores `intent` under `hwnd` without checking its owner.
#[cfg(test)]
fn replace_foreign_style_intent(hwnd: isize, intent: ForeignStyleIntent) {
  lock_foreign_gui().intents.insert(hwnd, intent);
}

/// Cached foreign-style bookkeeping for one `HWND`.
#[cfg(test)]
struct ForeignStyleCacheView {
  intent: Option<ForeignStyleIntent>,
  in_flight_ticket: Option<u64>,
  detached_tickets: HashSet<u64>,
  thread_count: usize,
}

/// Reads the foreign-style bookkeeping for `hwnd`.
#[cfg(test)]
fn foreign_style_cache_view(hwnd: isize) -> ForeignStyleCacheView {
  let state = lock_foreign_gui();
  ForeignStyleCacheView {
    intent: state.intents.get(&hwnd).cloned(),
    in_flight_ticket: state
      .in_flight
      .get(&hwnd)
      .map(|worker| worker.ticket),
    detached_tickets: state.detached_tickets.clone(),
    thread_count: foreign_style_thread_count(&state),
  }
}

/// Inserts an in-flight claim without starting a thread.
#[cfg(test)]
fn plant_foreign_style_worker(
  hwnd: isize,
  ticket: u64,
  owner: ForeignWindowOwner,
) {
  lock_foreign_gui()
    .in_flight
    .insert(hwnd, ForeignStyleWorker { ticket, owner });
}

/// Removes test-only bookkeeping for `hwnd`.
///
/// A planted replacement ticket is not a real thread. Leaving it in
/// the table would consume a slot in later tests in this process.
#[cfg(test)]
fn forget_foreign_style_hwnd(hwnd: isize) {
  let mut state = lock_foreign_gui();
  state.intents.remove(&hwnd);
  if let Some(worker) = state.in_flight.remove(&hwnd) {
    state.detached_tickets.remove(&worker.ticket);
  }
}

/// Waits until `ticket` is neither detached nor in flight.
#[cfg(test)]
fn wait_until_foreign_style_ticket_released(
  ticket: u64,
  timeout: Duration,
) -> bool {
  let (lock, cvar) = foreign_gui_sync();
  let guard = lock
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  let still_held = |state: &mut ForeignGuiState| {
    state.detached_tickets.contains(&ticket)
      || state
        .in_flight
        .values()
        .any(|worker| worker.ticket == ticket)
  };
  match cvar.wait_timeout_while(guard, timeout, still_held) {
    Ok((guard, wait)) => {
      !wait.timed_out()
        && !guard.detached_tickets.contains(&ticket)
        && !guard
          .in_flight
          .values()
          .any(|worker| worker.ticket == ticket)
    }
    Err(poisoned) => {
      let guard = poisoned.into_inner().0;
      !guard.detached_tickets.contains(&ticket)
        && !guard
          .in_flight
          .values()
          .any(|worker| worker.ticket == ticket)
    }
  }
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
      warn!(
        "Failed to get window's frame position. Falling back to border position."
      );
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
    #[allow(clippy::cast_possible_wrap)]
    let bit = style.0 as isize;
    if !self.record_ex_style_bit(bit) {
      mark_native_op_result("unchanged");
      return;
    }
    self.drive_recorded_style();
  }

  /// Records an extended-style bit that must be present.
  ///
  /// Returns false when the bit is already present and no in-flight
  /// worker is going to clear it.
  fn record_ex_style_bit(&self, bit: isize) -> bool {
    let present = self.has_window_style_ex_bit(bit);
    let hwnd = self.hwnd().0;
    let mut state = lock_foreign_gui();
    let Some(owner) = reclaim_foreign_style_owner(&mut state, hwnd) else {
      return false;
    };
    let in_flight = state.in_flight.contains_key(&hwnd);
    let intent =
      state
        .intents
        .entry(hwnd)
        .or_insert_with(|| ForeignStyleIntent {
          owner,
          ..ForeignStyleIntent::default()
        });
    let layered = bit == layered_ex_bit();
    let desired_on = intent.ex_style_or & bit == bit
      && (!layered || intent.layered == Some(true));
    let settled =
      !in_flight && intent.applied_generation == intent.generation;
    if desired_on && (in_flight || (present && settled)) {
      return false;
    }
    if present && settled && (!layered || intent.layered != Some(false)) {
      return false;
    }
    intent.ex_style_or |= bit;
    if layered {
      intent.layered = Some(true);
    }
    intent.generation = intent.generation.wrapping_add(1);
    true
  }

  /// Whether `bit` is set in `GWL_EXSTYLE`.
  fn has_window_style_ex_bit(&self, bit: isize) -> bool {
    (unsafe { GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) } & bit) != 0
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
  /// [`NativeWindow::add_window_style_ex`]. A worker that resumes
  /// after the wait reapplies the latest title-bar request. The
  /// following `SetWindowPos` stays asynchronous and does not change
  /// z-order.
  pub(crate) fn set_title_bar_visibility(
    &self,
    visible: bool,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_title_bar_visibility", || {
      self.set_title_bar_visibility_inner(visible);
      Ok(())
    })
  }

  fn set_title_bar_visibility_inner(&self, visible: bool) {
    if !self.record_title_bar(visible) {
      mark_native_op_result("unchanged");
      return;
    }
    self.drive_recorded_style();
  }

  /// Records the latest title-bar visibility.
  ///
  /// Returns false when the frame bit already matches and no worker
  /// is about to apply the opposite value.
  fn record_title_bar(&self, visible: bool) -> bool {
    let style = unsafe { GetWindowLongPtrW(self.hwnd(), GWL_STYLE) };
    let has_frame = (style & dlg_frame_bit()) != 0;
    let hwnd = self.hwnd().0;
    let mut state = lock_foreign_gui();
    let Some(owner) = reclaim_foreign_style_owner(&mut state, hwnd) else {
      return false;
    };
    let in_flight = state.in_flight.contains_key(&hwnd);
    let intent =
      state
        .intents
        .entry(hwnd)
        .or_insert_with(|| ForeignStyleIntent {
          owner,
          ..ForeignStyleIntent::default()
        });
    let settled =
      !in_flight && intent.applied_generation == intent.generation;
    if intent.title_bar_visible == Some(visible)
      && (in_flight || (has_frame == visible && settled))
    {
      return false;
    }
    if has_frame == visible && settled {
      return false;
    }
    intent.title_bar_visible = Some(visible);
    intent.generation = intent.generation.wrapping_add(1);
    true
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
  /// that is already layered still receives `opacity_value`. The
  /// foreign apply runs on a worker. If that worker resumes after a
  /// newer request, it writes the newer alpha and layered bit.
  pub(crate) fn set_transparency(
    &self,
    opacity_value: &OpacityValue,
  ) -> crate::Result<()> {
    timed_native_op(self.hwnd(), "set_transparency", || {
      self.set_transparency_inner(opacity_value);
      Ok(())
    })
  }

  fn set_transparency_inner(&self, opacity_value: &OpacityValue) {
    if !self.record_transparency(opacity_value.to_alpha()) {
      mark_native_op_result("unchanged");
      return;
    }
    self.drive_recorded_style();
  }

  /// Records the latest layered-alpha request.
  ///
  /// Alpha 255 on a window that is not layered, and that has no
  /// in-flight request to become layered, is a no-op. Alpha 255 while
  /// a worker may still add `WS_EX_LAYERED` records "not layered" so
  /// that worker removes the bit when it resumes.
  fn record_transparency(&self, alpha: u8) -> bool {
    let has_layered = self.has_window_style_ex(WS_EX_LAYERED);
    let hwnd = self.hwnd().0;
    let mut state = lock_foreign_gui();
    let Some(owner) = reclaim_foreign_style_owner(&mut state, hwnd) else {
      return false;
    };
    let in_flight = state.in_flight.contains_key(&hwnd);
    let intent =
      state
        .intents
        .entry(hwnd)
        .or_insert_with(|| ForeignStyleIntent {
          owner,
          ..ForeignStyleIntent::default()
        });
    let pending_layer = intent.layered == Some(true)
      && (in_flight || intent.applied_generation != intent.generation);

    if alpha == u8::MAX && !has_layered && !pending_layer {
      return false;
    }
    if alpha == u8::MAX && !has_layered && pending_layer {
      intent.layered = Some(false);
      intent.alpha = None;
      intent.ex_style_or &= !layered_ex_bit();
      intent.generation = intent.generation.wrapping_add(1);
      return true;
    }

    if intent.layered == Some(true)
      && intent.alpha == Some(alpha)
      && has_layered
      && !in_flight
      && intent.applied_generation == intent.generation
    {
      return false;
    }
    intent.layered = Some(true);
    intent.alpha = Some(alpha);
    intent.ex_style_or |= layered_ex_bit();
    intent.generation = intent.generation.wrapping_add(1);
    true
  }

  /// Drives the recorded intent and logs when the caller did not see it
  /// finish.
  fn drive_recorded_style(&self) {
    let drive = drive_foreign_style(self);
    if matches!(
      drive,
      ForeignStyleDrive::TimedOut | ForeignStyleDrive::Skipped
    ) {
      warn!(
        "Skipped style apply for unresponsive hwnd={}.",
        hwnd_token(self.hwnd())
      );
    }
    finish_foreign_style_drive(&drive);
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
    is_hung: Some(unsafe { IsHungAppWindow(hwnd) }.as_bool()),
    errors,
  }
}

/// Returns top-to-bottom z-order ranks for the requested window IDs.
///
/// Rank 0 is closest to the top of the desktop among currently enumerated
/// top-level windows. Missing HWNDs are omitted. Used by verbose z-order
/// diagnostics (`GLAZEWM_VERBOSE_Z_ORDER=1`).
pub(crate) fn sample_z_order_ranks(
  window_ids: &[WindowId],
) -> Vec<(WindowId, u32)> {
  if window_ids.is_empty() {
    return Vec::new();
  }

  let wanted: HashSet<isize> =
    window_ids.iter().map(|window_id| window_id.0).collect();
  let mut handles: Vec<isize> = Vec::new();

  #[allow(clippy::items_after_statements)]
  extern "system" fn sample_proc(handle: HWND, data: LPARAM) -> BOOL {
    let handles = data.0 as *mut Vec<isize>;
    unsafe { (*handles).push(handle.0) };
    true.into()
  }

  let _ = unsafe {
    EnumWindows(
      Some(sample_proc),
      LPARAM(std::ptr::from_mut(&mut handles) as _),
    )
  };

  let mut ranks = Vec::new();
  for (index, handle) in handles.into_iter().enumerate() {
    if wanted.contains(&handle) {
      #[allow(clippy::cast_possible_truncation)]
      ranks.push((WindowId(handle), index as u32));
    }
  }
  ranks
}

fn z_order_chain_matches(window_ids: &[WindowId]) -> bool {
  let ranks = sample_z_order_ranks(window_ids);
  let present_ids: HashSet<WindowId> =
    ranks.iter().map(|(window_id, _)| *window_id).collect();
  let expected: Vec<WindowId> = window_ids
    .iter()
    .filter(|window_id| present_ids.contains(window_id))
    .copied()
    .collect();
  let actual: Vec<WindowId> =
    ranks.into_iter().map(|(window_id, _)| window_id).collect();
  actual == expected
}

fn foreground_is_in_z_order_chain(window_ids: &[WindowId]) -> bool {
  let foreground = z_order_foreground_window().0;
  window_ids.iter().any(|window_id| window_id.0 == foreground)
}

/// Reorders a normal-window chain without changing focus or visibility.
///
/// The first usable window is placed at the top of the normal z-order.
/// Each subsequent usable window is placed immediately after the previous
/// usable one. Hung / non-pumping and non-enumerated hwnds are skipped so
/// they cannot block the WM loop or anchor peers below floaters. Because
/// asynchronous requests to independent GUI queues can complete out of
/// order, a generation-aware worker re-applies the chain until native
/// ranks remain correct across two samples. When any hwnd is skipped,
/// delayed recovery retries re-apply the same chain (100/250/500ms then
/// once per second) so a later-responsive window eventually joins full
/// order. Workers stop when a newer z-order generation starts or the
/// foreground leaves this workspace chain. Recovery also requires the
/// original chain head to remain foreground.
pub(crate) fn reorder_z_order(
  window_ids: &[WindowId],
) -> crate::Result<()> {
  if window_ids.is_empty() {
    return Ok(());
  }

  let generation = next_z_order_generation();
  let initial_result = apply_z_order_chain(window_ids)?;
  let skipped_windows = initial_result.skipped_windows;

  let window_ids = window_ids.to_vec();
  if let Ok(runtime) = Handle::try_current() {
    let focused_window = window_ids[0];
    let retry_ids = window_ids.clone();
    runtime.spawn(async move {
      const VERIFY_DELAYS_MS: &[u64] = &[10, 25, 50, 100, 250, 500, 1000];
      let stale = || {
        Z_ORDER_GENERATION.load(Ordering::SeqCst) != generation
          || !foreground_is_in_z_order_chain(&retry_ids)
      };
      let mut delay_index = 0;
      let mut stable_sample_seen = false;
      let mut apply_result = initial_result;

      loop {
        tokio::time::sleep(Duration::from_millis(
          VERIFY_DELAYS_MS[delay_index],
        ))
        .await;
        if stale() {
          return;
        }

        if !apply_result.skipped_windows
          && z_order_chain_matches(&apply_result.responsive_window_ids)
        {
          if stable_sample_seen {
            return;
          }
          stable_sample_seen = true;
        } else {
          stable_sample_seen = false;
          match apply_z_order_chain(&retry_ids) {
            Ok(result) => apply_result = result,
            Err(_) => return,
          }
          if apply_result.skipped_windows {
            // A probe can briefly time out while a pumping window handles
            // an earlier queued position change. Wait for
            // those queues to drain before probing again
            // instead of escalating the normal convergence
            // backoff.
            delay_index = delay_index.max(2);
          }
        }

        delay_index = (delay_index + 1).min(VERIFY_DELAYS_MS.len() - 1);
      }
    });

    if skipped_windows {
      // Early absolute checkpoints, then low-frequency 1s backoff until
      // the full chain applies or generation/focus cancels. TID-cached
      // WM_NULL probes keep each attempt cheap. Recovery requires the
      // original chain head to remain foreground so an old skipped target
      // cannot rejoin after focus moves elsewhere.
      const SKIPPED_WINDOW_RECOVERY_EARLY_AT_MS: &[u64] = &[100, 250, 500];
      let recovery_ids = window_ids;
      runtime.spawn(async move {
        let started = tokio::time::Instant::now();
        let stale = move || {
          Z_ORDER_GENERATION.load(Ordering::SeqCst) != generation
            || z_order_foreground_window() != HWND(focused_window.0)
        };
        for &at_ms in SKIPPED_WINDOW_RECOVERY_EARLY_AT_MS {
          let target = started + Duration::from_millis(at_ms);
          tokio::time::sleep_until(target).await;
          if stale() {
            return;
          }
          match apply_z_order_chain(&recovery_ids) {
            Ok(result) if result.skipped_windows => {}
            Ok(_) | Err(_) => return,
          }
        }
        loop {
          tokio::time::sleep(Duration::from_secs(1)).await;
          if stale() {
            return;
          }
          match apply_z_order_chain(&recovery_ids) {
            Ok(result) if result.skipped_windows => {}
            Ok(_) | Err(_) => return,
          }
        }
      });
    }
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

/// Whether `hwnd` can accept a synchronous z-order change without
/// blocking.
///
/// Same-thread windows are treated as responsive. Cross-thread targets
/// must answer `WM_NULL` within [`FOREIGN_GUI_PROBE_TIMEOUT_MS`]. Hung /
/// debugger suspended helpers fail this probe and must be skipped by the
/// chain apply. Probe responsiveness, caching by GUI thread id for one
/// chain apply.
///
/// Multiple hwnds sharing a TID share one `WM_NULL` result so a hung
/// helper with several owned windows does not cost N×50ms probes.
fn hwnd_gui_responsive_with_tid_cache(
  hwnd: HWND,
  tid_cache: &mut Option<HashMap<u32, bool>>,
) -> bool {
  if !unsafe { IsWindow(hwnd) }.as_bool() {
    return false;
  }

  let mut process_id = 0u32;
  let thread_id =
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)) };
  if thread_id == 0 {
    return false;
  }
  if thread_id == unsafe { GetCurrentThreadId() } {
    return true;
  }

  if let Some(cache) = tid_cache.as_mut() {
    if let Some(&cached) = cache.get(&thread_id) {
      return cached;
    }
  }

  let mut result = 0usize;
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
  let responsive = returned.0 != 0;
  if let Some(cache) = tid_cache.as_mut() {
    cache.insert(thread_id, responsive);
  }
  responsive
}

/// Pure cache lookup helper for unit tests (avoids N×50ms probes per TID).
#[cfg(test)]
fn tid_probe_cache_get_or_insert(
  cache: &mut HashMap<u32, bool>,
  thread_id: u32,
  mut probe: impl FnMut() -> bool,
) -> bool {
  if let Some(&cached) = cache.get(&thread_id) {
    return cached;
  }
  let result = probe();
  cache.insert(thread_id, result);
  result
}

/// Planned insert-after target for one usable window in a chain.
///
/// Hung and non-enumerated predecessors are skipped so they cannot anchor
/// responsive peers below floaters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZOrderInsertAfter {
  Top,
  After(WindowId),
}

/// Resolves insert-after for `index`, skipping unusable predecessors.
fn planned_insert_after(
  window_ids: &[WindowId],
  usable: &[bool],
  index: usize,
) -> ZOrderInsertAfter {
  debug_assert_eq!(window_ids.len(), usable.len());
  for previous in (0..index).rev() {
    if usable[previous] {
      return ZOrderInsertAfter::After(window_ids[previous]);
    }
  }
  ZOrderInsertAfter::Top
}

struct ZOrderApplyResult {
  skipped_windows: bool,
  responsive_window_ids: Vec<WindowId>,
}

/// Applies the z-order chain. Reports responsive hwnds separately from
/// skipped windows so convergence checks only include windows that this
/// pass could place.
fn apply_z_order_chain(
  window_ids: &[WindowId],
) -> crate::Result<ZOrderApplyResult> {
  // Keep SWP_ASYNCWINDOWPOS so a slow-but-pumping foreign queue (or a hung
  // helper) cannot block the WM loop. Prefer ASYNC over sync SetWindowPos
  // even for responsive hwnds — a slow pump must never stall the WM
  // thread (plan: keep SWP_ASYNCWINDOWPOS; code is right). Hung /
  // non-pumping hwnds are skipped as placement targets *and* as
  // insert-after anchors so responsive tiles still rise to HWND_TOP above
  // floaters (layout.log hung helper 2168898).
  // Only clear TOPMOST when present — unconditional HWND_NOTOPMOST raises
  // normal floaters and races the follow-up insert_after under ASYNC.
  let flags = SWP_NOACTIVATE
    | SWP_NOCOPYBITS
    | SWP_NOMOVE
    | SWP_NOSIZE
    | SWP_ASYNCWINDOWPOS
    | SWP_NOOWNERZORDER;

  // Only use HWNDs that are present in EnumWindows as placement targets or
  // insert-after anchors. Some notification-band HWNDs answer WM_NULL but
  // are not part of the normal top-level z-order; SetWindowPos after one
  // of those HWNDs can lift the next window above the intended chain.
  let enumerated_window_ids: HashSet<WindowId> =
    sample_z_order_ranks(window_ids)
      .into_iter()
      .map(|(window_id, _)| window_id)
      .collect();
  let mut tid_cache: Option<HashMap<u32, bool>> = Some(HashMap::new());
  let mut usable = Vec::with_capacity(window_ids.len());
  let mut responsive_window_ids = Vec::new();

  for window_id in window_ids {
    let hwnd = HWND(window_id.0);
    let is_usable = enumerated_window_ids.contains(window_id)
      && hwnd_gui_responsive_with_tid_cache(hwnd, &mut tid_cache);
    usable.push(is_usable);
    if is_usable {
      responsive_window_ids.push(*window_id);
    }
  }

  for (index, window_id) in window_ids.iter().enumerate() {
    if !usable[index] {
      continue;
    }
    let hwnd = HWND(window_id.0);

    #[allow(clippy::cast_possible_wrap)]
    let is_topmost = (unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }
      & WS_EX_TOPMOST.0 as isize)
      != 0;
    if is_topmost {
      unsafe {
        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags)?;
      }
    }

    let insert_after =
      match planned_insert_after(window_ids, &usable, index) {
        ZOrderInsertAfter::Top => HWND_TOP,
        ZOrderInsertAfter::After(window_id) => HWND(window_id.0),
      };
    unsafe {
      SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags)?;
    }
  }

  Ok(ZOrderApplyResult {
    skipped_windows: usable.iter().any(|is_usable| !is_usable),
    responsive_window_ids,
  })
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
        SetLayeredWindowAttributes, SetWindowLongPtrW, TranslateMessage,
        UnregisterClassW, EVENT_OBJECT_DESTROY, EVENT_OBJECT_SHOW,
        GWL_EXSTYLE, GWL_STYLE, GW_HWNDNEXT,
        LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, MSG, OBJID_WINDOW,
        WINDOW_EX_STYLE, WM_NULL, WM_WINDOWPOSCHANGING, WNDCLASSW,
        WS_DLGFRAME, WS_EX_LAYERED, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
      },
    },
  };

  use super::reorder_z_order;
  use crate::{
    Color, CornerStyle, OpacityValue, Rect, WindowEvent, WindowId,
    WindowZOrder,
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

  /// Creates a visible window, then waits on `GLAZEWM_RESUME_EVENT` before
  /// pumping. Until the parent signals the event, `WM_NULL` probes fail
  /// (thread is asleep, not in `GetMessage`) — unlike `SuspendThread` on a
  /// pumping foreign GUI, which does not reliably fail same-desktop
  /// probes.
  fn create_deferred_pump_window() {
    use std::io::Write;

    use windows::Win32::{
      Foundation::CloseHandle,
      System::Threading::{
        OpenEventW, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
      },
    };

    let class_name =
      wide(&format!("GlazeWmDeferredPump{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register deferred-pump helper class");
    let title = wide("GlazeWM deferred-pump z-order helper");
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
    assert_ne!(hwnd.0, 0, "create deferred-pump helper window");
    println!("GLAZEWM_Z_ORDER_HWND:{}", hwnd.0);
    std::io::stdout().flush().expect("flush deferred-pump HWND");

    let name = std::env::var("GLAZEWM_RESUME_EVENT")
      .expect("GLAZEWM_RESUME_EVENT for deferred-pump");
    let wide_name = wide(&name);
    let handle = unsafe {
      OpenEventW(
        SYNCHRONIZATION_ACCESS_RIGHTS(0x001F_0003),
        false,
        PCWSTR(wide_name.as_ptr()),
      )
    }
    .expect("open deferred-pump resume event");
    // Sleep-wait (not GetMessage) so WM_NULL probes time out until
    // release.
    unsafe { WaitForSingleObject(handle, 120_000) };
    unsafe {
      let _ = CloseHandle(handle);
    }

    let mut message = MSG::default();
    while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool() {
      unsafe {
        TranslateMessage(&raw const message);
        DispatchMessageW(&raw const message);
      }
    }
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

  /// Armed after creation. Cross-thread sends other than `WM_NULL`
  /// wait until the parent signals the named event, then run.
  static RESUME_GATE_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

  unsafe extern "system" fn resume_gate_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> LRESULT {
    if RESUME_GATE_ARMED.load(std::sync::atomic::Ordering::SeqCst)
      && unsafe { InSendMessage() }.as_bool()
      && msg != WM_NULL
    {
      if let Ok(name) = std::env::var("GLAZEWM_RESUME_EVENT") {
        use windows::Win32::{
          Foundation::CloseHandle,
          System::Threading::{
            OpenEventW, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
          },
        };
        // `EVENT_ALL_ACCESS`. The Storage feature that names it is off.
        let wide_name = wide(&name);
        match unsafe {
          OpenEventW(
            SYNCHRONIZATION_ACCESS_RIGHTS(0x001F_0003),
            false,
            PCWSTR(wide_name.as_ptr()),
          )
        } {
          Ok(handle) => {
            // Parent signals this after publishing the newer desired
            // state.
            unsafe { WaitForSingleObject(handle, 30_000) };
            unsafe {
              let _ = CloseHandle(handle);
            }
          }
          Err(err) => {
            eprintln!("resume-gate OpenEventW failed: {err}");
          }
        }
      }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
  }

  /// Pumps messages, answers `WM_NULL`, and holds other sent messages
  /// until the parent releases the gate.
  fn create_resume_gate_window() {
    use std::io::Write;

    RESUME_GATE_ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
    let class_name =
      wide(&format!("GlazeWmResumeGate{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(resume_gate_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register resume-gate helper class");

    let title = wide("GlazeWM resume-gate helper");
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW,
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
    assert_ne!(hwnd.0, 0, "create resume-gate helper window");
    if std::env::var("GLAZEWM_HELPER_LAYERED").ok().as_deref() == Some("1")
    {
      super::NativeWindow::new(hwnd.0)
        .set_transparency(&OpacityValue::from_alpha(200))
        .expect("seed layered alpha");
    }
    // Let a running GlazeWM finish its own create-time style calls
    // before the gate starts holding sent messages.
    let pump_until =
      std::time::Instant::now() + std::time::Duration::from_millis(300);
    let mut message = MSG::default();
    while std::time::Instant::now() < pump_until {
      let found = unsafe {
        windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
          &raw mut message,
          None,
          0,
          0,
          windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
      };
      if found.as_bool() {
        unsafe {
          TranslateMessage(&raw const message);
          DispatchMessageW(&raw const message);
        }
      } else {
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
    }
    RESUME_GATE_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
    println!("GLAZEWM_Z_ORDER_HWND:{}", hwnd.0);
    std::io::stdout().flush().expect("flush helper HWND");

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
  fn resume_gate_helper_window() {
    if helper_mode("resume-gate") {
      create_resume_gate_window();
    }
  }

  #[test]
  fn deferred_pump_helper_window() {
    if helper_mode("deferred-pump") {
      create_deferred_pump_window();
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

  #[test]
  fn planned_insert_after_skips_hung_predecessors() {
    let hung = WindowId(1);
    let tile = WindowId(2);
    let floater = WindowId(3);
    let chain = [hung, tile, floater];
    let responsive = [false, true, true];

    assert_eq!(
      super::planned_insert_after(&chain, &responsive, 0),
      super::ZOrderInsertAfter::Top
    );
    assert_eq!(
      super::planned_insert_after(&chain, &responsive, 1),
      super::ZOrderInsertAfter::Top,
      "first responsive after hung focused tile must rise to HWND_TOP"
    );
    assert_eq!(
      super::planned_insert_after(&chain, &responsive, 2),
      super::ZOrderInsertAfter::After(tile)
    );
  }

  /// layout.log 2026-10-02 22:35:50.830 ET and 22:35:53.793 ET:
  /// focused Grok `131888` had `bring_to_front` skipped
  /// (`tiling_group_defer_workspace_reorder`). The chain was
  /// Grok, tiles, floater, toast `984154` (`EnumWindows` rank `=?`),
  /// Notepad `67996`. Post-reorder Notepad was above Grok; +25ms
  /// Notepad snapped to rank 8. At 22:35:59 the toast left the chain
  /// and the same Grok focus kept Grok above Notepad.
  ///
  /// The toast still answers `WM_NULL`, so it is not skipped as hung.
  /// It is also missing from `EnumWindows`, so it must not be treated as
  /// an insert-after anchor. Native `SetWindowPos` after a live hwnd that
  /// is missing from the top-level z-order (notification z-band) places
  /// Notepad at the top of the normal band. Desired anchor is Grok.
  #[test]
  fn insert_after_skips_unenumerated_notification_before_notepad() {
    let grok = WindowId(131_888);
    let notification = WindowId(984_154);
    let notepad = WindowId(67_996);
    let chain = [grok, notification, notepad];
    let enumerated = std::collections::HashSet::from([grok, notepad]);
    let usable = chain.map(|window_id| enumerated.contains(&window_id));

    assert_eq!(
      super::planned_insert_after(&chain, &usable, 2),
      super::ZOrderInsertAfter::After(grok),
      "Notepad must not insert after toast 984154; that hwnd is not in EnumWindows and lifts Notepad above focused Grok"
    );
  }

  #[test]
  fn tid_probe_cache_reuses_result_per_thread() {
    let mut probes = 0usize;
    let mut cache = std::collections::HashMap::new();
    let mut probe = || {
      probes += 1;
      false
    };

    assert!(!super::tid_probe_cache_get_or_insert(
      &mut cache, 42, &mut probe
    ));
    assert!(
      !super::tid_probe_cache_get_or_insert(&mut cache, 42, &mut probe),
      "same TID must not re-probe"
    );
    assert!(!super::tid_probe_cache_get_or_insert(
      &mut cache, 99, &mut probe
    ));
    assert_eq!(probes, 2, "one probe per distinct TID");
  }

  #[test]
  fn reorder_z_order_raises_responsive_tiles_above_floaters_when_focused_tile_is_hung(
  ) {
    use std::time::Duration;

    // Cross-process layout matching layout.log hung helper @ 05:02Z:
    // hung tiled focus + responsive tile peer + foreign floater (Steam).
    let current_exe = std::env::current_exe().expect("test executable");
    let mut hung_helper = std::process::Command::new(&current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "window")
      .arg("z_order_test_helper_window")
      .arg("--nocapture")
      .stdout(std::process::Stdio::piped())
      .spawn()
      .expect("spawn hung helper");
    let hung = read_helper_hwnd(&mut hung_helper, "GLAZEWM_Z_ORDER_HWND:");

    let (mut tile_helper, tile) =
      spawn_single_pumping_window_helper(false, 0);
    let (mut floater_helper, floater) =
      spawn_single_pumping_window_helper(false, 0);

    // Steam-on-top starting shape.
    let initial = [floater.0, tile.0, hung.0];
    reorder_z_order(&[
      WindowId(floater.0),
      WindowId(tile.0),
      WindowId(hung.0),
    ])
    .expect("seed floater-on-top order");
    std::thread::sleep(Duration::from_millis(50));

    // Super+arrow onto hung tiled helper.
    let intended = [hung.0, tile.0, floater.0];
    reorder_z_order(&[
      WindowId(hung.0),
      WindowId(tile.0),
      WindowId(floater.0),
    ])
    .expect("reorder with hung focused tile");

    // Fixed apply skips the hung hwnd as an anchor and avoids raising
    // floaters via HWND_NOTOPMOST, so the responsive tile reaches HWND_TOP
    // without waiting on the 10ms retry. Allow a short foreign-queue drain
    // (well under the old +25ms failure window).
    use std::time::Instant;
    let deadline = Instant::now() + Duration::from_millis(40);
    let mut order = relative_order(&[hung, tile, floater]);
    let mut tile_above_floater = false;
    while Instant::now() < deadline {
      order = relative_order(&[hung, tile, floater]);
      tile_above_floater = order
        .iter()
        .position(|id| *id == tile.0)
        .zip(order.iter().position(|id| *id == floater.0))
        .is_some_and(|(t, f)| t < f);
      if tile_above_floater {
        break;
      }
      std::thread::sleep(Duration::from_millis(2));
    }

    let _ = hung_helper.kill();
    let _ = hung_helper.wait();
    let _ = tile_helper.kill();
    let _ = tile_helper.wait();
    let _ = floater_helper.kill();
    let _ = floater_helper.wait();

    assert!(
      tile_above_floater,
      "responsive foreign tile must rise above foreign floater when focused        tile is hung (no 10ms/25ms race); initial={initial:?}        intended={intended:?} actual={order:?}"
    );
  }

  #[test]
  fn hung_hwnd_eventually_rejoins_chain_after_resume() {
    use std::time::{Duration, Instant};

    // deferred-pump hung -> reorder (skip) -> release during early
    // checkpoints -> recovery.
    let hung_gate = spawn_deferred_pump_helper();
    let hung = hung_gate.hwnd;
    let (mut tile_helper, tile) =
      spawn_single_pumping_window_helper(false, 0);
    let (mut floater_helper, floater) =
      spawn_single_pumping_window_helper(false, 0);
    let _foreground_override =
      super::TestForegroundWindowOverride::new(hung);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
      || {
        runtime.block_on(async {
        reorder_z_order(&[WindowId(floater.0), WindowId(tile.0)])
          .expect("seed floater-on-top");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
          super::z_order_foreground_window().0,
          hung.0,
          "foreground left hung helper after seed reorder"
        );

        reorder_z_order(&[
          WindowId(hung.0),
          WindowId(tile.0),
          WindowId(floater.0),
        ])
        .expect("reorder while focused tile GUI is not pumping");

        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
          super::z_order_foreground_window().0,
          hung.0,
          "foreground left hung helper after hung reorder"
        );
        let tile_deadline = Instant::now() + Duration::from_millis(100);
        let mut mid = relative_order(&[hung, tile, floater]);
        let mut tile_above = false;
        while Instant::now() < tile_deadline {
          mid = relative_order(&[hung, tile, floater]);
          tile_above = mid
            .iter()
            .position(|id| *id == tile.0)
            .zip(mid.iter().position(|id| *id == floater.0))
            .is_some_and(|(t, f)| t < f);
          if tile_above {
            break;
          }
          tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
          tile_above,
          "before resume, responsive tile must beat floater: {mid:?}"
        );
        assert_ne!(
          mid.first().copied(),
          Some(hung.0),
          "hung hwnd must stay skipped before resume: {mid:?}"
        );

        hung_gate.release();

        let deadline = Instant::now() + Duration::from_millis(800);
        let mut order = relative_order(&[hung, tile, floater]);
        let mut full_chain = false;
        while Instant::now() < deadline {
          order = relative_order(&[hung, tile, floater]);
          full_chain = order == vec![hung.0, tile.0, floater.0];
          if full_chain {
            break;
          }
          tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
          full_chain,
          "after resume, early recovery must restore full chain; actual={order:?}"
        );
      });
      },
    ));

    drop(hung_gate);
    let _ = tile_helper.kill();
    let _ = tile_helper.wait();
    let _ = floater_helper.kill();
    let _ = floater_helper.wait();
    if let Err(payload) = result {
      std::panic::resume_unwind(payload);
    }
  }

  #[test]
  fn hung_hwnd_rejoins_after_suspend_past_early_recovery_window() {
    use std::time::{Duration, Instant};

    // Stay non-pumping past the 100/250/500ms checkpoints, then release
    // and require the 1s backoff recovery to converge without further
    // input.
    let hung_gate = spawn_deferred_pump_helper();
    let hung = hung_gate.hwnd;
    let (mut tile_helper, tile) =
      spawn_single_pumping_window_helper(false, 0);
    let (mut floater_helper, floater) =
      spawn_single_pumping_window_helper(false, 0);
    let _foreground_override =
      super::TestForegroundWindowOverride::new(hung);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
      || {
        runtime.block_on(async {
        reorder_z_order(&[WindowId(floater.0), WindowId(tile.0)])
          .expect("seed floater-on-top");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
          super::z_order_foreground_window().0,
          hung.0,
          "foreground left hung helper after seed reorder"
        );

        let reorder_at = Instant::now();
        reorder_z_order(&[
          WindowId(hung.0),
          WindowId(tile.0),
          WindowId(floater.0),
        ])
        .expect("reorder while focused tile GUI is not pumping");

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
          super::z_order_foreground_window().0,
          hung.0,
          "foreground left hung helper during long-suspend window"
        );
        let mid = relative_order(&[hung, tile, floater]);
        assert_eq!(mid.len(), 3, "all helper windows should remain present");
        assert_ne!(
          mid.first().copied(),
          Some(hung.0),
          "hung hwnd must stay skipped through early recovery window: {mid:?}"
        );

        hung_gate.release();

        let deadline = reorder_at + Duration::from_millis(3500);
        let mut order = relative_order(&[hung, tile, floater]);
        let mut full_chain = false;
        while Instant::now() < deadline {
          order = relative_order(&[hung, tile, floater]);
          full_chain = order == vec![hung.0, tile.0, floater.0];
          if full_chain {
            break;
          }
          tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
          full_chain,
          "after late resume, 1s backoff recovery must restore full chain; actual={order:?}"
        );
      });
      },
    ));

    drop(hung_gate);
    let _ = tile_helper.kill();
    let _ = tile_helper.wait();
    let _ = floater_helper.kill();
    let _ = floater_helper.wait();
    if let Err(payload) = result {
      std::panic::resume_unwind(payload);
    }
  }

  fn read_helper_hwnd(
    helper: &mut std::process::Child,
    prefix: &str,
  ) -> HWND {
    use std::io::{BufRead, BufReader};

    let stdout = helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    loop {
      let line = lines
        .next()
        .expect("helper HWND line")
        .expect("read helper HWND line");
      if let Some(hwnd) = line.strip_prefix(prefix) {
        return HWND(hwnd.parse::<isize>().expect("parse helper HWND"));
      }
    }
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
    panic!(
      "foreign z-order did not converge: expected={expected_order:?}, actual={order:?}, topmost={topmost_states:?}"
    );
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

  /// Foreign window that fails `WM_NULL` until [`DeferredPump::release`].
  struct DeferredPump {
    helper: std::process::Child,
    hwnd: HWND,
    event: windows::Win32::Foundation::HANDLE,
  }

  impl Drop for DeferredPump {
    fn drop(&mut self) {
      let _ = self.helper.kill();
      let _ = self.helper.wait();
      unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(self.event);
      }
    }
  }

  impl DeferredPump {
    fn release(&self) {
      unsafe {
        windows::Win32::System::Threading::SetEvent(self.event)
          .expect("signal deferred-pump");
      }
    }
  }

  fn spawn_deferred_pump_helper() -> DeferredPump {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
      sync::atomic::{AtomicU64, Ordering},
      time::Duration,
    };

    use windows::Win32::{
      Foundation::HANDLE, System::Threading::CreateEventW,
    };

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = format!(
      r"Local\GlazeWmDeferredPump{}-{}",
      std::process::id(),
      SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let wide_name = wide(&name);
    let event = unsafe {
      CreateEventW(None, true, false, PCWSTR(wide_name.as_ptr()))
    }
    .expect("create deferred-pump event");
    assert_ne!(event, HANDLE::default(), "deferred-pump event handle");

    let current_exe = std::env::current_exe().expect("test executable");
    let mut helper = Command::new(current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "deferred-pump")
      .env("GLAZEWM_RESUME_EVENT", &name)
      .arg("deferred_pump_helper_window")
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn deferred-pump helper");
    let stdout = helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let hwnd = loop {
      let line = lines
        .next()
        .expect("deferred-pump HWND")
        .expect("read deferred-pump HWND");
      if let Some(hwnd) = line.strip_prefix("GLAZEWM_Z_ORDER_HWND:") {
        break HWND(
          hwnd.parse::<isize>().expect("parse deferred-pump HWND"),
        );
      }
    };
    std::thread::sleep(Duration::from_millis(50));
    DeferredPump {
      helper,
      hwnd,
      event,
    }
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
    let _foreground_override =
      super::TestForegroundWindowOverride::new(prompt_peer);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
      reorder_z_order(&[
        WindowId(prompt_peer.0),
        WindowId(delayed_topmost.0),
      ])
      .expect("reorder independently serviced foreign windows");

      // Let the generation-aware 10 ms retry execute while the delayed
      // foreign queue is still processing its first request.
      tokio::time::sleep(Duration::from_millis(500)).await;
    });

    assert_foreign_z_order_converges(&[prompt_peer, delayed_topmost]);

    let _ = delayed_helper.kill();
    let _ = delayed_helper.wait();
    let _ = prompt_helper.kill();
    let _ = prompt_helper.wait();
  }

  #[test]
  fn reorder_z_order_converges_for_eight_independent_gui_queues() {
    use std::{
      panic::{catch_unwind, AssertUnwindSafe},
      time::{Duration, Instant},
    };

    // Model the captured workspace chain: focused floating VS, six tiled
    // peers, and a second floating VS. Each HWND owns an independent GUI
    // queue. The delay gradient makes later requests complete before
    // earlier ones, while keeping every queue responsive to the 50 ms
    // `WM_NULL` probe.
    let delays_ms = [0, 5, 10, 15, 20, 25, 30, 35];
    let mut helpers = Vec::new();
    let mut windows = Vec::new();
    for delay_ms in delays_ms {
      let (helper, hwnd) =
        spawn_single_pumping_window_helper(false, delay_ms);
      helpers.push(helper);
      windows.push(hwnd);
    }

    let focused = windows[7];
    let expected_hwnds = [
      focused, windows[1], windows[2], windows[3], windows[4], windows[5],
      windows[6], windows[0],
    ];
    let targets = windows.clone();

    let result = catch_unwind(AssertUnwindSafe(|| {
      let _foreground_override =
        super::TestForegroundWindowOverride::new(focused);
      assert_eq!(
        super::z_order_foreground_window().0,
        focused.0,
        "last helper should retain foreground as the requested chain head"
      );

      let initial_order = relative_order(&targets);
      assert_ne!(
        initial_order,
        expected_hwnds.iter().map(|hwnd| hwnd.0).collect::<Vec<_>>(),
        "test must start with a native order different from its target"
      );

      let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
      runtime.block_on(async {
        reorder_z_order(
          &expected_hwnds
            .iter()
            .map(|hwnd| WindowId(hwnd.0))
            .collect::<Vec<_>>(),
        )
        .expect("queue focused floating window chain");

        let deadline = Instant::now() + Duration::from_millis(2000);
        let expected = expected_hwnds
          .iter()
          .map(|hwnd| hwnd.0)
          .collect::<Vec<_>>();
        let mut actual = relative_order(&targets);
        while Instant::now() < deadline {
          actual = relative_order(&targets);
          if actual == expected {
            break;
          }
          tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
          actual, expected,
          "async cross-queue requests must converge to the eight-window chain"
        );
      });
    }));

    for helper in &mut helpers {
      let _ = helper.kill();
      let _ = helper.wait();
    }
    if let Err(payload) = result {
      std::panic::resume_unwind(payload);
    }
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

  /// Foreign window that blocks sent messages until
  /// [`ResumeGate::release`].
  struct ResumeGate {
    helper: std::process::Child,
    hwnd: isize,
    event: windows::Win32::Foundation::HANDLE,
  }

  impl Drop for ResumeGate {
    fn drop(&mut self) {
      let _ = self.helper.kill();
      let _ = self.helper.wait();
      unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(self.event);
      }
    }
  }

  impl ResumeGate {
    /// Signals the helper so the blocked native call can return.
    fn release(&self) {
      unsafe {
        windows::Win32::System::Threading::SetEvent(self.event)
          .expect("signal resume gate");
      }
    }
  }

  /// Spawns a pumping foreign window whose sent messages wait on an event.
  fn spawn_resume_gate(layered: bool) -> ResumeGate {
    use std::{
      io::{BufRead, BufReader},
      process::{Command, Stdio},
      sync::atomic::{AtomicU64, Ordering},
      time::Duration,
    };

    use windows::Win32::{
      Foundation::HANDLE, System::Threading::CreateEventW,
    };

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = format!(
      r"Local\GlazeWmResume{}-{}",
      std::process::id(),
      SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let wide_name = wide(&name);
    let event = unsafe {
      CreateEventW(None, true, false, PCWSTR(wide_name.as_ptr()))
    }
    .expect("create resume event");
    assert_ne!(event, HANDLE::default(), "resume event handle");

    let current_exe = std::env::current_exe().expect("test executable");
    let mut helper = Command::new(current_exe)
      .env("GLAZEWM_Z_ORDER_HELPER", "resume-gate")
      .env("GLAZEWM_RESUME_EVENT", &name)
      .env("GLAZEWM_HELPER_LAYERED", if layered { "1" } else { "0" })
      .arg("resume_gate_helper_window")
      .arg("--nocapture")
      .stdout(Stdio::piped())
      .spawn()
      .expect("spawn resume-gate helper");
    let stdout = helper.stdout.take().expect("helper stdout");
    let mut lines = BufReader::new(stdout).lines();
    let hwnd = loop {
      let line = lines
        .next()
        .expect("resume-gate HWND")
        .expect("read resume-gate HWND");
      if let Some(hwnd) = line.strip_prefix("GLAZEWM_Z_ORDER_HWND:") {
        break hwnd.parse::<isize>().expect("parse resume-gate HWND");
      }
    };
    // The HWND line is printed after the gate is armed and before the
    // pump.
    std::thread::sleep(Duration::from_millis(100));
    ResumeGate {
      helper,
      hwnd,
      event,
    }
  }

  #[test]
  fn resume_after_timeout_does_not_keep_stale_layered_bit() {
    let gate = spawn_resume_gate(false);
    let window = super::NativeWindow::new(gate.hwnd);
    let started = std::time::Instant::now();
    window
      .set_transparency(&OpacityValue::from_alpha(180))
      .expect("first transparency");
    assert!(
      started.elapsed() < std::time::Duration::from_secs(2),
      "first layered request blocked the caller"
    );
    assert!(
      super::foreign_style_in_flight(gate.hwnd),
      "first layered request was not still in the worker; layered={} elapsed={:?}",
      ex_style_bit(gate.hwnd, WS_EX_LAYERED.0),
      started.elapsed()
    );
    let started = std::time::Instant::now();
    window
      .set_transparency(&OpacityValue::from_alpha(u8::MAX))
      .expect("superseding opaque request");
    assert!(
      started.elapsed() < std::time::Duration::from_millis(500),
      "superseding request waited on the blocked worker"
    );
    gate.release();
    assert!(
      super::wait_until_foreign_style_idle(
        gate.hwnd,
        std::time::Duration::from_secs(2)
      ),
      "layered worker did not finish after resume"
    );
    assert!(
      !ex_style_bit(gate.hwnd, WS_EX_LAYERED.0),
      "resumed worker left WS_EX_LAYERED from the timed-out request"
    );
  }

  #[test]
  fn resume_after_timeout_restores_title_bar() {
    let gate = spawn_resume_gate(false);
    let window = super::NativeWindow::new(gate.hwnd);
    let started = std::time::Instant::now();
    window
      .set_title_bar_visibility(false)
      .expect("first title-bar request");
    assert!(
      started.elapsed() < std::time::Duration::from_secs(2),
      "first title-bar request blocked the caller"
    );
    assert!(super::foreign_style_in_flight(gate.hwnd));
    assert!(
      {
        let frame = super::dlg_frame_bit();
        (unsafe { GetWindowLongPtrW(HWND(gate.hwnd), GWL_STYLE) }) & frame
          != 0
      },
      "title bar was cleared before the worker resumed"
    );
    let started = std::time::Instant::now();
    window
      .set_title_bar_visibility(true)
      .expect("restore title bar");
    assert!(
      started.elapsed() < std::time::Duration::from_millis(500),
      "title-bar restore waited on the blocked worker"
    );
    gate.release();
    assert!(
      super::wait_until_foreign_style_idle(
        gate.hwnd,
        std::time::Duration::from_secs(2)
      ),
      "title-bar worker did not finish after resume"
    );
    let frame = super::dlg_frame_bit();
    assert!(
      (unsafe { GetWindowLongPtrW(HWND(gate.hwnd), GWL_STYLE) }) & frame
        != 0,
      "resumed worker left the title bar hidden"
    );
  }

  #[test]
  fn resume_after_timeout_applies_latest_layered_alpha() {
    // The first request blocks in `WM_STYLECHANGING` before the alpha
    // write. `SetLayeredWindowAttributes` itself does not enter the
    // foreign procedure, so the stale alpha is the one that would run
    // after that style call returns.
    let gate = spawn_resume_gate(false);
    let window = super::NativeWindow::new(gate.hwnd);
    let started = std::time::Instant::now();
    window
      .set_transparency(&OpacityValue::from_alpha(180))
      .expect("first alpha");
    assert!(
      started.elapsed() < std::time::Duration::from_secs(2),
      "first alpha request blocked the caller"
    );
    assert!(
      super::foreign_style_in_flight(gate.hwnd),
      "style write ahead of the alpha did not stay in the worker"
    );
    assert!(
      !ex_style_bit(gate.hwnd, WS_EX_LAYERED.0),
      "layered bit was committed while the style change was stalled"
    );
    let started = std::time::Instant::now();
    window
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("latest alpha");
    assert!(
      started.elapsed() < std::time::Duration::from_millis(500),
      "latest alpha waited on the blocked worker"
    );
    gate.release();
    assert!(
      super::wait_until_foreign_style_idle(
        gate.hwnd,
        std::time::Duration::from_secs(2)
      ),
      "alpha worker did not finish after resume"
    );
    assert!(ex_style_bit(gate.hwnd, WS_EX_LAYERED.0));
    assert_eq!(
      layered_alpha(gate.hwnd),
      Some(90),
      "resumed worker left the timed-out alpha"
    );
  }

  /// Hidden top-level window created on the calling thread.
  fn hidden_style_window(label: &str) -> (Vec<u16>, HWND) {
    let class_name =
      wide(&format!("GlazeWmOwner{label}{}", std::process::id()));
    let class = WNDCLASSW {
      lpfnWndProc: Some(reorder_test_wnd_proc),
      lpszClassName: PCWSTR(class_name.as_ptr()),
      ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&raw const class) };
    assert_ne!(atom, 0, "register owner-identity class");
    let hwnd = unsafe {
      CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(wide(label).as_ptr()),
        WS_OVERLAPPEDWINDOW,
        0,
        0,
        120,
        80,
        None,
        None,
        None,
        None,
      )
    };
    assert_ne!(hwnd.0, 0, "create owner-identity window");
    (class_name, hwnd)
  }

  #[test]
  fn changed_hwnd_owner_does_not_keep_cached_layered_style() {
    let (class_name, hwnd) = hidden_style_window("reuse");
    let window = super::NativeWindow::new(hwnd.0);
    window
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("seed alpha");
    let mut stale =
      super::foreign_style_intent_clone(hwnd.0).expect("seeded intent");
    stale.owner = super::ForeignWindowOwner {
      process_id: 1,
      thread_id: 1,
    };
    stale.title_bar_visible = Some(false);
    super::replace_foreign_style_intent(hwnd.0, stale);
    window
      .set_title_bar_visibility(true)
      .expect("title bar after owner change");
    let stored =
      super::foreign_style_intent_clone(hwnd.0).expect("fresh intent");
    assert_ne!(
      stored.alpha,
      Some(90),
      "cached alpha survived owner change"
    );
    assert_ne!(
      stored.layered,
      Some(true),
      "cached layered bit survived owner change"
    );
    assert_ne!(
      stored.title_bar_visible,
      Some(false),
      "cached title-bar hide survived owner change"
    );
    let frame = super::dlg_frame_bit();
    assert!(
      (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) }) & frame != 0,
      "owner change reapplied a hidden title bar"
    );
    unsafe {
      let _ = DestroyWindow(hwnd);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
  }

  #[test]
  fn stale_owner_snapshot_does_not_apply() {
    let (class_name, hwnd) = hidden_style_window("snapshot");
    let window = super::NativeWindow::new(hwnd.0);
    window
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("seed alpha");
    let mut stale =
      super::foreign_style_intent_clone(hwnd.0).expect("seeded intent");
    stale.owner = super::ForeignWindowOwner {
      process_id: 1,
      thread_id: 1,
    };
    stale.title_bar_visible = Some(false);
    super::replace_foreign_style_intent(hwnd.0, stale.clone());
    super::apply_foreign_style_intent(hwnd, &stale);
    let frame = super::dlg_frame_bit();
    assert!(
      (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) }) & frame != 0,
      "stale owner snapshot hid the title bar"
    );
    unsafe {
      let _ = DestroyWindow(hwnd);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
  }

  #[test]
  fn destroyed_hwnd_drops_cached_style() {
    let (class_name, hwnd) = hidden_style_window("destroyed");
    let window = super::NativeWindow::new(hwnd.0);
    window
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("seed alpha");
    unsafe {
      let _ = DestroyWindow(hwnd);
    }
    window
      .set_transparency(&OpacityValue::from_alpha(180))
      .expect("request after destroy");
    assert!(
      super::foreign_style_intent_clone(hwnd.0).is_none(),
      "destroyed HWND kept its cached style"
    );
    unsafe {
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
  }

  /// Drops planted foreign-style rows when the test returns.
  struct ForeignStyleCleanup(isize);

  impl Drop for ForeignStyleCleanup {
    fn drop(&mut self) {
      super::forget_foreign_style_hwnd(self.0);
    }
  }

  #[test]
  fn destroy_event_drops_cached_style_for_the_same_owner() {
    let (class_name, hwnd) = hidden_style_window("destroy-event");
    let _cleanup = ForeignStyleCleanup(hwnd.0);
    let window = super::NativeWindow::new(hwnd.0);
    window
      .set_transparency(&OpacityValue::from_alpha(90))
      .expect("seed alpha");
    window
      .set_title_bar_visibility(false)
      .expect("seed hidden title bar");
    let before =
      super::foreign_style_intent_clone(hwnd.0).expect("seeded intent");
    assert_eq!(before.alpha, Some(90));
    assert_eq!(before.layered, Some(true));
    assert_eq!(before.title_bar_visible, Some(false));

    let child = super::super::window_listener::classify_win_event(
      EVENT_OBJECT_DESTROY,
      hwnd,
      OBJID_WINDOW.0,
      1,
    );
    assert!(child.is_none(), "a child control is not a window");
    assert!(
      super::foreign_style_intent_clone(hwnd.0).is_some(),
      "a child-control event cleared the window cache"
    );

    let shown = super::super::window_listener::classify_win_event(
      EVENT_OBJECT_SHOW,
      hwnd,
      OBJID_WINDOW.0,
      0,
    );
    assert!(matches!(shown, Some(WindowEvent::Shown { .. })));
    assert_eq!(
      super::foreign_style_intent_clone(hwnd.0)
        .expect("intent after show")
        .alpha,
      Some(90),
      "a show event cleared cached style"
    );

    let destroyed = super::super::window_listener::classify_win_event(
      EVENT_OBJECT_DESTROY,
      hwnd,
      OBJID_WINDOW.0,
      0,
    );
    assert!(matches!(
      destroyed,
      Some(WindowEvent::Destroyed { window_id, .. }) if window_id == WindowId(hwnd.0)
    ));
    assert!(
      super::foreign_style_intent_clone(hwnd.0).is_none(),
      "destroy event left the cached style in place"
    );

    window
      .set_title_bar_visibility(true)
      .expect("fresh title bar");
    let fresh =
      super::foreign_style_intent_clone(hwnd.0).expect("fresh intent");
    assert_eq!(fresh.owner, before.owner);
    assert_eq!(fresh.alpha, None, "old alpha survived destroy");
    assert_eq!(fresh.layered, None, "old layered bit survived destroy");
    assert_eq!(fresh.title_bar_visible, Some(true));
    let frame = super::dlg_frame_bit();
    assert!(
      (unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) }) & frame != 0,
      "fresh title-bar request left the frame hidden"
    );
    unsafe {
      let _ = DestroyWindow(hwnd);
      let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
    }
  }

  #[test]
  fn detached_worker_does_not_clear_replacement_after_destroy() {
    let gate = spawn_resume_gate(false);
    let _cleanup = ForeignStyleCleanup(gate.hwnd);
    let window = super::NativeWindow::new(gate.hwnd);
    window
      .set_transparency(&OpacityValue::from_alpha(180))
      .expect("blocked layered request");
    assert!(
      super::foreign_style_in_flight(gate.hwnd),
      "layered request did not stay in the worker"
    );
    window
      .set_title_bar_visibility(false)
      .expect("title-bar hide while blocked");
    let blocked = super::foreign_style_cache_view(gate.hwnd);
    let old_ticket =
      blocked.in_flight_ticket.expect("blocked worker ticket");
    let blocked_intent = blocked.intent.expect("blocked intent");
    assert_eq!(blocked_intent.title_bar_visible, Some(false));
    let owner = blocked_intent.owner;
    let threads_while_blocked = blocked.thread_count;

    super::invalidate_foreign_style_state(HWND(gate.hwnd));
    let cleared = super::foreign_style_cache_view(gate.hwnd);
    assert!(cleared.intent.is_none(), "destroy left the intent");
    assert!(
      cleared.in_flight_ticket.is_none(),
      "destroy left the worker attached"
    );
    assert!(
      cleared.detached_tickets.contains(&old_ticket),
      "detached worker was dropped instead of counted"
    );
    assert_eq!(
      cleared.thread_count, threads_while_blocked,
      "detaching the worker freed its cap slot"
    );

    let replacement_ticket = old_ticket.wrapping_add(1_000_000);
    super::replace_foreign_style_intent(
      gate.hwnd,
      super::ForeignStyleIntent {
        generation: 50,
        applied_generation: 0,
        owner,
        layered: Some(false),
        ex_style_or: 0,
        title_bar_visible: Some(true),
        alpha: None,
      },
    );
    super::plant_foreign_style_worker(
      gate.hwnd,
      replacement_ticket,
      owner,
    );
    gate.release();
    assert!(
      super::wait_until_foreign_style_ticket_released(
        old_ticket,
        std::time::Duration::from_secs(2),
      ),
      "detached worker did not release its ticket"
    );
    let after = super::foreign_style_cache_view(gate.hwnd);
    let intent = after.intent.expect("replacement intent");
    assert_eq!(intent.generation, 50);
    assert_eq!(intent.applied_generation, 0);
    assert_eq!(intent.owner, owner);
    assert_eq!(intent.layered, Some(false));
    assert_eq!(intent.alpha, None);
    assert_eq!(intent.title_bar_visible, Some(true));
    assert_eq!(after.in_flight_ticket, Some(replacement_ticket));
    assert!(
      !after.detached_tickets.contains(&old_ticket),
      "old ticket was still detached after the worker returned"
    );
    let frame = super::dlg_frame_bit();
    assert!(
      (unsafe { GetWindowLongPtrW(HWND(gate.hwnd), GWL_STYLE) }) & frame
        != 0,
      "detached worker hid the title bar after destroy"
    );
  }

  struct TimedCall {
    blocked: bool,
    thread: std::thread::JoinHandle<()>,
  }

  /// Runs `body` on another thread. `blocked` is set when `body` is
  /// still running after `limit`.
  fn call_exceeds(
    limit: std::time::Duration,
    body: impl FnOnce() + Send + 'static,
  ) -> TimedCall {
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
      body();
      let _ = sender.send(());
    });
    TimedCall {
      blocked: receiver.recv_timeout(limit).is_err(),
      thread,
    }
  }

  struct SuspendedProbe {
    hwnd: isize,
    thread_id: u32,
    thread: windows::Win32::Foundation::HANDLE,
    gui: std::thread::JoinHandle<()>,
  }

  /// Quit the probe GUI thread, join it, and close the thread handle.
  ///
  /// Avoids `CloseHandle` + `mem::forget(gui)` leaving
  /// `GlazeWmSuspendProbe*` registered for later tests in the same
  /// process.
  fn shutdown_suspended_probe(probe: SuspendedProbe) {
    use windows::Win32::UI::WindowsAndMessaging::{
      PostThreadMessageW, WM_QUIT,
    };

    unsafe {
      let _ =
        PostThreadMessageW(probe.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
      let _ = windows::Win32::Foundation::CloseHandle(probe.thread);
    }
    probe.gui.join().expect("probe gui thread");
  }

  /// Hidden window whose GUI thread is suspended inside `GetMessage`.
  fn spawn_suspended_probe() -> SuspendedProbe {
    use std::sync::{
      atomic::{AtomicU64, Ordering},
      mpsc,
    };

    use windows::Win32::System::Threading::{
      GetCurrentThreadId, OpenThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    static PROBE_CLASS_SEQ: AtomicU64 = AtomicU64::new(0);
    let class_seq = PROBE_CLASS_SEQ.fetch_add(1, Ordering::SeqCst);
    let (sender, receiver) = mpsc::channel();
    let gui = std::thread::spawn(move || {
      // Unique per invocation so a leaked prior probe cannot poison
      // RegisterClassW for later tests in the same process.
      let class_name = wide(&format!(
        "GlazeWmSuspendProbe{}-{}",
        std::process::id(),
        class_seq
      ));
      let class = WNDCLASSW {
        lpfnWndProc: Some(reorder_test_wnd_proc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
      };
      let atom = unsafe { RegisterClassW(&raw const class) };
      assert_ne!(atom, 0, "register suspend-probe class");
      let hwnd = unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE::default(),
          PCWSTR(class_name.as_ptr()),
          PCWSTR(wide("suspend probe").as_ptr()),
          WS_OVERLAPPEDWINDOW,
          0,
          0,
          120,
          80,
          None,
          None,
          None,
          None,
        )
      };
      assert_ne!(hwnd.0, 0, "create suspend-probe window");
      sender
        .send((hwnd.0, unsafe { GetCurrentThreadId() }))
        .expect("send probe hwnd");
      let mut message = MSG::default();
      while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.as_bool()
      {
        unsafe {
          TranslateMessage(&raw const message);
          DispatchMessageW(&raw const message);
        }
      }
      unsafe {
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), None);
      }
    });

    let (hwnd, thread_id) = receiver
      .recv_timeout(std::time::Duration::from_secs(2))
      .expect("probe hwnd");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let thread =
      unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, thread_id) }
        .expect("open gui thread");
    assert_ne!(
      unsafe { SuspendThread(thread) },
      u32::MAX,
      "suspend gui thread"
    );
    SuspendedProbe {
      hwnd,
      thread_id,
      thread,
      gui,
    }
  }

  #[test]
  fn suspended_gui_thread_blocks_style_write_not_layered_alpha() {
    use windows::Win32::System::Threading::{ResumeThread, SuspendThread};

    let probe = spawn_suspended_probe();
    let hwnd = probe.hwnd;
    let style_call =
      call_exceeds(std::time::Duration::from_millis(200), move || {
        let current =
          unsafe { GetWindowLongPtrW(HWND(hwnd), GWL_EXSTYLE) };
        unsafe {
          SetWindowLongPtrW(
            HWND(hwnd),
            GWL_EXSTYLE,
            current | super::layered_ex_bit(),
          );
        }
      });
    assert_ne!(
      unsafe { ResumeThread(probe.thread) },
      u32::MAX,
      "resume after style"
    );
    style_call.thread.join().expect("style call");
    assert!(
      style_call.blocked,
      "SetWindowLongPtrW returned while the GUI thread was suspended"
    );
    assert!(
      ex_style_bit(hwnd, WS_EX_LAYERED.0),
      "style write did not add WS_EX_LAYERED"
    );

    assert_ne!(
      unsafe { SuspendThread(probe.thread) },
      u32::MAX,
      "suspend gui thread again"
    );
    let alpha_call =
      call_exceeds(std::time::Duration::from_millis(200), move || {
        let _ = unsafe {
          SetLayeredWindowAttributes(HWND(hwnd), None, 90, LWA_ALPHA)
        };
      });
    assert_ne!(
      unsafe { ResumeThread(probe.thread) },
      u32::MAX,
      "resume after alpha"
    );
    alpha_call.thread.join().expect("alpha call");
    assert!(
      !alpha_call.blocked,
      "SetLayeredWindowAttributes blocked on a suspended GUI thread"
    );

    shutdown_suspended_probe(probe);
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
