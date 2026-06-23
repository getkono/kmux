import AppKit
import CoreText

import KmuxBindings

/// Monospaced cell geometry + per-style fonts derived from the configured
/// appearance — the SwiftUI/AppKit analog of kmux-gtk's `render::Metrics`
/// (Pango). Drives both the grid render and the content-size → cols/rows mapping
/// reported to the driver. Built from the toolkit-neutral `FfiAppearance`
/// (font family/size/style, OpenType features, cell adjustments) the driver
/// resolves from `config.toml`.
struct TerminalMetrics: Equatable {
    /// Regular face (with OpenType features applied).
    let font: NSFont
    /// Bold face: an explicit `font-family-bold`, else synthetic bold of `font`.
    let fontBold: NSFont
    /// Italic face: an explicit `font-family-italic`, else synthetic italic.
    let fontItalic: NSFont
    /// Bold-italic face: an explicit family, else synthetic bold+italic.
    let fontBoldItalic: NSFont
    /// Advance width of a monospaced cell (after `adjust-cell-width`, ceil'd).
    let cellWidth: CGFloat
    /// Line height = ascent + descent (after `adjust-cell-height`, ceil'd).
    let cellHeight: CGFloat
    /// Baseline offset from the top of a cell.
    let ascent: CGFloat

    /// Build the metrics from a resolved `FfiAppearance`.
    init(appearance: FfiAppearance) {
        let size = CGFloat(appearance.sizePt)
        let features = appearance.features
        let rawBase = TerminalMetrics.resolveBase(
            family: appearance.family, size: size, style: appearance.style)

        let regular = TerminalMetrics.applyFeatures(rawBase, features: features)
        self.font = TerminalMetrics.withSymbolFallback(regular)
        self.fontBold = TerminalMetrics.withSymbolFallback(TerminalMetrics.applyFeatures(
            TerminalMetrics.face(rawBase, family: appearance.familyBold, size: size, bold: true, italic: false),
            features: features))
        self.fontItalic = TerminalMetrics.withSymbolFallback(TerminalMetrics.applyFeatures(
            TerminalMetrics.face(rawBase, family: appearance.familyItalic, size: size, bold: false, italic: true),
            features: features))
        self.fontBoldItalic = TerminalMetrics.withSymbolFallback(TerminalMetrics.applyFeatures(
            TerminalMetrics.face(rawBase, family: appearance.familyBoldItalic, size: size, bold: true, italic: true),
            features: features))

        self.ascent = regular.ascender
        let descent = -regular.descender  // descender is negative
        self.cellHeight = max(
            1, TerminalMetrics.adjust(regular.ascender + descent, appearance.cellHeightAdjust).rounded(.up))
        let advance = ("M" as NSString).size(withAttributes: [.font: regular]).width
        self.cellWidth = max(1, TerminalMetrics.adjust(advance, appearance.cellWidthAdjust).rounded(.up))
    }

    /// Build metrics from a single plain `NSFont` (no OpenType features, no
    /// explicit variant families; bold/italic are synthesized). Used as a simple
    /// fallback and in tests.
    init(font: NSFont) {
        let size = font.pointSize
        self.font = TerminalMetrics.withSymbolFallback(font)
        self.fontBold = TerminalMetrics.withSymbolFallback(
            TerminalMetrics.face(font, family: nil, size: size, bold: true, italic: false))
        self.fontItalic = TerminalMetrics.withSymbolFallback(
            TerminalMetrics.face(font, family: nil, size: size, bold: false, italic: true))
        self.fontBoldItalic = TerminalMetrics.withSymbolFallback(
            TerminalMetrics.face(font, family: nil, size: size, bold: true, italic: true))
        self.ascent = font.ascender
        let descent = -font.descender
        self.cellHeight = max(1, (font.ascender + descent).rounded(.up))
        let advance = ("M" as NSString).size(withAttributes: [.font: font]).width
        self.cellWidth = max(1, advance.rounded(.up))
    }

    static func == (lhs: TerminalMetrics, rhs: TerminalMetrics) -> Bool {
        lhs.font == rhs.font && lhs.fontBold == rhs.fontBold && lhs.fontItalic == rhs.fontItalic
            && lhs.fontBoldItalic == rhs.fontBoldItalic && lhs.cellWidth == rhs.cellWidth
            && lhs.cellHeight == rhs.cellHeight
    }

    /// Default monospaced font (SF Mono via the system monospaced face). Used as
    /// the fallback when a configured family doesn't resolve.
    static func defaultFont(size: CGFloat = 13) -> NSFont {
        NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
    }

    /// Columns/rows that fit a content area of `width` × `height` logical points.
    func colsRows(width: CGFloat, height: CGFloat) -> (cols: UInt16, rows: UInt16) {
        let cols = max(1, Int((width / cellWidth).rounded(.down)))
        let rows = max(1, Int((height / cellHeight).rounded(.down)))
        return (UInt16(min(cols, Int(UInt16.max))), UInt16(min(rows, Int(UInt16.max))))
    }

    // MARK: - Font construction

    /// Resolve the regular base face from a family name (+ optional named style),
    /// falling back to the system monospaced face so the grid never renders with
    /// a proportional fallback.
    private static func resolveBase(family: String, size: CGFloat, style: String?) -> NSFont {
        guard let base = NSFont(name: family, size: size) else {
            return defaultFont(size: size)
        }
        if let style = style, !style.isEmpty {
            let desc = base.fontDescriptor.addingAttributes([.face: style])
            if let styled = NSFont(descriptor: desc, size: size) { return styled }
        }
        return base
    }

    /// Resolve a bold/italic/bold-italic face: an explicit variant `family` when
    /// configured, otherwise the base face with synthetic symbolic traits.
    private static func face(
        _ rawBase: NSFont, family: String?, size: CGFloat, bold: Bool, italic: Bool
    ) -> NSFont {
        if let family = family, !family.isEmpty, let named = NSFont(name: family, size: size) {
            return named
        }
        if !bold && !italic { return rawBase }
        var traits: NSFontDescriptor.SymbolicTraits = []
        if bold { traits.insert(.bold) }
        if italic { traits.insert(.italic) }
        let desc = rawBase.fontDescriptor.withSymbolicTraits(traits)
        return NSFont(descriptor: desc, size: size) ?? rawBase
    }

    /// Apply OpenType `features` (harfbuzz `"tag=value"` strings) to `font` via a
    /// CoreText font descriptor. Per-glyph features (stylistic sets `ss0x`,
    /// `zero`, …) work; cross-cell ligatures don't, since the grid draws each
    /// cell separately. Unparseable tags are skipped.
    private static func applyFeatures(_ font: NSFont, features: [String]) -> NSFont {
        guard !features.isEmpty else { return font }
        var settings: [[CFString: Any]] = []
        for token in features {
            guard let (tag, value) = parseFeature(token) else { continue }
            settings.append([
                kCTFontOpenTypeFeatureTag: tag,
                kCTFontOpenTypeFeatureValue: value,
            ])
        }
        guard !settings.isEmpty else { return font }
        let desc = font.fontDescriptor.addingAttributes([.featureSettings: settings])
        return NSFont(descriptor: desc, size: font.pointSize) ?? font
    }

    /// Add the bundled symbol fallback font to `font`'s CoreText cascade list so
    /// `NSAttributedString.draw()` substitutes it for the Powerline (U+E0B0–) and
    /// Nerd Private Use Area glyphs the configured face lacks (issue #145).
    /// Registering the font (`FontFallback`) only makes it discoverable by name;
    /// the cascade list is what makes CoreText actually fall back to it. The
    /// symbol descriptor is prepended to the font's *default* cascade list so PUA
    /// glyphs resolve to it while the system's emoji/CJK fallbacks are preserved.
    /// Returns `font` unchanged if the fallback descriptor or rebuilt face is
    /// unavailable.
    private static func withSymbolFallback(_ font: NSFont) -> NSFont {
        guard let symbol = symbolFallbackDescriptor else { return font }
        let defaults =
            (CTFontCopyDefaultCascadeListForLanguages(font as CTFont, nil) as? [CTFontDescriptor])
            ?? []
        let cascade = [symbol] + defaults
        let key = NSFontDescriptor.AttributeName(kCTFontCascadeListAttribute as String)
        let desc = font.fontDescriptor.addingAttributes([key: cascade])
        return NSFont(descriptor: desc, size: font.pointSize) ?? font
    }

    /// Parse a `"tag=value"` (or bare `"tag"`) feature setting into a 4-char
    /// OpenType tag + integer value. Returns `nil` for an empty tag.
    private static func parseFeature(_ token: String) -> (tag: String, value: Int)? {
        let parts = token.split(separator: "=", maxSplits: 1)
        let tag = parts.first.map(String.init)?.trimmingCharacters(in: .whitespaces) ?? ""
        guard !tag.isEmpty else { return nil }
        let value = parts.count > 1 ? Int(parts[1].trimmingCharacters(in: .whitespaces)) ?? 1 : 1
        return (tag, value)
    }

    /// Apply a cell-dimension adjustment to a measured base value.
    private static func adjust(_ base: CGFloat, _ adjust: FfiCellAdjust) -> CGFloat {
        switch adjust.kind {
        case .pixels: return base + CGFloat(adjust.value)
        case .percent: return base * (1 + CGFloat(adjust.value) / 100)
        }
    }
}
