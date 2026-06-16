//! Theme the native chrome from the active `kmux_app` palette.
//!
//! The chrome (header bar, tabs, sidebar, dialogs) uses default libadwaita
//! styling; we only override libadwaita's *accent* named colors from the kmux
//! palette so the whole UI tracks the `/theme` command, and style the
//! performance HUD ticker. The terminal grid itself is painted directly from
//! the palette in `render.rs` (not via CSS). Reloaded by the pump on `/theme`.

use gtk4::gdk;
use kmux_app::theme::{Rgb, Theme};

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Build the stylesheet for `palette`.
pub fn stylesheet(p: &Theme) -> String {
    let accent = hex(p.accent);
    let accent_fg = hex(p.bg);
    let green = hex(p.green);

    format!(
        "/* Route the kmux accent into libadwaita's named colors so the native\n\
   chrome (header, tabs, sidebar, dialogs) follows the active theme. */\n\
@define-color accent_color {accent};\n\
@define-color accent_bg_color {accent};\n\
@define-color accent_fg_color {accent_fg};\n\
\n\
/* Performance HUD ticker, on top of the libadwaita .osd panel. */\n\
.kmux-hud {{ padding: 6px 10px; }}\n\
.kmux-hud-line {{ color: {green}; font-family: monospace; }}\n\
/* Render-debug overlay (top-Start), distinct from the perf HUD (top-End). */\n\
.kmux-render-debug {{ padding: 6px 10px; }}\n\
.kmux-render-debug-line {{ color: {accent}; font-family: monospace; }}\n"
    )
}

/// A shared `CssProvider` so the stylesheet can be regenerated on theme change.
pub fn install(display: &gdk::Display, palette: &Theme) -> gtk4::CssProvider {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&stylesheet(palette));
    gtk4::style_context_add_provider_for_display(
        display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    provider
}

/// Re-render the stylesheet onto an existing provider (e.g. after `/theme`).
pub fn reload(provider: &gtk4::CssProvider, palette: &Theme) {
    provider.load_from_data(&stylesheet(palette));
}
