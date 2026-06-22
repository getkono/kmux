import CoreText
import Foundation
import KmuxBindings

/// Register the bundled symbol fallback font (Symbols Nerd Font Mono) with
/// CoreText so the CoreText render path — and any Nerd-glyph icons in the app
/// chrome — resolve Powerline separators (U+E0B0–U+E0D7) and Private Use Area
/// icon glyphs the configured terminal font lacks (issue #145). This mirrors the
/// GPU atlas's embedded fallback and the GTK fontconfig registration.
///
/// Best-effort and run exactly once: CoreText's automatic font cascade picks the
/// registered font up for missing glyphs, so no per-draw change is needed.
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

/// Idempotently register the bundled symbol fallback font with CoreText.
func registerSymbolFallbackFont() {
    _ = registerSymbolFallbackFontOnce
}
