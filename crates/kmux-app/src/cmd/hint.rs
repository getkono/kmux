//! Live autocomplete generation for `Mode::Command`.
//!
//! `build_hints` is pure of `App` from the call site's perspective: it reads
//! the current command buffer + registry + a small set of contextual values
//! (active session names, theme list) and returns a ranked candidate list.
//! It is recomputed at every render and after every editing keystroke that
//! could affect ranking; nothing is cached on `App`.

use crate::core::AppCore;
use crate::mode::Mode;

use super::parse::{completer_values, tokenize};
use super::registry;
use super::spec::Completer;

/// One row in the hint dropdown.
pub struct Hint {
    /// Text that appears on the left of the row.
    pub display: String,
    /// One-line description shown to the right.
    pub summary: &'static str,
    /// What Tab inserts. Not necessarily the same as `display` — for a
    /// command-name hint this is just the canonical name; for a value
    /// completion this is the value.
    pub replacement: String,
    /// Byte offset within the buffer where the in-progress token starts.
    /// Tab replaces `buffer[replace_from..]` with `replacement`.
    pub replace_from: usize,
    /// Whether Tab should append a trailing space after the replacement.
    /// True when there are more args to fill or when this is the command name
    /// (so the user can immediately type args).
    pub append_space: bool,
}

/// Maximum number of hints displayed at once.
pub const MAX_HINTS: usize = 8;

/// Compute the current dropdown contents for the buffer in `app.mode`.
/// Returns an empty vec if `app.mode` is not `Mode::Command`.
pub fn build_hints(app: &AppCore) -> Vec<Hint> {
    let buffer = match &app.mode {
        Mode::Command(s) => &s.buffer,
        _ => return Vec::new(),
    };

    // Tokenize, but preserve whether the buffer ends in whitespace because
    // that distinguishes "still typing the last token" from "moved past it".
    let trimmed = buffer.trim_start();
    let leading_ws = buffer.len() - trimmed.len();
    let ends_with_ws = !buffer.is_empty()
        && buffer
            .chars()
            .next_back()
            .map(|c| c.is_whitespace())
            .unwrap_or(false);

    let tokens = tokenize(buffer).unwrap_or_default();

    // Try resolving the longest command-name prefix among the tokens (the same
    // logic as `parse::parse`, except we don't fail on missing args — we just
    // want to know which arg to suggest values for).
    let resolved = resolve_command_prefix(&tokens, ends_with_ws);

    match resolved {
        Resolved::None | Resolved::Partial => {
            // Suggest commands whose name or alias begins with what's been typed.
            command_name_hints(buffer, leading_ws)
        }
        Resolved::Spec { spec, used } => {
            // Suggest values for the current arg (or the next arg if we just
            // finished a token with whitespace).
            let arg_index = if ends_with_ws {
                tokens.len() - used
            } else {
                tokens.len().saturating_sub(used + 1)
            };
            let Some(arg) = spec.args.get(arg_index) else {
                return Vec::new();
            };
            arg_value_hints(app, buffer, &tokens, used, arg.completer, ends_with_ws)
        }
    }
}

enum Resolved {
    None,
    /// Tokens partially match a command name (e.g. user typed `sess` and we
    /// have `session new`) — suggest commands.
    Partial,
    Spec {
        spec: &'static super::spec::CommandSpec,
        used: usize,
    },
}

fn resolve_command_prefix(tokens: &[String], ends_with_ws: bool) -> Resolved {
    if tokens.is_empty() {
        return Resolved::None;
    }
    // Greedy: longest match wins.
    let mut best: Option<(&'static super::spec::CommandSpec, usize)> = None;
    for spec in registry::ALL {
        let names = std::iter::once(spec.name).chain(spec.aliases.iter().copied());
        for nm in names {
            let nlen = nm.split_whitespace().count();
            // If we don't have enough tokens to spell out the full name, only
            // accept "still typing" when the typed prefix is a prefix of the
            // multi-word name AND the user hasn't moved past it (no trailing ws).
            if nlen > tokens.len() {
                continue;
            }
            let head = tokens[..nlen].join(" ");
            if head.eq_ignore_ascii_case(nm) && best.map(|(_, prev)| nlen > prev).unwrap_or(true) {
                // Whole-name match; but if there are no extra tokens AND the
                // buffer doesn't end in whitespace, the user might still be
                // typing — don't lock in yet.
                if nlen == tokens.len() && !ends_with_ws {
                    continue;
                }
                best = Some((spec, nlen));
            }
        }
    }
    match best {
        Some((spec, used)) => Resolved::Spec { spec, used },
        None => Resolved::Partial,
    }
}

fn command_name_hints(buffer: &str, leading_ws: usize) -> Vec<Hint> {
    let typed = buffer[leading_ws..].to_ascii_lowercase();
    let mut scored: Vec<(u8, &'static super::spec::CommandSpec, &'static str)> = Vec::new();
    for spec in registry::ALL {
        let candidates = std::iter::once(spec.name).chain(spec.aliases.iter().copied());
        let mut best_score: Option<u8> = None;
        let mut best_name: &'static str = spec.name;
        for nm in candidates {
            let nm_lc = nm.to_ascii_lowercase();
            let score = if nm_lc == typed {
                0
            } else if nm_lc.starts_with(&typed) {
                1
            } else if nm_lc.contains(&typed) {
                2
            } else {
                continue;
            };
            if best_score.is_none_or(|s| score < s) {
                best_score = Some(score);
                best_name = if score == 0 || nm == spec.name {
                    spec.name
                } else if nm_lc.starts_with(&typed) {
                    nm
                } else {
                    spec.name
                };
            }
        }
        if let Some(score) = best_score {
            scored.push((score, spec, best_name));
        }
    }
    // Sort: better score first, then alphabetical.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(b.1.name)));
    scored
        .into_iter()
        .take(MAX_HINTS)
        .map(|(_, spec, _name)| Hint {
            display: format!("{:<22} {}", spec.usage(), ""),
            summary: spec.summary,
            replacement: spec.name.to_string(),
            replace_from: leading_ws,
            append_space: !spec.args.is_empty(),
        })
        .collect()
}

fn arg_value_hints(
    app: &AppCore,
    buffer: &str,
    tokens: &[String],
    used: usize,
    completer: Completer,
    ends_with_ws: bool,
) -> Vec<Hint> {
    // Determine the prefix the user has typed for the value, and where in the
    // buffer the value starts (so Tab knows what to replace).
    let (prefix, replace_from) = if ends_with_ws {
        ("".to_string(), buffer.len())
    } else {
        let last = tokens.last().cloned().unwrap_or_default();
        // Find the byte index of the last token's start in `buffer`. Because
        // tokenize strips quotes, we have to scan from the right.
        let lc_last = last.to_ascii_lowercase();
        let from = find_last_token_start(buffer, &last).unwrap_or(buffer.len());
        (lc_last, from)
    };

    let arg_index = if ends_with_ws {
        tokens.len() - used
    } else {
        tokens.len() - used - 1
    };

    let static_values = completer_values(completer);
    let dynamic_values: Vec<String> = match completer {
        Completer::Sessions => app
            .mgr
            .session_list()
            .iter()
            .map(|e| app.mgr.display_name_for(&e.meta.word_id))
            .collect(),
        _ => Vec::new(),
    };

    let mut all: Vec<String> = static_values.iter().map(|s| s.to_string()).collect();
    all.extend(dynamic_values);

    let mut filtered: Vec<String> = all
        .into_iter()
        .filter(|v| prefix.is_empty() || v.to_ascii_lowercase().starts_with(&prefix))
        .collect();
    filtered.sort();
    filtered.dedup();

    let arg_label = registry::ALL
        .iter()
        .find_map(|s| s.args.get(arg_index).map(|a| a.name))
        .unwrap_or("");

    filtered
        .into_iter()
        .take(MAX_HINTS)
        .map(|v| Hint {
            display: format!("{:<22} <{}>", v, arg_label),
            summary: "",
            replacement: maybe_quote(&v),
            replace_from,
            append_space: true,
        })
        .collect()
}

fn maybe_quote(v: &str) -> String {
    if v.contains(char::is_whitespace) {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

fn find_last_token_start(buffer: &str, token: &str) -> Option<usize> {
    let lower_buf = buffer.to_ascii_lowercase();
    let lower_tok = token.to_ascii_lowercase();
    lower_buf.rfind(&lower_tok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::ClientCapabilities;

    fn empty_app(buffer: &str) -> AppCore {
        // Cheaply build a core fixture without booting the runtime. We only set
        // the fields the hint engine reads.
        let mgr = SessionManager::new(
            "127.0.0.1".into(),
            8443,
            "tok".into(),
            true,
            ClientCapabilities::default(),
        );
        let mut core = AppCore::for_test(mgr);
        core.mode = Mode::Command(crate::mode::CommandState {
            buffer: buffer.to_string(),
            cursor: buffer.len(),
            selected: 0,
            history_pos: None,
        });
        core
    }

    #[test]
    fn empty_buffer_returns_top_level_commands() {
        let app = empty_app("");
        let hints = build_hints(&app);
        assert!(!hints.is_empty(), "should return some commands");
        // The cap is MAX_HINTS; ensure we don't exceed it.
        assert!(hints.len() <= MAX_HINTS);
    }

    #[test]
    fn prefix_filters_command_names() {
        let app = empty_app("qu");
        let hints = build_hints(&app);
        assert!(hints.iter().any(|h| h.replacement == "quit"));
        assert!(!hints.iter().any(|h| h.replacement == "redraw"));
    }

    #[test]
    fn alias_match_resolves_to_canonical() {
        // "s n" should suggest "session new" via the alias path.
        let app = empty_app("s n");
        let hints = build_hints(&app);
        assert!(
            hints.iter().any(|h| h.replacement == "session new"),
            "hints: {:?}",
            hints.iter().map(|h| &h.replacement).collect::<Vec<_>>()
        );
    }

    #[test]
    fn theme_completer_returns_theme_names_after_space() {
        let app = empty_app("theme ");
        let hints = build_hints(&app);
        let names: Vec<_> = hints.iter().map(|h| h.replacement.clone()).collect();
        assert!(names.iter().any(|n| n == "dracula"), "names: {names:?}");
    }

    #[test]
    fn theme_completer_filters_by_prefix() {
        let app = empty_app("theme drac");
        let hints = build_hints(&app);
        let names: Vec<_> = hints.iter().map(|h| h.replacement.clone()).collect();
        assert_eq!(names, vec!["dracula"]);
    }

    #[test]
    fn signal_completer_returns_signal_names() {
        let app = empty_app("signal ");
        let hints = build_hints(&app);
        let names: Vec<_> = hints.iter().map(|h| h.replacement.clone()).collect();
        for s in ["cont", "kill", "stop", "term"] {
            assert!(names.contains(&s.to_string()), "missing {s}: {names:?}");
        }
    }

    #[test]
    fn unknown_command_returns_no_hints_after_resolution() {
        // After typing "session new " (with trailing space) the resolver
        // locks in `session new` and switches to value-completion for the
        // `name` arg, which has no completer — so no hints.
        let app = empty_app("session new ");
        let hints = build_hints(&app);
        assert!(hints.is_empty(), "got {} hints", hints.len());
    }
}
