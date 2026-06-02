//! Cursor-blink phase state machine (toolkit-agnostic).
//!
//! The inner-pane cursor is drawn by each frontend, but *whether* it is shown on
//! a given frame is shared interaction policy: a cursor that requested blinking
//! (DECSCUSR `blinking_*`) toggles every [`CURSOR_BLINK_HALF`]; a steady cursor
//! stays solid. [`FrontendDriver`](super::FrontendDriver) drives this off its
//! pump tick so every frontend blinks identically.

use std::time::{Duration, Instant};

/// Cursor blink half-period (on→off or off→on). Matches the common desktop
/// default `gtk-cursor-blink-time` (1200 ms full cycle) / 2.
pub const CURSOR_BLINK_HALF: Duration = Duration::from_millis(600);

/// Advance the cursor-blink phase for one pump tick.
///
/// Given the current phase (`blink_on`), when the current half-cycle started
/// (`phase_start`), whether the active cursor is currently *requesting* blink
/// (`cursor_blinks`), and `now`, returns `(new_blink_on, new_phase_start,
/// changed)`. `changed` drives a redraw.
///
/// - A blinking cursor toggles once a full [`CURSOR_BLINK_HALF`] has elapsed.
/// - A non-blinking (steady) cursor is pinned solid; if it was mid-"off" the
///   pin counts as a change so the solid cursor repaints immediately.
pub fn advance_blink(
    blink_on: bool,
    phase_start: Instant,
    cursor_blinks: bool,
    now: Instant,
) -> (bool, Instant, bool) {
    if cursor_blinks {
        if now.duration_since(phase_start) >= CURSOR_BLINK_HALF {
            (!blink_on, now, true)
        } else {
            (blink_on, phase_start, false)
        }
    } else if !blink_on {
        (true, phase_start, true)
    } else {
        (blink_on, phase_start, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinking_cursor_holds_phase_until_half_elapsed() {
        let t0 = Instant::now();
        // Just shy of the half-period: no toggle.
        let (on, start, changed) = advance_blink(true, t0, true, t0 + CURSOR_BLINK_HALF / 2);
        assert!(on, "still on");
        assert_eq!(start, t0, "phase start unchanged");
        assert!(!changed, "no redraw before the half-period");
    }

    #[test]
    fn blinking_cursor_toggles_after_half_period() {
        let t0 = Instant::now();
        let (on, start, changed) = advance_blink(true, t0, true, t0 + CURSOR_BLINK_HALF);
        assert!(!on, "toggled off");
        assert_eq!(start, t0 + CURSOR_BLINK_HALF, "phase restarts at now");
        assert!(changed, "toggle forces a redraw");
        // And back on after another half-period.
        let (on2, _, changed2) = advance_blink(on, start, true, start + CURSOR_BLINK_HALF);
        assert!(on2, "toggled back on");
        assert!(changed2);
    }

    #[test]
    fn steady_cursor_stays_solid_and_never_toggles() {
        let t0 = Instant::now();
        // Already on + not blinking → no change even long after the period.
        let (on, _, changed) = advance_blink(true, t0, false, t0 + CURSOR_BLINK_HALF * 10);
        assert!(on);
        assert!(!changed, "a steady cursor must not blink");
    }

    #[test]
    fn switching_to_steady_mid_off_restores_solid_cursor() {
        let t0 = Instant::now();
        // Cursor was mid-"off" (blink_on=false) and is no longer blinking →
        // restore solid and force one redraw.
        let (on, _, changed) = advance_blink(false, t0, false, t0);
        assert!(on, "restored to solid");
        assert!(changed, "repaint the now-solid cursor");
    }
}
