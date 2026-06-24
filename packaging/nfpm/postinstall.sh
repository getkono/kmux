#!/bin/sh
# Refresh the desktop-entry and icon caches so the kmux GUI appears in the app
# grid right after `apt install` / `dnf install`. Best-effort and rpm-ostree
# safe: every command is guarded and never fails the transaction.
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
fi
exit 0
