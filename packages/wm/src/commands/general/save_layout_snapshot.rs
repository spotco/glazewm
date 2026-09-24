use std::{
  fs,
  path::{Path, PathBuf},
  time::SystemTime,
};

use anyhow::Context;
use tracing::info;
use wm_common::{
  format_system_time_rfc3339, LayoutSnapshot, SnapshotWindow,
  SnapshotWindowIdentity,
};
use wm_platform::Dispatcher;

use crate::wm_state::WmState;

/// Build a live layout snapshot (same shape as `query layout`).
pub fn build_layout_snapshot(
  state: &WmState,
) -> anyhow::Result<LayoutSnapshot> {
  let monitors: Vec<_> = state
    .monitors()
    .into_iter()
    .map(|monitor| monitor.to_dto())
    .try_collect()?;

  let ignored_windows = state
    .ignored_windows
    .iter()
    .map(snapshot_from_native_window)
    .collect();

  let binding_modes = state
    .binding_modes
    .iter()
    .map(|mode| mode.name.clone())
    .collect();

  Ok(LayoutSnapshot::from_monitor_dtos(
    &monitors,
    ignored_windows,
    state.is_paused,
    binding_modes,
    Some(env!("VERSION_NUMBER").to_string()),
    format_system_time_rfc3339(SystemTime::now()),
  ))
}

/// Pretty durable JSON for clipboard / file write.
pub fn durable_snapshot_json(state: &WmState) -> anyhow::Result<String> {
  let snapshot = build_layout_snapshot(state)?.into_durable();
  Ok(serde_json::to_string_pretty(&snapshot)?)
}

/// Copy durable layout snapshot JSON to the system clipboard.
pub fn copy_layout_snapshot_to_clipboard(
  state: &WmState,
) -> anyhow::Result<()> {
  let json = durable_snapshot_json(state)?;
  set_clipboard_text(&json)?;
  info!(
    "Copied layout snapshot to clipboard ({} bytes).",
    json.len()
  );
  Ok(())
}

/// Save durable layout snapshot via native dialog and also copy to clipboard.
pub fn save_layout_snapshot_with_dialog(
  state: &WmState,
  dispatcher: &Dispatcher,
) -> anyhow::Result<()> {
  let json = durable_snapshot_json(state)?;
  let default_name = default_layout_filename();

  let path = dispatcher.dispatch_sync(move || {
    rfd::FileDialog::new()
      .add_filter("JSON", &["json"])
      .set_file_name(&default_name)
      .save_file()
  })?;

  let Some(path) = path else {
    info!("Save layout snapshot cancelled.");
    return Ok(());
  };

  write_snapshot_file(&path, &json)?;
  if let Err(err) = set_clipboard_text(&json) {
    tracing::warn!(
      "Saved layout snapshot but failed to copy to clipboard: {err}"
    );
  } else {
    info!("Also copied layout snapshot to clipboard.");
  }

  Ok(())
}

/// Open a layout snapshot JSON via native dialog.
pub fn pick_layout_snapshot_path(
  dispatcher: &Dispatcher,
) -> anyhow::Result<Option<PathBuf>> {
  let path = dispatcher.dispatch_sync(|| {
    rfd::FileDialog::new()
      .add_filter("JSON", &["json"])
      .pick_file()
  })?;
  Ok(path)
}

pub fn read_layout_snapshot_file(
  path: &Path,
) -> anyhow::Result<LayoutSnapshot> {
  let text = fs::read_to_string(path)
    .with_context(|| format!("Failed to read {}", path.display()))?;
  let snapshot: LayoutSnapshot = serde_json::from_str(&text)
    .with_context(|| format!("Failed to parse {}", path.display()))?;
  Ok(snapshot)
}

fn write_snapshot_file(path: &Path, json: &str) -> anyhow::Result<()> {
  fs::write(path, json)
    .with_context(|| format!("Failed to write {}", path.display()))?;
  info!("Saved layout snapshot to {}.", path.display());
  Ok(())
}

fn set_clipboard_text(text: &str) -> anyhow::Result<()> {
  let mut clipboard = arboard::Clipboard::new()
    .context("Failed to open system clipboard.")?;
  clipboard
    .set_text(text.to_string())
    .context("Failed to set clipboard text.")?;
  Ok(())
}

fn default_layout_filename() -> String {
  let rfc = format_system_time_rfc3339(SystemTime::now());
  let digits: String =
    rfc.chars().filter(char::is_ascii_digit).collect();
  // RFC3339 UTC → at least YYYYMMDDHHMMSS
  if digits.len() >= 14 {
    format!(
      "glazewm-layout-{}-{}.json",
      &digits[0..8],
      &digits[8..14]
    )
  } else {
    "glazewm-layout.json".to_string()
  }
}

fn snapshot_from_native_window(
  native: &wm_platform::NativeWindow,
) -> SnapshotWindow {
  #[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
  let handle = native.id().0 as isize;

  SnapshotWindow {
    identity: SnapshotWindowIdentity {
      process_path: native.process_path().ok(),
      process_name: native
        .process_name()
        .unwrap_or_else(|_| "unknown".to_string()),
      #[cfg(target_os = "windows")]
      class_name: {
        use wm_platform::NativeWindowWindowsExt;
        native.class_name().ok()
      },
      #[cfg(not(target_os = "windows"))]
      class_name: None,
      title_hint: native.title().ok(),
    },
    state: wm_common::WindowState::Floating(
      wm_common::FloatingStateConfig::default(),
    ),
    prev_state: None,
    floating_placement: native.frame().ok(),
    id: None,
    handle: Some(handle),
  }
}
