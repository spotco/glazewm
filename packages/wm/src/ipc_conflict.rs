//! Recovery when the GlazeWM IPC TCP port is already bound.
//!
//! On Windows, a crashed / half-dead `glazewm.exe` or `glazewm-watcher.exe`
//! (or a ghost socket whose owning PID is already gone) can leave
//! `127.0.0.1:<port>` in LISTENING state. Startup then fails with
//! `std::io::ErrorKind::AddrInUse` / WSAEADDRINUSE (10048).
//!
//! This module prompts the user (Kill existing vs Quit), logs to the
//! config-dir `layout.log`, best-effort kills other GlazeWM processes and
//! the listener PID from `netstat`, then retries the bind a limited number
//! of times.

use std::{
  io,
  process::Command,
  time::Duration,
};

use anyhow::{bail, Context};
use tokio::net::TcpListener;
use tracing::{info, warn};
use wm_common::ipc_port;
use wm_platform::Dispatcher;

use crate::commands::general::layout_debug_log;

/// Max times we ask the user to kill+retry after an AddrInUse failure.
const MAX_KILL_RETRY_ROUNDS: u32 = 2;

/// Windows WSAEADDRINUSE.
#[cfg(target_os = "windows")]
const WSAEADDRINUSE: i32 = 10048;

#[must_use]
pub fn is_addr_in_use(err: &io::Error) -> bool {
  if matches!(err.kind(), io::ErrorKind::AddrInUse) {
    return true;
  }
  #[cfg(target_os = "windows")]
  {
    if err.raw_os_error() == Some(WSAEADDRINUSE) {
      return true;
    }
  }
  // Some wrappers stringify the OS message without setting AddrInUse.
  let msg = err.to_string();
  msg.contains("10048")
    || msg.contains("address already in use")
    || msg.contains("Only one usage of each socket address")
}

/// Bind the IPC listener, offering Kill-existing vs Quit on AddrInUse.
pub async fn bind_ipc_listener(
  dispatcher: &Dispatcher,
) -> anyhow::Result<(TcpListener, String)> {
  let port = ipc_port();
  let server_addr = format!("127.0.0.1:{port}");
  let mut kill_rounds: u32 = 0;
  let mut last_detail = String::new();

  loop {
    match TcpListener::bind(&server_addr).await {
      Ok(listener) => {
        if kill_rounds > 0 {
          let msg = format!(
            "IPC bind succeeded on {server_addr} after {kill_rounds} kill+retry round(s)"
          );
          info!("{msg}");
          layout_debug_log(&msg);
        }
        return Ok((listener, server_addr));
      }
      Err(err) if is_addr_in_use(&err) => {
        let err_text = format!("{err}");
        let log_line = format!(
          "IPC bind AddrInUse on {server_addr} (round {kill_rounds}): {err_text}"
        );
        warn!("{log_line}");
        layout_debug_log(&log_line);

        if kill_rounds >= MAX_KILL_RETRY_ROUNDS {
          let msg = format!(
            "IPC port {port} still in use after {MAX_KILL_RETRY_ROUNDS} kill+retry attempt(s). \
             Last error: {err_text}. {last_detail} \
             Try setting GLAZEWM_IPC_PORT to a free port, or reboot if a ghost socket remains."
          );
          layout_debug_log(&msg);
          bail!("{msg}");
        }

        let ghost_hint = if last_detail.is_empty() {
          String::new()
        } else {
          format!("\n\nPrevious attempt notes:\n{last_detail}")
        };

        let prompt = format!(
          "GlazeWM could not bind the IPC socket on 127.0.0.1:{port}.\n\n\
           {err_text}\n\n\
           Another GlazeWM instance may still be running, or a ghost socket \
           may still own this port.{ghost_hint}\n\n\
           Yes — Kill existing GlazeWM / watcher processes and retry\n\
           No — Quit (do not start this instance)"
        );

        let kill = dispatcher.show_yes_no_dialog(
          "GlazeWM — IPC port in use",
          &prompt,
        );
        if !kill {
          let msg = format!(
            "IPC port {port} in use; user chose Quit (abort this instance)"
          );
          layout_debug_log(&msg);
          bail!("{msg}");
        }

        kill_rounds += 1;
        let report = kill_existing_glazewm_and_free_port(port);
        last_detail = report.clone();
        layout_debug_log(format!(
          "IPC kill+free-port round {kill_rounds}: {report}"
        ));
        info!("IPC kill+free-port round {kill_rounds}: {report}");

        // Give the OS a moment to release the socket after TerminateProcess.
        tokio::time::sleep(Duration::from_millis(750)).await;
      }
      Err(err) => {
        return Err(err).with_context(|| {
          format!("Failed to bind IPC listener on {server_addr}")
        });
      }
    }
  }
}

/// Best-effort: terminate other GlazeWM processes and the IPC listener PID.
#[must_use]
pub fn kill_existing_glazewm_and_free_port(port: u32) -> String {
  let self_pid = std::process::id();
  let mut notes: Vec<String> = Vec::new();

  #[cfg(target_os = "windows")]
  {
    let killed_named = kill_named_others(
      self_pid,
      &["glazewm.exe", "glazewm-watcher.exe"],
    );
    notes.push(killed_named);

    match listener_pids_on_port(port) {
      Ok(pids) if pids.is_empty() => {
        notes.push(format!(
          "netstat: no LISTENING PID found on 127.0.0.1:{port}"
        ));
      }
      Ok(pids) => {
        for pid in pids {
          if pid == self_pid {
            notes.push(format!(
              "netstat listener PID {pid} is this process; skipped"
            ));
            continue;
          }
          match taskkill_pid(pid) {
            Ok(true) => {
              notes.push(format!(
                "killed IPC listener PID {pid} on port {port}"
              ));
            }
            Ok(false) => {
              notes.push(format!(
                "IPC listener PID {pid} on port {port} not found \
                 (ghost/unkillable socket — try GLAZEWM_IPC_PORT or reboot)"
              ));
            }
            Err(err) => {
              notes.push(format!(
                "failed to kill IPC listener PID {pid} on port {port}: {err} \
                 (ghost/unkillable — try GLAZEWM_IPC_PORT or reboot)"
              ));
            }
          }
        }
      }
      Err(err) => {
        notes.push(format!("netstat lookup failed: {err:#}"));
      }
    }
  }

  #[cfg(not(target_os = "windows"))]
  {
    let _ = self_pid;
    let _ = port;
    notes.push(
      "non-Windows: best-effort pkill glazewm / glazewm-watcher".into(),
    );
    let _ = Command::new("pkill").args(["-x", "glazewm"]).status();
    let _ = Command::new("pkill")
      .args(["-x", "glazewm-watcher"])
      .status();
  }

  notes.join("; ")
}

#[cfg(target_os = "windows")]
fn kill_named_others(self_pid: u32, names: &[&str]) -> String {
  let mut parts: Vec<String> = Vec::new();
  for name in names {
    match pids_for_image_name(name) {
      Ok(pids) => {
        let others: Vec<u32> =
          pids.into_iter().filter(|p| *p != self_pid).collect();
        if others.is_empty() {
          parts.push(format!("{name}: none other than self"));
          continue;
        }
        for pid in others {
          match taskkill_pid(pid) {
            Ok(true) => parts.push(format!("killed {name} PID {pid}")),
            Ok(false) => parts.push(format!(
              "{name} PID {pid} already gone / unkillable"
            )),
            Err(err) => {
              parts.push(format!(
                "failed killing {name} PID {pid}: {err}"
              ));
            }
          }
        }
      }
      Err(err) => {
        parts.push(format!("tasklist for {name} failed: {err:#}"));
      }
    }
  }
  parts.join("; ")
}

#[cfg(target_os = "windows")]
fn pids_for_image_name(image_name: &str) -> anyhow::Result<Vec<u32>> {
  // CSV: "glazewm.exe","1234","Session Name","69","12,345 K"
  let output = Command::new("tasklist")
    .args([
      "/FI",
      &format!("IMAGENAME eq {image_name}"),
      "/FO",
      "CSV",
      "/NH",
    ])
    .output()
    .context("failed to run tasklist")?;

  let stdout = String::from_utf8_lossy(&output.stdout);
  let mut pids = Vec::new();
  for line in stdout.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with("INFO:") {
      continue;
    }
    // Parse second CSV field as PID.
    let fields: Vec<&str> = parse_csv_line(line);
    if fields.len() < 2 {
      continue;
    }
    if let Ok(pid) = fields[1].trim_matches('"').parse::<u32>() {
      pids.push(pid);
    }
  }
  Ok(pids)
}

#[cfg(target_os = "windows")]
fn parse_csv_line(line: &str) -> Vec<&str> {
  // tasklist CSV fields are quoted; split on "," between quotes is enough.
  line.split(',').collect()
}

#[cfg(target_os = "windows")]
fn taskkill_pid(pid: u32) -> anyhow::Result<bool> {
  let output = Command::new("taskkill")
    .args(["/PID", &pid.to_string(), "/F"])
    .output()
    .context("failed to run taskkill")?;

  let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
  let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
  let combined = format!("{stdout}{stderr}");

  if output.status.success()
    || combined.contains("success")
    || combined.contains("terminated")
  {
    return Ok(true);
  }

  // "not found" / "not running" → treat as already gone (ghost socket case).
  if combined.contains("not found")
    || combined.contains("not running")
    || combined.contains("no running instance")
    || combined.contains("could not find")
  {
    return Ok(false);
  }

  bail!(
    "taskkill PID {pid} exit={:?}: {combined}",
    output.status.code()
  );
}

/// PIDs shown by `netstat -ano` as LISTENING on 127.0.0.1:`port` (or 0.0.0.0).
#[cfg(target_os = "windows")]
fn listener_pids_on_port(port: u32) -> anyhow::Result<Vec<u32>> {
  let output = Command::new("netstat")
    .args(["-ano", "-p", "tcp"])
    .output()
    .context("failed to run netstat")?;

  let stdout = String::from_utf8_lossy(&output.stdout);
  let needles = [
    format!("127.0.0.1:{port}"),
    format!("0.0.0.0:{port}"),
    format!("[::1]:{port}"),
    format!("[::]:{port}"),
  ];

  let mut pids = Vec::new();
  for line in stdout.lines() {
    let upper = line.to_ascii_uppercase();
    if !upper.contains("LISTEN") {
      continue;
    }
    let matched = needles.iter().any(|n| line.contains(n.as_str()));
    if !matched {
      continue;
    }
    // Last whitespace-separated token is the PID.
    if let Some(pid_str) = line.split_whitespace().last() {
      if let Ok(pid) = pid_str.parse::<u32>() {
        if pid != 0 && !pids.contains(&pid) {
          pids.push(pid);
        }
      }
    }
  }
  Ok(pids)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::{Error, ErrorKind};

  #[test]
  fn detects_addr_in_use_kind() {
    let err = Error::new(ErrorKind::AddrInUse, "address already in use");
    assert!(is_addr_in_use(&err));
  }

  #[test]
  fn detects_windows_10048_message() {
    let err = Error::new(
      ErrorKind::Other,
      "Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048)",
    );
    assert!(is_addr_in_use(&err));
  }

  #[test]
  fn ignores_unrelated_errors() {
    let err = Error::new(ErrorKind::ConnectionRefused, "nope");
    assert!(!is_addr_in_use(&err));
  }
}
