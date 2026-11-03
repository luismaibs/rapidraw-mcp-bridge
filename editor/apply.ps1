<#
  Adds the MCP bridge to a RapidRAW source checkout.

    .\apply.ps1 -RapidRaw C:\src\RapidRAW          # apply
    .\apply.ps1 -RapidRaw C:\src\RapidRAW -Check   # only test that the patch applies

  The checkout must be a git repository at the commit the patch was made against (see the README) with no
  uncommitted changes in the files the patch edits. Everything done here is a plain file copy plus
  `git apply`, so `git diff` in the checkout shows exactly what changed and `git checkout .` undoes it.
#>
param(
    [Parameter(Mandatory)][string]$RapidRaw,
    [switch]$Check
)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path $RapidRaw).Path
if (-not (Test-Path (Join-Path $repo 'src-tauri\src\lib.rs'))) { throw "$repo does not look like a RapidRAW checkout." }
if (-not (Test-Path (Join-Path $repo '.git'))) { throw "$repo is not a git repository; the patch needs one." }

$patch = Join-Path $PSScriptRoot 'rapidraw-mcp.patch'
git -C $repo apply --check --ignore-whitespace $patch
if ($LASTEXITCODE -ne 0) {
    throw 'The patch does not apply. Use the RapidRAW commit named in the README, or adapt the patch by hand.'
}
if ($Check) { Write-Host 'The patch applies cleanly.'; return }

git -C $repo apply --ignore-whitespace $patch
if ($LASTEXITCODE -ne 0) { throw 'git apply failed.' }

$shared = Join-Path $repo 'src-tauri\src\mcp_shared'
New-Item -ItemType Directory -Force $shared | Out-Null
Copy-Item (Join-Path $PSScriptRoot '..\bridge\*.rs') $shared -Force
Copy-Item (Join-Path $PSScriptRoot 'src-tauri\src\mcp_bridge.rs') (Join-Path $repo 'src-tauri\src\mcp_bridge.rs') -Force
Copy-Item (Join-Path $PSScriptRoot 'src\hooks\*.ts') (Join-Path $repo 'src\hooks') -Force

Write-Host "Bridge added to $repo."
Write-Host 'Build it with:  npm install  and  npx tauri build --debug --no-bundle'
