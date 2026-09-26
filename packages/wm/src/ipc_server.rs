use std::net::SocketAddr;

use anyhow::{bail, Context};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::{
  net::TcpStream,
  sync::{broadcast, mpsc},
  task,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;
use wm_common::{
  AppCommand, AppMetadataData, BindingModesData, ClientResponseData,
  ClientResponseMessage, CommandData, EventSubscribeData,
  EventSubscriptionMessage, FocusedData, IgnoredWindowsData,
  LayoutSnapshot, LoadLayoutData, MonitorsData, QueryCommand,
  ServerMessage, SnapshotWindow, SnapshotWindowIdentity,
  SubscribableEvent, TilingDirectionData, WindowsData, WmEvent,
  WorkspacesData, format_system_time_rfc3339,
};
use wm_platform::Dispatcher;

use crate::{
  commands::general::{
    inspect_layout_snapshot, load_layout_snapshot,
    read_layout_snapshot_file,
  },
  traits::{CommonGetters, TilingDirectionGetters},
  user_config::UserConfig,
  wm::WindowManager,
};

pub struct IpcServer {
  abort_handle: task::AbortHandle,
  pub message_rx: mpsc::UnboundedReceiver<(
    String,
    mpsc::UnboundedSender<Message>,
    broadcast::Sender<()>,
  )>,
  _event_rx: broadcast::Receiver<(SubscribableEvent, WmEvent)>,
  event_tx: broadcast::Sender<(SubscribableEvent, WmEvent)>,
  _unsubscribe_rx: broadcast::Receiver<Uuid>,
  unsubscribe_tx: broadcast::Sender<Uuid>,
}

impl IpcServer {
  pub async fn start(dispatcher: &Dispatcher) -> anyhow::Result<Self> {
    let (message_tx, message_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (unsubscribe_tx, _unsubscribe_rx) = broadcast::channel(16);

    let (server, server_addr) =
      crate::ipc_conflict::bind_ipc_listener(dispatcher).await?;
    info!("IPC server started on: '{}'.", server_addr);

    let task = task::spawn(async move {
      while let Ok((stream, addr)) = server.accept().await {
        let message_tx = message_tx.clone();

        task::spawn(async move {
          if let Err(err) =
            Self::handle_connection(stream, addr, message_tx).await
          {
            warn!("Error handling connection: {}", err);
          }
        });
      }
    });

    Ok(Self {
      abort_handle: task.abort_handle(),
      #[allow(clippy::used_underscore_binding)]
      _event_rx,
      event_tx,
      message_rx,
      unsubscribe_tx,
      #[allow(clippy::used_underscore_binding)]
      _unsubscribe_rx,
    })
  }

  async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    message_tx: mpsc::UnboundedSender<(
      String,
      mpsc::UnboundedSender<Message>,
      broadcast::Sender<()>,
    )>,
  ) -> anyhow::Result<()> {
    info!("Incoming IPC connection from: {}.", addr);

    let ws_stream = accept_async(stream)
      .await
      .context("Error during websocket handshake.")?;

    let (mut outgoing, mut incoming) = ws_stream.split();
    let (response_tx, mut response_rx) = mpsc::unbounded_channel();
    let (disconnection_tx, _) = broadcast::channel(16);

    let res = async {
      loop {
        tokio::select! {
          Some(response) = response_rx.recv() => {
            outgoing.send(response).await?;
          }
          message = incoming.next() => {
            match message {
              Some(Ok(message)) => {
                if message.is_text() || message.is_binary() {
                  message_tx.send((
                    message.to_text()?.to_string(),
                    response_tx.clone(),
                    disconnection_tx.clone(),
                  ))?;
                }
              }
              Some(Err(err)) => bail!("WebSocket error: {}", err),
              None => {
                // WebSocket connection closed.
                break Ok(());
              },
            }
          }
        }
      }
    }
    .await;

    info!("IPC disconnection from: {}.", addr);

    if let Err(err) = disconnection_tx.send(()) {
      warn!("Failed to broadcast disconnection: {}", err);
    }

    res
  }

  pub fn process_message(
    &self,
    message: String,
    response_tx: &mpsc::UnboundedSender<Message>,
    disconnection_tx: &broadcast::Sender<()>,
    wm: &mut WindowManager,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // Quote-aware argv split so path-bearing commands survive spaces
    // (e.g. load-layout / inspect-layout / query layout-match).
    let argv = wm_common::ipc_argv_from_message(&message)?;
    let app_command = AppCommand::try_parse_from(argv);

    let response_data =
      app_command
        .map_err(anyhow::Error::msg)
        .and_then(|app_command| {
          self.handle_app_command(
            app_command,
            response_tx,
            disconnection_tx,
            wm,
            config,
          )
        });

    // Respond to the client with the result of the command.
    response_tx
      .send(Self::to_client_response_msg(message, response_data)?)
      .map_err(|err| {
        anyhow::anyhow!("Failed to send response: {}", err)
      })?;

    Ok(())
  }

  #[allow(clippy::too_many_lines)]
  fn handle_app_command(
    &self,
    app_command: AppCommand,
    response_tx: &mpsc::UnboundedSender<Message>,
    disconnection_tx: &broadcast::Sender<()>,
    wm: &mut WindowManager,
    config: &mut UserConfig,
  ) -> anyhow::Result<ClientResponseData> {
    let response_data = match app_command {
      AppCommand::Query { command } => match command {
        QueryCommand::Windows => {
          ClientResponseData::Windows(WindowsData {
            windows: wm
              .state
              .windows()
              .into_iter()
              .map(|window| window.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::Workspaces => {
          ClientResponseData::Workspaces(WorkspacesData {
            workspaces: wm
              .state
              .workspaces()
              .into_iter()
              .map(|workspace| workspace.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::Monitors => {
          ClientResponseData::Monitors(MonitorsData {
            monitors: wm
              .state
              .monitors()
              .into_iter()
              .map(|monitor| monitor.to_dto())
              .try_collect()?,
          })
        }
        QueryCommand::BindingModes => {
          ClientResponseData::BindingModes(BindingModesData {
            binding_modes: wm.state.binding_modes.clone(),
          })
        }
        QueryCommand::Focused => {
          let focused_container = wm
            .state
            .focused_container()
            .context("No focused container.")?;

          ClientResponseData::Focused(FocusedData {
            focused: focused_container.to_dto()?,
          })
        }
        QueryCommand::AppMetadata => {
          ClientResponseData::AppMetadata(AppMetadataData {
            version: env!("VERSION_NUMBER").to_string(),
          })
        }
        QueryCommand::TilingDirection => {
          let direction_container = wm
            .state
            .focused_container()
            .and_then(|focused| focused.direction_container())
            .context("No direction container.")?;

          ClientResponseData::TilingDirection(TilingDirectionData {
            direction_container: direction_container.to_dto()?,
            tiling_direction: direction_container.tiling_direction(),
          })
        }
        QueryCommand::Paused => {
          ClientResponseData::Paused(wm.state.is_paused)
        }
        QueryCommand::Layout { .. } => {
          let monitors: Vec<_> = wm
            .state
            .monitors()
            .into_iter()
            .map(|monitor| monitor.to_dto())
            .try_collect()?;

          let ignored_windows = wm
            .state
            .ignored_windows
            .iter()
            .map(snapshot_from_native_window)
            .collect();

          let binding_modes = wm
            .state
            .binding_modes
            .iter()
            .map(|mode| mode.name.clone())
            .collect();

          let snapshot = LayoutSnapshot::from_monitor_dtos(
            &monitors,
            ignored_windows,
            wm.state.is_paused,
            binding_modes,
            Some(env!("VERSION_NUMBER").to_string()),
            format_system_time_rfc3339(std::time::SystemTime::now()),
          );

          ClientResponseData::Layout(snapshot)
        }
        QueryCommand::Ignored => {
          ClientResponseData::Ignored(IgnoredWindowsData {
            windows: wm
              .state
              .ignored_windows
              .iter()
              .map(snapshot_from_native_window)
              .collect(),
          })
        }
        QueryCommand::LayoutMatch { path } => {
          let snapshot = read_layout_snapshot_file(&path)?;
          let report = inspect_layout_snapshot(&snapshot, &wm.state)?;
          ClientResponseData::LayoutMatch(report)
        }
      },
      AppCommand::LoadLayout { path, clipboard } => {
        if clipboard || path.is_none() {
          bail!("load-layout --clipboard is handled by glazewm-cli locally.");
        }
        let path = path.expect("path required without --clipboard");
        let snapshot = read_layout_snapshot_file(&path)?;
        let summary =
          load_layout_snapshot(&snapshot, &mut wm.state, config)?;
        if wm.state.pending_sync.has_changes() {
          crate::commands::general::platform_sync(&mut wm.state, config)?;
        }
        ClientResponseData::LoadLayout(LoadLayoutData {
          matched: summary.matched,
          unmatched_snapshot: summary.unmatched_snapshot,
          unmatched_live: summary.unmatched_live,
          workspace_moves: summary.workspace_moves,
          state_updates: summary.state_updates,
        })
      }
      AppCommand::Command {
        command: wm_common::InvokeCommand::LoadLayout { path },
        ..
      } => {
        let snapshot = read_layout_snapshot_file(&path)?;
        let summary =
          load_layout_snapshot(&snapshot, &mut wm.state, config)?;
        if wm.state.pending_sync.has_changes() {
          crate::commands::general::platform_sync(&mut wm.state, config)?;
        }
        ClientResponseData::LoadLayout(LoadLayoutData {
          matched: summary.matched,
          unmatched_snapshot: summary.unmatched_snapshot,
          unmatched_live: summary.unmatched_live,
          workspace_moves: summary.workspace_moves,
          state_updates: summary.state_updates,
        })
      }
      AppCommand::InspectLayout { path } => {
        let snapshot = read_layout_snapshot_file(&path)?;
        let report = inspect_layout_snapshot(&snapshot, &wm.state)?;
        ClientResponseData::LayoutMatch(report)
      }
      AppCommand::SaveLayout { .. } | AppCommand::CopyLayout => {
        // CLI handles durable write / clipboard after `query layout`.
        bail!("save-layout/copy-layout are handled by glazewm-cli locally.")
      }
      AppCommand::Command {
        subject_container_id,
        command,
      } => {
        let subject_container_id = wm.process_commands(
          &vec![command],
          subject_container_id,
          config,
        )?;

        ClientResponseData::Command(CommandData {
          subject_container_id,
        })
      }
      AppCommand::Sub { events } => {
        let subscription_id = Uuid::new_v4();
        info!("New event subscription {}: {:?}", subscription_id, events);

        let response_tx = response_tx.clone();
        let mut event_rx = self.event_tx.subscribe();
        let mut unsubscribe_rx = self.unsubscribe_tx.subscribe();
        let mut disconnection_rx = disconnection_tx.subscribe();

        task::spawn(async move {
          loop {
            tokio::select! {
              Ok(()) = disconnection_rx.recv() => {
                break;
              }
              Ok(id) = unsubscribe_rx.recv() => {
                if id == subscription_id {
                  break;
                }
              }
              Ok((event_type, event)) = event_rx.recv() => {
                // Check whether the event is one of the subscribed events.
                if events.contains(&event_type)
                  || events.contains(&SubscribableEvent::All)
                {
                  let send_result = Self::to_event_subscription_msg(
                    subscription_id,
                    event,
                  )
                  .and_then(|event_msg| {
                    response_tx
                      .send(event_msg)
                      .map_err(anyhow::Error::from)
                  });

                  if let Err(err) = send_result {
                    warn!("Error emitting WM event: {}", err);
                    break;
                  }
                }
              }
            }
          }
        });

        ClientResponseData::EventSubscribe(EventSubscribeData {
          subscription_id,
        })
      }
      AppCommand::Unsub { subscription_id } => {
        self
          .unsubscribe_tx
          .send(subscription_id)
          .context("Failed to unsubscribe from event.")?;

        ClientResponseData::EventUnsubscribe
      }
      AppCommand::Start { .. } => bail!("Unsupported IPC command."),
    };

    Ok(response_data)
  }

  fn to_client_response_msg(
    client_message: String,
    response_data: anyhow::Result<ClientResponseData>,
  ) -> anyhow::Result<Message> {
    let error = response_data.as_ref().err().map(ToString::to_string);
    let success = response_data.as_ref().is_ok();

    let message = ServerMessage::ClientResponse(ClientResponseMessage {
      client_message,
      data: response_data.ok(),
      error,
      success,
    });

    let message_json = serde_json::to_string(&message)?;
    Ok(Message::Text(message_json.into()))
  }

  fn to_event_subscription_msg(
    subscription_id: Uuid,
    event: WmEvent,
  ) -> anyhow::Result<Message> {
    let message =
      ServerMessage::EventSubscription(EventSubscriptionMessage {
        data: Some(event),
        error: None,
        subscription_id,
        success: true,
      });

    let message_json = serde_json::to_string(&message)?;
    Ok(Message::Text(message_json.into()))
  }

  pub fn process_event(&mut self, event: WmEvent) -> anyhow::Result<()> {
    let event_type = match event {
      WmEvent::ApplicationExiting => SubscribableEvent::ApplicationExiting,
      WmEvent::BindingModesChanged { .. } => {
        SubscribableEvent::BindingModesChanged
      }
      WmEvent::FocusChanged { .. } => SubscribableEvent::FocusChanged,
      WmEvent::FocusedContainerMoved { .. } => {
        SubscribableEvent::FocusedContainerMoved
      }
      WmEvent::MonitorAdded { .. } => SubscribableEvent::MonitorAdded,
      WmEvent::MonitorUpdated { .. } => SubscribableEvent::MonitorUpdated,
      WmEvent::MonitorRemoved { .. } => SubscribableEvent::MonitorRemoved,
      WmEvent::TilingDirectionChanged { .. } => {
        SubscribableEvent::TilingDirectionChanged
      }
      WmEvent::UserConfigChanged { .. } => {
        SubscribableEvent::UserConfigChanged
      }
      WmEvent::WindowManaged { .. } => SubscribableEvent::WindowManaged,
      WmEvent::WindowUnmanaged { .. } => {
        SubscribableEvent::WindowUnmanaged
      }
      WmEvent::WorkspaceActivated { .. } => {
        SubscribableEvent::WorkspaceActivated
      }
      WmEvent::WorkspaceDeactivated { .. } => {
        SubscribableEvent::WorkspaceDeactivated
      }
      WmEvent::WorkspaceUpdated { .. } => {
        SubscribableEvent::WorkspaceUpdated
      }
      WmEvent::PauseChanged { .. } => SubscribableEvent::PauseChanged,
    };

    self
      .event_tx
      .send((event_type, event))
      .map_err(|err| anyhow::anyhow!("Failed to send event: {}", err))?;

    Ok(())
  }

  pub fn stop(&self) {
    info!("Shutting down IPC server.");
    self.abort_handle.abort();
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
    // Ignored windows are unmanaged; treat as floating for export.
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

impl Drop for IpcServer {
  fn drop(&mut self) {
    self.stop();
  }
}
