//! Localhost IPC bind.
//!
//! The listener is `127.0.0.1` only (`GLAZEWM_IPC_PORT`, else 6123). One
//! attempt is made. Failure does not kill other processes, fall back to
//! another port, or abort startup — the caller disables IPC, logs an
//! error, shows a dialog, and continues.
//!
//! **Prevention:** soft `wm-exit` must Drop the `TcpListener`
//! (`SO_LINGER=0`) via `IpcServer::stop_and_wait` before process exit.
//! `taskkill /F` and crashes can still leave the port busy.

use std::{io, net::IpAddr, process::Command};

use anyhow::{bail, Context};
use tokio::net::TcpListener;
use tracing::{info, warn};
use wm_common::{preferred_bind_port, write_ipc_port_file};

use crate::commands::general::layout_debug_log;

/// Windows WSAEADDRINUSE.
#[cfg(target_os = "windows")]
const WSAEADDRINUSE: i32 = 10048;

/// Returns whether `ip` is a loopback address.
///
/// IPC accepts no other peers. The listen socket is also bound to
/// `127.0.0.1`, so this is a second check at accept time.
#[must_use]
pub fn is_localhost_ip(ip: IpAddr) -> bool {
  ip.is_loopback()
}

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

/// Binds the IPC listener on localhost, once.
///
/// Logs the attempt before the bind. Returns the listener and
/// `127.0.0.1:{port}` on success.
pub async fn bind_ipc_listener() -> anyhow::Result<(TcpListener, String)> {
  let preferred = preferred_bind_port();
  let addr = format!("127.0.0.1:{preferred}");
  let starting =
    format!("startup: about to bind IPC on {addr} (localhost only)");
  info!("{starting}");
  layout_debug_log(&starting);

  match try_bind_port(preferred).await {
    Ok(listener) => Ok(finish_bind(listener, preferred, preferred, 0)),
    Err(err) => {
      let reason = if is_addr_in_use(&err) {
        "port already in use"
      } else {
        "bind failed"
      };
      bail!("Failed to bind IPC listener on {addr} ({reason}): {err}")
    }
  }
}

fn finish_bind(
  listener: TcpListener,
  bound_port: u32,
  preferred: u32,
  kill_rounds: u32,
) -> (TcpListener, String) {
  let addr = format!("127.0.0.1:{bound_port}");
  // Always write ipc.port so Zebar/CLI have a single authoritative port
  // (including when bound to preferred 6123). Do not clear on default.
  match write_ipc_port_file(bound_port) {
    Ok(()) => {
      layout_debug_log(format!(
        "IPC ipc.port write -> {bound_port} (preferred={preferred}, kill_rounds={kill_rounds})"
      ));
    }
    Err(err) => {
      warn!("Failed to write ipc.port file: {err}");
      layout_debug_log(format!("IPC ipc.port write FAILED: {err}"));
    }
  }
  let msg = format!(
    "IPC bind succeeded on {addr} (preferred={preferred}, kill_rounds={kill_rounds})"
  );
  info!("{msg}");
  layout_debug_log(&msg);
  (listener, addr)
}

// Async so non-Windows can `.await` Tokio bind; Windows path is sync.
#[allow(clippy::unused_async)]
async fn try_bind_port(port: u32) -> io::Result<TcpListener> {
  // Build the listen socket with SO_LINGER=0 so an abortive close on Drop
  // / process soft-exit releases the port immediately on Windows instead
  // of leaving a ghost LISTENING entry after taskkill-style deaths when
  // possible. Soft wm-exit still must Drop the listener (see
  // IpcServer::stop_and_wait); linger alone cannot fix TerminateProcess
  // ghosts.
  #[cfg(target_os = "windows")]
  {
    use std::net::SocketAddr;

    use socket2::{Domain, Protocol, Socket, Type};

    let addr: SocketAddr = format!("127.0.0.1:{port}")
      .parse()
      .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let socket =
      Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    // Do NOT enable SO_REUSEADDR on Windows for exclusive IPC — reuse can
    // mask live conflicts and confuse recovery. Linger=0 for abortive
    // close on Drop.
    socket.set_linger(Some(std::time::Duration::from_secs(0)))?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
  }
  #[cfg(not(target_os = "windows"))]
  {
    TcpListener::bind(format!("127.0.0.1:{port}")).await
  }
}

#[cfg(target_os = "windows")]
fn hidden_command(program: &str) -> Command {
  use std::os::windows::process::CommandExt;
  // CREATE_NO_WINDOW: prevent blank console flashes for
  // tasklist/netstat/taskkill.
  const CREATE_NO_WINDOW: u32 = 0x0800_0000;
  let mut cmd = Command::new(program);
  cmd.creation_flags(CREATE_NO_WINDOW);
  cmd
}

/// Kill watcher during graceful WM exit.
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
            Ok(false) => parts
              .push(format!("{name} PID {pid} already gone / unkillable")),
            Err(err) => {
              parts
                .push(format!("failed killing {name} PID {pid}: {err}"));
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
  let output = hidden_command("tasklist")
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
  let output = hidden_command("taskkill")
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

#[cfg(test)]
mod tests {
  use std::{
    io::{Error, ErrorKind},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
  };

  use super::*;

  #[test]
  fn detects_addr_in_use_kind() {
    let err = Error::new(ErrorKind::AddrInUse, "address already in use");
    assert!(is_addr_in_use(&err));
  }

  #[test]
  fn detects_windows_10048_message() {
    let err = Error::other(
      "Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048)"
    );
    assert!(is_addr_in_use(&err));
  }

  #[test]
  fn ignores_unrelated_errors() {
    let err = Error::new(ErrorKind::ConnectionRefused, "nope");
    assert!(!is_addr_in_use(&err));
  }

  #[test]
  fn localhost_check_accepts_only_loopback() {
    assert!(is_localhost_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert!(is_localhost_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    assert!(!is_localhost_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    assert!(!is_localhost_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    assert!(!is_localhost_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
  }
}
