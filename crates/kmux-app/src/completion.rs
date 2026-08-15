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
    kmux_sys::dirs::Dirs::from_env().map_or_else(
        // No resolvable config dir still completes the built-ins: a broken
        // HOME should degrade completion, not empty it.
        |_| {
            theme_candidates_in(&kmux_sys::dirs::Dirs::rooted(std::path::Path::new(
                "/nonexistent",
            )))
        },
        |dirs| theme_candidates_in(&dirs),
    )
}

/// [`theme_candidates`], reading custom themes from `dirs` rather than the
/// process environment. See docs/testing.md R3.
pub fn theme_candidates_in(dirs: &kmux_sys::dirs::Dirs) -> Vec<CompletionCandidate> {
    let mut out: Vec<CompletionCandidate> = crate::theme::BUILTIN_THEMES
        .iter()
        .map(|name| CompletionCandidate::new(*name).help(Some("built-in theme".into())))
        .collect();

    if let Ok(dir) = dirs.config_dir()
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

/// Candidates for the `server` positional: configured SSH hosts.
pub fn server_candidates() -> Vec<CompletionCandidate> {
    server_candidates_from_hosts(kmux_client::hosts::discover_ssh_hosts())
}

fn server_candidates_from_hosts(
    hosts: impl IntoIterator<Item = kmux_client::hosts::DiscoveredSshHost>,
) -> Vec<CompletionCandidate> {
    hosts
        .into_iter()
        .map(|host| {
            let help = match host.source {
                kmux_client::hosts::SshHostSource::KmuxHostsToml => "hosts.toml alias",
                kmux_client::hosts::SshHostSource::OpenSshConfig(_) => "ssh config host",
            };
            CompletionCandidate::new(host.alias).help(Some(help.into()))
        })
        .collect()
}

/// Candidates for `--session`: the local daemon's live session display names and
/// `word_ids` (both accepted by `--session`).
///
/// The completer is a *synchronous* callback but [`run_cli`](crate::launch::run_cli)
/// calls it from inside a Tokio runtime, so we cannot `block_on` directly (that
/// panics with "Cannot start a runtime from within a runtime"). Instead we run
/// the async query on a dedicated thread with its own current-thread runtime,
/// bounded by `SESSION_QUERY_TIMEOUT`. Any error — no daemon, parse failure,
/// timeout — yields no candidates rather than blocking.
pub fn session_candidates() -> Vec<CompletionCandidate> {
    let Ok(socket) = kmux_sys::dirs::Dirs::from_env().and_then(|d| d.socket_path()) else {
        return Vec::new();
    };
    session_candidates_at(&socket)
}

/// [`session_candidates`], querying `socket` rather than whatever the process
/// environment resolves to. See docs/testing.md R3.
pub fn session_candidates_at(socket: &std::path::Path) -> Vec<CompletionCandidate> {
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return Vec::new();
        };
        rt.block_on(async {
            let query = kmux_client::daemon::query_daemon_sessions_at(&socket);
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

    /// The values of a candidate list, as plain strings, for easy assertions.
    fn values(candidates: &[CompletionCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn theme_candidates_include_all_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        let vals = values(&theme_candidates_in(&kmux_sys::dirs::Dirs::rooted(
            tmp.path(),
        )));

        for builtin in crate::theme::BUILTIN_THEMES {
            assert!(
                vals.iter().any(|v| v == builtin),
                "missing built-in theme {builtin} in {vals:?}"
            );
        }
    }

    #[test]
    fn theme_candidates_include_custom_themes() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = kmux_sys::dirs::Dirs::rooted(tmp.path());
        let themes_dir = dirs.config_dir().unwrap().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(themes_dir.join("my-theme.toml"), "name = \"my-theme\"").unwrap();
        // A non-.toml file is ignored.
        std::fs::write(themes_dir.join("README.md"), "ignore me").unwrap();
        let vals = values(&theme_candidates_in(&dirs));

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
        use kmux_client::hosts::{DiscoveredSshHost, SshHostSource};
        let mut vals = values(&server_candidates_from_hosts([
            DiscoveredSshHost {
                alias: "devbox".into(),
                user: None,
                hostname: Some("dev.example.com".into()),
                port: None,
                source: SshHostSource::KmuxHostsToml,
            },
            DiscoveredSshHost {
                alias: "prod".into(),
                user: None,
                hostname: Some("prod.internal".into()),
                port: None,
                source: SshHostSource::KmuxHostsToml,
            },
        ]));

        vals.sort();
        assert_eq!(vals, vec!["devbox".to_string(), "prod".to_string()]);
    }

    #[test]
    fn server_candidates_empty_without_discovered_hosts() {
        let vals = values(&server_candidates_from_hosts([]));
        assert!(vals.is_empty(), "expected no aliases, got {vals:?}");
    }

    #[test]
    fn session_candidates_empty_when_no_daemon() {
        // Nothing is listening on this socket, so the query fails fast and
        // returns nothing rather than hanging.
        let tmp = tempfile::tempdir().unwrap();
        let socket = kmux_sys::dirs::Dirs::rooted(tmp.path())
            .socket_path()
            .unwrap();
        let vals = session_candidates_at(&socket);
        assert!(
            vals.is_empty(),
            "expected no sessions, got {:?}",
            values(&vals)
        );
    }
}
