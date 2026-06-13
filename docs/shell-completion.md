# Shell completion

`kmux` ships **dynamic** tab-completion: completions are computed by the `kmux`
binary itself at completion time, so they always match the installed CLI and can
include *live* runtime values — `hosts.toml` aliases, theme names on disk, and
the local daemon's current sessions. There are **no completion files to install
or regenerate**; you only add one line to your shell config.

## Setup

Add the line for your shell, then restart the shell (or re-source the file):

| Shell | File | Line |
| ----- | ---- | ---- |
| bash | `~/.bashrc` | `source <(COMPLETE=bash kmux)` |
| zsh | `~/.zshrc` | `source <(COMPLETE=zsh kmux)` |
| fish | `~/.config/fish/config.fish` | `COMPLETE=fish kmux \| source` |

Elvish and PowerShell are also supported via the same mechanism — use
`COMPLETE=elvish kmux` / `COMPLETE=powershell kmux`.

`just install` prints these same instructions at the end of a successful install.

## What completes

- **Subcommands and flags** — `daemon` (`start`/`stop`/`status`/`restart`/`logs`/
  `sessions`), `ls` / `list-sessions`, and every flag — derived automatically
  from the CLI definition.
- **`--format`** → `table` / `json`; **`--cursor-blink`** → `true` / `false`
  (enum values, free from the derive).
- **`--theme`** → the built-in themes plus any custom theme in
  `~/.config/kmux/themes/*.toml`.
- **the `server` positional** (default connect and `ls`) → the alias keys in
  `~/.config/kmux/hosts.toml`.
- **`--session`** → the local daemon's live session display names and word_ids.

## How it works

When you press <kbd>Tab</kbd>, the shell re-invokes `kmux` with the `COMPLETE`
environment variable set. The very first thing
[`run_cli`](../crates/kmux-app/src/launch.rs) does is hand off to
clap_complete's `CompleteEnv`, which produces the candidates and exits the
process *before* any logging, daemon connection, or GUI handoff happens — so
completion is fast and never launches the app. The dynamic value sources live in
[`crates/kmux-app/src/completion.rs`](../crates/kmux-app/src/completion.rs); each
degrades to "no candidates" on any error, so a missing config or a stopped
daemon never breaks completion (the `--session` query is additionally bounded by
a short timeout).

## Troubleshooting

- **No alias / theme suggestions:** confirm `~/.config/kmux/hosts.toml` and
  `~/.config/kmux/themes/` exist and are readable (`XDG_CONFIG_HOME` is honored
  if set).
- **No session suggestions:** the local daemon must be running
  (`kmux daemon status`); session completion queries it live.
- **Nothing completes at all:** make sure the setup line ran in your *current*
  shell (`type _clap_complete_kmux` in bash/zsh should print a function), and
  that `kmux` is on `PATH`.
