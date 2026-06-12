# Themes

kmux supports colour themes for the client. Six themes are built in and
compiled into the binary; custom themes can be added without rebuilding.

For fonts and cell geometry (font family, size, OpenType features, …), see
[appearance.md](appearance.md).

## Built-in themes

| Name | Description |
|------|-------------|
| `catppuccin-macchiato` | Catppuccin Macchiato — **default** |
| `catppuccin-latte` | Catppuccin Latte (light) |
| `catppuccin-frappe` | Catppuccin Frappé |
| `catppuccin-mocha` | Catppuccin Mocha |
| `dracula` | Dracula |
| `one-dark` | One Dark |

## Selecting a theme

**CLI flag** (takes highest priority):

```sh
kmux --theme dracula
kmux --theme catppuccin-latte
```

**Config file** (`~/.config/kmux/config.toml`):

```toml
theme = "catppuccin-mocha"
```

If `--theme` is given it overrides the config file. If neither is set, the
default theme (`catppuccin-macchiato`) is used. If the named theme cannot be
found, an error is logged and the default is used as a fallback.

## Custom themes

Place a `.toml` file in `~/.config/kmux/themes/` and reference it by name
(without the `.toml` extension):

```sh
# Create a custom theme
mkdir -p ~/.config/kmux/themes
cp /path/to/my-theme.toml ~/.config/kmux/themes/my-theme.toml

# Use it
kmux --theme my-theme
# or set it permanently in ~/.config/kmux/config.toml:
# theme = "my-theme"
```

## Theme file schema

Every theme file is a flat TOML document. All fields are required except
`cursor_bg` / `cursor_fg`, which are optional. Colors must be 6-digit hex
strings prefixed with `#`.

```toml
name      = "my-theme"      # human-readable identifier (required)
bg        = "#24273a"       # main background
fg        = "#cad3f5"       # default foreground text
fg_dim    = "#6e738d"       # dimmed/secondary text, inactive borders
accent    = "#8aadf4"       # highlights, active borders, badges
green     = "#a6da95"       # success, normal mode indicator, HUD metrics
red       = "#ed8796"       # errors, locked mode, close-session prompt
yellow    = "#eed49f"       # scroll indicator, warnings in HUD
purple    = "#c6a0f6"       # session mode indicator
orange    = "#f5a97f"       # rename mode indicator
status_bg = "#1e2030"       # background of the status and session bars

# Optional. The inner-pane cursor is rendered by kmux itself (painted in-cell
# rather than delegating to a host terminal's hardware cursor), so these control
# how it looks regardless of any host terminal cursor settings.
# cursor_bg defaults to `fg`, cursor_fg defaults to `bg`.
cursor_bg = "#cad3f5"       # Block cursor bg + Bar/Underline glyph color
cursor_fg = "#24273a"       # text drawn on top of the Block cursor
```

The canonical theme definitions are in the [`themes/`](../themes/) directory at
the repository root, which you can use as reference when authoring custom themes.

## Implementation

Theme parsing and resolution are frontend-agnostic and live in `kmux-app`
(`kmux_app::theme` and `kmux_app::config::resolve_theme`). The palette is stored
as a toolkit-neutral `Rgb` triple — `kmux_app::theme::Rgb` — and the active
palette lives on `AppCore.palette`. Each frontend converts to its own color type
at the render boundary (the GTK frontend to cairo colors, the Swift app to
`NSColor` via FFI). See [architecture-frontend.md](architecture-frontend.md).
