//! End-to-end regression for the terminal query-response path.
//!
//! Full-screen and interactive programs send terminal queries (DSR/DA/…) and
//! block until the emulator replies. Before this path existed, kmux parsed those
//! queries but never wrote a reply back to the child, so programs stalled until a
//! timeout or the next keypress (delayed `vim :q` repaint, invisible `fzf`).
//!
//! This test drives the real daemon with a child that emits `CSI 6 n` (DSR
//! cursor-position report), reads exactly the 6-byte reply back from its stdin,
//! and echoes it visibly via `cat -v`. If the reply never arrives the child
//! blocks forever and the test times out; when it works, `^[[1;1R` appears on
//! the grid — proving the query → reply → child round-trip completes with **no**
//! user input. It runs under both the in-process and process-isolated engines,
//! since the fix must behave identically across that seam.

#![cfg(unix)]

mod harness;

use harness::{Cleanup, Daemon, Sandbox, Screen, connect_client, create_and_attach, daemon_token};

/// The DSR cursor-position round-trip: the child emits `CSI 6 n`, reads the
/// 6-byte reply the daemon writes back, and echoes it via `cat -v` as `^[[1;1R`.
/// A missing reply blocks the child forever and this times out.
async fn assert_dsr_roundtrip(isolated: bool) {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();

    let daemon = Daemon::new(&sandbox);
    let daemon = if isolated { daemon.isolated() } else { daemon };
    cleanup.track(daemon.spawn(None).await.cast_signed());

    let token = daemon_token(&sandbox).await;
    let mut client = connect_client(&sandbox, &token).await;
    // Emit DSR, read back exactly the 6-byte `\x1b[1;1R` reply, echo it visibly.
    let pane = create_and_attach(
        &mut client,
        1,
        Some(&["/bin/sh", "-c", "printf '\\033[6n'; head -c 6 | cat -v"]),
    )
    .await;

    let mut screen = Screen::new(&pane);
    assert!(
        screen.follow_until(&mut client, "[1;1R").await,
        "the DSR cursor-position reply must round-trip back to the child and \
         render (looked for `^[[1;1R` via `cat -v`); grid was {:?}, and the \
         daemon logged:\n{}",
        screen.text().trim_end(),
        sandbox.daemon_log()
    );
}

#[tokio::test]
async fn dsr_query_reply_reaches_child_in_process() {
    assert_dsr_roundtrip(false).await;
}

#[tokio::test]
async fn dsr_query_reply_reaches_child_isolated_worker() {
    assert_dsr_roundtrip(true).await;
}
