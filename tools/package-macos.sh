#!/bin/bash
# Builds dist/NASYNCLOUD.app (the menu bar app, desktop/, with the nasyncloud service
# next to it) and dist/NASYNCLOUD-<version>-macos-<arch>.dmg. macOS only; nothing
# else to install (the mount uses the NFS client that comes with macOS).
# The app is ad-hoc signed, not notarized: see packaging/macos/LEIA-ME.txt.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(uname)" = Darwin ] || { echo "macOS only"; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
TARGET_DIR=${CARGO_TARGET_DIR:-target}
OUT=dist
APP=$OUT/NASYNCLOUD.app
DMG=$OUT/NASYNCLOUD-$VERSION-macos-$ARCH.dmg

cargo build --release --locked -p nasyncloud -p nasyncloud-desktop

rm -rf "$APP" "$OUT/dmg" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/nasyncloud" "$APP/Contents/MacOS/nasyncloud"
cp "$TARGET_DIR/release/nasyncloud-desktop" "$APP/Contents/MacOS/nasyncloud-desktop"
chmod 755 "$APP/Contents/MacOS/nasyncloud" "$APP/Contents/MacOS/nasyncloud-desktop"
cp assets/nasyncloud.icns "$APP/Contents/Resources/nasyncloud.icns"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --sign - "$APP/Contents/MacOS/nasyncloud"
codesign --force --sign - "$APP/Contents/MacOS/nasyncloud-desktop"
codesign --force --sign - "$APP"
codesign --verify --strict "$APP"

mkdir -p "$OUT/dmg"
cp -R "$APP" "$OUT/dmg/"
ln -s /Applications "$OUT/dmg/Aplicações"
cp packaging/macos/LEIA-ME.txt "$OUT/dmg/LEIA-ME.txt"
hdiutil create -quiet -volname "NASYNCLOUD $VERSION" -srcfolder "$OUT/dmg" -fs HFS+ -format UDZO -ov "$DMG"
rm -rf "$OUT/dmg"
echo "$DMG"
