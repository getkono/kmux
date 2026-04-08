use std::time::Duration;

use tokio::time::timeout;

use crate::error::{KmuxError, Result};

/// A readiness probe that checks if some condition is met.
///
/// Returns `true` when the target is ready, `false` to continue waiting.
pub type ProbeFn<T> = Box<dyn Fn(&T) -> bool + Send + Sync>;

/// Wait until a probe function returns `true`, polling at a fixed interval.
///
/// - `probe`: called with `target` on each poll tick
/// - `target`: the thing being probed (e.g., buffered output so far)
/// - `poll_interval`: how often to poll
/// - `deadline`: maximum time to wait before returning `Err(KmuxError::Timeout)`
pub async fn wait_until_ready<T: Send>(
    target: &T,
    probe: &ProbeFn<T>,
    poll_interval: Duration,
    deadline: Duration,
) -> Result<()> {
    let result = timeout(deadline, async {
        loop {
            if probe(target) {
                return;
            }
            tokio::time::sleep(poll_interval).await;
        }
    })
    .await;

    result.map_err(|_| KmuxError::Timeout)
}

/// A string-match probe: ready when the output buffer contains `pattern`.
pub fn contains_probe(pattern: &str) -> ProbeFn<String> {
    let pattern = pattern.to_string();
    Box::new(move |s: &String| s.contains(&pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_succeeds_immediately() {
        let target = "hello world".to_string();
        let probe = contains_probe("hello");
        let result = wait_until_ready(
            &target,
            &probe,
            Duration::from_millis(10),
            Duration::from_millis(100),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn probe_times_out() {
        let target = "nothing here".to_string();
        let probe = contains_probe("expected pattern");
        let result = wait_until_ready(
            &target,
            &probe,
            Duration::from_millis(10),
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Err(KmuxError::Timeout)));
    }
}
