use crate::error::{KmuxError, Result};
use std::path::Path;

/// Detect the user's preferred shell.
///
/// Resolution order:
/// 1. `$SHELL` environment variable
/// 2. `/bin/sh` as a universal fallback
pub fn detect_shell() -> Result<String> {
    shell_from(std::env::var("SHELL").ok())
}

/// [`detect_shell`] given the value of `$SHELL`.
fn shell_from(shell_env: Option<String>) -> Result<String> {
    if let Some(shell) = shell_env
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
        return Err(KmuxError::ShellNotFound {
            path: path.to_string(),
        });
    }
    // Check execute permission via metadata
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(p).map_err(KmuxError::Io)?;
    let mode = meta.permissions().mode();
    // Check owner/group/other execute bits (0o111)
    if mode & 0o111 == 0 {
        return Err(KmuxError::ShellNotFound {
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

    /// `detect_shell` is `shell_from` over this process's `$SHELL`, read and
    /// never set (R3).
    #[test]
    fn detect_shell_resolves_this_processs_shell_variable() {
        let expected = shell_from(std::env::var("SHELL").ok()).ok();
        assert_eq!(detect_shell().ok(), expected);
    }

    #[test]
    fn shell_from_prefers_shell_env_and_falls_back_to_sh() {
        let cases = [
            // Any existing executable passes validation; not the fallback.
            (Some("/usr/bin/env"), "/usr/bin/env"),
            (None, "/bin/sh"),
            (Some(""), "/bin/sh"),
        ];
        for (env, expected) in cases {
            assert_eq!(
                shell_from(env.map(Into::into)).expect("a shell"),
                expected,
                "$SHELL = {env:?}"
            );
        }
        assert!(shell_from(Some("/nonexistent/shell/path".into())).is_err());
    }
}
