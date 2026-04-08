//! kmux -- async PTY lifecycle management for Rust
//!
//! kmux wraps POSIX `forkpty`/`openpty` with an async-first tokio API,
//! providing ergonomic process spawning, I/O, lifecycle management,
//! session persistence, and one-shot command execution.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use kmux_pty::{PtyConfig, oneshot};
//!
//! #[tokio::main]
//! async fn main() -> kmux_pty::Result<()> {
//!     let config = PtyConfig::new("/bin/echo").args(["hello, kmux!"]);
//!     let output = oneshot::run(&config).await?;
//!     println!("{}", output.stdout_str());
//!     Ok(())
//! }
//! ```

// Module declarations
pub mod config;
pub mod error;
pub mod events;
pub mod expect;
pub mod io;
pub mod mock;
pub mod oneshot;
pub mod platform;
pub mod probe;
pub mod process;
pub mod pty;
pub mod registry;
pub mod resize;
pub mod session;
pub mod shell;
pub mod shutdown;
pub mod timeout;

// Re-exports for ergonomic top-level usage
pub use config::{EnvBuilder, EnvMode, PtyConfig, TimeoutConfig, WindowSize};
pub use error::{KmuxError, Result};
pub use events::{EventBus, SessionEvent, TimeoutKind};
pub use expect::ExpectSession;
pub use mock::{MockPty, MockPtyHandle};
pub use probe::{ProbeFn, contains_probe, wait_until_ready};
pub use process::ExitStatus;
pub use pty::PtyProcess;
pub use registry::SessionManager;
pub use session::{PtyReader, PtySession, PtyWriter};
pub use shutdown::graceful_shutdown;
