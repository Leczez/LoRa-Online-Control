#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo &>/dev/null; then
    echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"

echo "Building lora-cli (desktop)..."
# The rpi feature (rppal GPIO) is opt-in; the default build is the desktop variant.
cd "$WORKSPACE_ROOT"
cargo build --release -p lora-cli

mkdir -p "$HOME/.local/bin"
cp "$TARGET_DIR/release/lora-cli" "$HOME/.local/bin/lora-cli.tmp"
mv "$HOME/.local/bin/lora-cli.tmp" "$HOME/.local/bin/lora-cli"
chmod +x "$HOME/.local/bin/lora-cli"
echo "Installed to $HOME/.local/bin/lora-cli"

if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "warning: $HOME/.local/bin is not in your PATH."
    echo "  Add this to your shell config: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
