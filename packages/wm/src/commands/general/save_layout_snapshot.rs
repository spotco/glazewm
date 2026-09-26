use std::{
  fs::{self, File},
  io::Write,
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
    Some(state.global_tiling_direction.clone()),
  ))
}

/// Pretty durable JSON for clipboard / file write.
pub fn durable_snapshot_json(state: &WmState) -> anyhow::Result<String> {
  let snapshot = build_layout_snapshot(state)?.into_durable();
  Ok(serde_json::to_string_pretty(&snapshot)?)
}

/// Save durable layout snapshot via native dialog and also copy to
/// clipboard.
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

pub fn write_snapshot_file(path: &Path, json: &str) -> anyhow::Result<()> {
  atomic_write_snapshot_file(path, json.as_bytes())?;
  info!("Saved layout snapshot to {}.", path.display());
  Ok(())
}

/// Write `contents` via temp+replace so a crash mid-write cannot truncate
/// the only recovery `layout.json`. Manual Save and autosave both use this
/// helper.
///
/// On Windows, `rename` cannot clobber an existing destination, so the
/// previous file is moved aside to `*.bak` first. If the final replace
/// fails, the `.bak` is restored when possible.
fn atomic_write_snapshot_file(
  path: &Path,
  contents: &[u8],
) -> anyhow::Result<()> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent).with_context(|| {
        format!("Failed to create directory {}", parent.display())
      })?;
    }
  }

  let temp_path = sibling_temp_path(path);
  let bak_path = sibling_bak_path(path);

  {
    let mut file = File::create(&temp_path).with_context(|| {
      format!("Failed to create temp snapshot {}", temp_path.display())
    })?;
    file.write_all(contents).with_context(|| {
      format!("Failed to write temp snapshot {}", temp_path.display())
    })?;
    file.sync_all().with_context(|| {
      format!("Failed to sync temp snapshot {}", temp_path.display())
    })?;
  }

  // Move existing destination aside (Windows cannot rename over it).
  if path.exists() {
    let _ = fs::remove_file(&bak_path);
    if let Err(err) = fs::rename(path, &bak_path) {
      // If we cannot retain a .bak, still try a best-effort replace so a
      // successful write is not blocked by a stubborn previous file.
      tracing::warn!(
        "Could not retain previous snapshot as {}: {err:#}; removing destination",
        bak_path.display()
      );
      fs::remove_file(path).with_context(|| {
        format!("Failed to remove previous snapshot {}", path.display())
      })?;
    }
  }

  match fs::rename(&temp_path, path) {
    Ok(()) => Ok(()),
    Err(err) => {
      // Prefer restoring the previous file over leaving neither.
      if bak_path.exists() {
        let _ = fs::rename(&bak_path, path);
      }
      let _ = fs::remove_file(&temp_path);
      Err(err).with_context(|| {
        format!(
          "Failed to replace snapshot {} with temp {}",
          path.display(),
          temp_path.display()
        )
      })
    }
  }
}

fn sibling_temp_path(path: &Path) -> PathBuf {
  let mut os = path.as_os_str().to_owned();
  os.push(".tmp");
  PathBuf::from(os)
}

fn sibling_bak_path(path: &Path) -> PathBuf {
  let mut os = path.as_os_str().to_owned();
  os.push(".bak");
  PathBuf::from(os)
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
  let digits: String = rfc.chars().filter(char::is_ascii_digit).collect();
  // RFC3339 UTC → at least YYYYMMDDHHMMSS
  if digits.len() >= 14 {
    format!("glazewm-layout-{}-{}.json", &digits[0..8], &digits[8..14])
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
    floating_placement_relative: None,
    id: None,
    handle: Some(handle),
  }
}

#[cfg(test)]
mod tests {
  use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
  };

  use super::{
    atomic_write_snapshot_file, read_layout_snapshot_file,
    sibling_bak_path, sibling_temp_path,
  };

  fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "glazewm-{prefix}-{}-{}",
      std::process::id(),
      nanos
    ));
    let _ = fs::create_dir_all(&dir);
    dir
  }

  #[test]
  fn atomic_write_creates_file_and_is_readable() {
    let dir = temp_dir("atomic-write");
    let path = dir.join("layout.json");
    atomic_write_snapshot_file(&path, br#"{"ok":true}"#).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"ok":true}"#);
    assert!(!sibling_temp_path(&path).exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn atomic_write_replaces_existing_and_keeps_bak() {
    let dir = temp_dir("atomic-bak");
    let path = dir.join("layout.json");
    fs::write(&path, b"old").unwrap();
    atomic_write_snapshot_file(&path, b"new-content").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new-content");
    let bak = sibling_bak_path(&path);
    assert!(bak.exists(), "previous content should be retained as .bak");
    assert_eq!(fs::read_to_string(&bak).unwrap(), "old");
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn rejects_invalid_json_without_returning_snapshot() {
    let dir = temp_dir("layout-bad");
    let path = dir.join("bad.json");
    fs::write(&path, "{not json").unwrap();
    assert!(read_layout_snapshot_file(&path).is_err());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn rejects_unsupported_version_at_load_gate() {
    use wm_common::{
      validate_layout_snapshot_version, LayoutSnapshot,
      LAYOUT_SNAPSHOT_VERSION,
    };
    let snap = LayoutSnapshot {
      version: LAYOUT_SNAPSHOT_VERSION + 1,
      captured_at: "t".into(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      global_tiling_direction: None,
      monitors: vec![],
      ignored_windows: vec![],
    };
    assert!(validate_layout_snapshot_version(&snap).is_err());
  }
}
