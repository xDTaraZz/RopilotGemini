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

pkill -x "Ropilot Studio" >/dev/null 2>&1 || true
pkill -x "ropilot-infra" >/dev/null 2>&1 || true
sleep 0.6

for ASAR in "${ASAR_CANDIDATES[@]}"; do
  if [ -f "$ASAR.bak" ]; then
    mv -f "$ASAR.bak" "$ASAR"
    rm -f "$ASAR.bak.meta.json"
    echo "Restored $ASAR from backup."
  fi
done

if [ -f "$USER_DATA/claude/claude-real" ]; then
  mv -f "$USER_DATA/claude/claude-real" "$USER_DATA/claude/claude"
  chmod +x "$USER_DATA/claude/claude"
  echo "Restored original claude binary."
elif [ -f "$USER_DATA/claude/claude" ]; then
  rm -f "$USER_DATA/claude/claude"
  echo "Removed RopilotGemini claude binary."
fi

rm -f "$USER_DATA/bin/ropilot-mcp-router"

if [ -f "$USER_DATA/mcp-backup.json" ] && command -v node >/dev/null 2>&1; then
  node -e '
    const fs = require("fs");
    const bkp = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    for (const [cfgPath, orig] of Object.entries(bkp)) {
      if (!fs.existsSync(cfgPath)) continue;
      const cfg = JSON.parse(fs.readFileSync(cfgPath, "utf8"));
      if (cfg.mcpServers) { cfg.mcpServers.ropilot = orig; fs.writeFileSync(cfgPath, JSON.stringify(cfg, null, 2)); }
    }
  ' "$USER_DATA/mcp-backup.json"
  rm -f "$USER_DATA/mcp-backup.json"
  echo "Restored Antigravity MCP config."
fi

rm -rf "$USER_DATA/agy-routes" "$USER_DATA/agy-inputs"
rm -f "$USER_DATA/agy-route-latest.json" "$USER_DATA/agy-session-mcp.txt"

echo "RopilotGemini uninstalled cleanly."