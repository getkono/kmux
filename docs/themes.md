# Themes

kmux supports colour themes for the TUI client. Six themes are built in and
compiled into the binary; custom themes can be added without rebuilding.

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

Every theme file is a flat TOML document. All fields are required. Colors must
be 6-digit hex strings prefixed with `#`.

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

# Optional. The inner-pane cursor is rendered in-cell (kmux does not
# delegate to the host terminal's hardware cursor), so these colors
# control how it looks regardless of host terminal cursor settings.
# cursor_bg defaults to `fg`, cursor_fg defaults to `bg`.
cursor_bg = "#cad3f5"       # Block cursor bg + Bar/Underline glyph color
cursor_fg = "#24273a"       # text on top of the Block cursor
```

The canonical theme definitions are in the [`themes/`](../themes/) directory at
the repository root, which you can use as reference when authoring custom themes.
