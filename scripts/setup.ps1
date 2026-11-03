<#
  One-time setup for the connector.

  - Creates %APPDATA%\RapidRAW-MCP\config.json with a private random secret and the folders the agent
    may use. RapidRAW and the MCP server both read it, so no client needs environment variables or
    secrets in its configuration.
  - With -Register, adds the server to Claude Code, Codex and OpenCode when they are installed.

  Examples:
    .\setup.ps1 -Roots 'C:\Users\me\Pictures\RapidRAW-MCP'
    .\setup.ps1 -Roots 'D:\Weddings\Copies' -Register
#>
param(
    [string[]]$Roots = @(),
    [string]$McpExe = (Join-Path $PSScriptRoot '..\server\target\debug\rapidraw-mcp.exe'),
    [switch]$Register
)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $env:APPDATA 'RapidRAW-MCP'
New-Item -ItemType Directory -Force $dir | Out-Null
$file = Join-Path $dir 'config.json'
$config = if (Test-Path $file) { Get-Content $file -Raw | ConvertFrom-Json } else { [pscustomobject]@{} }

if (-not $config.secret -or $config.secret.Length -lt 32) {
    $bytes = New-Object byte[] 32
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $config | Add-Member -Force -MemberType NoteProperty -Name secret -Value (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
}

$existing = @($config.roots | Where-Object { $_ })
$wanted = @()
foreach ($root in $Roots) {
    New-Item -ItemType Directory -Force $root | Out-Null
    $wanted += (Resolve-Path $root).Path
}
$all = @($existing + $wanted | Select-Object -Unique)
$config | Add-Member -Force -MemberType NoteProperty -Name roots -Value ([string[]]$all)

[IO.File]::WriteAllText($file, ($config | ConvertTo-Json), (New-Object Text.UTF8Encoding($false)))
icacls $file /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null
Write-Host "Config: $file"
Write-Host ("Allowed folders: " + ($all -join '; '))
if ($all.Count -eq 0) { Write-Warning 'No folders yet: rerun with -Roots <folder> so the bridge can start.' }

if ($Register) {
    if (-not (Test-Path $McpExe)) { throw "MCP server not found: $McpExe (build it with 'cargo build' in server/)" }
    $McpExe = (Resolve-Path $McpExe).Path

    # Claude Code, user scope
    $claude = (Get-Command claude -ErrorAction SilentlyContinue).Source
    if ($claude) {
        $ErrorActionPreference = 'Continue'   # "remove" fails harmlessly when nothing is registered yet
        & $claude mcp remove rapidraw --scope user 2>&1 | Out-Null
        & $claude mcp add rapidraw --scope user -- $McpExe
        $ErrorActionPreference = 'Stop'
    } else { Write-Warning 'claude CLI not found; skipped Claude Code' }

    # Codex: AI masks and exports can take minutes, so raise the tool timeout
    $codex = Join-Path $env:USERPROFILE '.codex\config.toml'
    if (Test-Path $codex) {
        $text = Get-Content $codex -Raw
        $text = [regex]::Replace($text, '(?ms)^\[mcp_servers\.rapidraw\].*?(?=^\[|\z)', '')
        $block = "[mcp_servers.rapidraw]`ncommand = '$McpExe'`nstartup_timeout_sec = 30`ntool_timeout_sec = 900`n`n"
        [IO.File]::WriteAllText($codex, $text.TrimEnd() + "`n`n" + $block, (New-Object Text.UTF8Encoding($false)))
        Write-Host "Codex: updated $codex"
    } else { Write-Warning 'No ~/.codex/config.toml; skipped Codex' }

    # OpenCode, global config
    $oc = Join-Path $env:USERPROFILE '.config\opencode\opencode.json'
    New-Item -ItemType Directory -Force (Split-Path $oc) | Out-Null
    $json = if (Test-Path $oc) { Get-Content $oc -Raw | ConvertFrom-Json } else { [pscustomobject]@{ '$schema' = 'https://opencode.ai/config.json' } }
    if (-not $json.mcp) { $json | Add-Member -MemberType NoteProperty -Name mcp -Value ([pscustomobject]@{}) }
    $json.mcp | Add-Member -Force -MemberType NoteProperty -Name rapidraw -Value ([pscustomobject]@{ type = 'local'; command = @($McpExe); enabled = $true; timeout = 900000 })
    [IO.File]::WriteAllText($oc, ($json | ConvertTo-Json -Depth 10), (New-Object Text.UTF8Encoding($false)))
    Write-Host "OpenCode: updated $oc"

    # DeepSeek Harness: a plugin entry in ~/.dsh/profiles/desktop/cordis.patch.yml. Its default tool
    # timeout is 60 s, too short for AI masks and exports, hence toolCallTimeoutMs.
    $dsh = Join-Path $env:USERPROFILE '.dsh\profiles\desktop\cordis.patch.yml'
    if (Test-Path $dsh) {
        # Drop any previous rapidraw entry (its indented body included), keep everything else
        $kept = @(); $skip = $false
        foreach ($line in (Get-Content $dsh)) {
            if ($line -match '^- id: mcp-rapidraw\s*$') { $skip = $true; continue }
            if ($skip -and $line -match '^\s+\S') { continue }
            $skip = $false
            $kept += $line
        }
        $yaml = ($kept -join "`n")
        $entry = "- id: mcp-rapidraw`n  name: '@deepseek-ai/dsh-mcp-client'`n  config:`n    serverName: rapidraw`n    transport: stdio`n    command: '$McpExe'`n    toolCallTimeoutMs: 900000`n"
        [IO.File]::WriteAllText($dsh, $yaml.TrimEnd() + "`n" + $entry, (New-Object Text.UTF8Encoding($false)))
        Write-Host "DeepSeek Harness: updated $dsh (restart the app to load it)"
    } else {
        Write-Host 'DeepSeek Harness profile not found; skipped.'
    }
}

Write-Host "`nStart the MCP-enabled RapidRAW with: .\start-rapidraw.ps1 -Exe <path to RapidRAW.exe>"
Write-Host "Any other stdio MCP client: command = $McpExe (no arguments, no environment variables)"
