use tokio::sync::mpsc;

use kmux_protocol::messages::{ClientMessage, ServerMessage};

use kmux_client::grid::{GridPos, SelectionMode};

/// Messages flowing through the iced update loop.
#[derive(Debug, Clone)]
pub enum Message {
    // Connect form
    HostChanged(String),
    PortChanged(String),
    TokenChanged(String),
    ConnectPressed,
    DisconnectPressed,

    // Async connection events (emitted by subscription)
    Connected(mpsc::UnboundedSender<ClientMessage>),
    ConnectionFailed(String),
    ServerMsgBatch(Vec<ServerMessage>),

    // Session management
    SelectSession(String),
    CreateSessionPressed,
    CloseSession(String),

    // Raw keyboard event from subscription
    RawKeyEvent {
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text: Option<String>,
    },

    // Connection lost / reconnect
    Disconnected,
    Reconnect,
    DismissDisconnectToast,

    // Toggle the HUD overlay (F12) — dispatched internally via leader key
    #[allow(dead_code)]
    ToggleHud,

    // Toggle full-snapshot mode
    #[allow(dead_code)]
    ToggleSnapshotMode,

    // Terminal canvas resize detected
    TerminalResized {
        rows: u16,
        cols: u16,
    },

    // Leader key system
    LeaderTimeout,

    // Rename
    RenameInput(String),
    RenameSubmit,

    // Command palette
    CommandPaletteInput(String),
    CommandPaletteSelect,
    CommandPaletteNavigate(i32),
    #[allow(dead_code)]
    CommandPaletteClose,

    /// Scroll the terminal by the given number of lines.
    /// Positive = scroll up (into history), negative = scroll down.
    ScrollTerminal(i32),

    /// Forward mouse scroll to the PTY as escape-encoded bytes.
    /// `col` and `row` are 1-based terminal coordinates.
    ForwardMouseScroll {
        col: u16,
        row: u16,
        lines: i32,
    },

    /// Clipboard contents received for paste.
    ClipboardPaste(Option<String>),

    // Text selection
    SelectionStart {
        pos: GridPos,
        mode: SelectionMode,
    },
    SelectionUpdate {
        pos: GridPos,
    },
    SelectionEnd,
    SelectionAutoScroll(i32),
}
