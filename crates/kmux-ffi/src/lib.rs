//! uniffi bindings exposing the kmux client to non-Rust frontends.
//!
//! This crate is the language boundary for a native GUI written in another
//! language — concretely, the SwiftUI macOS app. It wraps the toolkit-agnostic
//! [`kmux_app::driver::FrontendDriver`] in an opaque, thread-confined
//! [`KmuxDriver`] handle and re-exports a small, flat, uniffi-friendly surface:
//!
//! - **Lifecycle**: [`KmuxDriver::new`] builds an `AppCore` (resolving the
//!   server target / theme exactly as the CLI front door does), wraps it in a
//!   `FrontendDriver`, and owns the tokio runtime the driver's background tasks
//!   run on.
//! - **Pump**: [`KmuxDriver::tick`] runs one driver iteration and returns the
//!   [`FfiEffect`]s the frontend must act on. The Swift app calls this each frame
//!   (a main-thread timer, the analog of the GTK `glib` timeout).
//! - **Render (hot path)**: [`KmuxDriver::grid_info`] /
//!   [`KmuxDriver::grid_snapshot`] expose the active grid as a generation-gated,
//!   packed byte buffer (see [`kmux_render::packed`]) so the renderer copies only changed
//!   frames; plus [`KmuxDriver::theme`], [`KmuxDriver::blink_on`],
//!   [`KmuxDriver::selection`], and [`KmuxDriver::scroll_info`].
//! - **Input**: structured **mode-aware** keys ([`KmuxDriver::send_char`] /
//!   [`KmuxDriver::send_named_key`], encoded by the daemon — not hand-rolled
//!   here), [`KmuxDriver::dispatch`] (a curated [`FfiAction`] set),
//!   [`KmuxDriver::send_input`] (raw PTY bytes), [`KmuxDriver::feed_paste`], the
//!   mouse helpers ([`KmuxDriver::scroll_at`] + the selection setters), and
//!   [`KmuxDriver::set_term_size`] / [`KmuxDriver::request_resize`].
//! - **Chrome / overlays**: [`KmuxDriver::connection`], [`KmuxDriver::sessions`],
//!   [`KmuxDriver::panes`] / [`KmuxDriver::select_pane`], [`KmuxDriver::mode`],
//!   the command palette ([`KmuxDriver::command_hints`] /
//!   [`KmuxDriver::run_command`]), the [`KmuxDriver::picker`] getter + drivers,
//!   session [`KmuxDriver::rename_session`] / [`KmuxDriver::close_session`],
//!   [`KmuxDriver::metrics`], and [`KmuxDriver::available_themes`] /
//!   [`KmuxDriver::set_theme`].
//!
//! ## Threading
//!
//! All `KmuxDriver` methods are expected to be called from a single thread (the
//! Swift main thread). Background tokio tasks communicate with the driver only
//! through the four mpsc channels it owns; the `Mutex<FrontendDriver>` makes the
//! handle `Send + Sync` for uniffi, and `tick` (under the lock) is the sole
//! place `AppCore` is mutated, so there is no shared mutable cross-thread
//! access. Off-main-thread calls are not part of the contract yet.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use tokio::runtime::Runtime;

use kmux_app::appearance::{Appearance, CellAdjust};
use kmux_app::cmd;
use kmux_app::config;
use kmux_app::core::{
    AddRemoteForm, AppCore, DirBrowserRow, LaunchRow, OverviewRowKind, PauseReason, RemoteStatus,
    TopBarAction,
};
use kmux_app::diagnostic::{self, DiagnosticTest};
use kmux_app::driver::{FrontendDriver, FrontendEffect};
use kmux_app::mode::{Action, CommandState, Mode};
use kmux_app::subcommands::parse_target;
use kmux_app::theme::{self, Rgb, Theme};
use kmux_client::connection_state::ConnectionState;
use kmux_client::generate_instance_id;
use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::{
    MouseButton, MouseEvent, MouseEventKind, MouseMods, char_to_proto_key, encode_mouse_scroll,
};
use kmux_protocol::messages::{
    AttentionKind, ClientCapabilities, KeyAction, KeyCode, KeyEvent, KeyMods, PaneProgressState,
    SplitDir, TermSize,
};
use kmux_protocol::{format_pane_id, pane_index};
// The packed-cell format is owned by `kmux-render` (the single encoder shared
// with the GPU renderer; see docs/architecture-render.md). The non-GPU Swift
// path encodes through it here, so the bytes are identical to the renderer's.
use kmux_render::packed;
// `CursorView` is part of kmux-render's wgpu-free core and the render-debug
// overlay reads it on the Cairo path too, so it must not sit behind `gpu` — the
// lean build (`mise run build-no-gpu`) needs it.
use kmux_render::CursorView;
#[cfg(feature = "gpu")]
use kmux_render::{CellSource, Frame, PaneView, ScrollIndicator, TerminalRenderer};

uniffi::setup_scaffolding!();

mod action;
mod bootstrap;
mod driver;
mod grid;
mod input;
mod layout;
mod palette;
mod picker;
mod renderer;
mod session;
mod status;

pub(crate) use bootstrap::{build_core, init_ffi_logging};

pub use action::*;
pub use grid::*;
pub use input::*;
pub use layout::*;
pub use palette::*;
pub use picker::*;
pub use session::*;
pub use status::*;

pub use driver::KmuxDriver;
#[cfg(feature = "gpu")]
pub use renderer::KmuxRenderer;

/// ABI version of this FFI surface. Bumped on any breaking change to the
/// exported types/functions, mirroring the repo's other versioned boundaries
/// (`kmux-ghostty-sys`'s `EXPECTED_ABI_VERSION`, the wire protocol range).
/// The Swift wrapper asserts this on startup, on top of uniffi's built-in
/// binding-checksum check.
pub const KMUX_FFI_ABI_VERSION: u32 = 26;

/// Returns [`KMUX_FFI_ABI_VERSION`]. A free function so the Swift wrapper can
/// check it before constructing a driver.
#[uniffi::export]
pub fn kmux_ffi_abi_version() -> u32 {
    KMUX_FFI_ABI_VERSION
}

/// Build + version metadata for the Swift app's "About" panel — the same matrix
/// `kmux -V` prints, plus this binary's renderer API and FFI ABI versions. Mirror
/// of [`kmux_app::version::VersionInfo`] flattened for uniffi.
#[derive(uniffi::Record)]
pub struct FfiVersionInfo {
    /// Crate semver, e.g. `"0.2.0"`.
    pub semver: String,
    /// Short git commit (or `"unknown"`).
    pub git_sha: String,
    /// Working tree had uncommitted changes at build time.
    pub git_dirty: bool,
    /// Build date, `YYYY-MM-DD`.
    pub build_date: String,
    /// Cargo profile (`"debug"` / `"release"`).
    pub build_profile: String,
    /// Client↔daemon supported wire protocol range.
    pub protocol: String,
    /// Renderer API version (this binary links `kmux-render`).
    pub render_api: u32,
    /// FFI C-ABI version ([`KMUX_FFI_ABI_VERSION`]).
    pub ffi_abi: u32,
}

/// Version + build metadata for the Swift "About" panel. The build identity
/// (semver, git SHA + dirty, date, profile) is what makes a running build
/// verifiable; the protocol/render/FFI numbers pin the linked boundaries.
#[uniffi::export]
pub fn kmux_ffi_version_info() -> FfiVersionInfo {
    let v = kmux_app::version::VersionInfo::current();
    FfiVersionInfo {
        semver: v.semver.to_string(),
        git_sha: v.git_sha.to_string(),
        git_dirty: v.git_dirty,
        build_date: v.build_date.to_string(),
        build_profile: v.build_profile.to_string(),
        protocol: v.protocol,
        render_api: kmux_render::KMUX_RENDER_API_VERSION,
        ffi_abi: KMUX_FFI_ABI_VERSION,
    }
}

/// The terminal renderer backend resolved from `~/.config/kmux/config.toml`
/// (`"cairo"` or `"gpu"`), as a stable lowercase token. The Swift frontend calls
/// this once at startup to decide whether to build the Metal path — the renderer
/// is config-only (not a CLI flag) because a singleton GUI client cannot honor a
/// per-launch flag. The render-debug overlay still reports the *effective*
/// renderer from live state (a GPU-init failure falls back to CoreText).
#[uniffi::export]
pub fn resolve_renderer() -> String {
    config::resolve_renderer().as_str().to_string()
}

/// The bytes of the bundled symbol fallback font (Symbols Nerd Font Mono). The
/// Swift app registers these with CoreText at startup
/// (`CTFontManagerRegisterGraphicsFont`) so its CoreText render path — and any
/// Nerd-glyph icons in the app chrome — resolve Powerline/PUA glyphs the
/// configured font lacks, matching the GPU atlas's fallback (issue #145).
#[uniffi::export]
pub fn symbol_fallback_font_bytes() -> Vec<u8> {
    kmux_render::symbol_fallback_bytes().to_vec()
}

/// The kmux-render API version this crate was written against. A compile-time
/// guard (below) asserts the linked `kmux-render` matches, so the renderer
/// boundary is versioned like the others.
#[cfg(feature = "gpu")]
pub const EXPECTED_RENDER_API: u32 = 1;

#[cfg(feature = "gpu")]
const _: () = assert!(
    kmux_render::KMUX_RENDER_API_VERSION == EXPECTED_RENDER_API,
    "linked kmux-render API version does not match EXPECTED_RENDER_API",
);

/// The linked kmux-render API version, asserted on the Swift side alongside
/// [`KMUX_FFI_ABI_VERSION`].
#[cfg(feature = "gpu")]
#[uniffi::export]
pub fn kmux_ffi_render_api_version() -> u32 {
    kmux_render::KMUX_RENDER_API_VERSION
}

/// Failure constructing a [`KmuxDriver`].
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("failed to initialize kmux: {message}")]
    Init { message: String },
    /// GPU renderer construction or operation failed (the `gpu` feature).
    #[error("renderer error: {message}")]
    Render { message: String },
}

/// Parameters for [`KmuxDriver::new`]. Mirrors the CLI front door's launch plan:
/// `server` is `None` for the local daemon or `"[user@]host"` for SSH; `theme`
/// is a built-in theme name (defaulting when `None`); `rows`/`cols` and the
/// pixel size are the initial content geometry.
#[derive(uniffi::Record)]
pub struct DriverConfig {
    pub server: Option<String>,
    pub ssh_port: Option<u16>,
    pub cwd: Option<String>,
    pub session: Option<String>,
    pub theme: Option<String>,
    /// Whether the inner-pane cursor blinks. `None` defaults to `true` (and
    /// falls back to the `cursor_blink` key in `config.toml`).
    pub cursor_blink: Option<bool>,
    /// Render diagnostic to launch instead of a shell session: a
    /// [`DiagnosticTest`] name
    /// (`glyphs`/`attrs`/…). `None` for an ordinary launch (issue #145).
    pub diagnostic: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_mods_map_to_proto_bits() {
        let none = FfiKeyMods {
            shift: false,
            ctrl: false,
            alt: false,
            command: false,
        };
        assert_eq!(none.to_proto(), KeyMods::empty());

        let all = FfiKeyMods {
            shift: true,
            ctrl: true,
            alt: true,
            command: true,
        };
        assert_eq!(
            all.to_proto(),
            KeyMods::SHIFT | KeyMods::CTRL | KeyMods::ALT | KeyMods::SUPER
        );

        let ctrl = FfiKeyMods {
            shift: false,
            ctrl: true,
            alt: false,
            command: false,
        };
        assert_eq!(ctrl.to_proto(), KeyMods::CTRL);
    }

    #[test]
    fn pane_label_falls_back_to_index() {
        assert_eq!(pane_label(0, ""), "pane 1");
        assert_eq!(pane_label(3, "   "), "pane 4");
        assert_eq!(pane_label(0, "vim"), "vim");
    }

    #[test]
    fn tab_label_falls_back_to_one_based_index() {
        assert_eq!(tab_label(0, "", ""), "1");
        assert_eq!(tab_label(2, "   ", "   "), "3");
        assert_eq!(tab_label(0, "build", "vim"), "build");
        assert_eq!(tab_label(0, "", "vim"), "vim");
    }

    #[test]
    fn pause_state_maps_from_pause_reason() {
        assert_eq!(
            FfiPauseState::from(PauseReason::None),
            FfiPauseState::Active
        );
        assert_eq!(
            FfiPauseState::from(PauseReason::Manual),
            FfiPauseState::PausedManual
        );
        assert_eq!(
            FfiPauseState::from(PauseReason::Auto),
            FfiPauseState::PausedBackground
        );
    }

    #[test]
    fn abi_version_export_matches_constant() {
        // The exported free fn must return the constant verbatim. Asserting the
        // invariant (not a hardcoded number) keeps `KMUX_FFI_ABI_VERSION` the
        // single source of truth: bumping it needs no edit here, in the Swift
        // app, or in CI. uniffi's regenerated binding-checksum check is what
        // actually guards against stale bindings/dylib drift.
        assert_eq!(kmux_ffi_abi_version(), KMUX_FFI_ABI_VERSION);
    }

    /// The whole point of exporting this is that Swift draws what the other two
    /// renderers draw. A shape whose rects diverge here is a cursor that looks
    /// different on macOS, which is the bug this replaced.
    #[test]
    fn exported_cursor_rects_are_the_renderer_s_own() {
        for code in 0_u8..=4 {
            let exported = kmux_cursor_rects(2, 1, code, 10, 5, 8.0, 16.0);

            let view = CursorView {
                col: 2,
                row: 1,
                shape: packed::cursor_shape_from_code(code),
                blink: false,
                visible: true,
            };
            let expected = kmux_render::cursor_geometry(
                &view,
                (0.0, 0.0),
                10,
                5,
                &kmux_render::CellMetrics::new(8.0, 16.0),
            );
            let got: Vec<_> = exported.iter().map(|r| (r.x, r.y, r.w, r.h)).collect();
            let want: Vec<_> = expected
                .rects
                .iter()
                .map(|r| (r.x, r.y, r.w, r.h))
                .collect();
            assert_eq!(got, want, "shape code {code}");
        }
    }

    /// Out of range and hidden both draw nothing, so a frontend can fill
    /// whatever it is handed without a range check of its own.
    #[test]
    fn a_cursor_with_nothing_to_draw_yields_no_rects() {
        // Out of range in either axis, including a value too large for the
        // grid's own u16.
        assert!(kmux_cursor_rects(0, 99, 0, 10, 5, 8.0, 16.0).is_empty());
        assert!(kmux_cursor_rects(u32::MAX, 0, 0, 10, 5, 8.0, 16.0).is_empty());
        // Hidden.
        assert!(kmux_cursor_rects(0, 0, 4, 10, 5, 8.0, 16.0).is_empty());
        // And a code from a build that knows more shapes than this one.
        assert!(kmux_cursor_rects(0, 0, 200, 10, 5, 8.0, 16.0).is_empty());
    }

    // ─── Boundary parity ─────────────────────────────────────────────────────
    //
    // Both conversions below are one `match` of dozens of same-named arms, which
    // is the shape a copy-paste transposes: `ArrowUp => ArrowDown` compiles, type
    // checks, and silently breaks a key on macOS only. Rather than restate the
    // mapping by hand — a second copy to transpose — these assert two properties
    // the correct mapping has and a transposed one does not:
    //
    //   * **injectivity** — no two inputs land on the same output, so a
    //     duplicated right-hand side is caught wherever it is;
    //   * **name agreement** — each variant maps to the identically-named one,
    //     which is the mapping's entire rule.
    //
    // A new variant added to either enum has to be added here too; the lists are
    // the one place the test is not self-maintaining, so they are kept in source
    // order to make a diff against the enum obvious.

    /// Every [`FfiNamedKey`], in the order the enum declares them.
    fn all_named_keys() -> Vec<FfiNamedKey> {
        vec![
            FfiNamedKey::Enter,
            FfiNamedKey::Tab,
            FfiNamedKey::Backspace,
            FfiNamedKey::Escape,
            FfiNamedKey::ArrowUp,
            FfiNamedKey::ArrowDown,
            FfiNamedKey::ArrowLeft,
            FfiNamedKey::ArrowRight,
            FfiNamedKey::PageUp,
            FfiNamedKey::PageDown,
            FfiNamedKey::Home,
            FfiNamedKey::End,
            FfiNamedKey::Delete,
            FfiNamedKey::Insert,
            FfiNamedKey::F1,
            FfiNamedKey::F2,
            FfiNamedKey::F3,
            FfiNamedKey::F4,
            FfiNamedKey::F5,
            FfiNamedKey::F6,
            FfiNamedKey::F7,
            FfiNamedKey::F8,
            FfiNamedKey::F9,
            FfiNamedKey::F10,
            FfiNamedKey::F11,
            FfiNamedKey::F12,
        ]
    }

    #[test]
    fn every_named_key_maps_to_the_key_code_of_the_same_name() {
        let keys = all_named_keys();
        assert_eq!(keys.len(), 26, "a variant was added to FfiNamedKey");

        for key in &keys {
            assert_eq!(
                format!("{key:?}"),
                format!("{:?}", key.to_code()),
                "{key:?} maps to a differently-named key code"
            );
        }
    }

    /// The other half. Name agreement catches a *swap* (`ArrowUp => ArrowDown`
    /// and back); injectivity catches a *duplicate* (both arms landing on
    /// `ArrowDown`), which a swap-only check would pass. Neither subsumes the
    /// other, so both are here.
    #[test]
    fn no_two_named_keys_share_a_key_code() {
        let mut seen: Vec<String> = all_named_keys()
            .iter()
            .map(|k| format!("{:?}", k.to_code()))
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "two named keys map to the same key code");
    }

    /// Every [`FfiAction`] that carries no payload, in declaration order. The
    /// four that do are asserted separately, since their names cannot match.
    fn all_unit_actions() -> Vec<FfiAction> {
        vec![
            FfiAction::CreateSession,
            FfiAction::CloseSession,
            FfiAction::NextSession,
            FfiAction::PrevSession,
            FfiAction::CreatePane,
            FfiAction::ClosePane,
            FfiAction::UndoClose,
            FfiAction::NextTab,
            FfiAction::PrevTab,
            FfiAction::NextPaneInTab,
            FfiAction::PrevPaneInTab,
            FfiAction::CloseTab,
            FfiAction::RenameTab,
            FfiAction::SplitRight,
            FfiAction::SplitDown,
            FfiAction::FocusLeft,
            FfiAction::FocusRight,
            FfiAction::FocusUp,
            FfiAction::FocusDown,
            FfiAction::ResizeLeft,
            FfiAction::ResizeRight,
            FfiAction::ResizeUp,
            FfiAction::ResizeDown,
            FfiAction::SwapNext,
            FfiAction::SwapPrev,
            FfiAction::CycleLayout,
            FfiAction::ToggleZoom,
            FfiAction::ScrollPageUp,
            FfiAction::ScrollPageDown,
            FfiAction::ToggleHud,
            FfiAction::ToggleMetrics,
            FfiAction::ToggleProcessOverview,
            FfiAction::ToggleConnectedClients,
            FfiAction::ToggleConnection,
            FfiAction::ToggleRenderDebug,
            FfiAction::ResetRenderer,
            FfiAction::ToggleInputLock,
            FfiAction::TogglePause,
            FfiAction::ToggleFocusedPaneNoAutoPause,
            FfiAction::ToggleActiveSessionNoAutoPause,
            FfiAction::CopySelection,
            FfiAction::Paste,
            FfiAction::Quit,
            FfiAction::Reconnect,
        ]
    }

    #[test]
    fn every_payload_free_action_maps_to_the_core_action_of_the_same_name() {
        let actions = all_unit_actions();
        assert_eq!(
            actions.len(),
            44,
            "a payload-free variant was added to FfiAction"
        );

        for action in actions {
            let name = format!("{action:?}");
            let core: Action = action.into();
            assert_eq!(
                name,
                format!("{core:?}"),
                "{name} maps to a differently-named action"
            );
        }
    }

    #[test]
    fn no_two_actions_share_a_core_action() {
        let mut seen: Vec<String> = all_unit_actions()
            .into_iter()
            .map(|a| format!("{:?}", Action::from(a)))
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            total,
            "two FFI actions map to the same core action"
        );
    }

    /// The four that carry a payload: the value has to survive, and the widening
    /// or narrowing has to be the right one.
    #[test]
    fn actions_carrying_a_payload_pass_it_through() {
        assert_eq!(
            Action::from(FfiAction::JumpToSession { index: 7 }),
            Action::JumpToSession(7)
        );
        assert_eq!(
            Action::from(FfiAction::FocusPaneAt { index: 3 }),
            Action::FocusPaneAt(3)
        );
        assert_eq!(
            Action::from(FfiAction::ScrollUp { lines: 12 }),
            Action::ScrollUp(12)
        );
        assert_eq!(
            Action::from(FfiAction::ScrollDown { lines: 12 }),
            Action::ScrollDown(12)
        );
    }

    #[test]
    fn launch_row_flattens_each_variant() {
        // Local new + add-remote bookends.
        let r = launch_row_to_ffi(LaunchRow::LocalNewSession {
            default_cwd: "/home/u".into(),
        });
        assert_eq!(r.kind, FfiLaunchRowKind::LocalNewSession);
        assert_eq!(r.detail, "/home/u");

        // A remote header carries its peer, status, and expansion.
        let r = launch_row_to_ffi(LaunchRow::Remote {
            peer: "alice@box".into(),
            label: "alice@box".into(),
            status: RemoteStatus::Error("nope".into()),
            expanded: true,
        });
        assert_eq!(r.kind, FfiLaunchRowKind::Remote);
        assert_eq!(r.peer.as_deref(), Some("alice@box"));
        assert_eq!(r.status, FfiRemoteStatus::Error);
        assert_eq!(r.detail, "nope"); // the error reason surfaces as detail
        assert!(r.expanded);

        // An existing remote session carries both routing keys.
        let r = launch_row_to_ffi(LaunchRow::RemoteExisting {
            peer: "alice@box".into(),
            word_id: "eagle".into(),
            name: "proj".into(),
            cwd: "/srv".into(),
            active: true,
        });
        assert_eq!(r.kind, FfiLaunchRowKind::RemoteExisting);
        assert_eq!(r.peer.as_deref(), Some("alice@box"));
        assert_eq!(r.word_id.as_deref(), Some("eagle"));
        assert!(r.active);
    }

    #[test]
    fn mode_to_ffi_maps_launcher_modes() {
        assert!(matches!(
            mode_to_ffi(&Mode::LaunchPicker),
            FfiMode::LaunchPicker
        ));
        assert!(matches!(mode_to_ffi(&Mode::AddRemote), FfiMode::AddRemote));
        assert!(matches!(
            mode_to_ffi(&Mode::RemoteNewSession { peer: "alice@box".into() }),
            FfiMode::RemoteNewSession { peer } if peer == "alice@box"
        ));
    }

    #[test]
    fn mode_to_ffi_maps_session_close_confirmation() {
        assert!(matches!(
            mode_to_ffi(&Mode::ConfirmCloseSession {
                word_id: "eagle".into(),
                name: "build".into(),
            }),
            FfiMode::ConfirmCloseSession { word_id, name }
                if word_id == "eagle" && name == "build"
        ));
    }

    #[test]
    fn ffi_appearance_maps_fields() {
        use kmux_app::appearance::FontFeature;
        let appearance = Appearance {
            family: "JetBrains Mono".into(),
            family_bold: Some("JetBrains Mono Bold".into()),
            family_italic: None,
            family_bold_italic: None,
            size_pt: 14.0,
            style: Some("Medium".into()),
            features: vec![
                FontFeature {
                    tag: "ss01".into(),
                    value: 1,
                },
                FontFeature {
                    tag: "liga".into(),
                    value: 0,
                },
            ],
            cell_width_adjust: CellAdjust::Pixels(2.0),
            cell_height_adjust: CellAdjust::Percent(10.0),
        };
        let ffi = FfiAppearance::from(&appearance);
        assert_eq!(ffi.family, "JetBrains Mono");
        assert_eq!(ffi.family_bold.as_deref(), Some("JetBrains Mono Bold"));
        assert_eq!(ffi.size_pt, 14.0);
        assert_eq!(ffi.style.as_deref(), Some("Medium"));
        assert_eq!(
            ffi.features,
            vec!["ss01=1".to_string(), "liga=0".to_string()]
        );
        assert!(matches!(
            ffi.cell_width_adjust,
            FfiCellAdjust {
                kind: FfiCellAdjustKind::Pixels,
                value
            } if value == 2.0
        ));
        assert!(matches!(
            ffi.cell_height_adjust,
            FfiCellAdjust {
                kind: FfiCellAdjustKind::Percent,
                value
            } if value == 10.0
        ));
    }

    #[test]
    fn divider_round_trips_through_ffi() {
        let d = kmux_app::layout::Divider {
            path: vec![1, 0],
            dir: SplitDir::Vertical,
            before: 1,
            hit_col: 3,
            hit_row: 12,
            hit_cols: 40,
            hit_rows: 1,
            pair_start: 6,
            pair_len: 24,
        };
        let back = FfiDivider::from_layout(d.clone()).into_layout();
        assert_eq!(back, d);
        // The Horizontal/Vertical ⇄ vertical_bar mapping is faithful.
        assert!(!FfiDivider::from_layout(d.clone()).vertical_bar);
        let h = kmux_app::layout::Divider {
            dir: SplitDir::Horizontal,
            ..d
        };
        assert!(FfiDivider::from_layout(h).vertical_bar);
    }
}
