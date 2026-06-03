import AppKit

/// Monospaced cell geometry derived from the configured font — the SwiftUI/AppKit
/// analog of kmux-gtk's `render::Metrics` (Pango). Drives both the grid render
/// and the content-size → cols/rows mapping reported to the driver.
struct TerminalMetrics: Equatable {
    let font: NSFont
    /// Advance width of a monospaced cell (ceil'd to a whole pixel).
    let cellWidth: CGFloat
    /// Line height = ascent + descent (ceil'd), matching the GTK frontend.
    let cellHeight: CGFloat
    /// Baseline offset from the top of a cell.
    let ascent: CGFloat

    init(font: NSFont) {
        self.font = font
        self.ascent = font.ascender
        let descent = -font.descender  // descender is negative
        self.cellHeight = max(1, (font.ascender + descent).rounded(.up))
        let advance = ("M" as NSString).size(withAttributes: [.font: font]).width
        self.cellWidth = max(1, advance.rounded(.up))
    }

    static func == (lhs: TerminalMetrics, rhs: TerminalMetrics) -> Bool {
        lhs.font == rhs.font && lhs.cellWidth == rhs.cellWidth && lhs.cellHeight == rhs.cellHeight
    }

    /// Default monospaced font (SF Mono via the system monospaced face).
    static func defaultFont(size: CGFloat = 13) -> NSFont {
        NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
    }

    /// Columns/rows that fit a content area of `width` × `height` logical points.
    func colsRows(width: CGFloat, height: CGFloat) -> (cols: UInt16, rows: UInt16) {
        let cols = max(1, Int((width / cellWidth).rounded(.down)))
        let rows = max(1, Int((height / cellHeight).rounded(.down)))
        return (UInt16(min(cols, Int(UInt16.max))), UInt16(min(rows, Int(UInt16.max))))
    }
}
