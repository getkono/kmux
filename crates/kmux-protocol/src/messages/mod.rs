pub mod category;
pub mod session;
pub mod vt;

mod client;
mod server;
mod types;

pub use category::MessageCategory;
pub use client::*;
pub use server::*;
pub use session::*;
pub use types::*;
pub use vt::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_attrs_bits_no_overlap() {
        let flags: &[u16] = &[
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::STRIKETHROUGH,
            CellAttrs::INVERSE,
            CellAttrs::HIDDEN,
            CellAttrs::DIM,
            CellAttrs::BLINK,
            CellAttrs::WIDE_CHAR,
            CellAttrs::WIDE_CHAR_SPACER,
            CellAttrs::DEFAULT_FG,
            CellAttrs::DEFAULT_BG,
        ];
        for (i, a) in flags.iter().enumerate() {
            for (j, b) in flags.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bit overlap between flag {i} and {j}");
                }
            }
        }
    }

    #[test]
    fn term_modes_bits_no_overlap() {
        let flags: &[u16] = &[
            TermModes::APP_CURSOR,
            TermModes::BRACKETED_PASTE,
            TermModes::MOUSE_REPORT_CLICK,
            TermModes::MOUSE_DRAG,
            TermModes::MOUSE_MOTION,
            TermModes::SGR_MOUSE,
        ];
        for (i, a) in flags.iter().enumerate() {
            assert!(a.is_power_of_two(), "flag {i} is not a single bit: {a}");
            for (j, b) in flags.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bit overlap between flag {i} and {j}");
                }
            }
        }
    }

    #[test]
    fn term_modes_accessors() {
        let empty = TermModes::EMPTY;
        assert!(!empty.app_cursor());
        assert!(!empty.bracketed_paste());

        let bp = TermModes(TermModes::BRACKETED_PASTE);
        assert!(!bp.app_cursor());
        assert!(bp.bracketed_paste());

        let both = TermModes(TermModes::APP_CURSOR | TermModes::BRACKETED_PASTE);
        assert!(both.app_cursor());
        assert!(both.bracketed_paste());

        let mouse = TermModes(TermModes::MOUSE_REPORT_CLICK | TermModes::SGR_MOUSE);
        assert!(mouse.mouse_report());
        assert!(mouse.sgr_mouse());
        assert!(!mouse.app_cursor());

        let empty = TermModes::EMPTY;
        assert!(!empty.mouse_report());
        assert!(!empty.sgr_mouse());

        let drag = TermModes(TermModes::MOUSE_DRAG);
        assert!(drag.mouse_report());
        assert!(!drag.sgr_mouse());
    }

    #[test]
    fn cell_attrs_each_flag_is_single_bit() {
        let flags: &[u16] = &[
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::STRIKETHROUGH,
            CellAttrs::INVERSE,
            CellAttrs::HIDDEN,
            CellAttrs::DIM,
            CellAttrs::BLINK,
            CellAttrs::WIDE_CHAR,
            CellAttrs::WIDE_CHAR_SPACER,
            CellAttrs::DEFAULT_FG,
            CellAttrs::DEFAULT_BG,
        ];
        for (i, flag) in flags.iter().enumerate() {
            assert!(
                flag.is_power_of_two(),
                "flag {i} is not a single bit: {flag}"
            );
        }
    }

    #[test]
    fn pane_id_format() {
        let word_id = "eagle".to_string();
        let pane_index = 0u32;
        let pane_id = format!("{word_id}/{pane_index}");
        assert_eq!(pane_id, "eagle/0");

        // Parse back
        let (w, idx_str) = pane_id.rsplit_once('/').unwrap();
        let idx: u32 = idx_str.parse().unwrap();
        assert_eq!(w, "eagle");
        assert_eq!(idx, 0);
    }

    #[test]
    fn connection_id_serialization_roundtrip() {
        let id = ConnectionId(0xdeadbeef_u64);
        // Use postcard (the wire codec) for the roundtrip.
        let bytes = postcard::to_allocvec(&id).unwrap();
        let decoded: ConnectionId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, decoded);
    }
}
