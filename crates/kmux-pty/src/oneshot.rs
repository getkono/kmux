use tokio::io::AsyncReadExt;
use tokio::time::timeout;

use crate::config::PtyConfig;
use crate::error::{KmuxError, Result};
use crate::process::ExitStatus;
use crate::pty::PtyProcess;

/// Output collected from a one-shot command.
#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub status: ExitStatus,
}

impl CommandOutput {
    /// Return stdout as a lossy UTF-8 string.
    pub fn stdout_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Return true if the command exited successfully.
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Run a command to completion, collecting all PTY output.
///
/// This is the "batteries included" one-shot runner that:
/// 1. Spawns the PTY process
/// 2. Reads all output until EOF
/// 3. Waits for process exit
/// 4. Enforces an optional wall-clock timeout
pub async fn run(config: &PtyConfig) -> Result<CommandOutput> {
    let wall_clock = config.timeouts.wall_clock;

    if let Some(deadline) = wall_clock {
        timeout(deadline, run_inner(config))
            .await
            .map_err(|_| KmuxError::Timeout)?
    } else {
        run_inner(config).await
    }
}

async fn run_inner(config: &PtyConfig) -> Result<CommandOutput> {
    let mut pty = PtyProcess::spawn(config)?;
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];

    // Read until EOF (child closed the PTY master end)
    loop {
        match pty.io.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e) if is_eof_error(&e) => break,
            Err(e) => return Err(KmuxError::Io(e)),
        }
    }

    let status = pty.wait().await;
    Ok(CommandOutput {
        stdout: output,
        status,
    })
}

/// PTY EOF manifests as EIO on Linux when the slave side is closed.
fn is_eof_error(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(nix::libc::EIO) | Some(nix::libc::EBADF)
    ) || e.kind() == std::io::ErrorKind::UnexpectedEof
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn echo_hello() {
        let config = PtyConfig::new("/bin/echo").args(["hello world"]);
        let output = run(&config).await.expect("run failed");
        assert!(output.success());
        let text = output.stdout_str();
        assert!(text.contains("hello world"), "got: {text:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_code() {
        let config = PtyConfig::new("/bin/sh").args(["-c", "exit 42"]);
        let output = run(&config).await.expect("run failed");
        assert!(!output.success());
        assert_eq!(output.status.code(), Some(42));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let config = PtyConfig::new("/bin/sleep")
            .args(["999"])
            .wall_clock_timeout(Duration::from_millis(200));
        let result = run(&config).await;
        assert!(
            matches!(result, Err(KmuxError::Timeout)),
            "expected Timeout, got {result:?}"
        );
    }
}
