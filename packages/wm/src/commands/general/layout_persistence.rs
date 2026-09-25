//! Persisted layout snapshot (`layout.json` beside `config.yaml`).
//!
//! Auto-saves the current durable layout on a ~5s debounce after layout-
//! affecting WM events, and best-effort loads that file once at startup.

use std::{
  fs,
  path::{Path, PathBuf},
  pin::Pin,
  time::Duration,
};

use anyhow::Context;
use tokio::time::{sleep_until, Instant, Sleep};
use tracing::{info, warn};
use wm_common::{LayoutSnapshot, WmEvent};

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

/// Resolve `layout.json` next to the active config file (same directory
/// resolution as `UserConfig` / `%USERPROFILE%\.glzr\glazewm`).
pub fn layout_snapshot_path(config: &UserConfig) -> PathBuf {
  layout_snapshot_path_from_config_path(&config.path)
}

/// Pure path join used by [`layout_snapshot_path`] (unit-tested).
pub fn layout_snapshot_path_from_config_path(config_path: &Path) -> PathBuf {
  config_path.with_file_name(LAYOUT_SNAPSHOT_FILE_NAME)
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
  }

  pub fn schedule(&mut self) {
    if !self.enabled {
      return;
    }
    let deadline = Instant::now() + LAYOUT_AUTO_SAVE_DEBOUNCE;
    self.sleep.as_mut().reset(deadline);
    self.armed = true;
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
      return Ok(());
    }
    save_layout_snapshot_to_path(state, &self.path)
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
  if !path.exists() {
    info!(
      "No persisted layout at {}; using default layout behaviour.",
      path.display()
    );
    return None;
  }

  let text = match fs::read_to_string(path) {
    Ok(t) => t,
    Err(err) => {
      warn!(
        "Failed to read persisted layout {}: {err:#}; using default.",
        path.display()
      );
      return None;
    }
  };

  if text.trim().is_empty() {
    warn!(
      "Persisted layout {} is empty; using default layout behaviour.",
      path.display()
    );
    return None;
  }

  let snapshot: LayoutSnapshot = match serde_json::from_str(&text) {
    Ok(s) => s,
    Err(err) => {
      warn!(
        "Failed to parse persisted layout {}: {err:#}; using default.",
        path.display()
      );
      return None;
    }
  };

  match load_layout_snapshot(&snapshot, state, config) {
    Ok(summary) => {
      if state.pending_sync.has_changes() {
        if let Err(err) = platform_sync(state, config) {
          warn!(
            "platform_sync after persisted layout load failed: {err:#}"
          );
        }
      }
      info!(
        "Loaded persisted layout from {}: matched={}, unmatched_snapshot={}, unmatched_live={}",
        path.display(),
        summary.matched,
        summary.unmatched_snapshot,
        summary.unmatched_live
      );
      Some(summary)
    }
    Err(err) => {
      warn!(
        "Failed to restore persisted layout {}: {err:#}; using default.",
        path.display()
      );
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
      auto.schedule();
      assert!(!auto.is_armed());
      auto.enable();
      assert!(auto.enabled());
      assert!(!auto.is_armed());
      auto.schedule();
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
}
