import CoreText
import Foundation
import KmuxBindings

/// Register the bundled symbol fallback font (Symbols Nerd Font Mono) with
/// CoreText so app chrome can reference it by name. This mirrors the GPU atlas's
/// embedded fallback and the GTK fontconfig registration (issue #145).
///
/// Registration is NOT what fixes the terminal grid: it only makes the font
/// discoverable by name, and a custom cascade list is silently ignored by
/// CoreText on the system monospaced font (the default). The grid instead
/// resolves fallback *per glyph* — `TerminalMetrics.drawFont(for:base:)` draws
/// a Powerline (U+E0B0–) or Nerd glyph the configured face lacks straight from
/// the `symbolFallbackDescriptor` font. Best-effort and run exactly once.
private let registerSymbolFallbackFontOnce: Void = {
    let bytes = symbolFallbackFontBytes()
    let data = Data(bytes)
    guard let provider = CGDataProvider(data: data as CFData),
        let cgFont = CGFont(provider)
    else {
        NSLog("kmux: symbol fallback font: could not construct CGFont")
        return
    }
    var error: Unmanaged<CFError>?
    if !CTFontManagerRegisterGraphicsFont(cgFont, &error) {
        // A duplicate registration (e.g. across windows in one process) is
        // harmless; log anything else.
        NSLog(
            "kmux: symbol fallback font registration failed: "
                + String(describing: error?.takeRetainedValue()))
    }
}()

/// A `CTFontDescriptor` for the bundled symbol fallback font, built once from the
/// embedded bytes (independent of registration / PostScript name). `nil` only if
/// the embedded font fails to parse. `TerminalMetrics` builds its `symbolFont`
/// from this and draws it per-glyph for the Powerline (U+E0B0–) and Nerd PUA
/// icons the configured font lacks.
let symbolFallbackDescriptor: CTFontDescriptor? = {
    let data = Data(symbolFallbackFontBytes())
    guard
        let descriptors = CTFontManagerCreateFontDescriptorsFromData(data as CFData)
            as? [CTFontDescriptor],
        let first = descriptors.first
    else {
        NSLog("kmux: symbol fallback font: could not build CTFontDescriptor")
        return nil
    }
    return first
}()

/// Idempotently register the bundled symbol fallback font with CoreText.
func registerSymbolFallbackFont() {
    _ = registerSymbolFallbackFontOnce
}
