/// Current wire protocol version. Bump when the wire format changes.
///
/// The client sends this in `ClientMessage::Auth` and the server rejects
/// connections whose version does not match exactly. Because the wire codec
/// (postcard) is positional, any field addition, removal, or reordering in
/// `ClientMessage` or `ServerMessage` is a breaking change that requires a
/// bump.
///
/// # When to bump
///
/// - Adding, removing, or reordering fields in any message variant.
/// - Adding new enum variants (postcard encodes variant index as a varint).
/// - Changing the semantics of an existing field in a way that old code would
///   misinterpret.
///
/// You do **not** need to bump for purely behavioural changes that leave the
/// wire format unchanged (e.g. changing server-side timeout values).
pub const PROTOCOL_VERSION: u32 = 13;

/// Parse a version-mismatch reason string and return an actionable upgrade
/// hint, or an empty string if the reason is not a version mismatch.
///
/// Expected format: `"protocol version mismatch: client=X, server=Y"`.
pub fn version_mismatch_hint(reason: &str) -> &'static str {
    if let Some(rest) = reason.strip_prefix("protocol version mismatch: client=") {
        let parts: Vec<&str> = rest.splitn(2, ", server=").collect();
        if parts.len() == 2
            && let (Ok(client_v), Ok(server_v)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>())
        {
            return if client_v < server_v {
                "Hint: your client is older than the server. Update kmux to match."
            } else {
                "Hint: your client is newer than the server. Update kmuxd to match."
            };
        }
    }
    ""
}

/// Return the current wall-clock time as milliseconds since the Unix epoch.
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
