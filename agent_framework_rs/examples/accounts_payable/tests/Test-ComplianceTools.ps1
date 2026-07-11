<#
    Deterministische Offline-Tests der komponierbaren Compliance-Werkzeuge (tools/*.ps1), die
    sowohl die Batch-Pipeline als auch der interaktive Orchestrator nutzen. KEIN Netz, KEIN LLM.

    Prüft: check-duplicate (Erkennung + Registrierung), datev-export (Buchung -> EXTF-Zeile,
    Blockade bei nicht buchbar), gobd-manifest (schreibgeschütztes Original + SHA-256-Manifest),
    xcheck (saubere Degradierung ohne API-Konfiguration).

    Aufruf:  pwsh -File .\tests\Test-ComplianceTools.ps1
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$base = Split-Path -Parent $here
$tools = Join-Path $base 'tools'

$pass = 0; $fail = 0
function Assert($cond, [string]$name) {
    if ($cond) { $script:pass++; Write-Host "  [PASS] $name" -ForegroundColor Green }
    else { $script:fail++; Write-Host "  [FAIL] $name" -ForegroundColor Red }
}

$ws = Join-Path $env:TEMP ("aptools_test_" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force $ws | Out-Null
$out = Join-Path $ws 'out'; New-Item -ItemType Directory -Force $out | Out-Null

try {
    # --- Testdaten ---------------------------------------------------------------------
    $fields = Join-Path $ws 'fields.json'
    @'
{ "lieferant": { "name": "Buerobedarf Meier GmbH", "ust_idnr": "DE255558888" },
  "rechnungsnummer": "BM-2025-3311", "rechnungsdatum": "2025-07-02",
  "betraege": { "netto": 420.00, "steuersatz_prozent": 19, "steuerbetrag": 79.80, "brutto": 499.80, "waehrung": "EUR" } }
'@ | Set-Content -Path $fields -Encoding utf8

    $booking = Join-Path $ws 'booking.json'
    @'
{ "buchung_moeglich": true, "kontenrahmen": "SKR03", "kostenstelle": "KST-4900",
  "buchungszeilen": [
    { "konto": "4930", "bezeichnung": "Buerobedarf", "soll": 420.00, "haben": 0 },
    { "konto": "1576", "bezeichnung": "Vorsteuer 19%", "soll": 79.80, "haben": 0 },
    { "konto": "1600", "bezeichnung": "Verbindlichkeiten", "soll": 0, "haben": 499.80 } ] }
'@ | Set-Content -Path $booking -Encoding utf8

    $blocked = Join-Path $ws 'booking_blocked.json'
    '{ "buchung_moeglich": false, "grund_falls_blockiert": "Dublette", "buchungszeilen": [] }' | Set-Content -Path $blocked -Encoding utf8

    $txtInvoice = Join-Path $ws 'rechnung.txt'
    "Buerobedarf Meier GmbH`nRechnungsnummer: BM-2025-3311`nGesamtbetrag (brutto): 499,80 EUR" | Set-Content -Path $txtInvoice -Encoding utf8

    $reg = Join-Path $out '_register.json'

    Write-Host "== 1) check-duplicate =="
    $r1 = pwsh -NoProfile -File (Join-Path $tools 'check-duplicate.ps1') -FieldsJson $fields -Register $reg -Add -Name 'BM-2025-3311' | ConvertFrom-Json
    Assert ($r1.dublette -eq $false -and $r1.registriert -eq $true) 'Erste Rechnung: keine Dublette, registriert'
    $r2 = pwsh -NoProfile -File (Join-Path $tools 'check-duplicate.ps1') -FieldsJson $fields -Register $reg | ConvertFrom-Json
    Assert ($r2.dublette -eq $true -and $r2.bezug -eq 'BM-2025-3311') 'Zweite Prüfung: Dublette erkannt (Bezug korrekt)'

    Write-Host "== 2) datev-export =="
    $stapel = Join-Path $out 'datev_buchungsstapel.csv'
    $store = Join-Path $out '_datev_rows.txt'
    $d1 = pwsh -NoProfile -File (Join-Path $tools 'datev-export.ps1') -BookingJson $booking -FieldsJson $fields -Stapel $stapel -RowStore $store | ConvertFrom-Json
    Assert ($d1.gebucht -eq $true -and $d1.konto -eq '4930' -and $d1.zeilen -eq 1) 'Buchbare Rechnung -> DATEV-Zeile (Konto 4930)'
    $csv = Get-Content $stapel
    Assert ($csv.Count -eq 3 -and $csv[0] -match '^"EXTF"' -and ($csv[2] -split ';')[6] -eq '4930') 'Sammelstapel: Kopf + Spalten + 1 Buchungszeile'
    Assert ($csv[2] -match '499,80' -and $csv[2] -match ';9;') 'DATEV-Zeile: Bruttobetrag + BU-Schlüssel 9 (19% Vorsteuer)'
    $d2 = pwsh -NoProfile -File (Join-Path $tools 'datev-export.ps1') -BookingJson $blocked -FieldsJson $fields -Stapel $stapel -RowStore $store | ConvertFrom-Json
    Assert ($d2.gebucht -eq $false) 'Nicht buchbarer Vorschlag erzeugt keine DATEV-Zeile'

    Write-Host "== 3) gobd-manifest =="
    $resdir = Join-Path $out 'BM-2025-3311'
    $g = pwsh -NoProfile -File (Join-Path $tools 'gobd-manifest.ps1') -Source $txtInvoice -Dir $resdir | ConvertFrom-Json
    $src = Join-Path $resdir '00_source.txt'
    $manifest = Join-Path $resdir 'manifest.json'
    Assert ((Test-Path $src) -and (Get-Item $src).IsReadOnly) 'Original schreibgeschützt als 00_source.txt abgelegt'
    Assert (Test-Path $manifest) 'manifest.json geschrieben'
    $m = Get-Content $manifest -Raw | ConvertFrom-Json
    $entry = @($m.artefakte) | Where-Object { $_.datei -eq '00_source.txt' } | Select-Object -First 1
    Assert ($entry -and $entry.sha256 -match '^[0-9a-f]{64}$') 'Manifest enthält SHA-256 des Originals'
    Assert (($m.aufbewahrung_bis_jahr - (Get-Date).Year) -eq 10) 'Aufbewahrungsfrist 10 Jahre'

    Write-Host "== 4) xcheck (Degradierung ohne Konfiguration) =="
    $env:XCHECK_URL = $null; $env:XCHECK_API_KEY = $null
    $x = pwsh -NoProfile -File (Join-Path $tools 'xcheck.ps1') -File $txtInvoice | ConvertFrom-Json
    Assert ($x.geprueft -eq $false -and $x.grund) 'Text-/Papierrechnung: sauber übersprungen (kein Fehler)'
}
finally {
    Get-ChildItem $ws -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { try { $_.IsReadOnly = $false } catch {} }
    Remove-Item $ws -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host ("Ergebnis: {0} PASS, {1} FAIL" -f $pass, $fail) -ForegroundColor $(if ($fail -eq 0) { 'Green' } else { 'Red' })
if ($fail -gt 0) { exit 1 }
