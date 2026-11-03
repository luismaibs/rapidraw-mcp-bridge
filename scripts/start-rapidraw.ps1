<#
  Starts an MCP-enabled RapidRAW build. It reads %APPDATA%\RapidRAW-MCP\config.json by itself, so it
  needs no environment variables. Close the regular RapidRAW first: only the MCP-enabled build has the bridge.
#>
param([Parameter(Mandatory)][string]$Exe)
if (-not (Test-Path $Exe)) { throw "RapidRAW build not found: $Exe" }
if (-not (Test-Path (Join-Path $env:APPDATA 'RapidRAW-MCP\config.json'))) { throw 'Run setup.ps1 first.' }
Start-Process $Exe -WorkingDirectory (Split-Path $Exe)
Write-Host "Started $Exe"
