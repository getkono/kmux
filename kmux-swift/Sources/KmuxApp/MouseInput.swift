import AppKit

import KmuxBindings

/// Distance (view px) from the top/bottom edge within which a held drag
/// auto-scrolls so the selection can run past the viewport.
private let autoScrollMargin: CGFloat = 8
/// Display rows scrolled per auto-scroll tick (~60 Hz).
private let autoScrollLines: Int32 = 2

/// Pointer input: scroll-wheel (PTY mouse-report or local scrollback, decided in
/// the FFI) and click-drag text selection with word/line modes — the AppKit
/// analog of kmux-gtk's `input.rs`. A drag held at the top/bottom edge
/// auto-scrolls so a selection can span more than one screen. Selection state
/// lives in the grid (via the FFI); the frontend keeps only the drag anchor,
/// last pointer position, and auto-scroll timer.
extension TerminalNSView {
    override func scrollWheel(with event: NSEvent) {
        guard case .normal = model.mode else {
            super.scrollWheel(with: event)
            return
        }
        let dy = event.scrollingDeltaY
        guard dy != 0 else { return }
        let (col, row) = cellLocation(of: event)
        // dy > 0 scrolls up, into history (the FFI `scroll_at` takes lines>0 =
        // up). One notch ≈ 3 lines for a wheel (matching GTK's SCROLL_LINES), 1
        // for a precise trackpad delta.
        let magnitude = event.hasPreciseScrollingDeltas ? 1 : 3
        let lines = Int32(dy > 0 ? magnitude : -magnitude)
        model.driver.scrollAt(col: UInt32(col), row: UInt32(row), lines: lines)
        model.refreshGridView()
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        let (col, row) = cellLocation(of: event)
        switch event.clickCount {
        case 2:
            model.driver.selectWordAt(row: UInt32(row), col: UInt32(col))
            endDrag()
        case 3:
            model.driver.selectLineAt(row: UInt32(row))
            endDrag()
        default:
            dragAnchor = (col, row)
            dragLast = convert(event.locationInWindow, from: nil)
            model.driver.setSelection(
                anchorRow: UInt32(row), anchorCol: UInt32(col),
                endRow: UInt32(row), endCol: UInt32(col))
            startAutoScroll()
        }
        model.refreshGridView()
    }

    override func mouseDragged(with event: NSEvent) {
        guard let anchor = dragAnchor else { return }
        dragLast = convert(event.locationInWindow, from: nil)
        let (col, row) = cellLocation(of: event)
        model.driver.setSelection(
            anchorRow: UInt32(anchor.row), anchorCol: UInt32(anchor.col),
            endRow: UInt32(row), endCol: UInt32(col))
        model.refreshGridView()
    }

    override func mouseUp(with event: NSEvent) {
        endDrag()
    }

    /// The visible cell under a mouse event. The FFI clamps to the grid and maps
    /// to absolute coordinates, so out-of-bounds values here are harmless.
    func cellLocation(of event: NSEvent) -> (col: Int, row: Int) {
        let p = convert(event.locationInWindow, from: nil)
        let col = max(0, Int(p.x / metrics.cellWidth))
        let row = max(0, Int(p.y / metrics.cellHeight))
        return (col, row)
    }

    // MARK: - Auto-scroll while dragging past the edge

    private func startAutoScroll() {
        autoScrollTimer?.invalidate()
        // `.common` so the timer keeps firing during the event-tracking run-loop
        // mode of a mouse drag — even when the pointer is held still at the edge
        // and no further `mouseDragged` events arrive.
        let timer = Timer(timeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in
            self?.autoScrollStep()
        }
        RunLoop.current.add(timer, forMode: .common)
        autoScrollTimer = timer
    }

    private func endDrag() {
        dragAnchor = nil
        dragLast = nil
        autoScrollTimer?.invalidate()
        autoScrollTimer = nil
    }

    /// One auto-scroll step: while a drag sits within `autoScrollMargin` of the
    /// top/bottom edge, scroll local scrollback and extend the selection to the
    /// edge cell under the pointer's last column. The view is flipped (y grows
    /// downward), so small y is the top edge.
    private func autoScrollStep() {
        guard let anchor = dragAnchor, let last = dragLast else { return }
        let rows = Int(model.snapshot?.rows ?? 0)
        let edgeRow: Int
        if last.y < autoScrollMargin {
            model.driver.scrollLines(lines: autoScrollLines)  // up, into history
            edgeRow = 0
        } else if last.y > bounds.height - autoScrollMargin {
            model.driver.scrollLines(lines: -autoScrollLines)  // down, toward live
            edgeRow = max(0, rows - 1)
        } else {
            return
        }
        let col = max(0, Int(last.x / metrics.cellWidth))
        model.driver.setSelection(
            anchorRow: UInt32(anchor.row), anchorCol: UInt32(anchor.col),
            endRow: UInt32(edgeRow), endCol: UInt32(col))
        model.refreshGridView()
    }
}
