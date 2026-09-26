use std::path::PathBuf;

use clap::{error::KindFormatter, Args, Parser, ValueEnum};
use serde::{Deserialize, Deserializer, Serialize};
use tracing::Level;
use uuid::Uuid;
use wm_platform::{Delta, Direction, LengthValue, OpacityValue};

use crate::TilingDirection;

const VERSION: &str = env!("VERSION_NUMBER");

#[derive(Clone, Debug, Parser)]
#[clap(name = "glazewm", author, version = VERSION, about, long_about = None)]
pub enum AppCommand {
  /// Starts the window manager.
  Start {
    /// Custom path to user config file.
    ///
    /// The default path is `%userprofile%/.glzr/glazewm/config.yaml`
    #[clap(short = 'c', long = "config", value_hint = clap::ValueHint::FilePath)]
    config_path: Option<PathBuf>,

    #[clap(flatten)]
    verbosity: Verbosity,
  },

  /// Retrieves and outputs a specific part of the window manager's state.
  ///
  /// Requires an already running instance of the window manager.
  #[clap(alias = "q")]
  Query {
    #[clap(subcommand)]
    command: QueryCommand,
  },

  /// Invokes a window manager command.
  ///
  /// Requires an already running instance of the window manager.
  #[clap(alias = "c")]
  Command {
    #[clap(long = "id")]
    subject_container_id: Option<Uuid>,

    #[clap(subcommand)]
    command: InvokeCommand,
  },

  /// Subscribes to one or more WM events (e.g. `window_close`), and
  /// continuously outputs the incoming events.
  ///
  /// Requires an already running instance of the window manager.
  Sub {
    /// WM event(s) to subscribe to.
    #[clap(short = 'e', long, value_enum, num_args = 1..)]
    events: Vec<SubscribableEvent>,
  },

  /// Unsubscribes from a prior event subscription.
  ///
  /// Requires an already running instance of the window manager.
  Unsub {
    /// Subscription ID to unsubscribe from.
    #[clap(long = "id")]
    subscription_id: Uuid,
  },

  /// Save a durable layout snapshot JSON to `path` (CLI-local write).
  ///
  /// Requires an already running instance of the window manager.
  /// Equivalent to `query layout -o <path>`.
  #[clap(name = "save-layout")]
  SaveLayout {
    /// Destination path for durable snapshot JSON.
    #[clap(value_hint = clap::ValueHint::FilePath)]
    path: PathBuf,
  },

  /// Load a layout snapshot JSON into the running WM (best-effort restore).
  ///
  /// Requires an already running instance of the window manager.
  /// Use `--clipboard` to load JSON previously copied via `copy-layout` / tray.
  #[clap(name = "load-layout")]
  LoadLayout {
    /// Path to a durable layout snapshot JSON file.
    #[clap(value_hint = clap::ValueHint::FilePath, required_unless_present = "clipboard")]
    path: Option<PathBuf>,

    /// Load durable snapshot JSON from the system clipboard instead of a file.
    #[clap(long = "clipboard", conflicts_with = "path")]
    clipboard: bool,
  },

  /// Dry-run match a layout snapshot against live windows (no mutation).
  ///
  /// Alias of `query layout-match`. Requires a running WM.
  #[clap(name = "inspect-layout")]
  InspectLayout {
    /// Path to a durable layout snapshot JSON file.
    #[clap(value_hint = clap::ValueHint::FilePath)]
    path: PathBuf,
  },

  /// Copy a durable layout snapshot JSON to the system clipboard (CLI-local).
  ///
  /// Requires an already running instance of the window manager.
  /// Same JSON as tray "Copy layout snapshot" / `save-layout`.
  #[clap(name = "copy-layout")]
  CopyLayout,
}

impl AppCommand {
  /// Parses `AppCommand` from command line arguments.
  ///
  /// Defaults to `AppCommand::Start` if no arguments are provided.
  #[must_use]
  pub fn parse_with_default(args: &Vec<String>) -> Self {
    if args.len() == 1 {
      AppCommand::Start {
        config_path: None,
        verbosity: Verbosity {
          verbose: false,
          quiet: false,
        },
      }
    } else {
      AppCommand::parse_from(args)
    }
  }
}

/// Verbosity flags to be used with `#[command(flatten)]`.
#[derive(Args, Clone, Debug)]
#[clap(about = None, long_about = None)]
pub struct Verbosity {
  /// Enables verbose logging.
  #[clap(short = 'v', long, action)]
  verbose: bool,

  /// Disables logging.
  #[clap(short = 'q', long, action, conflicts_with = "verbose")]
  quiet: bool,
}

impl Verbosity {
  /// Gets the log level based on the verbosity flags.
  #[must_use]
  pub fn level(&self) -> Level {
    match (self.verbose, self.quiet) {
      (true, _) => Level::DEBUG,
      (_, true) => Level::ERROR,
      _ => Level::INFO,
    }
  }
}

#[derive(Clone, Debug, Parser)]
pub enum QueryCommand {
  /// Outputs metadata about the application (e.g. version number).
  AppMetadata,
  /// Outputs the active binding modes.
  BindingModes,
  /// Outputs the focused container (either a window or an empty
  /// workspace).
  Focused,
  /// Outputs the tiling direction of the focused container.
  TilingDirection,
  /// Outputs all monitors.
  Monitors,
  /// Outputs all windows.
  Windows,
  /// Outputs all active workspaces.
  Workspaces,
  /// Outputs whether the window manager is paused.
  Paused,
  /// Outputs a persistence-oriented layout snapshot (monitors tree,
  /// ignored windows, paused, binding modes, version).
  Layout {
    /// Optional path to write durable (ephemeral-stripped) snapshot JSON.
    /// Handled by `glazewm-cli`; the IPC server ignores this flag.
    #[clap(short = 'o', long = "output", value_hint = clap::ValueHint::FilePath)]
    output: Option<PathBuf>,
  },
  /// Outputs windows currently ignored by the WM.
  Ignored,

  /// Dry-run match a layout snapshot file against live managed windows.
  ///
  /// Returns matched pairs (with scores), unmatched snapshot/live windows,
  /// and optional planned workspace moves. Does not mutate WM state.
  #[clap(name = "layout-match")]
  LayoutMatch {
    /// Path to a durable layout snapshot JSON file.
    #[clap(value_hint = clap::ValueHint::FilePath)]
    path: PathBuf,
  },
}

#[derive(Clone, Debug, PartialEq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum SubscribableEvent {
  All,
  ApplicationExiting,
  BindingModesChanged,
  FocusChanged,
  FocusedContainerMoved,
  MonitorAdded,
  MonitorUpdated,
  MonitorRemoved,
  TilingDirectionChanged,
  UserConfigChanged,
  WindowManaged,
  WindowUnmanaged,
  WorkspaceActivated,
  WorkspaceDeactivated,
  WorkspaceUpdated,
  PauseChanged,
}

#[derive(Clone, Debug, Parser, PartialEq, Serialize)]
pub enum InvokeCommand {
  AdjustBorders(InvokeAdjustBordersCommand),
  Close,
  Focus(InvokeFocusCommand),
  Ignore,
  Move(InvokeMoveCommand),
  MoveWorkspace {
    #[clap(long)]
    direction: Direction,
  },
  Position(InvokePositionCommand),
  Resize(InvokeResizeCommand),
  UpdateWorkspaceConfig {
    #[clap(long, allow_hyphen_values = true)]
    workspace: Option<String>,
    #[clap(flatten)]
    new_config: InvokeUpdateWorkspaceConfig,
  },
  SetFloating {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    centered: Option<bool>,

    #[clap(long, allow_hyphen_values = true)]
    x_pos: Option<i32>,

    #[clap(long, allow_hyphen_values = true)]
    y_pos: Option<i32>,

    #[clap(long, allow_hyphen_values = true)]
    width: Option<LengthValue>,

    #[clap(long, allow_hyphen_values = true)]
    height: Option<LengthValue>,
  },
  SetFullscreen {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    maximized: Option<bool>,
  },
  SetMinimized,
  SetTiling,
  SetTitleBarVisibility {
    #[clap(required = true, value_enum)]
    visibility: TitleBarVisibility,
  },
  SetTransparency(SetTransparencyCommand),
  ShellExec {
    #[clap(long, action)]
    hide_window: bool,

    #[clap(required = true, trailing_var_arg = true)]
    command: Vec<String>,
  },
  // Reuse `InvokeResizeCommand` struct.
  Size(InvokeResizeCommand),
  ToggleFloating {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    centered: Option<bool>,
  },
  ToggleFullscreen {
    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    shown_on_top: Option<bool>,

    #[clap(long, default_missing_value = "true", require_equals = true, num_args = 0..=1)]
    maximized: Option<bool>,
  },
  ToggleMinimized,
  ToggleTiling,
  ToggleTilingDirection,
  SetTilingDirection {
    #[clap(required = true)]
    tiling_direction: TilingDirection,
  },
  WmCycleFocus {
    #[clap(long, default_value_t = false)]
    omit_floating: bool,

    #[clap(long, default_value_t = false)]
    omit_fullscreen: bool,

    #[clap(long, default_value_t = true)]
    omit_minimized: bool,

    #[clap(long, default_value_t = false)]
    omit_tiling: bool,
  },
  WmDisableBindingMode {
    #[clap(long)]
    name: String,
  },
  WmEnableBindingMode {
    #[clap(long)]
    name: String,
  },
  WmExit,
  /// Uncloak DWM-cloaked top-level windows not currently managed (Windows).
  WmUncloakNonTracked,
  WmRedraw,
  WmReloadConfig,
  WmTogglePause,

  /// Load a layout snapshot JSON (best-effort restore; no app launch).
  LoadLayout {
    /// Path to a durable layout snapshot JSON file.
    #[clap(value_hint = clap::ValueHint::FilePath)]
    path: PathBuf,
  },
}

impl<'de> Deserialize<'de> for InvokeCommand {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    // Clap expects an array of string slices where the first argument is
    // the binary name/path. When deserializing commands from the user
    // config, we therefore have to prepend an additional empty argument.
    let unparsed = String::deserialize(deserializer)?;
    // Quote-aware split so `load-layout "C:\path with spaces\a.json"` works
    // in config bindings and IPC `command load-layout ...`.
    let unparsed_split = crate::ipc_argv_from_message(&unparsed).map_err(|err| {
      serde::de::Error::custom(err.to_string())
    })?;

    InvokeCommand::try_parse_from(unparsed_split).map_err(|err| {
      // Format the error message and remove the "error: " prefix.
      let err_msg = err.apply::<KindFormatter>().to_string();
      serde::de::Error::custom(err_msg.trim_start_matches("error: "))
    })
  }
}

#[derive(Clone, Debug, PartialEq, Serialize, ValueEnum)]
#[clap(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TitleBarVisibility {
  Shown,
  Hidden,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeAdjustBordersCommand {
  #[clap(long, allow_hyphen_values = true)]
  pub top: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub right: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub bottom: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub left: Option<LengthValue>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = false)]
#[allow(clippy::struct_excessive_bools)]
pub struct InvokeFocusCommand {
  #[clap(long)]
  pub direction: Option<Direction>,

  #[clap(long)]
  pub container_id: Option<Uuid>,

  #[clap(long)]
  pub workspace_in_direction: Option<Direction>,

  #[clap(long)]
  pub workspace: Option<String>,

  #[clap(long)]
  pub monitor: Option<usize>,

  #[clap(long)]
  pub next_active_workspace: bool,

  #[clap(long)]
  pub prev_active_workspace: bool,

  #[clap(long)]
  pub next_workspace: bool,

  #[clap(long)]
  pub prev_workspace: bool,

  #[clap(long)]
  pub next_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub prev_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub recent_workspace: bool,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = false)]
#[allow(clippy::struct_excessive_bools)]
pub struct InvokeMoveCommand {
  /// Direction to move the window.
  #[clap(long)]
  pub direction: Option<Direction>,

  /// Move window to workspace in specified direction.
  #[clap(long)]
  pub workspace_in_direction: Option<Direction>,

  /// Name of workspace to move the window.
  #[clap(long)]
  pub workspace: Option<String>,

  #[clap(long)]
  pub next_active_workspace: bool,

  #[clap(long)]
  pub prev_active_workspace: bool,

  #[clap(long)]
  pub next_workspace: bool,

  #[clap(long)]
  pub prev_workspace: bool,

  #[clap(long)]
  pub next_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub prev_active_workspace_on_monitor: bool,

  #[clap(long)]
  pub recent_workspace: bool,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeResizeCommand {
  #[clap(long, allow_hyphen_values = true)]
  pub width: Option<LengthValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub height: Option<LengthValue>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct SetTransparencyCommand {
  #[clap(long)]
  pub opacity: Option<OpacityValue>,

  #[clap(long, allow_hyphen_values = true)]
  pub opacity_delta: Option<Delta<OpacityValue>>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokePositionCommand {
  #[clap(long, action)]
  pub centered: bool,

  #[clap(long, allow_hyphen_values = true)]
  pub x_pos: Option<i32>,

  #[clap(long, allow_hyphen_values = true)]
  pub y_pos: Option<i32>,
}

#[derive(Args, Clone, Debug, PartialEq, Serialize)]
#[group(required = true, multiple = true)]
pub struct InvokeUpdateWorkspaceConfig {
  #[clap(long, allow_hyphen_values = true)]
  pub name: Option<String>,

  #[clap(long, allow_hyphen_values = true)]
  pub display_name: Option<String>,

  #[clap(long)]
  pub bind_to_monitor: Option<u32>,

  #[clap(long)]
  pub keep_alive: Option<bool>,
}

#[cfg(test)]
mod shell_exec_ipc_tests {
  use super::*;
  use crate::{ipc_argv_from_message, join_ipc_args};

  fn parse_shell_exec(message: &str) -> Vec<String> {
    let argv = ipc_argv_from_message(message).expect("tokenize");
    match InvokeCommand::try_parse_from(argv).expect("parse") {
      InvokeCommand::ShellExec { command, .. } => command,
      other => panic!("expected ShellExec, got {other:?}"),
    }
  }

  #[test]
  fn invoke_shell_exec_preserves_powershell_file_path_with_spaces() {
    let msg = r#"shell-exec powershell -File "C:\My Scripts\foo.ps1""#;
    let command = parse_shell_exec(msg);
    assert_eq!(
      command,
      vec![
        "powershell".to_string(),
        "-File".to_string(),
        r"C:\My Scripts\foo.ps1".to_string(),
      ]
    );
    assert_eq!(
      join_ipc_args(&command),
      r#"powershell -File "C:\My Scripts\foo.ps1""#
    );
  }

  #[test]
  fn invoke_shell_exec_preserves_code_project_path() {
    let command = parse_shell_exec(r#"shell-exec code "C:\My Project""#);
    assert_eq!(join_ipc_args(&command), r#"code "C:\My Project""#);
  }

  #[test]
  fn invoke_shell_exec_multiple_quoted_args_and_empty() {
    let command = parse_shell_exec(r#"shell-exec tool "arg one" "" --flag"#);
    assert_eq!(
      command,
      vec![
        "tool".to_string(),
        "arg one".to_string(),
        String::new(),
        "--flag".to_string(),
      ]
    );
    assert_eq!(join_ipc_args(&command), r#"tool "arg one" "" --flag"#);
  }

  #[test]
  fn invoke_shell_exec_unc_path_round_trip() {
    let unc = r"\\server\share\My Folder\file.ps1";
    let msg = format!("shell-exec powershell -File {}", crate::quote_ipc_arg(unc));
    let command = parse_shell_exec(&msg);
    assert_eq!(command.last().map(String::as_str), Some(unc));
    let joined = join_ipc_args(&command);
    let again = parse_shell_exec(&format!("shell-exec {joined}"));
    assert_eq!(again, command);
  }
}
