import AppKit
import QuartzCore
import SwiftUI

import KmuxBindings

/// SwiftUI host for the terminal grid `NSView`.
struct TerminalView: NSViewRepresentable {
    let model: KmuxModel

    func makeNSView(context: Context) -> TerminalNSView {
        let view = TerminalNSView(model: model)
        model.terminalView = view
        return view
    }

    func updateNSView(_ nsView: TerminalNSView, context: Context) {}
}

/// The terminal grid leaf: a flipped `NSView` that paints the driver's active
/// grid with CoreText/CoreGraphics — the AppKit analog of kmux-gtk's
/// `DrawingArea` + `render.rs` (cairo/Pango). Reports its content size to the
/// driver on resize. Keyboard/mouse input is attached in `KeyInput`/`MouseInput`.
final class TerminalNSView: NSView {
    let model: KmuxModel
    private(set) var metrics: TerminalMetrics
    /// Anchor cell (visible coords) of an in-progress single-click drag selection.
    var dragAnchor: (col: Int, row: Int)?
    /// Last pointer location (view coords) during a drag; drives auto-scroll.
    var dragLast: NSPoint?
    /// Repeating timer that auto-scrolls while a drag sits at the viewport edge.
    var autoScrollTimer: Timer?
    /// The divider being dragged (a press that began on a gutter), if any.
    var activeDivider: FfiDivider?
    /// `true` while a primary-button drag is being forwarded to a mouse-tracking
    /// inner program (so motion/release forward too and local selection is
    /// suppressed). Mutually exclusive with `dragAnchor`.
    var ptyDragging = false
    /// Tracking area driving the hover resize cursor over dividers.
    var dividerTrackingArea: NSTrackingArea?
    /// Last `(cols, rows)` reported to the driver. AppKit re-issues `setFrameSize`
    /// (and `viewDidMoveToWindow`) on layout passes that don't change the cell
    /// grid; skipping those keeps a redundant callback from restarting the
    /// driver's 100 ms resize debounce (so a settled size actually reaches the daemon).
    private var lastReportedCells: (cols: UInt16, rows: UInt16)?

    #if KMUX_GPU
        /// When `KMUX_RENDERER=wgpu`, the grid is drawn by the shared GPU renderer
        /// (kmux-render, over the FFI) into a `CAMetalLayer` instead of CoreText.
        /// The view, input, sizing, and pump are otherwise unchanged: `needsDisplay`
        /// routes to `updateLayer()` (Metal) instead of `draw(_:)` (CoreText).
        private let gpuActive =
            ProcessInfo.processInfo.environment["KMUX_RENDERER"]?.lowercased() == "wgpu"
        private var gpuRenderer: KmuxRenderer?
        private var gpuDrawable: (w: UInt32, h: UInt32)?
    #endif

    init(model: KmuxModel) {
        self.model = model
        self.metrics = TerminalMetrics(appearance: model.appearance)
        super.init(frame: .zero)
        wantsLayer = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    // Top-left origin so grid row 0 sits at the top, like a terminal.
    override var isFlipped: Bool { true }
    override var acceptsFirstResponder: Bool { true }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        window?.makeFirstResponder(self)
        reportSize(bounds.size)
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        reportSize(newSize)
    }

    /// Report the content geometry so the driver can resize the remote PTY
    /// (debounced inside the driver). Mirrors kmux-gtk's `connect_resize`.
    private func reportSize(_ size: NSSize) {
        guard size.width > 0, size.height > 0 else { return }
        let (cols, rows) = metrics.colsRows(width: size.width, height: size.height)
        if let last = lastReportedCells, last == (cols, rows) { return }
        lastReportedCells = (cols, rows)
        let scale = window?.backingScaleFactor ?? 2.0
        let pw = UInt16(min(Int(size.width * scale), Int(UInt16.max)))
        let ph = UInt16(min(Int(size.height * scale), Int(UInt16.max)))
        model.driver.requestResize(rows: rows, cols: cols, pixelWidth: pw, pixelHeight: ph)
    }

    // MARK: - GPU rendering (opt-in)

    #if KMUX_GPU
        // A CAMetalLayer backing when GPU rendering is active, so wgpu presents to
        // it; otherwise AppKit's default layer (CoreText `draw`).
        override func makeBackingLayer() -> CALayer {
            gpuActive ? CAMetalLayer() : super.makeBackingLayer()
        }

        // Route display to `updateLayer` (Metal) instead of `draw` when GPU-active.
        override var wantsUpdateLayer: Bool { gpuActive }

        override func updateLayer() {
            renderMetalFrame()
        }

        /// Render the active tab via the FFI `KmuxRenderer` into the metal layer.
        /// Created lazily on the first frame; on init failure it logs once and
        /// leaves the layer blank (the user explicitly opted into wgpu).
        private func renderMetalFrame() {
            guard gpuActive, let metalLayer = layer as? CAMetalLayer else { return }
            let scale = window?.backingScaleFactor ?? 2.0
            metalLayer.contentsScale = scale
            let w = UInt32(max(1, Int((bounds.width * scale).rounded())))
            let h = UInt32(max(1, Int((bounds.height * scale).rounded())))

            if gpuRenderer == nil {
                let ptr = UInt64(UInt(bitPattern: Unmanaged.passUnretained(metalLayer).toOpaque()))
                gpuRenderer = try? KmuxRenderer.newMetal(
                    driver: model.driver, layerPtr: ptr, width: w, height: h, scale: Float(scale))
                gpuDrawable = (w, h)
                if gpuRenderer == nil {
                    NSLog("kmux-render: GPU renderer init failed; KMUX_RENDERER=wgpu had no effect")
                }
            }
            guard let renderer = gpuRenderer else { return }
            if gpuDrawable.map({ $0 != (w, h) }) ?? true {
                renderer.resize(width: w, height: h, scale: Float(scale))
                gpuDrawable = (w, h)
            }
            renderer.render(driver: model.driver, width: w, height: h, scale: Float(scale))
        }
    #endif

    // MARK: - Render-debug tooling

    /// Diagnostic renderer reset (Reset Renderer menu / FfiEffect): drop the GPU
    /// renderer + drawable so the next frame rebuilds them with a fresh glyph
    /// atlas, then repaint. On the CoreText path this is just a repaint (there is
    /// no persistent renderer/atlas to rebuild).
    func resetGpuRenderer() {
        #if KMUX_GPU
            gpuRenderer = nil
            gpuDrawable = nil
        #endif
        needsDisplay = true
    }

    /// The active renderer leaf, for the render-debug overlay.
    var activeRendererName: String {
        #if KMUX_GPU
            return gpuActive ? "wgpu" : "coretext"
        #else
            return "coretext"
        #endif
    }

    // MARK: - Rendering

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let theme = model.theme
        ctx.setFillColor(theme.bg.cgColor)
        ctx.fill(bounds)
        let m = metrics

        let rects = model.layout
        if rects.isEmpty {
            drawPlaceholder(theme: theme)
            return
        }

        // Tile each visible pane into its resolved sub-rect (clip + translate,
        // mirroring kmux-gtk's `render_tiled`).
        for rect in rects {
            guard let snap = model.paneSnapshots[rect.paneId], snap.rows > 0, snap.cols > 0 else {
                continue
            }
            drawPane(snap, rect: rect, theme: theme, metrics: m, ctx: ctx)
        }

        // Accent border on the focused pane (only when there's more than one).
        if rects.count > 1, let focused = rects.first(where: { $0.focused }) {
            ctx.setStrokeColor(theme.accent.cgColor)
            ctx.setLineWidth(1)
            ctx.stroke(pixelRect(focused, metrics: m).insetBy(dx: 0.5, dy: 0.5))
        }

        // OSC 9;4 progress bar (issue #125): a thin bar along each pane's bottom
        // edge, colored by state, width proportional to the percent (full-width
        // for indeterminate). Mirrors kmux-gtk's `render_tiled` progress bar.
        for rect in rects {
            guard let (color, frac) = progressBarFill(rect, theme: theme) else { continue }
            let px = pixelRect(rect, metrics: m)
            let barH: CGFloat = min(3, px.height)
            let barW = min(px.width, px.width * frac)
            if barW > 0 {
                ctx.setFillColor(color)
                ctx.fill(CGRect(x: px.minX, y: px.maxY - barH, width: barW, height: barH))
            }
        }
    }

    /// `(color, width-fraction)` for a pane's OSC 9;4 progress bar, or `nil` for
    /// `Remove`. `Indeterminate` fills the full width; the numeric states use
    /// `progress / 100`. Colours mirror the GTK frontend (set→accent, error→red,
    /// pause→orange).
    private func progressBarFill(_ rect: FfiPaneRect, theme: FfiTheme) -> (CGColor, CGFloat)? {
        let frac = CGFloat(min(rect.progress ?? 0, 100)) / 100.0
        switch rect.progressState {
        case .remove: return nil
        case .set: return (theme.accent.cgColor, frac)
        case .error: return (theme.red.cgColor, frac)
        case .pause: return (theme.orange.cgColor, frac)
        case .indeterminate: return (theme.accent.cgColor, 1.0)
        }
    }

    /// Render one pane's grid into its sub-rect. Backgrounds then glyphs, clipped
    /// and translated to the pane's pixel origin so the per-cell helpers run in
    /// pane-local coordinates. The focused pane also gets the selection wash, the
    /// cursor, and a scroll indicator (its state lives in `model.selection` /
    /// `model.scrollInfo` / `model.snapshot`).
    private func drawPane(
        _ snap: GridSnapshot, rect: FfiPaneRect, theme: FfiTheme,
        metrics m: TerminalMetrics, ctx: CGContext
    ) {
        let px = pixelRect(rect, metrics: m)
        ctx.saveGState()
        ctx.clip(to: px)
        ctx.translateBy(x: px.minX, y: px.minY)

        let stride = Int(snap.cols)
        let rows = min(Int(snap.rows), Int(rect.rows))
        let cols = min(Int(snap.cols), Int(rect.cols))
        snap.cells.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            // Pass 1: cell backgrounds (so a wide glyph can overdraw its spacer).
            for r in 0..<rows {
                for c in 0..<cols {
                    let cell = PackedCellLayout.decode(raw, r * stride + c)
                    ctx.setFillColor(cgRGB(cell.bg))
                    ctx.fill(cellRect(row: r, col: c, metrics: m))
                }
            }
            // Pass 2: glyphs.
            for r in 0..<rows {
                for c in 0..<cols {
                    let cell = PackedCellLayout.decode(raw, r * stride + c)
                    if cell.isSpacer { continue }
                    guard let ch = cell.character else { continue }
                    drawGlyph(ch, cell: cell, row: r, col: c, theme: theme, metrics: m)
                }
            }
        }

        if rect.focused {
            // Translucent selection tint over the glyphs (live screen only).
            drawSelection(ctx, theme: theme, metrics: m)
            // Cursor (hidden while scrolled into history, like the GTK frontend).
            if model.scrollInfo.offset == 0 {
                drawCursor(snap.cursor, theme: theme, metrics: m)
            }
            if model.scrollInfo.offset > 0 {
                drawScrollIndicator(width: px.width, height: px.height, theme: theme, metrics: m)
            }
        }

        ctx.restoreGState()
    }

    /// A pane rect's pixel rectangle in view coordinates.
    func pixelRect(_ rect: FfiPaneRect, metrics m: TerminalMetrics) -> CGRect {
        CGRect(
            x: CGFloat(rect.col) * m.cellWidth,
            y: CGFloat(rect.row) * m.cellHeight,
            width: CGFloat(rect.cols) * m.cellWidth,
            height: CGFloat(rect.rows) * m.cellHeight
        )
    }

    private func cellRect(row: Int, col: Int, metrics m: TerminalMetrics) -> CGRect {
        CGRect(
            x: CGFloat(col) * m.cellWidth,
            y: CGFloat(row) * m.cellHeight,
            width: m.cellWidth,
            height: m.cellHeight
        )
    }

    private func drawGlyph(
        _ ch: Character, cell: PackedCell, row: Int, col: Int, theme: FfiTheme,
        metrics m: TerminalMetrics
    ) {
        // Pick the matching face (explicit variant family, else synthetic style),
        // each already carrying the configured OpenType features.
        let font: NSFont
        switch (cell.bold, cell.italic) {
        case (true, true): font = m.fontBoldItalic
        case (true, false): font = m.fontBold
        case (false, true): font = m.fontItalic
        case (false, false): font = m.font
        }
        var color = nsRGB(cell.fg)
        if cell.dim { color = color.withAlphaComponent(0.55) }

        var attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color]
        if cell.underline { attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue }
        if cell.strikethrough { attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }

        NSAttributedString(string: String(ch), attributes: attrs)
            .draw(at: CGPoint(x: CGFloat(col) * m.cellWidth, y: CGFloat(row) * m.cellHeight))
    }

    private func drawCursor(_ cursor: FfiCursor, theme: FfiTheme, metrics m: TerminalMetrics) {
        guard cursor.visible, cursor.shape != 4 else { return }  // 4 = hidden
        if cursor.blink && !model.blinkOn { return }
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }

        let x = CGFloat(cursor.col) * m.cellWidth
        let y = CGFloat(cursor.row) * m.cellHeight
        let rect = CGRect(x: x, y: y, width: m.cellWidth, height: m.cellHeight)

        switch cursor.shape {
        case 0:  // block: invert, then redraw the underlying glyph in cursor_fg
            ctx.setFillColor(theme.cursorBg.cgColor)
            ctx.fill(rect)
            if let snap = model.snapshot {
                let idx = Int(cursor.row) * Int(snap.cols) + Int(cursor.col)
                if (idx + 1) * PackedCellLayout.stride <= snap.cells.count {
                    let cell = snap.cells.withUnsafeBytes { PackedCellLayout.decode($0, idx) }
                    if let ch = cell.character {
                        NSAttributedString(
                            string: String(ch),
                            attributes: [.font: m.font, .foregroundColor: theme.cursorFg.nsColor]
                        ).draw(at: CGPoint(x: x, y: y))
                    }
                }
            }
        case 3:  // hollow block: outline
            ctx.setStrokeColor(theme.cursorBg.cgColor)
            ctx.setLineWidth(1)
            ctx.stroke(rect.insetBy(dx: 0.5, dy: 0.5))
        case 1:  // underline: bar at the bottom
            ctx.setFillColor(theme.cursorBg.cgColor)
            ctx.fill(CGRect(x: x, y: y + m.cellHeight - 2, width: m.cellWidth, height: 2))
        case 2:  // bar: bar at the left
            ctx.setFillColor(theme.cursorBg.cgColor)
            ctx.fill(CGRect(x: x, y: y, width: 2, height: m.cellHeight))
        default:
            break
        }
    }

    private func drawSelection(_ ctx: CGContext, theme: FfiTheme, metrics m: TerminalMetrics) {
        let spans = model.selection
        guard !spans.isEmpty else { return }
        let a = theme.accent
        ctx.setFillColor(
            CGColor(
                srgbRed: CGFloat(a.r) / 255.0, green: CGFloat(a.g) / 255.0,
                blue: CGFloat(a.b) / 255.0, alpha: 0.30))

        // One wash rect per visible-row span (already scroll- and wrap-aware).
        for span in spans {
            let row = Int(span.row)
            let cStart = Int(span.colStart)
            let cEnd = Int(span.colEnd)
            ctx.fill(
                CGRect(
                    x: CGFloat(cStart) * m.cellWidth,
                    y: CGFloat(row) * m.cellHeight,
                    width: CGFloat(cEnd - cStart + 1) * m.cellWidth,
                    height: m.cellHeight))
        }
    }

    /// Draw the focused pane's scroll indicator at its local bottom-right. Called
    /// inside the pane's translated context, so `width`/`height` are the pane's
    /// pixel extent (not the whole view's).
    private func drawScrollIndicator(
        width: CGFloat, height: CGFloat, theme: FfiTheme, metrics m: TerminalMetrics
    ) {
        let label = "[\(model.scrollInfo.offset)/\(model.scrollInfo.total)]"
        let s = NSAttributedString(
            string: label,
            attributes: [
                .font: m.font,
                .foregroundColor: theme.accent.nsColor,
                .backgroundColor: theme.statusBg.nsColor,
            ])
        let size = s.size()
        s.draw(at: CGPoint(x: width - size.width - 4, y: height - size.height - 2))
    }

    private func drawPlaceholder(theme: FfiTheme) {
        let label = model.connection.label.isEmpty ? "connecting…" : model.connection.label
        let s = NSAttributedString(
            string: label,
            attributes: [.font: metrics.font, .foregroundColor: theme.fgDim.nsColor])
        let size = s.size()
        s.draw(at: CGPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2))
    }

    // MARK: - Color helpers

    private func cgRGB(_ c: (r: UInt8, g: UInt8, b: UInt8)) -> CGColor {
        CGColor(
            srgbRed: CGFloat(c.r) / 255.0, green: CGFloat(c.g) / 255.0, blue: CGFloat(c.b) / 255.0,
            alpha: 1.0)
    }

    private func nsRGB(_ c: (r: UInt8, g: UInt8, b: UInt8)) -> NSColor {
        NSColor(
            srgbRed: CGFloat(c.r) / 255.0, green: CGFloat(c.g) / 255.0, blue: CGFloat(c.b) / 255.0,
            alpha: 1.0)
    }
}
