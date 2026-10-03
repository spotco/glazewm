use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;
use wm_common::{
  ContainerDto, DisplayState, HideMethod, TilingDirection, WindowDto,
};
#[cfg(target_os = "windows")]
use wm_platform::{sample_z_order_ranks, WindowId};
use wm_platform::{Dispatcher, NativeWindowDebugInfo};

use crate::{
  diagnostic_history::{
    self, hwnd_i64, NativeZRank, DIAGNOSTIC_HISTORY_CAP,
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

const SCHEMA_VERSION: u32 = 1;
const KEY_STYLE: &str = "snake_case envelope; nested wm and native objects keep their existing IPC camelCase keys";

/// Writes one pretty-printed JSON snapshot under the user's dumps folder.
///
/// Directory: `%USERPROFILE%\.glzr\glazewm\dumps` (this machine:
/// `C:\Users\mooto\.glzr\glazewm\dumps`).
pub fn dump_wm_state(
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<PathBuf> {
  let dump = build_state_dump(state, config)?;
  let path = write_state_dump(&dump)?;
  tracing::info!("dumped wm state to {}", path.display());
  Ok(path)
}

/// Writes the same dump to `path`, overwriting an existing file.
///
/// A missing `.json` extension is appended. No file picker.
pub fn dump_wm_state_to_path(
  state: &WmState,
  config: &UserConfig,
  path: &Path,
) -> anyhow::Result<PathBuf> {
  let dump = build_state_dump(state, config)?;
  let path = ensure_json_extension(path.to_path_buf());
  write_dump_file(&path, &dump, true)?;
  tracing::info!("dumped wm state to {}", path.display());
  Ok(path)
}

/// Tray path: save picker, then a copyable dialog with the written path.
///
/// Cancel leaves state untouched and shows nothing. The CLI command does
/// not call this.
pub fn dump_wm_state_with_dialog(
  state: &WmState,
  config: &UserConfig,
  dispatcher: &Dispatcher,
) -> anyhow::Result<()> {
  let directory = state_dump_directory();
  fs::create_dir_all(&directory).with_context(|| {
    format!("create dumps directory {}", directory.display())
  })?;
  let default_name =
    default_state_dump_filename(&diagnostic_history::local_file_stamp());
  let directory_for_dialog = directory.clone();
  let picked = dispatcher
    .dispatch_sync(move || {
      rfd::FileDialog::new()
        .set_title("GlazeWM state dump")
        .add_filter("JSON", &["json"])
        .set_directory(&directory_for_dialog)
        .set_file_name(&default_name)
        .save_file()
    })
    .map_err(|err| {
      anyhow::anyhow!("state dump file dialog failed: {err}")
    })?;

  let Some(path) = picked else {
    tracing::info!("State dump cancelled.");
    return Ok(());
  };

  let path = dump_wm_state_to_path(state, config, &path)?;
  dispatcher.show_copyable_text_dialog(
    "GlazeWM state dump",
    &path.display().to_string(),
  );
  Ok(())
}

fn build_state_dump(
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<StateDump> {
  let (top_level_windows, debug_windows_error) =
    match state.dispatcher.debug_windows() {
      Ok(windows) => (windows, None),
      Err(err) => (
        Vec::new(),
        Some(diagnostic_history::truncate_error(&err.to_string())),
      ),
    };
  let native_by_handle = native_index(&top_level_windows);
  let windows = managed_windows(state, config, &native_by_handle)?;
  let ignored_windows = ignored_windows(state, &native_by_handle);
  let z_order = workspace_z_order(state);
  let history: Vec<_> =
    state.diagnostic_records().iter().cloned().collect();
  let workspace_count = state.workspaces().len();
  let monitor_count = state.monitors().len();

  Ok(StateDump {
    schema_version: SCHEMA_VERSION,
    captured_at_local: diagnostic_history::local_iso(),
    captured_at_unix_ms: diagnostic_history::unix_ms(),
    key_style: KEY_STYLE,
    history_order: "oldest_first",
    app: AppInfo {
      version: env!("VERSION_NUMBER").to_string(),
      build_id: env!("SPOTCO_BUILD_ID").to_string(),
    },
    counts: Counts {
      monitor_count,
      workspace_count,
      window_count: windows.len(),
      ignored_window_count: ignored_windows.len(),
      top_level_window_count: top_level_windows.len(),
      history_len: history.len(),
      history_capacity: DIAGNOSTIC_HISTORY_CAP,
    },
    wm: wm_section(state, config)?,
    z_order,
    windows,
    ignored_windows,
    top_level_windows,
    debug_windows_error,
    history,
  })
}

fn wm_section(
  state: &WmState,
  config: &UserConfig,
) -> anyhow::Result<WmSection> {
  let focused_container = match state.focused_container() {
    Some(container) => Some(serde_json::to_value(container.to_dto()?)?),
    None => None,
  };
  let os_foreground = state
    .dispatcher
    .focused_window()
    .ok()
    .map(|window| window.debug_info());
  let binding_modes = state
    .binding_modes
    .iter()
    .map(|mode| BindingModeDump {
      name: mode.name.clone(),
      display_name: mode.display_name.clone(),
    })
    .collect();

  Ok(WmSection {
    is_paused: state.is_paused,
    is_focus_synced: state.is_focus_synced,
    recent_workspace_name: state.recent_workspace_name.clone(),
    global_tiling_direction: state.global_tiling_direction.clone(),
    binding_modes,
    ignored_native_foreground: state
      .ignored_native_foreground
      .map(|window_id| hwnd_i64(window_id.0)),
    startup_z_order_pending: state.startup_z_order_pending(),
    show_desktop_session_active: state.show_desktop_session_active(),
    show_desktop_minimized_ids: state.show_desktop_minimized_ids(),
    hide_method: hide_method_name(&config.value.general.hide_method),
    show_all_in_taskbar: config.value.general.show_all_in_taskbar,
    layout_history_undo_depth: state.layout_history.undo_depth(),
    layout_history_redo_depth: state.layout_history.redo_depth(),
    layout_history_last_transaction_id: state
      .layout_history
      .last_transaction_id(),
    focused_container,
    os_foreground,
    tree: serde_json::to_value(state.root_container.to_dto()?)?,
  })
}

fn managed_windows(
  state: &WmState,
  config: &UserConfig,
  native_by_handle: &HashMap<isize, NativeWindowDebugInfo>,
) -> anyhow::Result<Vec<ManagedWindowDump>> {
  let mut windows = Vec::new();
  for window in state.windows() {
    let ContainerDto::Window(wm) = window.to_dto()? else {
      continue;
    };
    let snapshot = window.native().debug_info();
    let native = native_by_handle
      .get(&snapshot.handle)
      .cloned()
      .unwrap_or(snapshot);
    let workspace = window.workspace();
    let (taskbar_visibility, taskbar_visible) = taskbar_visibility(
      &config.value.general.hide_method,
      config.value.general.show_all_in_taskbar,
      &window.display_state(),
    );
    windows.push(ManagedWindowDump {
      hwnd: hwnd_i64(native.handle),
      workspace_id: workspace.as_ref().map(CommonGetters::id),
      workspace_name: workspace
        .as_ref()
        .map(|workspace| workspace.config().name),
      monitor_id: window.monitor().as_ref().map(CommonGetters::id),
      owner_hwnd: native.owner_handle.map(hwnd_i64),
      taskbar_visibility,
      taskbar_visible,
      wm,
      native,
    });
  }
  windows.sort_by_key(|window| window.hwnd);
  Ok(windows)
}

fn ignored_windows(
  state: &WmState,
  native_by_handle: &HashMap<isize, NativeWindowDebugInfo>,
) -> Vec<NativeWindowDebugInfo> {
  let mut ignored: Vec<_> = state
    .ignored_windows
    .iter()
    .map(|window| {
      let snapshot = window.debug_info();
      native_by_handle
        .get(&snapshot.handle)
        .cloned()
        .unwrap_or(snapshot)
    })
    .collect();
  ignored.sort_by_key(|window| window.handle);
  ignored
}

fn workspace_z_order(state: &WmState) -> Vec<WorkspaceZOrder> {
  let mut rows = Vec::new();
  for workspace in state.workspaces() {
    let intended_ids = state
      .normal_z_order_for_workspace(workspace.id())
      .unwrap_or(&[]);
    rows.push(WorkspaceZOrder {
      workspace_id: workspace.id(),
      workspace_name: workspace.config().name,
      monitor_id: workspace.monitor().as_ref().map(CommonGetters::id),
      is_displayed: workspace.is_displayed(),
      intended_top_to_bottom: intended_ids
        .iter()
        .map(|window_id| hwnd_i64(window_id.0))
        .collect(),
      native_ranks: native_ranks_for(intended_ids),
      native_sample_timing: "dump_time",
    });
  }
  rows.sort_by(|left, right| {
    left
      .workspace_name
      .cmp(&right.workspace_name)
      .then(left.workspace_id.cmp(&right.workspace_id))
  });
  rows
}

#[cfg(target_os = "windows")]
fn native_ranks_for(window_ids: &[WindowId]) -> Vec<NativeZRank> {
  let mut ranks: Vec<_> = sample_z_order_ranks(window_ids)
    .into_iter()
    .map(|(window_id, z_order_index)| NativeZRank {
      hwnd: hwnd_i64(window_id.0),
      z_order_index,
    })
    .collect();
  ranks.sort_by_key(|rank| (rank.z_order_index, rank.hwnd));
  ranks
}

#[cfg(not(target_os = "windows"))]
fn native_ranks_for(
  _window_ids: &[wm_platform::WindowId],
) -> Vec<NativeZRank> {
  Vec::new()
}

fn native_index(
  windows: &[NativeWindowDebugInfo],
) -> HashMap<isize, NativeWindowDebugInfo> {
  windows
    .iter()
    .cloned()
    .map(|window| (window.handle, window))
    .collect()
}

fn taskbar_visibility(
  hide_method: &HideMethod,
  show_all_in_taskbar: bool,
  display_state: &DisplayState,
) -> (&'static str, Option<bool>) {
  let tracks =
    matches!(hide_method, HideMethod::Cloak) && !show_all_in_taskbar;
  if !tracks {
    return ("not_tracked", None);
  }
  match display_state {
    DisplayState::Shown | DisplayState::Showing => ("shown", Some(true)),
    DisplayState::Hidden | DisplayState::Hiding => ("hidden", Some(false)),
  }
}

fn hide_method_name(hide_method: &HideMethod) -> &'static str {
  match hide_method {
    HideMethod::Hide => "hide",
    HideMethod::Cloak => "cloak",
    HideMethod::PlaceInCorner => "place_in_corner",
  }
}

pub(crate) fn state_dump_directory() -> PathBuf {
  home::home_dir()
    .unwrap_or_else(|| PathBuf::from(r"C:\Users\mooto"))
    .join(".glzr")
    .join("glazewm")
    .join("dumps")
}

pub(crate) fn default_state_dump_filename(stamp: &str) -> String {
  format!("state-{stamp}.json")
}

/// Appends `.json` when the chosen name does not already end with it.
pub(crate) fn ensure_json_extension(path: PathBuf) -> PathBuf {
  match path.extension().and_then(|ext| ext.to_str()) {
    Some(ext) if ext.eq_ignore_ascii_case("json") => path,
    Some(_) => {
      let mut os = path.into_os_string();
      os.push(".json");
      PathBuf::from(os)
    }
    None => path.with_extension("json"),
  }
}

fn unique_state_dump_path(
  directory: &Path,
  stamp: &str,
) -> anyhow::Result<PathBuf> {
  let mut path = directory.join(default_state_dump_filename(stamp));
  let mut suffix = 2u32;
  while path.exists() {
    path = directory.join(format!("state-{stamp}-{suffix}.json"));
    suffix = suffix.saturating_add(1);
    if suffix > 50 {
      anyhow::bail!("too many state dumps in the same millisecond");
    }
  }
  Ok(path)
}

fn write_state_dump(dump: &StateDump) -> anyhow::Result<PathBuf> {
  let directory = state_dump_directory();
  fs::create_dir_all(&directory).with_context(|| {
    format!("create dumps directory {}", directory.display())
  })?;
  let path = unique_state_dump_path(
    &directory,
    &diagnostic_history::local_file_stamp(),
  )?;
  write_dump_file(&path, dump, false)?;
  Ok(path)
}

fn write_dump_file(
  path: &Path,
  dump: &StateDump,
  overwrite: bool,
) -> anyhow::Result<()> {
  if let Some(parent) = path.parent() {
    if !parent.as_os_str().is_empty() {
      fs::create_dir_all(parent).with_context(|| {
        format!("create dumps directory {}", parent.display())
      })?;
    }
  }
  let json = serde_json::to_string_pretty(dump)?;
  let partial = path.with_extension("json.partial");
  fs::write(&partial, format!("{json}\n"))
    .with_context(|| format!("write state dump {}", partial.display()))?;
  if overwrite && path.exists() {
    fs::remove_file(path).with_context(|| {
      format!("replace existing state dump {}", path.display())
    })?;
  }
  fs::rename(&partial, path).or_else(|err| {
    let _ = fs::remove_file(path);
    fs::rename(&partial, path).or(Err(err))
  })?;
  Ok(())
}

#[derive(Serialize)]
struct StateDump {
  schema_version: u32,
  captured_at_local: String,
  captured_at_unix_ms: u64,
  key_style: &'static str,
  history_order: &'static str,
  app: AppInfo,
  counts: Counts,
  wm: WmSection,
  z_order: Vec<WorkspaceZOrder>,
  windows: Vec<ManagedWindowDump>,
  ignored_windows: Vec<NativeWindowDebugInfo>,
  top_level_windows: Vec<NativeWindowDebugInfo>,
  debug_windows_error: Option<String>,
  history: Vec<crate::diagnostic_history::DiagnosticRecord>,
}

#[derive(Serialize)]
struct AppInfo {
  version: String,
  build_id: String,
}

#[derive(Serialize)]
struct Counts {
  monitor_count: usize,
  workspace_count: usize,
  window_count: usize,
  ignored_window_count: usize,
  top_level_window_count: usize,
  history_len: usize,
  history_capacity: usize,
}

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct WmSection {
  is_paused: bool,
  is_focus_synced: bool,
  recent_workspace_name: Option<String>,
  global_tiling_direction: TilingDirection,
  binding_modes: Vec<BindingModeDump>,
  ignored_native_foreground: Option<i64>,
  startup_z_order_pending: bool,
  show_desktop_session_active: bool,
  show_desktop_minimized_ids: Vec<Uuid>,
  hide_method: &'static str,
  show_all_in_taskbar: bool,
  layout_history_undo_depth: usize,
  layout_history_redo_depth: usize,
  layout_history_last_transaction_id: Option<u64>,
  focused_container: Option<Value>,
  os_foreground: Option<NativeWindowDebugInfo>,
  tree: Value,
}

#[derive(Serialize)]
struct BindingModeDump {
  name: String,
  display_name: Option<String>,
}

#[derive(Serialize)]
struct WorkspaceZOrder {
  workspace_id: Uuid,
  workspace_name: String,
  monitor_id: Option<Uuid>,
  is_displayed: bool,
  intended_top_to_bottom: Vec<i64>,
  native_ranks: Vec<NativeZRank>,
  native_sample_timing: &'static str,
}

#[derive(Serialize)]
struct ManagedWindowDump {
  hwnd: i64,
  workspace_id: Option<Uuid>,
  workspace_name: Option<String>,
  monitor_id: Option<Uuid>,
  owner_hwnd: Option<i64>,
  taskbar_visibility: &'static str,
  taskbar_visible: Option<bool>,
  wm: WindowDto,
  native: NativeWindowDebugInfo,
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use wm_common::{DisplayState, HideMethod, TilingDirection};

  use super::{
    default_state_dump_filename, ensure_json_extension,
    state_dump_directory, taskbar_visibility, unique_state_dump_path,
    StateDump, KEY_STYLE, SCHEMA_VERSION,
  };

  #[test]
  fn envelope_keys_stay_in_schema_order() {
    let dump = StateDump {
      schema_version: SCHEMA_VERSION,
      captured_at_local: "2026-10-02T23:00:00.000".into(),
      captured_at_unix_ms: 0,
      key_style: KEY_STYLE,
      history_order: "oldest_first",
      app: super::AppInfo {
        version: "test".into(),
        build_id: "test".into(),
      },
      counts: super::Counts {
        monitor_count: 0,
        workspace_count: 0,
        window_count: 0,
        ignored_window_count: 0,
        top_level_window_count: 0,
        history_len: 0,
        history_capacity: 200,
      },
      wm: super::WmSection {
        is_paused: false,
        is_focus_synced: true,
        recent_workspace_name: None,
        global_tiling_direction: TilingDirection::Horizontal,
        binding_modes: Vec::new(),
        ignored_native_foreground: None,
        startup_z_order_pending: false,
        show_desktop_session_active: false,
        show_desktop_minimized_ids: Vec::new(),
        hide_method: "cloak",
        show_all_in_taskbar: false,
        layout_history_undo_depth: 0,
        layout_history_redo_depth: 0,
        layout_history_last_transaction_id: None,
        focused_container: None,
        os_foreground: None,
        tree: serde_json::Value::Null,
      },
      z_order: Vec::new(),
      windows: Vec::new(),
      ignored_windows: Vec::new(),
      top_level_windows: Vec::new(),
      debug_windows_error: None,
      history: Vec::new(),
    };
    let value = serde_json::to_value(&dump).unwrap();
    let keys: Vec<_> =
      value.as_object().unwrap().keys().cloned().collect();
    // serde_json object maps sort keys. That order is the stable
    // contract, not source field order.
    assert_eq!(
      keys,
      [
        "app",
        "captured_at_local",
        "captured_at_unix_ms",
        "counts",
        "debug_windows_error",
        "history",
        "history_order",
        "ignored_windows",
        "key_style",
        "schema_version",
        "top_level_windows",
        "windows",
        "wm",
        "z_order",
      ]
    );
    let pretty = serde_json::to_string_pretty(&dump).unwrap();
    assert!(pretty.contains("\n  \"schema_version\": 1"));
  }

  #[test]
  fn taskbar_visibility_follows_cloak_tracking() {
    assert_eq!(
      taskbar_visibility(&HideMethod::Cloak, false, &DisplayState::Hidden),
      ("hidden", Some(false))
    );
    assert_eq!(
      taskbar_visibility(&HideMethod::Cloak, true, &DisplayState::Hidden),
      ("not_tracked", None)
    );
    assert_eq!(
      taskbar_visibility(&HideMethod::Hide, false, &DisplayState::Shown),
      ("not_tracked", None)
    );
  }

  #[test]
  fn default_filename_includes_the_timestamp_stamp() {
    let name = default_state_dump_filename("20261003-002530-123");
    assert_eq!(name, "state-20261003-002530-123.json");
    assert!(name.contains("20261003-002530-123"));
  }

  #[test]
  fn dump_directory_is_glazewm_dumps_under_home() {
    let dir = state_dump_directory();
    let rendered = dir.to_string_lossy().replace('\\', "/");
    assert!(rendered.ends_with(".glzr/glazewm/dumps"), "{rendered}");
  }

  #[test]
  fn json_extension_is_kept_or_appended() {
    assert_eq!(
      ensure_json_extension(PathBuf::from("state-1.json")),
      PathBuf::from("state-1.json")
    );
    assert_eq!(
      ensure_json_extension(PathBuf::from("state-1.JSON")),
      PathBuf::from("state-1.JSON")
    );
    assert_eq!(
      ensure_json_extension(PathBuf::from("state-1")),
      PathBuf::from("state-1.json")
    );
    assert_eq!(
      ensure_json_extension(PathBuf::from("notes.txt")),
      PathBuf::from("notes.txt.json")
    );
  }

  #[test]
  fn unique_path_adds_a_suffix_when_the_stamp_exists() {
    let nanos = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let dir = std::env::temp_dir().join(format!(
      "glazewm-dump-name-{}-{}",
      std::process::id(),
      nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let stamp = "20261003-002530-123";
    let first = dir.join(default_state_dump_filename(stamp));
    std::fs::write(&first, b"{}").unwrap();
    let next = unique_state_dump_path(&dir, stamp).unwrap();
    assert_eq!(next, dir.join(format!("state-{stamp}-2.json")));
    let _ = std::fs::remove_dir_all(&dir);
  }
}
