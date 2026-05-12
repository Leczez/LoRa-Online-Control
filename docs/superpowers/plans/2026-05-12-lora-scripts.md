# LoRa Install & Deploy Scripts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two shell scripts — `scripts/install.sh` (local desktop build + install) and `scripts/deploy.sh` (cross-compile for RPi, SCP binary, remote systemd setup via SSH).

**Architecture:** Both scripts are self-contained bash files with prereq checks, build steps, and install/deploy logic. `deploy.sh` runs all remote commands in a single SSH heredoc to avoid multiple round-trips.

**Tech Stack:** bash, `cargo`, `cargo cross`, `ssh`, `scp`, systemd

---

## File Structure

```
scripts/
├── install.sh    — local Linux desktop: build (no rpi feature) + install to ~/.local/bin
└── deploy.sh     — RPi: cross-compile (aarch64) + SCP + remote install + systemd service
```

---

### Task 1: `scripts/install.sh`

**Files:**
- Create: `scripts/install.sh`

- [ ] **Step 1: Create `scripts/` directory and `install.sh`**

```bash
mkdir -p scripts
```

Create `scripts/install.sh` with the following content:

```bash
#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo &>/dev/null; then
    echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"

echo "Building lora-cli (desktop)..."
cd "$WORKSPACE_ROOT"
cargo build --release -p lora-cli

mkdir -p "$HOME/.local/bin"
cp target/release/lora-cli "$HOME/.local/bin/lora-cli"
echo "Installed to $HOME/.local/bin/lora-cli"

if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "warning: $HOME/.local/bin is not in your PATH."
    echo "  Add this to your shell config: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
```

- [ ] **Step 2: Make executable**

```bash
chmod +x scripts/install.sh
```

- [ ] **Step 3: Verify the script is valid bash**

```bash
bash -n scripts/install.sh
```

Expected: no output (exit 0).

- [ ] **Step 4: Commit**

```bash
git add scripts/install.sh
git commit -m "feat: add install.sh for local desktop install"
```

---

### Task 2: `scripts/deploy.sh`

**Files:**
- Create: `scripts/deploy.sh`

- [ ] **Step 1: Create `scripts/deploy.sh`**

Create `scripts/deploy.sh` with the following content:

```bash
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

echo "Cross-compiling lora-cli for aarch64 (RPi)..."
cd "$WORKSPACE_ROOT"
cross build --release -p lora-cli --features rpi --target aarch64-unknown-linux-gnu

BINARY="target/aarch64-unknown-linux-gnu/release/lora-cli"

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
```

- [ ] **Step 2: Make executable**

```bash
chmod +x scripts/deploy.sh
```

- [ ] **Step 3: Verify the script is valid bash**

```bash
bash -n scripts/deploy.sh
```

Expected: no output (exit 0).

- [ ] **Step 4: Commit**

```bash
git add scripts/deploy.sh
git commit -m "feat: add deploy.sh for RPi SSH deploy with systemd service"
```
