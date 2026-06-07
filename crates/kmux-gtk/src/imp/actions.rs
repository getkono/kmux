//! Native GTK actions: the `win.*` `gio::SimpleAction`s, their accelerators, the
//! primary (hamburger) menu, and the keyboard-shortcuts window.
//!
//! This is the accelerators-only keyboard model: every command the old `Ctrl+G`
//! chords reached is a `gio` action here, bound to a reserved accelerator and/or
//! a menu item, and dispatched straight to the shared [`Action`] vocabulary
//! (`AppCore::dispatch_action` / `apply_top_bar_action`). Reserved combos use
//! `Ctrl+Shift+…` / function keys / `Ctrl+digit` so they never shadow keys the
//! inner terminal program needs — everything else falls through to the PTY.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Application, Builder, ShortcutsWindow, gio};

use kmux_app::core::TopBarAction;
use kmux_app::mode::{Action, CommandState, Mode};

use super::shell::Shell;
use super::{Frontend, apply_effects, prefs};

/// The (`win.` name, dispatched `Action`, accelerators) table for the actions
/// that map straight onto a shared [`Action`]. Pure data, shared by [`install`]
/// and the tests. Reserved combos avoid keys the inner terminal needs.
fn dispatched_specs() -> Vec<(&'static str, Action, &'static [&'static str])> {
    vec![
        ("new-pane", Action::CreatePane, &["<Ctrl><Shift>t"]),
        ("close-pane", Action::ClosePane, &["<Ctrl><Shift>q"]),
        ("next-pane", Action::NextPane, &["<Ctrl><Shift>Right"]),
        ("prev-pane", Action::PrevPane, &["<Ctrl><Shift>Left"]),
        ("new-session", Action::CreateSession, &["<Ctrl><Shift>n"]),
        ("close-session", Action::CloseSession, &["<Ctrl><Shift>w"]),
        ("rename-session", Action::RenameSession, &["F2"]),
        (
            "next-session",
            Action::NextSession,
            &["<Ctrl>Tab", "<Ctrl>Page_Down"],
        ),
        (
            "prev-session",
            Action::PrevSession,
            &["<Ctrl><Shift>Tab", "<Ctrl>Page_Up"],
        ),
        ("disconnect", Action::Disconnect, &[]),
        ("reconnect", Action::Reconnect, &["<Ctrl><Shift>r"]),
        ("toggle-lock", Action::ToggleInputLock, &["<Ctrl><Shift>l"]),
        ("toggle-hud", Action::ToggleHud, &["<Ctrl><Shift>h"]),
        ("toggle-metrics", Action::ToggleMetrics, &["<Ctrl><Shift>m"]),
        ("snapshot", Action::ToggleSnapshotMode, &[]),
        ("redraw", Action::ForceRedraw, &[]),
        ("scroll-page-up", Action::ScrollPageUp, &["<Shift>Page_Up"]),
        (
            "scroll-page-down",
            Action::ScrollPageDown,
            &["<Shift>Page_Down"],
        ),
        ("copy", Action::CopySelection, &["<Ctrl><Shift>c"]),
        ("paste", Action::Paste, &["<Ctrl><Shift>v"]),
        ("signal-term", Action::SendSignal(15), &[]),
        ("signal-kill", Action::SendSignal(9), &[]),
        ("signal-stop", Action::SendSignal(19), &[]),
        ("signal-cont", Action::SendSignal(18), &[]),
    ]
}

/// The `Action` a `jump-session-N` accelerator dispatches (N is 1-based).
fn jump_action(n: u8) -> Action {
    Action::JumpToSession(n.saturating_sub(1) as usize)
}

/// Install every action + accelerator + the primary menu. Called once.
pub fn install(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, app: &Application) {
    for (name, action, accels) in dispatched_specs() {
        add_dispatch(shell, fe, app, name, action);
        if !accels.is_empty() {
            app.set_accels_for_action(&format!("win.{name}"), accels);
        }
    }

    // Jump to session 1..9.
    for n in 1..=9u8 {
        let name = format!("jump-session-{n}");
        add_dispatch(shell, fe, app, &name, jump_action(n));
        let accel = format!("<Ctrl>{n}");
        app.set_accels_for_action(&format!("win.{name}"), &[&accel]);
    }

    add_command(shell, fe);
    add_switch_server(shell, fe);
    add_toggle_sidebar(shell);
    add_preferences(shell, fe);
    add_help(shell);
    add_quit(shell, app);

    app.set_accels_for_action("win.command", &["<Ctrl><Shift>p"]);
    app.set_accels_for_action("win.toggle-sidebar", &["F9"]);
    app.set_accels_for_action("win.preferences", &["<Ctrl>comma"]);
    app.set_accels_for_action("win.help", &["<Ctrl>question", "F1"]);
    app.set_accels_for_action("win.quit", &["<Ctrl>q"]);

    shell.menu_btn.set_menu_model(Some(&build_menu()));
}

/// Add a `win.<name>` action that dispatches `action` and routes the result.
fn add_dispatch(
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    name: &str,
    action: Action,
) {
    let act = gio::SimpleAction::new(name, None);
    let fe = fe.clone();
    let s = shell.clone();
    let app = app.clone();
    act.connect_activate(move |_, _| {
        let effects = {
            let mut f = fe.borrow_mut();
            let e = futures::executor::block_on(f.core.dispatch_action(action.clone()));
            f.core.needs_render = true;
            e
        };
        apply_effects(&fe, effects, &app, &s.drawing);
        s.drawing.queue_draw();
    });
    shell.window.add_action(&act);
}

/// Open the `/`-command palette (enter `Mode::Command`; the dialog renders it).
fn add_command(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let act = gio::SimpleAction::new("command", None);
    let fe = fe.clone();
    let s = shell.clone();
    act.connect_activate(move |_, _| {
        {
            let mut f = fe.borrow_mut();
            if !matches!(f.core.mode, Mode::Command(_)) {
                f.core.mode = Mode::Command(CommandState::default());
                f.core.needs_render = true;
            }
        }
        s.drawing.queue_draw();
    });
    shell.window.add_action(&act);
}

fn add_switch_server(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let act = gio::SimpleAction::new("switch-server", None);
    let fe = fe.clone();
    let s = shell.clone();
    act.connect_activate(move |_, _| {
        {
            let mut f = fe.borrow_mut();
            f.core.apply_top_bar_action(TopBarAction::OpenServerPicker);
            f.core.needs_render = true;
        }
        s.drawing.queue_draw();
    });
    shell.window.add_action(&act);
}

fn add_toggle_sidebar(shell: &Rc<Shell>) {
    let act = gio::SimpleAction::new("toggle-sidebar", None);
    let s = shell.clone();
    act.connect_activate(move |_, _| {
        s.split.set_show_sidebar(!s.split.shows_sidebar());
    });
    shell.window.add_action(&act);
}

fn add_preferences(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let act = gio::SimpleAction::new("preferences", None);
    let fe = fe.clone();
    let s = shell.clone();
    act.connect_activate(move |_, _| {
        prefs::open(&fe, &s.drawing);
    });
    shell.window.add_action(&act);
}

fn add_help(shell: &Rc<Shell>) {
    let act = gio::SimpleAction::new("help", None);
    let s = shell.clone();
    act.connect_activate(move |_, _| {
        let builder = Builder::from_string(SHORTCUTS_XML);
        if let Some(win) = builder.object::<ShortcutsWindow>("shortcuts") {
            win.set_transient_for(Some(&s.window));
            win.set_modal(true);
            win.present();
        }
    });
    shell.window.add_action(&act);
}

fn add_quit(shell: &Rc<Shell>, app: &Application) {
    let act = gio::SimpleAction::new("quit", None);
    let app = app.clone();
    act.connect_activate(move |_, _| app.quit());
    shell.window.add_action(&act);
}

/// The primary (hamburger) menu model.
fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let s1 = gio::Menu::new();
    s1.append(Some("New Session"), Some("win.new-session"));
    s1.append(Some("New Pane"), Some("win.new-pane"));
    menu.append_section(None, &s1);

    let s2 = gio::Menu::new();
    s2.append(Some("Rename Session"), Some("win.rename-session"));
    s2.append(Some("Close Pane"), Some("win.close-pane"));
    s2.append(Some("Close Session"), Some("win.close-session"));
    menu.append_section(None, &s2);

    let s3 = gio::Menu::new();
    s3.append(Some("Switch Server…"), Some("win.switch-server"));
    s3.append(Some("Reconnect"), Some("win.reconnect"));
    s3.append(Some("Disconnect"), Some("win.disconnect"));
    let signals = gio::Menu::new();
    signals.append(Some("SIGTERM"), Some("win.signal-term"));
    signals.append(Some("SIGKILL"), Some("win.signal-kill"));
    signals.append(Some("SIGSTOP"), Some("win.signal-stop"));
    signals.append(Some("SIGCONT"), Some("win.signal-cont"));
    s3.append_submenu(Some("Send Signal"), &signals);
    menu.append_section(None, &s3);

    let s4 = gio::Menu::new();
    s4.append(Some("Lock Input"), Some("win.toggle-lock"));
    s4.append(Some("Performance HUD"), Some("win.toggle-hud"));
    s4.append(Some("Metrics"), Some("win.toggle-metrics"));
    menu.append_section(None, &s4);

    let s5 = gio::Menu::new();
    s5.append(Some("Preferences"), Some("win.preferences"));
    s5.append(Some("Keyboard Shortcuts"), Some("win.help"));
    s5.append(Some("Quit"), Some("win.quit"));
    menu.append_section(None, &s5);

    menu
}

/// The keyboard-shortcuts window, built from inline GtkBuilder XML (the standard
/// way `GtkShortcutsWindow` is described).
const SHORTCUTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">main</property>
        <property name="max-height">12</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Sessions</property>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;n</property><property name="title">New session</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;Tab</property><property name="title">Next session</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;Tab</property><property name="title">Previous session</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;1</property><property name="title">Jump to session 1–9</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">F2</property><property name="title">Rename session</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;w</property><property name="title">Close session</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Panes</property>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;t</property><property name="title">New pane</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;q</property><property name="title">Close pane</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;Right</property><property name="title">Next pane</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;Left</property><property name="title">Previous pane</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Terminal</property>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;c</property><property name="title">Copy</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;v</property><property name="title">Paste</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Shift&gt;Page_Up &lt;Shift&gt;Page_Down</property><property name="title">Scroll history</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;p</property><property name="title">Command palette</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">F9</property><property name="title">Toggle sidebar</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;l</property><property name="title">Lock input</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;&lt;Shift&gt;r</property><property name="title">Reconnect</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;comma</property><property name="title">Preferences</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="accelerator">&lt;Ctrl&gt;q</property><property name="title">Quit</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Look up the `Action` a dispatched accelerator maps to.
    fn action_for(name: &str) -> Option<Action> {
        dispatched_specs()
            .into_iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, a, _)| a)
    }

    #[test]
    fn signal_actions_use_posix_numbers() {
        assert_eq!(action_for("signal-term"), Some(Action::SendSignal(15)));
        assert_eq!(action_for("signal-kill"), Some(Action::SendSignal(9)));
        assert_eq!(action_for("signal-stop"), Some(Action::SendSignal(19)));
        assert_eq!(action_for("signal-cont"), Some(Action::SendSignal(18)));
    }

    #[test]
    fn jump_session_accelerator_is_zero_based() {
        assert_eq!(jump_action(1), Action::JumpToSession(0));
        assert_eq!(jump_action(3), Action::JumpToSession(2));
        assert_eq!(jump_action(9), Action::JumpToSession(8));
    }

    #[test]
    fn core_commands_are_registered_with_expected_actions() {
        assert_eq!(action_for("new-pane"), Some(Action::CreatePane));
        assert_eq!(action_for("close-pane"), Some(Action::ClosePane));
        assert_eq!(action_for("next-session"), Some(Action::NextSession));
        assert_eq!(action_for("copy"), Some(Action::CopySelection));
        assert_eq!(action_for("paste"), Some(Action::Paste));
        assert_eq!(action_for("reconnect"), Some(Action::Reconnect));
    }

    #[test]
    fn every_dispatched_action_has_a_unique_name() {
        let specs = dispatched_specs();
        let mut names: Vec<&str> = specs.iter().map(|(n, _, _)| *n).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            count,
            "duplicate action name in dispatched_specs"
        );
    }
}
