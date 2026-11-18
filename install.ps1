$ErrorActionPreference = 'Stop'

function AssertCmd($name) {
  if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
    throw "Missing required tool: $name"
  }
}

AssertCmd 'rustc'
AssertCmd 'node'

$agy = Join-Path $env:USERPROFILE '.gemini\bin\agy.exe'
if (-not (Test-Path $agy) -and -not (Get-Command 'agy.exe' -ErrorAction SilentlyContinue)) {
  throw "Antigravity CLI (agy.exe) not found at $agy or in PATH."
}

$procs = Get-Process | Where-Object { $_.ProcessName -match '^(Ropilot Studio|ropilot-infra|ropilot-infra-helper|ropilot-mcp-router|claude|claude-real)$' }
if ($procs) {
  Write-Host 'Closing running Ropilot Studio processes...'
  $procs | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 600
}

$claudeDir = Join-Path $env:APPDATA 'ropilot\claude'
$binDir = Join-Path $env:APPDATA 'ropilot\bin'
New-Item -ItemType Directory -Force -Path $claudeDir, $binDir | Out-Null

$claudeExe = Join-Path $claudeDir 'claude.exe'
$claudeReal = Join-Path $claudeDir 'claude-real.exe'
if ((Test-Path $claudeExe) -and -not (Test-Path $claudeReal)) {
  Copy-Item $claudeExe $claudeReal -Force
}

Write-Host 'Building RopilotGemini bridge and MCP router...'
rustc -O -C debuginfo=0 -D warnings (Join-Path $PSScriptRoot 'src\bridge.rs') -o $claudeExe
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

rustc -O -C debuginfo=0 -D warnings (Join-Path $PSScriptRoot 'src\router.rs') -o (Join-Path $binDir 'ropilot-mcp-router.exe')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host 'Patching Ropilot Studio app.asar...'
node (Join-Path $PSScriptRoot 'patcher\patch_asar.mjs')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host 'RopilotGemini by xDTaraZ installed successfully.'