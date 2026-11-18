$ErrorActionPreference = 'Stop'

$procs = Get-Process | Where-Object { $_.ProcessName -match '^(Ropilot Studio|ropilot-infra|ropilot-infra-helper|ropilot-mcp-router|claude|claude-real)$' }
if ($procs) {
  $procs | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 600
}

$liveAsar = Join-Path $env:LOCALAPPDATA 'Programs\Ropilot Studio\resources\app.asar'
$bakAsar = "$liveAsar.bak"
if (Test-Path $bakAsar) {
  Copy-Item $bakAsar $liveAsar -Force
  Write-Host 'Restored app.asar from backup.'
}

$claudeDir = Join-Path $env:APPDATA 'ropilot\claude'
$claudeExe = Join-Path $claudeDir 'claude.exe'
$claudeReal = Join-Path $claudeDir 'claude-real.exe'
if (Test-Path $claudeReal) {
  Copy-Item $claudeReal $claudeExe -Force
  Write-Host 'Restored original claude.exe.'
}

Write-Host 'RopilotGemini uninstalled cleanly.'