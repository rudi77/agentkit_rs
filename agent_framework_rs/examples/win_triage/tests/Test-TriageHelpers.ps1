<#
.SYNOPSIS
    Deterministische Offline-Tests der Triage-Helfer — kein LLM, kein Netz, kein Event-Log.

.DESCRIPTION
    Getestet wird alles, was OHNE Modell prüfbar ist: der Fixture-Fallback, die Normalisierung
    und vor allem die Verdichtung (aus 96 identischen Anmeldeversuchen wird EIN Befund mit
    anzahl=96). Die Urteilsfähigkeit der Agenten lässt sich so nicht prüfen — die zeigt der
    Lauf mit `-UseFixtures`.

.EXAMPLE
    pwsh -File .\tests\Test-TriageHelpers.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
. (Join-Path $root 'modules\triage-helpers.ps1')

$fixtures = Join-Path $root 'fixtures'
$fehler = 0
$ok = 0

function Check([string]$name, [scriptblock]$pruefung) {
    try {
        $ergebnis = & $pruefung
        if ($ergebnis) { Write-Host "  [ok]   $name" -ForegroundColor Green; $script:ok++ }
        else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:fehler++ }
    }
    catch {
        Write-Host "  [FAIL] $name — $($_.Exception.Message)" -ForegroundColor Red
        $script:fehler++
    }
}

Write-Host "`n=== Triage-Helfer (offline) ===" -ForegroundColor Cyan

if (-not (Test-Path (Join-Path $fixtures 'system.json'))) {
    Write-Host "  Fixtures fehlen — erzeuge sie …" -ForegroundColor Yellow
    & (Join-Path $root 'tools\Build-Fixtures.ps1') | Out-Null
}

$subs = Get-TriageSubsystems

Check 'Vier Subsysteme definiert' { @($subs).Count -eq 4 }

Check 'Jedes Subsystem hat Prompt und Fixture' {
    @($subs | Where-Object { $_.Prompt -and $_.Fixture }).Count -eq 4
}

# --- Fixture-Fallback ---------------------------------------------------------------
$sys = Get-TriageEvents -Subsystem ($subs | Where-Object Name -eq 'system') -FixtureDir $fixtures -UseFixtures

Check 'Fixture-Fallback liefert Ereignisse' { $sys.quelle -eq 'fixture' -and @($sys.events).Count -gt 0 }

Check 'Ereignisse tragen die normalisierten Felder' {
    $e = $sys.events[0]
    $null -ne $e.zeit -and $null -ne $e.id -and $null -ne $e.quelle -and $null -ne $e.text
}

Check 'Fehlende Fixture wird sauber gemeldet (kein Absturz)' {
    $leer = Get-TriageEvents -Subsystem ([pscustomobject]@{ Name = 'x'; Log = 'X'; Fixture = 'gibtsnicht.json' }) `
        -FixtureDir $fixtures -UseFixtures
    $leer.quelle -eq 'leer' -and @($leer.events).Count -eq 0
}

# --- Verdichtung: der eigentliche Trick ---------------------------------------------
$sec = Get-TriageEvents -Subsystem ($subs | Where-Object Name -eq 'security') -FixtureDir $fixtures -UseFixtures
$secRoh = @($sec.events).Count
$secDicht = @(Compress-TriageEvents -Events $sec.events)

Check "Security: $secRoh Rohereignisse werden verdichtet" { $secDicht.Count -lt $secRoh }

Check 'Die 96 Anmeldeversuche werden zu EINEM Befund (anzahl=96)' {
    $bruteforce = $secDicht | Where-Object { $_.id -eq 4625 }
    @($bruteforce).Count -eq 1 -and $bruteforce.anzahl -eq 96
}

Check 'Der verdichtete Befund trägt Beginn UND Ende' {
    $bruteforce = $secDicht | Where-Object { $_.id -eq 4625 }
    $bruteforce.zeit -lt $bruteforce.bis
}

Check 'Die Kontosperrung (4740) bleibt ein eigener Befund' {
    @($secDicht | Where-Object { $_.id -eq 4740 }).Count -eq 1
}

$app = Get-TriageEvents -Subsystem ($subs | Where-Object Name -eq 'application') -FixtureDir $fixtures -UseFixtures
$appDicht = @(Compress-TriageEvents -Events $app.events)

Check 'Die 43 Anwendungsabstürze werden zu EINEM Befund (anzahl=43)' {
    $crash = $appDicht | Where-Object { $_.id -eq 1000 -and $_.quelle -eq 'Application Error' }
    @($crash).Count -eq 1 -and $crash.anzahl -eq 43
}

Check 'Einzelereignisse behalten anzahl=1 und bis=$null' {
    $einzeln = $appDicht | Where-Object { $_.quelle -eq 'Microsoft-Windows-Defrag' }
    $einzeln.anzahl -eq 1 -and $null -eq $einzeln.bis
}

Check 'Verdichtung ist chronologisch sortiert' {
    $zeiten = $appDicht | ForEach-Object { $_.zeit }
    ($zeiten -join '|') -eq (($zeiten | Sort-Object) -join '|')
}

Check 'Leere Eingabe verdichtet zu leerer Liste (kein Absturz)' {
    @(Compress-TriageEvents -Events @()).Count -eq 0
}

# --- Inventar ------------------------------------------------------------------------
$inv = Get-SystemInventory -FixtureDir $fixtures -UseFixtures

Check 'Inventar liefert die Rückkopplung: volles C: + großes Abbild' {
    $c = $inv.volumes | Where-Object laufwerk -eq 'C:'
    $c.frei_prozent -lt 5 -and @($inv.absturzabbilder).Count -ge 1 -and $inv.absturzabbilder[0].groesse_gb -gt 5
}

Check 'Inventar nennt die hängenden Autostart-Dienste' {
    @($inv.haengende_dienste).Count -eq 2
}

# --- Skript-Ablage -------------------------------------------------------------------
Check 'Reparaturskript bekommt den Warnkopf und wird NICHT ausgeführt' {
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("triage_test_{0}.ps1" -f [guid]::NewGuid())
    $plan = [pscustomobject]@{ risiko = 'niedrig'; skript = "Write-Host 'hallo'" }
    Write-RemediationScript -Path $tmp -Plan $plan -Rechner 'TEST-01'
    $inhalt = Get-Content $tmp -Raw
    Remove-Item $tmp -Force
    $inhalt -match 'NICHT AUTOMATISCH AUSGEFUEHRT|NICHT AUTOMATISCH AUSGEFÜHRT' -and $inhalt -match 'dry-run' -and $inhalt -match "Write-Host 'hallo'"
}

# --- Ergebnis ------------------------------------------------------------------------
Write-Host ""
if ($fehler -eq 0) {
    Write-Host "Alle $ok Prüfungen bestanden." -ForegroundColor Green
    exit 0
}
Write-Host "$fehler von $($ok + $fehler) Prüfungen fehlgeschlagen." -ForegroundColor Red
exit 1
