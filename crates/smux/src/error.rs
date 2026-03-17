use thiserror::Error;

/// All errors that smux can produce.
#[derive(Debug, Error)]
pub enum SmuxError {
    #[error("PTY syscall failed: {0}")]
    Pty(#[from] nix::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("process spawn failed: {0}")]
    Spawn(String),

    #[error("process exited with signal {0}")]
    Signal(i32),

    #[error("timeout elapsed")]
    Timeout,

    #[error("idle timeout elapsed after {seconds}s of inactivity")]
    IdleTimeout { seconds: u64 },

    #[error("shell not found or not executable: {path}")]
    ShellNotFound { path: String },

    #[error("session not found: {name}")]
    SessionNotFound { name: String },

    #[error("session already exists: {name}")]
    SessionAlreadyExists { name: String },

    #[error("PTY is closed")]
    Closed,

    #[error("send on closed channel")]
    ChannelClosed,

    #[error("platform not supported: {detail}")]
    UnsupportedPlatform { detail: String },
}

pub type Result<T> = std::result::Result<T, SmuxError>;
