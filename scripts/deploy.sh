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

echo "Cross-compiling lora-server (daemon) and lora-tui (attach client)..."
cd "$WORKSPACE_ROOT"
# shellcheck disable=SC2086
cross build --release -p lora-server $FEATURES --target "$RUST_TARGET"

SERVER_BINARY="$TARGET_DIR/$RUST_TARGET/release/lora-server"
TUI_BINARY="$TARGET_DIR/$RUST_TARGET/release/lora-tui"
[[ -f "$SERVER_BINARY" ]] || { echo "error: expected binary not found at $SERVER_BINARY" >&2; exit 1; }
[[ -f "$TUI_BINARY" ]] || { echo "error: expected binary not found at $TUI_BINARY" >&2; exit 1; }

echo "Copying binaries to $TARGET_HOST..."
scp "$SERVER_BINARY" "$TARGET_HOST:/tmp/lora-server"
scp "$TUI_BINARY" "$TARGET_HOST:/tmp/lora-tui"

echo "Installing on remote..."
ssh "$TARGET_HOST" bash <<'REMOTE'
set -euo pipefail

sudo install -m 755 /tmp/lora-server /usr/local/bin/lora-server
sudo install -m 755 /tmp/lora-tui /usr/local/bin/lora-tui
rm /tmp/lora-server /tmp/lora-tui
echo "Binaries installed to /usr/local/bin/lora-server and /usr/local/bin/lora-tui"

# A node deployed before the lora-cli -> lora-server rename has an old
# lora-cli.service still enabled under its own unit name — a different
# name means starting lora-server doesn't stop it, and both ends up
# fighting over the same serial port / SPI bus / GPIO pins at once.
if systemctl list-unit-files lora-cli.service &>/dev/null; then
    echo "Found old lora-cli.service — stopping and disabling it"
    sudo systemctl stop lora-cli || true
    sudo systemctl disable lora-cli || true
fi

if [[ ! -f /etc/lora-server/env ]] && [[ -f /etc/lora-cli/env ]]; then
    sudo mkdir -p /etc/lora-server
    sudo cp /etc/lora-cli/env /etc/lora-server/env
    echo "Migrated /etc/lora-cli/env -> /etc/lora-server/env"
fi

if [[ ! -f /etc/lora-server/env ]]; then
    sudo mkdir -p /etc/lora-server
    sudo tee /etc/lora-server/env > /dev/null <<'ENV'
# lora-server configuration
# Edit these values then: sudo systemctl restart lora-server
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
    echo "Created /etc/lora-server/env (edit to configure)"
else
    echo "Preserved existing /etc/lora-server/env"
fi

# The systemd unit's ExecStart depends on which radio transport this env file
# configures — regenerating the wrong template would silently switch a node
# using --radio spi back to the UART/HAT flag set (or vice versa).
if grep -q '^LORA_RADIO=spi' /etc/lora-server/env; then
    sudo tee /etc/systemd/system/lora-server.service > /dev/null <<'SERVICE'
[Unit]
Description=lora-server (LoRa daemon)
After=multi-user.target

[Service]
RuntimeDirectory=lora-server
EnvironmentFile=/etc/lora-server/env
ExecStart=/usr/local/bin/lora-server \
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
    sudo tee /etc/systemd/system/lora-server.service > /dev/null <<'SERVICE'
[Unit]
Description=lora-server (LoRa daemon)
After=multi-user.target

[Service]
RuntimeDirectory=lora-server
EnvironmentFile=/etc/lora-server/env
ExecStart=/usr/local/bin/lora-server \
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
sudo systemctl enable lora-server
sudo systemctl restart lora-server
echo "lora-server service enabled and started"
REMOTE

echo ""
echo "Deploy complete!"
echo "  Binaries: /usr/local/bin/lora-server, /usr/local/bin/lora-tui"
echo "  Config:   /etc/lora-server/env  (edit on device, then: sudo systemctl restart lora-server)"
echo "  Service:  sudo systemctl {start,stop,status} lora-server"
echo ""
echo "  To watch live traffic without stopping the service:"
echo "    ssh $TARGET_HOST 'lora-tui'"
