//! Rendering numbers the way a person reads them.
//!
//! This exists because the same byte count reached a user two different ways.
//! `kmux ps` printed `1.5 MiB`; the GUI's process overview printed `1.5M`, from
//! a second copy of the scale arithmetic whose doc comment claimed it "mirrors
//! the CLI's `kmux ps` formatter" — which it did not, in two ways at once. It
//! also rendered zero as an empty cell, and the CLI did not.
//!
//! Neither rendering is wrong. A narrow GUI column really does want `1.5M`, and
//! a blank beats `0 B` in a table of mostly-idle processes. What was wrong is
//! that the difference lived in a comment that disagreed with the code. Here
//! they are two named styles over one scale, so changing how big numbers break
//! down changes both, and the divergence that remains is the deliberate part.

/// Binary scale steps, largest first, with the suffix for each style.
const SCALES: [(u64, &str, &str); 3] = [
    (1024 * 1024 * 1024, " GiB", "G"),
    (1024 * 1024, " MiB", "M"),
    (1024, " KiB", "K"),
];

/// `1.5 MiB`. Full units, and an explicit `0 B` rather than a blank — for CLI
/// output, where a column is as wide as it needs to be and a missing value
/// would be indistinguishable from a bug.
#[must_use]
pub fn bytes(n: u64) -> String {
    for (scale, long, _) in SCALES {
        if n >= scale {
            #[expect(
                clippy::cast_precision_loss,
                reason = "display only; f64 is exact to 2^53 bytes, ~9 PiB"
            )]
            return format!("{:.1}{long}", n as f64 / scale as f64);
        }
    }
    format!("{n} B")
}

/// `1.5M`, and empty for zero. For a GUI table column, where the width is
/// fixed and a row of `0 B` is noise the eye has to filter out.
#[must_use]
pub fn bytes_compact(n: u64) -> String {
    if n == 0 {
        return String::new();
    }
    for (scale, _, short) in SCALES {
        if n >= scale {
            #[expect(
                clippy::cast_precision_loss,
                reason = "display only; f64 is exact to 2^53 bytes, ~9 PiB"
            )]
            return format!("{:.1}{short}", n as f64 / scale as f64);
        }
    }
    format!("{n}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_scale_boundary_picks_the_unit_above_it() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1024 * 1024 - 1), "1024.0 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(bytes(1536 * 1024 * 1024), "1.5 GiB");
    }

    #[test]
    fn the_compact_style_differs_only_in_suffix_and_at_zero() {
        assert_eq!(bytes_compact(0), "", "a blank cell, not 0 B");
        assert_eq!(bytes_compact(1023), "1023B");
        assert_eq!(bytes_compact(1024), "1.0K");
        assert_eq!(bytes_compact(1024 * 1024), "1.0M");
        assert_eq!(bytes_compact(1536 * 1024 * 1024), "1.5G");
    }

    #[test]
    fn both_styles_break_at_the_same_places() {
        // The point of one SCALES table: whatever the suffixes are, the two
        // styles must never disagree about which magnitude a number is.
        for n in [
            0_u64,
            1,
            1023,
            1024,
            4096,
            1024 * 1024 - 1,
            1024 * 1024,
            3 * 1024 * 1024 * 1024,
        ] {
            let long = bytes(n);
            let short = bytes_compact(n);
            if n == 0 {
                continue; // the one deliberate divergence
            }
            let magnitude = |s: &str| match s {
                s if s.contains('G') => 3,
                s if s.contains('M') => 2,
                s if s.contains('K') => 1,
                _ => 0,
            };
            assert_eq!(
                magnitude(&long),
                magnitude(&short),
                "{n} rendered as {long:?} and {short:?}"
            );
        }
    }

    #[test]
    fn above_a_gibibyte_keeps_scaling_rather_than_wrapping() {
        assert_eq!(bytes(9 * 1024 * 1024 * 1024), "9.0 GiB");
        assert_eq!(bytes(2048_u64 * 1024 * 1024 * 1024), "2048.0 GiB");
    }
}
