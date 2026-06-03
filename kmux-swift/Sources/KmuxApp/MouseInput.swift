import AppKit

import KmuxBindings

/// Pointer input: scroll-wheel (PTY mouse-report or local scrollback, decided in
/// the FFI) and click-drag text selection with word/line modes — the AppKit
/// analog of kmux-gtk's `input.rs`. Selection state lives in the grid (via the
/// FFI); the only frontend state is the drag anchor.
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
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        let (col, row) = cellLocation(of: event)
        switch event.clickCount {
        case 2:
            model.driver.selectWordAt(row: UInt32(row), col: UInt32(col))
            dragAnchor = nil
        case 3:
            model.driver.selectLineAt(row: UInt32(row))
            dragAnchor = nil
        default:
            dragAnchor = (col, row)
            model.driver.setSelection(
                anchorRow: UInt32(row), anchorCol: UInt32(col),
                endRow: UInt32(row), endCol: UInt32(col))
        }
        needsDisplay = true
    }

    override func mouseDragged(with event: NSEvent) {
        guard let anchor = dragAnchor else { return }
        let (col, row) = cellLocation(of: event)
        model.driver.setSelection(
            anchorRow: UInt32(anchor.row), anchorCol: UInt32(anchor.col),
            endRow: UInt32(row), endCol: UInt32(col))
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        dragAnchor = nil
    }

    /// The visible cell under a mouse event. The FFI clamps to the grid and maps
    /// to absolute coordinates, so out-of-bounds values here are harmless.
    func cellLocation(of event: NSEvent) -> (col: Int, row: Int) {
        let p = convert(event.locationInWindow, from: nil)
        let col = max(0, Int(p.x / metrics.cellWidth))
        let row = max(0, Int(p.y / metrics.cellHeight))
        return (col, row)
    }
}
