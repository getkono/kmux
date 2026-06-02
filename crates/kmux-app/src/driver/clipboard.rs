//! Clipboard payload sanitization (toolkit-agnostic).
//!
//! Terminal selections and OSC 52 payloads can carry interior `\0` bytes (empty
//! grid cells, raw byte streams). Some toolkit clipboard APIs convert the string
//! to a C string and abort the process on an interior NUL — e.g. GTK's
//! `Clipboard::set_text` is a non-unwinding FFI trampoline, so the abort cannot
//! even be caught. [`FrontendDriver`](super::FrontendDriver) sanitizes every
//! `CopyToClipboard` payload here *before* it reaches a frontend, so each
//! frontend's clipboard write is safe by construction.

use std::borrow::Cow;

/// Remove interior NUL bytes that would make a C-string-based clipboard API
/// abort. Returns the input untouched (borrowed) in the common NUL-free case.
pub fn sanitize_clipboard_text(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.chars().filter(|&c| c != '\0').collect())
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_passes_through_nul_free_text() {
        // No allocation: NUL-free input is borrowed unchanged.
        assert!(matches!(
            sanitize_clipboard_text("hello world"),
            Cow::Borrowed("hello world")
        ));
    }

    #[test]
    fn sanitize_strips_interior_nuls() {
        // A terminal selection over empty grid cells yields interior NULs,
        // which would otherwise abort a C-string clipboard write.
        assert_eq!(sanitize_clipboard_text("ab\0cd\0").as_ref(), "abcd");
    }

    #[test]
    fn sanitize_handles_all_nul_input() {
        assert_eq!(sanitize_clipboard_text("\0\0\0").as_ref(), "");
    }

    #[test]
    fn sanitize_preserves_unicode() {
        assert_eq!(sanitize_clipboard_text("café\0🦀").as_ref(), "café🦀");
    }
}
