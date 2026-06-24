#!/bin/sh
# Build a self-contained kmux .flatpak bundle from a LOCAL release tarball (the CI
# artifact), so it doesn't depend on the release being published yet. Reuses the
# committed manifest's build-commands, swapping its remote `sources` for the local
# tarball + the repo's metainfo file. Requires flatpak + flatpak-builder and the
# org.gnome.Platform//48 + org.gnome.Sdk//48 runtimes installed (see the
# build-flatpak job in .github/workflows/release.yml).
#
#   packaging/flatpak/build-bundle.sh <tarball> <out.flatpak>
set -eu

tarball=${1:?usage: build-bundle.sh <tarball> <out.flatpak>}
out=${2:?usage: build-bundle.sh <tarball> <out.flatpak>}

here=$(CDPATH='' cd "$(dirname "$0")" && pwd)
manifest="$here/dev.getkono.kmux.yaml"
metainfo="$here/../../crates/kmux-gtk/data/dev.getkono.kmux.metainfo.xml"
tarball_abs=$(CDPATH='' cd "$(dirname "$tarball")" && pwd)/$(basename "$tarball")
metainfo_abs=$(CDPATH='' cd "$(dirname "$metainfo")" && pwd)/$(basename "$metainfo")
out_abs=$(CDPATH='' cd "$(dirname "$out")" && pwd)/$(basename "$out")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
local_manifest="$work/manifest.yaml"

# Reuse the committed build-commands; only the sources point at local files.
python3 - "$manifest" "$tarball_abs" "$metainfo_abs" "$local_manifest" <<'PY'
import sys, yaml
src_manifest, tarball, metainfo, out = sys.argv[1:5]
m = yaml.safe_load(open(src_manifest))
m['modules'][0]['sources'] = [
    {'type': 'archive', 'path': tarball},
    {'type': 'file', 'path': metainfo},
]
yaml.safe_dump(m, open(out, 'w'), sort_keys=False)
PY

flatpak-builder --force-clean --user --disable-rofiles-fuse \
    --repo="$work/repo" "$work/build" "$local_manifest"
flatpak build-bundle "$work/repo" "$out_abs" dev.getkono.kmux
echo "==> built $out_abs"
