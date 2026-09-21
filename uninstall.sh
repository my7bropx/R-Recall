#!/usr/bin/env bash
# Remove a system-wide recall install made by install.sh.
#
#   ./uninstall.sh
#   PREFIX=$HOME/.local ./uninstall.sh   # matches a per-user install
#
set -euo pipefail

BIN_NAME="recall"
PREFIX="${PREFIX:-/usr/local}"
DEST="$PREFIX/bin/$BIN_NAME"

if [ ! -e "$DEST" ]; then
    echo "$DEST not found — nothing to remove."
    exit 0
fi

echo "Removing $DEST"
if [ -w "$(dirname "$DEST")" ] || [ "$(id -u)" -eq 0 ]; then
    rm -f "$DEST"
elif command -v sudo >/dev/null 2>&1; then
    sudo rm -f "$DEST"
else
    echo "error: no write access to $(dirname "$DEST") and no sudo available." >&2
    exit 1
fi

echo "Removed. Your database at \$HOME/.local/share/recall/recall.db is untouched."
