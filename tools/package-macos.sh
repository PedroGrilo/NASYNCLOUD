#!/bin/bash
# Builds dist/nasdrive.app (the menu bar app, desktop/, with the nasdrive service
# next to it) and dist/nasdrive-<version>-macos-<arch>.dmg. macOS only; building
# needs macFUSE installed (the service links against its libfuse).
# The app is ad-hoc signed, not notarized: see packaging/macos/LEIA-ME.txt.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
TARGET_DIR=${CARGO_TARGET_DIR:-target}
OUT=dist
APP=$OUT/nasdrive.app
DMG=$OUT/nasdrive-$VERSION-macos-$ARCH.dmg

cargo build --release --locked -p nasdrive -p nasdrive-desktop

rm -rf "$APP" "$OUT/dmg" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/nasdrive" "$APP/Contents/MacOS/nasdrive"
cp "$TARGET_DIR/release/nasdrive-desktop" "$APP/Contents/MacOS/nasdrive-desktop"
chmod 755 "$APP/Contents/MacOS/nasdrive" "$APP/Contents/MacOS/nasdrive-desktop"
cp assets/nasdrive.icns "$APP/Contents/Resources/nasdrive.icns"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --sign - "$APP/Contents/MacOS/nasdrive"
codesign --force --sign - "$APP/Contents/MacOS/nasdrive-desktop"
codesign --force --sign - "$APP"
codesign --verify --strict "$APP"

mkdir -p "$OUT/dmg"
cp -R "$APP" "$OUT/dmg/"
ln -s /Applications "$OUT/dmg/Aplicações"
cp packaging/macos/LEIA-ME.txt "$OUT/dmg/LEIA-ME.txt"
hdiutil create -quiet -volname "nasdrive $VERSION" -srcfolder "$OUT/dmg" -fs HFS+ -format UDZO -ov "$DMG"
rm -rf "$OUT/dmg"
echo "$DMG"
