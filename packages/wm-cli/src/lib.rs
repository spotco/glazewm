#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use wm_common::{AppCommand, ClientResponseData, QueryCommand};
use wm_ipc_client::IpcClient;

pub async fn start(args: Vec<String>) -> anyhow::Result<()> {
  let app_command = AppCommand::parse_from(&args);

  let layout_output_path: Option<PathBuf> = match &app_command {
    AppCommand::Query {
      command: QueryCommand::Layout { output },
    } => output.clone(),
    AppCommand::SaveLayout { path } => Some(path.clone()),
    _ => None,
  };

  let copy_to_clipboard = matches!(app_command, AppCommand::CopyLayout);

  // Materialize clipboard JSON to a temp file for load-layout --clipboard
  // (IPC server reads from a filesystem path).
  let clipboard_temp_path: Option<PathBuf> =
    if let AppCommand::LoadLayout {
      clipboard: true, ..
    } = &app_command
    {
      Some(write_clipboard_snapshot_temp()?)
    } else {
      None
    };

  // Reconstruct IPC message carefully for path-bearing commands.
  // Layout --output / save-layout / copy-layout write is CLI-local;
  // IPC only gets `query layout`.
  let message = match &app_command {
    AppCommand::Query {
      command: QueryCommand::Layout { .. },
    }
    | AppCommand::SaveLayout { .. }
    | AppCommand::CopyLayout => "query layout".to_string(),
    AppCommand::LoadLayout {
      path,
      clipboard: false,
    } => {
      let path = path
        .as_ref()
        .context("load-layout requires a path or --clipboard")?;
      format!("load-layout {}", quote_path(path))
    }
    AppCommand::LoadLayout {
      clipboard: true, ..
    } => {
      let path = clipboard_temp_path
        .as_ref()
        .context("internal: missing clipboard temp path")?;
      format!("load-layout {}", quote_path(path))
    }
    AppCommand::Query {
      command: QueryCommand::LayoutMatch { path },
    } => {
      format!("query layout-match {}", quote_path(path))
    }
    AppCommand::InspectLayout { path } => {
      format!("inspect-layout {}", quote_path(path))
    }
    AppCommand::Command {
      subject_container_id: None,
      command: wm_common::InvokeCommand::LoadLayout { path },
    } => {
      format!("command load-layout {}", quote_path(path))
    }
    AppCommand::Command {
      subject_container_id: Some(id),
      command: wm_common::InvokeCommand::LoadLayout { path },
    } => {
      format!("command --id {id} load-layout {}", quote_path(path))
    }
    _ => args[1..].join(" "),
  };

  let mut client = IpcClient::connect().await?;

  client
    .send(&message)
    .await
    .context("Failed to send command to IPC server.")?;

  let client_response = client
    .client_response(&message)
    .await
    .context("Failed to receive response from IPC server.")?;

  if let (Some(path), Some(ClientResponseData::Layout(snapshot))) =
    (&layout_output_path, &client_response.data)
  {
    let durable = snapshot.clone().into_durable();
    let json = serde_json::to_string_pretty(&durable)
      .context("Failed to serialize durable layout snapshot.")?;
    std::fs::write(path, json).with_context(|| {
      format!("Failed to write layout snapshot to {}.", path.display())
    })?;
  }

  if copy_to_clipboard {
    if let Some(ClientResponseData::Layout(snapshot)) =
      &client_response.data
    {
      let durable = snapshot.clone().into_durable();
      let json = serde_json::to_string_pretty(&durable)
        .context("Failed to serialize durable layout snapshot.")?;
      set_clipboard_text(&json)?;
      eprintln!(
        "Copied layout snapshot to clipboard ({} bytes).",
        json.len()
      );
    } else if !client_response.success {
      // Fall through to normal response printing / exit.
    } else {
      anyhow::bail!("copy-layout expected layout data in IPC response.");
    }
  }

  // Best-effort cleanup of clipboard temp file.
  if let Some(path) = &clipboard_temp_path {
    let _ = std::fs::remove_file(path);
  }

  match client_response.data {
    // For event subscriptions, omit the initial response message and
    // continuously output subsequent event messages.
    Some(ClientResponseData::EventSubscribe(data)) => loop {
      let event_subscription = client
        .event_subscription(&data.subscription_id)
        .await
        .context("Failed to receive response from IPC server.")?;

      println!("{}", serde_json::to_string(&event_subscription)?);
    },
    // For all other messages, output and exit when the first response
    // message is received.
    _ => {
      println!("{}", serde_json::to_string(&client_response)?);
    }
  }

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

fn write_clipboard_snapshot_temp() -> anyhow::Result<PathBuf> {
  let mut clipboard = arboard::Clipboard::new()
    .context("Failed to open system clipboard.")?;
  let text = clipboard
    .get_text()
    .context("Failed to read clipboard text.")?;
  let trimmed = text.trim();
  if !trimmed.starts_with('{') {
    anyhow::bail!(
      "Clipboard does not look like layout snapshot JSON (expected '{{')."
    );
  }
  // Validate parse early for clearer errors.
  let _: wm_common::LayoutSnapshot = serde_json::from_str(trimmed)
    .context("Clipboard JSON is not a valid layout snapshot.")?;

  let path = std::env::temp_dir().join(format!(
    "glazewm-clipboard-layout-{}.json",
    std::process::id()
  ));
  std::fs::write(&path, trimmed).with_context(|| {
    format!("Failed to write clipboard snapshot to {}.", path.display())
  })?;
  Ok(path)
}

/// Format path for IPC using quote-aware encoding (`wm_common::quote_ipc_arg`).
/// The server tokenizes with `split_ipc_args`, so spaces in TEMP / user paths
/// round-trip for load-layout, inspect-layout, layout-match, and command load-layout.
fn quote_path(path: &std::path::Path) -> String {
  wm_common::quote_ipc_arg(&path.display().to_string())
}

#[cfg(test)]
mod tests {
  use super::quote_path;
  use std::path::Path;
  use wm_common::split_ipc_args;

  #[test]
  fn path_with_spaces_round_trips_for_all_path_bearing_commands() {
    let path = Path::new(r"C:\Users\mooto\AppData\Local\Temp\glazewm path test\snap.json");
    let quoted = quote_path(path);
    let messages = [
      format!("load-layout {quoted}"),
      format!("inspect-layout {quoted}"),
      format!("query layout-match {quoted}"),
      format!("command load-layout {quoted}"),
    ];
    let expected = path.display().to_string();
    for msg in messages {
      let tokens = split_ipc_args(&msg).expect("tokenize");
      assert_eq!(tokens.last().map(String::as_str), Some(expected.as_str()), "{msg}");
    }
  }
}
