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
}
