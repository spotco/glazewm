use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wm_platform::Rect;

use crate::{
  ContainerDto, MonitorDto, TilingDirection, WindowDto, WindowState,
  WorkspaceDto,
};

/// Schema version for persisted layout snapshots.
pub const LAYOUT_SNAPSHOT_VERSION: u32 = 1;

/// Unified JSON export of monitors, workspaces, tiling tree, ignored
/// windows, and useful GlazeWM status. Foundation for save/load/restore.
///
/// Live queries may populate ephemeral `id` / `handle` fields for debugging.
/// Call [`LayoutSnapshot::strip_ephemeral`] before writing a durable file.
///
/// Tiling vs floating is the "dock" signal (user Super+T = toggle-floating).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LayoutSnapshot {
  pub version: u32,
  pub captured_at: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub glazewm_version: Option<String>,
  pub paused: bool,
  pub binding_modes: Vec<String>,
  pub monitors: Vec<SnapshotMonitor>,
  pub ignored_windows: Vec<SnapshotWindow>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMonitor {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub hardware_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub device_path: Option<String>,
  pub device_name: String,
  pub bounds: SnapshotBounds,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub focused_workspace_name: Option<String>,
  pub workspaces: Vec<SnapshotWorkspace>,
  /// Ephemeral live container id (cleared by [`LayoutSnapshot::strip_ephemeral`]).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBounds {
  pub x: i32,
  pub y: i32,
  pub width: i32,
  pub height: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotWorkspace {
  pub name: String,
  pub tiling_direction: TilingDirection,
  pub child_focus_order: Vec<String>,
  pub root: SnapshotNode,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotNode {
  pub local_id: String,
  pub kind: SnapshotNodeKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tiling_size: Option<f32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tiling_direction: Option<TilingDirection>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub children: Option<Vec<SnapshotNode>>,
  /// Local-id focus history for split nodes (from `SplitContainerDto.child_focus_order`).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub child_focus_order: Option<Vec<String>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub window: Option<SnapshotWindow>,
  /// Ephemeral source container id.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotNodeKind {
  Split,
  Window,
}

/// Durable window identity + state. `id` / `handle` are live-only.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotWindow {
  pub identity: SnapshotWindowIdentity,
  pub state: WindowState,
  /// Prior state before minimize/fullscreen; needed so restore can leave Minimized.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub prev_state: Option<WindowState>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub floating_placement: Option<Rect>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<Uuid>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub handle: Option<isize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotWindowIdentity {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub process_path: Option<String>,
  pub process_name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub class_name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub title_hint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IgnoredWindowsData {
  pub windows: Vec<SnapshotWindow>,
}

impl LayoutSnapshot {
  /// Build a live snapshot from existing monitor DTOs (same tree as
  /// `query monitors`) plus WM status and ignored windows.
  #[must_use]
  pub fn from_monitor_dtos(
    monitors: &[ContainerDto],
    ignored_windows: Vec<SnapshotWindow>,
    paused: bool,
    binding_modes: Vec<String>,
    glazewm_version: Option<String>,
    captured_at: String,
  ) -> Self {
    let monitors = monitors
      .iter()
      .filter_map(|dto| match dto {
        ContainerDto::Monitor(monitor) => {
          Some(SnapshotMonitor::from_monitor_dto(monitor))
        }
        _ => None,
      })
      .collect();

    Self {
      version: LAYOUT_SNAPSHOT_VERSION,
      captured_at,
      glazewm_version,
      paused,
      binding_modes,
      monitors,
      ignored_windows,
    }
  }

  /// Clear ephemeral Uuid/handle fields for durable persistence.
  pub fn strip_ephemeral(&mut self) {
    for monitor in &mut self.monitors {
      monitor.id = None;
      for workspace in &mut monitor.workspaces {
        workspace.id = None;
        strip_node_ephemeral(&mut workspace.root);
      }
    }
    for window in &mut self.ignored_windows {
      window.id = None;
      window.handle = None;
    }
  }

  #[must_use]
  pub fn into_durable(mut self) -> Self {
    self.strip_ephemeral();
    self
  }
}

impl SnapshotMonitor {
  fn from_monitor_dto(monitor: &MonitorDto) -> Self {
    let focused_workspace_name = monitor
      .child_focus_order
      .first()
      .and_then(|focus_id| {
        monitor.children.iter().find_map(|child| match child {
          ContainerDto::Workspace(ws) if ws.id == *focus_id => {
            Some(ws.name.clone())
          }
          _ => None,
        })
      });

    let workspaces = monitor
      .children
      .iter()
      .filter_map(|child| match child {
        ContainerDto::Workspace(ws) => {
          Some(SnapshotWorkspace::from_workspace_dto(ws))
        }
        _ => None,
      })
      .collect();

    Self {
      hardware_id: monitor.hardware_id.clone(),
      device_path: monitor.device_path.clone(),
      device_name: monitor.device_name.clone(),
      bounds: SnapshotBounds {
        x: monitor.x,
        y: monitor.y,
        width: monitor.width,
        height: monitor.height,
      },
      focused_workspace_name,
      workspaces,
      id: Some(monitor.id),
    }
  }
}

impl SnapshotWorkspace {
  fn from_workspace_dto(workspace: &WorkspaceDto) -> Self {
    let mut id_map = HashMap::new();
    let mut next_id = 0u32;

    let children: Vec<SnapshotNode> = workspace
      .children
      .iter()
      .filter_map(|child| {
        convert_node(child, &mut id_map, &mut next_id)
      })
      .collect();

    let child_focus_order = workspace
      .child_focus_order
      .iter()
      .filter_map(|uuid| id_map.get(uuid).cloned())
      .collect();

    // Workspace root is a virtual split with the workspace tiling direction.
    let root = SnapshotNode {
      local_id: next_local_id(&mut next_id),
      kind: SnapshotNodeKind::Split,
      tiling_size: None,
      tiling_direction: Some(workspace.tiling_direction.clone()),
      children: Some(children),
      // Workspace focus order lives on SnapshotWorkspace; virtual root has none.
      child_focus_order: None,
      window: None,
      id: Some(workspace.id),
    };

    Self {
      name: workspace.name.clone(),
      tiling_direction: workspace.tiling_direction.clone(),
      child_focus_order,
      root,
      id: Some(workspace.id),
    }
  }
}

fn convert_node(
  dto: &ContainerDto,
  id_map: &mut HashMap<Uuid, String>,
  next_id: &mut u32,
) -> Option<SnapshotNode> {
  match dto {
    ContainerDto::Split(split) => {
      let local_id = next_local_id(next_id);
      id_map.insert(split.id, local_id.clone());

      let children: Vec<SnapshotNode> = split
        .children
        .iter()
        .filter_map(|child| convert_node(child, id_map, next_id))
        .collect();

      let child_focus_order: Vec<String> = split
        .child_focus_order
        .iter()
        .filter_map(|uuid| id_map.get(uuid).cloned())
        .collect();

      Some(SnapshotNode {
        local_id,
        kind: SnapshotNodeKind::Split,
        tiling_size: Some(split.tiling_size),
        tiling_direction: Some(split.tiling_direction.clone()),
        children: Some(children),
        child_focus_order: if child_focus_order.is_empty() {
          None
        } else {
          Some(child_focus_order)
        },
        window: None,
        id: Some(split.id),
      })
    }
    ContainerDto::Window(window) => {
      let local_id = next_local_id(next_id);
      id_map.insert(window.id, local_id.clone());

      Some(SnapshotNode {
        local_id,
        kind: SnapshotNodeKind::Window,
        tiling_size: window.tiling_size,
        tiling_direction: None,
        children: None,
        child_focus_order: None,
        window: Some(SnapshotWindow::from_window_dto(window)),
        id: Some(window.id),
      })
    }
    _ => None,
  }
}

fn next_local_id(next_id: &mut u32) -> String {
  let id = format!("n{next_id}");
  *next_id += 1;
  id
}

fn strip_node_ephemeral(node: &mut SnapshotNode) {
  node.id = None;
  if let Some(window) = node.window.as_mut() {
    window.id = None;
    window.handle = None;
  }
  if let Some(children) = node.children.as_mut() {
    for child in children {
      strip_node_ephemeral(child);
    }
  }
}

impl SnapshotWindow {
  #[must_use]
  pub fn from_window_dto(window: &WindowDto) -> Self {
    Self {
      identity: SnapshotWindowIdentity {
        process_path: window.process_path.clone(),
        process_name: window.process_name.clone(),
        #[cfg(target_os = "windows")]
        class_name: Some(window.class_name.clone()),
        #[cfg(not(target_os = "windows"))]
        class_name: None,
        title_hint: Some(window.title.clone()),
      },
      state: window.state.clone(),
      prev_state: window.prev_state.clone(),
      floating_placement: Some(window.floating_placement.clone()),
      id: Some(window.id),
      handle: Some(window.handle),
    }
  }
}

/// Format `SystemTime` as RFC3339 UTC without extra dependencies.
#[must_use]
pub fn format_system_time_rfc3339(
  time: std::time::SystemTime,
) -> String {
  let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) else {
    return "1970-01-01T00:00:00Z".to_string();
  };
  let secs = duration.as_secs();
  let nanos = duration.subsec_nanos();

  let (year, month, day, hour, min, sec) = civil_from_days(secs);
  if nanos == 0 {
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
  } else {
    format!(
      "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{nanos:09}Z"
    )
  }
}

/// Howard Hinnant civil_from_days (UTC).
fn civil_from_days(unix_secs: u64) -> (i32, u32, u32, u32, u32, u32) {
  let z = (i64::try_from(unix_secs / 86_400).unwrap_or(0)) + 719_468;
  let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
  let doe = (z - era * 146_097) as u64;
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
  let y = i32::try_from(yoe).unwrap_or(0) + (era as i32) * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = if m <= 2 { y + 1 } else { y };

  let rem = unix_secs % 86_400;
  let hour = (rem / 3600) as u32;
  let min = ((rem % 3600) / 60) as u32;
  let sec = (rem % 60) as u32;

  (
    year,
    u32::try_from(m).unwrap_or(1),
    u32::try_from(d).unwrap_or(1),
    hour,
    min,
    sec,
  )
}


/// Validate snapshot schema version before any restore mutation.
///
/// Returns `Ok(())` when `version == LAYOUT_SNAPSHOT_VERSION`. Callers must
/// invoke this (or equivalent) before mutating WM state so unsupported
/// versions fail cleanly with no partial apply.
pub fn validate_layout_snapshot_version(
  snapshot: &LayoutSnapshot,
) -> Result<(), String> {
  if snapshot.version != LAYOUT_SNAPSHOT_VERSION {
    Err(format!(
      "Unsupported layout snapshot version {} (expected {}).",
      snapshot.version, LAYOUT_SNAPSHOT_VERSION
    ))
  } else {
    Ok(())
  }
}

/// Plan workspace-name → snapshot-monitor index (first occurrence wins).
///
/// Includes **empty** workspaces — restore must not rely solely on window
/// leaves to discover workspace→monitor placement.
#[must_use]
pub fn snapshot_workspace_monitor_indices(
  monitors: &[SnapshotMonitor],
) -> std::collections::HashMap<String, usize> {
  let mut planned = std::collections::HashMap::new();
  for (index, mon) in monitors.iter().enumerate() {
    for workspace in &mon.workspaces {
      planned.entry(workspace.name.clone()).or_insert(index);
    }
  }
  planned
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use crate::{DisplayState, SplitContainerDto};
  use wm_platform::{Rect, RectDelta};

  fn sample_window(id: Uuid, name: &str, tiling_size: Option<f32>) -> WindowDto {
    WindowDto {
      id,
      parent_id: None,
      has_focus: false,
      tiling_size,
      width: 100,
      height: 100,
      x: 0,
      y: 0,
      state: WindowState::Tiling,
      prev_state: None,
      display_state: DisplayState::Shown,
      border_delta: RectDelta::zero(),
      floating_placement: Rect::from_xy(10, 20, 300, 200),
      handle: 42,
      title: format!("{name} Title"),
      #[cfg(target_os = "windows")]
      class_name: format!("{name}Class"),
      process_name: name.to_string(),
      process_path: Some(format!("C:\\\\Apps\\\\{name}.exe")),
      active_drag: None,
    }
  }

  #[test]
  fn converts_monitor_tree_to_local_ids_and_focus_order() {
    let win_a = Uuid::from_u128(1);
    let win_b = Uuid::from_u128(2);
    let split_id = Uuid::from_u128(3);
    let ws_id = Uuid::from_u128(4);
    let mon_id = Uuid::from_u128(5);

    let workspace = WorkspaceDto {
      id: ws_id,
      name: "1".into(),
      display_name: None,
      parent_id: Some(mon_id),
      children: vec![ContainerDto::Split(SplitContainerDto {
        id: split_id,
        parent_id: Some(ws_id),
        children: vec![
          ContainerDto::Window(sample_window(win_a, "alpha", Some(0.5))),
          ContainerDto::Window(sample_window(win_b, "beta", Some(0.5))),
        ],
        child_focus_order: vec![win_b, win_a],
        has_focus: true,
        tiling_size: 1.0,
        width: 800,
        height: 600,
        x: 0,
        y: 0,
        tiling_direction: TilingDirection::Horizontal,
      })],
      child_focus_order: vec![split_id],
      has_focus: true,
      is_displayed: true,
      width: 800,
      height: 600,
      x: 0,
      y: 0,
      tiling_direction: TilingDirection::Horizontal,
    };

    let monitor = ContainerDto::Monitor(MonitorDto {
      id: mon_id,
      parent_id: None,
      children: vec![ContainerDto::Workspace(workspace)],
      child_focus_order: vec![ws_id],
      has_focus: true,
      width: 1920,
      height: 1080,
      x: 0,
      y: 0,
      dpi: 96,
      scale_factor: 1.0,
      handle: Some(1),
      device_name: "DISPLAY1".into(),
      device_path: Some("\\\\.\\DISPLAY1".into()),
      hardware_id: Some("HW1".into()),
      working_rect: Rect::from_xy(0, 0, 1920, 1040),
    });

    let snapshot = LayoutSnapshot::from_monitor_dtos(
      &[monitor],
      vec![],
      false,
      vec!["mode_a".into()],
      Some("9.9.9".into()),
      "2026-09-23T00:00:00Z".into(),
    );

    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.binding_modes, vec!["mode_a".to_string()]);
    assert_eq!(snapshot.monitors.len(), 1);
    let mon = &snapshot.monitors[0];
    assert_eq!(mon.device_name, "DISPLAY1");
    assert_eq!(mon.focused_workspace_name.as_deref(), Some("1"));
    assert_eq!(mon.workspaces.len(), 1);

    let ws = &mon.workspaces[0];
    assert_eq!(ws.child_focus_order, vec!["n0".to_string()]);
    assert_eq!(ws.root.kind, SnapshotNodeKind::Split);
    let children = ws.root.children.as_ref().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].kind, SnapshotNodeKind::Split);
    assert_eq!(children[0].local_id, "n0");
    let windows = children[0].children.as_ref().unwrap();
    assert_eq!(windows[0].local_id, "n1");
    assert_eq!(windows[1].local_id, "n2");
    assert_eq!(
      children[0].child_focus_order.as_ref().unwrap(),
      &vec!["n2".to_string(), "n1".to_string()]
    );
    assert_eq!(
      windows[0].window.as_ref().unwrap().identity.process_name,
      "alpha"
    );
    assert!(windows[0].window.as_ref().unwrap().handle.is_some());
  }

  #[test]
  fn strip_ephemeral_clears_ids_and_handles() {
    let win_id = Uuid::from_u128(1);
    let ws_id = Uuid::from_u128(2);
    let mon_id = Uuid::from_u128(3);

    let monitor = ContainerDto::Monitor(MonitorDto {
      id: mon_id,
      parent_id: None,
      children: vec![ContainerDto::Workspace(WorkspaceDto {
        id: ws_id,
        name: "1".into(),
        display_name: None,
        parent_id: Some(mon_id),
        children: vec![ContainerDto::Window(sample_window(
          win_id, "alpha", Some(1.0),
        ))],
        child_focus_order: vec![win_id],
        has_focus: true,
        is_displayed: true,
        width: 800,
        height: 600,
        x: 0,
        y: 0,
        tiling_direction: TilingDirection::Vertical,
      })],
      child_focus_order: vec![ws_id],
      has_focus: true,
      width: 800,
      height: 600,
      x: 0,
      y: 0,
      dpi: 96,
      scale_factor: 1.0,
      handle: None,
      device_name: "M".into(),
      device_path: None,
      hardware_id: None,
      working_rect: Rect::from_xy(0, 0, 800, 600),
    });

    let ignored = SnapshotWindow::from_window_dto(&sample_window(
      Uuid::from_u128(9),
      "ignored",
      None,
    ));

    let mut snapshot = LayoutSnapshot::from_monitor_dtos(
      &[monitor],
      vec![ignored],
      true,
      vec![],
      None,
      "t".into(),
    );
    snapshot.strip_ephemeral();

    assert!(snapshot.monitors[0].id.is_none());
    assert!(snapshot.monitors[0].workspaces[0].id.is_none());
    assert!(snapshot.monitors[0].workspaces[0].root.id.is_none());
    let win = snapshot.monitors[0].workspaces[0].root.children.as_ref().unwrap()
      [0]
      .window
      .as_ref()
      .unwrap();
    assert!(win.id.is_none());
    assert!(win.handle.is_none());
    assert!(snapshot.ignored_windows[0].handle.is_none());
    // Durable identity kept.
    assert_eq!(win.identity.process_name, "alpha");
    assert!(win.identity.process_path.is_some());
  }

  #[test]
  fn persists_prev_state_for_minimized_windows() {
    use crate::FloatingStateConfig;

    let mut win = sample_window(Uuid::from_u128(7), "minapp", None);
    win.state = WindowState::Minimized;
    win.prev_state = Some(WindowState::Floating(FloatingStateConfig {
      centered: false,
      shown_on_top: false,
    }));

    let snap = SnapshotWindow::from_window_dto(&win);
    assert_eq!(snap.state, WindowState::Minimized);
    assert_eq!(
      snap.prev_state,
      Some(WindowState::Floating(FloatingStateConfig {
        centered: false,
        shown_on_top: false,
      }))
    );

    // Ignored / fresh windows may omit prev_state.
    let plain = SnapshotWindow::from_window_dto(&sample_window(
      Uuid::from_u128(8),
      "plain",
      Some(1.0),
    ));
    assert!(plain.prev_state.is_none());
  }

  #[test]
  fn unsupported_version_fails_validation() {
    let mut snap = LayoutSnapshot {
      version: LAYOUT_SNAPSHOT_VERSION + 99,
      captured_at: "t".into(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      monitors: vec![],
      ignored_windows: vec![],
    };
    assert!(validate_layout_snapshot_version(&snap).is_err());
    snap.version = LAYOUT_SNAPSHOT_VERSION;
    assert!(validate_layout_snapshot_version(&snap).is_ok());
  }

  #[test]
  fn unsupported_version_is_rejected_before_any_restore() {
    // Mirrors load_layout_snapshot's first gate: bad versions error with
    // no WM mutation. Invalid JSON is rejected earlier by
    // `read_layout_snapshot_file` in the wm package (serde_json parse).
    let snap = LayoutSnapshot {
      version: 0,
      captured_at: "t".into(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      monitors: vec![],
      ignored_windows: vec![],
    };
    let err = validate_layout_snapshot_version(&snap).unwrap_err();
    assert!(err.contains("Unsupported layout snapshot version"));
  }

  #[test]
  fn empty_workspace_included_in_monitor_plan() {
    let empty_root = SnapshotNode {
      local_id: "root".into(),
      kind: SnapshotNodeKind::Split,
      tiling_size: None,
      tiling_direction: Some(TilingDirection::Horizontal),
      children: Some(vec![]),
      child_focus_order: Some(vec![]),
      window: None,
      id: None,
    };
    let monitors = vec![
      SnapshotMonitor {
        hardware_id: Some("HW1".into()),
        device_path: None,
        device_name: "DISPLAY1".into(),
        bounds: SnapshotBounds { x: 0, y: 0, width: 1920, height: 1080 },
        focused_workspace_name: Some("empty-focus".into()),
        workspaces: vec![
          SnapshotWorkspace {
            name: "with-win".into(),
            tiling_direction: TilingDirection::Horizontal,
            child_focus_order: vec![],
            root: empty_root.clone(),
            id: None,
          },
        ],
        id: None,
      },
      SnapshotMonitor {
        hardware_id: Some("HW2".into()),
        device_path: None,
        device_name: "DISPLAY2".into(),
        bounds: SnapshotBounds { x: 1920, y: 0, width: 1920, height: 1080 },
        focused_workspace_name: Some("empty-focus".into()),
        workspaces: vec![
          SnapshotWorkspace {
            name: "empty-focus".into(),
            tiling_direction: TilingDirection::Horizontal,
            child_focus_order: vec![],
            root: empty_root,
            id: None,
          },
        ],
        id: None,
      },
    ];
    // Put a window only on monitor 0 so leaf-driven planning would miss monitor 1.
    let plan = snapshot_workspace_monitor_indices(&monitors);
    assert_eq!(plan.get("with-win").copied(), Some(0));
    assert_eq!(
      plan.get("empty-focus").copied(),
      Some(1),
      "empty workspace must map to its snapshot monitor"
    );
  }

  #[test]
  fn floating_placement_rect_round_trips_xywh() {
    let rect = Rect::from_xy(100, 200, 640, 480);
    let mut win = sample_window(Uuid::from_u128(42), "floaty", None);
    win.state = WindowState::Floating(crate::FloatingStateConfig {
      centered: false,
      shown_on_top: true,
    });
    win.prev_state = Some(WindowState::Tiling);
    win.floating_placement = rect.clone();
    let snap = SnapshotWindow::from_window_dto(&win);
    assert_eq!(snap.floating_placement.as_ref().map(|r| (r.x(), r.y(), r.width(), r.height())), Some((100, 200, 640, 480)));
    assert_eq!(snap.prev_state, Some(WindowState::Tiling));
    assert_eq!(
      snap.state,
      WindowState::Floating(crate::FloatingStateConfig {
        centered: false,
        shown_on_top: true,
      })
    );
  }


  #[test]
  fn format_rfc3339_known_instant() {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let s = format_system_time_rfc3339(t);
    assert_eq!(s, "2023-11-14T22:13:20Z");
  }


  /// Fields a successful restore is expected to bring back. Intentionally
  /// excludes ephemeral ids/handles and `captured_at` (changes every save).
  #[derive(Clone, Debug, PartialEq)]
  struct RestoredLeafView {
    process_name: String,
    workspace_name: String,
    order_index: usize,
    state: WindowState,
    prev_state: Option<WindowState>,
    floating_xywh: Option<(i32, i32, i32, i32)>,
    tiling_size: Option<f32>,
  }

  fn collect_restored_leaf_views(snapshot: &LayoutSnapshot) -> Vec<RestoredLeafView> {
    let mut leaves = Vec::new();
    for monitor in &snapshot.monitors {
      for workspace in &monitor.workspaces {
        let mut order = 0usize;
        walk_node(&workspace.root, &workspace.name, &mut order, &mut leaves);
      }
    }
    leaves.sort_by(|a, b| a.process_name.cmp(&b.process_name));
    leaves
  }

  fn walk_node(
    node: &SnapshotNode,
    workspace_name: &str,
    order: &mut usize,
    out: &mut Vec<RestoredLeafView>,
  ) {
    match node.kind {
      SnapshotNodeKind::Window => {
        if let Some(window) = &node.window {
          let floating_xywh = window
            .floating_placement
            .as_ref()
            .map(|r| (r.x(), r.y(), r.width(), r.height()));
          out.push(RestoredLeafView {
            process_name: window.identity.process_name.clone(),
            workspace_name: workspace_name.to_string(),
            order_index: *order,
            state: window.state.clone(),
            prev_state: window.prev_state.clone(),
            floating_xywh,
            tiling_size: node.tiling_size,
          });
          *order += 1;
        }
      }
      SnapshotNodeKind::Split => {
        if let Some(children) = &node.children {
          for child in children {
            walk_node(child, workspace_name, order, out);
          }
        }
      }
    }
  }

  /// Pure data-level restore: copy restored fields from target onto scrambled
  /// live leaves matched by process_name. Covers the field set
  /// `load_layout_snapshot` must preserve without needing a live WM session.
  fn virtual_restore_leaves(
    target: &LayoutSnapshot,
    scrambled: &LayoutSnapshot,
  ) -> Vec<RestoredLeafView> {
    let mut by_name: HashMap<String, RestoredLeafView> =
      collect_restored_leaf_views(target)
        .into_iter()
        .map(|l| (l.process_name.clone(), l))
        .collect();
    let mut restored = Vec::new();
    for live in collect_restored_leaf_views(scrambled) {
      if let Some(wanted) = by_name.remove(&live.process_name) {
        restored.push(wanted);
      }
    }
    restored.sort_by(|a, b| a.process_name.cmp(&b.process_name));
    restored
  }

  fn make_window(
    id: u128,
    name: &str,
    state: WindowState,
    prev_state: Option<WindowState>,
    tiling_size: Option<f32>,
    float_xywh: Option<(i32, i32, i32, i32)>,
  ) -> WindowDto {
    let mut w = sample_window(Uuid::from_u128(id), name, tiling_size);
    w.state = state;
    w.prev_state = prev_state;
    if let Some((x, y, width, height)) = float_xywh {
      w.floating_placement = Rect::from_xy(x, y, width, height);
    }
    w
  }

  fn snapshot_from_workspaces(
    workspaces: Vec<(Uuid, &str, Uuid, Vec<ContainerDto>)>,
    monitors: Vec<(Uuid, &str, Vec<Uuid>)>,
    captured_at: &str,
  ) -> LayoutSnapshot {
    use crate::MonitorDto;

    let monitor_dtos: Vec<ContainerDto> = monitors
      .into_iter()
      .map(|(mon_id, device, ws_ids)| {
        let children: Vec<ContainerDto> = ws_ids
          .iter()
          .filter_map(|ws_id| {
            workspaces.iter().find(|(id, _, parent, _)| id == ws_id && parent == &mon_id)
              .map(|(id, name, parent, children)| {
                ContainerDto::Workspace(WorkspaceDto {
                  id: *id,
                  name: (*name).into(),
                  display_name: None,
                  parent_id: Some(*parent),
                  children: children.clone(),
                  child_focus_order: vec![],
                  has_focus: true,
                  is_displayed: true,
                  width: 800,
                  height: 600,
                  x: 0,
                  y: 0,
                  tiling_direction: TilingDirection::Horizontal,
                })
              })
          })
          .collect();
        ContainerDto::Monitor(MonitorDto {
          id: mon_id,
          parent_id: None,
          children,
          child_focus_order: ws_ids,
          has_focus: true,
          width: 1920,
          height: 1080,
          x: 0,
          y: 0,
          dpi: 96,
          scale_factor: 1.0,
          handle: Some(1),
          device_name: device.into(),
          device_path: None,
          hardware_id: None,
          working_rect: Rect::from_xy(0, 0, 1920, 1040),
        })
      })
      .collect();

    LayoutSnapshot::from_monitor_dtos(
      &monitor_dtos,
      vec![],
      false,
      vec![],
      Some("test".into()),
      captured_at.into(),
    )
    .into_durable()
  }

  #[test]
  fn restore_round_trip_after_scramble_virtual() {
    use crate::{FloatingStateConfig, FullscreenStateConfig, SplitContainerDto};

    let mon_a = Uuid::from_u128(10);
    let mon_b = Uuid::from_u128(11);
    let ws1 = Uuid::from_u128(20);
    let ws2 = Uuid::from_u128(21);
    let split_id = Uuid::from_u128(30);

    let tiler = make_window(1, "tiler", WindowState::Tiling, None, Some(0.6), None);
    let tiler_b = make_window(5, "tiler_b", WindowState::Tiling, None, Some(0.4), None);
    let floater = make_window(
      2,
      "floater",
      WindowState::Floating(FloatingStateConfig {
        centered: false,
        shown_on_top: true,
      }),
      None,
      None,
      Some((40, 50, 640, 480)),
    );
    let fuller = make_window(
      3,
      "fuller",
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        shown_on_top: false,
      }),
      Some(WindowState::Tiling),
      None,
      None,
    );
    let miner = make_window(
      4,
      "miner",
      WindowState::Minimized,
      Some(WindowState::Floating(FloatingStateConfig {
        centered: true,
        shown_on_top: false,
      })),
      None,
      None,
    );

    let snapshot_a = snapshot_from_workspaces(
      vec![
        (
          ws1,
          "1",
          mon_a,
          vec![
            ContainerDto::Split(SplitContainerDto {
              id: split_id,
              parent_id: Some(ws1),
              children: vec![
                ContainerDto::Window(tiler.clone()),
                ContainerDto::Window(tiler_b.clone()),
              ],
              child_focus_order: vec![tiler.id, tiler_b.id],
              has_focus: true,
              tiling_size: 1.0,
              width: 800,
              height: 600,
              x: 0,
              y: 0,
              tiling_direction: TilingDirection::Horizontal,
            }),
            ContainerDto::Window(miner.clone()),
          ],
        ),
        (
          ws2,
          "2",
          mon_b,
          vec![
            ContainerDto::Window(floater.clone()),
            ContainerDto::Window(fuller.clone()),
          ],
        ),
      ],
      vec![(mon_a, "DISPLAY1", vec![ws1]), (mon_b, "DISPLAY2", vec![ws2])],
      "2026-09-24T11:00:00Z",
    );

    // Scramble: move windows across workspaces/monitors, reverse tiling
    // order/sizes, flip tiling<->floating, change floating XYWH, clear
    // fullscreen/minimized (+ prev_state).
    let tiler_s = make_window(
      1,
      "tiler",
      WindowState::Floating(FloatingStateConfig {
        centered: true,
        shown_on_top: false,
      }),
      None,
      None,
      Some((1, 2, 10, 10)),
    );
    let tiler_b_s = make_window(5, "tiler_b", WindowState::Tiling, None, Some(0.75), None);
    let floater_s = make_window(2, "floater", WindowState::Tiling, None, Some(0.25), None);
    let fuller_s = make_window(3, "fuller", WindowState::Tiling, None, None, None);
    let miner_s = make_window(4, "miner", WindowState::Tiling, None, None, None);

    let scrambled = snapshot_from_workspaces(
      vec![(
        ws2,
        "2",
        mon_a,
        vec![
          ContainerDto::Split(SplitContainerDto {
            id: split_id,
            parent_id: Some(ws2),
            children: vec![
              ContainerDto::Window(tiler_b_s.clone()),
              ContainerDto::Window(floater_s.clone()),
            ],
            child_focus_order: vec![floater_s.id, tiler_b_s.id],
            has_focus: true,
            tiling_size: 1.0,
            width: 800,
            height: 600,
            x: 0,
            y: 0,
            tiling_direction: TilingDirection::Horizontal,
          }),
          ContainerDto::Window(tiler_s.clone()),
          ContainerDto::Window(fuller_s.clone()),
          ContainerDto::Window(miner_s.clone()),
        ],
      )],
      vec![(mon_a, "DISPLAY1", vec![ws2]), (mon_b, "DISPLAY2", vec![])],
      "2026-09-24T12:00:00Z",
    );

    let leaves_a = collect_restored_leaf_views(&snapshot_a);
    assert!(
      leaves_a.len() >= 5,
      "fixture should include tiling/floating/fullscreen/minimized/sibling"
    );
    let leaves_scrambled = collect_restored_leaf_views(&scrambled);
    assert_ne!(leaves_a, leaves_scrambled);

    let restored = virtual_restore_leaves(&snapshot_a, &scrambled);
    assert_eq!(
      restored, leaves_a,
      "virtual restore must recover workspace, order, state, prev_state, \
       floating WxH, and tiling sizes from snapshot A"
    );

    assert!(leaves_a.iter().any(|l| {
      l.workspace_name == "1"
        && matches!(l.state, WindowState::Minimized)
        && l.prev_state.is_some()
    }));
    assert!(leaves_a.iter().any(|l| {
      matches!(l.state, WindowState::Floating(_))
        && l.floating_xywh == Some((40, 50, 640, 480))
    }));
    assert!(leaves_a.iter().any(|l| {
      matches!(l.state, WindowState::Fullscreen(_))
        && l
          .prev_state
          .as_ref()
          .is_some_and(|s| matches!(s, WindowState::Tiling))
    }));
    assert!(leaves_a.iter().any(|l| l.tiling_size == Some(0.6)));
    assert!(leaves_a
      .iter()
      .any(|l| l.process_name == "tiler" && l.order_index == 0));
  }

  #[test]
  fn json_round_trip_preserves_nested_split_tiling_directions() {
    let nested = SnapshotNode {
      local_id: "root".into(),
      kind: SnapshotNodeKind::Split,
      tiling_size: None,
      tiling_direction: Some(TilingDirection::Vertical),
      children: Some(vec![
        SnapshotNode {
          local_id: "n0".into(),
          kind: SnapshotNodeKind::Window,
          tiling_size: Some(0.4),
          tiling_direction: None,
          children: None,
          child_focus_order: None,
          window: Some(SnapshotWindow {
            identity: SnapshotWindowIdentity {
              process_path: Some("C:\\Apps\\top.exe".into()),
              process_name: "top".into(),
              class_name: Some("TopClass".into()),
              title_hint: Some("top".into()),
            },
            state: WindowState::Tiling,
            prev_state: None,
            floating_placement: None,
            id: None,
            handle: None,
          }),
          id: None,
        },
        SnapshotNode {
          local_id: "n1".into(),
          kind: SnapshotNodeKind::Split,
          tiling_size: Some(0.6),
          tiling_direction: Some(TilingDirection::Horizontal),
          children: Some(vec![
            SnapshotNode {
              local_id: "n2".into(),
              kind: SnapshotNodeKind::Window,
              tiling_size: Some(0.5),
              tiling_direction: None,
              children: None,
              child_focus_order: None,
              window: Some(SnapshotWindow {
                identity: SnapshotWindowIdentity {
                  process_path: Some("C:\\Apps\\left.exe".into()),
                  process_name: "left".into(),
                  class_name: Some("LeftClass".into()),
                  title_hint: Some("left".into()),
                },
                state: WindowState::Tiling,
                prev_state: None,
                floating_placement: None,
                id: None,
                handle: None,
              }),
              id: None,
            },
            SnapshotNode {
              local_id: "n3".into(),
              kind: SnapshotNodeKind::Window,
              tiling_size: Some(0.5),
              tiling_direction: None,
              children: None,
              child_focus_order: None,
              window: Some(SnapshotWindow {
                identity: SnapshotWindowIdentity {
                  process_path: Some("C:\\Apps\\right.exe".into()),
                  process_name: "right".into(),
                  class_name: Some("RightClass".into()),
                  title_hint: Some("right".into()),
                },
                state: WindowState::Tiling,
                prev_state: None,
                floating_placement: None,
                id: None,
                handle: None,
              }),
              id: None,
            },
          ]),
          child_focus_order: Some(vec!["n2".into(), "n3".into()]),
          window: None,
          id: None,
        },
      ]),
      child_focus_order: None,
      window: None,
      id: None,
    };

    let snapshot = LayoutSnapshot {
      version: LAYOUT_SNAPSHOT_VERSION,
      captured_at: "2026-09-24T00:00:00Z".into(),
      glazewm_version: Some("test".into()),
      paused: false,
      binding_modes: vec![],
      monitors: vec![SnapshotMonitor {
        hardware_id: None,
        device_path: None,
        device_name: "DISPLAY1".into(),
        bounds: SnapshotBounds {
          x: 0,
          y: 0,
          width: 1920,
          height: 1080,
        },
        focused_workspace_name: Some("1".into()),
        workspaces: vec![SnapshotWorkspace {
          name: "1".into(),
          tiling_direction: TilingDirection::Vertical,
          child_focus_order: vec!["n0".into(), "n1".into()],
          root: nested,
          id: None,
        }],
        id: None,
      }],
      ignored_windows: vec![],
    };

    let json = serde_json::to_string_pretty(&snapshot).expect("serialize");
    assert!(
      json.contains("\"tilingDirection\": \"vertical\""),
      "workspace/root vertical must be persisted: {json}"
    );
    assert!(
      json.contains("\"tilingDirection\": \"horizontal\""),
      "nested horizontal must be persisted: {json}"
    );

    let loaded: LayoutSnapshot =
      serde_json::from_str(&json).expect("deserialize");
    let ws = &loaded.monitors[0].workspaces[0];
    assert_eq!(ws.tiling_direction, TilingDirection::Vertical);
    let children = ws.root.children.as_ref().expect("root children");
    assert_eq!(children[1].tiling_direction, Some(TilingDirection::Horizontal));

    // Plan after JSON load must still carry directions for attach.
    let matched: std::collections::HashSet<String> =
      ["n0", "n2", "n3"].into_iter().map(str::to_string).collect();
    let plan = crate::plan_workspace_tiling_layout(ws, &matched);
    assert_eq!(plan.tiling_direction, TilingDirection::Vertical);
    match &plan.children[1] {
      crate::LayoutPlanNode::Split { tiling_direction, .. } => {
        assert_eq!(*tiling_direction, TilingDirection::Horizontal);
      }
      other => panic!("expected split after JSON round-trip, got {other:?}"),
    }
  }

  #[test]
  fn snapshot_leaves_retain_workspace_names_across_monitors() {
    // Windows under different workspace names must keep that membership in
    // the durable JSON tree (load keys off workspace_name on each leaf).
    let win = |local_id: &str, name: &str| SnapshotNode {
      local_id: local_id.into(),
      kind: SnapshotNodeKind::Window,
      tiling_size: Some(1.0),
      tiling_direction: None,
      children: None,
      child_focus_order: None,
      window: Some(SnapshotWindow {
        identity: SnapshotWindowIdentity {
          process_path: Some(format!("C:\\\\Apps\\\\{name}.exe")),
          process_name: name.into(),
          class_name: Some(format!("{name}Class")),
          title_hint: Some(name.into()),
        },
        state: WindowState::Tiling,
        prev_state: None,
        floating_placement: None,
        id: None,
        handle: None,
      }),
      id: None,
    };

    let ws = |name: &str, child: SnapshotNode, dir: TilingDirection| SnapshotWorkspace {
      name: name.into(),
      tiling_direction: dir.clone(),
      child_focus_order: vec![child.local_id.clone()],
      root: SnapshotNode {
        local_id: "root".into(),
        kind: SnapshotNodeKind::Split,
        tiling_size: None,
        tiling_direction: Some(dir),
        children: Some(vec![child]),
        child_focus_order: None,
        window: None,
        id: None,
      },
      id: None,
    };

    let snapshot = LayoutSnapshot {
      version: LAYOUT_SNAPSHOT_VERSION,
      captured_at: "t".into(),
      glazewm_version: None,
      paused: false,
      binding_modes: vec![],
      monitors: vec![
        SnapshotMonitor {
          hardware_id: Some("HW1".into()),
          device_path: None,
          device_name: "DISPLAY1".into(),
          bounds: SnapshotBounds { x: 0, y: 0, width: 1920, height: 1080 },
          focused_workspace_name: Some("1".into()),
          workspaces: vec![ws("1", win("n0", "editor"), TilingDirection::Horizontal)],
          id: None,
        },
        SnapshotMonitor {
          hardware_id: Some("HW2".into()),
          device_path: None,
          device_name: "DISPLAY2".into(),
          bounds: SnapshotBounds { x: 1920, y: 0, width: 1920, height: 1080 },
          focused_workspace_name: Some("2".into()),
          workspaces: vec![ws("2", win("n0", "browser"), TilingDirection::Vertical)],
          id: None,
        },
      ],
      ignored_windows: vec![],
    };

    let json = serde_json::to_string(&snapshot).unwrap();
    let loaded: LayoutSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(loaded.monitors[0].workspaces[0].name, "1");
    assert_eq!(loaded.monitors[1].workspaces[0].name, "2");
    let p0 = loaded.monitors[0].workspaces[0].root.children.as_ref().unwrap()[0]
      .window.as_ref().unwrap().identity.process_name.clone();
    let p1 = loaded.monitors[1].workspaces[0].root.children.as_ref().unwrap()[0]
      .window.as_ref().unwrap().identity.process_name.clone();
    assert_eq!(p0, "editor");
    assert_eq!(p1, "browser");

    let plan1 = crate::plan_workspace_tiling_layout(
      &loaded.monitors[0].workspaces[0],
      &["n0".to_string()].into_iter().collect(),
    );
    let plan2 = crate::plan_workspace_tiling_layout(
      &loaded.monitors[1].workspaces[0],
      &["n0".to_string()].into_iter().collect(),
    );
    assert_eq!(plan1.workspace_name, "1");
    assert_eq!(plan2.workspace_name, "2");
    assert_eq!(plan1.tiling_direction, TilingDirection::Horizontal);
    assert_eq!(plan2.tiling_direction, TilingDirection::Vertical);
  }
}
