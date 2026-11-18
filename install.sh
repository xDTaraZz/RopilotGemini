#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v rustc >/dev/null 2>&1 || { echo "Missing required tool: rustc" >&2; exit 1; }
command -v node >/dev/null 2>&1 || { echo "Missing required tool: node" >&2; exit 1; }

if [ ! -x "$HOME/.gemini/bin/agy" ] && ! command -v agy >/dev/null 2>&1; then
  echo "Antigravity CLI (agy) not found at $HOME/.gemini/bin/agy or in PATH." >&2
  exit 1
fi

if [ "$(uname -s)" = "Darwin" ]; then
  USER_DATA="$HOME/Library/Application Support/ropilot"
else
  USER_DATA="${XDG_CONFIG_HOME:-$HOME/.config}/ropilot"
fi

CLAUDE_DIR="$USER_DATA/claude"
BIN_DIR="$USER_DATA/bin"
mkdir -p "$CLAUDE_DIR" "$BIN_DIR"

pkill -f "Ropilot Studio" >/dev/null 2>&1 || true

if [ -f "$CLAUDE_DIR/claude" ] && [ ! -f "$CLAUDE_DIR/claude-real" ]; then
  cp "$CLAUDE_DIR/claude" "$CLAUDE_DIR/claude-real"
fi

echo "Building RopilotGemini bridge and MCP router..."
rustc -O -C debuginfo=0 -D warnings "$SCRIPT_DIR/src/bridge.rs" -o "$CLAUDE_DIR/claude"
rustc -O -C debuginfo=0 -D warnings "$SCRIPT_DIR/src/router.rs" -o "$BIN_DIR/ropilot-mcp-router"
chmod +x "$CLAUDE_DIR/claude" "$BIN_DIR/ropilot-mcp-router"

echo "Patching Ropilot Studio app.asar..."
node "$SCRIPT_DIR/patcher/patch_asar.mjs"

echo "RopilotGemini by xDTaraZ installed successfully."