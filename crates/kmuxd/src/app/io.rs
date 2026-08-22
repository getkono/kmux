use kmux_protocol::messages::{ClientId, InputMode, KeyEvent, ScrollbackLine, TermSize};
use kmux_pty::error::{KmuxError, Result};

use crate::conversions::term_size_to_window;
use tracing::warn;

use super::ServerApp;
use super::attach::InputLockOutcome;
use super::helpers::{as_pane_error, get_pane_relay, get_pane_relay_mut, touch_session_for_pane};

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
        touch_session_for_pane(&sessions, pane_id);
        relay.engine.write_input(&data).await
    }

    /// Encode a batch of key events in order using the pane's live Ghostty
    /// mode state and concatenate the encoded bytes into a single PTY
    /// write.  Each event sees the state left by the previous event in the
    /// batch — important for sequences that toggle modes mid-batch (e.g.
    /// pressing the key that disables modifyOtherKeys then a follow-up).
    pub async fn write_key_batch(
        &self,
        pane_id: &str,
        client_id: ClientId,
        events: &[KeyEvent],
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
        // Nothing to write — but only after the pane and the input lock have
        // been checked, so the answer to "may I write here?" does not depend on
        // how many keys were in the batch. Not activity, so no `last_active`
        // stamp either.
        if events.is_empty() {
            return Ok(());
        }
        touch_session_for_pane(&sessions, pane_id);
        // The engine encodes each event against the emulator's live mode state
        // (in-process under the term_state lock; in a worker it owns that state)
        // so a mode-mutating sequence from an earlier event is visible to later
        // ones in the batch, then writes the bytes to the PTY.
        relay.engine.write_keys(events).await
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
        touch_session_for_pane(&sessions, pane_id);
        relay.engine.write_paste(data.as_bytes()).await
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
        let session = self
            .manager
            .get_session(pane_id)
            .await
            .map_err(|e| as_pane_error(pane_id, e))?;
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

    /// Fetch `count` scrollback lines starting at absolute `start` for
    /// `pane_id`. Returns `(first_index, lines, history_total)` where
    /// `first_index` may be greater than `start` if requested lines were
    /// evicted. An empty `lines` means the range is fully beyond the mirror.
    pub async fn fetch_history(
        &self,
        pane_id: &str,
        start: u64,
        count: u32,
    ) -> Result<(u64, Vec<ScrollbackLine>, u64)> {
        let sessions = self.sessions.read().await;
        let relay = get_pane_relay(&sessions, pane_id)?;
        let (first_index, lines, history_total) = relay.engine.fetch_history(start, count).await;
        Ok((first_index, lines, history_total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::format_pane_id;
    use kmux_protocol::messages::ClientCapabilities;

    /// A session running a long-lived childless process, plus its only pane id.
    async fn app_with_one_pane() -> (ServerApp, String, String) {
        let app = ServerApp::new("tok".to_string());
        let entry = app
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/sleep".to_string()),
                vec!["30".to_string()],
                TermSize::default(),
                &ClientCapabilities::default(),
            )
            .await
            .expect("create_session");
        let word = entry.meta.word_id;
        let pane_id = format_pane_id(&word, 0);
        (app, word, pane_id)
    }

    /// An empty batch used to return before the pane was ever looked up, so
    /// `PtyKeyBatch { pane_id: "nosuch/0", events: [] }` succeeded.
    #[tokio::test]
    async fn an_empty_key_batch_to_an_unknown_pane_is_still_a_missing_pane() {
        let app = ServerApp::new("tok".to_string());
        let err = app
            .write_key_batch("nosuch/0", ClientId(1), &[])
            .await
            .expect_err("the pane does not exist");
        assert!(
            matches!(&err, KmuxError::PaneNotFound { id } if id == "nosuch/0"),
            "expected PaneNotFound, got {err:?}"
        );
    }

    /// The input lock is the same answer for an empty batch as for a full one:
    /// whether this client may write here does not depend on how much it sent.
    #[tokio::test]
    async fn an_empty_key_batch_respects_another_clients_input_lock() {
        let (app, word, pane_id) = app_with_one_pane().await;
        let holder = ClientId(1);
        let other = ClientId(2);
        assert!(matches!(
            app.request_input_lock(&pane_id, holder).await,
            Ok(InputLockOutcome::Granted)
        ));

        let err = app
            .write_key_batch(&pane_id, other, &[])
            .await
            .expect_err("another client holds the lock");
        assert!(
            matches!(&err, KmuxError::Pty(errno) if *errno == nix::Error::EPERM),
            "expected EPERM, got {err:?}"
        );
        // The holder itself still gets the no-op.
        app.write_key_batch(&pane_id, holder, &[])
            .await
            .expect("the lock holder may write nothing");

        let _ = app.close_session(&word).await;
    }

    /// The short circuit is still there — it just runs after validation.
    #[tokio::test]
    async fn an_empty_key_batch_to_an_open_pane_writes_nothing_and_succeeds() {
        let (app, word, pane_id) = app_with_one_pane().await;
        app.write_key_batch(&pane_id, ClientId(1), &[])
            .await
            .expect("an open pane accepts an empty batch");
        let _ = app.close_session(&word).await;
    }
}
