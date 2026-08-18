<#
.SYNOPSIS
    agentkit-Installer für Windows (PowerShell).

.DESCRIPTION
    Baut die agentkit-Executable lokal aus dem Quellcode (nativer Rust-Build via
    `cargo install`) und legt sie in den PATH. Siehe ..\INSTALL.md.

.PARAMETER NoTui
    Ohne Terminal-UI bauen (schlanker, kein ratatui).

.EXAMPLE
    .\scripts\install.ps1
    .\scripts\install.ps1 -NoTui
#>
[CmdletBinding()]
param(
    [switch]$NoTui
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RustDir  = Join-Path $RepoRoot 'agentkit_app'

function Write-Info($m) { Write-Host "» $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "✓ $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "! $m" -ForegroundColor Yellow }
function Have($cmd)     { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

function Install-Rust {
    if (-not (Have 'cargo')) {
        throw "cargo nicht gefunden. Rust installieren: https://rustup.rs"
    }
    # Derselbe Feature-Satz, den auch der Release baut (.github/workflows/release.yml,
    # scripts/build.ps1): PDF, Context-Management, Wissensgraph, Arbeits-Runtime und
    # Betrachter sind in BEIDEN Varianten drin — alles reines Rust ohne C-Toolchain.
    # Nur das TUI ist optional, das ist der einzige Unterschied zwischen 'voll' und 'cli'.
    if ($NoTui) {
        Write-Info "Baue Rust-Executable 'agentkit' ohne Terminal-UI (cargo install)…"
        cargo install --path $RustDir --bin agentkit --features "pdf ctxman tiktoken graph work viz" --force
    } else {
        Write-Info "Baue Rust-Executable 'agentkit' mit Terminal-UI (cargo install)…"
        cargo install --path $RustDir --bin agentkit --features "tui pdf ctxman tiktoken graph work viz" --force
    }
    Write-Ok "agentkit installiert (üblicherweise nach %USERPROFILE%\.cargo\bin\agentkit.exe)."
    Write-Warn2 "Liegt %USERPROFILE%\.cargo\bin im PATH? (rustup richtet das normalerweise ein)"
    Install-Completions
}

# PowerShell-Completion idempotent an $PROFILE anhängen. Nie fatal.
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

Install-Rust

Write-Ok 'Fertig. Test:  agentkit --demo "Was ist 17 + 25?"'
