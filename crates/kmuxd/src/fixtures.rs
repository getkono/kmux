//! Shared builders for kmuxd's unit tests (docs/testing.md R5, R6).
//!
//! `#[cfg(test)]` only, so nothing here is reachable from a release build. Each
//! builder replaced the same few lines copied into several modules' tests; a
//! module whose tests need something only they use keeps it local.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use kmux_protocol::TransportKind;
use kmux_protocol::messages::{
    CursorState, GridSnapshot, LayoutNode, ServerMessage, SessionMeta, SessionStatus, TermModes,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
use crate::backend::{
    BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK, NullEventSink,
};
use crate::client_handler::{OutboundCompression, PaneAttacher, SharedClientState};
use crate::persist::{PersistedPane, PersistedSession, PersistedTab, PersistedTermSize};
use crate::term_state::{TermState, new_term_state};

/// The auth token of [`fixture_app`] — what a test client must present.
pub(crate) const FIXTURE_TOKEN: &str = "tok";

/// An empty daemon with default config, accepting [`FIXTURE_TOKEN`].
pub(crate) fn fixture_app() -> ServerApp {
    ServerApp::new(FIXTURE_TOKEN.to_string())
}

/// A [`PaneAttacher`] for tests that never stream a pane: it refuses every
/// attach, so a test that unexpectedly reaches one sees an error, not a hang.
pub(crate) struct NoopAttacher;

impl PaneAttacher for NoopAttacher {
    fn start_pane_stream(
        &self,
        _pane_id: String,
        _result: AttachResult,
        _client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl Future<Output = Result<AbortHandle, String>> + Send {
        std::future::ready(Err("NoopAttacher streams no panes".to_string()))
    }
}

/// A fresh, unauthenticated connection's state on `app`, plus the outbound
/// compressor it shares with its writer and the receiving end of its control
/// channel (where every reply lands).
pub(crate) fn fixture_client_state(
    app: Arc<ServerApp>,
    transport: TransportKind,
) -> (
    SharedClientState,
    Arc<OutboundCompression>,
    crate::outbound::OutboundRx,
) {
    let (ctrl_tx, ctrl_rx) = crate::outbound::test_channel();
    let comp_out = Arc::new(OutboundCompression::new(
        app.compression.level,
        app.compression.min_size,
    ));
    let state = SharedClientState::new(
        app,
        ctrl_tx,
        tracing::Span::none(),
        transport,
        Arc::new(ConnectionMetrics::new()),
        Arc::clone(&comp_out),
    );
    (state, comp_out, ctrl_rx)
}

/// A blank in-process terminal of `rows` × `cols`, with no event consumer and
/// every capability off.
pub(crate) fn fixture_term_state(rows: u16, cols: u16) -> Arc<Mutex<TermState>> {
    Arc::new(Mutex::new(new_term_state(BackendConfig {
        size: BackendSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        },
        capabilities: CapabilityHandles {
            kitty_graphics: Arc::new(AtomicBool::new(false)),
            kitty_keyboard: Arc::new(AtomicBool::new(false)),
        },
        events: Arc::new(NullEventSink),
        scrollback: DEFAULT_SCROLLBACK,
    })))
}

/// A blank 24 × 80 grid.
pub(crate) fn sample_grid() -> GridSnapshot {
    GridSnapshot {
        rows: 24,
        cols: 80,
        cells: vec![Default::default(); 24 * 80],
        cursor: CursorState::default(),
        modes: TermModes::EMPTY,
        history_total: 0,
        scrollback_base: 0,
        scrollback_tail: Vec::new(),
    }
}

/// A persisted one-tab, one-pane `/bin/sh` session in `/tmp` with a blank
/// grid, as the checkpoint and the graveyard store it.
pub(crate) fn sample_persisted_session(
    word: &str,
    name: &str,
    last_active_ms: u64,
) -> PersistedSession {
    PersistedSession {
        meta: SessionMeta {
            index: 0,
            word_id: word.to_string(),
            name: name.to_string(),
            cwd: "/tmp".to_string(),
        },
        next_pane_index: 1,
        panes: vec![PersistedPane {
            pane_index: 0,
            program: "/bin/sh".to_string(),
            args: vec![],
            size: PersistedTermSize { rows: 24, cols: 80 },
            status: SessionStatus::Running,
            child_pid: None,
            grid: sample_grid(),
            scrollback_lines: vec![],
            cwd: "/tmp".to_string(),
        }],
        tabs: vec![PersistedTab {
            tab_index: 0,
            name: "1".to_string(),
            layout: LayoutNode::single(0),
            focused_pane: 0,
        }],
        next_tab_index: 1,
        active_tab: 0,
        last_active_ms,
    }
}
