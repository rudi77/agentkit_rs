<#
.SYNOPSIS
    GoBD-konforme Ablage: Original schreibgeschützt archivieren und ein SHA-256-Manifest über
    alle Artefakte eines Ergebnis-Ordners schreiben. Komponierbares Kommando für Orchestrator
    und Batch.

.DESCRIPTION
    Kopiert die Originalrechnung unveränderbar (schreibgeschützt) als `00_source.<ext>` in den
    Ergebnis-Ordner und erzeugt `manifest.json` mit SHA-256 je Datei + Aufbewahrungshinweis
    (10 Jahre). Legt der Orchestrator vorher weitere Artefakte (Merkmale, Report …) in denselben
    Ordner, werden sie mit gehasht. Gibt auf stdout aus:

        { "format": string, "artefakte": int, "manifest": string }

.PARAMETER Source  Pfad zur Originalrechnung (.pdf | .xml | .txt).
.PARAMETER Dir     Ergebnis-Ordner, in dem archiviert und das Manifest geschrieben wird
                   (z. B. out/BM-2025-3311). Wird bei Bedarf angelegt.

.EXAMPLE
    pwsh -File tools/gobd-manifest.ps1 -Source inbox/rechnung_meier.txt -Dir out/BM-2025-3311
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$Source,
    [Parameter(Mandatory)] [string]$Dir
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
. (Join-Path $PSScriptRoot '..\modules\ap-helpers.ps1')

if (-not (Test-Path -LiteralPath $Source)) { throw "Original nicht gefunden: $Source" }
New-Item -ItemType Directory -Force -Path $Dir | Out-Null

$src = Get-Item -LiteralPath $Source
$kind = Get-InvoiceKind $src.FullName
$ext = $src.Extension.ToLower()
$srcOut = Join-Path $Dir ('00_source' + $ext)

# Vorhandene schreibgeschützte Kopie zunächst freigeben (idempotent), dann neu ablegen.
if (Test-Path -LiteralPath $srcOut) { (Get-Item -LiteralPath $srcOut).IsReadOnly = $false }
Copy-Item -LiteralPath $src.FullName -Destination $srcOut -Force
Set-ItemProperty -LiteralPath $srcOut -Name IsReadOnly -Value $true

New-GobdManifest -Dir $Dir -OriginalName $src.Name -Kind $kind

$count = @(Get-ChildItem -Path $Dir -File | Where-Object { $_.Name -ne 'manifest.json' }).Count
[pscustomobject]@{ format = $kind; artefakte = $count; manifest = (Join-Path $Dir 'manifest.json') } | ConvertTo-Json -Compress
exit 0
