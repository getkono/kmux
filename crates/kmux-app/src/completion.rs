//! Dynamic shell-completion value sources (clap_complete `unstable-dynamic`).
//!
//! These functions feed the [`clap_complete::engine::ArgValueCandidates`]
//! completers attached in [`crate::cli`] and are invoked at *completion time*
//! (the shell re-runs `kmux` with `COMPLETE=<shell>` set; see
//! [`crate::launch::run_cli`]). They therefore return **live** values —
//! `hosts.toml` aliases, theme names on disk, and the local daemon's sessions —
//! which is the whole point of "dynamic" completion.
//!
//! Every function degrades to an empty `Vec` on any error so a missing or
//! garbled config, or a daemon that is down, never breaks completion.

use clap_complete::engine::CompletionCandidate;
use std::time::Duration;

/// Hard cap on the local-daemon query for `--session` completion so a slow or
/// unreachable daemon never blocks a keystroke.
const SESSION_QUERY_TIMEOUT: Duration = Duration::from_millis(200);

/// Candidates for `--theme`: the built-in theme names plus any custom themes in
/// `<config_dir>/themes/*.toml` (the same directory [`crate::config`] reads).
pub fn theme_candidates() -> Vec<CompletionCandidate> {
    let mut out: Vec<CompletionCandidate> = crate::theme::BUILTIN_THEMES
        .iter()
        .map(|name| CompletionCandidate::new(*name).help(Some("built-in theme".into())))
        .collect();

    if let Ok(dir) = kmux_protocol::dirs::config_dir()
        && let Ok(entries) = std::fs::read_dir(dir.join("themes"))
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                out.push(CompletionCandidate::new(stem).help(Some("custom theme".into())));
            }
        }
    }
    out
}

/// Candidates for the `server` positional: the alias keys from `hosts.toml`.
pub fn server_candidates() -> Vec<CompletionCandidate> {
    kmux_client::hosts::HostsConfig::load()
        .hosts
        .into_keys()
        .map(|alias| CompletionCandidate::new(alias).help(Some("hosts.toml alias".into())))
        .collect()
}

/// Candidates for `--session`: the local daemon's live session display names and
/// word_ids (both accepted by `--session`).
///
/// The completer is a *synchronous* callback but [`run_cli`](crate::launch::run_cli)
/// calls it from inside a Tokio runtime, so we cannot `block_on` directly (that
/// panics with "Cannot start a runtime from within a runtime"). Instead we run
/// the async query on a dedicated thread with its own current-thread runtime,
/// bounded by [`SESSION_QUERY_TIMEOUT`]. Any error — no daemon, parse failure,
/// timeout — yields no candidates rather than blocking.
pub fn session_candidates() -> Vec<CompletionCandidate> {
    std::thread::spawn(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return Vec::new(),
        };
        rt.block_on(async {
            let query = kmux_client::daemon::query_daemon_sessions();
            match tokio::time::timeout(SESSION_QUERY_TIMEOUT, query).await {
                Ok(Ok(resp)) => resp
                    .sessions
                    .into_iter()
                    .flat_map(|s| {
                        [
                            CompletionCandidate::new(s.meta.name),
                            CompletionCandidate::new(s.meta.word_id),
                        ]
                    })
                    .collect(),
                _ => Vec::new(),
            }
        })
    })
    .join()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes `XDG_CONFIG_HOME` mutation across the whole `kmux-app` test
    /// binary (shared with `config.rs`'s tests, which mutate the same var).
    use crate::ENV_LOCK;

    /// The values of a candidate list, as plain strings, for easy assertions.
    fn values(candidates: &[CompletionCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn theme_candidates_include_all_builtins() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let vals = values(&theme_candidates());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        for builtin in crate::theme::BUILTIN_THEMES {
            assert!(
                vals.iter().any(|v| v == builtin),
                "missing built-in theme {builtin} in {vals:?}"
            );
        }
    }

    #[test]
    fn theme_candidates_include_custom_themes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let themes_dir = tmp.path().join("kmux").join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(themes_dir.join("my-theme.toml"), "name = \"my-theme\"").unwrap();
        // A non-.toml file is ignored.
        std::fs::write(themes_dir.join("README.md"), "ignore me").unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let vals = values(&theme_candidates());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        assert!(
            vals.iter().any(|v| v == "my-theme"),
            "custom theme missing: {vals:?}"
        );
        assert!(
            !vals.iter().any(|v| v == "README"),
            "non-toml leaked: {vals:?}"
        );
    }

    #[test]
    fn server_candidates_list_hosts_toml_aliases() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kmux_dir = tmp.path().join("kmux");
        std::fs::create_dir_all(&kmux_dir).unwrap();
        std::fs::write(
            kmux_dir.join("hosts.toml"),
            "[hosts.devbox]\nhostname = \"dev.example.com\"\n\n[hosts.prod]\nhostname = \"prod.internal\"\n",
        )
        .unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let mut vals = values(&server_candidates());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        vals.sort();
        assert_eq!(vals, vec!["devbox".to_string(), "prod".to_string()]);
    }

    #[test]
    fn server_candidates_empty_without_hosts_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let vals = values(&server_candidates());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert!(vals.is_empty(), "expected no aliases, got {vals:?}");
    }

    #[test]
    fn session_candidates_empty_when_no_daemon() {
        // No daemon is running under the test's XDG_RUNTIME_DIR socket, so the
        // query fails fast and returns nothing rather than hanging.
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        let vals = session_candidates();
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        assert!(
            vals.is_empty(),
            "expected no sessions, got {:?}",
            values(&vals)
        );
    }
}
