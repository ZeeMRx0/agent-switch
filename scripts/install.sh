#!/bin/sh
# Build agent-switch in release mode and install it:
#   - the CLI to ~/.local/bin/agent-switch (with the signed Keychain helper beside it when signed)
#   - the menu bar app to ~/Applications/AgentSwitch.app
#
# Usage: scripts/install.sh [--adhoc] [--allow-vault-backend-change]
#   (default)  sign with the stable identity "agent-switch Code Signing"; fail with instructions if it is
#              missing. See README.md for the one-time certificate steps. Override the name with
#              AGENT_SWITCH_SIGN_IDENTITY.
#   --adhoc    no certificate: install the CLI alone and an ad-hoc signed app. The CLI then keeps the vault
#              in the Keychain through /usr/bin/security. The app's Keychain trust and notification
#              permission reset on every reinstall.
#   --allow-vault-backend-change
#              signed install only. The signed helper opens a different vault than /usr/bin/security does, so
#              logins saved earlier are not visible to it. This script refuses when it finds an existing
#              vault unless you pass this flag (then re-add your accounts afterwards).
set -eu

IDENTITY="${AGENT_SWITCH_SIGN_IDENTITY:-agent-switch Code Signing}"
BIN_DIR="$HOME/.local/bin"

ADHOC=0
ALLOW_BACKEND_CHANGE=0
for arg in "$@"; do
    case "$arg" in
        --adhoc) ADHOC=1 ;;
        --allow-vault-backend-change) ALLOW_BACKEND_CHANGE=1 ;;
        *) echo "error: unknown option $arg" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [ "$ADHOC" -eq 0 ]; then
    if ! security find-identity -p codesigning | grep -F "\"$IDENTITY\"" >/dev/null 2>&1; then
        echo "error: code-signing identity \"$IDENTITY\" not found in the keychain." >&2
        echo "Create it once: Keychain Access > Certificate Assistant > Create a Certificate..." >&2
        echo "  Name \"$IDENTITY\", Identity Type Self Signed Root, Certificate Type Code Signing." >&2
        echo "Then check it with: security find-identity -p codesigning" >&2
        echo "Or pass --adhoc to install without the certificate (see the header of this script)." >&2
        exit 1
    fi
    if [ "$ALLOW_BACKEND_CHANGE" -eq 0 ] && security find-generic-password -s agent-switch >/dev/null 2>&1; then
        echo "error: a vault from /usr/bin/security already exists (Keychain service \"agent-switch\")." >&2
        echo "The signed helper opens a different vault, so those saved logins would stop being found." >&2
        echo "Re-run with --allow-vault-backend-change and add your accounts again afterwards, or use --adhoc." >&2
        exit 1
    fi
fi

cargo build --release --workspace
OUT="$ROOT/target/release"

mkdir -p "$BIN_DIR"
if [ "$ADHOC" -eq 0 ]; then
    "$ROOT/scripts/sign.sh" release
    install -m 755 "$OUT/agent-switch-keychain" "$BIN_DIR/agent-switch-keychain"
fi
install -m 755 "$OUT/agent-switch" "$BIN_DIR/agent-switch"
echo "installed: $BIN_DIR/agent-switch"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on PATH; add it to your shell profile." ;;
esac

if [ "$ADHOC" -eq 1 ]; then
    "$ROOT/scripts/bundle-app.sh" --adhoc --install
else
    "$ROOT/scripts/bundle-app.sh" --install
fi
