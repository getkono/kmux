# Command Mode

Command mode is a floating, `/`-prefixed input box for the kmux client that
supplements (and partially replaces) the Zellij-style key chord tree. It is
designed so new commands can be added by appending one struct to a static
table — no new `Action` variants, mode arms, or rendering glue per command.

## Activation

Command mode lives behind the existing Ctrl+G mode-select chord:

| Step | Key | Outcome |
|---|---|---|
| 1 | **Ctrl+G** | Enter `Mode::Select` (existing) |
| 2 | **Ctrl+/** _or_ **`/`** _or_ legacy `\x1f` byte | Enter `Mode::Command` |

The second-step accepts three encodings because Ctrl+/ has no portable
representation across terminal emulators:

* kitty / Ghostty / xterm with modify-other-keys: `Char('/')+CTRL`
* legacy xterm / iTerm: raw `\x1f` byte with no CTRL flag
* bare `/` is also accepted, which works in every terminal and matches the
  Slack/Minecraft convention the prefix glyph mimics.

`Esc` and `Ctrl+C` cancel and return to `Mode::Normal`. The buffer is
discarded; submitted commands are kept in a 100-deep ring (`App::command_history`).

## State

```rust
pub struct CommandState {
    pub buffer: String,            // does NOT include the leading '/'
    pub cursor: usize,              // byte offset into `buffer`
    pub selected: usize,            // highlighted hint index
    pub history_pos: Option<usize>, // None while editing freely
}
```

`CommandState` lives inside `Mode::Command(CommandState)` rather than on `App`
so it is automatically cleared when the mode is dropped. The leading `/` is
chrome — it is rendered by the overlay, never stored.

## Architecture

```
┌─────────────────┐     keys      ┌────────────────┐
│ resolve_command │ ────────────▶ │  Action::Cmd*  │ ───▶ App::dispatch_action
└─────────────────┘               └────────────────┘
        ▲
   Ctrl+G,/  (in Mode::Select)
        ▲
┌──────────────────────────────────────────────────────┐
│ submit (Enter)                                       │
│   buffer  ──▶ cmd::parse  ──▶ Parsed{spec, args}      │
│                                       │              │
│                                       ▼              │
│                              (CommandSpec::run)(app, args)
│                                       │              │
│                                       ▼              │
│                              CommandSuccess          │
│                              { Ok | Status | Quit |  │
│                                Reconnect | SwitchSrv}│
└──────────────────────────────────────────────────────┘
```

Files:

| File | Responsibility |
|---|---|
| `crates/kmux-app/src/mode/mod.rs` | `Mode::Command(CommandState)` and the `Action::Command*` editing variants |
| `crates/kmux-app/src/mode/resolve.rs` | `resolve_command`: keys → editing actions; chord activation in `resolve_mode_select` |
| `crates/kmux-app/src/cmd/spec.rs` | `CommandSpec`, `ArgSpec`, `Completer`, `CommandSuccess`, `CommandResult` |
| `crates/kmux-app/src/cmd/parse.rs` | tokenizer (with `"…"` / `'…'` quoting) + buffer→`Parsed` resolver |
| `crates/kmux-app/src/cmd/hint.rs` | `build_hints(&AppCore)`: pure ranked dropdown contents |
| `crates/kmux-app/src/cmd/registry.rs` | `static ALL: &[CommandSpec]` plus the command bodies (`fn(&mut AppCore, …)`) |
| `crates/kmux-app/src/cmd/exec.rs` | `run(&mut AppCore, buffer)` glue between submit and registry |
| `crates/kmux-app/src/core/dispatch.rs` | `AppCore::dispatch_action` (the unified action handler) and command-edit arms |
| `crates/kmux-gtk/src/imp/` | floating overlay rendering (GTK render leaf) |

The command palette, mode model, and action dispatch are all frontend-agnostic
and live in `kmux-app` (see [architecture-frontend.md](architecture-frontend.md));
only the overlay's rendering stays in the frontend.

## Refactor seam: `dispatch_action`

A key resolves into an `Action`; applying that `Action` is the single source of
truth in `AppCore::dispatch_action(action)` (toolkit-agnostic, in
`kmux-app/src/core/dispatch.rs`). The key path and the command palette both
funnel through it: a frontend converts its toolkit key event, calls
`mode::resolve`, then `core.dispatch_action`; the command palette's
`CommandSubmit` arm runs `cmd::exec::run`, whose handlers mutate the same
`AppCore`.

This was a deliberate de-duplication ("ruthless refactor, no drift"): a
parallel command dispatcher would have invited two definitions of "what
Quit/CreateSession/etc. mean" that drift over time.

Two arms are *not* in the core dispatch because they require toolkit I/O:
`Action::ForwardKey` (the frontend encodes the keystroke to PTY bytes under the
live terminal-mode state) and clipboard copy/paste (emitted as
`KeyResult::CopyToClipboard` / `RequestPaste` effects the frontend performs).
`dispatch_action` therefore takes no raw key event — the frontend handles
`ForwardKey` before calling it.

## Commands

The full list lives in `cmd::registry::ALL`. Categories:

* **Client controls** — `quit`, `redraw`, `help`, `hud`, `metrics`, `lock`,
  `snapshot on|off`, `theme <name>`, `clear-history`.
* **Connection** — `disconnect`, `reconnect`, `server` (open picker), `local`
  (switch to UDS).
* **Sessions** — `session new [name] [cwd]`, `session close` (opens confirmation),
  `session rename <name>`, `session next`, `session prev`,
  `session switch <name|id|index>`, `session list`. All have `s …` aliases.
* **Panes** — `pane new`, `pane close`, `pane next`, `pane prev`. `p …` aliases.
* **Signals** — `signal <kill|term|stop|cont>`.
* **Daemon** — `daemon status`, `daemon ping`. `d …` aliases.

Aliases are resolved during parsing; the registry's canonical name is the only
form that appears in help, status messages, and history.

## Submit semantics

Pressing **Enter** runs the typed buffer, with one user-friendly fallback: if
the buffer doesn't parse cleanly **but** there is a highlighted hint, the
hint's replacement is applied first (equivalent to pressing Tab + Enter). This
is what makes `/qu` + Enter run `/quit` and `/sess` + Enter run
`/session new` without forcing the user to press Tab.

History records the *typed* form (so ↑/↓ recall what the user actually
pressed), not the auto-completed expansion.

## Connection feedback

Commands that talk to the daemon (`session …`, `pane …`, `signal`, `daemon
ping`) check `SessionManager::is_connected()` first and emit a status message
when there's no live connection — otherwise the message would be silently
dropped on the wire and the user would see nothing happen. Locally-scoped
commands (`hud`, `metrics`, `theme`, `redraw`, `clear-history`, `help`, …)
work whether or not a daemon is connected.

## Hints

`cmd::hint::build_hints(&App)` is recomputed on every render. It is **pure**
relative to `App`: it reads only the `Mode::Command` buffer, the registry, and
contextual values (currently active sessions for the `Sessions` completer).
Nothing is cached on `App`.

The function:

1. Tokenizes the buffer (same tokenizer as `parse::parse`).
2. Greedily matches the longest registered command name or alias.
3. **Either** suggests further command names (when no command has been locked
   in yet — i.e. typed prefix is still ambiguous), **or** suggests values for
   the active argument's `Completer`.
4. Caps the result at `MAX_HINTS` (8).

Tab applies the highlighted hint, replacing the trailing token in-place. ↑/↓
move the highlight; Enter submits.

## Adding a command

Three steps:

1. Write the body in `cmd/registry.rs`:
   ```rust
   fn cmd_my_thing(app: &mut App, args: &[String]) -> CommandResult {
       let name = require_arg(args, 0, "name")?;
       app.do_my_thing(name);
       Ok(CommandSuccess::Status(format!("did {name}")))
   }
   ```
2. Append a `CommandSpec` to `ALL`:
   ```rust
   CommandSpec {
       name: "my-thing",
       aliases: &["mt"],
       summary: "Do my thing",
       args: &[ArgSpec { name: "name", required: true, completer: Completer::None }],
       run: cmd_my_thing,
   },
   ```
3. (Optional) Add a hint test in `cmd::hint::tests` if the new command has a
   non-trivial completer, or a parse test in `cmd::parse::tests` if its arg
   shape is unusual.

The registry's `no_duplicate_canonical_names`, `no_duplicate_aliases`, and
`usage_strings_well_formed` tests run on every `cargo test`, so name conflicts
are caught at CI time rather than at runtime.

## Control flow back to the event loop

Most commands return `CommandSuccess::Ok` (no follow-up) or
`CommandSuccess::Status(s)` (toast in the bottom bar). Two variants escape
back to `App::run`:

| Variant | Maps to `KeyResult` | Used by |
|---|---|---|
| `Quit` | `KeyResult::Quit` | `/quit` |
| `Reconnect` | `KeyResult::Reconnect` | `/reconnect` |

`exec::Outcome` mirrors these so the submit handler in `dispatch_action` can
funnel them up unchanged. (The old `SwitchServer` escape was retired with the
`ServerPicker` whole-client reconnect path; remotes are now federated in place —
`/connect` / `/disconnect-remote` go through the launcher's `open_peer`/`close_peer`,
not a reconnect.)

## Future work

* `/connect <host[:port]> [token]` — Direct-mode (token-authenticated) federation
  still needs token capture; `/connect <user@host>` already federates an SSH remote
  into the local hub via `open_peer` (`cmd_connect`).
* Real `Ping`/`Pong` round-trip — `daemon ping` currently piggybacks on
  `request_session_list`. Adding a dedicated public method on
  `SessionManager` would let the command surface RTT.
* Up/Down history recall — `command_history` and `CommandState::history_pos`
  are wired up but the editor actions don't yet bind ↑/↓ to history when the
  hint dropdown is empty. Follow-up PR.
