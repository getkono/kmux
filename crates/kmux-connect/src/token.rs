use kmux_sys::dirs::token_path;

/// Try to read the auth token from the kmux runtime token file.
/// Returns `None` if the file is missing or any I/O error occurs.
pub fn read_local_token() -> Option<String> {
    let path = token_path().ok()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
