<#
.SYNOPSIS
    E-Rechnungs-Konformitätsprüfung (EN 16931) über die xcheck-API — als komponierbares
    Kommando für den Orchestrator (run_shell) UND die Batch-Pipeline.

.DESCRIPTION
    Erkennt das Format der Rechnung (xrechnung | zugferd | pdf) und ruft für E-Rechnungen die
    xcheck-API (KoSIT-Validator) auf. Gibt IMMER ein JSON-Objekt auf stdout aus:

        { "geprueft": bool, "grund": string|null, "format": string|null,
          "konform_en16931": bool|null, "syntax_valid": bool|null,
          "anzahl_findings": int, "findings": [string], "credits_remaining": int|null }

    Degradiert sauber: reine Papier-PDFs werden nicht geprüft; fehlt die xcheck-Konfiguration
    (XCheckUrl/XCheckApiKey bzw. Env XCHECK_URL/XCHECK_API_KEY), wird die Prüfung übersprungen.
    Der Rest des Prozesses läuft in beiden Fällen weiter (Exit 0).

.PARAMETER File   Pfad zur Rechnungsdatei (.pdf | .xml).
.PARAMETER Url    Basis-URL der xcheck-API (Default: Env XCHECK_URL).
.PARAMETER ApiKey API-Key (Default: Env XCHECK_API_KEY).

.EXAMPLE
    pwsh -File tools/xcheck.ps1 -File inbox/eingang_04_schreinerei-holzmann.xml
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$File,
    [string]$Url,
    [string]$ApiKey
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
. (Join-Path $PSScriptRoot '..\modules\ap-helpers.ps1')

if (-not $Url) { $Url = $env:XCHECK_URL }
if (-not $ApiKey) { $ApiKey = $env:XCHECK_API_KEY }
if (-not (Test-Path -LiteralPath $File)) { throw "Datei nicht gefunden: $File" }

$kind = Get-InvoiceKind (Resolve-Path -LiteralPath $File).Path

if ($kind -notin @('xrechnung', 'zugferd')) {
    $result = [pscustomobject]@{ geprueft = $false; grund = 'papier-/textbasierte Rechnung (keine strukturierte E-Rechnung)'; anzahl_findings = 0; findings = @() }
}
elseif (-not $Url -or -not $ApiKey) {
    $result = [pscustomobject]@{ geprueft = $false; grund = 'xcheck nicht konfiguriert (XCheckUrl/XCheckApiKey bzw. Env XCHECK_URL/XCHECK_API_KEY fehlen)'; anzahl_findings = 0; findings = @() }
}
else {
    $x = Invoke-XCheck -FilePath (Resolve-Path -LiteralPath $File).Path -Kind $kind -Url $Url -ApiKey $ApiKey
    if ($x.available) {
        $result = [pscustomobject]@{
            geprueft          = $true
            grund             = $null
            format            = $x.formatDetected
            konform_en16931   = $x.isValid
            syntax_valid      = $x.syntaxValid
            anzahl_findings   = @($x.semanticErrors).Count
            findings          = @($x.semanticErrors)
            credits_remaining = $x.creditsRemaining
        }
    }
    else {
        $result = [pscustomobject]@{ geprueft = $false; grund = $x.reason; anzahl_findings = 0; findings = @() }
    }
}

$result | ConvertTo-Json -Depth 6 -Compress
exit 0
