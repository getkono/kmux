//! Unix domain socket transport listener.
//!
//! Phase 4+: `UdsListener` for local same-host connections.
//! Provides the lowest-overhead transport option; only available on the same host.

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::net::UnixListener;

use crate::messages::TransportKind;
use crate::transport::{AcceptError, IncomingSession, Listener, PeerInfo};

// ─── UdsListener ─────────────────────────────────────────────────────────────

/// Server-side Unix domain socket listener.
///
/// Creates the socket with restrictive permissions (`0600`) so only the owning
/// user can connect.  Any pre-existing file at `path` is removed before
/// binding to recover from a stale socket left by a previous daemon run.
///
/// The data socket path is typically `$XDG_RUNTIME_DIR/kmux/daemon-data.sock`
/// (see `kmux_protocol::dirs::data_socket_path`).
pub struct UdsListener {
    inner: UnixListener,
    path: PathBuf,
}

impl UdsListener {
    /// Bind a UDS listener at `path`.
    ///
    /// Removes any pre-existing file at `path` before binding and restricts
    /// permissions to `0600` after binding.
    pub fn bind(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        // Remove stale socket if present so `bind` doesn't fail.
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        // Restrict access to the owning user only.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            inner: listener,
            path,
        })
    }

    /// Return the socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Listener for UdsListener {
    fn kind(&self) -> TransportKind {
        TransportKind::Uds
    }

    fn accept(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send + '_>> {
        Box::pin(async move {
            let (stream, _addr) = self.inner.accept().await.map_err(AcceptError::Io)?;
            let conn_span = tracing::info_span!(
                "connection",
                transport = "uds",
                conn_id = tracing::field::Empty,
                client_id = tracing::field::Empty,
            );
            tracing::info!(parent: &conn_span, "UDS connection accepted");
            let (read, write) = tokio::io::split(stream);
            Ok(IncomingSession {
                read: Box::new(read),
                write: Box::new(write),
                kind: TransportKind::Uds,
                peer: PeerInfo { addr: None },
                span: conn_span,
                extra: Box::new(()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn uds_listener_binds_and_accepts() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.sock");

        let mut listener = UdsListener::bind(&path).expect("bind should succeed");
        assert!(path.exists());

        let connect_path = path.clone();
        tokio::spawn(async move {
            tokio::net::UnixStream::connect(&connect_path)
                .await
                .expect("connect should succeed");
        });

        let session = listener.accept().await.expect("accept should succeed");
        assert_eq!(session.kind, TransportKind::Uds);
        assert!(session.peer.addr.is_none());
    }

    #[tokio::test]
    async fn uds_listener_removes_stale_socket() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stale.sock");
        // Create a file at the path to simulate a stale socket.
        std::fs::write(&path, b"stale").unwrap();
        let _listener = UdsListener::bind(&path).expect("should bind over stale file");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn uds_listener_path_accessor() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("named.sock");
        let listener = UdsListener::bind(&path).unwrap();
        assert_eq!(listener.path(), path.as_path());
    }

    #[tokio::test]
    async fn uds_listener_permissions_0600() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("perms.sock");
        let _listener = UdsListener::bind(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        // mode() includes file type bits; mask to low 9 permission bits.
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
