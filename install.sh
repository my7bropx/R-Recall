#!/usr/bin/env bash
# Build recall in release mode and install it system-wide.
#
#   ./install.sh                  # installs to /usr/local/bin (sudo if needed)
#   PREFIX=$HOME/.local ./install.sh   # per-user install, no sudo
#
set -euo pipefail

BIN_NAME="recall"
PREFIX="${PREFIX:-/usr/local}"
BINDIR="$PREFIX/bin"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
cd "$SCRIPT_DIR"

command -v cargo >/dev/null 2>&1 || {
    echo "error: cargo not found — install Rust first: https://rustup.rs" >&2
    exit 1
}

if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then
    echo "error: no C compiler found (cc/gcc) — required to build rusqlite's bundled SQLite" >&2
    exit 1
fi

echo "Building $BIN_NAME (release)..."
cargo build --release

BIN_PATH="target/release/$BIN_NAME"
[ -x "$BIN_PATH" ] || {
    echo "error: build did not produce $BIN_PATH" >&2
    exit 1
}

DEST="$BINDIR/$BIN_NAME"
echo "Installing to $DEST"

if install -Dm755 "$BIN_PATH" "$DEST" 2>/dev/null; then
    :
elif command -v sudo >/dev/null 2>&1; then
    sudo install -Dm755 "$BIN_PATH" "$DEST"
else
    cat >&2 <<EOF
error: no write access to $BINDIR and no sudo available.
Either re-run as root, or install per-user instead:
  PREFIX=\$HOME/.local ./install.sh
(make sure \$HOME/.local/bin is on your PATH)
EOF
    exit 1
fi

echo "Installed: $DEST"

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) echo "note: $BINDIR is not on your PATH — add it to use '$BIN_NAME' directly" ;;
esac

if ! command -v xclip >/dev/null 2>&1 && ! command -v xsel >/dev/null 2>&1 && ! command -v wl-copy >/dev/null 2>&1; then
    echo "note: install xclip, xsel, or wl-copy for clipboard support (yy, +y, Visual yank)"
fi

echo "Run '$BIN_NAME' to launch the TUI, or '$BIN_NAME --help' for CLI usage."
