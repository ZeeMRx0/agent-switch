#!/bin/sh
# For local testing. Users install with Homebrew (see README.md).
# Build agent-switch in release mode and install it:
#   - the CLI to ~/.local/bin/agent-switch
#   - the menu bar app to ~/Applications/Agent Switch.app
#
# Usage: scripts/install.sh
#   No certificate needed. Installs the CLI and an ad-hoc signed app. The vault keeps logins in the
#   Keychain through /usr/bin/security (logins over about 4 KB are stored in parts). macOS may ask
#   again for notification permission after a reinstall.
set -eu

BIN_DIR="$HOME/.local/bin"

if [ "$#" -gt 0 ]; then
    echo "error: unknown option $1" >&2
    exit 2
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

cargo build --release --workspace
OUT="$ROOT/target/release"

mkdir -p "$BIN_DIR"
install -m 755 "$OUT/agent-switch" "$BIN_DIR/agent-switch"
echo "installed: $BIN_DIR/agent-switch"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH; add it to your shell profile." ;;
esac

"$ROOT/scripts/bundle-app.sh" --install
