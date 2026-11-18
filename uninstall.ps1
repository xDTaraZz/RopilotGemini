$ErrorActionPreference = 'Stop'

$ropilotData = Join-Path $env:APPDATA 'ropilot'
$ropilotProg = Join-Path $env:LOCALAPPDATA 'Programs\Ropilot Studio'

Get-Process -ErrorAction SilentlyContinue | Where-Object {
  $_.ProcessName -eq 'Ropilot Studio' -or
  ($_.Path -and ($_.Path.StartsWith($ropilotData, 'OrdinalIgnoreCase') -or $_.Path.StartsWith($ropilotProg, 'OrdinalIgnoreCase')))
} | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$liveAsar = Join-Path $ropilotProg 'resources\app.asar'
$bakAsar = "$liveAsar.bak"
if (Test-Path $bakAsar) {
  Move-Item $bakAsar $liveAsar -Force
  Remove-Item "$liveAsar.bak.meta.json" -Force -ErrorAction SilentlyContinue
  Write-Host 'Restored app.asar from backup.'
}

$claudeDir = Join-Path $ropilotData 'claude'
$claudeExe = Join-Path $claudeDir 'claude.exe'
$claudeReal = Join-Path $claudeDir 'claude-real.exe'
if (Test-Path $claudeReal) {
  Move-Item $claudeReal $claudeExe -Force
  Write-Host 'Restored original claude.exe.'
} elseif (Test-Path $claudeExe) {
  Remove-Item $claudeExe -Force
  Write-Host 'Removed RopilotGemini claude.exe.'
}
Remove-Item (Join-Path $claudeDir 'claude.pdb') -Force -ErrorAction SilentlyContinue

$routerExe = Join-Path $ropilotData 'bin\ropilot-mcp-router.exe'
Remove-Item $routerExe, (Join-Path $ropilotData 'bin\ropilot-mcp-router.pdb') -Force -ErrorAction SilentlyContinue

$mcpBackup = Join-Path $ropilotData 'mcp-backup.json'
if (Test-Path $mcpBackup) {
  node -e "
    const fs = require('fs');
    const bkp = JSON.parse(fs.readFileSync(process.argv[1], 'utf8'));
    for (const [cfgPath, orig] of Object.entries(bkp)) {
      if (!fs.existsSync(cfgPath)) continue;
      const cfg = JSON.parse(fs.readFileSync(cfgPath, 'utf8'));
      if (cfg.mcpServers) { cfg.mcpServers.ropilot = orig; fs.writeFileSync(cfgPath, JSON.stringify(cfg, null, 2)); }
    }
  " $mcpBackup
  Remove-Item $mcpBackup -Force -ErrorAction SilentlyContinue
  Write-Host 'Restored Antigravity MCP config.'
}

Remove-Item (Join-Path $ropilotData 'agy-routes') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path $ropilotData 'agy-route-latest.json'), (Join-Path $ropilotData 'agy-session-mcp.txt'), (Join-Path $ropilotData 'agy-inputs') -Recurse -Force -ErrorAction SilentlyContinue

Write-Host 'RopilotGemini uninstalled cleanly.'