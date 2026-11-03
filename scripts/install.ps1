<#
  Installs "MCP RapidRAW" for the current user.

  - Copies the MCP-enabled RapidRAW build and the MCP server to %LOCALAPPDATA%\Programs\MCP RapidRAW.
  - Creates %APPDATA%\RapidRAW-MCP\config.json if needed and registers the server in the agent clients
    that are installed (Claude Code, Codex, OpenCode).
  - Adds "MCP RapidRAW" shortcuts to the Desktop and the Start menu.

  Run it again after a rebuild to update the installed copy. Close MCP RapidRAW first.

  Example:
    .\install.ps1 -BuildDir C:\src\RapidRAW\src-tauri\target\debug -Roots D:\Photos\Copies
#>
param(
    # Folder with the built RapidRAW.exe and its resources folder.
    [Parameter(Mandatory)][string]$BuildDir,
    [string]$McpBuild = (Join-Path $PSScriptRoot '..\server\target\debug\rapidraw-mcp.exe'),
    [string[]]$Roots = @((Join-Path $env:USERPROFILE 'Pictures\RapidRAW-MCP'))
)
$ErrorActionPreference = 'Stop'

$app = Join-Path $BuildDir 'RapidRAW.exe'
foreach ($needed in @($app, $McpBuild, (Join-Path $BuildDir 'resources'))) {
    if (-not (Test-Path $needed)) { throw "Missing build output: $needed" }
}

$install = Join-Path $env:LOCALAPPDATA 'Programs\MCP RapidRAW'
New-Item -ItemType Directory -Force $install | Out-Null
$exe = Join-Path $install 'MCP RapidRAW.exe'
$running = Get-Process | Where-Object { $_.Path -eq $exe }
if ($running) { throw 'MCP RapidRAW is running; close it and run the installer again.' }

Copy-Item $app $exe -Force
Copy-Item $McpBuild (Join-Path $install 'rapidraw-mcp.exe') -Force
foreach ($dll in 'DirectML.dll', 'onnxruntime.dll') {
    $source = @((Join-Path $BuildDir $dll), (Join-Path $BuildDir "resources\$dll")) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($source) { Copy-Item $source (Join-Path $install $dll) -Force }
}
foreach ($folder in 'resources', 'lensfun_db') {
    $from = Join-Path $BuildDir $folder
    if (Test-Path $from) { robocopy $from (Join-Path $install $folder) /MIR /NFL /NDL /NJH /NJS /NP | Out-Null }
}

& (Join-Path $PSScriptRoot 'setup.ps1') -Roots $Roots -McpExe (Join-Path $install 'rapidraw-mcp.exe') -Register

$shell = New-Object -ComObject WScript.Shell
$places = @(
    [Environment]::GetFolderPath('Desktop'),
    (Join-Path ([Environment]::GetFolderPath('Programs')) '')
)
foreach ($place in $places) {
    $link = $shell.CreateShortcut((Join-Path $place 'MCP RapidRAW.lnk'))
    $link.TargetPath = $exe
    $link.WorkingDirectory = $install
    $link.IconLocation = "$exe,0"
    $link.Description = 'RapidRAW with the MCP bridge for AI agents'
    $link.Save()
    Write-Host "Shortcut: $(Join-Path $place 'MCP RapidRAW.lnk')"
}
Write-Host "`nInstalled in $install"
