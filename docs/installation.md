# Installation

The authoritative install reference. For the common commands, see the
[README Install section](../README.md#install); this doc is the full matrix plus
the installer's flags, the on-disk layout, checksum verification, and offline
installs.

kmux ships two flavors:

- **Full** — desktop GUI + daemon. For a workstation.
- **Headless** — `kmuxd` + the `kmux` CLI, no GUI, no GTK dependency. For a
  server. Run the daemon here and connect from a full GUI client over QUIC.

All channels are built from the same per-target release tarballs and are
checksum-verified.

## macOS

| Want | How |
| --- | --- |
| GUI app (full) | `brew install --cask getkono/tap/kmux`, or the `.dmg` from [releases](https://github.com/getkono/kmux/releases/latest) |
| CLI + daemon (headless) | `brew install getkono/tap/kmux`, or `install.sh --headless` |

The app is signed with a Developer ID and notarized, so Gatekeeper accepts it with
no `xattr` workaround. The cask installs `kmux.app` to `/Applications` and puts the
`kmux` CLI on your `PATH`; the `kmux` command finds the app in either
`/Applications` or `~/Applications`.

The `install.sh` tarball on macOS contains the CLI + daemon only — the GUI is the
signed app (cask / `.dmg`), so the installer points you there.

## Linux

The GUI links the system **GTK4 + libadwaita** at runtime. The native packages
declare those as dependencies; the `install.sh` full install expects them already
present (`apt install libgtk-4-1 libadwaita-1-0` / `dnf install gtk4 libadwaita` /
`pacman -S gtk4 libadwaita`). `kmuxd`, the `kmux` CLI, and the bundled
`libkmux_ghostty` need nothing extra.

### Debian / Ubuntu (`.deb`)

```bash
# full (GUI + daemon)
sudo apt install ./kmux_<ver>_<arch>.deb
# headless (server)
sudo apt install ./kmux-headless_<ver>_<arch>.deb
```

`<arch>` is `amd64` or `arm64`. `kmux` and `kmux-headless` conflict with each
other — installing one replaces the other.

### Fedora / RHEL / openSUSE (`.rpm`)

```bash
sudo dnf install ./kmux-<ver>.<arch>.rpm           # full
sudo dnf install ./kmux-headless-<ver>.<arch>.rpm  # headless
```

`<arch>` is `x86_64` or `aarch64`. The same RPM works on image-based systems via
`rpm-ostree install kmux-<ver>.<arch>.rpm` (the package is FHS-clean with no
stateful scriptlets).

### Arch (AUR)

```bash
paru -S kmux-bin      # or: yay -S kmux-bin
```

`kmux-bin` installs the prebuilt release binaries.

### Flatpak

```bash
flatpak install ./kmux-<ver>-x86_64.flatpak
flatpak run dev.getkono.kmux
```

The flatpak bundles its own GTK4 + libadwaita from the GNOME runtime, so no system
GTK is needed. (A Flathub listing is planned; for now use the `.flatpak` bundle
from the release.)

## Universal installer (`install.sh`)

Works on Linux and macOS, no package manager required:

```bash
curl -fsSL https://raw.githubusercontent.com/getkono/kmux/master/install.sh | sh
```

It detects your OS/arch, downloads the matching release tarball **and its
`.sha256`, verifies the checksum**, and installs. Flags (pass after `-s --` when
piping, e.g. `… | sh -s -- --headless`):

| Flag | Meaning |
| --- | --- |
| `--headless` | Daemon + CLI only; skip the GTK GUI and desktop entry. Default is full. |
| `--prefix <dir>` | Install prefix. Default `~/.local` (no sudo). Use `/usr/local` for system-wide (needs sudo). |
| `--version <ver>` | Install a specific version (`0.2.0` or `v0.2.0`). Default: latest. Also `KMUX_VERSION`. |
| `--uninstall` | Remove a previous install from `<prefix>` (leaves config + session state). |
| `--help` | Show usage. |

### Install layout

The installer preserves the relocatable tarball layout: the rpath-coupled set
lives together in `<prefix>/lib/kmux`, with the user-facing commands symlinked
onto `PATH`:

```
<prefix>/bin/kmux        -> ../lib/kmux/kmux        (symlink)
<prefix>/bin/kmuxd       -> ../lib/kmux/kmuxd       (symlink)
<prefix>/bin/kmux-gtk    -> ../lib/kmux/kmux-gtk    (symlink; Linux full only)
<prefix>/lib/kmux/{kmux,kmuxd,kmux-vt-worker,libkmux_ghostty.so[,kmux-gtk]}
<prefix>/share/applications/dev.getkono.kmux.desktop   (Linux full only)
<prefix>/share/icons/hicolor/scalable/apps/dev.getkono.kmux.svg
```

`kmuxd` and `kmux-vt-worker` carry an `$ORIGIN` (`@loader_path` on macOS) runpath,
so they load the bundled `libkmux_ghostty` from beside the real binary in
`lib/kmux` — which is why the binaries stay together there and only symlinks go in
`bin`. Re-running the installer upgrades in place. If `<prefix>/bin` isn't on your
`PATH`, the installer prints the line to add.

## Manual download + checksum verification

Every release asset has a `.sha256` sidecar. To install without the script:

```bash
ver=0.2.0; target=x86_64-unknown-linux-gnu
base="https://github.com/getkono/kmux/releases/download/v${ver}"
curl -fLO "${base}/kmux-${ver}-${target}.tar.gz"
curl -fLO "${base}/kmux-${ver}-${target}.tar.gz.sha256"
shasum -a 256 -c "kmux-${ver}-${target}.tar.gz.sha256"   # or: sha256sum -c
tar -xzf "kmux-${ver}-${target}.tar.gz"
```

Run the binaries from the extracted directory (they find `libkmux_ghostty` next to
themselves), or move the whole directory somewhere stable and symlink `kmux` /
`kmuxd` onto your `PATH`. This is also the offline/air-gapped path: download the
tarball + `.sha256` on a connected machine, copy both across, verify, and extract.

The `.deb` / `.rpm` / `.dmg` / `.flatpak` assets each have a `.sha256` too.

## Build from source

See [Building from source](../README.md#building-from-source) in the README and
[docs/building-macos.md](building-macos.md) for the macOS app. `mise run install`
performs a from-source install equivalent to the packaged layout.
