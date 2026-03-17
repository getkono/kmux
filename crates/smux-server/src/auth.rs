use rand::RngCore;

/// Generate a cryptographically-random auth token (32 bytes, hex-encoded = 64 chars).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// Constant-time token comparison to prevent timing attacks.
pub fn validate_token(provided: &str, expected: &str) -> bool {
    // Equal-length comparison using XOR accumulator — always takes the same time
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
}
