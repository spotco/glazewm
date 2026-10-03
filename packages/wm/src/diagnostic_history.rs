use std::{
  collections::VecDeque,
  time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use uuid::Uuid;

/// Newest focus and z-order samples kept for a later state dump.
///
/// 200 events covers the last few seconds of focus and reconcile
/// traffic. Payloads stay short (hwnds, ranks, truncated titles) so
/// five workspaces of about ten windows stay far under 10MB.
pub const DIAGNOSTIC_HISTORY_CAP: usize = 200;

const TEXT_CAP: usize = 160;
const ERROR_CAP: usize = 300;

#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticRecord {
  pub seq: u64,
  pub at_unix_ms: u64,
  pub at_local: String,
  #[serde(flatten)]
  pub body: DiagnosticBody,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticBody {
  Focus {
    hwnd: i64,
    title: Option<String>,
    class_name: Option<String>,
    outcome: FocusOutcome,
    wm_container_id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    workspace_name: Option<String>,
    display_state: Option<String>,
    window_state: Option<String>,
    shown_on_top: Option<bool>,
    focus_synced: bool,
  },
  ZOrderReconcile {
    workspace_id: Uuid,
    workspace_name: String,
    focused_hwnd: i64,
    focused_layer: Option<&'static str>,
    intended_top_to_bottom: Vec<i64>,
    native_ranks: Vec<NativeZRank>,
    /// When `native_ranks` was sampled relative to `SetWindowPos`.
    /// Reorder is asynchronous, so an immediate sample can still show
    /// the previous native order.
    native_sample_timing: &'static str,
    reorder_error: Option<String>,
  },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusOutcome {
  IgnoredSuspend,
  OverrideRecentUnmanage,
  UnknownForeground,
  AlreadySynced,
  ManualFocus,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeZRank {
  pub hwnd: i64,
  pub z_order_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticHistory {
  next_seq: u64,
  events: VecDeque<DiagnosticRecord>,
}

impl DiagnosticHistory {
  pub fn push(&mut self, body: DiagnosticBody) {
    self.next_seq = self.next_seq.saturating_add(1);
    self.events.push_back(DiagnosticRecord {
      seq: self.next_seq,
      at_unix_ms: unix_ms(),
      at_local: local_iso(),
      body,
    });
    while self.events.len() > DIAGNOSTIC_HISTORY_CAP {
      self.events.pop_front();
    }
  }

  #[must_use]
  pub fn records(&self) -> &VecDeque<DiagnosticRecord> {
    &self.events
  }
}

#[must_use]
pub fn hwnd_i64(handle: isize) -> i64 {
  i64::try_from(handle).unwrap_or_else(|_| {
    if handle.is_negative() {
      i64::MIN
    } else {
      i64::MAX
    }
  })
}

#[must_use]
pub fn truncate_text(value: &str, max_chars: usize) -> String {
  let mut chars = value.chars();
  let truncated: String = chars.by_ref().take(max_chars).collect();
  let _ = chars.next();
  truncated
}

pub(crate) fn truncate_title(value: &str) -> String {
  truncate_text(value, TEXT_CAP)
}

pub(crate) fn truncate_error(value: &str) -> String {
  truncate_text(value, ERROR_CAP)
}

#[must_use]
pub fn unix_ms() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |duration| {
      u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    })
}

#[must_use]
pub fn local_iso() -> String {
  let stamp = local_system_time();
  format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
    stamp.year,
    stamp.month,
    stamp.day,
    stamp.hour,
    stamp.minute,
    stamp.second,
    stamp.milliseconds
  )
}

#[must_use]
pub fn local_file_stamp() -> String {
  let stamp = local_system_time();
  format!(
    "{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}",
    stamp.year,
    stamp.month,
    stamp.day,
    stamp.hour,
    stamp.minute,
    stamp.second,
    stamp.milliseconds
  )
}

struct LocalStamp {
  year: u16,
  month: u16,
  day: u16,
  hour: u16,
  minute: u16,
  second: u16,
  milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
  fn GetLocalTime(out: *mut WindowsSystemTime);
}

#[cfg(windows)]
#[repr(C)]
struct WindowsSystemTime {
  year: u16,
  month: u16,
  day_of_week: u16,
  day: u16,
  hour: u16,
  minute: u16,
  second: u16,
  milliseconds: u16,
}

fn local_system_time() -> LocalStamp {
  #[cfg(windows)]
  {
    let mut raw = WindowsSystemTime {
      year: 0,
      month: 0,
      day_of_week: 0,
      day: 0,
      hour: 0,
      minute: 0,
      second: 0,
      milliseconds: 0,
    };
    // SAFETY: `raw` is a valid `SYSTEMTIME` out-buffer.
    unsafe { GetLocalTime(std::ptr::from_mut(&mut raw)) };
    LocalStamp {
      year: raw.year,
      month: raw.month,
      day: raw.day,
      hour: raw.hour,
      minute: raw.minute,
      second: raw.second,
      milliseconds: raw.milliseconds,
    }
  }

  #[cfg(not(windows))]
  {
    let millis = unix_ms();
    let seconds = millis / 1000;
    utc_ymd_hms(seconds, millis % 1000)
  }
}

#[cfg(not(windows))]
fn utc_ymd_hms(seconds: u64, millis: u64) -> LocalStamp {
  let time_of_day = seconds % 86_400;
  let hour = u16::try_from(time_of_day / 3600).unwrap_or(0);
  let minute = u16::try_from((time_of_day % 3600) / 60).unwrap_or(0);
  let second = u16::try_from(time_of_day % 60).unwrap_or(0);
  let milliseconds = u16::try_from(millis).unwrap_or(0);
  let (year, month, day) = utc_ymd(seconds / 86_400);
  LocalStamp {
    year,
    month,
    day,
    hour,
    minute,
    second,
    milliseconds,
  }
}

/// Days since Unix epoch to a civil UTC date (Howard Hinnant).
#[cfg(not(windows))]
fn utc_ymd(days_since_epoch: u64) -> (u16, u16, u16) {
  let z = i64::try_from(days_since_epoch).unwrap_or(0) + 719_468;
  let era = z.div_euclid(146_097);
  let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
  let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = u16::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
  let month =
    u16::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
  let year =
    u16::try_from(if month <= 2 { y + 1 } else { y }).unwrap_or(1970);
  (year, month, day)
}

#[cfg(test)]
mod tests {
  use super::{
    DiagnosticBody, DiagnosticHistory, FocusOutcome,
    DIAGNOSTIC_HISTORY_CAP,
  };

  #[test]
  fn history_keeps_only_the_newest_cap() {
    let mut history = DiagnosticHistory::default();
    for index in 0..250 {
      history.push(DiagnosticBody::Focus {
        hwnd: index,
        title: None,
        class_name: None,
        outcome: FocusOutcome::UnknownForeground,
        wm_container_id: None,
        workspace_id: None,
        workspace_name: None,
        display_state: None,
        window_state: None,
        shown_on_top: None,
        focus_synced: false,
      });
    }

    assert_eq!(history.records().len(), DIAGNOSTIC_HISTORY_CAP);
    assert_eq!(history.records().front().unwrap().seq, 51);
    assert_eq!(history.records().back().unwrap().seq, 250);

    let json =
      serde_json::to_value(history.records().back().unwrap()).unwrap();
    assert_eq!(json["kind"], "focus");
    assert_eq!(json["outcome"], "unknown_foreground");
    assert!(json.get("seq").is_some());
    assert!(json.get("at_unix_ms").is_some());
    assert!(json.get("at_local").is_some());
    assert!(json.get("hwnd").is_some());
    assert!(json.get("shown_on_top").is_some());
    assert!(json.get("focus_synced").is_some());
  }
}
