//! Recovery when the GlazeWM IPC TCP port is already bound.
//!
//! On Windows, a crashed / half-dead `glazewm.exe` or `glazewm-watcher.exe`
//! (or a **ghost** listen socket whose owning PID is already gone) can leave
//! `127.0.0.1:<port>` in LISTENING state. Startup then fails with
//! `std::io::ErrorKind::AddrInUse` / WSAEADDRINUSE (10048).
//!
//! Flow:
//! 1. Try preferred port (`GLAZEWM_IPC_PORT` / `ipc.port` / 6123).
//! 2. On AddrInUse: prompt Kill vs Quit; kill glazewm + watcher + listener PID.
//! 3. Poll bind for ~5s (ghost sockets are not freed by taskkill).
//! 4. Detect ghost (netstat PID absent from process list) and fall back.
//! 5. Fallback: try preferred+1..+10, then ephemeral `127.0.0.1:0`; write
//!    `~/.glzr/glazewm/ipc.port` so CLI/`ipc_port()` find the new port.

use std::{
  io,
  process::Command,
  time::{Duration, Instant},
};

use anyhow::{bail, Context};
use tokio::net::TcpListener;
use tracing::{info, warn};
use wm_common::{
  clear_ipc_port_file, ipc_port, write_ipc_port_file, DEFAULT_IPC_PORT,
};
use wm_platform::Dispatcher;

use crate::commands::general::layout_debug_log;

/// Max times we ask the user to kill+retry after an AddrInUse failure.
const MAX_KILL_RETRY_ROUNDS: u32 = 2;

/// After each kill round, keep trying to bind for this long before giving up.
const POST_KILL_POLL: Duration = Duration::from_secs(5);
const POST_KILL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Alternate ports to try after preferred is a ghost / still busy.
const FALLBACK_PORT_SPAN: u32 = 10;

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
  let msg = err.to_string();
  msg.contains("10048")
    || msg.contains("address already in use")
    || msg.contains("Only one usage of each socket address")
}

/// Bind the IPC listener, offering Kill-existing vs Quit on AddrInUse, then
/// falling back to alternate ports when a ghost socket cannot be freed.
pub async fn bind_ipc_listener(
  dispatcher: &Dispatcher,
) -> anyhow::Result<(TcpListener, String)> {
  let preferred = ipc_port();
  let mut kill_rounds: u32 = 0;
  let mut last_detail = String::new();
  let mut saw_ghost = false;

  loop {
    match try_bind_port(preferred).await {
      Ok(listener) => {
        return finish_bind(listener, preferred, preferred, kill_rounds).await;
      }
      Err(err) if is_addr_in_use(&err) => {
        let err_text = format!("{err}");
        let ghost = port_has_ghost_listener(preferred);
        if ghost {
          saw_ghost = true;
        }
        let log_line = format!(
          "IPC bind AddrInUse on 127.0.0.1:{preferred} (round {kill_rounds}, ghost={ghost}): {err_text}"
        );
        warn!("{log_line}");
        layout_debug_log(&log_line);

        // Ghost sockets cannot be freed by taskkill — skip further kill
        // rounds once detected (still offer one kill attempt if we have not).
        if ghost && kill_rounds >= 1 {
          layout_debug_log(format!(
            "IPC ghost listener on {preferred}; skipping further kill rounds, falling back"
          ));
          break;
        }

        if kill_rounds >= MAX_KILL_RETRY_ROUNDS {
          layout_debug_log(format!(
            "IPC port {preferred} still in use after {MAX_KILL_RETRY_ROUNDS} kill+retry; falling back"
          ));
          break;
        }

        let ghost_hint = if ghost {
          "\n\nDetected a GHOST listen socket (netstat PID is not a live process). \
           Kill cannot free it — after this attempt GlazeWM will bind an alternate port."
            .to_string()
        } else if last_detail.is_empty() {
          String::new()
        } else {
          format!("\n\nPrevious attempt notes:\n{last_detail}")
        };

        let prompt = format!(
          "GlazeWM could not bind the IPC socket on 127.0.0.1:{preferred}.\n\n\
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
            "IPC port {preferred} in use; user chose Quit (abort this instance)"
          );
          layout_debug_log(&msg);
          bail!("{msg}");
        }

        kill_rounds += 1;
        let report = kill_existing_glazewm_and_free_port(preferred);
        last_detail = report.clone();
        layout_debug_log(format!(
          "IPC kill+free-port round {kill_rounds}: {report}"
        ));
        info!("IPC kill+free-port round {kill_rounds}: {report}");

        // Poll bind for several seconds — OS may release slowly; ghosts never release.
        if let Some(listener) =
          poll_bind_port(preferred, POST_KILL_POLL, POST_KILL_POLL_INTERVAL)
            .await
        {
          return finish_bind(listener, preferred, preferred, kill_rounds)
            .await;
        }

        if port_has_ghost_listener(preferred) {
          saw_ghost = true;
          layout_debug_log(format!(
            "IPC port {preferred} still ghost after kill+poll; will fall back"
          ));
          break;
        }
      }
      Err(err) => {
        return Err(err).with_context(|| {
          format!("Failed to bind IPC listener on 127.0.0.1:{preferred}")
        });
      }
    }
  }

  // Fallback: alternate ports, then ephemeral.
  layout_debug_log(format!(
    "IPC falling back from {preferred} (ghost={saw_ghost}, kill_rounds={kill_rounds}, last={last_detail})"
  ));
  bind_fallback_ports(preferred, saw_ghost, &last_detail).await
}

async fn finish_bind(
  listener: TcpListener,
  bound_port: u32,
  preferred: u32,
  kill_rounds: u32,
) -> anyhow::Result<(TcpListener, String)> {
  let addr = format!("127.0.0.1:{bound_port}");
  if bound_port == DEFAULT_IPC_PORT {
    clear_ipc_port_file();
  } else if let Err(err) = write_ipc_port_file(bound_port) {
    warn!("Failed to write ipc.port file: {err}");
    layout_debug_log(format!("Failed to write ipc.port: {err}"));
  } else {
    layout_debug_log(format!(
      "Wrote ipc.port -> {bound_port} (CLI will use this port)"
    ));
  }
  if kill_rounds > 0 || bound_port != preferred {
    let msg = format!(
      "IPC bind succeeded on {addr} (preferred={preferred}, kill_rounds={kill_rounds})"
    );
    info!("{msg}");
    layout_debug_log(&msg);
  }
  Ok((listener, addr))
}

async fn bind_fallback_ports(
  preferred: u32,
  saw_ghost: bool,
  last_detail: &str,
) -> anyhow::Result<(TcpListener, String)> {
  let start = preferred.saturating_add(1);
  let end = preferred.saturating_add(FALLBACK_PORT_SPAN);
  for port in start..=end {
    if port == 0 {
      continue;
    }
    match try_bind_port(port).await {
      Ok(listener) => {
        layout_debug_log(format!(
          "IPC fallback bound 127.0.0.1:{port} (preferred {preferred} ghost={saw_ghost})"
        ));
        return finish_bind(listener, port, preferred, 0).await;
      }
      Err(err) if is_addr_in_use(&err) => {
        layout_debug_log(format!(
          "IPC fallback port {port} also busy: {err}"
        ));
      }
      Err(err) => {
        layout_debug_log(format!(
          "IPC fallback port {port} bind error: {err}"
        ));
      }
    }
  }

  // Ephemeral port.
  match TcpListener::bind("127.0.0.1:0").await {
    Ok(listener) => {
      let port = listener.local_addr()?.port() as u32;
      layout_debug_log(format!(
        "IPC ephemeral fallback bound 127.0.0.1:{port}"
      ));
      finish_bind(listener, port, preferred, 0).await
    }
    Err(err) => {
      let msg = format!(
        "IPC port {preferred} still in use (ghost={saw_ghost}) and all fallbacks failed. \
         Last error: {err}. {last_detail} \
         Set GLAZEWM_IPC_PORT to a free port, or reboot to clear a ghost socket."
      );
      layout_debug_log(&msg);
      bail!("{msg}");
    }
  }
}

async fn try_bind_port(port: u32) -> io::Result<TcpListener> {
  TcpListener::bind(format!("127.0.0.1:{port}")).await
}

async fn poll_bind_port(
  port: u32,
  overall: Duration,
  interval: Duration,
) -> Option<TcpListener> {
  let deadline = Instant::now() + overall;
  let mut attempts = 0u32;
  while Instant::now() < deadline {
    attempts += 1;
    match try_bind_port(port).await {
      Ok(listener) => {
        layout_debug_log(format!(
          "IPC poll bind succeeded on {port} after {attempts} attempt(s)"
        ));
        return Some(listener);
      }
      Err(err) if is_addr_in_use(&err) => {
        tokio::time::sleep(interval).await;
      }
      Err(err) => {
        layout_debug_log(format!(
          "IPC poll bind non-AddrInUse error on {port}: {err}"
        ));
        tokio::time::sleep(interval).await;
      }
    }
  }
  layout_debug_log(format!(
    "IPC poll bind gave up on {port} after {attempts} attempt(s) / {overall:?}"
  ));
  None
}

/// True when netstat shows LISTEN on `port` but the PID is not a live process.
#[must_use]
pub fn port_has_ghost_listener(port: u32) -> bool {
  #[cfg(target_os = "windows")]
  {
    match listener_pids_on_port(port) {
      Ok(pids) => pids.iter().any(|pid| !process_exists(*pid)),
      Err(_) => false,
    }
  }
  #[cfg(not(target_os = "windows"))]
  {
    let _ = port;
    false
  }
}

#[cfg(target_os = "windows")]
fn process_exists(pid: u32) -> bool {
  // tasklist /FI "PID eq N" — avoid depending on OpenProcess privileges.
  let output = Command::new("tasklist")
    .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
    .output();
  let Ok(output) = output else {
    return true; // assume exists if we cannot check
  };
  let stdout = String::from_utf8_lossy(&output.stdout);
  for line in stdout.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with("INFO:") {
      continue;
    }
    // CSV line containing the PID means the process is alive.
    if line.contains(&format!("\"{pid}\"")) || line.split(',').nth(1).map(|s| s.trim_matches('"') == pid.to_string()).unwrap_or(false) {
      return true;
    }
  }
  false
}

/// Best-effort: terminate other GlazeWM processes and the IPC listener PID.
#[must_use]
pub fn kill_existing_glazewm_and_free_port(port: u32) -> String {
  let self_pid = std::process::id();
  let mut notes: Vec<String> = Vec::new();

  #[cfg(target_os = "windows")]
  {
    // Watcher first — it may restart glazewm if we kill glazewm alone.
    let killed_named = kill_named_others(
      self_pid,
      &["glazewm-watcher.exe", "glazewm.exe"],
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
          let alive = process_exists(pid);
          if !alive {
            notes.push(format!(
              "GHOST IPC listener PID {pid} on port {port} (not in tasklist) — cannot taskkill"
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
                "IPC listener PID {pid} on port {port} not found after kill attempt"
              ));
            }
            Err(err) => {
              notes.push(format!(
                "failed to kill IPC listener PID {pid} on port {port}: {err}"
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

/// Kill watcher (and optionally other glazewm) during graceful WM exit.
#[must_use]
pub fn kill_watcher_on_exit() -> String {
  let self_pid = std::process::id();
  #[cfg(target_os = "windows")]
  {
    kill_named_others(self_pid, &["glazewm-watcher.exe"])
  }
  #[cfg(not(target_os = "windows"))]
  {
    let _ = self_pid;
    let _ = Command::new("pkill")
      .args(["-x", "glazewm-watcher"])
      .status();
    "pkill glazewm-watcher".into()
  }
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
  line.split(',').collect()
}

#[cfg(target_os = "windows")]
fn taskkill_pid(pid: u32) -> anyhow::Result<bool> {
  let output = Command::new("taskkill")
    .args(["/PID", &pid.to_string(), "/F", "/T"])
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
  )
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
