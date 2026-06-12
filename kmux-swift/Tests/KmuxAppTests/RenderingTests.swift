import XCTest

import KmuxBindings

@testable import KmuxApp

/// Unit tests for the pure render-side logic (packed-cell decode + cell
/// geometry), mirroring the Rust-side `cells.rs` tests on the Swift side.
final class RenderingTests: XCTestCase {
    func testPackedCellDecodeExplicitColors() {
        var bytes = [UInt8](repeating: 0, count: PackedCellLayout.stride)
        bytes[0] = 0x41  // 'A'
        bytes[4] = 10
        bytes[5] = 20
        bytes[6] = 30
        bytes[7] = 0xff
        bytes[8] = 40
        bytes[9] = 50
        bytes[10] = 60
        bytes[11] = 0xff
        bytes[12] = 0x01  // BOLD (bit 0)
        bytes[14] = 1  // normal width

        let cell = bytes.withUnsafeBytes { PackedCellLayout.decode($0, 0) }
        XCTAssertEqual(cell.scalar, 0x41)
        XCTAssertEqual(cell.character, "A")
        XCTAssertTrue(cell.bold)
        XCTAssertFalse(cell.italic)
        XCTAssertEqual(cell.fg.r, 10)
        XCTAssertEqual(cell.fg.g, 20)
        XCTAssertEqual(cell.bg.b, 60)
        XCTAssertEqual(cell.width, 1)
        XCTAssertFalse(cell.isSpacer)
    }

    func testWideAndSpacerWidths() {
        var wideBytes = [UInt8](repeating: 0, count: PackedCellLayout.stride)
        wideBytes[14] = 2
        let wide = wideBytes.withUnsafeBytes { PackedCellLayout.decode($0, 0) }
        XCTAssertEqual(wide.width, 2)
        XCTAssertFalse(wide.isSpacer)

        var spacerBytes = [UInt8](repeating: 0, count: PackedCellLayout.stride)
        spacerBytes[14] = 0
        let spacer = spacerBytes.withUnsafeBytes { PackedCellLayout.decode($0, 0) }
        XCTAssertTrue(spacer.isSpacer)
    }

    func testBlankAndSpaceHaveNoGlyph() {
        // A space (0x20) and NUL produce no drawable character.
        var spaceBytes = [UInt8](repeating: 0, count: PackedCellLayout.stride)
        spaceBytes[0] = 0x20
        let space = spaceBytes.withUnsafeBytes { PackedCellLayout.decode($0, 0) }
        XCTAssertNil(space.character)

        let nul = [UInt8](repeating: 0, count: PackedCellLayout.stride)
            .withUnsafeBytes { PackedCellLayout.decode($0, 0) }
        XCTAssertNil(nul.character)
    }

    func testDecodeAtIndex() {
        // Two cells: index 1 carries 'B'.
        var bytes = [UInt8](repeating: 0, count: PackedCellLayout.stride * 2)
        bytes[PackedCellLayout.stride] = 0x42  // 'B' at cell 1, byte 0
        let cell = bytes.withUnsafeBytes { PackedCellLayout.decode($0, 1) }
        XCTAssertEqual(cell.character, "B")
    }

    func testMetricsColsRows() {
        let m = TerminalMetrics(font: TerminalMetrics.defaultFont(size: 13))
        XCTAssertGreaterThan(m.cellWidth, 0)
        XCTAssertGreaterThan(m.cellHeight, 0)
        let (cols, rows) = m.colsRows(width: m.cellWidth * 80, height: m.cellHeight * 24)
        XCTAssertEqual(cols, 80)
        XCTAssertEqual(rows, 24)
        // Always at least 1×1, even for a zero-size content area.
        let (c0, r0) = m.colsRows(width: 0, height: 0)
        XCTAssertEqual(c0, 1)
        XCTAssertEqual(r0, 1)
    }

    func testMetricsFromAppearanceAppliesCellAdjust() {
        func appearance(width: FfiCellAdjust, height: FfiCellAdjust) -> FfiAppearance {
            FfiAppearance(
                family: "Menlo", familyBold: nil, familyItalic: nil, familyBoldItalic: nil,
                sizePt: 13, style: nil, features: [],
                cellWidthAdjust: width, cellHeightAdjust: height)
        }
        let zero = FfiCellAdjust(kind: .pixels, value: 0)
        let base = TerminalMetrics(appearance: appearance(width: zero, height: zero))

        // +4px width adds exactly 4 (both sides ceil an integer offset).
        let wider = TerminalMetrics(
            appearance: appearance(width: FfiCellAdjust(kind: .pixels, value: 4), height: zero))
        XCTAssertEqual(wider.cellWidth, base.cellWidth + 4)

        // +50% height grows the cell.
        let taller = TerminalMetrics(
            appearance: appearance(width: zero, height: FfiCellAdjust(kind: .percent, value: 50)))
        XCTAssertGreaterThan(taller.cellHeight, base.cellHeight)
    }
}
