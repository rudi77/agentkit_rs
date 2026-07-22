<#
.SYNOPSIS
    logwatch — agentkit als lernender Log-Filter. Meldet nur, was NEU ist.

.DESCRIPTION
    Ein `grep` findet Auffälligkeiten. Es findet sie jeden Tag wieder — dieselben. Deshalb
    schaut irgendwann niemand mehr hin.

    logwatch besteht aus ZWEI agentkit-Agenten mit je genau einer Aufgabe:

      1. Analyse   --skills ./skills   Fachwissen je Logtyp, ON DEMAND geladen (progressive
                   (KEIN Gedächtnis)   disclosure): im Kontext liegt nur der schlanke Index,
                                       die Anleitung holt der Agent erst per read_skill().
                                       Seine einzige Frage: „Was steht in diesen Zeilen?“

      2. Abgleich  --memory state/…    Ein LANGZEITGEDÄCHTNIS (JSONL), das den Lauf überlebt.
                   (sieht das Log      Der Agent fragt per `recall`: kenne ich das schon?
                    NICHT)             Seine einzige Frage: „Neu — oder schon gemeldet?“

      3. Merken    (PowerShell)        Die neuen Befunde werden ins Gedächtnis geschrieben.
                                       Deterministisch, kein Modell.

    Warum die Trennung? Ein einzelner Agent, der beides tut, scheitert daran, dass `recall`
    innerhalb EINES Laufs sofort sieht, was `remember` gerade geschrieben hat — er findet seine
    eigenen frischen Einträge und hält alles für „schon bekannt“. Ein Werkzeug, eine Aufgabe.

    Ergebnis: Beim ZWEITEN Lauf über dieselben Daten meldet er NICHTS mehr — er weiß, dass er
    es schon gesagt hat. Neu ist nur, was wirklich neu ist. Genau das macht ihn benutzbar.

    Was NEU an den Zeilen ist (Offset seit letztem Lauf), rechnet PowerShell aus — das ist
    deterministische Arbeit. Was an den Zeilen AUFFÄLLIG ist und ob es schon bekannt war, ist
    Urteilsarbeit und gehört den Agenten.

.PARAMETER Path      Logdatei(en). Ohne Angabe: die Fixtures.
.PARAMETER Demo      Führt die ganze Beweiskette vor: Tag 1 -> Tag 1 nochmal -> Tag 2.
.PARAMETER Replay    Offsets ignorieren (Datei von vorn lesen), Gedächtnis BEHALTEN.
.PARAMETER Fresh     Gedächtnis UND Offsets verwerfen — der Agent fängt bei null an.
.PARAMETER StateDir  Ablage für Gedächtnis + Offsets (Default: .\state).
.PARAMETER Provider  LLM-Provider: auto | azure | openai (Default: auto).
.PARAMETER Model     Optionales OpenAI-Modell (setzt OPENAI_MODEL).
.PARAMETER EnvFile   Optionale .env mit LLM-Credentials (sonst Auto-Suche).
.PARAMETER AgentkitPath  Pfad zur agentkit-Executable (sonst Auto-Suche im Repo/PATH).

.EXAMPLE
    .\Invoke-LogWatch.ps1 -Demo               # die volle Beweiskette
    .\Invoke-LogWatch.ps1 -Path C:\inetpub\logs\LogFiles\W3SVC1\u_ex260711.log
    .\Invoke-LogWatch.ps1 -Fresh              # Gedächtnis löschen und neu anfangen

.OUTPUTS
    Exit-Code 0 = nichts Neues. 1 = neue Befunde. (Für geplante Tasks/Monitoring.)
#>
[CmdletBinding()]
param(
    [string[]]$Path,
    [switch]$Demo,
    [switch]$Replay,
    [switch]$Fresh,
    [string]$StateDir,
    [ValidateSet('auto', 'azure', 'openai')] [string]$Provider = 'auto',
    [string]$Model,
    [string]$EnvFile,
    [string]$AgentkitPath
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -Scope Global -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'modules\logwatch-helpers.ps1')

$repoDir = Split-Path -Parent (Split-Path -Parent $here)
$skills = Join-Path $here 'skills'
$prompts = Join-Path $here 'prompts'
$fixtures = Join-Path $here 'fixtures'
if (-not $StateDir) { $StateDir = Join-Path $here 'state' }
if ($Model) { $env:OPENAI_MODEL = $Model }

# --- LLM-Credentials -----------------------------------------------------------------
$envCandidates = @()
if ($EnvFile) { $envCandidates += $EnvFile }
$envCandidates += (Join-Path $here '.env')
$envCandidates += (Join-Path $repoDir '.env')
foreach ($cand in $envCandidates) { if (Import-DotEnv $cand) { Write-Host "  .env geladen: $cand" -ForegroundColor DarkGray; break } }

$ak = Resolve-Agentkit -Explicit $AgentkitPath -RepoDir $repoDir
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$memory = Join-Path $StateDir 'known.jsonl'

if ($Fresh) {
    Remove-Item $memory -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $StateDir 'offsets.json') -ErrorAction SilentlyContinue
    Write-Host "  Gedächtnis und Offsets verworfen — der Agent fängt bei null an." -ForegroundColor Yellow
}

Write-Host "agentkit:   $ak"
Write-Host "Skills:     $skills"
Write-Host "Gedächtnis: $memory$(if (Test-Path $memory) { ' (' + @(Get-Content $memory).Count + ' Einträge)' } else { ' (leer)' })"

# =====================================================================================
# Ein Lauf über EINE Datei
# =====================================================================================
function Invoke-Watch {
    param([string]$LogPath, [switch]$ReplayFile, [string]$Titel)

    $store = Get-OffsetStore -StateDir $StateDir
    $slice = Get-NewLines -Path $LogPath -Store $store -Replay:$ReplayFile
    $zeilen = Remove-LogComments -Lines $slice.neu
    $logtyp = Get-LogType -Path $LogPath
    $stamm = ($slice.datei -replace '\W', '_')

    Write-Head ($Titel ? $Titel : ("Lauf: " + $slice.datei))
    Write-Step ("Logtyp {0} — {1} neue Zeile(n) (ab Zeile {2} von {3})" -f $logtyp, @($zeilen).Count, $slice.ab_zeile, $slice.gesamt)

    if (@($zeilen).Count -eq 0) {
        Write-Ruhig 'Keine neuen Zeilen — nichts zu tun.'
        return 0
    }

    # ---------------------------------------------------------------- Stufe 1: Analyse
    # Dieser Agent hat --skills, aber KEIN --memory. Er weiß nichts von früher und soll
    # nichts davon wissen: seine einzige Frage ist „was steht in diesen Zeilen?“.
    $auftragA = "Logtyp: $logtyp. Datei: $($slice.datei). Analysiere die Logzeilen aus stdin. " +
    "Lade zuerst den passenden Skill und melde ALLE Auffaelligkeiten mit stabiler Signatur."

    $argsA = @('--provider', $Provider, '--format', 'json', '--no-subagents', '--steps', '--no-color',
        '--max-steps', '20', '--workspace', $StateDir,
        '--skills', $skills,
        '--system-file', (Join-Path $prompts '10_analyze.md'), $auftragA)

    $logA = Join-Path $StateDir "trace_${stamm}_1_analyse.log"
    $jsonA = ($zeilen -join "`n") | & $ak @argsA 2> $logA
    if ($LASTEXITCODE -ne 0) { Write-Fail "Analyse fehlgeschlagen (Exit $LASTEXITCODE) — siehe $logA"; return -1 }
    try { $a = $jsonA | ConvertFrom-Json } catch { Write-Fail 'Analyse: kein gültiges JSON.'; return -1 }

    $skillTxt = if ($a.skill_genutzt) { $a.skill_genutzt } else { '(keiner)' }
    $befunde = @($a.befunde)
    Write-Step ("Stufe 1 — Analyse (Skill: {0}): {1} Auffälligkeit(en)" -f $skillTxt, $befunde.Count)

    if ($befunde.Count -eq 0) {
        $store[$slice.datei] = $slice.gesamt
        Save-OffsetStore -StateDir $StateDir -Store $store
        Write-Okay 'Nichts Auffälliges im Log.'
        return 0
    }

    # ---------------------------------------------------------------- Stufe 2: Abgleich
    # Dieser Agent hat --memory (also `recall`), sieht aber das Log NICHT. Seine einzige
    # Frage: neu oder schon gemeldet? Er schreibt NICHT ins Gedächtnis — das macht die
    # Pipeline unten, damit `recall` nicht die eigenen frischen Einträge findet.
    $auftragB = "Hier sind die Befunde des Analyse-Agenten. Entscheide fuer jeden per recall, " +
    "ob er neu ist oder bereits gemeldet wurde. Nutze KEIN remember."

    $argsB = @('--provider', $Provider, '--format', 'json', '--no-subagents', '--steps', '--no-color',
        '--max-steps', '30', '--workspace', $StateDir,
        '--memory', $memory,
        '--system-file', (Join-Path $prompts '20_dedup.md'), $auftragB)

    $logB = Join-Path $StateDir "trace_${stamm}_2_abgleich.log"
    $jsonB = ($befunde | ConvertTo-Json -Depth 6) | & $ak @argsB 2> $logB
    if ($LASTEXITCODE -ne 0) { Write-Fail "Abgleich fehlgeschlagen (Exit $LASTEXITCODE) — siehe $logB"; return -1 }
    try { $r = $jsonB | ConvertFrom-Json } catch { Write-Fail 'Abgleich: kein gültiges JSON.'; return -1 }

    Write-Step ("Stufe 2 — Abgleich mit dem Gedächtnis: {0} neu, {1} bekannt" -f @($r.neu).Count, @($r.bereits_bekannt).Count)

    # Offset fortschreiben — die Zeilen gelten als gesehen.
    $store[$slice.datei] = $slice.gesamt
    Save-OffsetStore -StateDir $StateDir -Store $store

    # --- Ausgabe ---
    $neu = @($r.neu)
    foreach ($b in $neu) {
        $farbe = switch ($b.schweregrad) { 'kritisch' { 'Red' } 'hoch' { 'Yellow' } default { 'Gray' } }
        Write-Host ("  [NEU/{0}] {1}" -f $b.schweregrad.ToUpper(), $b.signatur) -ForegroundColor $farbe
        Write-Host ("           {0}" -f $b.was) -ForegroundColor DarkGray
        if ($b.anzahl) { Write-Host ("           {0}x, {1}" -f $b.anzahl, $b.zeitfenster) -ForegroundColor DarkGray }
        if ($b.bezieht_sich_auf_bekanntes) {
            Write-Host ("           ^ Verschlechterung von: {0}" -f $b.bezieht_sich_auf_bekanntes) -ForegroundColor Magenta
        }
        if ($b.empfehlung) { Write-Host ("           -> {0}" -f $b.empfehlung) -ForegroundColor DarkGray }
    }

    foreach ($k in @($r.bereits_bekannt)) {
        Write-Ruhig ("bekannt, nicht erneut gemeldet: {0}" -f $k.signatur)
    }

    # --- Stufe 3: Persistenz (deterministisch, kein Modell) ---
    # Nur die NEUEN Befunde wandern ins Gedächtnis. Beim nächsten Lauf findet `recall` sie
    # dort — und der Abgleich-Agent schweigt.
    $gemerkt = Add-ToMemory -MemoryPath $memory -Befunde $neu -Datum (Get-Date -Format 'yyyy-MM-dd')
    if ($gemerkt -gt 0) { Write-Step ("Stufe 3 — {0} Befund(e) ins Gedächtnis geschrieben." -f $gemerkt) }

    if ($neu.Count -eq 0) {
        Write-Okay 'Nichts Neues. Der Wachhund bleibt still.'
    }
    else {
        Write-Host ("  => {0} neue(r) Befund(e); {1} bekannte unterdrückt." -f $neu.Count, @($r.bereits_bekannt).Count) -ForegroundColor Cyan
    }
    return $neu.Count
}

# =====================================================================================
# Demo: die ganze Beweiskette
# =====================================================================================
if ($Demo) {
    if (-not (Test-Path (Join-Path $fixtures 'iis_tag1.log'))) {
        Write-Step 'Fixtures fehlen — erzeuge sie …'
        & (Join-Path $here 'tools\Build-Fixtures.ps1') | Out-Null
    }
    Remove-Item $memory -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $StateDir 'offsets.json') -ErrorAction SilentlyContinue
    $geseat = Initialize-NoiseMemory -MemoryPath $memory
    Write-Host "`n  Demo startet mit frischem Gedächtnis ($geseat Rausch-Einträge geseedet)." -ForegroundColor DarkGray

    $tag1 = Join-Path $fixtures 'iis_tag1.log'
    $tag2 = Join-Path $fixtures 'iis_tag2.log'
    $pg = Join-Path $fixtures 'postgres_tag1.log'

    $n1 = Invoke-Watch -LogPath $tag1 -Titel '1/4 — Tag 1, erster Blick (er kennt nur das Rauschen)'
    $n2 = Invoke-Watch -LogPath $tag1 -ReplayFile -Titel '2/4 — Tag 1 NOCH EINMAL (dieselben Zeilen!)'
    $n3 = Invoke-Watch -LogPath $tag2 -Titel '3/4 — Tag 2 (Bekanntes läuft weiter, aber etwas hat sich geändert)'
    $n4 = Invoke-Watch -LogPath $pg -Titel '4/4 — Anderer Logtyp: PostgreSQL (anderer Skill)'

    Write-Head 'Beweis'
    $p1 = $n1 -gt 0; $p2 = $n2 -eq 0; $p3 = $n3 -gt 0; $p4 = $n4 -gt 0
    function Zeile($label, $n, $ok, $gut, $schlecht) {
        Write-Host ("  {0,-26} {1} neu   — {2}" -f $label, $n, $(if ($ok) { $gut } else { $schlecht })) `
            -ForegroundColor $(if ($ok) { 'Green' } else { 'Red' })
    }
    Zeile 'Lauf 1 (Tag 1)' $n1 $p1 'er sieht die Probleme zum ersten Mal.' 'FEHLER — er hätte etwas finden müssen!'
    Zeile 'Lauf 2 (Tag 1 nochmal)' $n2 $p2 'STILL. Er weiß, dass er es schon gesagt hat.' 'FEHLER — er wiederholt sich!'
    Zeile 'Lauf 3 (Tag 2)' $n3 $p3 'nur das wirklich Neue (Bekanntes blieb still).' 'FEHLER — die Verschlechterung wurde übersehen!'
    Zeile 'Lauf 4 (PostgreSQL)' $n4 $p4 'anderer Skill, anderes Fachwissen.' 'FEHLER — nichts gefunden.'

    Write-Host "`n  Gedächtnis: $memory ($(@(Get-Content $memory -ErrorAction SilentlyContinue).Count) Einträge)" -ForegroundColor Cyan

    if ($p1 -and $p2 -and $p3 -and $p4) {
        Write-Host "`n  Der Filter lernt: Bekanntes schweigt, Neues wird gemeldet." -ForegroundColor Green
        exit 0
    }
    Write-Host "`n  Die Beweiskette ist NICHT vollständig." -ForegroundColor Red
    exit 1
}

# =====================================================================================
# Normalbetrieb
# =====================================================================================
if (-not $Path) {
    $Path = @(Get-ChildItem -Path $fixtures -Filter '*.log' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
    if (-not $Path) { Write-Fail "Keine Logdatei angegeben und keine Fixtures da. Erst: .\tools\Build-Fixtures.ps1"; exit 1 }
    Write-Step 'Keine -Path angegeben — nutze die Fixtures.'
}

$gesamtNeu = 0
foreach ($p in $Path) {
    if (-not (Test-Path $p)) { Write-Fail "Nicht gefunden: $p"; continue }
    $n = Invoke-Watch -LogPath $p -ReplayFile:$Replay
    if ($n -gt 0) { $gesamtNeu += $n }
}

Write-Host ""
if ($gesamtNeu -eq 0) { Write-Host "Nichts Neues." -ForegroundColor Green; exit 0 }
Write-Host "$gesamtNeu neue(r) Befund(e) -> Exit 1." -ForegroundColor Yellow
exit 1
