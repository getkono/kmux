#!/bin/sh
# kmux installer — downloads the prebuilt release tarball for your platform and
# installs the CLI + daemon (and, on Linux, the GUI). Inspired by the
# rustup/starship/ghostty installers.
#
#   curl -fsSL https://raw.githubusercontent.com/getkono/kmux/master/install.sh | sh
#
# Full install (default):  GUI client + daemon  (Linux; macOS GUI ships separately, see below)
# Headless install:        daemon + CLI only    (servers)
#       curl -fsSL .../install.sh | sh -s -- --headless
#
# Flags (pass after `-s --` when piping):
#   --headless          daemon + CLI only; skip the GTK GUI + desktop entry
#   --prefix <dir>      install prefix (default: $HOME/.local; use /usr/local for system-wide)
#   --version <ver>     install a specific version (e.g. 0.2.0 or v0.2.0); default: latest
#   --uninstall         remove a previous install from <prefix> (leaves config + state)
#   --help              show this help
#
# Environment: KMUX_VERSION overrides the version (same as --version).
#
# On macOS this installs the CLI + daemon (headless) only — the GUI is the signed
# native app, delivered via the Homebrew cask `getkono/tap/kmux` or the .dmg on
# the releases page. The script points you there after installing.
#
# POSIX sh only (runs under dash/ash/busybox). The whole body is wrapped in a
# main() called on the last line, so a truncated `curl | sh` never executes a
# partial script. Every download is checksum-verified before anything is unpacked.
set -eu

REPO="getkono/kmux"

# ----- output helpers -------------------------------------------------------
if [ -t 2 ]; then BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); YEL=$(printf '\033[33m'); RST=$(printf '\033[0m'); else BOLD=; DIM=; YEL=; RST=; fi
info() { printf '%s\n' "${DIM}==>${RST} $*" >&2; }
warn() { printf '%s\n' "${YEL}warning:${RST} $*" >&2; }
err()  { printf '%s\n' "${YEL}error:${RST} $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    sed -n '2,28p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || true
}

# ----- tool detection -------------------------------------------------------
detect_tools() {
    if have curl; then DL=curl
    elif have wget; then DL=wget
    else err "need curl or wget to download kmux"; fi
    if have sha256sum; then SHA=sha256sum
    elif have shasum; then SHA="shasum -a 256"
    else err "need sha256sum or shasum to verify downloads"; fi
    have tar || err "need tar to unpack kmux"
}

download() { # url dest
    if [ "$DL" = curl ]; then
        curl -fsSL "$1" -o "$2" || err "download failed: $1"
    else
        wget -qO "$2" "$1" || err "download failed: $1"
    fi
}

# Resolve the latest release tag by following the /releases/latest redirect —
# no API token, no jq, immune to the unauthenticated API rate limit.
latest_tag() {
    url="https://github.com/${REPO}/releases/latest"
    if [ "$DL" = curl ]; then
        eff=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url") || return 1
        printf '%s\n' "${eff##*/tag/}"
    else
        loc=$(wget -q -S -O /dev/null "$url" 2>&1 | sed -n 's/^[[:space:]]*Location:[[:space:]]*//p' | tail -1) || return 1
        [ -n "$loc" ] || return 1
        printf '%s\n' "${loc##*/}"
    fi
}

sha_hex() { $SHA "$1" | awk '{print $1}'; }

verify_sha() { # tarball shafile
    expected=$(awk 'NR==1{print $1}' "$2")
    [ -n "$expected" ] || err "empty checksum file"
    actual=$(sha_hex "$1")
    [ "$expected" = "$actual" ] || err "checksum mismatch for $(basename "$1")
  expected $expected
  got      $actual"
    info "checksum verified"
}

# ----- platform detection ---------------------------------------------------
detect_target() {
    os=$(uname -s)
    case "$os" in
        Linux)  OS=linux;  triple_os=unknown-linux-gnu ;;
        Darwin) OS=darwin; triple_os=apple-darwin ;;
        *) err "unsupported OS '$os' — kmux supports Linux and macOS only" ;;
    esac
    arch=$(uname -m)
    case "$arch" in
        x86_64|amd64)   ARCH=x86_64 ;;
        aarch64|arm64)  ARCH=aarch64 ;;
        *) err "unsupported architecture '$arch' — kmux ships x86_64 and aarch64" ;;
    esac
    TARGET="${ARCH}-${triple_os}"
}

# ----- install layout helpers ----------------------------------------------
# The release tarball is flat: kmuxd / kmux-vt-worker have an $ORIGIN
# (@loader_path) runpath, so they must stay in the same directory as
# libkmux_ghostty. We drop the whole binary set into <prefix>/lib/kmux and expose
# the user-facing commands on PATH via symlinks in <prefix>/bin. current_exe()
# resolves the symlink to the real lib/kmux path, so every sibling lookup
# (kmux->kmux-gtk, kmuxd->kmux-vt-worker, runpath->dylib) lands there.
install_bin() { # src dst   (copy + chmod, atomic-ish via temp + mv)
    cp "$1" "$2.tmp.$$" && chmod 755 "$2.tmp.$$" && mv -f "$2.tmp.$$" "$2"
}
link_bin() { # name
    ln -sf "../lib/kmux/$1" "$BINDIR/$1"
}

ensure_writable() { # dir — walk to nearest existing ancestor, check -w
    d=$1
    while [ ! -d "$d" ]; do d=$(dirname "$d"); done
    [ -w "$d" ] || err "prefix '$PREFIX' is not writable.
  Re-run with sudo:   curl -fsSL https://raw.githubusercontent.com/${REPO}/master/install.sh | sudo sh -s -- --prefix $PREFIX
  or pick a writable --prefix (default: \$HOME/.local)."
}

refresh_desktop() {
    have update-desktop-database && update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
    have gtk-update-icon-cache && gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true
}

# ----- uninstall ------------------------------------------------------------
do_uninstall() {
    LIBDIR="$PREFIX/lib/kmux"; BINDIR="$PREFIX/bin"
    removed=0
    for n in kmux kmuxd kmux-gtk; do
        if [ -e "$BINDIR/$n" ] || [ -L "$BINDIR/$n" ]; then rm -f "$BINDIR/$n"; removed=1; fi
    done
    if [ -d "$LIBDIR" ]; then rm -rf "$LIBDIR"; removed=1; fi
    rm -f "$PREFIX/share/applications/dev.getkono.kmux.desktop"
    rm -f "$PREFIX/share/icons/hicolor/scalable/apps/dev.getkono.kmux.svg"
    refresh_desktop
    if [ "$removed" = 1 ]; then
        info "removed kmux from $PREFIX"
        info "left your config (~/.config/kmux*) and daemon state untouched"
    else
        warn "no kmux install found under $PREFIX"
    fi
}

# ----- install --------------------------------------------------------------
do_install() {
    detect_target
    if [ -n "$VERSION" ]; then
        case "$VERSION" in v*) TAG="$VERSION" ;; *) TAG="v$VERSION" ;; esac
    else
        info "resolving latest release"
        TAG=$(latest_tag) || err "could not resolve the latest release tag"
    fi
    VER="${TAG#v}"
    tarball="kmux-${VER}-${TARGET}.tar.gz"
    base="https://github.com/${REPO}/releases/download/${TAG}"

    info "installing ${BOLD}kmux ${VER}${RST} (${TARGET}) → ${PREFIX}"
    [ "$HEADLESS" = 1 ] && info "headless: daemon + CLI only (no GUI)"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t kmux)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    info "downloading $tarball"
    download "${base}/${tarball}"          "$tmp/$tarball"
    download "${base}/${tarball}.sha256"   "$tmp/$tarball.sha256"
    verify_sha "$tmp/$tarball" "$tmp/$tarball.sha256"

    tar -C "$tmp" -xzf "$tmp/$tarball"
    src="$tmp/kmux-${VER}-${TARGET}"
    [ -d "$src" ] || err "unexpected tarball layout (missing $src)"

    LIBDIR="$PREFIX/lib/kmux"; BINDIR="$PREFIX/bin"
    ensure_writable "$LIBDIR"; ensure_writable "$BINDIR"
    mkdir -p "$LIBDIR" "$BINDIR"

    # Core (every flavor): CLI, daemon, isolation worker, shared library.
    install_bin "$src/kmux"  "$LIBDIR/kmux"
    install_bin "$src/kmuxd" "$LIBDIR/kmuxd"
    [ -f "$src/kmux-vt-worker" ] && install_bin "$src/kmux-vt-worker" "$LIBDIR/kmux-vt-worker"
    for lib in "$src"/libkmux_ghostty.*; do
        [ -f "$lib" ] && install_bin "$lib" "$LIBDIR/$(basename "$lib")"
    done
    link_bin kmux
    link_bin kmuxd

    # GUI (Linux, full install only): the GTK client + its desktop entry/icon.
    if [ "$OS" = linux ] && [ "$HEADLESS" = 0 ] && [ -f "$src/kmux-gtk" ]; then
        install_bin "$src/kmux-gtk" "$LIBDIR/kmux-gtk"
        link_bin kmux-gtk
        if [ -d "$src/share" ]; then
            mkdir -p "$PREFIX/share"
            cp -R "$src/share/." "$PREFIX/share/"
            refresh_desktop
        fi
        info "installed the GTK GUI + desktop entry (GTK4 + libadwaita are runtime deps you install from your distro)"
    fi

    trap - EXIT INT TERM
    rm -rf "$tmp"

    info "${BOLD}kmux ${VER} installed${RST}"
    post_install_notes
}

post_install_notes() {
    case ":$PATH:" in
        *":$BINDIR:"*) : ;;
        *) warn "$BINDIR is not on your PATH. Add it, then restart your shell:
      export PATH=\"$BINDIR:\$PATH\"" ;;
    esac

    if [ "$OS" = darwin ] && [ "$HEADLESS" = 0 ]; then
        printf '\n' >&2
        info "Installed the kmux CLI + daemon. The macOS desktop ${BOLD}GUI${RST} ships as a signed app:"
        info "    brew install --cask getkono/tap/kmux"
        info "    or download the .dmg → https://github.com/${REPO}/releases/latest"
    fi

    printf '\n' >&2
    info "Optional: enable dynamic tab-completion for kmux. Add the line for your"
    info "shell, then restart it (completes subcommands, flags, themes, sessions):"
    printf '\n' >&2
    printf '      %s\n' "bash (~/.bashrc):                  source <(COMPLETE=bash kmux)" >&2
    printf '      %s\n' "zsh  (~/.zshrc):                   source <(COMPLETE=zsh kmux)"  >&2
    printf '      %s\n' "fish (~/.config/fish/config.fish): COMPLETE=fish kmux | source" >&2
    printf '\n' >&2
    info "Run ${BOLD}kmux --version${RST} to check, ${BOLD}kmux${RST} to start. See https://github.com/${REPO}#install"
}

main() {
    PREFIX="${HOME}/.local"
    VERSION="${KMUX_VERSION:-}"
    HEADLESS=0
    ACTION=install

    while [ $# -gt 0 ]; do
        case "$1" in
            --headless)        HEADLESS=1 ;;
            --prefix)          shift; [ $# -gt 0 ] || err "--prefix needs a directory"; PREFIX=$1 ;;
            --prefix=*)        PREFIX=${1#*=} ;;
            --version)         shift; [ $# -gt 0 ] || err "--version needs a value"; VERSION=$1 ;;
            --version=*)       VERSION=${1#*=} ;;
            --uninstall)       ACTION=uninstall ;;
            -h|--help)         usage; exit 0 ;;
            *) err "unknown option '$1' (try --help)" ;;
        esac
        shift
    done

    # Normalize PREFIX to an absolute path without requiring it to exist yet.
    case "$PREFIX" in
        /*) : ;;
        ~*) PREFIX="${HOME}${PREFIX#\~}" ;;
        *)  PREFIX="$(pwd)/$PREFIX" ;;
    esac

    detect_tools
    if [ "$ACTION" = uninstall ]; then
        do_uninstall
    else
        do_install
    fi
}

main "$@"
