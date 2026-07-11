<#
.SYNOPSIS
    Accounts-Payable-Pipeline für deutsche Kleinunternehmer/Freelancer — komponiert aus
    einzelnen agentkit-Agenten (ein Agent pro Schritt) und PowerShell.

.DESCRIPTION
    Für jede PDF-Rechnung im Inbox-Ordner entsteht ein eigener Ergebnis-Ordner mit ALLEN
    Zwischenergebnissen. Die Schritte sind bewusst als eigenständige, komponierbare
    Kommandos verdrahtet (Unix-Prinzip „ein Werkzeug, eine Aufgabe“):

        00  Ingest      agentkit read-pdf  (deterministisch, kein LLM)   -> 01_text.txt
        02  Extraktion  agentkit -p --format json --system-file 02_...   -> 02_fields.json
        03  Validierung agentkit -p --format json --system-file 03_...   -> 03_validation.json
        04  Buchung     agentkit -p --format json --system-file 04_...   -> 04_booking.json
        05  Report      agentkit -p            --system-file 05_...       -> 05_report.md

    Jede LLM-Stufe ist ein spezialisierter Agent, konfiguriert allein über seinen
    System-Prompt (--system-file). Datenfluss über stdin/stdout — reine Komposition.

    Es werden AUSSCHLIESSLICH PowerShell- und agentkit-Kommandos verwendet.

.PARAMETER InboxDir
    Ordner mit den Eingangs-PDFs (Default: .\inbox neben diesem Skript).

.PARAMETER OutDir
    Zielordner; je Rechnung entsteht ein Unterordner (Default: .\out).

.PARAMETER Provider
    LLM-Provider für die Denk-Stufen: auto | azure | openai (Default: auto —
    wählt Azure, wenn AZURE_OPENAI_* gesetzt/in .env, sonst OpenAI).

.PARAMETER Model
    Optionales OpenAI-Modell (setzt OPENAI_MODEL; bei Azure zählt das Deployment).

.PARAMETER EnvFile
    Optionale .env mit den LLM-Credentials. Ohne Angabe wird der Reihe nach gesucht:
    .\.env (neben dem Skript), dann ..\..\.env (agent_framework_rs\.env).

.PARAMETER AgentkitPath
    Pfad zur agentkit-Executable. Ohne Angabe wird das Release-/Debug-Binary im Repo
    gesucht, sonst `agentkit` aus dem PATH.

.EXAMPLE
    .\Invoke-ApPipeline.ps1
    .\Invoke-ApPipeline.ps1 -Provider azure
#>
[CmdletBinding()]
param(
    [string]$InboxDir,
    [string]$OutDir,
    [ValidateSet('auto', 'azure', 'openai')] [string]$Provider = 'auto',
    [string]$Model,
    [string]$EnvFile,
    [string]$AgentkitPath
)

$ErrorActionPreference = 'Stop'
# UTF-8 für native Kommando-Ein-/Ausgabe (Umlaute/€ korrekt durch die Pipe).
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
# Native stderr (die Agenten-Spur) soll NICHT als Fehler abbrechen — wir prüfen $LASTEXITCODE.
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -Scope Global -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$prompts = Join-Path $here 'prompts'
if (-not $InboxDir) { $InboxDir = Join-Path $here 'inbox' }
if (-not $OutDir)   { $OutDir   = Join-Path $here 'out' }
if ($Model) { $env:OPENAI_MODEL = $Model }

# --- LLM-Credentials laden ------------------------------------------------------------
# agentkit liest AZURE_OPENAI_*/OPENAI_* aus der Umgebung. Damit die Pipeline unabhängig
# vom Arbeitsverzeichnis funktioniert, ziehen wir eine .env selbst in die Prozess-Umgebung
# (Werte aus der Datei haben Vorrang — sie sind die gepflegten Credentials).
function Import-DotEnv([string]$Path) {
    if (-not (Test-Path $Path)) { return $false }
    foreach ($line in Get-Content -Path $Path) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('#')) { continue }
        $kv = $t -split '=', 2
        if ($kv.Count -ne 2) { continue }
        $k = $kv[0].Trim()
        $v = $kv[1].Trim().Trim('"').Trim("'")
        Set-Item -Path "Env:$k" -Value $v
    }
    Write-Host "  .env geladen: $Path" -ForegroundColor DarkGray
    return $true
}
$repoDir  = Split-Path -Parent (Split-Path -Parent $here)   # ...\agent_framework_rs
$envCandidates = @()
if ($EnvFile) { $envCandidates += $EnvFile }
$envCandidates += (Join-Path $here '.env')
$envCandidates += (Join-Path $repoDir '.env')
foreach ($cand in $envCandidates) { if (Import-DotEnv $cand) { break } }

# --- agentkit-Executable auflösen -----------------------------------------------------
function Resolve-Agentkit {
    param([string]$Explicit)
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw "agentkit nicht gefunden unter: $Explicit"
    }
    $repo = Split-Path -Parent (Split-Path -Parent $here)   # ...\agent_framework_rs
    foreach ($rel in @('target\release\agentkit.exe', 'target\debug\agentkit.exe')) {
        $p = Join-Path $repo $rel
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command agentkit -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "Keine agentkit-Executable gefunden. Baue mit: cargo build --release --features pdf  (im Ordner agent_framework_rs) oder gib -AgentkitPath an."
}
$ak = Resolve-Agentkit -Explicit $AgentkitPath

# --- Hilfsfunktionen ------------------------------------------------------------------
function Write-Head($m)  { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Write-Step($m)  { Write-Host "  → $m" -ForegroundColor DarkGray }
function Write-Okay($m)  { Write-Host "  ✓ $m" -ForegroundColor Green }
function Write-Fail($m)  { Write-Host "  ✗ $m" -ForegroundColor Red }

# Führt EINE agentkit-Stufe aus: pipet $StdinText hinein, schreibt stdout nach $OutFile
# (UTF-8). Gibt $true bei Exit 0 zurück. Die Agenten-Spur läuft live auf stderr mit.
function Invoke-Stage {
    param(
        [string]$StdinText,
        [string[]]$AkArgs,
        [string]$OutFile,
        [string]$Label
    )
    Write-Step $Label
    $out = $StdinText | & $ak @AkArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Fail "$Label fehlgeschlagen (Exit $LASTEXITCODE)."
        return $false
    }
    # $out ist ein String-Array (Zeilen) -> als UTF-8-Datei speichern.
    Set-Content -Path $OutFile -Value $out -Encoding utf8
    Write-Okay ("{0}  ({1})" -f $Label, (Split-Path -Leaf $OutFile))
    return $true
}

# --- Pipeline -------------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$pdfs = @(Get-ChildItem -Path $InboxDir -Filter '*.pdf' -File -ErrorAction SilentlyContinue)
if ($pdfs.Count -eq 0) {
    Write-Warning "Keine PDFs in $InboxDir. Erst Beispiele erzeugen: .\tools\Build-Samples.ps1"
    return
}

Write-Host "agentkit: $ak"
Write-Host "Provider: $Provider$(if ($Model) { " ($Model)" })"
Write-Host "Inbox:    $InboxDir  ($($pdfs.Count) Rechnung(en))"

$summary = @()
foreach ($pdf in $pdfs) {
    $name = [System.IO.Path]::GetFileNameWithoutExtension($pdf.Name)
    $dir  = Join-Path $OutDir $name
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Write-Head "Rechnung: $name"

    # Quelle in den Ergebnis-Ordner kopieren (alles an einem Ort).
    $source = Join-Path $dir '00_source.pdf'
    Copy-Item -Path $pdf.FullName -Destination $source -Force

    $textFile = Join-Path $dir '01_text.txt'
    $fldFile  = Join-Path $dir '02_fields.json'
    $valFile  = Join-Path $dir '03_validation.json'
    $bokFile  = Join-Path $dir '04_booking.json'
    $repFile  = Join-Path $dir '05_report.md'

    # Stufe 01 — Ingest (deterministisch, kein LLM): PDF -> Rohtext.
    Write-Step 'Stufe 01 — Ingest (agentkit read-pdf)'
    & $ak read-pdf $source | Set-Content -Path $textFile -Encoding utf8
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $textFile)) {
        Write-Fail 'PDF-Extraktion fehlgeschlagen — überspringe Rechnung.'
        $summary += [pscustomobject]@{ Rechnung = $name; Status = 'ingest-fehler' }
        continue
    }
    Write-Okay '01_text.txt'
    $text = Get-Content -Path $textFile -Raw

    # Gemeinsame Argumente der LLM-Stufen: One-shot, plain (reine Transformation), leiser Sandbox.
    $common = @('-p', '--provider', $Provider, '--strategy', 'plain', '--no-subagents', '--workspace', $dir)

    # Stufe 02 — Extraktion §14 -> JSON.
    $ok = Invoke-Stage -StdinText $text -OutFile $fldFile -Label 'Stufe 02 — Extraktion (§14 UStG)' `
        -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '02_extract_fields.md'),
            'Extrahiere die umsatzsteuerlichen Pflichtangaben nach §14 UStG aus dem Rechnungstext als JSON.'))
    if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Status = 'extraktion-fehler' }; continue }

    # Stufe 03 — Validierung -> JSON.
    $fields = Get-Content -Path $fldFile -Raw
    $ok = Invoke-Stage -StdinText $fields -OutFile $valFile -Label 'Stufe 03 — Validierung' `
        -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '03_validate.md'),
            'Validiere diese extrahierten Rechnungsmerkmale und gib das Ergebnis als JSON zurück.'))
    if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Status = 'validierung-fehler' }; continue }

    # Stufe 04 — Buchung SKR03 -> JSON. Eingabe: Felder + Validierung kombiniert.
    $validation = Get-Content -Path $valFile -Raw
    $combined = "### RECHNUNGSFELDER (JSON)`n$fields`n`n### VALIDIERUNG (JSON)`n$validation"
    $ok = Invoke-Stage -StdinText $combined -OutFile $bokFile -Label 'Stufe 04 — Buchung (SKR03)' `
        -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '04_book.md'),
            'Erzeuge aus Feldern und Validierung einen SKR03-Buchungsvorschlag als JSON.'))
    if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Status = 'buchung-fehler' }; continue }

    # Stufe 05 — Report (Markdown). Eingabe: alle drei JSON-Blöcke.
    $booking = Get-Content -Path $bokFile -Raw
    $allJson = "### FELDER`n$fields`n`n### VALIDIERUNG`n$validation`n`n### BUCHUNG`n$booking"
    $ok = Invoke-Stage -StdinText $allJson -OutFile $repFile -Label 'Stufe 05 — Report (Markdown)' `
        -AkArgs ($common + @('--system-file', (Join-Path $prompts '05_report.md'),
            'Erstelle den Prüf- und Buchungsbericht als Markdown.'))
    if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Status = 'report-fehler' }; continue }

    # Status aus dem Validierungs-JSON ziehen (best effort).
    $status = 'ok'
    try { $status = (Get-Content -Path $valFile -Raw | ConvertFrom-Json).gesamt_status } catch {}
    $summary += [pscustomobject]@{ Rechnung = $name; Status = $status; Ordner = $dir }
    Write-Okay "fertig — Ergebnisse in $dir"
}

Write-Head 'Zusammenfassung'
$summary | Format-Table -AutoSize | Out-String | Write-Host
Write-Host "Alle Ergebnis-Ordner unter: $OutDir" -ForegroundColor Cyan
