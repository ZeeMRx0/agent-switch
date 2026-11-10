#!/bin/sh
# For local testing. Users install with Homebrew (see README.md).
# Build agent-switch in release mode and install it:
#   - the CLI to ~/.local/bin/agent-switch (with the signed Keychain helper beside it for --signed)
#   - the menu bar app to ~/Applications/Agent Switch.app
#
# Usage: scripts/install.sh [--signed] [--allow-vault-backend-change]
#   (default)  no certificate needed. Installs the CLI alone and an ad-hoc signed app. The vault keeps
#              logins in the Keychain through /usr/bin/security (logins over about 4 KB are stored in
#              parts). macOS may ask again for notification permission after a reinstall.
#              --adhoc is accepted as an alias of the default.
#   --signed   sign with the stable identity "agent-switch Code Signing" and install the Keychain helper
#              beside the CLI; fail with instructions if the identity is missing. See README.md for the
#              one-time certificate steps. Override the name with AGENT_SWITCH_SIGN_IDENTITY.
#   --allow-vault-backend-change
#              with --signed only. The signed helper opens a different vault than /usr/bin/security does,
#              so logins saved earlier are not visible to it. This script refuses when it finds an
#              existing vault unless you pass this flag (then re-add your accounts afterwards).
set -eu

IDENTITY="${AGENT_SWITCH_SIGN_IDENTITY:-agent-switch Code Signing}"
BIN_DIR="$HOME/.local/bin"

SIGNED=0
ALLOW_BACKEND_CHANGE=0
for arg in "$@"; do
    case "$arg" in
        --signed) SIGNED=1 ;;
        --adhoc) SIGNED=0 ;;
        --allow-vault-backend-change) ALLOW_BACKEND_CHANGE=1 ;;
        *) echo "error: unknown option $arg" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ "$SIGNED" -eq 1 ]; then
    if ! security find-identity -p codesigning | grep -F "\"$IDENTITY\"" >/dev/null 2>&1; then
        echo "error: code-signing identity \"$IDENTITY\" not found in the keychain." >&2
        echo "Create it once: Keychain Access > Certificate Assistant > Create a Certificate..." >&2
        echo "  Name \"$IDENTITY\", Identity Type Self Signed Root, Certificate Type Code Signing." >&2
        echo "Then check it with: security find-identity -p codesigning" >&2
        echo "Or run scripts/install.sh without --signed to install without the certificate." >&2
        exit 1
    fi
    if [ "$ALLOW_BACKEND_CHANGE" -eq 0 ] && security find-generic-password -s agent-switch >/dev/null 2>&1; then
        echo "error: a vault from /usr/bin/security already exists (Keychain service \"agent-switch\")." >&2
        echo "The signed helper opens a different vault, so those saved logins would stop being found." >&2
        echo "Re-run with --allow-vault-backend-change and add your accounts again afterwards, or run without --signed to keep using /usr/bin/security." >&2
        exit 1
    fi
elif [ -e "$BIN_DIR/agent-switch-keychain" ]; then
    echo "error: $BIN_DIR/agent-switch-keychain exists, so this is a signed install." >&2
    echo "The signed install keeps its logins in a separate vault; a default install would stop finding them." >&2
    echo "Re-run with --signed to keep them, or remove that file and add your accounts again." >&2
    exit 1
fi

cargo build --release --workspace
OUT="$ROOT/target/release"

mkdir -p "$BIN_DIR"
if [ "$SIGNED" -eq 1 ]; then
    "$ROOT/scripts/sign.sh" release
    install -m 755 "$OUT/agent-switch-keychain" "$BIN_DIR/agent-switch-keychain"
fi
install -m 755 "$OUT/agent-switch" "$BIN_DIR/agent-switch"
echo "installed: $BIN_DIR/agent-switch"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH; add it to your shell profile." ;;
esac

if [ "$SIGNED" -eq 1 ]; then
    "$ROOT/scripts/bundle-app.sh" --signed --install
else
    "$ROOT/scripts/bundle-app.sh" --install
fi
