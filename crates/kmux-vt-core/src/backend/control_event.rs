//! The catalog of terminal control sequences kmux gives special treatment
//! (issue #187).
//!
//! kmux does **not** parse VT itself — libghostty-vt does. Almost every sequence
//! is handled inside libghostty's terminal model (cursor moves, SGR, modes, …)
//! and never surfaces here. A small, deliberate set is *intercepted* because
//! kmux has to do something with it beyond updating the grid: forward it to
//! clients, mirror it into per-pane state, or both. Those interceptions happen
//! in one Zig switch (`Handler.vt` in
//! `crates/kmux-ghostty-sys/zig/src/wrapper.zig`) and arrive on the Rust side as
//! the variants below.
//!
//! [`ControlEvent`] is the single value every interception is funneled through,
//! and [`super::BackendEventSink::on_control_event`] is the single method each
//! consumer (the daemon relay, the isolated VT worker) implements. To audit
//! "what does kmux do specially with VT sequences?", read this enum and the two
//! `on_control_event` `match`es — there is nowhere else to look.
//!
//! Sequences libghostty-vt does not implement never reach this enum: the parser
//! drops them (and, as of issue #187, logs them via the `kmux::vt` target so
//! they surface in `kmux daemon logs`). Adding a new interception means adding a
//! variant here, mapping it in the Zig handler + `EventSinkAdapter`, and
//! handling it in the `on_control_event` `match`es — the compiler points at each
//! site.

use kmux_protocol::messages::PaneProgressState;

/// One terminal control sequence kmux intercepts for special handling.
///
/// Borrowed (`&str`) rather than owned: the parser hands these out synchronously
/// inside `feed()`, and consumers copy only what they keep.
#[derive(Debug, Clone, Copy)]
pub enum ControlEvent<'a> {
    /// **OSC 0 / OSC 2** — window/icon title. Intercepted (`.window_title`)
    /// because libghostty's read-only handler drops the title; kmux stores it
    /// per pane and broadcasts `PaneTitleChanged` to clients.
    Title(&'a str),

    /// **BEL** (`0x07`) — terminal bell. Surfaced so a frontend could flash or
    /// ring; no client-facing wire event consumes it yet, so the daemon drops it.
    Bell,

    /// **OSC 52** — clipboard write. `selection` is the normalized target
    /// (`"c"`/`"p"`/`"s"`/`"0"`..`"7"`); `base64_data` is the still-encoded
    /// payload (decoded client-side). The daemon broadcasts `PaneClipboardCopy`.
    Osc52Copy {
        selection: &'a str,
        base64_data: &'a str,
    },

    /// **OSC 9;4** — ConEmu/Windows-Terminal progress report. `progress` is
    /// `0..=100` or `None`. The daemon stores the latest value per pane and
    /// broadcasts `PaneProgressChanged`. Not forwarded over the worker protocol,
    /// so the process-isolation path does not surface it (issue #126).
    Progress {
        state: PaneProgressState,
        progress: Option<u8>,
    },

    /// **OSC 8** — hyperlink. libghostty also tracks hyperlink cell state; kmux
    /// gets the id/uri here too, but no client-facing wire event consumes it yet,
    /// so the daemon drops it.
    Hyperlink { id: Option<&'a str>, uri: &'a str },
}
