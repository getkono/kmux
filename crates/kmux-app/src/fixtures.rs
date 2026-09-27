//! Test fixtures shared across `kmux-app`'s unit-test modules (testing.md R5).

use kmux_client::session_manager::SessionManager;
use kmux_protocol::messages::{
    ClientCapabilities, LayoutNode, PaneInfo, SessionEntry, SessionMeta, SessionStatus, TabInfo,
    TermSize,
};

use crate::core::AppCore;

/// A local, disconnected `AppCore` in `Mode::Normal` with default state.
pub(crate) fn fixture_core() -> AppCore {
    let mgr = SessionManager::new(
        "127.0.0.1".into(),
        0,
        String::new(),
        true,
        ClientCapabilities::default(),
    );
    AppCore::for_test(mgr)
}

/// A local session `word` rooted at `cwd`: one tab holding one running pane
/// `"{word}/0"`.
pub(crate) fn fixture_session_entry(word: &str, cwd: &str) -> SessionEntry {
    SessionEntry {
        meta: SessionMeta {
            index: 0,
            word_id: word.into(),
            name: word.into(),
            cwd: cwd.into(),
        },
        panes: vec![PaneInfo {
            pane_id: format!("{word}/0"),
            pane_index: 0,
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: String::new(),
            progress_state: Default::default(),
            progress: None,
        }],
        tabs: vec![TabInfo {
            tab_index: 0,
            name: "1".into(),
            layout: LayoutNode::single(0),
            focused_pane: 0,
        }],
        active_tab: 0,
        peer: None,
        peer_unreachable: false,
    }
}
