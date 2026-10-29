#!/usr/bin/env bash
# Builds the hook executable and places it where Tauri expects a sidecar:
# src-tauri/binaries/agentgoodnight-hook-<target-triple>[.exe]
set -euo pipefail

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
cargo build --release -p agn-hook --target "$target"

ext=""
case "$target" in *windows*) ext=".exe" ;; esac

mkdir -p src-tauri/binaries
cp "target/$target/release/agentgoodnight-hook$ext" "src-tauri/binaries/agentgoodnight-hook-$target$ext"
echo "sidecar ready: src-tauri/binaries/agentgoodnight-hook-$target$ext"
