#!/bin/sh
# Sign the compiled binaries with the stable self-signed identity, so Keychain items created by the
# keychain helper keep trusting it across rebuilds (an ad-hoc signature changes on every compile).
#
# One-time setup: Keychain Access > Certificate Assistant > Create a Certificate...
#   Name: "agent-switch Code Signing"   Identity Type: Self Signed Root   Certificate Type: Code Signing
# The first codesign run may ask to use the private key: choose "Always Allow".
#
# Usage: scripts/sign.sh [debug|release]      (default: release)
# Override the identity name with AGENT_SWITCH_SIGN_IDENTITY.
set -eu

IDENTITY="${AGENT_SWITCH_SIGN_IDENTITY:-agent-switch Code Signing}"
PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$ROOT/target/$PROFILE"

if ! security find-identity -p codesigning | grep -F "\"$IDENTITY\"" >/dev/null 2>&1; then
    echo "error: code-signing identity \"$IDENTITY\" not found in the keychain." >&2
    echo "Create it once: Keychain Access > Certificate Assistant > Create a Certificate..." >&2
    echo "  Name \"$IDENTITY\", Identity Type Self Signed Root, Certificate Type Code Signing." >&2
    echo "Then check it with: security find-identity -p codesigning" >&2
    exit 1
fi

signed=0
for name in agent-switch agent-switch-keychain agent-switch-menubar; do
    bin="$OUT_DIR/$name"
    [ -f "$bin" ] || continue
    # A fixed identifier keeps the designated requirement the same on every rebuild.
    codesign --force --sign "$IDENTITY" --identifier "dev.agent-switch.$name" "$bin"
    codesign --verify --strict "$bin"
    echo "signed $bin"
    signed=$((signed + 1))
done

if [ "$signed" -eq 0 ]; then
    echo "error: no binaries found in $OUT_DIR; run cargo build first (add --release for the release profile)." >&2
    exit 1
fi
