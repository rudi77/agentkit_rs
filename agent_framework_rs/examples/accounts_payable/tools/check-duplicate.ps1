<#
.SYNOPSIS
    Dublettenprüfung gegen ein Register — als komponierbares Kommando für Orchestrator und
    Batch. Verhindert Doppelbuchungen anhand Rechnungsnummer + Lieferant + Bruttobetrag.

.DESCRIPTION
    Liest die extrahierten §14-Merkmale (JSON) und bildet daraus einen Dublettenschlüssel.
    Prüft ihn gegen das Register (JSON-Array) und gibt auf stdout aus:

        { "dublette": bool, "bezug": string|null, "schluessel": string }

    Mit -Add wird die Rechnung nach erfolgreicher (nicht-dubletten-)Verarbeitung ins Register
    aufgenommen, sodass künftige Läufe sie als Dublette erkennen.

.PARAMETER FieldsJson  Pfad zur §14-Merkmale-JSON (Ausgabe des extractor) — ODER siehe Einzelfelder.
.PARAMETER Register    Pfad zum Register (Default: out/_register.json).
.PARAMETER Add         Nach der Prüfung ins Register aufnehmen (nur wenn keine Dublette).
.PARAMETER Name        Bezeichner der Rechnung fürs Register (Default: Rechnungsnummer).

.EXAMPLE
    pwsh -File tools/check-duplicate.ps1 -FieldsJson out/BM-2025-3311/fields.json -Register out/_register.json
    pwsh -File tools/check-duplicate.ps1 -FieldsJson fields.json -Register out/_register.json -Add -Name BM-2025-3311
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$FieldsJson,
    [string]$Register,
    [switch]$Add,
    [string]$Name
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
. (Join-Path $PSScriptRoot '..\modules\ap-helpers.ps1')

if (-not $Register) { $Register = Join-Path 'out' '_register.json' }
if (-not (Test-Path -LiteralPath $FieldsJson)) { throw "Merkmale-JSON nicht gefunden: $FieldsJson" }
$fields = Get-Content -LiteralPath $FieldsJson -Raw | ConvertFrom-Json

$key = Get-InvoiceKey -Fields $fields
$dup = Find-Duplicate -RegisterPath $Register -Key $key
$isDup = [bool]$dup

if ($Add -and -not $isDup) {
    $regDir = Split-Path -Parent $Register
    if ($regDir -and -not (Test-Path $regDir)) { New-Item -ItemType Directory -Force -Path $regDir | Out-Null }
    $n = if ($Name) { $Name } elseif ($fields.rechnungsnummer) { [string]$fields.rechnungsnummer } else { 'rechnung' }
    Add-ToRegister -RegisterPath $Register -Key $key -Name $n -Fields $fields
}

[pscustomobject]@{
    dublette   = $isDup
    bezug      = $(if ($isDup) { $dup.rechnung } else { $null })
    schluessel = $key
    registriert = [bool]($Add -and -not $isDup)
} | ConvertTo-Json -Depth 5 -Compress
exit 0
