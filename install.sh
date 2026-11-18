#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v rustc >/dev/null 2>&1 || { echo "Missing required tool: rustc" >&2; exit 1; }
command -v node >/dev/null 2>&1 || { echo "Missing required tool: node" >&2; exit 1; }
command -v npm >/dev/null 2>&1 || { echo "Missing required tool: npm" >&2; exit 1; }

if [ ! -x "$HOME/.gemini/bin/agy" ] && [ ! -x "/opt/homebrew/bin/agy" ] && [ ! -x "/usr/local/bin/agy" ] && ! command -v agy >/dev/null 2>&1; then
  echo "Antigravity CLI (agy) not found." >&2
  exit 1
fi

if [ ! -d "$SCRIPT_DIR/node_modules/@electron/asar" ]; then
  echo "Installing @electron/asar..."
  (cd "$SCRIPT_DIR" && npm install --no-audit --no-fund)
fi

if [ "$(uname -s)" = "Darwin" ]; then
  USER_DATA="$HOME/Library/Application Support/ropilot"
else
  USER_DATA="${XDG_CONFIG_HOME:-$HOME/.config}/ropilot"
fi

STAGE_DIR="$(mktemp -d)"
cleanup() { rm -rf "$STAGE_DIR"; }
trap cleanup EXIT

echo "Building RopilotGemini bridge and MCP router..."
rustc --edition 2021 -O -C debuginfo=0 "$SCRIPT_DIR/src/bridge.rs" -o "$STAGE_DIR/claude"
rustc --edition 2021 -O -C debuginfo=0 "$SCRIPT_DIR/src/router.rs" -o "$STAGE_DIR/ropilot-mcp-router"

pkill -x "Ropilot Studio" >/dev/null 2>&1 || true
pkill -x "ropilot-infra" >/dev/null 2>&1 || true
sleep 0.6

echo "Patching Ropilot Studio app.asar..."
node "$SCRIPT_DIR/patcher/patch_asar.mjs"

if [ "$(uname -s)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
  for APP_PATH in "/Applications/Ropilot Studio.app" "$HOME/Applications/Ropilot Studio.app"; do
    if [ -d "$APP_PATH" ]; then
      codesign --force --deep --sign - "$APP_PATH" >/dev/null 2>&1 || true
    fi
  done
fi

CLAUDE_DIR="$USER_DATA/claude"
BIN_DIR="$USER_DATA/bin"
mkdir -p "$CLAUDE_DIR" "$BIN_DIR"

if [ -f "$CLAUDE_DIR/claude" ] && [ ! -f "$CLAUDE_DIR/claude-real" ]; then
  cp "$CLAUDE_DIR/claude" "$CLAUDE_DIR/claude-real"
  chmod +x "$CLAUDE_DIR/claude-real"
fi

cp "$STAGE_DIR/claude" "$CLAUDE_DIR/claude"
cp "$STAGE_DIR/ropilot-mcp-router" "$BIN_DIR/ropilot-mcp-router"
chmod +x "$CLAUDE_DIR/claude" "$BIN_DIR/ropilot-mcp-router"

echo "RopilotGemini by xDTaraZ installed successfully."