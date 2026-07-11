<#
.SYNOPSIS
    Accounts Payable mit agentkit — EIN Beispiel, zwei Betriebsarten (Batch & interaktiv).

.DESCRIPTION
    Derselbe Eingangsrechnungs-Prozess (E-Rechnung/EN 16931, GoBD, DATEV, Dublettenprüfung) in
    zwei Modi, die dieselben Bausteine teilen (modules/ap-helpers.ps1, tools/*.ps1, roles/):

      -Mode Batch        Deterministische Pipeline über .\inbox — ein Werkzeug/Agent pro Stufe,
                         volle Compliance-Artefakte je Rechnung (out\<name>\ + Sammelstapel +
                         Register). Nicht-interaktiv, ideal für Automatisierung/CI.

      -Mode Interactive  Orchestrator-Agent („Leiterin der Buchhaltung") im agentkit-TUI:
                         delegiert an die Fach-Rollen (extractor/validator/booker), ruft die
                         Compliance-Werkzeuge (xcheck/GoBD/DATEV/Dublette) über run_shell auf,
                         fragt bei Wissenslücken per ask_user nach (Human-in-the-Loop) und lernt
                         in den OKF-Wissensgraph (knowledge/). Superset der Batch-Fähigkeiten.

      -Mode Repl         Wie Interactive, aber der (scriptbare) REPL statt des TUI — praktisch
                         für Automatisierung und Tests.

    Shell-Aufrufe des Orchestrators an die Compliance-Werkzeuge werden im interaktiven Modus per
    `--yes` automatisch freigegeben; die menschlichen Entscheidungen laufen bewusst über
    `ask_user`, nicht über Shell-Freigaben. Mit -ApproveShell wird jede Shell-Ausführung einzeln
    bestätigt.

.PARAMETER Mode          Batch | Interactive | Repl (Default: Interactive).
.PARAMETER InboxDir      (Batch) Ordner mit Eingangsrechnungen (Default: .\inbox).
.PARAMETER OutDir        (Batch) Zielordner; je Rechnung ein Unterordner (Default: .\out).
.PARAMETER WorkspaceDir  (Interactive/Repl) Arbeitsordner (Default: .\workspace).
.PARAMETER Fresh         (Interactive/Repl) Arbeitsordner neu aus dem Seed aufsetzen.
.PARAMETER ApproveShell  (Interactive/Repl) Shell-Ausführungen einzeln bestätigen (kein --yes).
.PARAMETER Provider      LLM-Provider: auto | azure | openai (Default: auto).
.PARAMETER Model         Optionales OpenAI-Modell (setzt OPENAI_MODEL).
.PARAMETER EnvFile       Optionale .env mit LLM-Credentials (sonst Auto-Suche).
.PARAMETER AgentkitPath  Pfad zur agentkit-Executable (sonst Auto-Suche im Repo/PATH).
.PARAMETER XCheckUrl     Basis-URL der xcheck-E-Rechnungs-API (Default: Env XCHECK_URL).
.PARAMETER XCheckApiKey  API-Key für xcheck (Default: Env XCHECK_API_KEY).

.EXAMPLE
    .\Invoke-Ap.ps1 -Mode Interactive
.EXAMPLE
    .\Invoke-Ap.ps1 -Mode Batch -XCheckUrl 'http://localhost:5080' -XCheckApiKey 'inv_port_...'
.EXAMPLE
    .\Invoke-Ap.ps1 -Mode Repl -Fresh
#>
[CmdletBinding()]
param(
    [ValidateSet('Batch', 'Interactive', 'Repl')] [string]$Mode = 'Interactive',
    [string]$InboxDir,
    [string]$OutDir,
    [string]$WorkspaceDir,
    [switch]$Fresh,
    [switch]$ApproveShell,
    [ValidateSet('auto', 'azure', 'openai')] [string]$Provider = 'auto',
    [string]$Model,
    [string]$EnvFile,
    [string]$AgentkitPath,
    [string]$XCheckUrl,
    [string]$XCheckApiKey
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -Scope Global -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoDir = Split-Path -Parent (Split-Path -Parent $here)
. (Join-Path $here 'modules\ap-helpers.ps1')

if ($Model) { $env:OPENAI_MODEL = $Model }

# --- LLM-Credentials (.env in Prozess-Umgebung ziehen) -------------------------------
$envCandidates = @()
if ($EnvFile) { $envCandidates += $EnvFile }
$envCandidates += (Join-Path $here '.env')
$envCandidates += (Join-Path $repoDir '.env')
foreach ($cand in $envCandidates) { if (Import-DotEnv $cand) { Write-Host "  .env geladen: $cand" -ForegroundColor DarkGray; break } }

# --- xcheck-Konfiguration (Param > Env) — gilt für beide Modi -------------------------
if (-not $XCheckUrl) { $XCheckUrl = $env:XCHECK_URL }
if (-not $XCheckApiKey) { $XCheckApiKey = $env:XCHECK_API_KEY }
if ($XCheckUrl) { $env:XCHECK_URL = $XCheckUrl }          # an den Orchestrator (xcheck.ps1) durchreichen
if ($XCheckApiKey) { $env:XCHECK_API_KEY = $XCheckApiKey }
$xcheckOn = [bool]$XCheckUrl -and [bool]$XCheckApiKey

# --- agentkit-Executable auflösen ----------------------------------------------------
function Resolve-Agentkit {
    param([string]$Explicit)
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw "agentkit nicht gefunden: $Explicit"
    }
    foreach ($rel in @('target\release\agentkit.exe', 'target\debug\agentkit.exe')) {
        $p = Join-Path $repoDir $rel
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command agentkit -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "Keine agentkit-Executable gefunden. Baue: cargo build --release --features `"tui pdf`" (in agent_framework_rs)."
}
$ak = Resolve-Agentkit -Explicit $AgentkitPath

function Write-Head($m) { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Write-Step($m) { Write-Host "  -> $m" -ForegroundColor DarkGray }
function Write-Okay($m) { Write-Host "  [ok] $m" -ForegroundColor Green }
function Write-Fail($m) { Write-Host "  [!!] $m" -ForegroundColor Red }

# =====================================================================================
#  MODUS: BATCH — deterministische Pipeline (ein Werkzeug/Agent pro Stufe)
# =====================================================================================
function Invoke-BatchPipeline {
    $prompts = Join-Path $here 'prompts'
    if (-not $InboxDir) { $InboxDir = Join-Path $here 'inbox' }
    if (-not $OutDir) { $OutDir = Join-Path $here 'out' }

    # Führt EINE agentkit-LLM-Stufe aus: stdin -> agentkit -> $OutFile (UTF-8). $true bei Exit 0.
    function Invoke-Stage {
        param([string]$StdinText, [string[]]$AkArgs, [string]$OutFile, [string]$Label)
        Write-Step $Label
        $out = $StdinText | & $ak @AkArgs
        if ($LASTEXITCODE -ne 0) { Write-Fail "$Label fehlgeschlagen (Exit $LASTEXITCODE)."; return $false }
        Set-Content -Path $OutFile -Value $out -Encoding utf8
        Write-Okay ("{0}  ({1})" -f $Label, (Split-Path -Leaf $OutFile))
        return $true
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $register = Join-Path $OutDir '_register.json'
    $inputs = @(Get-ChildItem -Path $InboxDir -File -Include *.pdf, *.xml -ErrorAction SilentlyContinue)
    if ($inputs.Count -eq 0) {
        $inputs = @(Get-ChildItem -Path (Join-Path $InboxDir '*') -File | Where-Object { $_.Extension -in '.pdf', '.xml' })
    }
    if ($inputs.Count -eq 0) { Write-Warning "Keine PDF/XML in $InboxDir. Erst Beispiele erzeugen: .\tools\Build-Samples.ps1"; return }

    Write-Host "agentkit: $ak"
    Write-Host "Provider: $Provider$(if ($Model) { " ($Model)" })"
    Write-Host "xcheck:   $(if ($xcheckOn) { $XCheckUrl } else { '(nicht konfiguriert -> E-Rechnungs-Prüfung wird übersprungen)' })"
    Write-Host "Inbox:    $InboxDir  ($($inputs.Count) Rechnung(en))"

    $summary = @()
    $datevAll = @()

    foreach ($inv in ($inputs | Sort-Object Name)) {
        $name = [System.IO.Path]::GetFileNameWithoutExtension($inv.Name)
        $kind = Get-InvoiceKind $inv.FullName
        $dir = Join-Path $OutDir $name
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Write-Head "Rechnung: $name  [$kind]"

        # --- 00 Ingest: Original GoBD-konform ablegen (schreibgeschützt) ---
        $srcOut = Join-Path $dir ('00_source' + $inv.Extension.ToLower())
        if (Test-Path -LiteralPath $srcOut) { (Get-Item -LiteralPath $srcOut).IsReadOnly = $false }
        Copy-Item -LiteralPath $inv.FullName -Destination $srcOut -Force
        Set-ItemProperty -LiteralPath $srcOut -Name IsReadOnly -Value $true

        $contentFile = Join-Path $dir '01_content.txt'
        $checkFile = Join-Path $dir '02_einvoice_check.json'
        $fldFile = Join-Path $dir '03_fields.json'
        $valFile = Join-Path $dir '04_validation.json'
        $bokFile = Join-Path $dir '05_booking.json'
        $datevFile = Join-Path $dir '06_datev.csv'
        $repFile = Join-Path $dir '07_report.md'

        # --- 01 Inhalt gewinnen: XML direkt bzw. sichtbaren Text via read-pdf ---
        Write-Step 'Stufe 01 - Inhalt (read-pdf / XML)'
        if ($kind -eq 'xrechnung') {
            Copy-Item -LiteralPath $inv.FullName -Destination $contentFile -Force
            (Get-Item $contentFile).IsReadOnly = $false
        }
        else {
            & $ak read-pdf $srcOut | Set-Content -Path $contentFile -Encoding utf8
            if ($LASTEXITCODE -ne 0) { Write-Fail 'PDF-Extraktion fehlgeschlagen.'; $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; Status = 'ingest-fehler' }; continue }
        }
        Write-Okay '01_content.txt'
        $content = Get-Content -Path $contentFile -Raw

        # --- 02 E-Rechnungs-Konformität (xcheck / EN 16931) — nur für E-Rechnungen ---
        Write-Step 'Stufe 02 - E-Rechnung (EN 16931 via xcheck)'
        if ($kind -eq 'pdf') {
            $check = [pscustomobject]@{ geprueft = $false; grund = 'papierbasierte Rechnung (keine strukturierte E-Rechnung)' }
        }
        elseif (-not $xcheckOn) {
            $check = [pscustomobject]@{ geprueft = $false; grund = 'xcheck nicht konfiguriert (XCheckUrl/XCheckApiKey fehlen)' }
        }
        else {
            $x = Invoke-XCheck -FilePath $inv.FullName -Kind $kind -Url $XCheckUrl -ApiKey $XCheckApiKey
            if ($x.available) {
                $check = [pscustomobject]@{
                    geprueft          = $true
                    format            = $x.formatDetected
                    konform_en16931   = $x.isValid
                    syntax_valid      = $x.syntaxValid
                    anzahl_findings   = @($x.semanticErrors).Count
                    findings          = @($x.semanticErrors)
                    credits_remaining = $x.creditsRemaining
                }
            }
            else {
                $check = [pscustomobject]@{ geprueft = $false; grund = $x.reason }
            }
        }
        $check | ConvertTo-Json -Depth 6 | Set-Content -Path $checkFile -Encoding utf8
        $enText = if ($check.geprueft) { "konform=$($check.konform_en16931), findings=$($check.anzahl_findings)" } else { $check.grund }
        Write-Okay "02_einvoice_check.json  ($enText)"

        # --- 03 Extraktion §14 -> JSON ---
        $common = @('-p', '--provider', $Provider, '--strategy', 'plain', '--no-subagents', '--workspace', $dir)
        $ok = Invoke-Stage -StdinText $content -OutFile $fldFile -Label 'Stufe 03 - Extraktion (§14 UStG)' `
            -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '02_extract_fields.md'),
                'Extrahiere die §14-Merkmale aus dem Rechnungsinhalt (Text ODER EN-16931-XML) als JSON.'))
        if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; Status = 'extraktion-fehler' }; continue }
        $fields = Get-Content -Path $fldFile -Raw | ConvertFrom-Json

        # --- Dublettenprüfung ---
        $key = Get-InvoiceKey -Fields $fields
        $dup = Find-Duplicate -RegisterPath $register -Key $key
        $dupObj = if ($dup) { [pscustomobject]@{ dublette = $true; bezug = $dup.rechnung } } else { [pscustomobject]@{ dublette = $false } }
        if ($dup) { Write-Fail "Dublette erkannt (bereits verarbeitet als '$($dup.rechnung)')." }

        # --- 04 Validierung (Arithmetik + §14 + EN-16931-Verdikt + Dublette) ---
        $checkJson = Get-Content -Path $checkFile -Raw
        $valInput = "### RECHNUNGSFELDER (JSON)`n$(Get-Content $fldFile -Raw)`n`n### E-RECHNUNG-PRÜFUNG (xcheck / EN 16931)`n$checkJson`n`n### DUBLETTE`n$($dupObj | ConvertTo-Json -Compress)"
        $ok = Invoke-Stage -StdinText $valInput -OutFile $valFile -Label 'Stufe 04 - Validierung' `
            -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '03_validate.md'),
                'Validiere die Rechnung (Pflichtangaben, Arithmetik, EN-16931-Verdikt, Dublette) als JSON.'))
        if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; Status = 'validierung-fehler' }; continue }

        # --- 05 Buchung (SKR03) ---
        $bokInput = "### RECHNUNGSFELDER (JSON)`n$(Get-Content $fldFile -Raw)`n`n### VALIDIERUNG (JSON)`n$(Get-Content $valFile -Raw)"
        $ok = Invoke-Stage -StdinText $bokInput -OutFile $bokFile -Label 'Stufe 05 - Buchung (SKR03)' `
            -AkArgs ($common + @('--format', 'json', '--system-file', (Join-Path $prompts '04_book.md'),
                'Erzeuge einen SKR03-Buchungsvorschlag als JSON (blockiere bei Fehler/Dublette).'))
        if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; Status = 'buchung-fehler' }; continue }
        $booking = Get-Content -Path $bokFile -Raw | ConvertFrom-Json

        # --- 06 DATEV-Buchungsstapel (EXTF) ---
        Write-Step 'Stufe 06 - DATEV-Export'
        $year = if ($fields.rechnungsdatum -match '^(\d{4})') { [int]$Matches[1] } else { (Get-Date).Year }
        $datevRow = ConvertTo-DatevRow -Booking $booking -Fields $fields
        Write-DatevCsv -Path $datevFile -DataRows (@($datevRow) | Where-Object { $_ }) -Year $year
        if ($datevRow) { $datevAll += $datevRow; Write-Okay '06_datev.csv (1 Buchung)' } else { Write-Okay '06_datev.csv (keine Buchung - blockiert)' }

        # --- 07 Report (Markdown) ---
        $repInput = "### FELDER`n$(Get-Content $fldFile -Raw)`n`n### E-RECHNUNG`n$checkJson`n`n### VALIDIERUNG`n$(Get-Content $valFile -Raw)`n`n### BUCHUNG`n$(Get-Content $bokFile -Raw)"
        $ok = Invoke-Stage -StdinText $repInput -OutFile $repFile -Label 'Stufe 07 - Report (Markdown)' `
            -AkArgs ($common + @('--system-file', (Join-Path $prompts '05_report.md'),
                'Erstelle den Prüf- und Buchungsbericht als Markdown (inkl. E-Rechnung, GoBD, DATEV).'))
        if (-not $ok) { $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; Status = 'report-fehler' }; continue }

        # --- GoBD-Manifest über alle Artefakte ---
        New-GobdManifest -Dir $dir -OriginalName $inv.Name -Kind $kind

        # --- Register aktualisieren (Dublettenschutz künftiger Läufe) ---
        if (-not $dup) { Add-ToRegister -RegisterPath $register -Key $key -Name $name -Fields $fields }

        # Status bestimmen.
        $status = 'ok'
        try { $status = (Get-Content $valFile -Raw | ConvertFrom-Json).gesamt_status } catch {}
        if ($dup) { $status = 'dublette' }
        $enShort = if ($check.geprueft) { if ($check.konform_en16931) { 'konform' } else { "nicht konform ($($check.anzahl_findings))" } } else { '-' }
        $summary += [pscustomobject]@{ Rechnung = $name; Format = $kind; EN16931 = $enShort; Status = $status; Buchbar = [bool]$booking.buchung_moeglich }
        Write-Okay "fertig - Ergebnisse in $dir"
    }

    # --- Sammel-DATEV-Buchungsstapel über alle buchbaren Rechnungen ---
    Write-Head 'Sammel-Export & Zusammenfassung'
    $stapel = Join-Path $OutDir 'datev_buchungsstapel.csv'
    Write-DatevCsv -Path $stapel -DataRows $datevAll
    Write-Host "DATEV-Sammelstapel: $stapel  ($($datevAll.Count) Buchung(en))" -ForegroundColor Green

    $summary | Format-Table -AutoSize | Out-String | Write-Host
    Write-Host "Alle Ergebnis-Ordner unter: $OutDir" -ForegroundColor Cyan
}

# =====================================================================================
#  MODUS: INTERACTIVE / REPL — Orchestrator-Agent (HITL + Lernen + Compliance-Werkzeuge)
# =====================================================================================
function Start-Orchestrator {
    param([switch]$UseRepl)
    if (-not $WorkspaceDir) { $WorkspaceDir = Join-Path $here 'workspace' }

    # Arbeitsordner aus dem Seed aufsetzen: Wissensgraph + Inbox + die Compliance-Werkzeuge,
    # damit der Orchestrator sie via run_shell (cwd = Workspace) aufrufen kann. Seed bleibt rein.
    if ($Fresh -and (Test-Path $WorkspaceDir)) {
        Get-ChildItem $WorkspaceDir -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { $_.IsReadOnly = $false }
        Remove-Item $WorkspaceDir -Recurse -Force
    }
    if (-not (Test-Path (Join-Path $WorkspaceDir 'knowledge'))) {
        New-Item -ItemType Directory -Force -Path $WorkspaceDir | Out-Null
        Copy-Item (Join-Path $here 'knowledge') -Destination $WorkspaceDir -Recurse -Force
        Copy-Item (Join-Path $here 'inbox') -Destination $WorkspaceDir -Recurse -Force
        Copy-Item (Join-Path $here 'tools') -Destination $WorkspaceDir -Recurse -Force
        Copy-Item (Join-Path $here 'modules') -Destination $WorkspaceDir -Recurse -Force
        New-Item -ItemType Directory -Force -Path (Join-Path $WorkspaceDir 'out') | Out-Null
        Write-Host "Arbeitsordner aus Seed aufgesetzt: $WorkspaceDir" -ForegroundColor Green
    }

    $roles = Join-Path $here 'roles'
    $orch = Join-Path $here 'orchestrator.md'
    $akArgs = @('--provider', $Provider, '-w', $WorkspaceDir, '--agents', $roles, '--system-file', $orch)
    if (-not $ApproveShell) { $akArgs += '--yes' }   # Compliance-Werkzeuge ohne Einzel-Freigabe; HITL läuft über ask_user

    Write-Host "agentkit:  $ak"
    Write-Host "Workspace: $WorkspaceDir  (Wissensgraph: knowledge/, Rechnungen: inbox/, Werkzeuge: tools/, Ergebnisse: out/)"
    Write-Host "xcheck:    $(if ($xcheckOn) { $XCheckUrl } else { '(nicht konfiguriert -> E-Rechnungs-Prüfung wird übersprungen)' })"
    Write-Host "Tipp: 'Verarbeite die Rechnung inbox/rechnung_meier.txt' — der Orchestrator fragt bei Bedarf nach.`n"

    if ($UseRepl) { & $ak --repl @akArgs } else { & $ak --tui @akArgs }
}

# --- Dispatch ------------------------------------------------------------------------
switch ($Mode) {
    'Batch' { Invoke-BatchPipeline }
    'Interactive' { Start-Orchestrator }
    'Repl' { Start-Orchestrator -UseRepl }
}
