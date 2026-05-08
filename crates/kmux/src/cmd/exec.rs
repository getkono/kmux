//! Glue between `Mode::Command` submission and the registered command bodies.

use crate::app::{App, SwitchTarget};

use super::parse::{ParseError, parse};
use super::registry;
use super::spec::CommandSuccess;

/// What the command runner asks the event loop to do next.
pub enum Outcome {
    /// Continue normally (status message may be set).
    Continue,
    Quit,
    Reconnect,
    SwitchServer(SwitchTarget),
}

/// Parse `buffer` against the registry and execute it. Sets `app.mgr.status_msg`
/// for status / error messages so the existing bottom-bar render shows them.
pub fn run(app: &mut App, buffer: &str) -> Outcome {
    let parsed = match parse(buffer, registry::ALL) {
        Ok(p) => p,
        Err(e) => {
            app.mgr.set_status_msg(error_message(&e));
            return Outcome::Continue;
        }
    };
    let result = (parsed.spec.run)(app, &parsed.args);
    match result {
        Ok(CommandSuccess::Ok) => Outcome::Continue,
        Ok(CommandSuccess::Status(s)) => {
            app.mgr.set_status_msg(s);
            Outcome::Continue
        }
        Ok(CommandSuccess::Quit) => Outcome::Quit,
        Ok(CommandSuccess::Reconnect) => Outcome::Reconnect,
        Ok(CommandSuccess::SwitchServer(t)) => Outcome::SwitchServer(t),
        Err(msg) => {
            app.mgr
                .set_status_msg(format!("/{}: {msg}", parsed.spec.name));
            Outcome::Continue
        }
    }
}

fn error_message(e: &ParseError) -> String {
    e.message()
}
