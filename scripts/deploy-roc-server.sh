#!/usr/bin/env bash
set -euo pipefail

TARGET_HOST="${1:-}"
if [[ -z "$TARGET_HOST" ]]; then
    echo "Usage: $0 user@host" >&2
    echo "  Example: $0 pi@raspberrypi.local" >&2
    exit 1
fi

for cmd in ssh rsync; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "error: '$cmd' not found. Install openssh-client and rsync." >&2
        exit 1
    fi
done

echo "Checking Docker is available on $TARGET_HOST..."
if ! ssh "$TARGET_HOST" 'command -v docker' &>/dev/null; then
    echo "error: docker not found on $TARGET_HOST. Install Docker (and the compose plugin) there first." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"

# roc-server's Dockerfile builds from the repo root (it's a Cargo workspace
# member — even though roc-server itself has no path dependency on the
# other crates, Cargo still needs every [workspace] member's Cargo.toml
# present to resolve the workspace manifest at all). Building on the target
# itself, not cross-compiling and copying a binary like deploy.sh does for
# lora-server, since docker-compose.yml is already written around `docker
# compose build` from source — simplest to stay consistent with that rather
# than introduce a second, different packaging path.
REMOTE_DIR="/opt/roc-server-src"

# Computed here, not inside build.rs, because the build context excludes
# .git (see ../.dockerignore) and the rsync below excludes it too — the
# remote build has no working .git to run `git describe` against at all.
# Forwarded to the remote below as a real env var (docker-compose.yml reads
# it from there for the build.args passed into the Dockerfile's ARG
# GIT_SHA) — see roc-server/build.rs for the consuming side.
GIT_SHA="$(git describe --always --dirty=.dirty --abbrev=8 2>/dev/null || echo unknown)"

echo "Syncing source to $TARGET_HOST:$REMOTE_DIR..."
# /opt requires root to create things in; sudo the mkdir, then chown it to
# the connecting user so the plain (non-sudo) rsync below can write into it
# directly — rsync-over-ssh doesn't cleanly support a sudo remote shell.
ssh "$TARGET_HOST" "sudo mkdir -p $REMOTE_DIR && sudo chown \"\$(whoami)\" $REMOTE_DIR"
rsync -az --delete \
    --exclude target --exclude '.git' \
    "$WORKSPACE_ROOT/Cargo.toml" "$WORKSPACE_ROOT/Cargo.lock" \
    "$WORKSPACE_ROOT/sx127x" "$WORKSPACE_ROOT/lora-server" "$WORKSPACE_ROOT/roc-server" \
    "$TARGET_HOST:$REMOTE_DIR/"

echo "Building and (re)starting roc-server on $TARGET_HOST..."
ssh "$TARGET_HOST" bash <<REMOTE
set -euo pipefail
cd "$REMOTE_DIR/roc-server"
# 'sudo VAR=val cmd' does NOT set VAR for cmd (sudo resets the environment
# by default) — 'sudo env VAR=val cmd' does, regardless of sudoers config.
if sudo docker compose version &>/dev/null; then
    sudo env GIT_SHA="$GIT_SHA" docker compose up -d --build
else
    sudo env GIT_SHA="$GIT_SHA" docker-compose up -d --build
fi
REMOTE

echo ""
echo "Deploy complete!"
echo "  Version:  $GIT_SHA (curl the deployed /status.json's \"version\" field to confirm it took)"
echo "  roc-server listening on port 8080 (see roc-server/docker-compose.yml for the mapping)"
echo "  Punch data persists in the 'roc-server-data' Docker volume across redeploys."
echo "  Logs:   ssh $TARGET_HOST 'cd $REMOTE_DIR/roc-server && sudo docker compose logs -f'"
echo "  Status: ssh $TARGET_HOST 'cd $REMOTE_DIR/roc-server && sudo docker compose ps'"
