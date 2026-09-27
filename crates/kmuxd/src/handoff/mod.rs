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
mod status;

pub use status::{BegunHandoff, HandoffStatus, StoodDown};

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

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

/// `recvmsg` flags for a frame that may carry a PTY master fd. Where the
/// platform can, the received fd is close-on-exec from the moment it exists,
/// so a child forked concurrently never inherits it; elsewhere (macOS) it
/// gains the flag when `kmux_pty` adopts it.
#[cfg(any(target_os = "linux", target_os = "android"))]
const RECV_FLAGS: MsgFlags = MsgFlags::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECV_FLAGS: MsgFlags = MsgFlags::empty();

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
                let r = recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut cmsg), RECV_FLAGS)
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

/// A shutdown signal for a handoff in flight (issue #207). Once it fires, the
/// handoff abandons whatever step it waits on — unless it is past the commit
/// point — and rolls back, telling the successor to stand down, so that a
/// SIGTERM mid-restart stops both daemons.
#[derive(Debug, Clone)]
pub struct Cancel(tokio::sync::watch::Receiver<bool>);

/// The end of a [`Cancel`] that fires it.
#[derive(Debug)]
pub struct CancelHandle(tokio::sync::watch::Sender<bool>);

impl Cancel {
    /// A connected handle and signal, not fired.
    pub fn channel() -> (CancelHandle, Self) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (CancelHandle(tx), Self(rx))
    }

    /// Resolves once the signal fires; never, if its handle is dropped
    /// unfired. Cancel-safe.
    pub async fn cancelled(&self) {
        let mut fired = self.0.clone();
        if fired.wait_for(|fired| *fired).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl CancelHandle {
    /// Fire the signal.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

/// The pid of the process at the other end of `stream`, from the kernel's
/// peer credentials (`SO_PEERCRED` on Linux; `LOCAL_PEEREPID` and
/// `getpeereid` on macOS), refused unless it runs as this user. Each daemon
/// identifies the other by this, never by a file (issue #207).
pub(crate) fn peer_pid(stream: &UnixStream) -> anyhow::Result<nix::unistd::Pid> {
    let cred = stream
        .peer_cred()
        .map_err(|e| anyhow::anyhow!("handoff: reading the peer's credentials: {e}"))?;
    let ours = nix::unistd::getuid().as_raw();
    if cred.uid() != ours {
        anyhow::bail!("handoff: the peer runs as uid {}, not {ours}", cred.uid());
    }
    let pid = cred
        .pid()
        .ok_or_else(|| anyhow::anyhow!("handoff: the platform reports no pid for the peer"))?;
    Ok(nix::unistd::Pid::from_raw(pid))
}

/// [`read_frame`] that fails with `TimedOut` after `timeout`.
pub(crate) async fn read_frame_within(
    stream: &UnixStream,
    timeout: Duration,
) -> io::Result<(HandoffMessage, Option<OwnedFd>)> {
    tokio::time::timeout(timeout, read_frame(stream))
        .await
        .map_err(|_| timed_out("read", timeout))?
}

/// [`write_frame`] that fails with `TimedOut` after `timeout`.
pub(crate) async fn write_frame_within(
    stream: &UnixStream,
    msg: &HandoffMessage,
    fd: Option<RawFd>,
    timeout: Duration,
) -> io::Result<()> {
    tokio::time::timeout(timeout, write_frame(stream, msg, fd))
        .await
        .map_err(|_| timed_out("write", timeout))?
}

fn timed_out(what: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("handoff: frame {what} timed out after {timeout:?}"),
    )
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

    /// A shutdown signal resolves once fired (for every clone), and never
    /// when its handle is dropped unfired.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_signal_resolves_only_once_fired() {
        let never = Duration::from_secs(3600);
        let (handle, signal) = Cancel::channel();
        let clone = signal.clone();
        assert!(
            tokio::time::timeout(never, signal.cancelled())
                .await
                .is_err()
        );
        handle.cancel();
        tokio::time::timeout(never, signal.cancelled())
            .await
            .expect("fired");
        tokio::time::timeout(never, clone.cancelled())
            .await
            .expect("fired for a clone too");

        let (handle, signal) = Cancel::channel();
        drop(handle);
        assert!(
            tokio::time::timeout(never, signal.cancelled())
                .await
                .is_err()
        );
    }

    /// The peer credentials of one end of a socket pair name this process.
    #[tokio::test]
    async fn the_peer_of_a_socket_pair_is_this_process() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        assert_eq!(peer_pid(&a).expect("peer"), nix::unistd::Pid::this());
    }

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
            pid: 1,
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
