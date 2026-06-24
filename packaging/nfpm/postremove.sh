#!/bin/sh
# Drop the kmux desktop entry / icon from the shared caches after removal.
# Best-effort and rpm-ostree safe: guarded, never fails the transaction.
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
fi
exit 0
