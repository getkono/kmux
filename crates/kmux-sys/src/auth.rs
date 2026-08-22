// Authentication helpers shared across transports.
//
// Peer-credential helpers allow the server to verify that a UDS client is
// running as the same user (UID) without requiring a token, which is useful
// when the daemon and client are on the same host.
//
// Platform support:
//   - Linux / Android: `SO_PEERCRED` via `getsockopt(2)`.
//   - macOS / iOS:     `LOCAL_PEERCRED` via `getsockopt(2)`.

/// Retrieve the effective UID of the process at the other end of a Unix
/// domain socket.
///
/// Returns `Ok(uid)` on success, or an `Err` if the platform is unsupported
/// or the syscall fails.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_uid(fd: std::os::unix::io::BorrowedFd<'_>) -> std::io::Result<u32> {
    nix::sys::socket::getsockopt(&fd, nix::sys::socket::sockopt::PeerCredentials)
        .map(|c| c.uid())
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}

/// Retrieve the effective UID of the process at the other end of a Unix
/// domain socket.
#[cfg(target_os = "macos")]
pub fn peer_uid(fd: std::os::unix::io::BorrowedFd<'_>) -> std::io::Result<u32> {
    nix::sys::socket::getsockopt(&fd, nix::sys::socket::sockopt::LocalPeerCred)
        .map(|c| c.uid())
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}

/// Check whether the peer UID of a UDS socket matches the effective UID of
/// the current process — i.e., the connection is from the same user.
///
/// Returns `true` if `peer_uid(fd) == geteuid()`, `false` otherwise (including
/// on error, to fail safe).
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
pub fn peer_is_same_user(fd: std::os::unix::io::BorrowedFd<'_>) -> bool {
    match peer_uid(fd) {
        Ok(uid) => uid == nix::unistd::geteuid().as_raw(),
        Err(_) => false,
    }
}

#[cfg(test)]
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::io::AsFd;

    #[test]
    fn peer_is_same_user_on_connected_socket_pair() {
        // socketpair gives us two connected UDS sockets in the same process.
        let (a, b) = nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::empty(),
        )
        .expect("socketpair");
        assert!(peer_is_same_user(a.as_fd()), "same-user check failed");
        assert!(peer_is_same_user(b.as_fd()), "same-user check failed");
    }
}
