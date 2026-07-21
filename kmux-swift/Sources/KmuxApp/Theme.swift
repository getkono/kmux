import AppKit
import SwiftUI

import KmuxBindings

// Bridge the toolkit-neutral RGB palette (`FfiTheme` / `FfiColor`, resolved in
// Rust) to AppKit colors at the render leaf — the SwiftUI analog of kmux-gtk's
// cairo `set_source_rgb` / `css.rs`.

extension FfiColor {
    var cgColor: CGColor {
        CGColor(
            srgbRed: CGFloat(r) / 255.0,
            green: CGFloat(g) / 255.0,
            blue: CGFloat(b) / 255.0,
            alpha: 1.0
        )
    }

    var nsColor: NSColor {
        NSColor(
            srgbRed: CGFloat(r) / 255.0,
            green: CGFloat(g) / 255.0,
            blue: CGFloat(b) / 255.0,
            alpha: 1.0
        )
    }

    var color: Color {
        Color(.sRGB, red: Double(r) / 255.0, green: Double(g) / 255.0, blue: Double(b) / 255.0)
    }
}

extension FfiTheme {
    /// Whether the background is dark (drives SwiftUI's light/dark appearance,
    /// mirroring kmux-gtk's `scheme_for` luminance check).
    var isDark: Bool {
        let lum = 0.299 * Double(bg.r) + 0.587 * Double(bg.g) + 0.114 * Double(bg.b)
        return lum < 128.0
    }

    /// Semantic application chrome derived from the terminal theme. Keeping
    /// these roles here prevents individual views from inventing unrelated
    /// opacities while still allowing the terminal palette to set the mood.
    var chrome: ChromePalette { ChromePalette(theme: self) }
}

/// A deliberately small design system for the native shell. The terminal keeps
/// its exact configured colors; surrounding controls use clamped semantic
/// colors so unusual terminal themes cannot make navigation illegible.
struct ChromePalette {
    let background: Color
    let raised: Color
    let hover: Color
    let selection: Color
    let border: Color
    let primaryText: Color
    let accent: Color

    init(theme: FfiTheme) {
        let dark = theme.isDark
        background = dark ? Color(white: 0.075) : Color(white: 0.955)
        raised = dark ? Color(white: 0.115) : .white
        hover = dark ? Color.white.opacity(0.065) : Color.black.opacity(0.055)
        selection = theme.accent.color.opacity(dark ? 0.22 : 0.14)
        border = dark ? Color.white.opacity(0.09) : Color.black.opacity(0.10)
        primaryText = dark ? Color.white.opacity(0.92) : Color.black.opacity(0.86)
        accent = theme.accent.color
    }
}

enum ChromeMetrics {
    static let smallRadius: CGFloat = 6
    static let radius: CGFloat = 9
    static let largeRadius: CGFloat = 14
}
