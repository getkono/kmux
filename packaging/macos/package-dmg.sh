#!/bin/sh
# Codesign a prebuilt kmux.app (assembled by `mise run package-app`), wrap it in a
# .dmg, notarize it with Apple, and staple the ticket — producing a
# Gatekeeper-clean, offline-verifiable installer. Run by the release workflow on
# the macOS runners after the signing keychain is set up.
#
#   packaging/macos/package-dmg.sh <app-path> <dmg-out>
#
# Required environment:
#   SIGN_IDENTITY   "Developer ID Application: NAME (TEAMID)"
#   KEYCHAIN        path to the keychain holding the Developer ID cert
#   ASC_KEY_P8      path to the App Store Connect API key (.p8)
#   ASC_KEY_ID      App Store Connect API key id
#   ASC_ISSUER_ID   App Store Connect API issuer id
set -eu

app=${1:?usage: package-dmg.sh <app-path> <dmg-out>}
dmg=${2:?usage: package-dmg.sh <app-path> <dmg-out>}
: "${SIGN_IDENTITY:?SIGN_IDENTITY is required}"
: "${KEYCHAIN:?KEYCHAIN is required}"
: "${ASC_KEY_P8:?ASC_KEY_P8 is required}"
: "${ASC_KEY_ID:?ASC_KEY_ID is required}"
: "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required}"

sign() {
    codesign --force --options runtime --timestamp \
        --sign "$SIGN_IDENTITY" --keychain "$KEYCHAIN" "$@"
}

echo "==> codesigning bundle (inner-out)"
# Sign nested Mach-O before the outer bundle; --deep is deprecated for signing.
sign "$app/Contents/Frameworks/libkmux_ghostty.dylib"
sign "$app/Contents/MacOS/kmuxd"
sign "$app/Contents/MacOS/kmux-vt-worker"
sign "$app/Contents/MacOS/kmux"
sign "$app/Contents/MacOS/kmux-swift"
# The outer bundle carries the hardened-runtime entitlements.
codesign --force --options runtime --timestamp \
    --sign "$SIGN_IDENTITY" --keychain "$KEYCHAIN" \
    --entitlements kmux-swift/macos/entitlements.plist "$app"
codesign --verify --deep --strict --verbose=2 "$app"

echo "==> building dmg"
stage=$(mktemp -d)
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
rm -f "$dmg"
hdiutil create -volname kmux -srcfolder "$stage" -ov -format UDZO "$dmg"
rm -rf "$stage"

echo "==> signing + notarizing dmg"
codesign --force --timestamp --sign "$SIGN_IDENTITY" --keychain "$KEYCHAIN" "$dmg"
xcrun notarytool submit "$dmg" \
    --key "$ASC_KEY_P8" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" \
    --wait --timeout 30m
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"

shasum -a 256 "$dmg" > "$dmg.sha256"
echo "==> notarized + stapled $dmg"
