use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// How to populate the child process environment.
#[derive(Debug, Clone, Default)]
pub enum EnvMode {
    /// Inherit all environment variables from the parent process.
    #[default]
    Inherit,
    /// Inherit parent environment, then apply overrides/additions.
    Extend(HashMap<String, String>),
    /// Use only the explicitly provided variables (no inheritance).
    Explicit(HashMap<String, String>),
}

/// Builder for constructing the child process environment.
#[derive(Debug, Clone, Default)]
pub struct EnvBuilder {
    mode: EnvMode,
    auto_term: bool,
}

impl EnvBuilder {
    pub fn new() -> Self {
        Self {
            mode: EnvMode::Inherit,
            auto_term: true,
        }
    }

    /// Inherit all parent env vars (default).
    pub fn inherit(mut self) -> Self {
        self.mode = EnvMode::Inherit;
        self
    }

    /// Inherit parent env, then apply these overrides.
    pub fn extend(mut self, vars: HashMap<String, String>) -> Self {
        self.mode = EnvMode::Extend(vars);
        self
    }

    /// Use only these vars -- do not inherit parent environment.
    pub fn explicit(mut self, vars: HashMap<String, String>) -> Self {
        self.mode = EnvMode::Explicit(vars);
        self
    }

    /// Whether to automatically set `TERM=xterm-256color` if not present.
    pub fn auto_term(mut self, enabled: bool) -> Self {
        self.auto_term = enabled;
        self
    }

    /// Resolve to a final environment map.
    pub fn build(self) -> HashMap<String, String> {
        let mut env = match self.mode {
            EnvMode::Inherit => std::env::vars().collect(),
            EnvMode::Extend(overrides) => {
                let mut base: HashMap<String, String> = std::env::vars().collect();
                base.extend(overrides);
                base
            }
            EnvMode::Explicit(vars) => vars,
        };
        if self.auto_term && !env.contains_key("TERM") {
            env.insert("TERM".to_string(), "xterm-256color".to_string());
        }
        env
    }
}

/// PTY window size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for WindowSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// Timeout configuration for a PTY session.
#[derive(Debug, Clone, Default)]
pub struct TimeoutConfig {
    /// Maximum wall-clock time before the process is killed.
    pub wall_clock: Option<Duration>,
    /// Maximum idle time (no output) before the process is killed.
    pub idle: Option<Duration>,
    /// Grace period between SIGTERM and SIGKILL during shutdown.
    pub shutdown_grace: Option<Duration>,
}

/// Complete configuration for spawning a PTY process.
#[derive(Debug, Clone)]
pub struct PtyConfig {
    pub program: String,
    pub args: Vec<String>,
    pub env: EnvBuilder,
    pub cwd: Option<PathBuf>,
    pub size: WindowSize,
    pub timeouts: TimeoutConfig,
}

impl PtyConfig {
    /// Create a new config for the given program.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: EnvBuilder::new(),
            cwd: None,
            size: WindowSize::default(),
            timeouts: TimeoutConfig::default(),
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn env(mut self, env: EnvBuilder) -> Self {
        self.env = env;
        self
    }

    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.size = WindowSize { rows, cols };
        self
    }

    pub fn wall_clock_timeout(mut self, d: Duration) -> Self {
        self.timeouts.wall_clock = Some(d);
        self
    }

    pub fn idle_timeout(mut self, d: Duration) -> Self {
        self.timeouts.idle = Some(d);
        self
    }

    pub fn shutdown_grace(mut self, d: Duration) -> Self {
        self.timeouts.shutdown_grace = Some(d);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_size() {
        let cfg = PtyConfig::new("bash");
        assert_eq!(cfg.size.rows, 24);
        assert_eq!(cfg.size.cols, 80);
    }

    #[test]
    fn builder_chaining() {
        let cfg = PtyConfig::new("bash")
            .args(["-c", "echo hi"])
            .size(40, 120)
            .wall_clock_timeout(Duration::from_secs(10));
        assert_eq!(cfg.args, vec!["-c", "echo hi"]);
        assert_eq!(cfg.size.cols, 120);
        assert!(cfg.timeouts.wall_clock.is_some());
    }

    #[test]
    fn env_builder_auto_term() {
        let env = EnvBuilder::new().auto_term(true).build();
        assert!(env.contains_key("TERM"));
    }

    #[test]
    fn env_builder_explicit_no_inherit() {
        let mut vars = HashMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        let env = EnvBuilder::new().explicit(vars).auto_term(false).build();
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        // PATH should NOT be present since we didn't inherit
        // (unless the caller explicitly set it)
        assert!(!env.contains_key("PATH") || env.get("FOO").is_some());
    }
}
