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
    /// Parse `KMUX_NET_DELAY_MS`, `KMUX_NET_JITTER_MS`, `KMUX_NET_SEED`.
    /// Returns `None` when both delay and jitter are zero/absent (the shim is
    /// then a complete no-op).
    fn from_env() -> Option<Self> {
        let delay_ms = env_u64("KMUX_NET_DELAY_MS");
        let jitter_ms = env_u64("KMUX_NET_JITTER_MS");
        if delay_ms == 0 && jitter_ms == 0 {
            return None;
        }
        let seed = std::env::var("KMUX_NET_SEED")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        Some(Self {
            delay_ms,
            jitter_ms,
            seed,
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

fn env_u64(key: &str) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn nondeterministic_seed() -> u64 {
    // Date/time is fine in the real daemon (only the workflow sandbox forbids
    // it). A coarse wall-clock seed is plenty for jitter we never verify.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9ABC_DEF0)
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

/// Tiny dependency-free seedable PRNG (SplitMix64). Deterministic given a seed;
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

    #[test]
    fn from_env_none_when_unset() {
        // Both knobs absent → no impairment. (Env is process-global; this test
        // relies on the vars not being set in the test environment.)
        unsafe {
            std::env::remove_var("KMUX_NET_DELAY_MS");
            std::env::remove_var("KMUX_NET_JITTER_MS");
        }
        assert!(ImpairConfig::from_env().is_none());
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
