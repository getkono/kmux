use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use kmux_sys::dirs::Dirs;
use rand::Rng;

/// Persist `token` to this user's kmux runtime token file with mode 0600,
/// truncating any previous token. Returns the path on success.
///
/// Resolves the directories from the process environment; [`persist_token_in`]
/// is the same write against explicit directories.
pub fn persist_token(token: &str) -> anyhow::Result<PathBuf> {
    persist_token_in(&Dirs::from_env()?, token)
}

/// [`persist_token`] into the runtime dir of `dirs`.
///
/// The directories are a parameter so the write is testable against a
/// `Dirs::rooted` tempdir without pointing the process-global
/// `XDG_RUNTIME_DIR` at it (docs/testing.md R3).
pub fn persist_token_in(dirs: &Dirs, token: &str) -> anyhow::Result<PathBuf> {
    let token_path = dirs.token_path()?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&token_path)?;
    file.write_all(token.as_bytes())?;

    Ok(token_path)
}

/// Generate a cryptographically-random auth token (32 bytes, hex-encoded = 64 chars).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// Constant-time token comparison to prevent timing attacks.
pub fn validate_token(provided: &str, expected: &str) -> bool {
    // Equal-length comparison using XOR accumulator -- always takes the same time
    // regardless of where the first difference is.
    if provided.len() != expected.len() {
        return false;
    }
    let diff: u8 = provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn validate_correct() {
        let t = generate_token();
        assert!(validate_token(&t, &t));
    }

    #[test]
    fn validate_wrong() {
        let t = generate_token();
        let wrong = generate_token();
        // Two random tokens are astronomically unlikely to match
        assert!(!validate_token(&t, &wrong));
    }

    #[test]
    fn validate_different_lengths() {
        assert!(!validate_token("short", "longer-token"));
    }

    #[test]
    fn persist_token_in_writes_the_runtime_token_file_owner_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());

        let token = generate_token();
        let path = persist_token_in(&dirs, &token).expect("persist_token_in");

        // Verify path
        assert_eq!(path, dirs.token_path().expect("token path"));

        // Verify contents
        let contents = std::fs::read_to_string(&path).expect("read token");
        assert_eq!(contents, token);

        // Verify file permissions (mode 0600)
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be mode 0600");
    }

    /// `persist_token`'s only job is resolving the runtime dir from the process
    /// environment, which a test may not mutate (R3). So it is checked in a
    /// child — this same test binary, re-run on this one test — handed
    /// `XDG_RUNTIME_DIR` through `Command::env` (R7: the in-process tier cannot
    /// set it without mutating this process).
    #[test]
    fn persist_token_resolves_the_runtime_dir_from_the_environment() {
        const ROOT: &str = "KMUX_TEST_PERSIST_TOKEN_ROOT";
        const NAME: &str =
            "auth::tests::persist_token_resolves_the_runtime_dir_from_the_environment";
        if let Some(root) = std::env::var_os(ROOT) {
            let dirs = Dirs::rooted(std::path::Path::new(&root));
            let path = persist_token("child-token").expect("persist_token");
            assert_eq!(path, dirs.token_path().expect("token path"));
            assert_eq!(std::fs::read_to_string(&path).expect("read"), "child-token");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        // `Dirs::rooted` lays the runtime base out at `<root>/run`; create it,
        // since a base named by `XDG_RUNTIME_DIR` is someone else's to create.
        Dirs::rooted(tmp.path()).runtime_dir().expect("runtime dir");
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--test-threads=1"])
            .env(ROOT, tmp.path())
            .env("XDG_RUNTIME_DIR", tmp.path().join("run"))
            .output()
            .expect("re-run the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // "1 passed" also proves the filter matched: a filter that selects
        // nothing exits 0 too.
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child persist_token did not write under XDG_RUNTIME_DIR:
{stdout}"
        );
    }
}
