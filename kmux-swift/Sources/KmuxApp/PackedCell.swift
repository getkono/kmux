import Foundation

/// A decoded terminal cell from the FFI's packed 16-byte, little-endian layout.
/// Must match `kmux-ffi/src/cells.rs` (`DEFAULT_FG`/`DEFAULT_BG` are already
/// resolved to RGBA in Rust, so the colors here are final).
struct PackedCell {
    /// Unicode scalar value (`char as u32`).
    let scalar: UInt32
    let fg: (r: UInt8, g: UInt8, b: UInt8)
    let bg: (r: UInt8, g: UInt8, b: UInt8)
    /// `CellAttrs` bitfield.
    let attrs: UInt16
    /// `0` = wide-char trailing spacer, `1` = normal, `2` = wide char.
    let width: UInt8

    // Attribute bit positions (see cells.rs). The `default_*` bits are resolved
    // away in Rust and never set here.
    var bold: Bool { attrs & (1 << 0) != 0 }
    var italic: Bool { attrs & (1 << 1) != 0 }
    var underline: Bool { attrs & (1 << 2) != 0 }
    var strikethrough: Bool { attrs & (1 << 3) != 0 }
    var dim: Bool { attrs & (1 << 6) != 0 }

    /// Trailing half of a wide glyph; its background is filled but no glyph is
    /// drawn (the lead cell's wide glyph covers it).
    var isSpacer: Bool { width == 0 }

    /// The character to draw, or `nil` for an unpaintable scalar / blank.
    var character: Character? {
        guard scalar != 0, scalar != 0x20, let s = Unicode.Scalar(scalar) else { return nil }
        if s.properties.isDefaultIgnorableCodePoint { return nil }
        let c = Character(s)
        return c.isWhitespace ? nil : c
    }
}

enum PackedCellLayout {
    /// Bytes per packed cell (`PACKED_CELL_LEN`).
    static let stride = 16

    /// Decode the cell at `index` from a raw byte buffer (a `GridSnapshot.cells`
    /// region). Bytes are read individually so there is no alignment requirement.
    @inline(__always)
    static func decode(_ buf: UnsafeRawBufferPointer, _ index: Int) -> PackedCell {
        let b = index * stride
        let scalar =
            UInt32(buf[b]) | (UInt32(buf[b + 1]) << 8) | (UInt32(buf[b + 2]) << 16)
            | (UInt32(buf[b + 3]) << 24)
        let attrs = UInt16(buf[b + 12]) | (UInt16(buf[b + 13]) << 8)
        return PackedCell(
            scalar: scalar,
            fg: (buf[b + 4], buf[b + 5], buf[b + 6]),
            bg: (buf[b + 8], buf[b + 9], buf[b + 10]),
            attrs: attrs,
            width: buf[b + 14]
        )
    }
}
