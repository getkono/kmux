use std::path::PathBuf;

use nix::unistd::getuid;

/// Returns the kmux runtime directory, creating it if necessary.
///
/// Prefers `$XDG_RUNTIME_DIR/kmux` — a per-user, in-memory directory set by
/// systemd/logind on Linux with tight permissions (mode 0700).
///
/// Falls back to `/tmp/kmux-<uid>` when `XDG_RUNTIME_DIR` is unset (macOS,
/// BSDs, containers, minimal Linux environments). The UID suffix prevents
/// cross-user collisions, and `/tmp` is guaranteed on all POSIX systems.
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            let uid = getuid().as_raw();
            PathBuf::from(format!("/tmp/kmux-{uid}"))
        }
    };
    let dir = base.join("kmux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create runtime dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the daemon Unix domain socket.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// Path to the daemon PID file.
pub fn pid_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.pid"))
}

/// Path to the auth token file.
pub fn token_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests mutate XDG_RUNTIME_DIR, which is global state.
    /// Run them under a mutex so they don't race each other or other tests
    /// in this crate that also call set_var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn runtime_dir_xdg() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: protected by ENV_LOCK; no other thread touches XDG_RUNTIME_DIR
        // while this guard is held.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        let dir = runtime_dir().unwrap();
        assert_eq!(dir, tmp.path().join("kmux"));
        assert!(dir.exists());
    }

    #[test]
    fn path_helpers() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        assert!(socket_path().unwrap().ends_with("daemon.sock"));
        assert!(pid_path().unwrap().ends_with("daemon.pid"));
        assert!(token_path().unwrap().ends_with("token"));
    }
}
