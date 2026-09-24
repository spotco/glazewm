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
}
