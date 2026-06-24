<#
.SYNOPSIS
    Render a single Markdown file with DocCrate and capture a PNG snapshot.

.DESCRIPTION
    Drives `doc-crate.exe --testsnap <file>`, which renders the file and writes
    `screen.png` into the parent of the file's directory once layout completes.
    The GUI process does NOT self-exit after snapping, so this script launches it
    in the background, waits for the PNG to appear and settle, kills the process,
    and copies the result to tools/doccrate/snaps/<name>.png for inspection.

    Use this to verify a document you authored renders correctly — especially
    Mermaid diagrams, which degrade to a visible "mermaid error:" line on failure.

.EXAMPLE
    .\Test-Render.ps1 -File ..\..\docs\manual\compiler\reader.md
.EXAMPLE
    .\Test-Render.ps1 -File ..\..\docs\manual\index.md -ScrollTo 120
#>
param(
    [Parameter(Mandatory = $true)][string]$File,
    [double]$Scroll = 0,
    [int]$ScrollTo = 0,
    [int]$TimeoutSec = 25
)

$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot 'doc-crate.exe'
if (-not (Test-Path $exe)) { throw "doc-crate.exe not found at $exe" }
if (-not (Test-Path $File)) { throw "markdown file not found: $File" }

$file    = (Resolve-Path $File).Path
$docsDir = Split-Path $file -Parent       # mirrors main.rs: docs_dir = file.parent()
$snapDir = Split-Path $docsDir -Parent    # screenshot_path = docs_dir.parent()/screen.png
$png     = Join-Path $snapDir 'screen.png'

if (Test-Path $png) { Remove-Item $png -Force }

$argList = @('--testsnap', $file)
if ($Scroll   -gt 0) { $argList += @('--scroll',   $Scroll) }
if ($ScrollTo -gt 0) { $argList += @('--scrollto', $ScrollTo) }

$proc = Start-Process -FilePath $exe -ArgumentList $argList -PassThru

$deadline   = (Get-Date).AddSeconds($TimeoutSec)
$lastSize   = -1
$stable     = 0
$ok         = $false
while ((Get-Date) -lt $deadline) {
    if (Test-Path $png) {
        $sz = (Get-Item $png).Length
        if ($sz -gt 0 -and $sz -eq $lastSize) { $stable++ } else { $stable = 0 }
        $lastSize = $sz
        if ($stable -ge 2) { $ok = $true; break }
    }
    Start-Sleep -Milliseconds 250
}

try { if (-not $proc.HasExited) { $proc.Kill() } } catch {}
Get-Process doc-crate -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

if (-not $ok) { throw "snapshot not produced within $TimeoutSec s (expected $png)" }

$snaps = Join-Path $PSScriptRoot 'snaps'
New-Item -ItemType Directory -Force -Path $snaps | Out-Null
$name = [IO.Path]::GetFileNameWithoutExtension($file)
$out  = Join-Path $snaps "$name.png"
Copy-Item $png $out -Force
Remove-Item $png -Force -ErrorAction SilentlyContinue

Write-Output $out
