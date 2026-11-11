#!/bin/sh
# Build the app bundle and package it as a ZIP (for Homebrew) and a DMG (for manual download).
# Usage: scripts/ci/package-release.sh <version> <output dir>
# Writes Agent-Switch-<version>-arm64.zip and Agent-Switch-<version>-arm64.dmg to <output dir>.
set -eu

[ "$#" -eq 2 ] || { echo "usage: $0 <version> <output dir>" >&2; exit 2; }
VERSION="$1"
OUT="$2"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BUILD_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
APP="$BUILD_DIR/bundle/Agent Switch.app"
ZIP="$OUT/Agent-Switch-$VERSION-arm64.zip"
DMG="$OUT/Agent-Switch-$VERSION-arm64.dmg"

mkdir -p "$OUT"
rm -f "$ZIP" "$DMG"

"$ROOT/scripts/bundle-app.sh"

# ditto keeps the code signature intact; --keepParent puts "Agent Switch.app" at the top of the ZIP.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

STAGE="$(mktemp -d)"
CHECK="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$CHECK"' EXIT
ditto "$APP" "$STAGE/Agent Switch.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Agent Switch" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null

# Prove that the shipped ZIP unpacks to a valid signed app.
ditto -x -k "$ZIP" "$CHECK"
codesign --verify --strict --deep "$CHECK/Agent Switch.app"

echo "zip: $ZIP"
echo "dmg: $DMG"
