use kmux_protocol::messages::{ClientId, InputMode, TermSize};
use kmux_pty::error::{KmuxError, Result};

use crate::conversions::term_size_to_window;
use tracing::warn;

use super::ServerApp;
use super::attach::InputLockOutcome;
use super::helpers::{get_pane_relay, get_pane_relay_mut};

impl ServerApp {
    /// Forward user input bytes to a pane's PTY stdin.
    pub async fn write_input(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<()> {
        let sessions = self.sessions.read().await;
        let relay = get_pane_relay(&sessions, pane_id)?;
        match &relay.input_mode {
            InputMode::Open => {}
            InputMode::Locked(holder) if *holder == client_id => {}
            InputMode::Locked(_) | InputMode::Disabled => {
                return Err(KmuxError::Pty(nix::Error::EPERM));
            }
        }
        relay.writer.write_all(&data).await
    }

    /// Paste clipboard text into a pane's PTY stdin.
    pub async fn write_paste(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data: String,
    ) -> Result<()> {
        let sessions = self.sessions.read().await;
        let relay = get_pane_relay(&sessions, pane_id)?;
        match &relay.input_mode {
            InputMode::Open => {}
            InputMode::Locked(holder) if *holder == client_id => {}
            InputMode::Locked(_) | InputMode::Disabled => {
                return Err(KmuxError::Pty(nix::Error::EPERM));
            }
        }
        let bracketed = relay.term_state.lock().unwrap().modes().bracketed_paste();
        if bracketed {
            let mut buf = Vec::with_capacity(data.len() + 12);
            buf.extend_from_slice(b"\x1b[200~");
            buf.extend_from_slice(data.as_bytes());
            buf.extend_from_slice(b"\x1b[201~");
            relay.writer.write_all(&buf).await
        } else {
            relay.writer.write_all(data.as_bytes()).await
        }
    }

    /// Resize a pane's PTY and its server-side terminal emulator.
    ///
    /// The effective pane size is the minimum of all attached clients'
    /// dimensions (smallest-wins).  The PTY TIOCSWINSZ is issued after the
    /// emulator resize and the sessions lock is released so that the async
    /// syscall doesn't hold the write guard.
    pub async fn resize(&self, pane_id: &str, client_id: ClientId, size: TermSize) -> Result<()> {
        let resize_to = {
            let mut sessions = self.sessions.write().await;
            let relay = get_pane_relay_mut(&mut sessions, pane_id)?;

            // Update this client's declared size.
            if let Some(sender) = relay.clients.lock().unwrap().get_mut(&client_id) {
                sender.size = size;
            }

            // Compute effective (smallest-wins) size; apply if changed.
            let seqno = relay
                .seqno_counter
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_sub(1);
            if let Some(new_size) = relay.apply_effective_size() {
                relay.broadcast_resize(pane_id, new_size, seqno);
                Some(new_size)
            } else {
                None
            }
        }; // sessions write lock released here

        // Issue the kernel PTY resize outside the lock (async syscall).
        if let Some(new_size) = resize_to
            && let Err(e) = self
                .manager
                .resize(pane_id, term_size_to_window(new_size))
                .await
        {
            warn!("resize: PTY ioctl failed for '{pane_id}': {e}");
        }

        Ok(())
    }

    /// Send a Unix signal to a pane's child process.
    pub async fn send_signal(&self, pane_id: &str, signal: i32) -> Result<()> {
        use nix::sys::signal::Signal;
        let session = self.manager.get_session(pane_id).await?;
        let sig = Signal::try_from(signal).map_err(|_| KmuxError::Pty(nix::Error::EINVAL))?;
        session.send_signal(sig).await
    }

    /// Request an exclusive input lock for `client_id` on `pane_id`.
    pub async fn request_input_lock(
        &self,
        pane_id: &str,
        client_id: ClientId,
    ) -> Result<InputLockOutcome> {
        let mut sessions = self.sessions.write().await;
        let relay = get_pane_relay_mut(&mut sessions, pane_id)?;
        match &relay.input_mode {
            InputMode::Open => {
                relay.input_mode = InputMode::Locked(client_id);
                Ok(InputLockOutcome::Granted)
            }
            InputMode::Locked(holder) if *holder == client_id => {
                Ok(InputLockOutcome::Granted) // idempotent
            }
            InputMode::Locked(holder) => Ok(InputLockOutcome::Denied(*holder)),
            InputMode::Disabled => Ok(InputLockOutcome::Denied(ClientId(0))),
        }
    }

    /// Release the input lock held by `client_id` on `pane_id`.
    pub async fn release_input_lock(&self, pane_id: &str, client_id: ClientId) -> Result<bool> {
        let mut sessions = self.sessions.write().await;
        let relay = get_pane_relay_mut(&mut sessions, pane_id)?;
        if relay.input_mode == InputMode::Locked(client_id) {
            relay.input_mode = InputMode::Open;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
