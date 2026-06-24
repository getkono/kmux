#!/bin/sh
# Repoint the libkmux_ghostty consumers (kmuxd + kmux-vt-worker) from the release
# tarball's relocatable `$ORIGIN` runpath to an absolute FHS libdir, so a
# /usr/bin install finds the shared library in /usr/lib/kmux. The tarball itself
# keeps `$ORIGIN` (for the relocatable install.sh / Homebrew layouts); only the
# distro packages (deb/rpm) need the absolute path. Run on the extracted tarball
# staging dir before handing it to nfpm.
#
#   packaging/relocate-rpath.sh <stage-dir> <libdir>     # e.g. ./stage /usr/lib/kmux
set -eu

stage=${1:?usage: relocate-rpath.sh <stage-dir> <libdir>}
libdir=${2:?usage: relocate-rpath.sh <stage-dir> <libdir>}

command -v patchelf >/dev/null 2>&1 || {
    echo "error: patchelf is required (apt-get install patchelf / dnf install patchelf)" >&2
    exit 1
}

for bin in kmuxd kmux-vt-worker; do
    if [ -f "$stage/$bin" ]; then
        patchelf --set-rpath "$libdir" "$stage/$bin"
        echo "==> set rpath of $bin to $libdir"
    fi
done
