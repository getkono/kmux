//! Generate a GTK CSS stylesheet from the active `kmux_app` palette and install
//! it on the display. The structural classes are attached by `chrome.rs` and
//! the overlay widgets; this maps the toolkit-neutral palette onto them so the
//! GUI tracks the `/theme` command and built-in themes. libadwaita window
//! styling + reload-on-theme-change build on this in a later pass.

use gtk4::gdk;
use kmux_app::theme::{Rgb, Theme};

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Build the stylesheet for `palette`.
pub fn stylesheet(p: &Theme) -> String {
    let bg = hex(p.bg);
    let fg = hex(p.fg);
    let fg_dim = hex(p.fg_dim);
    let accent = hex(p.accent);
    let green = hex(p.green);
    let red = hex(p.red);
    let yellow = hex(p.yellow);
    let purple = hex(p.purple);
    let orange = hex(p.orange);
    let status_bg = hex(p.status_bg);

    // Badges are flat (no GTK button chrome) so they read as terminal segments.
    format!(
        "
.kmux-session-bar, .kmux-status-bar {{ background: {status_bg}; color: {fg}; }}
.kmux-hint-bar {{ background: {bg}; color: {fg}; }}
.kmux-badge {{
  background: {status_bg}; color: {fg};
  border: none; box-shadow: none; outline: none;
  min-height: 0; min-width: 0; padding: 1px 2px; margin: 0;
  border-radius: 0; font-weight: bold;
}}
.kmux-server {{ background: {purple}; color: {bg}; }}
.kmux-session {{ background: {accent}; color: {bg}; }}
.kmux-pane {{ background: {status_bg}; color: {fg}; font-weight: normal; }}
.kmux-pane.active {{ background: {green}; color: {bg}; font-weight: bold; }}
.kmux-add-pane {{ color: {fg_dim}; }}
.kmux-pane-empty {{ color: {fg_dim}; }}
.kmux-conn-connected {{ background: {green}; color: {bg}; }}
.kmux-conn-connecting {{ background: {yellow}; color: {bg}; }}
.kmux-conn-disconnected {{ background: {red}; color: {bg}; }}
.kmux-conn-idle {{ background: {fg_dim}; color: {bg}; }}
.kmux-hostport {{ color: {green}; }}
.kmux-sessions {{ color: {fg}; }}
.kmux-locked {{ color: {red}; font-weight: bold; }}
.kmux-dims, .kmux-status-msg {{ color: {fg_dim}; }}
.kmux-debug-badge {{ background: {orange}; color: {bg}; font-weight: bold; }}
.kmux-mode-badge {{ background: {accent}; color: {bg}; font-weight: bold; padding: 1px 4px; }}
.kmux-hint-key {{ background: {fg_dim}; color: {bg}; font-weight: bold; }}
.kmux-hint-desc {{ color: {fg}; }}
.kmux-overlay {{
  background: {bg}; color: {fg};
  border: 2px solid {accent}; border-radius: 8px;
  padding: 16px; margin: 24px;
}}
.kmux-overlay-title {{ color: {accent}; font-weight: bold; }}
.kmux-overlay-dim {{ color: {fg_dim}; }}
.kmux-overlay-error {{ color: {red}; }}
.kmux-overlay-row {{ padding: 1px 6px; border-radius: 4px; }}
.kmux-overlay-row.selected {{ background: {accent}; color: {bg}; }}
.kmux-overlay-caret {{ color: {accent}; font-weight: bold; }}
.kmux-hud {{ padding: 6px 10px; }}
.kmux-hud-line {{ color: {green}; }}
"
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
