use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use rand::Rng;

/// Persist `token` to the kmux runtime token file with mode 0600.
/// Returns the path on success.
pub fn persist_token(token: &str) -> anyhow::Result<PathBuf> {
    let token_path = kmux_protocol::dirs::token_path()?;
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
    fn persist_and_read_token() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: single-threaded test, no concurrent env access
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let token = generate_token();
        let path = persist_token(&token).expect("persist_token");

        // Verify path
        assert_eq!(
            path,
            tmp.path()
                .join(kmux_protocol::dirs::KMUX_DIR_NAME)
                .join("token")
        );

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
}
