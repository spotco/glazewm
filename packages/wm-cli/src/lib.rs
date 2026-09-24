#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use wm_common::{AppCommand, ClientResponseData, QueryCommand};
use wm_ipc_client::IpcClient;

pub async fn start(args: Vec<String>) -> anyhow::Result<()> {
  let app_command = AppCommand::parse_from(&args);

  let output_path: Option<PathBuf> = match &app_command {
    AppCommand::Query {
      command: QueryCommand::Layout { output },
    } => output.clone(),
    _ => None,
  };

  // Layout --output is CLI-local; IPC server only receives `query layout`.
  let message = match &app_command {
    AppCommand::Query {
      command: QueryCommand::Layout { .. },
    } => "query layout".to_string(),
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
    (&output_path, &client_response.data)
  {
    let durable = snapshot.clone().into_durable();
    let json = serde_json::to_string_pretty(&durable)
      .context("Failed to serialize durable layout snapshot.")?;
    std::fs::write(path, json)
      .with_context(|| format!("Failed to write layout snapshot to {}.", path.display()))?;
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
