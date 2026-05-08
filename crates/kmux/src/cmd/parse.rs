//! Command-line tokenizer and resolver.
//!
//! Buffers in `Mode::Command` exclude the leading `/`. Tokens are
//! whitespace-separated except inside `"…"` or `'…'` quotes, which support
//! exactly one level of quoting (no escapes).

use super::spec::{CommandSpec, Completer};

/// Outcome of parsing a buffer against the registry.
pub struct Parsed {
    /// The matched spec.
    pub spec: &'static CommandSpec,
    /// Positional arguments, post-quote-stripping, in declaration order.
    pub args: Vec<String>,
}

/// What can go wrong while parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Empty buffer (e.g. user pressed Enter on an empty input).
    Empty,
    /// No command in the registry matched.
    Unknown { typed: String },
    /// Spec found but missing required args.
    MissingArgs { usage: String },
    /// Spec found but too many positional args.
    TooManyArgs { usage: String },
    /// Quote that never closed.
    UnclosedQuote,
}

impl ParseError {
    pub fn message(&self) -> String {
        match self {
            ParseError::Empty => "empty command".into(),
            ParseError::Unknown { typed } => format!("unknown command: /{typed}"),
            ParseError::MissingArgs { usage } => format!("missing args — usage: {usage}"),
            ParseError::TooManyArgs { usage } => format!("too many args — usage: {usage}"),
            ParseError::UnclosedQuote => "unclosed quote".into(),
        }
    }
}

/// Tokenize a buffer into shell-ish tokens. Supports `"…"` and `'…'` quoted
/// strings (no escapes). Whitespace outside quotes separates tokens.
pub fn tokenize(input: &str) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        // Skip leading whitespace.
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        let mut token = String::new();
        if first == '"' || first == '\'' {
            let quote = first;
            chars.next();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == quote {
                    closed = true;
                    break;
                }
                token.push(c);
            }
            if !closed {
                return Err(ParseError::UnclosedQuote);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                token.push(c);
                chars.next();
            }
        }
        out.push(token);
    }
    Ok(out)
}

/// Parse a buffer into a [`Parsed`] using the supplied registry.
pub fn parse(input: &str, registry: &'static [CommandSpec]) -> Result<Parsed, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }
    let tokens = tokenize(trimmed)?;
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }

    // Find the longest prefix of `tokens` that resolves to a command. Multi-word
    // names like `session new` win over `session` alone if both exist.
    let mut best: Option<(&'static CommandSpec, usize)> = None;
    for spec in registry {
        let max_len = spec.name.split_whitespace().count();
        for take in 1..=max_len.min(tokens.len()) {
            let head = tokens[..take].join(" ");
            if spec.matches_command_part(&head) && best.map(|(_, prev)| take > prev).unwrap_or(true)
            {
                best = Some((spec, take));
            }
        }
        // Aliases may have a different word-count than the canonical name.
        for alias in spec.aliases {
            let alen = alias.split_whitespace().count();
            for take in 1..=alen.min(tokens.len()) {
                let head = tokens[..take].join(" ");
                if head.eq_ignore_ascii_case(alias)
                    && best.map(|(_, prev)| take > prev).unwrap_or(true)
                {
                    best = Some((spec, take));
                }
            }
        }
    }

    let Some((spec, used)) = best else {
        return Err(ParseError::Unknown {
            typed: trimmed.to_string(),
        });
    };

    let args: Vec<String> = tokens[used..].to_vec();
    if args.len() < spec.required_arg_count() {
        return Err(ParseError::MissingArgs {
            usage: spec.usage(),
        });
    }
    if args.len() > spec.args.len() {
        return Err(ParseError::TooManyArgs {
            usage: spec.usage(),
        });
    }
    Ok(Parsed { spec, args })
}

/// Returns the static list of valid argument values for an `OnOff`/`Signals`/
/// `Themes` completer. Used by [`super::hint`] for value suggestions.
pub fn completer_values(c: Completer) -> &'static [&'static str] {
    match c {
        Completer::OnOff => &["on", "off"],
        Completer::Signals => &["kill", "term", "stop", "cont"],
        Completer::Themes => &[
            "one-dark",
            "dracula",
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
        ],
        Completer::Sessions | Completer::None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::super::spec::{ArgSpec, CommandSpec, Completer};
    use super::*;

    fn noop(_: &mut crate::app::App, _: &[String]) -> super::super::spec::CommandResult {
        Ok(super::super::spec::CommandSuccess::Ok)
    }

    static FIXTURE: &[CommandSpec] = &[
        CommandSpec {
            name: "quit",
            aliases: &["q", "exit"],
            summary: "Quit",
            args: &[],
            run: noop,
        },
        CommandSpec {
            name: "session new",
            aliases: &["s new"],
            summary: "New session",
            args: &[
                ArgSpec {
                    name: "name",
                    required: false,
                    completer: Completer::None,
                },
                ArgSpec {
                    name: "cwd",
                    required: false,
                    completer: Completer::None,
                },
            ],
            run: noop,
        },
        CommandSpec {
            name: "session rename",
            aliases: &["s rename"],
            summary: "Rename session",
            args: &[ArgSpec {
                name: "name",
                required: true,
                completer: Completer::None,
            }],
            run: noop,
        },
    ];

    #[test]
    fn tokenize_basic() {
        assert_eq!(tokenize("a b c").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn tokenize_double_quoted() {
        assert_eq!(
            tokenize("session new \"my project\" /tmp").unwrap(),
            vec!["session", "new", "my project", "/tmp"]
        );
    }

    #[test]
    fn tokenize_single_quoted() {
        assert_eq!(
            tokenize("session new 'my project'").unwrap(),
            vec!["session", "new", "my project"]
        );
    }

    #[test]
    fn tokenize_unclosed_quote() {
        assert_eq!(tokenize("a \"b").unwrap_err(), ParseError::UnclosedQuote);
    }

    fn ok(input: &str) -> Parsed {
        match parse(input, FIXTURE) {
            Ok(p) => p,
            Err(e) => panic!("parse({input:?}) failed: {e:?}"),
        }
    }
    fn err(input: &str) -> ParseError {
        match parse(input, FIXTURE) {
            Ok(_) => panic!("parse({input:?}) unexpectedly succeeded"),
            Err(e) => e,
        }
    }

    #[test]
    fn parse_simple_command() {
        let p = ok("quit");
        assert_eq!(p.spec.name, "quit");
        assert!(p.args.is_empty());
    }

    #[test]
    fn parse_alias_resolves_to_canonical() {
        let p = ok("q");
        assert_eq!(p.spec.name, "quit");
    }

    #[test]
    fn parse_multi_word_command_takes_longest_prefix() {
        let p = ok("session new myproj /tmp");
        assert_eq!(p.spec.name, "session new");
        assert_eq!(p.args, vec!["myproj", "/tmp"]);
    }

    #[test]
    fn parse_alias_for_multi_word_command() {
        let p = ok("s new myproj /tmp");
        assert_eq!(p.spec.name, "session new");
        assert_eq!(p.args, vec!["myproj", "/tmp"]);
    }

    #[test]
    fn parse_quoted_arg() {
        let p = ok("session new \"my project\"");
        assert_eq!(p.args, vec!["my project"]);
    }

    #[test]
    fn parse_missing_required_arg() {
        match err("session rename") {
            ParseError::MissingArgs { usage } => {
                assert!(usage.contains("/session rename <name>"));
            }
            other => panic!("expected MissingArgs, got {other:?}"),
        }
    }

    #[test]
    fn parse_too_many_args() {
        match err("quit oops") {
            ParseError::TooManyArgs { .. } => {}
            other => panic!("expected TooManyArgs, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_command() {
        assert!(matches!(err("nonsense"), ParseError::Unknown { .. }));
    }

    #[test]
    fn parse_empty() {
        assert_eq!(err(""), ParseError::Empty);
        assert_eq!(err("   "), ParseError::Empty);
    }

    #[test]
    fn case_insensitive_match() {
        let p = ok("QUIT");
        assert_eq!(p.spec.name, "quit");
    }
}
