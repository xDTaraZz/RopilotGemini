$ErrorActionPreference = 'Stop'

function AssertCmd($name) {
  if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
    throw "Missing required tool: $name"
  }
}

AssertCmd 'rustc'
AssertCmd 'node'
AssertCmd 'npm'

$agy = Join-Path $env:USERPROFILE '.gemini\bin\agy.exe'
if (-not (Test-Path $agy) -and -not (Get-Command 'agy.exe' -ErrorAction SilentlyContinue)) {
  throw "Antigravity CLI (agy.exe) not found at $agy or in PATH."
}

$nodeModules = Join-Path $PSScriptRoot 'node_modules\@electron\asar'
if (-not (Test-Path $nodeModules)) {
  Write-Host 'Installing @electron/asar...'
  Push-Location $PSScriptRoot
  try {
    & npm.cmd install --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
  } finally {
    Pop-Location
  }
}

$stageDir = Join-Path $env:TEMP ("ropilot-gemini-stage-$PID")
New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

try {
  Write-Host 'Building RopilotGemini bridge and MCP router...'
  $stagedBridge = Join-Path $stageDir 'claude.exe'
  $stagedRouter = Join-Path $stageDir 'ropilot-mcp-router.exe'
  rustc --edition 2021 -O -C debuginfo=0 (Join-Path $PSScriptRoot 'src\bridge.rs') -o $stagedBridge
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
  rustc --edition 2021 -O -C debuginfo=0 (Join-Path $PSScriptRoot 'src\router.rs') -o $stagedRouter
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

  $ropilotData = Join-Path $env:APPDATA 'ropilot'
  $ropilotProg = Join-Path $env:LOCALAPPDATA 'Programs\Ropilot Studio'
  Get-Process -ErrorAction SilentlyContinue | Where-Object {
    $_.ProcessName -eq 'Ropilot Studio' -or
    ($_.Path -and ($_.Path.StartsWith($ropilotData, 'OrdinalIgnoreCase') -or $_.Path.StartsWith($ropilotProg, 'OrdinalIgnoreCase')))
  } | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 600

  Write-Host 'Patching Ropilot Studio app.asar...'
  & node (Join-Path $PSScriptRoot 'patcher\patch_asar.mjs')
  if ($LASTEXITCODE -ne 0) { throw 'app.asar patching failed; aborting binary swap.' }

  $claudeDir = Join-Path $ropilotData 'claude'
  $binDir = Join-Path $ropilotData 'bin'
  New-Item -ItemType Directory -Force -Path $claudeDir, $binDir | Out-Null

  $claudeExe = Join-Path $claudeDir 'claude.exe'
  $claudeReal = Join-Path $claudeDir 'claude-real.exe'
  if ((Test-Path $claudeExe) -and -not (Test-Path $claudeReal)) {
    Copy-Item $claudeExe $claudeReal -Force
  }

  Copy-Item $stagedBridge $claudeExe -Force
  Copy-Item $stagedRouter (Join-Path $binDir 'ropilot-mcp-router.exe') -Force
  Write-Host 'RopilotGemini by xDTaraZ installed successfully.'
} finally {
  Remove-Item -Recurse -Force $stageDir -ErrorAction SilentlyContinue
}