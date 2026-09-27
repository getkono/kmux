//! Automatic reconnect (issue #208): when the link to the daemon drops, the
//! driver retries on its own with backoff, keeps the UI live, buffers what the
//! user types meanwhile, and says so.
//!
//! Three pieces, all pure (time is a parameter), so GTK and Swift share the
//! policy through [`super::FrontendDriver`]:
//!
//! - [`Reconnect`] schedules the attempts, with the delays of
//!   [`kmux_client::backoff`].
//! - [`OutageInput`] holds the input typed while the link is down — bounded, in
//!   order, counting what did not fit — and hands it back on reconnect.
//! - [`connection_banner`] turns both into the one line a frontend shows.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use kmux_client::backoff::next_delay;
use kmux_protocol::messages::ClientMessage;

/// Failed attempts after which the banner calls the daemon unreachable rather
/// than merely reconnecting. The retries go on either way.
pub const UNREACHABLE_AFTER_ATTEMPTS: u32 = 5;

/// How many keystrokes typed during an outage are kept for delivery on
/// reconnect. A paste or a raw input write counts as one.
pub const OUTAGE_INPUT_CAPACITY: usize = 256;

/// How long after a reconnect the banner keeps reporting keystrokes an outage
/// dropped, so the notice outlives the outage long enough to be read.
pub const DROPPED_NOTICE: Duration = Duration::from_secs(8);

/// The reconnect schedule of one link.
#[derive(Debug, Clone)]
pub struct Reconnect {
    jitter_seed: u64,
    state: Schedule,
    /// Why the last attempt failed, until the link is back.
    last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schedule {
    /// The link is up (or was never lost): nothing to retry.
    Idle,
    /// Attempt `attempt` (1-based) starts at `due`.
    Waiting { attempt: u32, due: Instant },
    /// Attempt `attempt` is under way.
    Trying { attempt: u32 },
}

impl Reconnect {
    pub fn new(jitter_seed: u64) -> Self {
        Self {
            jitter_seed,
            state: Schedule::Idle,
            last_error: None,
        }
    }

    /// The link dropped at `now`: schedule the first attempt.
    pub fn on_lost(&mut self, now: Instant) {
        self.last_error = None;
        self.schedule(1, now);
    }

    /// Whether the link is down and being retried.
    pub fn is_active(&self) -> bool {
        self.state != Schedule::Idle
    }

    /// The current (or next) attempt, 1-based; `None` while the link is up.
    pub fn attempt(&self) -> Option<u32> {
        match self.state {
            Schedule::Idle => None,
            Schedule::Waiting { attempt, .. } | Schedule::Trying { attempt } => Some(attempt),
        }
    }

    /// Whether enough attempts failed to call the daemon unreachable.
    pub fn is_unreachable(&self) -> bool {
        self.attempt()
            .is_some_and(|attempt| attempt > UNREACHABLE_AFTER_ATTEMPTS)
    }

    /// How long until the next attempt starts; `None` unless one is waiting.
    pub fn next_in(&self, now: Instant) -> Option<Duration> {
        match self.state {
            Schedule::Waiting { due, .. } => Some(due.saturating_duration_since(now)),
            _ => None,
        }
    }

    /// The attempt to start now, if one is due: it is then under way until
    /// [`Self::on_failed`] or [`Self::on_connected`].
    pub fn take_due(&mut self, now: Instant) -> Option<u32> {
        match self.state {
            Schedule::Waiting { attempt, due } if now >= due => {
                self.state = Schedule::Trying { attempt };
                Some(attempt)
            }
            _ => None,
        }
    }

    /// The attempt under way failed at `now` with `error`: schedule the next
    /// one.
    pub fn on_failed(&mut self, now: Instant, error: String) {
        if let Schedule::Trying { attempt } = self.state {
            self.last_error = Some(error);
            self.schedule(attempt + 1, now);
        }
    }

    /// Why the last attempt failed, while the link is still down.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// "Reconnect now": a waiting attempt starts at `now` instead of later.
    pub fn retry_now(&mut self, now: Instant) {
        if let Schedule::Waiting { attempt, .. } = self.state {
            self.state = Schedule::Waiting { attempt, due: now };
        }
    }

    /// The link is back: stop retrying.
    pub fn on_connected(&mut self) {
        self.state = Schedule::Idle;
        self.last_error = None;
    }

    fn schedule(&mut self, attempt: u32, now: Instant) {
        let due = now + next_delay(attempt - 1, self.jitter_seed);
        self.state = Schedule::Waiting { attempt, due };
    }
}

/// Input typed while the link is down, kept in order for delivery on
/// reconnect, up to [`OUTAGE_INPUT_CAPACITY`] keystrokes.
#[derive(Debug, Default)]
pub struct OutageInput {
    queued: VecDeque<ClientMessage>,
    keystrokes: usize,
    dropped: u64,
    dropped_notice_until: Option<Instant>,
}

/// How many keystrokes one input message carries.
fn keystrokes(msg: &ClientMessage) -> usize {
    match msg {
        ClientMessage::PtyKeyBatch { events, .. } => events.len(),
        _ => 1,
    }
}

impl OutageInput {
    /// Keep `msg` for delivery on reconnect, or count it as dropped when it
    /// does not fit.
    pub fn push(&mut self, msg: ClientMessage) {
        let weight = keystrokes(&msg);
        if self.keystrokes + weight > OUTAGE_INPUT_CAPACITY {
            self.dropped += weight as u64;
        } else {
            self.keystrokes += weight;
            self.queued.push_back(msg);
        }
    }

    /// Keystrokes waiting for the link.
    pub fn queued(&self) -> usize {
        self.keystrokes
    }

    /// Keystrokes this outage could not keep.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// A new outage begins: forget what the last one dropped.
    pub fn begin(&mut self) {
        self.dropped = 0;
        self.dropped_notice_until = None;
    }

    /// Everything kept, oldest first, for sending on reconnect at `now`. What
    /// the outage dropped stays reported for [`DROPPED_NOTICE`] after it.
    pub fn flush(&mut self, now: Instant) -> Vec<ClientMessage> {
        self.keystrokes = 0;
        if self.dropped > 0 {
            self.dropped_notice_until = Some(now + DROPPED_NOTICE);
        }
        self.queued.drain(..).collect()
    }

    /// Keystrokes dropped by an outage that ended shortly before `now`, while
    /// the notice lasts.
    pub fn dropped_notice(&self, now: Instant) -> Option<u64> {
        self.dropped_notice_until
            .filter(|until| now < *until)
            .map(|_| self.dropped)
    }
}

/// The one line a frontend shows about the link, and whether it offers
/// "Reconnect now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionBanner {
    /// What to show, e.g. `Reconnecting… attempt 2, next try in 1 s · 3
    /// keystrokes queued`.
    pub text: String,
    /// Whether the link is down and a "Reconnect now" action applies.
    pub reconnecting: bool,
    /// Whether the retries have gone on long enough to call it unreachable.
    pub unreachable: bool,
    /// Keystrokes waiting for the link.
    pub queued: u64,
    /// Keystrokes the current (or just-ended) outage dropped.
    pub dropped: u64,
}

/// The banner for this moment: `None` while the link is up and nothing is
/// left to report.
pub fn connection_banner(
    reconnect: &Reconnect,
    input: &OutageInput,
    now: Instant,
) -> Option<ConnectionBanner> {
    let Some(attempt) = reconnect.attempt() else {
        let dropped = input.dropped_notice(now)?;
        return Some(ConnectionBanner {
            text: format!(
                "Reconnected · {dropped} keystrokes typed while disconnected were dropped"
            ),
            reconnecting: false,
            unreachable: false,
            queued: 0,
            dropped,
        });
    };
    let unreachable = reconnect.is_unreachable();
    let mut text = if unreachable {
        format!("Daemon unreachable · retrying (attempt {attempt})")
    } else {
        format!("Reconnecting… attempt {attempt}")
    };
    match reconnect.next_in(now) {
        Some(wait) => text.push_str(&format!(", next try in {} s", wait.as_secs_f64().ceil())),
        None => text.push_str(", trying now"),
    }
    let queued = input.queued() as u64;
    if queued > 0 {
        text.push_str(&format!(" · {queued} keystrokes queued"));
    }
    let dropped = input.dropped();
    if dropped > 0 {
        text.push_str(&format!(" · {dropped} dropped"));
    }
    if let Some(error) = reconnect.last_error() {
        text.push_str(&format!(" — {error}"));
    }
    Some(ConnectionBanner {
        text,
        reconnecting: true,
        unreachable,
        queued,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    use kmux_client::backoff::BACKOFF_MIN;
    use kmux_protocol::messages::{KeyAction, KeyCode, KeyEvent, KeyMods};

    use super::*;

    /// A keypress typing `c` (the code is immaterial here; the text is what
    /// the tests read back).
    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::A,
            mods: KeyMods::default(),
            action: KeyAction::Press,
            text: c.to_string(),
            unshifted_codepoint: 0,
        }
    }

    fn keys(pane: &str, chars: &str) -> ClientMessage {
        ClientMessage::PtyKeyBatch {
            pane_id: pane.to_string(),
            events: chars.chars().map(key).collect(),
        }
    }

    fn typed(msgs: &[ClientMessage]) -> String {
        msgs.iter()
            .map(|msg| match msg {
                ClientMessage::PtyKeyBatch { events, .. } => {
                    events.iter().map(|e| e.text.as_str()).collect::<String>()
                }
                ClientMessage::PtyPaste { data, .. } => data.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    // ── Reconnect ───────────────────────────────────────────────────────────

    /// A lost link is retried with the backoff delays, one attempt at a time,
    /// and a reconnect ends the retries.
    #[test]
    fn reconnect_retries_on_the_backoff_schedule_until_connected() {
        let seed = 0;
        let t0 = Instant::now();
        let mut reconnect = Reconnect::new(seed);
        assert!(!reconnect.is_active());
        assert_eq!(reconnect.take_due(t0), None);

        reconnect.on_lost(t0);
        let first = t0 + next_delay(0, seed);
        assert_eq!(reconnect.attempt(), Some(1));
        assert_eq!(reconnect.next_in(t0), Some(next_delay(0, seed)));
        assert_eq!(
            reconnect.take_due(first - Duration::from_millis(1)),
            None,
            "not before its delay"
        );
        assert_eq!(reconnect.take_due(first), Some(1));
        assert_eq!(reconnect.take_due(first), None, "one attempt at a time");
        assert_eq!(reconnect.next_in(first), None, "under way");

        reconnect.on_failed(first, "refused".to_string());
        assert_eq!(reconnect.attempt(), Some(2));
        assert_eq!(reconnect.next_in(first), Some(next_delay(1, seed)));
        let second = first + next_delay(1, seed);
        assert_eq!(reconnect.take_due(second), Some(2));

        assert_eq!(reconnect.last_error(), Some("refused"));

        reconnect.on_connected();
        assert!(!reconnect.is_active());
        assert_eq!(reconnect.attempt(), None);
        assert_eq!(
            reconnect.last_error(),
            None,
            "forgotten once the link is back"
        );

        // A new outage starts without the last one's error.
        reconnect.on_lost(second);
        reconnect.take_due(second + BACKOFF_MIN);
        reconnect.on_failed(second, "gone".to_string());
        reconnect.on_lost(second);
        assert_eq!(reconnect.last_error(), None);
    }

    /// "Reconnect now" starts a waiting attempt at once; with nothing waiting
    /// it changes nothing.
    #[test]
    fn reconnect_now_starts_a_waiting_attempt_at_once() {
        let t0 = Instant::now();
        let mut reconnect = Reconnect::new(0);
        reconnect.retry_now(t0);
        assert!(!reconnect.is_active(), "nothing to retry while connected");

        reconnect.on_lost(t0);
        reconnect.retry_now(t0);
        assert_eq!(reconnect.take_due(t0), Some(1));
        reconnect.retry_now(t0);
        assert_eq!(
            reconnect.take_due(t0),
            None,
            "the attempt is already under way"
        );
    }

    /// A failure report with no attempt under way schedules nothing.
    #[test]
    fn reconnect_ignores_a_failure_with_no_attempt_under_way() {
        let t0 = Instant::now();
        let mut reconnect = Reconnect::new(0);
        reconnect.on_failed(t0, "refused".to_string());
        assert!(!reconnect.is_active());
        reconnect.on_lost(t0);
        reconnect.on_failed(t0, "refused".to_string());
        assert_eq!(reconnect.attempt(), Some(1), "still waiting for attempt 1");
    }

    /// The daemon is called unreachable only once the threshold is passed.
    #[test]
    fn reconnect_is_unreachable_after_the_threshold() {
        let mut now = Instant::now();
        let mut reconnect = Reconnect::new(0);
        reconnect.on_lost(now);
        for attempt in 1..=UNREACHABLE_AFTER_ATTEMPTS {
            assert!(!reconnect.is_unreachable(), "attempt {attempt}");
            now += Duration::from_secs(60);
            assert_eq!(reconnect.take_due(now), Some(attempt));
            reconnect.on_failed(now, "refused".to_string());
        }
        assert!(reconnect.is_unreachable());
    }

    // ── OutageInput ─────────────────────────────────────────────────────────

    /// Keystrokes up to the capacity are kept in order; what does not fit is
    /// counted as dropped; a flush hands the kept ones back and empties it.
    #[test]
    fn outage_input_keeps_up_to_capacity_in_order_and_counts_the_rest() {
        let now = Instant::now();
        let mut input = OutageInput::default();
        input.push(keys("eagle/0", "ls"));
        input.push(ClientMessage::PtyPaste {
            pane_id: "eagle/0".to_string(),
            data: " -la".to_string(),
        });
        let filler = "x".repeat(OUTAGE_INPUT_CAPACITY - 3);
        input.push(keys("eagle/0", &filler));
        assert_eq!(input.queued(), OUTAGE_INPUT_CAPACITY);
        assert_eq!(input.dropped(), 0);

        input.push(keys("eagle/0", "\r!"));
        assert_eq!(
            input.dropped(),
            2,
            "a batch that does not fit is dropped whole"
        );
        assert_eq!(input.queued(), OUTAGE_INPUT_CAPACITY);

        let flushed = input.flush(now);
        assert_eq!(typed(&flushed), format!("ls -la{filler}"));
        assert_eq!(input.queued(), 0);
        assert!(input.flush(now).is_empty(), "a flush empties it");
    }

    /// The dropped count is reported for a while after the outage, then reset.
    #[test]
    fn outage_input_reports_dropped_keystrokes_for_a_while_after_the_outage() {
        let now = Instant::now();
        let mut input = OutageInput::default();
        assert_eq!(input.dropped_notice(now), None);
        input.push(keys("eagle/0", &"x".repeat(OUTAGE_INPUT_CAPACITY + 3)));
        input.flush(now);

        assert_eq!(input.dropped_notice(now + DROPPED_NOTICE / 2), Some(259));
        assert_eq!(input.dropped_notice(now + DROPPED_NOTICE), None);

        // The next outage starts from nothing dropped.
        input.begin();
        assert_eq!(input.dropped(), 0);
        assert_eq!(input.dropped_notice(now), None);
    }

    /// An outage that dropped nothing leaves nothing to report.
    #[test]
    fn outage_input_with_nothing_dropped_reports_nothing() {
        let now = Instant::now();
        let mut input = OutageInput::default();
        input.push(keys("eagle/0", "a"));
        input.flush(now);
        assert_eq!(input.dropped_notice(now), None);
    }

    // ── Banner ──────────────────────────────────────────────────────────────

    #[test]
    fn banner_is_absent_while_connected_with_nothing_to_report() {
        let reconnect = Reconnect::new(0);
        let input = OutageInput::default();
        assert_eq!(connection_banner(&reconnect, &input, Instant::now()), None);
    }

    /// While reconnecting the banner names the attempt, the wait, and what
    /// was typed meanwhile.
    #[test]
    fn banner_while_reconnecting_names_the_attempt_the_wait_and_the_input() {
        let t0 = Instant::now();
        let mut reconnect = Reconnect::new(0);
        reconnect.on_lost(t0);
        let mut input = OutageInput::default();
        input.push(keys("eagle/0", "ab"));
        input.push(keys("eagle/0", &"x".repeat(OUTAGE_INPUT_CAPACITY)));

        let banner = connection_banner(&reconnect, &input, t0).unwrap();
        assert_eq!(
            banner,
            ConnectionBanner {
                text:
                    "Reconnecting… attempt 1, next try in 1 s · 2 keystrokes queued · 256 dropped"
                        .to_string(),
                reconnecting: true,
                unreachable: false,
                queued: 2,
                dropped: 256,
            }
        );

        reconnect.take_due(t0 + Duration::from_secs(1));
        let banner = connection_banner(&reconnect, &input, t0).unwrap();
        assert!(
            banner
                .text
                .ends_with(", trying now · 2 keystrokes queued · 256 dropped")
        );
    }

    /// Past the threshold the banner says the daemon is unreachable.
    #[test]
    fn banner_past_the_threshold_says_unreachable() {
        let mut now = Instant::now();
        let mut reconnect = Reconnect::new(0);
        reconnect.on_lost(now);
        for _ in 0..=UNREACHABLE_AFTER_ATTEMPTS {
            now += Duration::from_secs(60);
            reconnect.take_due(now);
            reconnect.on_failed(now, "refused".to_string());
        }
        let banner = connection_banner(&reconnect, &OutageInput::default(), now).unwrap();
        assert!(banner.unreachable);
        assert_eq!(
            banner.text,
            "Daemon unreachable · retrying (attempt 7), next try in 15 s — refused"
        );
    }

    /// After a reconnect the banner reports what the outage dropped, then goes.
    #[test]
    fn banner_after_reconnect_reports_dropped_keystrokes_then_goes() {
        let now = Instant::now();
        let reconnect = Reconnect::new(0);
        let mut input = OutageInput::default();
        input.push(keys("eagle/0", &"x".repeat(OUTAGE_INPUT_CAPACITY + 1)));
        input.flush(now);

        let banner = connection_banner(&reconnect, &input, now).unwrap();
        assert_eq!(
            banner,
            ConnectionBanner {
                text: "Reconnected · 257 keystrokes typed while disconnected were dropped"
                    .to_string(),
                reconnecting: false,
                unreachable: false,
                queued: 0,
                dropped: 257,
            }
        );
        assert_eq!(
            connection_banner(&reconnect, &input, now + DROPPED_NOTICE),
            None
        );
    }
}
