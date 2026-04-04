use crate::error::{Result, kmuxError};
use std::path::Path;

/// Detect the user's preferred shell.
///
/// Resolution order:
/// 1. `$SHELL` environment variable
/// 2. `/bin/sh` as a universal fallback
pub fn detect_shell() -> Result<String> {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        validate_shell(&shell)?;
        return Ok(shell);
    }
    // Universal POSIX fallback
    let fallback = "/bin/sh";
    validate_shell(fallback)?;
    Ok(fallback.to_string())
}

/// Validate that the shell path exists and is executable.
pub fn validate_shell(path: &str) -> Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(kmuxError::ShellNotFound {
            path: path.to_string(),
        });
    }
    // Check execute permission via metadata
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(p).map_err(kmuxError::Io)?;
    let mode = meta.permissions().mode();
    // Check owner/group/other execute bits (0o111)
    if mode & 0o111 == 0 {
        return Err(kmuxError::ShellNotFound {
            path: path.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_fallback_exists() {
        // /bin/sh must exist on any POSIX system
        assert!(validate_shell("/bin/sh").is_ok());
    }

    #[test]
    fn nonexistent_shell_errors() {
        let result = validate_shell("/nonexistent/shell/path");
        assert!(result.is_err());
    }

    #[test]
    fn detect_returns_something() {
        let shell = detect_shell().expect("should detect a shell");
        assert!(!shell.is_empty());
    }
}
