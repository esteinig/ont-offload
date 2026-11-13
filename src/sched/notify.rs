// SPDX-License-Identifier: AGPL-3.0-or-later
//! systemd notification (`Type=notify`, `WatchdogSec`) without extra
//! dependencies: datagrams to `$NOTIFY_SOCKET`. Does nothing when the
//! variable is not set.

use std::path::PathBuf;

/// Where notifications go.
#[derive(Debug, Clone, Default)]
pub struct Notifier {
    socket: Option<String>,
}

impl Notifier {
    /// From `$NOTIFY_SOCKET`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            socket: std::env::var("NOTIFY_SOCKET")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    /// To an explicit socket path (tests).
    #[must_use]
    pub fn to(path: &std::path::Path) -> Self {
        Self {
            socket: Some(path.to_string_lossy().into_owned()),
        }
    }

    /// Send `state` (e.g. `READY=1`). Errors are ignored: notification must
    /// never stop the offload.
    ///
    /// Implements: SRS-RES-003
    pub fn send(&self, state: &str) {
        let Some(s) = &self.socket else { return };
        let _ = send_to(s, state);
    }

    /// `READY=1` with a status line.
    pub fn ready(&self, status: &str) {
        self.send(&format!("READY=1\nSTATUS={}", one_line(status)));
    }

    /// `WATCHDOG=1` with a status line; sent only by a live controller loop.
    pub fn watchdog(&self, status: &str) {
        self.send(&format!("WATCHDOG=1\nSTATUS={}", one_line(status)));
    }

    /// `STOPPING=1`.
    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }
}

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(target_os = "linux")]
fn send_to(socket: &str, state: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let sock = UnixDatagram::unbound()?;
    // Never block the controller on a reader that does not keep up.
    sock.set_nonblocking(true)?;
    if let Some(name) = socket.strip_prefix('@') {
        let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
        sock.send_to_addr(state.as_bytes(), &addr)?;
    } else {
        sock.send_to(state.as_bytes(), PathBuf::from(socket))?;
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn send_to(socket: &str, state: &str) -> std::io::Result<()> {
    let sock = std::os::unix::net::UnixDatagram::unbound()?;
    sock.set_nonblocking(true)?;
    sock.send_to(state.as_bytes(), PathBuf::from(socket))?;
    Ok(())
}

#[cfg(not(unix))]
fn send_to(_socket: &str, _state: &str) -> std::io::Result<()> {
    let _ = PathBuf::new();
    Ok(())
}
