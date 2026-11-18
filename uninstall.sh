#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" = "Darwin" ]; then
  USER_DATA="$HOME/Library/Application Support/ropilot"
  ASAR_CANDIDATES=(
    "/Applications/Ropilot Studio.app/Contents/Resources/app.asar"
    "$HOME/Applications/Ropilot Studio.app/Contents/Resources/app.asar"
  )
else
  USER_DATA="${XDG_CONFIG_HOME:-$HOME/.config}/ropilot"
  ASAR_CANDIDATES=(
    "/opt/Ropilot Studio/resources/app.asar"
    "$HOME/.local/share/Ropilot Studio/resources/app.asar"
  )
fi

pkill -f "Ropilot Studio" >/dev/null 2>&1 || true

for ASAR in "${ASAR_CANDIDATES[@]}"; do
  if [ -f "$ASAR.bak" ]; then
    cp "$ASAR.bak" "$ASAR"
    echo "Restored $ASAR from backup."
  fi
done

if [ -f "$USER_DATA/claude/claude-real" ]; then
  cp "$USER_DATA/claude/claude-real" "$USER_DATA/claude/claude"
  chmod +x "$USER_DATA/claude/claude"
  echo "Restored original claude binary."
fi

echo "RopilotGemini uninstalled cleanly."