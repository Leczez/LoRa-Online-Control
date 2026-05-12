# LoRa Install & Deploy Scripts — Design Spec

**Date:** 2026-05-12
**Status:** Approved

## Overview

Two shell scripts to make it easy to build and distribute `lora-cli` across the two supported targets: Linux desktop (serialport backend, no GPIO) and Raspberry Pi (rppal GPIO backend, deployed over SSH).

## Scripts

```
scripts/
├── install.sh    — local Linux desktop install
└── deploy.sh     — cross-compile + SSH deploy to Raspberry Pi
```

---

## `install.sh` — Local Desktop Install

Builds `lora-cli` without the `rpi` feature and installs it to `~/.local/bin`.

### Flow

1. Check `cargo` is on PATH; print a clear error and exit if not
2. `cargo build --release -p lora-cli` (no `--features rpi`)
3. `mkdir -p ~/.local/bin`
4. Copy `target/release/lora-cli` → `~/.local/bin/lora-cli`
5. Warn if `~/.local/bin` is not in `$PATH`

No `sudo` required. Binary goes to user space.

---

## `deploy.sh` — Raspberry Pi SSH Deploy

Cross-compiles for `aarch64-unknown-linux-gnu` using `cargo cross` and deploys over SSH.

### Usage

```bash
./scripts/deploy.sh pi@raspberrypi.local
```

### Flow

1. Validate argument (require `user@host`)
2. Check `cross`, `ssh`, `scp` on PATH; print clear errors and exit if missing
3. `cargo cross build --release -p lora-cli --features rpi --target aarch64-unknown-linux-gnu`
4. `scp target/aarch64-unknown-linux-gnu/release/lora-cli user@host:/tmp/lora-cli`
5. SSH remote block:
   - `sudo install -m 755 /tmp/lora-cli /usr/local/bin/lora-cli`
   - Create `/etc/lora-cli/env` template **only if it does not already exist**
   - Write `/etc/systemd/system/lora-cli.service` (always overwrite — service definition is managed by the script)
   - `sudo systemctl daemon-reload && sudo systemctl enable --now lora-cli`

### Prerequisites

- `cargo cross` installed locally (`cargo install cross`)
- Docker running locally (required by `cross`)
- SSH key-based auth to the target device (no password prompts)
- `sudo` without password on the target (standard Raspberry Pi OS default)

---

## Environment File — `/etc/lora-cli/env`

Created by `deploy.sh` on first deploy. **Never overwritten** on subsequent deploys so user edits are preserved.

```ini
# LoRa CLI configuration
# Edit these values then: sudo systemctl restart lora-cli
LORA_PORT=/dev/ttyS0
LORA_FREQ=868
LORA_ADDR=0
LORA_DEST=1
LORA_POWER=22
LORA_M0_PIN=22
LORA_M1_PIN=27
```

---

## Systemd Unit — `/etc/systemd/system/lora-cli.service`

`lora-cli` is a TUI app and requires an interactive terminal. The service is configured with `StandardInput=tty` and `TTYPath=/dev/tty1` so it auto-starts on the Pi's first virtual console. To use it interactively over SSH, stop the service first (`sudo systemctl stop lora-cli`) then run `lora-cli` directly.

```ini
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
```

---

## Out of Scope

- Config file support (`~/.config/lora-cli/config.toml`)
- ESP32 deployment (separate repo)
- Windows or macOS install scripts
- Automated uninstall
