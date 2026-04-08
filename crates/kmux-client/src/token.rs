/// Try to read the auth token from `$XDG_RUNTIME_DIR/kmux/token`.
/// Returns `None` if the env var is unset, the file is missing, or any I/O error occurs.
pub fn read_local_token() -> Option<String> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let path = std::path::Path::new(&runtime_dir)
        .join("kmux")
        .join("token");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
