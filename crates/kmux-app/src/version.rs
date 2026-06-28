//! Single source of build + version metadata, surfaced identically by `kmux -V`
//! and the GUI "About" panels (GTK `adw::AboutWindow`; the Swift app via the FFI
//! `kmux_ffi_version_info`).
//!
//! The build identity (crate version, git SHA + dirty flag, date, profile) is the
//! part that answers "is this the binary I just built?" — the question the dev
//! launcher's kill-and-replace flow and the debug window-title marker exist to
//! make verifiable. The protocol/ABI numbers pin the compatibility boundaries
//! this client links. Daemon-side boundaries (ghostty ABI, worker protocol) are
//! not linked by any client binary, so they are intentionally omitted here.

/// Version + build metadata for the running client binary. Build-identity fields
/// come from compile-time env vars emitted by this crate's `build.rs`.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Crate semver (workspace `version`), e.g. `"0.2.0"`.
    pub semver: &'static str,
    /// Short git commit the binary was built from (or `"unknown"`).
    pub git_sha: &'static str,
    /// Whether the working tree had uncommitted changes at build time.
    pub git_dirty: bool,
    /// Build date, `YYYY-MM-DD`.
    pub build_date: &'static str,
    /// Full UTC build timestamp, ISO-8601 (`YYYY-MM-DDThh:mm:ssZ`) — a superset
    /// of [`build_date`](Self::build_date), shown by the multi-line `-V`.
    pub build_timestamp: &'static str,
    /// Full `rustc --version` string the binary was compiled with.
    pub rustc: &'static str,
    /// Cargo profile the binary was built with (`"debug"` / `"release"`).
    pub build_profile: &'static str,
    /// Client↔daemon wire protocol ([`kmux_protocol::messages::PROTOCOL_VERSION`]).
    pub protocol: u32,
    /// Renderer API version (`kmux_render::KMUX_RENDER_API_VERSION`) — set only by
    /// the GUI frontends that link `kmux-render`; `None` for the toolkit-free CLI
    /// (which doesn't link the renderer, so `kmux -V` omits it).
    pub render_api: Option<u32>,
    /// FFI C-ABI version — set only for the uniffi/Swift binary (`kmux-ffi` fills
    /// it in); `None` for the CLI and the GTK frontend, which don't link `kmux-ffi`.
    pub ffi_abi: Option<u32>,
}

impl VersionInfo {
    /// Metadata for this binary. `ffi_abi` is `None`; `kmux-ffi` overrides it.
    pub fn current() -> Self {
        Self {
            semver: env!("CARGO_PKG_VERSION"),
            git_sha: env!("BUILD_GIT_SHA"),
            git_dirty: !env!("BUILD_GIT_DIRTY_SUFFIX").is_empty(),
            build_date: env!("BUILD_DATE"),
            build_timestamp: env!("BUILD_TIMESTAMP"),
            rustc: env!("BUILD_RUSTC_VERSION"),
            build_profile: env!("BUILD_PROFILE"),
            protocol: kmux_protocol::messages::PROTOCOL_VERSION,
            render_api: None,
            ffi_abi: None,
        }
    }

    /// Whether this is a debug build (the dev launcher's fresh-build path / the
    /// at-a-glance window-title marker key off this).
    pub fn is_debug(&self) -> bool {
        self.build_profile == "debug"
    }

    /// The build fingerprint: `<sha>` or `<sha>-dirty`. Used in the debug
    /// window-title marker and the one-line version.
    pub fn commit(&self) -> String {
        if self.git_dirty {
            format!("{}-dirty", self.git_sha)
        } else {
            self.git_sha.to_string()
        }
    }

    /// One-line build identity: `0.2.0 (a1b2c3d-dirty, 2026-06-24, debug)`. No
    /// program-name prefix — clap prepends `kmux ` for `-V`/`--version`.
    pub fn one_line(&self) -> String {
        format!(
            "{} ({}, {}, {})",
            self.semver,
            self.commit(),
            self.build_date,
            self.build_profile
        )
    }

    /// Multi-line block: the build identity plus the compatibility-boundary
    /// versions this binary links. Shared verbatim by `kmux -V` and both About
    /// panels.
    pub fn long_string(&self) -> String {
        let mut s = self.one_line();
        s.push_str(&format!("\n  protocol:   {}", self.protocol));
        if let Some(render_api) = self.render_api {
            s.push_str(&format!("\n  render API: {render_api}"));
        }
        if let Some(abi) = self.ffi_abi {
            s.push_str(&format!("\n  FFI ABI:    {abi}"));
        }
        s.push_str(&format!("\n  rustc:      {}", self.rustc));
        s.push_str(&format!("\n  built:      {}", self.build_timestamp));
        s
    }
}
