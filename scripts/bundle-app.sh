#!/bin/sh
# Build the menu bar app and the CLI and wrap them in a signed .app bundle.
# The CLI sits in Contents/MacOS/agent-switch so one bundle carries both.
#
# A bundle is required: notifications with buttons (UNUserNotificationCenter) and the login item
# (SMAppService) both refuse to work from a bare executable.
#
# Usage: scripts/bundle-app.sh [--debug] [--install]
#   The bundle is signed ad hoc; no certificate needed.
#   --debug    use the debug build (default: release)
#   --install  copy the bundle to ~/Applications (replacing the old copy and a legacy AgentSwitch.app; quit the
#              app first)
set -eu

BUNDLE_ID="dev.agent-switch.menubar"
APP_NAME="Agent Switch.app"
LEGACY_APP_NAME="AgentSwitch.app"

PROFILE=release
INSTALL=0
for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE=debug ;;
        --install) INSTALL=1 ;;
        *) echo "error: unknown option $arg" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT_DIR="$BUILD_DIR/$PROFILE"
APP="$BUILD_DIR/bundle/$APP_NAME"

cd "$ROOT"
if [ "$PROFILE" = release ]; then
    cargo build --release -p agent-switch-menubar -p agent-switch-cli
else
    cargo build -p agent-switch-menubar -p agent-switch-cli
fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -n 1)"
[ -n "$VERSION" ] || { echo "error: cannot read the version from Cargo.toml" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$OUT_DIR/agent-switch-menubar" "$APP/Contents/MacOS/agent-switch-menubar"
# The CLI beside the app executable; the Homebrew cask links it onto PATH from here.
cp "$OUT_DIR/agent-switch" "$APP/Contents/MacOS/agent-switch"
# cat, not cp: cp keeps the quarantine flag of a downloaded source file (cp -X does too on APFS).
cat crates/menubar/AppIcon.icns > "$APP/Contents/Resources/AppIcon.icns"
sed "s/__VERSION__/$VERSION/g" crates/menubar/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

# Sign the nested binaries first, then the bundle (which seals Info.plist and the main executable).
# A fixed identifier keeps the designated requirement the same on every rebuild.
codesign --force --sign - --identifier "dev.agent-switch.cli" "$APP/Contents/MacOS/agent-switch"
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"
codesign --verify --strict --deep "$APP"

if [ "$INSTALL" -eq 1 ]; then
    for prefix in "$(brew --prefix 2>/dev/null || true)" /opt/homebrew /usr/local; do
        if [ -n "$prefix" ] && [ -e "$prefix/Caskroom/agent-switch" ]; then
            echo "error: Agent Switch is installed with Homebrew; run \`brew uninstall --cask agent-switch\` before a local install." >&2
            exit 1
        fi
    done
    DEST="$HOME/Applications"
    mkdir -p "$DEST"
    for name in "$APP_NAME" "$LEGACY_APP_NAME"; do
        if pgrep -f "$DEST/$name/Contents/MacOS/agent-switch-menubar" >/dev/null 2>&1; then
            echo "error: the installed app ($name) is running; quit it from its menu first." >&2
            exit 1
        fi
    done
    rm -rf "$DEST/$APP_NAME" "$DEST/$LEGACY_APP_NAME"
    cp -R "$APP" "$DEST/$APP_NAME"
    APP="$DEST/$APP_NAME"
fi

echo "bundle: $APP"
codesign -dvv "$APP" 2>&1 | grep -E '^(Identifier|Authority|Signature)' | sed 's/^/  /'
echo "note: built without a certificate; macOS may ask again for notification permission after a reinstall."
