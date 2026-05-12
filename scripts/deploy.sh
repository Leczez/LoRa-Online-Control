#!/usr/bin/env bash
set -euo pipefail

TARGET_HOST="${1:-}"
if [[ -z "$TARGET_HOST" ]]; then
    echo "Usage: $0 user@host" >&2
    echo "  Example: $0 pi@raspberrypi.local" >&2
    exit 1
fi

for cmd in cross ssh scp; do
    if ! command -v "$cmd" &>/dev/null; then
        case "$cmd" in
            cross) echo "error: 'cross' not found. Install with: cargo install cross" >&2 ;;
            ssh|scp) echo "error: '$cmd' not found. Install openssh-client." >&2 ;;
        esac
        exit 1
    fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"

echo "Cross-compiling lora-cli for aarch64 (RPi)..."
cd "$WORKSPACE_ROOT"
cross build --release -p lora-cli --features rpi --target aarch64-unknown-linux-gnu

BINARY="$TARGET_DIR/aarch64-unknown-linux-gnu/release/lora-cli"

echo "Copying binary to $TARGET_HOST..."
scp "$BINARY" "$TARGET_HOST:/tmp/lora-cli"

echo "Installing on remote..."
ssh "$TARGET_HOST" bash <<'REMOTE'
set -euo pipefail

sudo install -m 755 /tmp/lora-cli /usr/local/bin/lora-cli
echo "Binary installed to /usr/local/bin/lora-cli"

if [[ ! -f /etc/lora-cli/env ]]; then
    sudo mkdir -p /etc/lora-cli
    sudo tee /etc/lora-cli/env > /dev/null <<'ENV'
# LoRa CLI configuration
# Edit these values then: sudo systemctl restart lora-cli
LORA_PORT=/dev/ttyS0
LORA_FREQ=868
LORA_ADDR=0
LORA_DEST=1
LORA_POWER=22
LORA_M0_PIN=22
LORA_M1_PIN=27
ENV
    echo "Created /etc/lora-cli/env (edit to configure)"
else
    echo "Preserved existing /etc/lora-cli/env"
fi

sudo tee /etc/systemd/system/lora-cli.service > /dev/null <<'SERVICE'
[Unit]
Description=LoRa CLI
After=multi-user.target

[Service]
EnvironmentFile=/etc/lora-cli/env
ExecStart=/usr/local/bin/lora-cli \
  --port $LORA_PORT --freq $LORA_FREQ --addr $LORA_ADDR \
  --dest $LORA_DEST --power $LORA_POWER \
  --m0-pin $LORA_M0_PIN --m1-pin $LORA_M1_PIN
StandardInput=tty
TTYPath=/dev/tty1
Restart=on-failure

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
sudo systemctl enable --now lora-cli
echo "lora-cli service enabled and started"
REMOTE

echo ""
echo "Deploy complete!"
echo "  Binary:  /usr/local/bin/lora-cli"
echo "  Config:  /etc/lora-cli/env  (edit on device, then: sudo systemctl restart lora-cli)"
echo "  Service: sudo systemctl {start,stop,status} lora-cli"
echo ""
echo "  To use interactively over SSH:"
echo "    ssh $TARGET_HOST 'sudo systemctl stop lora-cli && lora-cli'"
