# Releasing

kmux ships prebuilt `kmux` + `kmuxd` binaries as GitHub Release assets. Releases
are cut locally with one command and published by a tag-triggered workflow:

```sh
mise run release 0.2.0
```

That bumps the version, regenerates the changelog, commits, tags `v0.2.0`, and
pushes. The tag push fires `.github/workflows/release.yml`, which builds native
binaries for every target and attaches them to the release.

The crates are `publish = false` — kmux is **not** published to crates.io. The
unit of distribution is the binary tarball, not a registry crate.

## Versioning

There is a single source of truth: `[workspace.package].version` in the root
`Cargo.toml`. All seven crates inherit it via `version.workspace = true`, so a
release bumps exactly one line (plus the matching `Cargo.lock` entries). Tags are
`v`-prefixed (`v0.2.0`); the version string inside is plain semver (`0.2.0`).

## `mise run release <ver>`

The recipe is idempotent and re-runnable. Run it from a clean `master` checkout.
It:

1. Validates `<ver>` is semver (rejects a leading `v`).
2. Requires branch `master` and a clean working tree; fetches tags.
3. Rewrites `[workspace.package].version` (anchored to that block so
   `[workspace.dependencies]` versions are untouched) and runs
   `cargo update --workspace --offline` to sync `Cargo.lock`.
4. Regenerates `CHANGELOG.md` with `git cliff --tag v<ver>`.
5. Commits `chore(release): v<ver>` — skipped if nothing changed (re-run case).
6. Force-creates the `v<ver>` tag at `HEAD`.
7. Pushes `master` (running the pre-push fmt/clippy/test gate once), then
   force-pushes the tag (`--no-verify`, since the same `HEAD` was just gated).

Re-running with the same version converges: the bump and commit become no-ops and
the tag is force-repushed, which re-triggers the workflow. This is how a failed or
yanked release is re-cut.

## CHANGELOG

`CHANGELOG.md` is generated from the conventional-commit history by
[git-cliff](https://git-cliff.org) (pinned in `mise.toml`), configured in
`cliff.toml`. Commits are grouped into Features / Bug Fixes / Performance /
Refactor / Documentation / Build System; merges, `chore(release)`, and
non-conventional commits are dropped.

The "since the last release" boundary is the **nearest ancestor tag in the commit
graph**, regardless of the tag's name — `topo_order = true` walks the graph rather
than trusting tag dates, and `tag_pattern = ".*"` considers every tag, not only
semver-looking ones. The committed `CHANGELOG.md` and the GitHub Release body are
produced from the same config (`git cliff --current` in the workflow), so they
always agree.

## Release workflow

`.github/workflows/release.yml` runs on `v*` tag pushes and on `workflow_dispatch`
(with a `tag` input, for re-running without moving the tag).

- **prepare** resolves the tag and fails fast if the tagged `Cargo.toml` version
  doesn't match it.
- **build** is a `fail-fast: false` matrix of native runners, mirroring `ci.yml`'s
  submodule checkout, mise toolchain, and cargo/zig caches:

  | Target | Runner |
  |---|---|
  | `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
  | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
  | `aarch64-apple-darwin` | `macos-14` |
  | `x86_64-apple-darwin` | `macos-15-intel` |

  Each leg runs `mise run package` and uploads the resulting tarball + `.sha256`.
- **release** generates notes with git-cliff and publishes every artifact via
  `softprops/action-gh-release` with `overwrite_files: true`, so re-runs replace
  assets in place rather than duplicating them.

## Packaging (`mise run package`) and the shared library

`kmuxd` **dynamically** links `libkmux_ghostty` — a Zig-built shared library that
wraps libghostty-vt (see `crates/kmux-ghostty-sys/zig/build.zig`, which builds it
with `.linkage = .dynamic`). `crates/kmux-ghostty-sys/build.rs` links it as a
`dylib` and bakes an **absolute** runpath pointing into the build tree
(`target/.../out/install/lib`), which is convenient for development but useless on
any other machine. (`kmux`, the client, does not link it.)

So `mise run package` does more than copy two binaries:

1. Builds `kmux` + `kmuxd` in release mode.
2. Copies both, the shared library, and `README.md` into
   `dist/kmux-<ver>-<target>/`. The library path is read back from the binary's own
   runpath, so it is always the exact artifact the binary was linked against.
3. Rewrites `kmuxd`'s runpath so it loads the sibling library:
   - Linux: `patchelf --set-rpath '$ORIGIN'`
   - macOS: `install_name_tool -change … @rpath/libkmux_ghostty.dylib` and
     `-add_rpath @loader_path`
4. Strips the binaries, produces `kmux-<ver>-<target>.tar.gz`, and writes a
   `.sha256` sidecar (verify with `shasum -c`).

The result runs from anywhere — `kmuxd` finds `libkmux_ghostty.{so,dylib}` next to
itself, with no `LD_LIBRARY_PATH` or build tree required.

## Prerequisites

`mise run release` and `mise run package` need the `mise`-pinned tools
(`mise install`): Zig, Rust, and `git-cliff`. On Linux, `mise run package`
additionally needs `patchelf` on `PATH` (e.g. `dnf install patchelf` /
`apt-get install patchelf`);
the release workflow installs it on its Linux legs. macOS uses `install_name_tool`
from the Xcode command line tools.

## Re-running a failed release

Two equivalent paths, both idempotent:

- `mise run release <ver>` again — converges local state and force-repushes the tag.
- Actions → **Release** → **Run workflow**, with the existing tag — rebuilds and
  re-publishes without touching the tag or commits.
