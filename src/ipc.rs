//! The listener socket and the client that writes to it.
//!
//! A datagram socket rather than a stream or a FIFO, because one send is one
//! message: a notification body with newlines in it needs no framing and no
//! escaping. It also fails the right way. Opening a FIFO for writing blocks
//! until a reader arrives, so a hook run from a notification daemon leaves a
//! process hung for every notification while nothing is listening; `sendto`
//! to an unbound path returns an error immediately.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::MAX_TEXT_CHARS;

/// Largest datagram accepted, in bytes: four per allowed character, the same
/// bound `read_capped` puts on stdin, plus one byte to notice an overflow.
pub const MAX_DATAGRAM: usize = MAX_TEXT_CHARS * 4 + 1;

/// Where the socket lives when no path is given.
pub fn default_path() -> Result<PathBuf> {
    socket_path(std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Resolve the default path from the supplied variable.
///
/// Split out so the rule can be exercised without mutating the environment,
/// the same reason `config_path` is split out of `default_path`.
///
/// There is deliberately no fallback to a world-writable directory. The
/// config file falls back to `$HOME/.config` when `XDG_RUNTIME_DIR` is
/// unusable, but a socket is not a file to read: anyone who can reach it can
/// put text on the screen. `XDG_RUNTIME_DIR` is per-user and 0700, and if it
/// is missing the answer is to say so rather than to bind somewhere shared.
fn socket_path(xdg: Option<OsString>) -> Result<PathBuf> {
    let base = xdg.map(PathBuf::from).filter(|p| p.is_absolute()).context(
        "XDG_RUNTIME_DIR is unset or not an absolute path; \
             pass --socket to choose one explicitly",
    )?;
    Ok(base.join("wayhud.sock"))
}

/// Send one message. Fails rather than blocking when nothing is listening.
pub fn send(path: &Path, text: &str) -> Result<()> {
    // The character cap is enforced upstream, in `read_text`; this bounds the
    // datagram itself, which is what the listener sizes its buffer from.
    anyhow::ensure!(
        text.len() < MAX_DATAGRAM,
        "message is {} bytes; the socket carries at most {}",
        text.len(),
        MAX_DATAGRAM - 1
    );
    let socket = UnixDatagram::unbound().context("creating a socket")?;
    socket.send_to(text.as_bytes(), path).with_context(|| {
        format!(
            "sending to {}; is `wayhud --listen` running?",
            path.display()
        )
    })?;
    Ok(())
}

/// Bind the listener.
///
/// A socket file left behind by a listener that did not exit cleanly would
/// make this fail forever, so a path that nothing answers on is removed and
/// rebound. Whether anything answers is decided by connecting to it, not by
/// its presence: the file outlives the process that made it.
pub fn bind(path: &Path) -> Result<UnixDatagram> {
    if path.exists() {
        anyhow::ensure!(
            UnixDatagram::unbound()
                .and_then(|s| s.connect(path))
                .is_err(),
            "another wayhud is already listening on {}",
            path.display()
        );
        std::fs::remove_file(path)
            .with_context(|| format!("removing the stale socket {}", path.display()))?;
    }
    let socket = UnixDatagram::bind(path).with_context(|| format!("binding {}", path.display()))?;
    // The runtime directory is already private, so this is the second lock on
    // the same door rather than the first; a path given with --socket may not
    // be anywhere near as careful.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    socket
        .set_nonblocking(true)
        .context("making the socket non-blocking")?;
    Ok(socket)
}

/// Read one message, or `None` when nothing is waiting.
///
/// Datagrams that are not UTF-8, or that fill the buffer, are reported and
/// dropped: a listener stays up for the next message rather than exiting on
/// something a sender got wrong.
pub fn recv(socket: &UnixDatagram, buf: &mut [u8]) -> Option<String> {
    let n = socket.recv(buf).ok()?;
    if n >= buf.len() {
        eprintln!("wayhud: dropped a message longer than {MAX_TEXT_CHARS} characters");
        return None;
    }
    match std::str::from_utf8(&buf[..n]) {
        Ok(text) => Some(text.to_string()),
        Err(e) => {
            eprintln!("wayhud: dropped a message that is not valid UTF-8: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path in the per-test temporary directory, so two runs cannot collide.
    fn temp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("wayhud-test-{tag}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_message_arrives_whole() {
        let path = temp_path("whole");
        let socket = bind(&path).expect("bind");
        send(&path, "SYSTEM ONLINE").expect("send");
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert_eq!(recv(&socket, &mut buf).as_deref(), Some("SYSTEM ONLINE"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn newlines_need_no_escaping() {
        // The whole reason for a datagram: a notification body carries line
        // breaks, and a stream would need them delimited from the message.
        let path = temp_path("newlines");
        let socket = bind(&path).expect("bind");
        send(&path, "FIRST\nSECOND\n").expect("send");
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert_eq!(recv(&socket, &mut buf).as_deref(), Some("FIRST\nSECOND\n"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn each_send_is_one_message() {
        let path = temp_path("framing");
        let socket = bind(&path).expect("bind");
        send(&path, "one").expect("send");
        send(&path, "two").expect("send");
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert_eq!(recv(&socket, &mut buf).as_deref(), Some("one"));
        assert_eq!(recv(&socket, &mut buf).as_deref(), Some("two"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nothing_waiting_is_not_an_error() {
        let path = temp_path("empty");
        let socket = bind(&path).expect("bind");
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(recv(&socket, &mut buf).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sending_with_no_listener_fails_instead_of_blocking() {
        // The property a FIFO cannot offer, and the reason this is a socket.
        let path = temp_path("nobody");
        assert!(send(&path, "x").is_err());
    }

    #[test]
    fn a_stale_socket_file_does_not_block_a_new_listener() {
        let path = temp_path("stale");
        {
            let _first = bind(&path).expect("bind");
        }
        // The file outlives the listener that made it.
        assert!(path.exists());
        let socket = bind(&path).expect("rebind over a stale socket");
        send(&path, "after").expect("send");
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert_eq!(recv(&socket, &mut buf).as_deref(), Some("after"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_second_listener_on_a_live_socket_is_refused() {
        let path = temp_path("live");
        let _first = bind(&path).expect("bind");
        let err = bind(&path).expect_err("a live socket must not be taken over");
        assert!(err.to_string().contains("already listening"), "{err:#}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_socket_is_not_readable_by_anyone_else() {
        let path = temp_path("perms");
        let _socket = bind(&path).expect("bind");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode {:o} lets others in", mode & 0o777);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_default_path_needs_an_absolute_runtime_dir() {
        assert_eq!(
            socket_path(Some("/run/user/1000".into())).unwrap(),
            PathBuf::from("/run/user/1000/wayhud.sock")
        );
        // Empty, relative or absent: no silent fallback to a shared directory.
        assert!(socket_path(Some("".into())).is_err());
        assert!(socket_path(Some("relative/path".into())).is_err());
        assert!(socket_path(None).is_err());
    }

    #[test]
    fn a_message_over_the_cap_is_refused_by_the_sender() {
        let path = temp_path("toolong");
        let _socket = bind(&path).expect("bind");
        let long = "x".repeat(MAX_DATAGRAM);
        assert!(send(&path, &long).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
