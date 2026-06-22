# Bundled fonts

## SymbolsNerdFontMono-Regular.ttf

Symbols Nerd Font Mono, from the **Nerd Fonts** project
(<https://github.com/ryanoasis/nerd-fonts>). It is the "Symbols Only" aggregate
that bundles the icon glyph sets (Powerline, Font Awesome, Devicons, Octicons,
Material, Weather, Seti-UI, …) without any Latin text glyphs, so it serves purely
as a fallback for code points the user's configured terminal font lacks
(Powerline separators U+E0B0–E0D7 and the Private Use Area icon ranges).

kmux embeds it (`crate::fallback`) and rasterizes from it when the primary face
has no glyph for a character, and registers it with the OS font systems
(fontconfig on GTK, CoreText on macOS) so the CPU render paths resolve the same
glyphs.

- Project: Nerd Fonts — <https://github.com/ryanoasis/nerd-fonts>
- License: MIT (the Nerd Fonts project). The constituent icon sets retain their
  own upstream licenses (MIT / SIL OFL / CC-BY), enumerated in the Nerd Fonts
  repository's `LICENSE` and per-glyph attribution.
- Redistribution: permitted, including bundling inside an application binary.
