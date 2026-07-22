<#
.SYNOPSIS
    Windows-Incident-Triage — komponiert aus einzelnen agentkit-Agenten und PowerShell.

.DESCRIPTION
    Beantwortet Fragen, an denen `Select-String` scheitert: „Warum ist die Kiste heute
    Nacht neu gestartet?", „Warum ist die Platte plötzlich voll?", „Hängt das zusammen?"

    Die Stufen sind eigenständige, komponierbare Kommandos (Unix-Prinzip):

        00  Sammeln       Get-WinEvent / Get-Service / Get-CimInstance   -> 00_*.json
        10  Fan-out       VIER agentkit-Agenten PARALLEL, je Subsystem   -> 1x_<name>.json
              system · security · application · update
        20  Korrelation   agentkit (verbindet die vier Befunde)          -> 20_correlation.json
        30  Reparatur     agentkit UNTER --dry-run (darf nichts ändern)  -> 30_remediation.json
                          Vorschlag als Skript                            -> remediation.ps1
        40  Bericht       agentkit (Markdown)                            -> 40_report.md

    Das Sicherheitsnetz: Stufe 30 läuft mit --dry-run. Damit sind run_shell, write_file
    und edit_file für den Agenten No-Ops (agentkit blockiert sie per Namensheuristik).
    Der Agent kann das System also LESEN, aber nichts anfassen. Sein Reparaturvorschlag
    erreicht die Maschine nur über remediation.ps1 — die ein Mensch liest und mit -Apply
    freigibt. Der Prompt bittet darum; --dry-run erzwingt es.

    Läuft ohne Administratorrechte und auch auf Nicht-Windows: fehlt ein Log (oder das
    ganze Event-Log), greift die Pipeline auf die mitgelieferten Fixtures zurück, die
    einen zusammenhängenden Störfall erzählen. So ist die Demo überall vorführbar.

.PARAMETER Hours        Betrachteter Zeitraum in Stunden (Default: 24).
.PARAMETER UseFixtures  Immer die Beispiel-Ereignisse nutzen (kein Zugriff aufs echte Log).
.PARAMETER Apply        Das vorgeschlagene remediation.ps1 nach Rückfrage ausführen.
.PARAMETER OutDir       Zielordner für alle Artefakte (Default: .\out).
.PARAMETER Provider     LLM-Provider: auto | azure | openai (Default: auto).
.PARAMETER Model        Optionales OpenAI-Modell (setzt OPENAI_MODEL).
.PARAMETER EnvFile      Optionale .env mit LLM-Credentials (sonst Auto-Suche).
.PARAMETER AgentkitPath Pfad zur agentkit-Executable (sonst Auto-Suche im Repo/PATH).

.EXAMPLE
    .\Invoke-WinTriage.ps1                       # letzte 24 h dieses Rechners
    .\Invoke-WinTriage.ps1 -UseFixtures          # der mitgelieferte Beispiel-Störfall
    .\Invoke-WinTriage.ps1 -Hours 72 -Apply      # 3 Tage, danach Reparatur freigeben

.OUTPUTS
    Exit-Code 0 = nichts Kritisches. 1 = kritischer Befund (für geplante Tasks/Monitoring).
#>
[CmdletBinding()]
param(
    [int]$Hours = 24,
    [switch]$UseFixtures,
    [switch]$Apply,
    [string]$OutDir,
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
. (Join-Path $here 'modules\triage-helpers.ps1')

$repoDir = Split-Path -Parent (Split-Path -Parent $here)
$prompts = Join-Path $here 'prompts'
$fixtures = Join-Path $here 'fixtures'
if (-not $OutDir) { $OutDir = Join-Path $here 'out' }
if ($Model) { $env:OPENAI_MODEL = $Model }

# --- LLM-Credentials -----------------------------------------------------------------
$envCandidates = @()
if ($EnvFile) { $envCandidates += $EnvFile }
$envCandidates += (Join-Path $here '.env')
$envCandidates += (Join-Path $repoDir '.env')
foreach ($cand in $envCandidates) { if (Import-DotEnv $cand) { Write-Host "  .env geladen: $cand" -ForegroundColor DarkGray; break } }

$ak = Resolve-Agentkit -Explicit $AgentkitPath -RepoDir $repoDir
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host "agentkit: $ak"
Write-Host "Rechner:  $(if ($env:COMPUTERNAME) { $env:COMPUTERNAME } else { [System.Net.Dns]::GetHostName() })"
Write-Host "Zeitraum: letzte $Hours Stunden"
Write-Host "Ausgabe:  $OutDir"

# =====================================================================================
# Stufe 00 — Sammeln (deterministisch, kein LLM)
# =====================================================================================
Write-Head 'Stufe 00 — Fakten sammeln (kein LLM)'

$subsystems = Get-TriageSubsystems
$slices = @{}
foreach ($sub in $subsystems) {
    $res = Get-TriageEvents -Subsystem $sub -Hours $Hours -FixtureDir $fixtures -UseFixtures:$UseFixtures
    $verdichtet = Compress-TriageEvents -Events $res.events
    $slices[$sub.Name] = $verdichtet
    $file = Join-Path $OutDir ("00_{0}.json" -f $sub.Name)
    ($verdichtet | ConvertTo-Json -Depth 6) | Set-Content -Path $file -Encoding utf8

    $marke = switch ($res.quelle) { 'live' { '[live]' } 'fixture' { '[fixture]' } default { '[leer]' } }
    $roh = @($res.events).Count
    Write-Step ("{0,-12} {1,-10} {2,3} roh -> {3,3} verdichtet  ({4})" -f $sub.Name, $marke, $roh, @($verdichtet).Count, $res.grund)
}

$inventory = Get-SystemInventory -FixtureDir $fixtures -UseFixtures:$UseFixtures
($inventory | ConvertTo-Json -Depth 6) | Set-Content -Path (Join-Path $OutDir '00_inventory.json') -Encoding utf8
Write-Step ("{0,-12} [{1}]  Uptime {2} h, {3} hängende(r) Autostart-Dienst(e)" -f 'inventar', $inventory.quelle, $inventory.uptime_stunden, @($inventory.haengende_dienste).Count)
Write-Okay 'Fakten in 00_*.json'

if (@($slices.Values | ForEach-Object { $_ }).Count -eq 0) {
    Write-Fail 'Keine Ereignisse gefunden (weder live noch Fixture). Abbruch.'
    exit 1
}

# =====================================================================================
# Stufe 10 — Fan-out: ein spezialisierter Agent PRO SUBSYSTEM, parallel
# =====================================================================================
Write-Head 'Stufe 10 — Fan-out: 4 Agenten parallel (je ein Subsystem)'

# Diese Agenten sind reine Transformatoren: Ereignisse rein (stdin), Befunde raus (JSON).
# Kein Werkzeug nötig -> --strategy plain --no-subagents. Schnell, billig, parallelisierbar.
$fanArgs = @('-p', '--provider', $Provider, '--strategy', 'plain', '--no-subagents',
    '--format', 'json', '--max-steps', '6', '--workspace', $OutDir)

$jobs = @(foreach ($sub in $subsystems) {
        [pscustomobject]@{
            Name       = $sub.Name
            Titel      = $sub.Titel
            SystemFile = Join-Path $prompts $sub.Prompt
            Stdin      = ($slices[$sub.Name] | ConvertTo-Json -Depth 6)
            OutFile    = Join-Path $OutDir ("1{0}_{1}.json" -f $subsystems.IndexOf($sub), $sub.Name)
            LogFile    = Join-Path $OutDir ("1{0}_{1}.log" -f $subsystems.IndexOf($sub), $sub.Name)
        }
    })

$parallel = $PSVersionTable.PSVersion.Major -ge 7
Write-Step $(if ($parallel) { "PowerShell 7+: die vier Agenten laufen gleichzeitig." } else { "PowerShell 5: die vier Agenten laufen nacheinander (parallel braucht pwsh 7)." })

$auftrag = 'Analysiere die Ereignisse aus stdin und liefere deine Befunde als JSON.'

$scriptBlock = {
    param($job, $ak, $fanArgs, $auftrag)
    $akArgs = $fanArgs + @('--system-file', $job.SystemFile, $auftrag)
    $out = $job.Stdin | & $ak @akArgs 2> $job.LogFile
    [pscustomobject]@{ Name = $job.Name; Titel = $job.Titel; Json = ($out -join "`n"); Exit = $LASTEXITCODE; OutFile = $job.OutFile }
}

if ($parallel) {
    $results = $jobs | ForEach-Object -ThrottleLimit 4 -Parallel {
        $job = $_
        $ak = $using:ak; $fanArgs = $using:fanArgs; $auftrag = $using:auftrag
        $akArgs = $fanArgs + @('--system-file', $job.SystemFile, $auftrag)
        $out = $job.Stdin | & $ak @akArgs 2> $job.LogFile
        [pscustomobject]@{ Name = $job.Name; Titel = $job.Titel; Json = ($out -join "`n"); Exit = $LASTEXITCODE; OutFile = $job.OutFile }
    }
}
else {
    $results = @($jobs | ForEach-Object { & $scriptBlock $_ $ak $fanArgs $auftrag })
}

$befunde = @()
foreach ($r in ($results | Sort-Object Name)) {
    if ($r.Exit -ne 0) { Write-Fail ("{0}: Agent fehlgeschlagen (Exit {1}) — siehe {2}" -f $r.Name, $r.Exit, (Split-Path -Leaf ($r.OutFile -replace '\.json$', '.log'))); continue }
    Set-Content -Path $r.OutFile -Value $r.Json -Encoding utf8
    try {
        $parsed = $r.Json | ConvertFrom-Json
        $n = @($parsed.befunde).Count
        Write-Okay ("{0,-12} {1} Befund(e)  ({2})" -f $r.Name, $n, (Split-Path -Leaf $r.OutFile))
        $befunde += [pscustomobject]@{ subsystem = $r.Name; titel = $r.Titel; ergebnis = $parsed }
    }
    catch { Write-Fail "$($r.Name): Antwort ist kein gültiges JSON." }
}

if ($befunde.Count -eq 0) { Write-Fail 'Kein Subsystem hat verwertbare Befunde geliefert. Abbruch.'; exit 1 }

# =====================================================================================
# Stufe 20 — Korrelation: was hängt zusammen, was nicht?
# =====================================================================================
Write-Head 'Stufe 20 — Korrelation (Ursachenkette)'

$korrInput = @"
### BEFUNDE DER SUBSYSTEM-AGENTEN (JSON)
$($befunde | ConvertTo-Json -Depth 8)

### SYSTEMINVENTAR (deterministisch erhoben)
$($inventory | ConvertTo-Json -Depth 6)
"@

$korrFile = Join-Path $OutDir '20_correlation.json'
$korrArgs = @('-p', '--provider', $Provider, '--strategy', 'plain', '--no-subagents',
    '--format', 'json', '--max-steps', '8', '--workspace', $OutDir,
    '--system-file', (Join-Path $prompts '20_correlate.md'),
    'Verbinde die Befunde zu Ursachenketten und trenne unabhängige Vorfälle.')

$korrJson = $korrInput | & $ak @korrArgs 2> (Join-Path $OutDir '20_correlation.log')
if ($LASTEXITCODE -ne 0) { Write-Fail "Korrelation fehlgeschlagen (Exit $LASTEXITCODE)."; exit 1 }
Set-Content -Path $korrFile -Value $korrJson -Encoding utf8
$korr = $korrJson | ConvertFrom-Json

foreach ($v in $korr.vorfaelle) {
    $farbe = switch ($v.schweregrad) { 'kritisch' { 'Red' } 'hoch' { 'Yellow' } default { 'Gray' } }
    Write-Host ("  [{0}] {1}" -f $v.schweregrad.ToUpper(), $v.titel) -ForegroundColor $farbe
    Write-Host ("        Ursache: {0}" -f $v.grundursache) -ForegroundColor DarkGray
}
Write-Okay '20_correlation.json'

# =====================================================================================
# Stufe 30 — Reparaturvorschlag UNTER --dry-run (das Sicherheitsnetz)
# =====================================================================================
Write-Head 'Stufe 30 — Reparaturvorschlag (Agent läuft unter --dry-run)'
Write-Step 'run_shell / write_file / edit_file sind für diesen Agenten No-Ops — er kann nichts verändern.'

$remInput = @"
### KORRELIERTE VORFÄLLE (JSON)
$korrJson

### SYSTEMINVENTAR
$($inventory | ConvertTo-Json -Depth 6)
"@

$remFile = Join-Path $OutDir '30_remediation.json'
$remLog = Join-Path $OutDir '30_remediation.log'
# Hier bewusst OHNE -p, dafür mit --steps: `-p` würde den Renderer stummschalten (quiet)
# und damit die Werkzeug-Spur unterdrücken — wir WOLLEN aber sehen, was --dry-run blockiert.
# stdout bleibt trotzdem sauber, dafür sorgt --format json.
$remArgs = @('--provider', $Provider, '--dry-run', '--steps', '--no-color', '-y', '--format', 'json',
    '--max-steps', '10', '--workspace', $OutDir,
    '--system-file', (Join-Path $prompts '30_remediation.md'),
    'Schlage die Reparatur als PowerShell-Skript vor. Führe nichts aus.')

$remJson = $remInput | & $ak @remArgs 2> $remLog
if ($LASTEXITCODE -ne 0) { Write-Fail "Reparatur-Stufe fehlgeschlagen (Exit $LASTEXITCODE)."; exit 1 }
Set-Content -Path $remFile -Value $remJson -Encoding utf8
$plan = $remJson | ConvertFrom-Json

# Falls der Agent doch ein veränderndes Werkzeug versucht hat: agentkit hat es blockiert
# und den Versuch auf stderr protokolliert. Das ist der Beweis, dass das Netz greift.
$blockiert = @(Select-String -Path $remLog -Pattern '\[dry-run\]' -ErrorAction SilentlyContinue)
if ($blockiert.Count -gt 0) {
    Write-Warn "$($blockiert.Count) verändernde(r) Werkzeugaufruf(e) von --dry-run blockiert (siehe 30_remediation.log):"
    $blockiert | Select-Object -First 3 | ForEach-Object { Write-Host "        $($_.Line.Trim())" -ForegroundColor DarkYellow }
}

$skriptPfad = Join-Path $OutDir 'remediation.ps1'
Write-RemediationScript -Path $skriptPfad -Plan $plan -Rechner $inventory.rechner
Write-Okay "30_remediation.json  +  remediation.ps1  (Vorschlag, NICHT ausgeführt)"

# =====================================================================================
# Stufe 40 — Bericht
# =====================================================================================
Write-Head 'Stufe 40 — Bericht (Markdown)'

$repInput = @"
### KORRELIERTE VORFÄLLE
$korrJson

### REPARATURPLAN
$remJson

### SYSTEMINVENTAR
$($inventory | ConvertTo-Json -Depth 6)
"@

$repFile = Join-Path $OutDir '40_report.md'
$repArgs = @('-p', '--provider', $Provider, '--strategy', 'plain', '--no-subagents',
    '--max-steps', '6', '--workspace', $OutDir,
    '--system-file', (Join-Path $prompts '40_report.md'),
    'Schreibe den Triage-Bericht als Markdown.')

$report = $repInput | & $ak @repArgs 2> (Join-Path $OutDir '40_report.log')
if ($LASTEXITCODE -ne 0) { Write-Fail "Bericht fehlgeschlagen (Exit $LASTEXITCODE)." }
else { Set-Content -Path $repFile -Value $report -Encoding utf8; Write-Okay '40_report.md' }

# =====================================================================================
# Zusammenfassung & menschliches Freigabe-Tor
# =====================================================================================
Write-Head 'Ergebnis'
$kritisch = @($korr.vorfaelle | Where-Object { $_.schweregrad -eq 'kritisch' })
$korr.vorfaelle | Select-Object @{n = 'Schweregrad'; e = { $_.schweregrad } }, @{n = 'Vorfall'; e = { $_.titel } },
@{n = 'Betroffen'; e = { ($_.betroffene_subsysteme -join ', ') } } | Format-Table -AutoSize | Out-String | Write-Host

Write-Host "Alle Artefakte: $OutDir" -ForegroundColor Cyan
Write-Host "Bericht:        $repFile" -ForegroundColor Cyan
Write-Host "Vorschlag:      $skriptPfad  (nicht ausgeführt)" -ForegroundColor Yellow

if ($Apply) {
    Write-Head 'Freigabe'
    Write-Host "--- $skriptPfad ---" -ForegroundColor DarkGray
    Get-Content $skriptPfad | Write-Host
    Write-Host '--- Ende ---' -ForegroundColor DarkGray
    $antwort = Read-Host "`nDieses Skript JETZT auf $($inventory.rechner) ausführen? [ja/NEIN]"
    if ($antwort -eq 'ja') {
        Write-Step 'Führe remediation.ps1 aus …'
        & pwsh -NoProfile -File $skriptPfad
        Write-Okay "remediation.ps1 beendet (Exit $LASTEXITCODE)."
    }
    else { Write-Warn 'Abgelehnt — nichts verändert.' }
}
else {
    Write-Host "`nZum Prüfen und Freigeben:  .\Invoke-WinTriage.ps1 -Apply" -ForegroundColor DarkGray
}

# Exit-Code als Signal für geplante Tasks / Monitoring.
if ($kritisch.Count -gt 0) {
    Write-Host "`n$($kritisch.Count) kritische(r) Vorfall/Vorfälle -> Exit 1 (Eskalation)." -ForegroundColor Red
    exit 1
}
exit 0
