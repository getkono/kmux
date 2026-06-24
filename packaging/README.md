# Packaging

How kmux is packaged for distribution. All packages are built from the same
prebuilt release tarballs (`mise run package`, see
[docs/releasing.md](../docs/releasing.md)) — nothing is rebuilt from source per
format. The release workflow (`.github/workflows/release.yml`) drives everything;
this directory holds the inputs.

| Channel | Files | Built by | Auto-published |
| --- | --- | --- | --- |
| Universal installer | [`../install.sh`](../install.sh) | n/a (served from `master`) | n/a |
| macOS `.dmg` (signed app) | [`macos/package-dmg.sh`](macos/package-dmg.sh), [`../kmux-swift/macos/entitlements.plist`](../kmux-swift/macos/entitlements.plist) | `build` job (macOS) | GitHub Release |
| Homebrew cask + formula | [`homebrew/render.sh`](homebrew/render.sh) | `update-tap` job | `getkono/homebrew-tap` |
| Debian/Ubuntu `.deb`, Fedora/RHEL `.rpm` | [`nfpm/`](nfpm), [`relocate-rpath.sh`](relocate-rpath.sh) | `package-linux` job | GitHub Release |
| Arch `kmux-bin` | [`aur/kmux-bin/`](aur/kmux-bin) | `publish-aur` job | AUR |
| Flatpak | [`flatpak/`](flatpak) | `build-flatpak` job | GitHub Release |

`rpm-ostree install kmux-*.rpm` consumes the same `.rpm` — no separate artifact.

## Full vs headless

- **Full** (`kmux` deb/rpm, `kmux-bin` AUR, flatpak, the macOS app, `install.sh`
  default): GUI client + daemon.
- **Headless** (`kmux-headless` deb/rpm, the Homebrew formula,
  `install.sh --headless`): daemon + CLI, no GUI, no GTK dependency — for servers.

## FHS layout (deb/rpm/AUR)

```
/usr/bin/{kmux,kmuxd,kmux-vt-worker,kmux-gtk}    # kmux-gtk: full only
/usr/lib/kmux/libkmux_ghostty.so                  # kmuxd/kmux-vt-worker rpath -> here
/usr/share/applications/dev.getkono.kmux.desktop  # full only
/usr/share/icons/hicolor/scalable/apps/dev.getkono.kmux.svg
/usr/share/metainfo/dev.getkono.kmux.metainfo.xml
```

The release tarball keeps a relocatable `$ORIGIN`/`@loader_path` runpath (used by
`install.sh` and Homebrew); the FHS packages repoint kmuxd + kmux-vt-worker to the
absolute `/usr/lib/kmux` via `relocate-rpath.sh` before packaging.

## Required secrets / accounts (release workflow)

See [docs/releasing.md](../docs/releasing.md#distribution-secrets) for the full
list: Apple Developer ID + App Store Connect API key (macOS signing), an AUR
account + SSH key (`AUR_SSH_PRIVATE_KEY`), and a `HOMEBREW_TAP_TOKEN`. Each leg
stays dormant until its secrets are present.
