#!/bin/sh
# Render packaging/homebrew/agent-switch.rb for a release and push it to the Homebrew tap.
# Usage: scripts/ci/bump-homebrew-cask.sh <version> <release ZIP>
# Env:   HOMEBREW_TAP_TOKEN   token with Contents write access to the tap (required unless HOMEBREW_TAP_REMOTE is set)
#        HOMEBREW_TAP_REMOTE  git remote of the tap (default: the GitHub URL with the token); set it for a dry run
set -eu

CASK="agent-switch"
TAP_BRANCH="main"

[ "$#" -eq 2 ] || { echo "usage: $0 <version> <release ZIP>" >&2; exit 2; }
VERSION="$1"
ASSET="$2"

[ -f "$ASSET" ] || { echo "error: asset not found: $ASSET" >&2; exit 1; }

if [ -z "${HOMEBREW_TAP_REMOTE:-}" ]; then
    [ -n "${HOMEBREW_TAP_TOKEN:-}" ] || {
        echo "error: HOMEBREW_TAP_TOKEN is empty. Add it as a repository secret (Contents read/write on ZeeMRx0/homebrew-tap)." >&2
        exit 1
    }
    HOMEBREW_TAP_REMOTE="https://x-access-token:${HOMEBREW_TAP_TOKEN}@github.com/ZeeMRx0/homebrew-tap.git"
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TEMPLATE="$ROOT/packaging/homebrew/$CASK.rb"
SHA256="$(shasum -a 256 "$ASSET" | cut -d' ' -f1)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Git output is discarded: its messages can contain the remote URL, which holds the token.
git clone --quiet --branch "$TAP_BRANCH" "$HOMEBREW_TAP_REMOTE" "$WORK/tap" >/dev/null 2>&1 \
    || { echo "error: cannot clone the tap" >&2; exit 1; }
cd "$WORK/tap"
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

attempt=1
while :; do
    mkdir -p Casks
    sed -e "s/__VERSION__/$VERSION/g" -e "s/__SHA256__/$SHA256/g" "$TEMPLATE" > "Casks/$CASK.rb"
    git add "Casks/$CASK.rb"
    if git diff --cached --quiet; then
        echo "Casks/$CASK.rb is already up to date."
        exit 0
    fi
    git commit --quiet -m "$CASK $VERSION"
    if git push --quiet origin "HEAD:$TAP_BRANCH" >/dev/null 2>&1; then
        echo "Pushed $CASK $VERSION to the tap."
        exit 0
    fi
    [ "$attempt" -lt 3 ] || { echo "error: push to the tap failed after $attempt attempts" >&2; exit 1; }
    attempt=$((attempt + 1))
    git reset --quiet --hard HEAD~1
    git pull --quiet --rebase origin "$TAP_BRANCH" >/dev/null 2>&1 || { echo "error: cannot rebase on the tap" >&2; exit 1; }
done
