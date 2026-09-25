//! Network impairment shim for diagnosing shell tearing (issue #72).
//!
//! When `KMUX_NET_DELAY_MS` / `KMUX_NET_JITTER_MS` are set, the per-pane writer
//! tasks sleep `delay + rand(0..=jitter)` ms before sending each **pane-data**
//! frame (`Shell` / `Scrollback` categories). This injects the high-latency,
//! high-jitter conditions under which a single logical screen paint — emitted by
//! the daemon as several diffs within one 60 Hz window — can land in different
//! client pump ticks and tear.
//!
//! Only pane-data frames are delayed. Liveness (`Ping`/`Pong`) and control
//! frames travel on a separate path and are never impaired, so the client's
//! liveness timeout is unaffected.
//!
//! The delay covers **every transport's** per-pane sender: the QUIC
//! `pane_uni_writer` (`connection.rs`) and the TCP/UDS `TcpAttacher`
//! (`tcp_listener.rs`), each applied before the frame reaches the shared writer
//! so batching never coalesces a delayed frame back together (issue #182, §5).
//!
//! The other adverse condition the grid-digest oracle must survive — a slow
//! client whose per-client data channel overflows to `ServerMessage::Lagged`
//! (issue #68) — is exercised deterministically by
//! `relay::tests::oracle_survives_data_channel_overflow_lagged`, which forces
//! the overflow with a tiny channel and asserts the digest stays clean across
//! the resync.
//!
//! The shim is **zero-cost when unset**: [`config`] returns `None` and the
//! writer hot paths skip the delay entirely. `KMUX_NET_SEED` makes the jitter
//! reproducible across runs.

use std::sync::OnceLock;
use std::time::Duration;

use kmux_protocol::messages::MessageCategory;
use tracing::info;

/// Parsed impairment knobs. Present only when at least one of delay/jitter > 0.
#[derive(Debug, Clone, Copy)]
pub struct ImpairConfig {
    pub delay_ms: u64,
    pub jitter_ms: u64,
    pub seed: Option<u64>,
}

impl ImpairConfig {
    /// Parse `KMUX_NET_DELAY_MS`, `KMUX_NET_JITTER_MS`, `KMUX_NET_SEED` from
    /// the process environment.
    fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// The parsing rules, with the variable lookup passed in so they are
    /// testable without mutating the process environment (docs/testing.md R3).
    /// Returns `None` when both delay and jitter are zero/absent (the shim is
    /// then a complete no-op). An unparsable value reads as absent.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let parse = |key: &str| lookup(key).and_then(|v| v.trim().parse::<u64>().ok());
        let delay_ms = parse("KMUX_NET_DELAY_MS").unwrap_or(0);
        let jitter_ms = parse("KMUX_NET_JITTER_MS").unwrap_or(0);
        if delay_ms == 0 && jitter_ms == 0 {
            return None;
        }
        Some(Self {
            delay_ms,
            jitter_ms,
            seed: parse("KMUX_NET_SEED"),
        })
    }

    /// Create a per-task RNG. `salt` (e.g. a hash of the pane id) keeps
    /// concurrent streams from drawing identical jitter sequences while staying
    /// deterministic when `KMUX_NET_SEED` is set.
    pub fn rng_for(&self, salt: u64) -> SplitMix64 {
        let base = self.seed.unwrap_or_else(nondeterministic_seed);
        SplitMix64::new(base ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

/// Cheap stable hash of a pane id, used as a per-stream RNG salt so concurrent
/// panes draw distinct jitter while a given seed stays reproducible.
pub fn pane_salt(pane_id: &str) -> u64 {
    pane_id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

fn nondeterministic_seed() -> u64 {
    // Date/time is fine in the real daemon (only the workflow sandbox forbids
    // it). A coarse wall-clock seed is plenty for jitter we never verify.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x1234_5678_9ABC_DEF0, |d| d.as_nanos() as u64)
}

static IMPAIR: OnceLock<Option<ImpairConfig>> = OnceLock::new();

/// The process-wide impairment config, parsed once from the environment.
pub fn config() -> Option<&'static ImpairConfig> {
    IMPAIR.get_or_init(ImpairConfig::from_env).as_ref()
}

/// Force-parse the config and log a line if impairment is active. Called once
/// at daemon startup so the operator sees the knobs in the log.
pub fn init_and_log() {
    if let Some(cfg) = config() {
        info!(
            delay_ms = cfg.delay_ms,
            jitter_ms = cfg.jitter_ms,
            seed = ?cfg.seed,
            "network impairment ACTIVE (KMUX_NET_*) — pane-data frames are delayed"
        );
    }
}

/// Sleep the configured delay+jitter before sending a pane-data frame.
/// No-op for non-pane categories (liveness, control, sync, bootstrap).
pub async fn maybe_delay(cfg: &ImpairConfig, category: MessageCategory, rng: &mut SplitMix64) {
    if !matches!(
        category,
        MessageCategory::Shell | MessageCategory::Scrollback
    ) {
        return;
    }
    let jitter = if cfg.jitter_ms == 0 {
        0
    } else {
        rng.next_u64() % (cfg.jitter_ms + 1)
    };
    let total = cfg.delay_ms + jitter;
    if total > 0 {
        tokio::time::sleep(Duration::from_millis(total)).await;
    }
}

/// Tiny dependency-free seedable PRNG (`SplitMix64`). Deterministic given a seed;
/// adequate for jitter we never statistically verify.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup over a fixed table, standing in for the environment.
    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn from_lookup_unset_is_none() {
        assert!(ImpairConfig::from_lookup(lookup(&[])).is_none());
    }

    #[test]
    fn from_lookup_zero_or_garbage_knobs_are_none() {
        let vars = [("KMUX_NET_DELAY_MS", "0"), ("KMUX_NET_JITTER_MS", "lots")];
        assert!(ImpairConfig::from_lookup(lookup(&vars)).is_none());
    }

    #[test]
    fn from_lookup_parses_every_knob() {
        let vars = [
            ("KMUX_NET_DELAY_MS", " 40 "),
            ("KMUX_NET_JITTER_MS", "15"),
            ("KMUX_NET_SEED", "9"),
        ];
        let cfg = ImpairConfig::from_lookup(lookup(&vars)).expect("active");
        assert_eq!((cfg.delay_ms, cfg.jitter_ms, cfg.seed), (40, 15, Some(9)));
    }

    /// `from_env`'s only job is reading the process environment, which a test
    /// may not mutate (R3). So it is checked in a child — this same test binary,
    /// re-run on this one test — handed the knobs through `Command::env` (R7:
    /// the in-process tier cannot set them without mutating this process).
    #[test]
    fn from_env_reads_the_knobs_from_the_process_environment() {
        const PROBE: &str = "KMUX_TEST_IMPAIR_FROM_ENV_CHILD";
        const NAME: &str = "impair::tests::from_env_reads_the_knobs_from_the_process_environment";
        if std::env::var_os(PROBE).is_some() {
            let cfg = ImpairConfig::from_env().expect("the parent set the knobs");
            assert_eq!((cfg.delay_ms, cfg.jitter_ms, cfg.seed), (40, 15, Some(9)));
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--test-threads=1"])
            .env(PROBE, "1")
            .env("KMUX_NET_DELAY_MS", "40")
            .env("KMUX_NET_JITTER_MS", "15")
            .env("KMUX_NET_SEED", "9")
            .output()
            .expect("re-run the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // "1 passed" also proves the filter matched: a filter that selects
        // nothing exits 0 too.
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child from_env did not read the knobs:\n{stdout}"
        );
    }

    #[test]
    fn from_lookup_jitter_alone_activates_without_a_seed() {
        let cfg =
            ImpairConfig::from_lookup(lookup(&[("KMUX_NET_JITTER_MS", "5")])).expect("active");
        assert_eq!((cfg.delay_ms, cfg.jitter_ms, cfg.seed), (0, 5, None));
    }

    #[test]
    fn splitmix_is_deterministic() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        assert_eq!(a.next_u64(), b.next_u64());
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn rng_for_is_seed_deterministic_and_salt_varied() {
        let cfg = ImpairConfig {
            delay_ms: 10,
            jitter_ms: 20,
            seed: Some(7),
        };
        // Same seed + salt → identical stream.
        assert_eq!(cfg.rng_for(1).next_u64(), cfg.rng_for(1).next_u64());
        // Different salt → (almost certainly) different stream.
        assert_ne!(cfg.rng_for(1).next_u64(), cfg.rng_for(2).next_u64());
    }

    #[tokio::test]
    async fn maybe_delay_skips_liveness() {
        let cfg = ImpairConfig {
            delay_ms: 10_000,
            jitter_ms: 0,
            seed: Some(1),
        };
        let mut rng = cfg.rng_for(0);
        // A 10s delay would hang the test if it were applied; liveness must skip.
        tokio::time::timeout(
            Duration::from_millis(200),
            maybe_delay(&cfg, MessageCategory::Liveness, &mut rng),
        )
        .await
        .expect("liveness frames must not be delayed");
    }
}
