# Appearance (fonts & cell geometry)

kmux lets the GUI clients configure the terminal font and cell geometry from
`~/.config/kmux/config.toml`. The settings track [Ghostty's config
reference][ghostty] key names so a Ghostty config is mostly copy-pasteable, and
they apply to both GUI frontends (GTK on Linux/macOS, the native SwiftUI app on
macOS). Colors are configured separately — see [themes.md](themes.md).

Appearance is a **client-side** concern: it is resolved from the config file and
applied at the render leaf, and is never sent over the wire to `kmuxd`.

## Supported keys

All keys are optional and live at the top level of `config.toml`. Each accepts
both the canonical kmux `snake_case` name and the Ghostty-style `kebab-case`
alias (e.g. `font_family` or `font-family`).

| Key | Ghostty alias | Type | Description |
|-----|---------------|------|-------------|
| `font_family` | `font-family` | string | Primary (regular) font family. |
| `font_family_bold` | `font-family-bold` | string | Face for bold text (synthesized from `font_family` if unset). |
| `font_family_italic` | `font-family-italic` | string | Face for italic text. |
| `font_family_bold_italic` | `font-family-bold-italic` | string | Face for bold-italic text. |
| `font_size` | `font-size` | number | Font size in points. |
| `font_style` | `font-style` | string | Named style/face for the regular font (e.g. `"Medium"`). |
| `font_feature` | `font-feature` | string array | OpenType feature settings (see below). |
| `adjust_cell_width` | `adjust-cell-width` | string | Cell-width tweak: pixels (`"2"`) or percent (`"10%"`). |
| `adjust_cell_height` | `adjust-cell-height` | string | Cell-height tweak (same form). |

### Example

```toml
font-family = "JetBrains Mono"
font-family-bold = "JetBrains Mono ExtraBold"
font-size = 14
font-feature = ["zero", "ss01", "-calt"]
adjust-cell-height = "8%"
```

### OpenType features

`font_feature` is a list of harfbuzz-style feature tokens:

- `"ss01"` / `"+ss01"` — enable a feature
- `"-liga"` / `"liga=0"` — disable a feature
- `"cv01=2"` — select a specific alternate

> **Ligatures are not rendered.** kmux paints the grid **one cell at a time**, so
> multi-glyph features that span cells (`liga`, `calt`) cannot fire — there is no
> shaping run for them to apply to. *Per-glyph* features such as stylistic sets
> (`ss01`…), `zero`, `onum`, and small caps do work. On macOS, OpenType tags that
> CoreText doesn't recognize are ignored.

## Precedence

For the font family and size, the following sources are consulted in order:

1. the structured `font_family` / `font_size` keys (per field);
2. the legacy `font` key (a Pango font-description string, e.g.
   `"JetBrains Mono 12"`), or the `--font` CLI flag — kept for backward
   compatibility, it seeds the family + size when the structured keys are absent;
3. the built-in defaults (`monospace`, 11pt).

The remaining keys (variant families, `font_style`, `font_feature`,
`adjust_cell_*`) are config-file-only and have no legacy or CLI equivalent.

> The legacy `font` key / `--font` flag is **deprecated** in favor of
> `font_family` + `font_size`, but still honored. The GTK preferences window's
> "Font" entry continues to edit it; any structured keys set in `config.toml`
> override it.

## Implementation

Resolution is frontend-agnostic and lives in `kmux-app`
(`kmux_app::appearance::Appearance` + `kmux_app::config::resolve_appearance`).
The resolved `Appearance` is stored on `AppCore.appearance` alongside the color
`palette`. Each frontend converts it to its own font/metrics types at the render
leaf:

- **GTK** (`kmux-gtk`): a `pango::FontDescription` per style + a
  `pango::AttrFontFeatures` attribute list, in `render::Metrics`.
- **Swift** (`kmux-swift`): an `NSFont` per style + CoreText feature settings, in
  `TerminalMetrics`. The `Appearance` crosses the `kmux-ffi` C ABI as
  `FfiAppearance` (read via `KmuxDriver::appearance()`).

See [architecture-frontend.md](architecture-frontend.md).

[ghostty]: https://ghostty.org/docs/config/reference
