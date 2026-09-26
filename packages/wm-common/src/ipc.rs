use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
  BindingModeConfig, ContainerDto, LayoutMatchReport, LayoutSnapshot,
  TilingDirection, WmEvent,
};

pub const DEFAULT_IPC_PORT: u32 = 6123;

/// File name written beside the user config when the WM falls back off the
/// preferred IPC port (ghost socket / AddrInUse). CLI and `ipc_port()` read it.
pub const IPC_PORT_FILE_NAME: &str = "ipc.port";

fn glazewm_config_dir() -> Option<std::path::PathBuf> {
  #[cfg(target_os = "windows")]
  {
    std::env::var_os("USERPROFILE").map(|h| {
      std::path::PathBuf::from(h).join(".glzr").join("glazewm")
    })
  }
  #[cfg(not(target_os = "windows"))]
  {
    std::env::var_os("HOME").map(|h| {
      std::path::PathBuf::from(h).join(".glzr").join("glazewm")
    })
  }
}

/// `~/.glzr/glazewm/ipc.port` when a home directory is available.
#[must_use]
pub fn ipc_port_file_path() -> Option<std::path::PathBuf> {
  glazewm_config_dir().map(|d| d.join(IPC_PORT_FILE_NAME))
}

/// Port the WM should try first when binding the IPC listener.
///
/// Order: `GLAZEWM_IPC_PORT` env, else [`DEFAULT_IPC_PORT`].
/// Does **not** read `ipc.port` - that file is for clients discovering a
/// fallback bind. Re-reading it as the server preferred port permanently
/// stuck the WM on e.g. 6125 after a ghost on 6123, while Zebar/`WmClient`
/// still defaulted to 6123.
#[must_use]
pub fn preferred_bind_port() -> u32 {
  if let Ok(s) = std::env::var("GLAZEWM_IPC_PORT") {
    if let Ok(p) = s.parse::<u32>() {
      if p > 0 {
        return p;
      }
    }
  }
  DEFAULT_IPC_PORT
}

/// Resolve IPC port for **clients**: `GLAZEWM_IPC_PORT` env, else `ipc.port` file, else default.
#[must_use]
pub fn ipc_port() -> u32 {
  if let Ok(s) = std::env::var("GLAZEWM_IPC_PORT") {
    if let Ok(p) = s.parse::<u32>() {
      if p > 0 {
        return p;
      }
    }
  }
  if let Some(path) = ipc_port_file_path() {
    if let Ok(s) = std::fs::read_to_string(&path) {
      if let Ok(p) = s.trim().parse::<u32>() {
        if p > 0 {
          return p;
        }
      }
    }
  }
  DEFAULT_IPC_PORT
}

/// Persist the active IPC port so CLI clients find a fallback bind.
pub fn write_ipc_port_file(port: u32) -> std::io::Result<()> {
  let path = ipc_port_file_path().ok_or_else(|| {
    std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory")
  })?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  std::fs::write(&path, format!("{port}\n"))
}

/// Remove a stale fallback port file (call after binding the default port).
pub fn clear_ipc_port_file() {
  if let Some(path) = ipc_port_file_path() {
    let _ = std::fs::remove_file(path);
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "messageType", rename_all = "snake_case")]
pub enum ServerMessage {
  ClientResponse(ClientResponseMessage),
  EventSubscription(EventSubscriptionMessage),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientResponseMessage {
  pub client_message: String,
  pub data: Option<ClientResponseData>,
  pub error: Option<String>,
  pub success: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ClientResponseData {
  AppMetadata(AppMetadataData),
  // Layout before BindingModes: both have indingModes; empty array would
  // otherwise deserialize as BindingModes and drop the snapshot.
  Layout(LayoutSnapshot),
  LayoutMatch(LayoutMatchReport),
  LoadLayout(LoadLayoutData),
  BindingModes(BindingModesData),
  Command(CommandData),
  EventSubscribe(EventSubscribeData),
  EventUnsubscribe,
  Focused(FocusedData),
  Monitors(MonitorsData),
  TilingDirection(TilingDirectionData),
  Windows(WindowsData),
  Workspaces(WorkspacesData),
  Paused(bool),
  Ignored(crate::IgnoredWindowsData),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMetadataData {
  pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingModesData {
  pub binding_modes: Vec<BindingModeConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandData {
  pub subject_container_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscribeData {
  pub subscription_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusedData {
  pub focused: ContainerDto,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorsData {
  pub monitors: Vec<ContainerDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TilingDirectionData {
  pub tiling_direction: TilingDirection,
  pub direction_container: ContainerDto,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowsData {
  pub windows: Vec<ContainerDto>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacesData {
  pub workspaces: Vec<ContainerDto>,
}

/// Summary returned after `load-layout` / `command load-layout`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadLayoutData {
  pub matched: usize,
  pub unmatched_snapshot: usize,
  pub unmatched_live: usize,
  pub workspace_moves: usize,
  pub state_updates: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSubscriptionMessage {
  pub data: Option<WmEvent>,
  pub error: Option<String>,
  pub subscription_id: Uuid,
  pub success: bool,
}
