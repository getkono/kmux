//! Framing for the daemon ↔ worker link.
//!
//! Two phases, two frame shapes on the **same** socketpair:
//!
//! 1. **Handshake (lock-step, fd-carrying).** [`send_with_fd`] / [`recv_with_fd`]
//!    write `[u32 BE len][postcard payload]` via `sendmsg`/`recvmsg`, optionally
//!    attaching one fd as `SCM_RIGHTS` ancillary data. Only the daemon's opening
//!    [`Hello`](crate::WorkerRequest::Hello) (carrying the PTY master fd) and the
//!    worker's [`Ready`](crate::WorkerEvent::Ready) reply use this path. It is
//!    strictly lock-step, so a `recvmsg` never straddles two frames and a local
//!    accumulator is sufficient (no data is buffered between calls).
//!
//! 2. **Steady state (streamed, fd-less).** After the handshake both ends
//!    `into_split()` the stream and exchange frames with [`send_msg`] /
//!    [`recv_msg`], which reuse the proven `kmux_protocol::codec` length-prefix
//!    framing (`read_exact`-based, so back-to-back frames never lose bytes).
//!    No fd ever crosses here, and the two directions run concurrently on the
//!    independent read/write halves.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::Interest;
use tokio::net::UnixStream;

/// Map a `nix` errno to `io::Error`, translating `EAGAIN`/`EWOULDBLOCK` to
/// `WouldBlock` so tokio's `async_io` retries instead of failing.
fn errno_to_io(e: nix::errno::Errno) -> io::Error {
    match e {
        nix::errno::Errno::EAGAIN => io::ErrorKind::WouldBlock.into(),
        other => io::Error::from_raw_os_error(other as i32),
    }
}

/// Write one handshake frame — `[u32 BE len][postcard payload]` — optionally
/// attaching a single fd as `SCM_RIGHTS` ancillary data, in one `sendmsg`.
///
/// Used only for the lock-step `Hello`/`Ready` handshake; the small frame is
/// never fragmented, so a partial write is a hard error.
pub async fn send_with_fd<M: Serialize>(
    stream: &UnixStream,
    msg: &M,
    fd: Option<RawFd>,
) -> io::Result<()> {
    let body = postcard::to_allocvec(msg).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);

    let fds = fd.map(|f| [f]);
    let n = stream
        .async_io(Interest::WRITABLE, || {
            let iov = [io::IoSlice::new(&frame)];
            let cmsg_buf;
            let cmsgs: &[ControlMessage] = if let Some(arr) = &fds {
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
            "worker handshake: partial frame write ({n}/{} bytes)",
            frame.len()
        )));
    }
    Ok(())
}

/// Read one handshake frame, returning the message and any fd it carried.
///
/// Accumulates across `recvmsg` calls in case of a short read; an inbound fd
/// arrives with the `recvmsg` delivering the frame's leading bytes and is
/// captured as it appears. Lock-step usage guarantees the accumulator never
/// holds bytes belonging to a later frame.
pub async fn recv_with_fd<M: DeserializeOwned>(
    stream: &UnixStream,
) -> io::Result<(M, Option<OwnedFd>)> {
    let mut acc: Vec<u8> = Vec::new();
    let mut fd: Option<OwnedFd> = None;

    loop {
        if acc.len() >= 4 {
            let len = u32::from_be_bytes([acc[0], acc[1], acc[2], acc[3]]) as usize;
            if acc.len() >= 4 + len {
                let msg: M = postcard::from_bytes(&acc[4..4 + len]).map_err(io::Error::other)?;
                return Ok((msg, fd));
            }
        }

        let mut buf = [0u8; 65536];
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
                        "worker handshake: truncated ancillary data (fd lost)",
                    ));
                }
                let mut got: Option<RawFd> = None;
                for cmsg in r
                    .cmsgs()
                    .map_err(|e| io::Error::other(format!("worker handshake: cmsgs: {e}")))?
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
                "worker handshake: peer closed connection",
            ));
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(raw) = got_fd {
            // SAFETY: a freshly-received fd from SCM_RIGHTS, owned by us now.
            fd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        }
    }
}

/// Write one steady-state frame (postcard payload, length-prefixed via the
/// shared `kmux_protocol` codec). No fd; safe for concurrent back-to-back sends.
pub async fn send_msg<W, M>(w: &mut W, msg: &M) -> io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
    M: Serialize,
{
    let body = postcard::to_allocvec(msg).map_err(io::Error::other)?;
    kmux_protocol::codec::write_frame(w, &body)
        .await
        .map_err(io::Error::other)
}

/// Read one steady-state frame. Returns `Ok(None)` on a clean stream close so
/// the peer's exit is observed as EOF rather than an error.
pub async fn recv_msg<R, M>(r: &mut R) -> io::Result<Option<M>>
where
    R: tokio::io::AsyncReadExt + Unpin,
    M: DeserializeOwned,
{
    match kmux_protocol::codec::read_frame(r)
        .await
        .map_err(io::Error::other)?
    {
        Some(body) => Ok(Some(postcard::from_bytes(&body).map_err(io::Error::other)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WORKER_PROTOCOL_VERSION, WorkerEvent, WorkerRequest};
    use kmux_protocol::messages::TermSize;
    use std::os::fd::AsRawFd;

    /// The opening `Hello` carries a real fd across the socketpair via
    /// `SCM_RIGHTS`, and the worker side receives an owned dup of it — the
    /// transport proof for handing the PTY master to the worker. Uses a pipe fd
    /// as a stand-in: the received fd must be independently usable.
    #[tokio::test]
    async fn hello_carries_fd_across_socketpair() {
        use std::io::{Read, Write};

        let (daemon, worker) = UnixStream::pair().expect("socketpair");

        // A throwaway fd to ship (read end of a pipe).
        let (pipe_rd, mut pipe_wr) = std::io::pipe().expect("pipe");

        let hello = WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION,
            pane_id: "eagle/0".into(),
            pid: 4242,
            size: TermSize::default(),
            scrollback: 50_000,
            kitty_graphics: false,
            kitty_keyboard: false,
        };
        let send = send_with_fd(&daemon, &hello, Some(pipe_rd.as_raw_fd()));
        let recv = recv_with_fd::<WorkerRequest>(&worker);
        let (sent, received) = tokio::join!(send, recv);
        sent.expect("send Hello");
        let (msg, got_fd) = received.expect("recv Hello");

        assert!(
            matches!(msg, WorkerRequest::Hello { ref pane_id, version, .. }
            if pane_id == "eagle/0" && version == WORKER_PROTOCOL_VERSION)
        );
        let got_fd = got_fd.expect("fd should have crossed the socket");

        // The received fd is an independent dup of the pipe's read end: a byte
        // written to the pipe is observable through it.
        pipe_wr.write_all(b"x").expect("write pipe");
        let mut received_end = std::fs::File::from(got_fd);
        let mut buf = [0u8; 1];
        received_end.read_exact(&mut buf).expect("read received fd");
        assert_eq!(buf[0], b'x');
    }

    /// After the handshake, steady-state frames stream back-to-back over the
    /// split halves with no fd and no loss — including two frames delivered in
    /// one batch (the over-read case the streamed codec must handle).
    #[tokio::test]
    async fn steady_state_frames_stream_without_loss() {
        let (daemon, worker) = UnixStream::pair().expect("socketpair");
        let (_d_rd, mut d_wr) = daemon.into_split();
        let (mut w_rd, _w_wr) = worker.into_split();

        let writer = tokio::spawn(async move {
            send_msg(
                &mut d_wr,
                &WorkerRequest::Input {
                    data: b"a".to_vec(),
                },
            )
            .await
            .expect("send 1");
            send_msg(
                &mut d_wr,
                &WorkerRequest::Resize {
                    size: TermSize::default(),
                },
            )
            .await
            .expect("send 2");
            send_msg(&mut d_wr, &WorkerRequest::Shutdown)
                .await
                .expect("send 3");
        });

        let m1: WorkerRequest = recv_msg(&mut w_rd).await.expect("recv 1").expect("frame 1");
        let m2: WorkerRequest = recv_msg(&mut w_rd).await.expect("recv 2").expect("frame 2");
        let m3: WorkerRequest = recv_msg(&mut w_rd).await.expect("recv 3").expect("frame 3");
        writer.await.unwrap();

        assert!(matches!(m1, WorkerRequest::Input { data } if data == b"a"));
        assert!(matches!(m2, WorkerRequest::Resize { .. }));
        assert!(matches!(m3, WorkerRequest::Shutdown));

        // A clean close surfaces as EOF (None), not an error.
        let eof: io::Result<Option<WorkerRequest>> = recv_msg(&mut w_rd).await;
        assert!(matches!(eof, Ok(None)), "clean close should be EOF");
    }

    /// An event round-trips daemon-ward over the steady-state stream.
    #[tokio::test]
    async fn event_streams_worker_to_daemon() {
        let (daemon, worker) = UnixStream::pair().expect("socketpair");
        let (mut d_rd, _d_wr) = daemon.into_split();
        let (_w_rd, mut w_wr) = worker.into_split();

        tokio::spawn(async move {
            send_msg(
                &mut w_wr,
                &WorkerEvent::Ready {
                    version: WORKER_PROTOCOL_VERSION,
                },
            )
            .await
            .expect("send Ready");
        });

        let ev: WorkerEvent = recv_msg(&mut d_rd).await.expect("recv").expect("frame");
        assert!(matches!(ev, WorkerEvent::Ready { version } if version == WORKER_PROTOCOL_VERSION));
    }
}
