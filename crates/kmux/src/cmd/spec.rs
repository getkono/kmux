//! Data types for the command-palette registry.

use crate::app::App;

/// The result a command body returns. Mirrors the small set of things keys can
/// do — most commands simply mutate `App` and return `Ok`. Quit/Reconnect/
/// SwitchServer flow back through `KeyResult` via the [`Outcome`](super::exec::Outcome) layer.
pub type CommandResult = Result<CommandSuccess, String>;

/// A successful command may optionally bubble a control-flow signal back to
/// the event loop.
#[derive(Debug, Default)]
pub enum CommandSuccess {
    /// Run completed. No follow-up.
    #[default]
    Ok,
    /// Run completed with a status message to flash on the bottom bar.
    Status(String),
    /// Quit the TUI.
    Quit,
    /// Force a reconnect.
    Reconnect,
    /// Switch to another server.
    SwitchServer(crate::app::SwitchTarget),
}

/// Function pointer signature for command bodies.
///
/// `args` excludes the command name itself; e.g. for `/session new myproj /tmp`
/// args is `["myproj", "/tmp"]`.
pub type CommandFn = fn(&mut App, args: &[String]) -> CommandResult;

/// Metadata + handler for one command.
#[derive(Clone, Copy)]
pub struct CommandSpec {
    /// Canonical name (one or more space-separated tokens, e.g. `session new`).
    pub name: &'static str,
    /// Equivalent forms that resolve to the same handler.
    pub aliases: &'static [&'static str],
    /// One-line description shown in the hint dropdown and in the help.
    pub summary: &'static str,
    /// Argument shape, used to build a usage line and to drive completion.
    pub args: &'static [ArgSpec],
    /// Handler invoked on Enter.
    pub run: CommandFn,
}

impl CommandSpec {
    /// `"/<name> <args>"` formatted for the hint dropdown and error messages.
    pub fn usage(&self) -> String {
        let mut s = format!("/{}", self.name);
        for a in self.args {
            if a.required {
                s.push_str(&format!(" <{}>", a.name));
            } else {
                s.push_str(&format!(" [{}]", a.name));
            }
        }
        s
    }

    /// Number of required (positional) arguments.
    pub fn required_arg_count(&self) -> usize {
        self.args.iter().filter(|a| a.required).count()
    }

    /// Returns true if `token` matches the canonical name or any alias
    /// (case-insensitive). The match is whole-string on the leading tokens of
    /// `token`, allowing multi-word names like `session new` to match.
    pub fn matches_command_part(&self, command_part: &str) -> bool {
        let cp = command_part.trim().to_ascii_lowercase();
        if cp == self.name.to_ascii_lowercase() {
            return true;
        }
        for a in self.aliases {
            if cp == a.to_ascii_lowercase() {
                return true;
            }
        }
        false
    }
}

/// One argument in a [`CommandSpec`].
#[derive(Clone, Copy)]
pub struct ArgSpec {
    pub name: &'static str,
    pub required: bool,
    pub completer: Completer,
}

/// How to suggest values for an argument while the user is mid-typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completer {
    None,
    /// Active sessions: word_id and display name.
    Sessions,
    /// kill / term / stop / cont.
    Signals,
    /// Built-in theme names.
    Themes,
    /// `on` / `off`.
    OnOff,
}
