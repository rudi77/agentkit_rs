<#
    End-to-End-Test des interaktiven Orchestrators (Human-in-the-Loop + Lernen).
    Nutzt den scriptbaren REPL (`agentkit --repl`) mit gescripteten Antworten und ein echtes
    Modell. Wird ÜBERSPRUNGEN, wenn keine LLM-Credentials gesetzt/auffindbar sind.

    Geprüft wird der Lernpfad: eine Rechnung eines UNBEKANNTEN Lieferanten wird verarbeitet;
    der Orchestrator muss (a) beim Menschen nachfragen (er beendet dazu seinen Zug — kein
    Sonderwerkzeug) und (b) danach eine neue Lieferanten-Entität im OKF-Wissensgraph anlegen.

    Aufruf:  pwsh -File .\tests\Test-Interactive.ps1
#>
[CmdletBinding()]
param([ValidateSet('auto', 'azure', 'openai')] [string]$Provider = 'auto', [string]$AgentkitPath)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$base = Split-Path -Parent $here
$repoDir = Split-Path -Parent (Split-Path -Parent $base)
. (Join-Path $base 'modules\ap-helpers.ps1')
foreach ($c in @((Join-Path $base '.env'), (Join-Path $repoDir '.env'))) { if (Import-DotEnv $c) { break } }

if (-not $env:AZURE_OPENAI_API_KEY -and -not $env:OPENAI_API_KEY) {
    Write-Host "ÜBERSPRUNGEN: keine LLM-Credentials (AZURE_OPENAI_* / OPENAI_API_KEY)." -ForegroundColor Yellow
    exit 0
}

# agentkit auflösen.
$ak = $AgentkitPath
if (-not $ak) {
    foreach ($rel in @('target\release\agentkit.exe', 'target\debug\agentkit.exe')) {
        $p = Join-Path $repoDir $rel; if (Test-Path $p) { $ak = $p; break }
    }
}
if (-not $ak) { $ak = (Get-Command agentkit -ErrorAction SilentlyContinue)?.Source }
if (-not $ak) { Write-Host "agentkit nicht gefunden — erst bauen." -ForegroundColor Red; exit 1 }

# Frischer Arbeitsordner aus dem Seed.
$ws = Join-Path $env:TEMP ("apint_test_" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force $ws | Out-Null
Copy-Item (Join-Path $base 'knowledge') $ws -Recurse -Force
Copy-Item (Join-Path $base 'inbox') $ws -Recurse -Force
Copy-Item (Join-Path $base 'tools') $ws -Recurse -Force        # Compliance-Werkzeuge (xcheck/gobd/datev/dublette)
Copy-Item (Join-Path $base 'modules') $ws -Recurse -Force      # ap-helpers, von den Werkzeugen dot-sourced
New-Item -ItemType Directory -Force (Join-Path $ws 'out') | Out-Null
$roles = Join-Path $base 'roles'; $orch = Join-Path $base 'orchestrator.md'

$pass = 0; $fail = 0
function Assert($cond, [string]$name) {
    if ($cond) { $script:pass++; Write-Host "  [PASS] $name" -ForegroundColor Green }
    else { $script:fail++; Write-Host "  [FAIL] $name" -ForegroundColor Red }
}

try {
    Write-Host "== Interaktiver Orchestrator: HITL-Lernpfad (unbekannter Lieferant) =="
    # Der Orchestrator stellt hier ZWEI Rückfragen (erst Extraktions-Abgleich, dann Kontierung des
    # unbekannten Lieferanten) — die genaue Zahl variiert je Modell. Damit die scriptbare REPL-Sitzung
    # robust bleibt, enthält JEDE Antwortzeile BEIDES: Bestätigung der Extraktion UND die Kontierung.
    # Egal welche Frage kommt, die Antwort passt, und die Lieferantendaten stehen immer bereit.
    $reply = "Die extrahierten Angaben passen, keine Korrekturen. Kontierung fuer diesen Lieferanten: Kostenstelle KST-4900 (Verwaltung); Standard-Aufwandskonto SKR03 4930 (Buerobedarf); Freigabe-Verantwortlicher: Stefan Klein."
    $script = "Verarbeite die Eingangsrechnung inbox/rechnung_meier.txt und melde mir das Ergebnis.`n$reply`n$reply`n$reply`n/exit`n"
    $out = $script | & $ak --repl --yes --provider $Provider --no-color -w $ws --agents $roles --system-file $orch 2>&1 | Out-String

    Assert ($out -match 'Kostenstelle|Konto|Freigabe|korrigier|stimmen|\?') 'Orchestrator hat beim Menschen nachgefragt (Rückfrage in Prosa, ohne Sonderwerkzeug)'
    $meier = Get-ChildItem (Join-Path $ws 'knowledge\lieferanten') -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match 'meier' } | Select-Object -First 1
    Assert ([bool]$meier) 'Neue Lieferanten-Entität im Wissensgraph angelegt (Lernen)'
    if ($meier) {
        $c = Get-Content $meier.FullName -Raw
        Assert ($c -match 'type:\s*lieferant') 'Neue Entität ist gültiges OKF (type: lieferant)'
        Assert ($c -match 'DE255558888') 'USt-IdNr. aus der Rechnung übernommen'
    }
}
finally {
    Get-ChildItem $ws -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { try { $_.IsReadOnly = $false } catch {} }
    Remove-Item $ws -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host ("Ergebnis: {0} PASS, {1} FAIL" -f $pass, $fail) -ForegroundColor $(if ($fail -eq 0) { 'Green' } else { 'Red' })
if ($fail -gt 0) { exit 1 }
