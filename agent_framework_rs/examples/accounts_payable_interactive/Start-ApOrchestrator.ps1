<#
.SYNOPSIS
    Startet den interaktiven Accounts-Payable-Orchestrator (Human-in-the-Loop) im agentkit-TUI.

.DESCRIPTION
    Frau Berger, die „Leiterin der Buchhaltung", bearbeitet Eingangsrechnungen, delegiert an
    Fach-Agenten (extractor/validator/booker), fragt bei Unklarheiten nach (ask_user) und baut
    dabei einen Company Knowledge Graph im OKF-Format (knowledge/) auf.

    Der Wissensgraph und die Inbox werden in einen Arbeitsordner (Default .\workspace) kopiert,
    damit der Seed sauber bleibt und die gelernten Einträge dort dauerhaft wachsen.

.PARAMETER WorkspaceDir  Arbeitsordner (Default: .\workspace).
.PARAMETER Fresh         Arbeitsordner neu aus dem Seed aufsetzen (verwirft Gelerntes).
.PARAMETER Repl          Statt TUI den (scriptbaren) REPL starten.
.PARAMETER Provider      LLM-Provider: auto | azure | openai (Default: auto).
.PARAMETER EnvFile       Optionale .env (sonst Auto-Suche).
.PARAMETER AgentkitPath  Pfad zur agentkit-Executable (sonst Auto-Suche im Repo/PATH).

.EXAMPLE
    .\Start-ApOrchestrator.ps1
    .\Start-ApOrchestrator.ps1 -Fresh
#>
[CmdletBinding()]
param(
    [string]$WorkspaceDir,
    [switch]$Fresh,
    [switch]$Repl,
    [ValidateSet('auto', 'azure', 'openai')] [string]$Provider = 'auto',
    [string]$EnvFile,
    [string]$AgentkitPath
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoDir = Split-Path -Parent (Split-Path -Parent $here)
if (-not $WorkspaceDir) { $WorkspaceDir = Join-Path $here 'workspace' }

# .env laden (LLM-Credentials in die Prozessumgebung ziehen).
. (Join-Path (Split-Path -Parent $here) 'accounts_payable\modules\ap-helpers.ps1')
$envCandidates = @()
if ($EnvFile) { $envCandidates += $EnvFile }
$envCandidates += (Join-Path $here '.env'); $envCandidates += (Join-Path $repoDir '.env')
foreach ($c in $envCandidates) { if (Import-DotEnv $c) { Write-Host "  .env geladen: $c" -ForegroundColor DarkGray; break } }

function Resolve-Agentkit {
    param([string]$Explicit)
    if ($Explicit) { if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path } else { throw "agentkit nicht gefunden: $Explicit" } }
    foreach ($rel in @('target\release\agentkit.exe', 'target\debug\agentkit.exe')) {
        $p = Join-Path $repoDir $rel; if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command agentkit -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "Keine agentkit-Executable gefunden. Baue: cargo build --release --features `"tui pdf`" (in agent_framework_rs)."
}
$ak = Resolve-Agentkit -Explicit $AgentkitPath

# Arbeitsordner aus dem Seed aufsetzen (Wissensgraph + Inbox), Seed bleibt unberührt.
if ($Fresh -and (Test-Path $WorkspaceDir)) {
    Get-ChildItem $WorkspaceDir -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { $_.IsReadOnly = $false }
    Remove-Item $WorkspaceDir -Recurse -Force
}
if (-not (Test-Path (Join-Path $WorkspaceDir 'knowledge'))) {
    New-Item -ItemType Directory -Force -Path $WorkspaceDir | Out-Null
    Copy-Item (Join-Path $here 'knowledge') -Destination $WorkspaceDir -Recurse -Force
    Copy-Item (Join-Path $here 'inbox') -Destination $WorkspaceDir -Recurse -Force
    Write-Host "Arbeitsordner aus Seed aufgesetzt: $WorkspaceDir" -ForegroundColor Green
}

$roles = Join-Path $here 'roles'
$orch = Join-Path $here 'orchestrator.md'
$common = @('--provider', $Provider, '-w', $WorkspaceDir, '--agents', $roles, '--system-file', $orch)

Write-Host "agentkit:  $ak"
Write-Host "Workspace: $WorkspaceDir  (Wissensgraph: knowledge/, Rechnungen: inbox/)"
Write-Host "Tipp: 'Verarbeite die Rechnung inbox/rechnung_meier.txt' — der Orchestrator fragt bei Bedarf nach.`n"

if ($Repl) {
    & $ak --repl @common
}
else {
    & $ak --tui @common
}
