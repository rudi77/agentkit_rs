<#
.SYNOPSIS
    Deterministische Offline-Tests der logwatch-Helfer — kein LLM, kein Netz.

.DESCRIPTION
    Geprüft wird alles, was OHNE Modell prüfbar ist: die Offset-Verwaltung (was ist neu seit
    dem letzten Lauf?), die Logtyp-Erkennung (welcher Skill?) und das Gedächtnis-Format
    (schreibt die Pipeline JSONL, das agentkits LongTermMemory auch wirklich lesen kann?).

    Ob der Agent RICHTIG urteilt, lässt sich hier nicht prüfen — das zeigt `-Demo`.

.EXAMPLE
    pwsh -File .\tests\Test-LogWatch.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
. (Join-Path $root 'modules\logwatch-helpers.ps1')

$fixtures = Join-Path $root 'fixtures'
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("logwatch_test_{0}" -f [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$fehler = 0; $ok = 0
function Check([string]$name, [scriptblock]$pruefung) {
    try {
        if (& $pruefung) { Write-Host "  [ok]   $name" -ForegroundColor Green; $script:ok++ }
        else { Write-Host "  [FAIL] $name" -ForegroundColor Red; $script:fehler++ }
    }
    catch { Write-Host "  [FAIL] $name — $($_.Exception.Message)" -ForegroundColor Red; $script:fehler++ }
}

Write-Host "`n=== logwatch-Helfer (offline) ===" -ForegroundColor Cyan

if (-not (Test-Path (Join-Path $fixtures 'iis_tag1.log'))) {
    Write-Host "  Fixtures fehlen — erzeuge sie …" -ForegroundColor Yellow
    & (Join-Path $root 'tools\Build-Fixtures.ps1') | Out-Null
}

# --- Logtyp-Erkennung: bestimmt, welchen Skill der Agent lädt ------------------------
Check 'IIS-Log wird erkannt' { (Get-LogType -Path (Join-Path $fixtures 'iis_tag1.log')) -eq 'iis' }
Check 'PostgreSQL-Log wird erkannt' { (Get-LogType -Path (Join-Path $fixtures 'postgres_tag1.log')) -eq 'postgres' }

Check 'Unbekannter Logtyp wird als solcher gemeldet (kein Raten)' {
    $f = Join-Path $tmp 'irgendwas.log'
    Set-Content -Path $f -Value @('hallo welt', 'noch eine zeile') -Encoding utf8
    (Get-LogType -Path $f) -eq 'unbekannt'
}

# --- Offsets: das `tail -f`-Verhalten ------------------------------------------------
$log = Join-Path $tmp 'u_ex_test.log'
Set-Content -Path $log -Value @('#Fields: a b c', 'zeile1', 'zeile2', 'zeile3') -Encoding utf8
$store = @{}

$s1 = Get-NewLines -Path $log -Store $store
Check 'Erster Lauf liefert ALLE Zeilen' { @($s1.neu).Count -eq 4 -and $s1.ab_zeile -eq 1 }

$store[$s1.datei] = $s1.gesamt
$s2 = Get-NewLines -Path $log -Store $store
Check 'Zweiter Lauf ohne neue Zeilen liefert nichts' { @($s2.neu).Count -eq 0 }

Add-Content -Path $log -Value 'zeile4' -Encoding utf8
$s3 = Get-NewLines -Path $log -Store $store
Check 'Nur die HINZUGEKOMMENE Zeile wird geliefert' { @($s3.neu).Count -eq 1 -and $s3.neu[0] -eq 'zeile4' }

$s4 = Get-NewLines -Path $log -Store $store -Replay
Check '-Replay liest die Datei wieder von vorn' { @($s4.neu).Count -eq 5 }

Set-Content -Path $log -Value @('neu1') -Encoding utf8   # Logrotation: Datei schrumpft
$s5 = Get-NewLines -Path $log -Store $store
Check 'Logrotation (Datei schrumpft) wird erkannt -> von vorn' { @($s5.neu).Count -eq 1 -and $s5.neu[0] -eq 'neu1' }

Check 'Offsets überleben einen Neustart (Datei-Roundtrip)' {
    $sd = Join-Path $tmp 'state'
    Save-OffsetStore -StateDir $sd -Store @{ 'a.log' = 42 }
    $wieder = Get-OffsetStore -StateDir $sd
    $wieder['a.log'] -eq 42
}

Check 'Kommentarzeilen (IIS-Header) werden entfernt' {
    $z = Remove-LogComments -Lines @('#Fields: x', 'daten1', '', '#noch ein kommentar', 'daten2')
    @($z).Count -eq 2 -and $z[0] -eq 'daten1'
}

# --- Gedächtnis-Format: muss zu agentkits LongTermMemory passen ----------------------
# src/memory.rs erwartet je Zeile ein JSON-Objekt mit `text` (String) und `tags` (Liste).
$mem = Join-Path $tmp 'known.jsonl'

Check 'Rausch-Seed legt Einträge an' { (Initialize-NoiseMemory -MemoryPath $mem) -eq 5 }
Check 'Rausch-Seed läuft nicht doppelt' { (Initialize-NoiseMemory -MemoryPath $mem) -eq 0 }

Check 'Neue Befunde werden ins Gedächtnis geschrieben' {
    $b = @([pscustomobject]@{ signatur = 'HTTP 500.19 /api/v2/orders'; was = 'Route liefert 500'; ausgang = 'Fehler 500.19' })
    (Add-ToMemory -MemoryPath $mem -Befunde $b -Datum '2026-07-11') -eq 1
}

Check 'Jede Zeile ist gültiges JSON mit text + tags (LongTermMemory-Format)' {
    $zeilen = @(Get-Content $mem)
    $zeilen.Count -eq 6 -and @($zeilen | ForEach-Object {
            $o = $_ | ConvertFrom-Json
            if (($o.text -is [string]) -and $o.text -and ($null -ne $o.tags)) { $true }
        }).Count -eq 6
}

Check 'Die Signaturwörter landen als Tags (damit recall sie findet)' {
    $letzte = (Get-Content $mem)[-1] | ConvertFrom-Json
    ($letzte.tags -contains '500.19') -and ($letzte.tags -contains '/api/v2/orders')
}

Check 'Befunde ohne Signatur werden übersprungen (kein Müll im Gedächtnis)' {
    $vorher = @(Get-Content $mem).Count
    $n = Add-ToMemory -MemoryPath $mem -Befunde @([pscustomobject]@{ was = 'ohne signatur' }) -Datum '2026-07-11'
    $n -eq 0 -and @(Get-Content $mem).Count -eq $vorher
}

Check 'Leere Befundliste schreibt nichts' {
    (Add-ToMemory -MemoryPath $mem -Befunde @() -Datum '2026-07-11') -eq 0
}

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue

Write-Host ""
if ($fehler -eq 0) { Write-Host "Alle $ok Prüfungen bestanden." -ForegroundColor Green; exit 0 }
Write-Host "$fehler von $($ok + $fehler) Prüfungen fehlgeschlagen." -ForegroundColor Red
exit 1
