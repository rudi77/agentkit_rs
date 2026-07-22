<#
.SYNOPSIS
    DATEV-Buchungsstapel (EXTF) aus einem Buchungsvorschlag — als komponierbares Kommando für
    Orchestrator und Batch.

.DESCRIPTION
    Leitet aus Buchungs-JSON (booker/05_booking) + §14-Merkmalen (extractor/03_fields) eine
    DATEV-EXTF-Datenzeile ab. Aktualisiert einen Sammelstapel (Kopf + alle bisherigen Zeilen)
    und schreibt optional einen Einzelstapel je Rechnung. Nicht buchbare Vorschläge (Fehler,
    Dublette) erzeugen keine Zeile. Gibt auf stdout aus:

        { "gebucht": bool, "grund": string|null, "konto": string|null, "stapel": string, "zeilen": int }

.PARAMETER BookingJson  Pfad zum Buchungs-JSON (buchung_moeglich, buchungszeilen …).
.PARAMETER FieldsJson   Pfad zur §14-Merkmale-JSON.
.PARAMETER Stapel       Pfad zum Sammelstapel-CSV (Default: out/datev_buchungsstapel.csv).
.PARAMETER RowStore     Interner Zeilenspeicher, aus dem der Sammelstapel neu erzeugt wird
                        (Default: out/_datev_rows.txt).
.PARAMETER RowOut       Optional: Pfad für einen Einzelstapel nur dieser Rechnung.

.EXAMPLE
    pwsh -File tools/datev-export.ps1 -BookingJson booking.json -FieldsJson fields.json -RowOut out/BM/datev.csv
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$BookingJson,
    [Parameter(Mandatory)] [string]$FieldsJson,
    [string]$Stapel,
    [string]$RowStore,
    [string]$RowOut
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
. (Join-Path $PSScriptRoot '..\modules\ap-helpers.ps1')

if (-not $Stapel) { $Stapel = Join-Path 'out' 'datev_buchungsstapel.csv' }
if (-not $RowStore) { $RowStore = Join-Path 'out' '_datev_rows.txt' }
$outDir = Split-Path -Parent $Stapel
if ($outDir -and -not (Test-Path $outDir)) { New-Item -ItemType Directory -Force -Path $outDir | Out-Null }

$booking = Get-Content -LiteralPath $BookingJson -Raw | ConvertFrom-Json
$fields = Get-Content -LiteralPath $FieldsJson -Raw | ConvertFrom-Json
$year = if ("$($fields.rechnungsdatum)" -match '^(\d{4})') { [int]$Matches[1] } else { (Get-Date).Year }

$row = ConvertTo-DatevRow -Booking $booking -Fields $fields

if (-not $row) {
    # Sammelstapel dennoch (neu) schreiben, damit die Datei konsistent bleibt.
    $existing = if (Test-Path $RowStore) { @(Get-Content -LiteralPath $RowStore) } else { @() }
    Write-DatevCsv -Path $Stapel -DataRows $existing -Year $year
    [pscustomobject]@{ gebucht = $false; grund = 'nicht buchbar (Fehler/Dublette/keine Buchungszeilen)'; konto = $null; stapel = $Stapel; zeilen = $existing.Count } | ConvertTo-Json -Compress
    exit 0
}

# Zeile in den Zeilenspeicher aufnehmen und Sammelstapel neu erzeugen.
Add-Content -LiteralPath $RowStore -Value $row -Encoding utf8
$allRows = @(Get-Content -LiteralPath $RowStore)
Write-DatevCsv -Path $Stapel -DataRows $allRows -Year $year
if ($RowOut) {
    $rowOutDir = Split-Path -Parent $RowOut
    if ($rowOutDir -and -not (Test-Path $rowOutDir)) { New-Item -ItemType Directory -Force -Path $rowOutDir | Out-Null }
    Write-DatevCsv -Path $RowOut -DataRows @($row) -Year $year
}

$konto = ($row -split ';')[6]
[pscustomobject]@{ gebucht = $true; grund = $null; konto = $konto; stapel = $Stapel; zeilen = $allRows.Count } | ConvertTo-Json -Compress
exit 0
