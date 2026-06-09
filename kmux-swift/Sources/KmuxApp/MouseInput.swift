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
        // A press on a divider is a resize, not a focus/selection: a double-click
        // resets the split to even, a single-click starts a drag.
        if let div = dividerAt(event) {
            if event.clickCount == 2 {
                model.resetDivider(div)
                activeDivider = nil
            } else {
                activeDivider = div
            }
            return
        }
        // Click-to-focus: a press in a non-focused tile focuses it first, so the
        // selection (and its pane-local coordinates) land in the right pane.
        if let pid = paneIdAt(event), pid != model.focusedPaneId {
            model.focusPane(pid)
        }
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
        // Live-resize while dragging a divider, recomputed against the current
        // tree each event (the FFI no-ops if the split was reshaped).
        if let div = activeDivider {
            let p = convert(event.locationInWindow, from: nil)
            let cell =
                div.verticalBar
                ? max(0, Int(p.x / metrics.cellWidth))
                : max(0, Int(p.y / metrics.cellHeight))
            model.applyDividerDrag(div, pointerCell: UInt32(cell))
            return
        }
        guard let anchor = dragAnchor else { return }
        dragLast = convert(event.locationInWindow, from: nil)
        let (col, row) = cellLocation(of: event)
        model.driver.setSelection(
            anchorRow: UInt32(anchor.row), anchorCol: UInt32(anchor.col),
            endRow: UInt32(row), endCol: UInt32(col))
        model.refreshGridView()
    }

    override func mouseUp(with event: NSEvent) {
        if activeDivider != nil {
            activeDivider = nil
            return
        }
        endDrag()
    }

    // MARK: - Divider hover + hit-test

    override func mouseMoved(with event: NSEvent) {
        if let div = dividerAt(event) {
            (div.verticalBar ? NSCursor.resizeLeftRight : NSCursor.resizeUpDown).set()
        } else {
            NSCursor.arrow.set()
        }
    }

    override func mouseExited(with event: NSEvent) {
        NSCursor.arrow.set()
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let ta = dividerTrackingArea { removeTrackingArea(ta) }
        let ta = NSTrackingArea(
            rect: bounds,
            options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
            owner: self, userInfo: nil)
        addTrackingArea(ta)
        dividerTrackingArea = ta
    }

    /// The draggable divider under a mouse event, within `dividerGrabPx` of its
    /// gutter strip. Uses raw (whole-view) coordinates, like `paneIdAt`. When
    /// several dividers sit near a nested corner, the nearest one wins.
    func dividerAt(_ event: NSEvent) -> FfiDivider? {
        let p = convert(event.locationInWindow, from: nil)
        let cw = metrics.cellWidth
        let ch = metrics.cellHeight
        let slop: CGFloat = 4
        var best: (FfiDivider, CGFloat)?
        for d in model.dividers {
            let px = CGFloat(d.hitCol) * cw
            let py = CGFloat(d.hitRow) * ch
            let pw = CGFloat(d.hitCols) * cw
            let ph = CGFloat(d.hitRows) * ch
            let edgeDist: CGFloat
            let alongOk: Bool
            if d.verticalBar {
                edgeDist = abs(p.x - (px + pw / 2)) - pw / 2
                alongOk = p.y >= py && p.y < py + ph
            } else {
                edgeDist = abs(p.y - (py + ph / 2)) - ph / 2
                alongOk = p.x >= px && p.x < px + pw
            }
            if alongOk && edgeDist <= slop {
                let key = max(edgeDist, 0)
                if best == nil || key < best!.1 { best = (d, key) }
            }
        }
        return best?.0
    }

    /// The visible cell under a mouse event, in the focused pane's local
    /// coordinates (the FFI selection setters act on the focused grid). The FFI
    /// clamps to the grid, so out-of-bounds values here are harmless.
    func cellLocation(of event: NSEvent) -> (col: Int, row: Int) {
        let p = convert(event.locationInWindow, from: nil)
        let gcol = max(0, Int(p.x / metrics.cellWidth))
        let grow = max(0, Int(p.y / metrics.cellHeight))
        if let rect = model.focusedPaneRect {
            return (max(0, gcol - Int(rect.col)), max(0, grow - Int(rect.row)))
        }
        return (gcol, grow)
    }

    /// The pane id whose tile contains a mouse event, if any (for click-to-focus).
    func paneIdAt(_ event: NSEvent) -> String? {
        let p = convert(event.locationInWindow, from: nil)
        let gcol = Int(p.x / metrics.cellWidth)
        let grow = Int(p.y / metrics.cellHeight)
        for r in model.layout
        where gcol >= Int(r.col) && gcol < Int(r.col + r.cols)
            && grow >= Int(r.row) && grow < Int(r.row + r.rows) {
            return r.paneId
        }
        return nil
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
        let gcol = max(0, Int(last.x / metrics.cellWidth))
        let col = model.focusedPaneRect.map { max(0, gcol - Int($0.col)) } ?? gcol
        model.driver.setSelection(
            anchorRow: UInt32(anchor.row), anchorCol: UInt32(anchor.col),
            endRow: UInt32(edgeRow), endCol: UInt32(col))
        model.refreshGridView()
    }
}
