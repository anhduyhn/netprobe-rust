#!/usr/bin/env bash
#
# Cross-compile netprobe to a native Windows .exe and deploy it to the
# Windows drive at C:\network-tools (/mnt/c/network-tools from WSL).
#
# netprobe must run on Windows (it shells out to powershell.exe / WinRM for
# Hyper-V inventory), so we target x86_64-pc-windows-gnu rather than the host
# Linux toolchain. Requires the gnu target and mingw-w64:
#   rustup target add x86_64-pc-windows-gnu
#   sudo apt install gcc-mingw-w64-x86-64
#
# Run directly (./scripts/build-windows.sh) or via the pre-push git hook.
set -euo pipefail

TARGET="x86_64-pc-windows-gnu"
DEST="/mnt/c/network-tools"

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

echo "netprobe: building release binary for $TARGET ..."
cargo build --release --target "$TARGET"

BIN="$REPO_ROOT/target/$TARGET/release/netprobe.exe"
if [[ ! -f "$BIN" ]]; then
    echo "netprobe: expected binary not found at $BIN" >&2
    exit 1
fi

if [[ ! -d "$DEST" ]]; then
    mkdir -p "$DEST"
fi

# Deploy the exe. If a netprobe instance is currently running it holds a lock on
# the destination and a plain overwrite fails. On Windows a running exe can be
# renamed but not overwritten, so rotate the old one aside and retry.
if ! cp -f "$BIN" "$DEST/netprobe.exe" 2>/dev/null; then
    echo "netprobe: destination in use, rotating old binary aside..."
    rm -f "$DEST/netprobe.exe.old" 2>/dev/null || true
    mv "$DEST/netprobe.exe" "$DEST/netprobe.exe.old" 2>/dev/null || true
    cp -f "$BIN" "$DEST/netprobe.exe"
fi

# Ship the config template alongside the exe. Never overwrite a real config.toml
# (it holds credentials and is gitignored); only seed one if none exists yet.
if [[ -f "$REPO_ROOT/config.toml.sample" ]]; then
    cp -f "$REPO_ROOT/config.toml.sample" "$DEST/config.toml.sample"
    if [[ ! -f "$DEST/config.toml" ]]; then
        cp "$REPO_ROOT/config.toml.sample" "$DEST/config.toml"
        echo "netprobe: seeded $DEST/config.toml from sample (edit it before first run)"
    fi
fi

echo "netprobe: deployed -> C:\\network-tools\\netprobe.exe ($(du -h "$DEST/netprobe.exe" | cut -f1))"
