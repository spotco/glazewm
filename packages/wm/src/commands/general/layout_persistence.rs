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
  sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
  },
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

/// File name written beside the user config (`config.yaml` →
/// `layout.json`).
pub const LAYOUT_SNAPSHOT_FILE_NAME: &str = "layout.json";

/// Debug log beside the snapshot (`config.yaml` → `layout.log`).
pub const LAYOUT_DEBUG_LOG_FILE_NAME: &str = "layout.log";

/// Soft size cap before rotating `layout.log` (keep a `.1` backup).
const LAYOUT_DEBUG_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Process-wide path for layout persistence debug logging (set at
/// startup).
static LAYOUT_DEBUG_LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Config-driven verbose z-order / native-op diagnostics for `layout.log`.
///
/// Updated on startup and config reload from
/// `general.verbose_z_order`. Env `GLAZEWM_VERBOSE_Z_ORDER=1` still forces
/// on at read time.
static VERBOSE_Z_ORDER_FROM_CONFIG: AtomicBool = AtomicBool::new(false);

/// Sync the process-wide verbose flag from the parsed user config.
pub fn set_verbose_z_order_from_config(enabled: bool) {
  VERBOSE_Z_ORDER_FROM_CONFIG.store(enabled, Ordering::Relaxed);
}

/// Whether verbose z-order / native-op layout.log lines are enabled.
///
/// True when `general.verbose_z_order` is set, or when env
/// `GLAZEWM_VERBOSE_Z_ORDER=1` is present.
#[must_use]
pub fn verbose_z_order_enabled() -> bool {
  if VERBOSE_Z_ORDER_FROM_CONFIG.load(Ordering::Relaxed) {
    return true;
  }
  std::env::var("GLAZEWM_VERBOSE_Z_ORDER").ok().as_deref() == Some("1")
}

/// Resolve `layout.json` next to the active config file (same directory
/// resolution as `UserConfig` / `%USERPROFILE%\.glzr\glazewm`).
pub fn layout_snapshot_path(config: &UserConfig) -> PathBuf {
  layout_snapshot_path_from_config_path(&config.path)
}

/// Pure path join used by [`layout_snapshot_path`] (unit-tested).
pub fn layout_snapshot_path_from_config_path(
  config_path: &Path,
) -> PathBuf {
  config_path.with_file_name(LAYOUT_SNAPSHOT_FILE_NAME)
}

/// Resolve `layout.log` beside the active config / `layout.json`.
pub fn layout_debug_log_path(config: &UserConfig) -> PathBuf {
  layout_debug_log_path_from_config_path(&config.path)
}

/// Pure path join for the layout debug log (unit-tested).
pub fn layout_debug_log_path_from_config_path(
  config_path: &Path,
) -> PathBuf {
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
    WmEvent::GlobalTilingDirectionChanged { .. } => {
      "GlobalTilingDirectionChanged"
    }
    WmEvent::UserConfigChanged { .. } => "UserConfigChanged",
    WmEvent::WindowManaged { .. } => "WindowManaged",
    WmEvent::WindowUnmanaged { .. } => "WindowUnmanaged",
    WmEvent::WorkspaceActivated { .. } => "WorkspaceActivated",
    WmEvent::WorkspaceDeactivated { .. } => "WorkspaceDeactivated",
    WmEvent::WorkspaceUpdated { .. } => "WorkspaceUpdated",
    WmEvent::PauseChanged { .. } => "PauseChanged",
  }
}

/// Append a high-signal line to `layout.log` (and optionally mirror via
/// tracing).
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

fn append_layout_debug_line(
  path: &Path,
  message: &str,
) -> anyhow::Result<()> {
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
  let mut file =
    OpenOptions::new()
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
      | WmEvent::GlobalTilingDirectionChanged { .. }
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
    // Far-future sleep placeholder until first arm (never fires while
    // disarmed).
    #[allow(clippy::duration_suboptimal_units)]
    let far = Instant::now() + Duration::from_secs(86_400 * 365);
    Self {
      path,
      sleep: Box::pin(sleep_until(far)),
      armed: false,
      enabled: false,
    }
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

  /// `true` while a debounced save is pending (for `tokio::select!`
  /// guards).
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
        let msg =
          format!("auto-save failed -> {}: {err:#}", self.path.display());
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

/// Sibling `*.bak` path used by atomic write / startup recovery.
fn sibling_bak_path(path: &Path) -> PathBuf {
  let mut os = path.as_os_str().to_owned();
  os.push(".bak");
  PathBuf::from(os)
}

/// Outcome of trying to read + parse one snapshot path.
#[derive(Debug)]
enum SnapshotReadOutcome {
  Missing,
  Empty,
  IoError(String),
  ParseError(String),
  Ok(LayoutSnapshot),
}

fn read_snapshot_file_outcome(path: &Path) -> SnapshotReadOutcome {
  if !path.exists() {
    return SnapshotReadOutcome::Missing;
  }
  let text = match fs::read_to_string(path) {
    Ok(t) => t,
    Err(err) => {
      return SnapshotReadOutcome::IoError(format!("{err:#}"));
    }
  };
  if text.trim().is_empty() {
    return SnapshotReadOutcome::Empty;
  }
  match serde_json::from_str::<LayoutSnapshot>(&text) {
    Ok(snapshot) => SnapshotReadOutcome::Ok(snapshot),
    Err(err) => SnapshotReadOutcome::ParseError(format!("{err:#}")),
  }
}

fn describe_snapshot_read_failure(
  path: &Path,
  outcome: &SnapshotReadOutcome,
) -> String {
  match outcome {
    SnapshotReadOutcome::Missing => {
      format!("no file at {}", path.display())
    }
    SnapshotReadOutcome::Empty => {
      format!("{} is empty", path.display())
    }
    SnapshotReadOutcome::IoError(err) => {
      format!("failed to read {}: {err}", path.display())
    }
    SnapshotReadOutcome::ParseError(err) => {
      format!("parse failed for {}: {err}", path.display())
    }
    SnapshotReadOutcome::Ok(_) => {
      format!("{} is valid", path.display())
    }
  }
}

/// Promote a valid `.bak` snapshot back to the primary path.
///
/// Best-effort: recovery still succeeds even if promotion fails (e.g. file
/// locks), so the next startup can try `.bak` again.
fn promote_bak_to_primary(primary: &Path, bak: &Path) {
  // Remove a corrupt/empty primary so rename can succeed on Windows.
  if primary.exists() {
    if let Err(err) = fs::remove_file(primary) {
      let msg = format!(
        "startup load: could not remove unusable primary {}: {err:#}; leaving .bak in place",
        primary.display()
      );
      warn!("{msg}");
      layout_debug_log(&msg);
      return;
    }
  }
  match fs::rename(bak, primary) {
    Ok(()) => {
      let msg = format!(
        "startup load: promoted {} -> {}",
        bak.display(),
        primary.display()
      );
      info!("{msg}");
      layout_debug_log(&msg);
    }
    Err(err) => {
      let msg = format!(
        "startup load: could not promote {} -> {}: {err:#}; will retry .bak next launch",
        bak.display(),
        primary.display()
      );
      warn!("{msg}");
      layout_debug_log(&msg);
    }
  }
}

/// Read a persisted layout snapshot for startup, falling back to `.bak`
/// when the primary is missing / empty / unreadable / invalid JSON.
///
/// On successful `.bak` recovery, attempts to promote `.bak` back to the
/// primary path so the next launch does not depend on the crash-window
/// leftover. Pure file I/O + parse (no `WmState`) so unit tests cover
/// the real startup selection path.
pub fn resolve_persisted_layout_snapshot(
  path: &Path,
) -> Option<LayoutSnapshot> {
  let primary = read_snapshot_file_outcome(path);
  if let SnapshotReadOutcome::Ok(snapshot) = primary {
    return Some(snapshot);
  }

  let bak_path = sibling_bak_path(path);
  let bak = read_snapshot_file_outcome(&bak_path);
  if let SnapshotReadOutcome::Ok(snapshot) = bak {
    let msg = format!(
      "startup load: primary unusable ({}); recovering from {}",
      describe_snapshot_read_failure(path, &primary),
      bak_path.display()
    );
    warn!("{msg}");
    layout_debug_log(&msg);
    promote_bak_to_primary(path, &bak_path);
    return Some(snapshot);
  }

  // Neither primary nor bak worked — keep prior messaging style.
  let msg = format!(
    "startup load: {}; using default layout behaviour",
    describe_snapshot_read_failure(path, &primary)
  );
  match &primary {
    SnapshotReadOutcome::Missing => {
      // Only mention .bak if it exists but was also unusable, or if we
      // looked and found nothing useful.
      if bak_path.exists() {
        let bak_msg = format!(
          "startup load: also tried {}: {}",
          bak_path.display(),
          describe_snapshot_read_failure(&bak_path, &bak)
        );
        warn!("{msg}");
        layout_debug_log(&msg);
        warn!("{bak_msg}");
        layout_debug_log(&bak_msg);
      } else {
        info!("{msg}");
        layout_debug_log(&msg);
      }
    }
    _ => {
      if matches!(bak, SnapshotReadOutcome::Missing) {
        warn!("{msg}");
        layout_debug_log(&msg);
      } else {
        let bak_msg = format!(
          "startup load: also tried {}: {}",
          bak_path.display(),
          describe_snapshot_read_failure(&bak_path, &bak)
        );
        warn!("{msg}");
        layout_debug_log(&msg);
        warn!("{bak_msg}");
        layout_debug_log(&bak_msg);
      }
    }
  }
  None
}

/// Best-effort startup load from `layout.json` (with `.bak` recovery).
///
/// Missing / empty / invalid JSON / restore errors are logged and ignored
/// (default GlazeWM behaviour). Never fatal. When the primary file is
/// missing or invalid but `layout.json.bak` is valid, that backup is
/// loaded and promoted back to the primary path when possible.
pub fn try_load_persisted_layout_snapshot(
  path: &Path,
  state: &mut WmState,
  config: &UserConfig,
) -> Option<LoadLayoutSummary> {
  let attempt = format!("startup load attempting from {}", path.display());
  info!("{attempt}");
  layout_debug_log(&attempt);

  let snapshot = resolve_persisted_layout_snapshot(path)?;

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
  use std::path::PathBuf;

  use super::*;

  #[test]
  fn layout_path_joins_beside_config_yaml() {
    let config =
      PathBuf::from(r"C:\Users\example\.glzr\glazewm\config.yaml");
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
  fn verbose_z_order_defaults_off_and_respects_config_flag() {
    // Config flag alone must enable diagnostics. Env override is OR-ed at
    // read time; skip mutating process env (unsafe on recent Rust).
    let env_forced =
      std::env::var("GLAZEWM_VERBOSE_Z_ORDER").ok().as_deref() == Some("1");
    set_verbose_z_order_from_config(true);
    assert!(verbose_z_order_enabled());
    set_verbose_z_order_from_config(false);
    assert_eq!(verbose_z_order_enabled(), env_forced);
  }

  #[test]
  fn layout_debug_log_path_joins_beside_config() {
    let config =
      PathBuf::from(r"C:\Users\example\.glzr\glazewm\config.yaml");
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
  fn auto_save_armed_flag_is_exit_flush_gate() {
    // Clean exit (main.rs) flushes when is_armed() is true. Re-enable
    // clears a pending debounce without writing (startup drain /
    // disable path).
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_time()
      .build()
      .unwrap();
    rt.block_on(async {
      let mut auto = LayoutAutoSave::new(PathBuf::from("layout.json"));
      auto.enable();
      auto.schedule(&WmEvent::ApplicationExiting);
      assert!(
        auto.is_armed(),
        "pending debounce must be visible to the exit flush gate"
      );
      // enable() is what startup uses to reset; it must clear armed
      // without a WmState write so the exit path remains the only
      // flush consumer here.
      auto.enable();
      assert!(!auto.is_armed());
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
  fn workspace_update_arms_snapshot_gate() {
    let workspace = wm_common::WorkspaceDto {
      id: uuid::Uuid::new_v4(),
      name: "wks1".into(),
      display_name: None,
      parent_id: None,
      children: Vec::new(),
      child_focus_order: Vec::new(),
      has_focus: true,
      is_displayed: true,
      width: 0,
      height: 0,
      x: 0,
      y: 0,
      tiling_direction: wm_common::TilingDirection::Horizontal,
    };

    assert!(wm_event_affects_layout_snapshot(
      &WmEvent::WorkspaceUpdated {
        updated_workspace: wm_common::ContainerDto::Workspace(workspace),
      }
    ));
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
    let dir = std::env::temp_dir()
      .join(format!("glazewm-layout-empty-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("layout.json");
    fs::write(&path, "   \n").unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.trim(), "");
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir(&dir);
  }

  #[test]
  fn layout_debug_log_appends_when_path_set() {
    let dir = std::env::temp_dir()
      .join(format!("glazewm-layout-debug-{}", std::process::id()));
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

  fn minimal_valid_snapshot_json(marker: &str) -> String {
    format!(
      r#"{{"version":1,"capturedAt":"{marker}","paused":false,"bindingModes":[],"monitors":[],"ignoredWindows":[]}}"#
    )
  }

  fn temp_layout_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
      "glazewm-layout-bak-{}-{}-{}",
      prefix,
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let _ = fs::create_dir_all(&dir);
    dir
  }

  #[test]
  fn resolve_recovers_when_primary_missing_and_bak_valid() {
    let dir = temp_layout_dir("missing-primary");
    let primary = dir.join("layout.json");
    let bak = sibling_bak_path(&primary);
    let _ = fs::remove_file(&primary);
    fs::write(&bak, minimal_valid_snapshot_json("from-bak")).unwrap();

    let snapshot = resolve_persisted_layout_snapshot(&primary)
      .expect("should recover from .bak");
    assert_eq!(snapshot.captured_at, "from-bak");
    assert!(
      primary.exists(),
      "successful .bak recovery should promote backup to primary"
    );
    assert!(fs::read_to_string(&primary).unwrap().contains("from-bak"));
    assert!(
      !bak.exists(),
      "promoted .bak should no longer sit beside primary"
    );
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn resolve_recovers_when_primary_corrupt_and_bak_valid() {
    let dir = temp_layout_dir("corrupt-primary");
    let primary = dir.join("layout.json");
    let bak = sibling_bak_path(&primary);
    fs::write(&primary, "{not-json").unwrap();
    fs::write(&bak, minimal_valid_snapshot_json("bak-wins")).unwrap();

    let snapshot = resolve_persisted_layout_snapshot(&primary)
      .expect("should recover from .bak when primary is corrupt");
    assert_eq!(snapshot.captured_at, "bak-wins");
    assert!(primary.exists());
    let promoted = fs::read_to_string(&primary).unwrap();
    assert!(promoted.contains("bak-wins"));
    assert!(!promoted.contains("not-json"));
    assert!(!bak.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn resolve_returns_none_when_primary_and_bak_missing() {
    let dir = temp_layout_dir("both-missing");
    let primary = dir.join("layout.json");
    let _ = fs::remove_file(&primary);
    let _ = fs::remove_file(sibling_bak_path(&primary));
    assert!(resolve_persisted_layout_snapshot(&primary).is_none());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn resolve_prefers_valid_primary_over_bak() {
    let dir = temp_layout_dir("prefer-primary");
    let primary = dir.join("layout.json");
    let bak = sibling_bak_path(&primary);
    fs::write(&primary, minimal_valid_snapshot_json("primary")).unwrap();
    fs::write(&bak, minimal_valid_snapshot_json("bak")).unwrap();

    let snapshot = resolve_persisted_layout_snapshot(&primary)
      .expect("valid primary should load");
    assert_eq!(snapshot.captured_at, "primary");
    // Primary left alone; bak untouched.
    assert!(bak.exists());
    let _ = fs::remove_dir_all(&dir);
  }
}
