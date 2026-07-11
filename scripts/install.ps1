<#
.SYNOPSIS
    agentkit-Installer für Windows (PowerShell).

.DESCRIPTION
    Baut die agentkit-Executable lokal aus dem Quellcode und legt sie in den PATH.
    Zwei Varianten (siehe ..\INSTALL.md):
      - rust   : nativer Rust-Build via `cargo install` (klein, schnell, keine Runtime)
      - python : Python-Paket via pipx (oder pip), Console-Script `agentkit`

.PARAMETER Target
    Welche Variante: rust | python | both | auto (Default: auto).

.PARAMETER NoTui
    Rust ohne Terminal-UI bauen (schlanker, kein ratatui).

.EXAMPLE
    .\scripts\install.ps1 rust
    .\scripts\install.ps1 both
    .\scripts\install.ps1 rust -NoTui
#>
[CmdletBinding()]
param(
    [ValidateSet('rust', 'python', 'both', 'auto')]
    [string]$Target = 'auto',
    [switch]$NoTui
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RustDir  = Join-Path $RepoRoot 'agent_framework_rs'
$PyDir    = Join-Path $RepoRoot 'agent_framework'

function Write-Info($m) { Write-Host "» $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "✓ $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "! $m" -ForegroundColor Yellow }
function Have($cmd)     { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

function Install-Rust {
    if (-not (Have 'cargo')) {
        throw "cargo nicht gefunden. Rust installieren: https://rustup.rs"
    }
    if ($NoTui) {
        Write-Info "Baue Rust-Executable 'agentkit' ohne Terminal-UI (cargo install)…"
        cargo install --path $RustDir --bin agentkit --force
    } else {
        Write-Info "Baue Rust-Executable 'agentkit' mit Terminal-UI (cargo install)…"
        cargo install --path $RustDir --bin agentkit --features tui --force
    }
    Write-Ok "Rust-agentkit installiert (üblicherweise nach %USERPROFILE%\.cargo\bin\agentkit.exe)."
    Write-Warn2 "Liegt %USERPROFILE%\.cargo\bin im PATH? (rustup richtet das normalerweise ein)"
    Install-Completions
}

# PowerShell-Completion idempotent an $PROFILE anhängen (nur Rust-Build). Nie fatal.
function Install-Completions {
    $bin = Join-Path $env:USERPROFILE '.cargo\bin\agentkit.exe'
    if (-not (Test-Path $bin)) {
        if (Have 'agentkit') { $bin = 'agentkit' }
        else { Write-Warn2 'agentkit nicht auffindbar — PowerShell-Completion übersprungen.'; return }
    }
    $marker = '# agentkit completions (auto)'
    if ((Test-Path $PROFILE) -and (Select-String -Path $PROFILE -SimpleMatch $marker -Quiet)) {
        Write-Ok 'PowerShell-Completion bereits in $PROFILE eingerichtet.'
        return
    }
    try {
        $dir = Split-Path -Parent $PROFILE
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        Add-Content -Path $PROFILE -Value "`n$marker"
        & $bin completions powershell | Add-Content -Path $PROFILE
        Write-Ok "PowerShell-Completion an `$PROFILE angehängt: $PROFILE (neue Shell starten)."
    } catch {
        Write-Warn2 "Konnte PowerShell-Completion nicht anhängen: $($_.Exception.Message)"
        Write-Warn2 'Manuell:  agentkit completions powershell | Out-String | Invoke-Expression'
    }
}

function Install-Python {
    if (Have 'pipx') {
        Write-Info "Installiere Python-agentkit via pipx…"
        pipx install --force $PyDir
        Write-Ok "Python-agentkit via pipx installiert (Console-Script 'agentkit')."
    } elseif (Have 'pip') {
        Write-Warn2 "pipx nicht gefunden — nutze 'pip install --user'."
        pip install --user $PyDir
        Write-Ok "Python-agentkit via pip --user installiert (Console-Script 'agentkit')."
        Write-Warn2 "Liegt das Python-Scripts-Verzeichnis im PATH?"
    } else {
        throw "Weder pipx noch pip gefunden. Python 3.10+ mit pip installieren."
    }
}

if ($Target -eq 'auto') {
    if (Have 'cargo') { $Target = 'rust' }
    elseif (Have 'pipx' -or (Have 'pip')) { $Target = 'python' }
    else { throw "Weder cargo noch pip/pipx gefunden. Bitte Rust oder Python installieren." }
    Write-Info "Keine Variante angegeben — wähle automatisch: $Target"
}

switch ($Target) {
    'rust'   { Install-Rust }
    'python' { Install-Python }
    'both'   { Install-Rust; Install-Python }
}

Write-Ok 'Fertig. Test:  agentkit --demo "Was ist 17 + 25?"'
