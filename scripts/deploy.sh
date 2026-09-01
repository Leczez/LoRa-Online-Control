#!/usr/bin/env bash
set -euo pipefail

TARGET_HOST="${1:-}"
if [[ -z "$TARGET_HOST" ]]; then
    echo "Usage: $0 user@host" >&2
    echo "  Example: $0 pi@raspberrypi.local" >&2
    exit 1
fi

if ! command -v cross &>/dev/null; then
    echo "'cross' not found. Installing via cargo..."
    cargo install cross
    export PATH="$HOME/.cargo/bin:$PATH"
fi

for cmd in ssh scp; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "error: '$cmd' not found. Install openssh-client." >&2
        exit 1
    fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"

echo "Detecting remote architecture..."
REMOTE_ARCH=$(ssh "$TARGET_HOST" 'uname -m')
case "$REMOTE_ARCH" in
    aarch64)       RUST_TARGET="aarch64-unknown-linux-gnu" ;;
    armv7l|armhf)  RUST_TARGET="armv7-unknown-linux-gnueabihf" ;;
    x86_64)        RUST_TARGET="x86_64-unknown-linux-gnu" ;;
    *)
        echo "error: unsupported remote architecture: $REMOTE_ARCH" >&2
        exit 1
        ;;
esac

IS_RPI=$(ssh "$TARGET_HOST" 'grep -qi "raspberry pi" /proc/cpuinfo && echo yes || echo no')
if [[ "$IS_RPI" == "yes" ]]; then
    FEATURES="--features rpi"
    echo "Detected: Raspberry Pi ($REMOTE_ARCH) → $RUST_TARGET"
else
    FEATURES=""
    echo "Detected: Linux device ($REMOTE_ARCH) → $RUST_TARGET"
fi

echo "Cross-compiling lora-cli..."
cd "$WORKSPACE_ROOT"
# shellcheck disable=SC2086
cross build --release -p lora-cli $FEATURES --target "$RUST_TARGET"

BINARY="$TARGET_DIR/$RUST_TARGET/release/lora-cli"
[[ -f "$BINARY" ]] || { echo "error: expected binary not found at $BINARY" >&2; exit 1; }

echo "Copying binary to $TARGET_HOST..."
scp "$BINARY" "$TARGET_HOST:/tmp/lora-cli"

echo "Installing on remote..."
ssh "$TARGET_HOST" bash <<'REMOTE'
set -euo pipefail

sudo install -m 755 /tmp/lora-cli /usr/local/bin/lora-cli
rm /tmp/lora-cli
echo "Binary installed to /usr/local/bin/lora-cli"

if [[ ! -f /etc/lora-cli/env ]]; then
    sudo mkdir -p /etc/lora-cli
    sudo tee /etc/lora-cli/env > /dev/null <<'ENV'
# LoRa CLI configuration
# Edit these values then: sudo systemctl restart lora-cli
LORA_PORT=/dev/ttyS0
LORA_FREQ=433
LORA_AIR_SPEED=1200
LORA_ADDR=0
LORA_DEST=1
LORA_POWER=22
LORA_HEARTBEAT_INTERVAL=60
LORA_M0_PIN=22
LORA_M1_PIN=27
ENV
    echo "Created /etc/lora-cli/env (edit to configure)"
else
    echo "Preserved existing /etc/lora-cli/env"
fi

# The systemd unit's ExecStart depends on which radio transport this env file
# configures — regenerating the wrong template would silently switch a node
# using --radio spi back to the UART/HAT flag set (or vice versa).
if grep -q '^LORA_RADIO=spi' /etc/lora-cli/env; then
    sudo tee /etc/systemd/system/lora-cli.service > /dev/null <<'SERVICE'
[Unit]
Description=LoRa CLI
After=multi-user.target

[Service]
RuntimeDirectory=lora-cli
EnvironmentFile=/etc/lora-cli/env
ExecStart=/usr/local/bin/lora-cli \
  --radio ${LORA_RADIO} --reset-pin ${LORA_RESET_PIN} \
  --sf ${LORA_SF} --bw-hz ${LORA_BW_HZ} --cr ${LORA_CR} --sync-word ${LORA_SYNC_WORD} \
  --freq ${LORA_FREQ} --addr ${LORA_ADDR} --dest ${LORA_DEST} --power ${LORA_POWER} \
  --heartbeat-interval ${LORA_HEARTBEAT_INTERVAL}
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE
else
    sudo tee /etc/systemd/system/lora-cli.service > /dev/null <<'SERVICE'
[Unit]
Description=LoRa CLI
After=multi-user.target

[Service]
RuntimeDirectory=lora-cli
EnvironmentFile=/etc/lora-cli/env
ExecStart=/usr/local/bin/lora-cli \
  --port ${LORA_PORT} --freq ${LORA_FREQ} --air-speed ${LORA_AIR_SPEED} \
  --addr ${LORA_ADDR} --dest ${LORA_DEST} --power ${LORA_POWER} \
  --heartbeat-interval ${LORA_HEARTBEAT_INTERVAL} \
  --m0-pin ${LORA_M0_PIN} --m1-pin ${LORA_M1_PIN}
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE
fi

sudo systemctl daemon-reload
sudo systemctl enable lora-cli
sudo systemctl restart lora-cli
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
