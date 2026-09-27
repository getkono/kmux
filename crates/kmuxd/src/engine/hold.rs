//! Holding a pane's PTY reader at a clean point, and letting it go again
//! (issue #207).
//!
//! A graceful handoff writes its final checkpoint *before* the commit point, so
//! that nothing fallible is left after it. The checkpoint must hold exactly
//! the bytes this daemon consumed — everything after it stays in the kernel PTY
//! buffer for the successor — so the readers stop first. Stopping used to mean
//! aborting the relay task, which cannot be undone, and a handoff that then
//! failed before its commit point left the daemon serving panes nobody read.
//! A hold is reversible: the reader parks between two reads, and resumes when
//! the hold is released.
//!
//! [`channel`] makes the two ends. The pane engine keeps the [`HoldControl`];
//! the relay loop (`relay::session_diff_loop`) takes the [`HoldPoint`] and
//! selects on [`HoldPoint::requested`] beside its read.

use tokio::sync::watch;

/// The engine's end: ask the reader to park, or let it go.
#[derive(Debug)]
pub struct HoldControl {
    held: watch::Sender<bool>,
    parked: watch::Receiver<bool>,
}

/// The reader's end: learn a hold was asked for, and park until released.
#[derive(Debug)]
pub struct HoldPoint {
    held: watch::Receiver<bool>,
    parked: watch::Sender<bool>,
}

/// A connected [`HoldControl`] and [`HoldPoint`], not held.
pub fn channel() -> (HoldControl, HoldPoint) {
    let (held_tx, held_rx) = watch::channel(false);
    let (parked_tx, parked_rx) = watch::channel(false);
    (
        HoldControl {
            held: held_tx,
            parked: parked_rx,
        },
        HoldPoint {
            held: held_rx,
            parked: parked_tx,
        },
    )
}

impl HoldControl {
    /// Ask the reader to park. The returned future resolves once it has — it
    /// is then between two reads, with everything it read fed on — or once
    /// the reader is gone (its pane ended), since a reader that is gone reads
    /// nothing either. It borrows nothing, so the caller need not hold the
    /// lock the engine lives under while it waits.
    pub fn hold(&self) -> impl Future<Output = ()> + Send + 'static {
        self.held.send_replace(true);
        let mut parked = self.parked.clone();
        async move {
            let _ = parked.wait_for(|parked| *parked).await;
        }
    }

    /// Let the reader go on.
    pub fn release(&self) {
        self.held.send_replace(false);
    }
}

impl HoldPoint {
    /// Resolves when a hold is asked for. Cancel-safe, for use in `select!`.
    /// Never resolves once the [`HoldControl`] is gone.
    pub async fn requested(&mut self) {
        // A control dropped mid-hold leaves `true` behind; it must not read as
        // a hold, or the reader would park and wake in a loop.
        let control_gone = self.held.has_changed().is_err();
        if control_gone || self.held.wait_for(|held| *held).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Report the reader parked and wait until the hold is released (or its
    /// [`HoldControl`] is gone).
    pub async fn park(&mut self) {
        self.parked.send_replace(true);
        let _ = self.held.wait_for(|held| !*held).await;
        self.parked.send_replace(false);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    /// A hold resolves only once the reader has parked, and a release lets
    /// the parked reader go on.
    #[tokio::test]
    async fn a_hold_waits_for_the_reader_to_park_and_a_release_frees_it() {
        let (control, mut point) = channel();
        let held = tokio::spawn(control.hold());
        tokio::task::yield_now().await;
        assert!(!held.is_finished(), "nobody has parked yet");

        tokio::time::timeout(WAIT, point.requested())
            .await
            .expect("the reader sees the hold");
        let parked = tokio::spawn(async move {
            point.park().await;
            point
        });
        tokio::time::timeout(WAIT, held)
            .await
            .expect("the hold resolves once parked")
            .unwrap();
        assert!(!parked.is_finished(), "parked until released");

        control.release();
        let mut point = tokio::time::timeout(WAIT, parked)
            .await
            .expect("a release lets the reader go on")
            .unwrap();
        let again = tokio::time::timeout(Duration::from_millis(50), point.requested()).await;
        assert!(again.is_err(), "no hold is pending after a release");
    }

    /// A hold on a reader that has ended resolves at once: nothing reads.
    #[tokio::test]
    async fn a_hold_on_a_reader_that_is_gone_resolves() {
        let (control, point) = channel();
        drop(point);
        tokio::time::timeout(WAIT, control.hold())
            .await
            .expect("resolves without a reader");
    }

    /// Once its control is gone a reader is never asked to hold, and a
    /// parked reader is let go.
    #[tokio::test]
    async fn a_reader_whose_control_is_gone_is_never_held() {
        let (control, mut point) = channel();
        drop(control.hold());
        drop(control);
        tokio::time::timeout(WAIT, point.park())
            .await
            .expect("a parked reader is let go");
        let asked = tokio::time::timeout(Duration::from_millis(50), point.requested()).await;
        assert!(asked.is_err(), "never asked again");
    }
}
