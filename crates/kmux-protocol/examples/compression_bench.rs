//! Offline compression-strategy bench for protocol compression (issue #59).
//!
//! Replays a frame capture produced by `kmuxd` with `KMUX_CAPTURE_FRAMES=<path>`
//! and reports, per zstd level, the resulting wire size, compression ratio, and
//! compression throughput — plus a per-category byte breakdown so it is obvious
//! where the bandwidth actually goes. This is the data that finalizes the v1
//! "stateless per-frame zstd" choice vs. the alternatives in
//! `docs/compression.md` (a static dictionary, a different level, lz4, …).
//!
//! Usage:
//!
//! ```sh
//! # 1. Capture a real session's server→client frames:
//! KMUX_CAPTURE_FRAMES=/tmp/frames.bin cargo run -p kmuxd
//! #    … attach a client, run `cat biglog`, `vim`, a build, etc., then quit.
//! # 2. Analyse it:
//! cargo run -p kmux-protocol --example compression_bench -- /tmp/frames.bin
//! ```
//!
//! Capture record format (see `kmuxd::capture`):
//! `[u8 category sort-key][u32 big-endian length][payload…]`, repeated.

use std::fs::File;
use std::io::{BufReader, Read};
use std::time::Instant;

/// Length prefix (4) + codec tag (1) — the per-frame wire overhead.
const FRAME_OVERHEAD: usize = 5;
/// Matches the default `[compression] min_size`: frames below this are never
/// compressed on the wire, so the bench mirrors that.
const MIN_SIZE: usize = 256;
/// zstd levels to sweep. 3 is the shipped default.
const LEVELS: &[i32] = &[1, 3, 9, 19];
/// Indexed by `MessageCategory::as_sort_key`.
const CATEGORIES: &[&str] = &[
    "Shell",
    "Scrollback",
    "Liveness",
    "Control",
    "Sync",
    "Bootstrap",
];

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: compression_bench <capture-file>");
    let mut reader = BufReader::new(File::open(&path).expect("open capture file"));

    let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
    loop {
        let mut header = [0u8; 5];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("read capture header: {e}"),
        }
        let category = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        reader
            .read_exact(&mut payload)
            .expect("read capture payload");
        frames.push((category, payload));
    }

    let raw_total: usize = frames.iter().map(|(_, p)| p.len() + FRAME_OVERHEAD).sum();
    println!(
        "capture: {path} — {} frames, {raw_total} raw wire bytes",
        frames.len()
    );

    println!("\nraw wire bytes by category:");
    for (i, name) in CATEGORIES.iter().enumerate() {
        let (count, bytes) = frames
            .iter()
            .filter(|(c, _)| *c as usize == i)
            .fold((0usize, 0usize), |(n, b), (_, p)| {
                (n + 1, b + p.len() + FRAME_OVERHEAD)
            });
        if count > 0 {
            let pct = 100.0 * bytes as f64 / raw_total as f64;
            println!("  {name:<11} {count:>8} frames  {bytes:>12} bytes  ({pct:>5.1}%)");
        }
    }

    println!("\nstateless per-frame zstd (compress-if-smaller, min_size={MIN_SIZE}):");
    println!(
        "  {:<6} {:>14} {:>8} {:>12}",
        "level", "wire bytes", "ratio", "MB/s"
    );
    for &level in LEVELS {
        let start = Instant::now();
        let mut wire_total = 0usize;
        let mut payload_total = 0usize;
        for (_, payload) in &frames {
            let wire = if payload.len() >= MIN_SIZE {
                match zstd::bulk::compress(payload, level) {
                    Ok(c) if c.len() < payload.len() => c.len() + FRAME_OVERHEAD,
                    _ => payload.len() + FRAME_OVERHEAD,
                }
            } else {
                payload.len() + FRAME_OVERHEAD
            };
            payload_total += payload.len();
            wire_total += wire;
        }
        let secs = start.elapsed().as_secs_f64();
        let mbps = payload_total as f64 / 1e6 / secs.max(f64::MIN_POSITIVE);
        let ratio = raw_total as f64 / wire_total as f64;
        println!("  {level:<6} {wire_total:>14} {ratio:>7.2}x {mbps:>12.0}");
    }

    println!(
        "\nNote: this measures strategy A (stateless per-frame). Strategies B \
         (static dictionary) and C (streaming) from docs/compression.md require \
         a trained dictionary / a stream harness and are tracked as follow-ups."
    );
}
