//! Graceful daemon handoff: migrate live PTY master file descriptors from an
//! outgoing daemon (O) to a freshly-spawned successor (N) so running shells
//! survive a planned restart (issue #35).
//!
//! The two daemons exchange [`HandoffMessage`] frames over a dedicated Unix
//! socket ([`kmux_sys::dirs::Dirs::handoff_socket_path`]); the only payload carried out-of-band
//! is the PTY master fd, delivered via `SCM_RIGHTS` ancillary data. Because the
//! successor receives its own `dup` of the same open file description, the child
//! keeps its controlling terminal across the handoff and is merely reparented to
//! init when O exits.
//!
//! - [`sender`] drives O: spawn N, advertise the panes, stream the fds, quiesce,
//!   checkpoint, and exit.
//! - [`receiver`] drives N: connect, pull the fds, and report them back to
//!   startup for reconstruction via [`crate::app::ServerApp::restore_with_handoff`].
//!
//! See `docs/daemon-handoff.md` for the full sequence and fault-tolerance model.

pub mod receiver;
pub mod sender;

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use kmux_protocol::control_rpc::HandoffMessage;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use tokio::io::Interest;
use tokio::net::UnixStream;

/// Convert a `nix` errno into an `io::Error`, mapping `EAGAIN`/`EWOULDBLOCK` to
/// `WouldBlock` so tokio's `async_io` retries instead of failing.
fn errno_to_io(e: nix::errno::Errno) -> io::Error {
    match e {
        nix::errno::Errno::EAGAIN => io::ErrorKind::WouldBlock.into(),
        other => io::Error::from_raw_os_error(other as i32),
    }
}

/// Write one handoff frame — a 4-byte big-endian length prefix followed by the
/// JSON-encoded message — optionally carrying a single fd as `SCM_RIGHTS`
/// ancillary data, in one `sendmsg`.
///
/// The handoff is lock-step (each side awaits the peer's reply before sending
/// again), so the send buffer is always drained and the small frame is never
/// fragmented; a partial write is treated as a hard error.
pub(crate) async fn write_frame(
    stream: &UnixStream,
    msg: &HandoffMessage,
    fd: Option<RawFd>,
) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);

    let fds = fd.map(|f| [f]);
    let n = stream
        .async_io(Interest::WRITABLE, || {
            let iov = [io::IoSlice::new(&frame)];
            let cmsg_buf;
            let cmsgs: &[ControlMessage<'_>] = if let Some(arr) = &fds {
                cmsg_buf = [ControlMessage::ScmRights(arr.as_slice())];
                &cmsg_buf
            } else {
                &[]
            };
            sendmsg::<()>(stream.as_raw_fd(), &iov, cmsgs, MsgFlags::empty(), None)
                .map_err(errno_to_io)
        })
        .await?;
    if n != frame.len() {
        return Err(io::Error::other(format!(
            "handoff: partial frame write ({n}/{} bytes)",
            frame.len()
        )));
    }
    Ok(())
}

/// Read one handoff frame, returning the message and any fd it carried.
///
/// Accumulates across `recvmsg` calls in case of a short read; an inbound fd
/// arrives with the `recvmsg` that delivers the frame's leading bytes and is
/// captured as it appears.
pub(crate) async fn read_frame(
    stream: &UnixStream,
) -> io::Result<(HandoffMessage, Option<OwnedFd>)> {
    let mut acc: Vec<u8> = Vec::new();
    let mut fd: Option<OwnedFd> = None;
    // One heap buffer for the whole read, not a 64 KiB stack array rebuilt on
    // every iteration: this lives inside an async fn, so a stack array of this
    // size lands in the future itself.
    let mut buf = vec![0u8; 65536];

    loop {
        if acc.len() >= 4 {
            let len = u32::from_be_bytes([acc[0], acc[1], acc[2], acc[3]]) as usize;
            if acc.len() >= 4 + len {
                let msg: HandoffMessage =
                    serde_json::from_slice(&acc[4..4 + len]).map_err(io::Error::other)?;
                return Ok((msg, fd));
            }
        }

        let mut cmsg = nix::cmsg_space!(RawFd);
        let (n, got_fd) = stream
            .async_io(Interest::READABLE, || {
                let mut iov = [io::IoSliceMut::new(&mut buf)];
                let r = recvmsg::<()>(
                    stream.as_raw_fd(),
                    &mut iov,
                    Some(&mut cmsg),
                    MsgFlags::empty(),
                )
                .map_err(errno_to_io)?;
                if r.flags.contains(MsgFlags::MSG_CTRUNC) {
                    return Err(io::Error::other(
                        "handoff: truncated ancillary data (fd lost)",
                    ));
                }
                let mut got: Option<RawFd> = None;
                for cmsg in r
                    .cmsgs()
                    .map_err(|e| io::Error::other(format!("handoff: cmsgs: {e}")))?
                {
                    if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
                        for raw in raw_fds {
                            match got {
                                None => got = Some(raw),
                                // Defensive: close any unexpected extra fds.
                                Some(_) => unsafe {
                                    nix::libc::close(raw);
                                },
                            }
                        }
                    }
                }
                Ok((r.bytes, got))
            })
            .await?;

        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff: peer closed connection",
            ));
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(raw) = got_fd {
            // SAFETY: a freshly-received fd from SCM_RIGHTS, owned by us now.
            fd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        }
    }
}

/// Removes a Unix socket path on drop (e.g. the handoff socket).
struct PathGuard(PathBuf);

impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kmux_protocol::control_rpc::HANDOFF_PROTOCOL_VERSION;
    use kmux_pty::PtyProcess;
    use kmux_pty::config::PtyConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use super::*;

    /// A live PTY master fd survives a trip across a Unix socket via `SCM_RIGHTS`:
    /// the receiver's dup drives the same child after the sender drops it. This is
    /// the end-to-end transport proof for live PTY migration.
    #[tokio::test]
    async fn pane_fd_round_trips_and_keeps_child_alive() {
        let (a, b) = UnixStream::pair().expect("socketpair");

        let original = PtyProcess::spawn(&PtyConfig::new("/bin/cat")).expect("spawn");
        let pid = original.pid;
        let size = original.size;
        let fd = original.io.dup_owned().expect("dup master");

        let pane_fd = HandoffMessage::PaneFd {
            pane_id: "eagle/0".to_string(),
        };
        let send = write_frame(&a, &pane_fd, Some(fd.as_raw_fd()));
        let recv = read_frame(&b);
        let (sent, received) = tokio::join!(send, recv);
        sent.expect("send PaneFd");
        let (msg, got_fd) = received.expect("recv PaneFd");

        assert!(matches!(msg, HandoffMessage::PaneFd { ref pane_id } if pane_id == "eagle/0"));
        let got_fd = got_fd.expect("fd should have crossed the socket");

        // Drop our local dup; only the received fd (and the soon-dropped original)
        // remain. Adopt the received fd and confirm the child is still usable.
        drop(fd);
        let mut inherited = PtyProcess::from_inherited(got_fd, pid, size).expect("from_inherited");
        original.set_keep_alive(true);
        drop(original);

        inherited.io.write_all(b"ping\n").await.expect("write");
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), inherited.io.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.contains("ping") {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            seen.contains("ping"),
            "received fd should drive the live child; got {seen:?}"
        );
        assert!(
            nix::sys::signal::kill(pid, None).is_ok(),
            "child should still be alive after the fd handoff"
        );
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    /// Control frames without an fd round-trip cleanly, and a version-mismatched
    /// `Hello` is observable so the successor can `Decline` and fall back to a
    /// snapshot restore.
    #[tokio::test]
    async fn hello_version_mismatch_round_trips_for_decline() {
        let (a, b) = UnixStream::pair().expect("socketpair");

        let hello = HandoffMessage::Hello {
            version: HANDOFF_PROTOCOL_VERSION + 1,
            token: "tok".to_string(),
            panes: vec![],
        };
        let (sent, received) = tokio::join!(write_frame(&a, &hello, None), read_frame(&b));
        sent.expect("send Hello");
        let (msg, fd) = received.expect("recv Hello");
        assert!(fd.is_none(), "Hello carries no fd");
        match msg {
            HandoffMessage::Hello { version, .. } => {
                assert_ne!(
                    version, HANDOFF_PROTOCOL_VERSION,
                    "test feeds a mismatched version"
                );
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        // The successor declines; the predecessor reads it back.
        let decline = HandoffMessage::Decline {
            reason: "version".to_string(),
        };
        let (sent, received) = tokio::join!(write_frame(&b, &decline, None), read_frame(&a));
        sent.expect("send Decline");
        assert!(matches!(
            received.expect("recv Decline").0,
            HandoffMessage::Decline { .. }
        ));
    }
}
