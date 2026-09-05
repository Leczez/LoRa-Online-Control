#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo &>/dev/null; then
    echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"

echo "Building lora-server and lora-tui..."
# SPI+GPIO (via rppal) is required unconditionally now that E22/UART support
# has been dropped — this only builds/runs correctly on Linux with GPIO/SPI
# access (a Raspberry Pi), not a plain desktop machine.
cd "$WORKSPACE_ROOT"
cargo build --release -p lora-server

mkdir -p "$HOME/.local/bin"
for bin in lora-server lora-tui; do
    cp "$TARGET_DIR/release/$bin" "$HOME/.local/bin/$bin.tmp"
    mv "$HOME/.local/bin/$bin.tmp" "$HOME/.local/bin/$bin"
    chmod +x "$HOME/.local/bin/$bin"
    echo "Installed to $HOME/.local/bin/$bin"
done

if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "warning: $HOME/.local/bin is not in your PATH."
    echo "  Add this to your shell config: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
