<#
.SYNOPSIS
    Launch NewCormanLisp in GUI mode, optionally running a demo.

.DESCRIPTION
    Starts ncl with the iGui frame open. Without arguments, drops into
    the REPL with the frame ready for (open-child …) / (open-text-window …)
    calls. With -Demo, loads one of Lisp/demos/<demo>.lisp and calls its
    (run-<demo>) entrypoint.

    The script auto-detects ncl.exe via the same search ladder the driver
    uses for Library/:
      1. $env:NCL_EXE (override)
      2. <script-dir>/../target/release/ncl.exe  (typical dev layout)
      3. ncl.exe on PATH

.PARAMETER Demo
    Name of a demo in Lisp/demos/. The script appends ".lisp" and calls
    (run-<demo>). Available demos:
      draw-square    static scene: rectangle, circle, line, caption
      paint-and-log  click-paints-dots + log pane
      click-counter  square that cycles colour on each click
      bouncing       physics-driven bouncing ball
      hello-igui     simple panel with shapes
      shapes         exercise all primitive drawing ops
      buttons        clickable buttons
      gui-repl       in-frame REPL (text-view based)
      native-repl    native graphical REPL (expeditor-style, Rust-implemented)
      heap-monitor   live GC bars (worker thread polls (gc-stats))

.PARAMETER Lean
    Start without the auto-loaded Library (--lean). CLOS / events /
    sequences / etc. are unavailable; useful for measuring startup
    cost or sandboxing.

.PARAMETER Eval
    Extra Lisp source to evaluate before the demo's run-* call.
    Multiple --eval flags can be passed by repeating -Eval.

.EXAMPLE
    PS> ./tools/Start-Gui.ps1
    Drops into the REPL with iGui ready. Type (open-child "foo") manually.

.EXAMPLE
    PS> ./tools/Start-Gui.ps1 -Demo paint-and-log
    Opens canvas + log windows, ready for clicks.

.EXAMPLE
    PS> ./tools/Start-Gui.ps1 -Demo bouncing -Eval "(setq *ball-w* 60)"
    Starts the bouncing demo with a 60-pixel-wide ball.
#>

[CmdletBinding()]
param(
    [string]$Demo = '',
    [switch]$Lean,
    [string[]]$Eval = @()
)

# ─── Locate ncl.exe ────────────────────────────────────────────────────
function Find-NclExe {
    if ($env:NCL_EXE -and (Test-Path $env:NCL_EXE)) {
        return $env:NCL_EXE
    }
    $scriptDir = $PSScriptRoot
    $candidate = Join-Path $scriptDir '..\target\release\ncl.exe'
    if (Test-Path $candidate) {
        return (Resolve-Path $candidate).Path
    }
    $onPath = Get-Command ncl.exe -ErrorAction SilentlyContinue
    if ($onPath) {
        return $onPath.Source
    }
    throw "ncl.exe not found. Set `$env:NCL_EXE, build the dev tree, or put it on PATH."
}

# ─── Resolve demo path ────────────────────────────────────────────────
function Resolve-DemoPath([string]$name) {
    if (-not $name) { return $null }
    $scriptDir = $PSScriptRoot
    $repoRoot = (Resolve-Path (Join-Path $scriptDir '..')).Path
    $candidate = Join-Path $repoRoot "Lisp\demos\$name.lisp"
    if (-not (Test-Path $candidate)) {
        $available = Get-ChildItem -Path (Join-Path $repoRoot 'Lisp\demos') -Filter '*.lisp' |
            Where-Object { $_.BaseName -ne 'clos-tour' } |
            ForEach-Object { $_.BaseName } |
            Sort-Object
        throw "Demo '$name' not found at $candidate.`nAvailable demos:`n  $($available -join "`n  ")"
    }
    return $candidate
}

# ─── Build argument list ──────────────────────────────────────────────
$nclExe = Find-NclExe
$args = @()

if ($Lean) {
    $args += '--lean'
}

# --windows enables the iGui surface before Library loading, which lets
# the startup splash bar run.  The iGui frame is started lazily inside
# splash::begin() via ensure_igui_started(), so (igui-start) is a no-op
# by the time demo code or the REPL runs.
$args += '--windows'

# User-supplied -Eval forms go in next. PowerShell strips embedded
# double-quotes when passing to native processes, so wrap each form
# in a brief `--%`-equivalent shape: we use the `%`-escaping trick
# by passing through the registered ProcessStartInfo. Simpler in
# practice: if your Lisp form has quotes, save it to a file and
# use a --load flag, or pass `--% (form …)` raw via the script's
# remaining-args ($args after this script ends). Documented in
# .EXAMPLE above.
foreach ($form in $Eval) {
    $args += @('--eval', $form)
}

# Demo (if any).
$demoPath = $null
if ($Demo) {
    $demoPath = Resolve-DemoPath $Demo
    # Normalise backslashes for the load-string literal.
    $loadPath = $demoPath -replace '\\', '/'
    $args += @('--load', $demoPath)
    $args += @('--eval', "(run-$Demo)")
}

# Drop into REPL after the demo's event-loop returns.
$args += '--repl'

Write-Host "[Start-Gui] ncl.exe = $nclExe"
if ($Demo) {
    Write-Host "[Start-Gui] demo    = $demoPath"
}
Write-Host "[Start-Gui] args    = $($args -join ' ')"
Write-Host ''

& $nclExe @args
