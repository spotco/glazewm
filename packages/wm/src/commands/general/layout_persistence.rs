//! Persisted layout snapshot (`layout.json` beside `config.yaml`).
//!
//! Auto-saves the current durable layout on a ~5s debounce after layout-
//! affecting WM events, and best-effort loads that file once at startup.
//!
//! Debug trail for this feature is appended to `layout.log` in the same
//! directory (resolved from the active config path).

use std::{
  fs::{self, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
  pin::Pin,
  sync::Mutex,
  time::{Duration, SystemTime},
};

use anyhow::Context;
use tokio::time::{sleep_until, Instant, Sleep};
use tracing::{debug, info, warn};
use wm_common::{format_system_time_rfc3339, LayoutSnapshot, WmEvent};

use crate::{
  commands::general::{
    durable_snapshot_json, load_layout_snapshot, platform_sync,
    write_snapshot_file, LoadLayoutSummary,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Debounce quiet period before writing `layout.json`.
pub const LAYOUT_AUTO_SAVE_DEBOUNCE: Duration = Duration::from_secs(5);

/// File name written beside the user config (`config.yaml` → `layout.json`).
pub const LAYOUT_SNAPSHOT_FILE_NAME: &str = "layout.json";

/// Debug log beside the snapshot (`config.yaml` → `layout.log`).
pub const LAYOUT_DEBUG_LOG_FILE_NAME: &str = "layout.log";

/// Soft size cap before rotating `layout.log` (keep a `.1` backup).
const LAYOUT_DEBUG_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Process-wide path for layout persistence debug logging (set at startup).
static LAYOUT_DEBUG_LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Resolve `layout.json` next to the active config file (same directory
/// resolution as `UserConfig` / `%USERPROFILE%\.glzr\glazewm`).
pub fn layout_snapshot_path(config: &UserConfig) -> PathBuf {
  layout_snapshot_path_from_config_path(&config.path)
}

/// Pure path join used by [`layout_snapshot_path`] (unit-tested).
pub fn layout_snapshot_path_from_config_path(config_path: &Path) -> PathBuf {
  config_path.with_file_name(LAYOUT_SNAPSHOT_FILE_NAME)
}

/// Resolve `layout.log` beside the active config / `layout.json`.
pub fn layout_debug_log_path(config: &UserConfig) -> PathBuf {
  layout_debug_log_path_from_config_path(&config.path)
}

/// Pure path join for the layout debug log (unit-tested).
pub fn layout_debug_log_path_from_config_path(config_path: &Path) -> PathBuf {
  config_path.with_file_name(LAYOUT_DEBUG_LOG_FILE_NAME)
}

/// Remember where layout persistence should append debug lines.
pub fn set_layout_debug_log_path(path: PathBuf) {
  if let Ok(mut guard) = LAYOUT_DEBUG_LOG_PATH.lock() {
    *guard = Some(path);
  }
}

/// Short discriminant name for debounce / schedule logging.
pub fn wm_event_kind_name(event: &WmEvent) -> &'static str {
  match event {
    WmEvent::ApplicationExiting => "ApplicationExiting",
    WmEvent::BindingModesChanged { .. } => "BindingModesChanged",
    WmEvent::FocusChanged { .. } => "FocusChanged",
    WmEvent::FocusedContainerMoved { .. } => "FocusedContainerMoved",
    WmEvent::MonitorAdded { .. } => "MonitorAdded",
    WmEvent::MonitorRemoved { .. } => "MonitorRemoved",
    WmEvent::MonitorUpdated { .. } => "MonitorUpdated",
    WmEvent::TilingDirectionChanged { .. } => "TilingDirectionChanged",
    WmEvent::UserConfigChanged { .. } => "UserConfigChanged",
    WmEvent::WindowManaged { .. } => "WindowManaged",
    WmEvent::WindowUnmanaged { .. } => "WindowUnmanaged",
    WmEvent::WorkspaceActivated { .. } => "WorkspaceActivated",
    WmEvent::WorkspaceDeactivated { .. } => "WorkspaceDeactivated",
    WmEvent::WorkspaceUpdated { .. } => "WorkspaceUpdated",
    WmEvent::PauseChanged { .. } => "PauseChanged",
  }
}

/// Append a high-signal line to `layout.log` (and optionally mirror via tracing).
///
/// Never fatal: I/O failures are swallowed after a single `warn!`.
pub fn layout_debug_log(message: impl AsRef<str>) {
  let message = message.as_ref();
  let path = match LAYOUT_DEBUG_LOG_PATH.lock() {
    Ok(guard) => guard.clone(),
    Err(_) => None,
  };

  let Some(path) = path else {
    debug!("layout.log unset; skipped: {message}");
    return;
  };

  if let Err(err) = append_layout_debug_line(&path, message) {
    warn!(
      "Failed to append layout debug log {}: {err:#}",
      path.display()
    );
  }
}

fn append_layout_debug_line(path: &Path, message: &str) -> anyhow::Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).with_context(|| {
      format!("Unable to create directory {}.", parent.display())
    })?;
  }

  // Soft rotate: keep previous contents as layout.log.1 once over the cap.
  if let Ok(meta) = fs::metadata(path) {
    if meta.len() >= LAYOUT_DEBUG_LOG_MAX_BYTES {
      let backup = path.with_extension("log.1");
      let _ = fs::remove_file(&backup);
      let _ = fs::rename(path, &backup);
    }
  }

  let ts = format_system_time_rfc3339(SystemTime::now());
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(path)
    .with_context(|| format!("Unable to open {}.", path.display()))?;
  writeln!(file, "[{ts}] {message}")
    .with_context(|| format!("Unable to write {}.", path.display()))?;
  Ok(())
}

/// Whether a WM event should schedule an auto-save of the layout snapshot.
///
/// Skips pure focus / pause / config / binding-mode noise that does not
/// change snapshot content in a meaningful way.
pub fn wm_event_affects_layout_snapshot(event: &WmEvent) -> bool {
  matches!(
    event,
    WmEvent::FocusedContainerMoved { .. }
      | WmEvent::WindowManaged { .. }
      | WmEvent::WindowUnmanaged { .. }
      | WmEvent::WorkspaceActivated { .. }
      | WmEvent::WorkspaceDeactivated { .. }
      | WmEvent::WorkspaceUpdated { .. }
      | WmEvent::TilingDirectionChanged { .. }
      | WmEvent::MonitorAdded { .. }
      | WmEvent::MonitorRemoved { .. }
      | WmEvent::MonitorUpdated { .. }
  )
}

/// Debounced writer for the persisted layout file.
pub struct LayoutAutoSave {
  path: PathBuf,
  sleep: Pin<Box<Sleep>>,
  armed: bool,
  enabled: bool,
}

impl LayoutAutoSave {
  pub fn new(path: PathBuf) -> Self {
    // Far-future sleep placeholder until first arm (never fires while disarmed).
    let far = Instant::now() + Duration::from_secs(60 * 60 * 24 * 365);
    Self {
      path,
      sleep: Box::pin(sleep_until(far)),
      armed: false,
      enabled: false,
    }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  #[allow(dead_code)] // exercised in unit tests
  pub fn enabled(&self) -> bool {
    self.enabled
  }

  /// Allow scheduling after startup restore + event drain.
  pub fn enable(&mut self) {
    self.enabled = true;
    self.armed = false;
    let msg = format!(
      "auto-save enabled (debounce {}s) -> {}",
      LAYOUT_AUTO_SAVE_DEBOUNCE.as_secs(),
      self.path.display()
    );
    info!("{msg}");
    layout_debug_log(&msg);
  }

  /// Schedule (or coalesce) a debounced save for a layout-affecting event.
  pub fn schedule(&mut self, event: &WmEvent) {
    let kind = wm_event_kind_name(event);
    if !self.enabled {
      let msg = format!(
        "auto-save suppressed (startup drain / disabled); event={kind}"
      );
      debug!("{msg}");
      layout_debug_log(&msg);
      return;
    }

    let was_armed = self.armed;
    let deadline = Instant::now() + LAYOUT_AUTO_SAVE_DEBOUNCE;
    self.sleep.as_mut().reset(deadline);
    self.armed = true;

    if was_armed {
      let msg = format!(
        "auto-save debounce coalesced; event={kind}; quiet={}s",
        LAYOUT_AUTO_SAVE_DEBOUNCE.as_secs()
      );
      debug!("{msg}");
      layout_debug_log(&msg);
    } else {
      let msg = format!(
        "auto-save scheduled; event={kind}; quiet={}s",
        LAYOUT_AUTO_SAVE_DEBOUNCE.as_secs()
      );
      info!("{msg}");
      layout_debug_log(&msg);
    }
  }

  /// `true` while a debounced save is pending (for `tokio::select!` guards).
  pub fn is_armed(&self) -> bool {
    self.armed
  }

  /// Mutable sleep future for `tokio::select!`.
  pub fn sleep_mut(&mut self) -> &mut Pin<Box<Sleep>> {
    &mut self.sleep
  }

  /// Write the latest durable snapshot; clears the armed flag.
  pub fn flush(&mut self, state: &WmState) -> anyhow::Result<()> {
    self.armed = false;
    if !self.enabled {
      let msg = "auto-save flush skipped (disabled)";
      debug!("{msg}");
      layout_debug_log(msg);
      return Ok(());
    }

    let workspace_count = state.workspaces().len();
    let window_count = state.windows().len();
    let msg = format!(
      "auto-save debounce fired; writing {} (workspaces={workspace_count}, windows={window_count})",
      self.path.display()
    );
    info!("{msg}");
    layout_debug_log(&msg);

    match save_layout_snapshot_to_path(state, &self.path) {
      Ok(()) => {
        let msg = format!(
          "auto-save success -> {} (workspaces={workspace_count}, windows={window_count})",
          self.path.display()
        );
        info!("{msg}");
        layout_debug_log(&msg);
        Ok(())
      }
      Err(err) => {
        let msg = format!(
          "auto-save failed -> {}: {err:#}",
          self.path.display()
        );
        warn!("{msg}");
        layout_debug_log(&msg);
        Err(err)
      }
    }
  }
}

/// Write durable layout JSON to `path` (same schema as tray/CLI save).
pub fn save_layout_snapshot_to_path(
  state: &WmState,
  path: &Path,
) -> anyhow::Result<()> {
  let json = durable_snapshot_json(state)?;
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).with_context(|| {
      format!("Unable to create directory {}.", parent.display())
    })?;
  }
  write_snapshot_file(path, &json)?;
  Ok(())
}

/// Best-effort startup load from `layout.json`.
///
/// Missing / empty / invalid JSON / restore errors are logged and ignored
/// (default GlazeWM behaviour). Never fatal.
pub fn try_load_persisted_layout_snapshot(
  path: &Path,
  state: &mut WmState,
  config: &UserConfig,
) -> Option<LoadLayoutSummary> {
  let attempt = format!("startup load attempting from {}", path.display());
  info!("{attempt}");
  layout_debug_log(&attempt);

  if !path.exists() {
    let msg = format!(
      "startup load: no file at {}; using default layout behaviour",
      path.display()
    );
    info!("{msg}");
    layout_debug_log(&msg);
    return None;
  }

  let text = match fs::read_to_string(path) {
    Ok(t) => t,
    Err(err) => {
      let msg = format!(
        "startup load: failed to read {}: {err:#}; using default",
        path.display()
      );
      warn!("{msg}");
      layout_debug_log(&msg);
      return None;
    }
  };

  if text.trim().is_empty() {
    let msg = format!(
      "startup load: {} is empty; using default layout behaviour",
      path.display()
    );
    warn!("{msg}");
    layout_debug_log(&msg);
    return None;
  }

  let snapshot: LayoutSnapshot = match serde_json::from_str(&text) {
    Ok(s) => s,
    Err(err) => {
      let msg = format!(
        "startup load: parse failed for {}: {err:#}; using default",
        path.display()
      );
      warn!("{msg}");
      layout_debug_log(&msg);
      return None;
    }
  };

  match load_layout_snapshot(&snapshot, state, config) {
    Ok(summary) => {
      if state.pending_sync.has_changes() {
        if let Err(err) = platform_sync(state, config) {
          let msg = format!(
            "platform_sync after persisted layout load failed: {err:#}"
          );
          warn!("{msg}");
          layout_debug_log(&msg);
        }
      }
      let msg = format!(
        "startup load success from {}: matched={}, unmatched_snapshot={}, unmatched_live={}, workspace_moves={}, state_updates={}, tiling_trees_restored={}, tiling_windows_placed={}",
        path.display(),
        summary.matched,
        summary.unmatched_snapshot,
        summary.unmatched_live,
        summary.workspace_moves,
        summary.state_updates,
        summary.tiling_trees_restored,
        summary.tiling_windows_placed
      );
      info!("{msg}");
      layout_debug_log(&msg);
      Some(summary)
    }
    Err(err) => {
      let msg = format!(
        "startup load: restore failed for {}: {err:#}; using default",
        path.display()
      );
      warn!("{msg}");
      layout_debug_log(&msg);
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  #[test]
  fn layout_path_joins_beside_config_yaml() {
    let config = PathBuf::from(r"C:\Users\example\.glzr\glazewm\config.yaml");
    assert_eq!(
      layout_snapshot_path_from_config_path(&config),
      PathBuf::from(r"C:\Users\example\.glzr\glazewm\layout.json")
    );
  }

  #[test]
  fn layout_path_works_with_unix_style_config() {
    let config = PathBuf::from("/home/user/.glzr/glazewm/config.yaml");
    assert_eq!(
      layout_snapshot_path_from_config_path(&config),
      PathBuf::from("/home/user/.glzr/glazewm/layout.json")
    );
  }

  #[test]
  fn layout_debug_log_path_joins_beside_config() {
    let config = PathBuf::from(r"C:\Users\example\.glzr\glazewm\config.yaml");
    assert_eq!(
      layout_debug_log_path_from_config_path(&config),
      PathBuf::from(r"C:\Users\example\.glzr\glazewm\layout.log")
    );
  }

  #[test]
  fn debounce_constant_is_five_seconds() {
    assert_eq!(LAYOUT_AUTO_SAVE_DEBOUNCE, Duration::from_secs(5));
  }

  #[test]
  fn auto_save_ignores_schedule_while_disabled() {
    // LayoutAutoSave owns a tokio Sleep; constructing it needs a runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_time()
      .build()
      .unwrap();
    rt.block_on(async {
      let mut auto = LayoutAutoSave::new(PathBuf::from("layout.json"));
      assert!(!auto.enabled());
      auto.schedule(&WmEvent::ApplicationExiting);
      assert!(!auto.is_armed());
      auto.enable();
      assert!(auto.enabled());
      assert!(!auto.is_armed());
      // schedule() arms whenever called while enabled; event filtering is
      // the caller's responsibility (wm_event_affects_layout_snapshot).
      auto.schedule(&WmEvent::ApplicationExiting);
      assert!(auto.is_armed());
    });
  }

  #[test]
  fn non_layout_events_do_not_affect_snapshot_gate() {
    assert!(!wm_event_affects_layout_snapshot(
      &WmEvent::ApplicationExiting
    ));
    assert!(!wm_event_affects_layout_snapshot(&WmEvent::PauseChanged {
      is_paused: true
    }));
  }

  #[test]
  fn missing_layout_file_is_detectable_without_wm() {
    let missing = std::env::temp_dir().join(format!(
      "glazewm-layout-missing-{}-layout.json",
      std::process::id()
    ));
    let _ = fs::remove_file(&missing);
    assert!(!missing.exists());
  }

  #[test]
  fn empty_layout_file_trim_gate() {
    let dir = std::env::temp_dir().join(format!(
      "glazewm-layout-empty-{}",
      std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("layout.json");
    fs::write(&path, "   \n").unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.trim().is_empty());
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir(&dir);
  }

  #[test]
  fn layout_debug_log_appends_when_path_set() {
    let dir = std::env::temp_dir().join(format!(
      "glazewm-layout-debug-{}",
      std::process::id()
    ));
    let _ = fs::create_dir_all(&dir);
    let log_path = dir.join("layout.log");
    let _ = fs::remove_file(&log_path);
    set_layout_debug_log_path(log_path.clone());
    layout_debug_log("unit-test line");
    let text = fs::read_to_string(&log_path).unwrap();
    assert!(text.contains("unit-test line"));
    let _ = fs::remove_file(&log_path);
    let _ = fs::remove_dir(&dir);
  }
}
