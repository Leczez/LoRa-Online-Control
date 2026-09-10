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

echo "Cross-compiling lora-server (daemon) and lora-tui (attach client)..."
cd "$WORKSPACE_ROOT"
# Computed here, not inside build.rs, because `cross build` runs in a Docker
# container that can't see this worktree's real .git (a worktree's .git is
# a file pointing at an absolute host path the container doesn't have) —
# forwarded into the container via Cross.toml's `[build.env] passthrough`.
# See lora-server/build.rs for the consuming side.
export GIT_SHA="$(git describe --always --dirty=.dirty --abbrev=8 2>/dev/null || echo unknown)"
cross build --release -p lora-server --target "$RUST_TARGET"

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
    sudo rm -f /etc/systemd/system/lora-cli.service
    sudo systemctl daemon-reload
fi

# The old lora-cli binary has no service pointing at it anymore, but left
# on disk it's still a loaded gun: manually running it would grab the same
# serial port/SPI bus/GPIO pins lora-server is using and reproduce the
# exact hardware lockup the service split was fixed for.
if [[ -f /usr/local/bin/lora-cli ]]; then
    sudo rm -f /usr/local/bin/lora-cli
    echo "Removed old /usr/local/bin/lora-cli"
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
LORA_RESET_PIN=25
LORA_SF=7
LORA_BW_HZ=125000
LORA_CR=5
LORA_SYNC_WORD=18
LORA_FREQ=433
LORA_ADDR=0
LORA_DEST=1
LORA_POWER=22
LORA_HEARTBEAT_INTERVAL=60
LORA_NETWORK_ID=LOC
LORA_HEALTH_LISTEN=0.0.0.0:8081
LORA_HEALTH_CHECK_INTERVAL_SECS=30
# Uncomment and point at roc-server's own /health to enable the mutual
# reachability check (e.g. http://100.x.y.z:8080/health, or
# http://host.docker.internal:8080/health if roc-server runs in Docker on
# this same host — see roc-server/docker-compose.yml). Left unset by
# default: lora-server still serves its own /health on LORA_HEALTH_LISTEN
# regardless, it just won't check roc-server's.
#LORA_ROC_HEALTH_URL=http://127.0.0.1:8080/health
ENV
    echo "Created /etc/lora-server/env (edit to configure)"
else
    echo "Preserved existing /etc/lora-server/env"
fi

sudo tee /etc/systemd/system/lora-server.service > /dev/null <<'SERVICE'
[Unit]
Description=lora-server (LoRa daemon)
After=multi-user.target

[Service]
RuntimeDirectory=lora-server
EnvironmentFile=/etc/lora-server/env
# None of LORA_ROC_HEALTH_URL, LORA_HEALTH_LISTEN, LORA_HEALTH_CHECK_INTERVAL_SECS,
# or LORA_WEB_LISTEN are passed as explicit --flags here, even though the
# latter three do have defaults: referencing ${VAR} in ExecStart for a var
# that's genuinely unset in /etc/lora-server/env (e.g. an existing
# deployment's env file predating one of these settings, preserved as-is on
# redeploy — see below) substitutes an empty string, which clap treats as an
# explicitly-provided empty value, not "not set" — so an old env file with a
# newly-added-since var missing crashes the daemon instead of silently using
# the default. Left for clap's own `env = "..."` on each of these args to
# pick up directly from the process environment instead, which correctly
# distinguishes "unset" from "set to empty" and falls back to the default.
ExecStart=/usr/local/bin/lora-server \
  --reset-pin ${LORA_RESET_PIN} \
  --sf ${LORA_SF} --bw-hz ${LORA_BW_HZ} --cr ${LORA_CR} --sync-word ${LORA_SYNC_WORD} \
  --freq ${LORA_FREQ} --addr ${LORA_ADDR} --dest ${LORA_DEST} --power ${LORA_POWER} \
  --heartbeat-interval ${LORA_HEARTBEAT_INTERVAL} --network-id "${LORA_NETWORK_ID}"
StandardOutput=journal
StandardError=journal
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
sudo systemctl enable lora-server
sudo systemctl restart lora-server
echo "lora-server service enabled and started"
REMOTE

echo ""
echo "Deploy complete!"
echo "  Version:  $GIT_SHA (curl the deployed /status.json's \"version\" field to confirm it took)"
echo "  Binaries: /usr/local/bin/lora-server, /usr/local/bin/lora-tui"
echo "  Config:   /etc/lora-server/env  (edit on device, then: sudo systemctl restart lora-server)"
echo "  Service:  sudo systemctl {start,stop,status} lora-server"
echo ""
echo "  To watch live traffic without stopping the service:"
echo "    ssh $TARGET_HOST 'lora-tui'"
