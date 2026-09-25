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

use std::time::{Duration, Instant};

use harness::{
    Cleanup, Client, Daemon, SIZE, Sandbox, connect_client, create_and_attach, daemon_token,
};
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::ServerMessage;

/// Reconstruct the pane's grid from the daemon's messages into one flat string,
/// applying updates until `pred` matches the accumulated text or `timeout`.
async fn grid_text_until(
    client: &mut Client,
    pane_id: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut grid = CellGrid::new(SIZE.rows as usize, SIZE.cols as usize);
    let deadline = Instant::now() + timeout;
    loop {
        let text: String = grid.to_snapshot().cells.iter().map(|c| c.c).collect();
        if pred(&text) {
            return text;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return text;
        }
        let Ok(Some(msg)) = tokio::time::timeout(remaining, client.rx.recv()).await else {
            return grid.to_snapshot().cells.iter().map(|c| c.c).collect();
        };
        match msg {
            ServerMessage::TerminalSnapshot {
                pane_id: p,
                snapshot,
                ..
            } if p == pane_id => {
                grid.apply_snapshot((*snapshot).clone());
            }
            ServerMessage::TerminalUpdate {
                pane_id: p, diff, ..
            } if p == pane_id => {
                grid.apply_diff((*diff).clone());
            }
            ServerMessage::CursorUpdate {
                pane_id: p,
                cursor,
                modes,
                ..
            } if p == pane_id => grid.apply_cursor_update(cursor, modes),
            _ => {}
        }
    }
}

/// The DSR cursor-position round-trip: the child emits `CSI 6 n`, reads the
/// 6-byte reply the daemon writes back, and echoes it via `cat -v` as `^[[1;1R`.
/// A missing reply blocks the child forever and this times out.
async fn assert_dsr_roundtrip(isolated: bool) {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();

    let daemon = Daemon::new(&sandbox);
    let daemon = if isolated { daemon.isolated() } else { daemon };
    cleanup.track(daemon.spawn(None).await as i32);

    let token = daemon_token(&sandbox).await;
    let mut client = connect_client(&sandbox, &token).await;
    // Emit DSR, read back exactly the 6-byte `\x1b[1;1R` reply, echo it visibly.
    let pane = create_and_attach(
        &mut client,
        1,
        Some(&["/bin/sh", "-c", "printf '\\033[6n'; head -c 6 | cat -v"]),
    )
    .await;

    // The round-trip completes in tens of milliseconds when it works; this
    // deadline only bounds how long a *broken* one takes to report. Ten seconds
    // was tight enough to fail on a fully loaded machine while the child was
    // still starting, which is a false failure, not a slow one.
    let text = grid_text_until(&mut client, &pane, Duration::from_secs(30), |t| {
        t.contains("[1;1R")
    })
    .await;

    assert!(
        text.contains("[1;1R"),
        "the DSR cursor-position reply must round-trip back to the child and \
         render (looked for `^[[1;1R` via `cat -v`); grid was: {:?}",
        text.trim_end()
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
