<#
    Integrationstest gegen die LIVE xcheck-E-Rechnungs-API (EN 16931).
    Wird ÜBERSPRUNGEN, wenn XCHECK_URL / XCHECK_API_KEY nicht gesetzt sind — analog zu
    xchecks eigenen KoSIT-Live-Tests. Setzt voraus, dass die Beispielrechnungen existieren
    (sonst vorher .\tools\Build-Samples.ps1).

    Aufruf:
      $env:XCHECK_URL='http://localhost:5080'; $env:XCHECK_API_KEY='inv_port_...'
      pwsh -File .\tests\Test-XCheckIntegration.ps1
#>
[CmdletBinding()]
param([string]$XCheckUrl = $env:XCHECK_URL, [string]$XCheckApiKey = $env:XCHECK_API_KEY)
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
. (Join-Path $root 'modules\ap-helpers.ps1')

if (-not $XCheckUrl -or -not $XCheckApiKey) {
    Write-Host "ÜBERSPRUNGEN: XCHECK_URL / XCHECK_API_KEY nicht gesetzt." -ForegroundColor Yellow
    exit 0
}

$inbox = Join-Path $root 'inbox'
$xr = Join-Path $inbox 'rechnung_xrechnung.xml'
$zf = Join-Path $inbox 'rechnung_zugferd.pdf'
if (-not (Test-Path $xr) -or -not (Test-Path $zf)) {
    Write-Host "Beispielrechnungen fehlen — erst .\tools\Build-Samples.ps1 ausführen." -ForegroundColor Red
    exit 1
}

$pass = 0; $fail = 0
function Assert($cond, [string]$name) {
    if ($cond) { $script:pass++; Write-Host "  [PASS] $name" -ForegroundColor Green }
    else { $script:fail++; Write-Host "  [FAIL] $name" -ForegroundColor Red }
}

Write-Host "== xcheck LIVE @ $XCheckUrl =="
$rx = Invoke-XCheck -FilePath $xr -Kind 'xrechnung' -Url $XCheckUrl -ApiKey $XCheckApiKey
Assert ($rx.available) 'XRechnung: API erreichbar'
Assert ($rx.formatDetected -in @('CII', 'UBL')) "XRechnung: Format erkannt ($($rx.formatDetected))"
Assert ($rx.isValid) 'XRechnung: EN-16931-konform (isValid)'

$rz = Invoke-XCheck -FilePath $zf -Kind 'zugferd' -Url $XCheckUrl -ApiKey $XCheckApiKey
Assert ($rz.available) 'ZUGFeRD: API erreichbar'
Assert ($rz.formatDetected -eq 'ZUGFeRD') 'ZUGFeRD: eingebettetes XML erkannt (Format ZUGFeRD)'
Assert ($rz.isValid) 'ZUGFeRD: EN-16931-konform (isValid)'

Write-Host ""
Write-Host ("Ergebnis: {0} PASS, {1} FAIL" -f $pass, $fail) -ForegroundColor $(if ($fail -eq 0) { 'Green' } else { 'Red' })
if ($fail -gt 0) { exit 1 }
