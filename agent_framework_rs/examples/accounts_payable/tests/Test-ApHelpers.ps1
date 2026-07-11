<#
    Deterministische Offline-Tests für die AP-Pipeline-Helfer (ap-helpers.ps1).
    Kein LLM, kein xcheck, kein Netz — prüft Klassifizierung, xcheck-Graceful-Degradation,
    DATEV-Ableitung, Dubletten-Register und die Integrität des GoBD-Manifests.

    Aufruf:  pwsh -File .\tests\Test-ApHelpers.ps1     (Exit 0 = alle grün)
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path (Split-Path -Parent $here) 'modules\ap-helpers.ps1')

$script:pass = 0; $script:fail = 0
function Assert($cond, [string]$name) {
    if ($cond) { $script:pass++; Write-Host "  [PASS] $name" -ForegroundColor Green }
    else { $script:fail++; Write-Host "  [FAIL] $name" -ForegroundColor Red }
}

$tmp = Join-Path $env:TEMP ("aptest_" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    Write-Host "== Get-InvoiceKind =="
    # XRechnung (.xml)
    $xmlF = Join-Path $tmp 'a.xml'; Set-Content $xmlF '<Invoice/>' -Encoding utf8
    Assert ((Get-InvoiceKind $xmlF) -eq 'xrechnung') 'xml -> xrechnung'
    # ZUGFeRD: PDF-Magic + /EmbeddedFile-Marker
    $zf = Join-Path $tmp 'z.pdf'
    [IO.File]::WriteAllBytes($zf, [Text.Encoding]::ASCII.GetBytes("%PDF-1.4`n/EmbeddedFile stream <x/> endstream`n%%EOF"))
    Assert ((Get-InvoiceKind $zf) -eq 'zugferd') 'pdf+/EmbeddedFile -> zugferd'
    # Plain PDF
    $pp = Join-Path $tmp 'p.pdf'
    [IO.File]::WriteAllBytes($pp, [Text.Encoding]::ASCII.GetBytes("%PDF-1.4`nnur text`n%%EOF"))
    Assert ((Get-InvoiceKind $pp) -eq 'pdf') 'plain pdf -> pdf'

    Write-Host "== Test-BytesContain =="
    $b = [Text.Encoding]::ASCII.GetBytes('hallo /EmbeddedFile welt')
    Assert (Test-BytesContain -Haystack $b -Needle '/EmbeddedFile') 'Marker gefunden'
    Assert (-not (Test-BytesContain -Haystack $b -Needle '/NichtDa')) 'Nicht-Marker nicht gefunden'

    Write-Host "== Invoke-XCheck (Graceful ohne Konfiguration) =="
    $r = Invoke-XCheck -FilePath $xmlF -Kind 'xrechnung' -Url '' -ApiKey ''
    Assert (-not $r.available -and $r.reason) 'nicht konfiguriert -> available=false + Grund'

    Write-Host "== ConvertTo-DatevRow =="
    $booking = '{"buchung_moeglich":true,"buchungszeilen":[{"konto":"4930","soll":100.0,"haben":0},{"konto":"1576","soll":19.0,"haben":0},{"konto":"1600","soll":0,"haben":119.0}]}' | ConvertFrom-Json
    $fields = '{"rechnungsnummer":"R-1","rechnungsdatum":"2025-03-08","lieferant":{"name":"Muster"},"betraege":{"brutto":119.0,"steuersatz_prozent":19}}' | ConvertFrom-Json
    $row = ConvertTo-DatevRow -Booking $booking -Fields $fields
    $cols = $row -split ';'
    Assert ($cols[0] -eq '119,00') 'Umsatz deutsch formatiert'
    Assert ($cols[1] -eq 'S') 'Soll/Haben = S'
    Assert ($cols[6] -eq '4930') 'Konto = Aufwand 4930'
    Assert ($cols[7] -eq '1600') 'Gegenkonto = Kreditor 1600'
    Assert ($cols[8] -eq '9') 'BU-Schlüssel 9 (19%)'
    Assert ($cols[9] -eq '0803') 'Belegdatum TTMM'
    # Blockierte Buchung -> keine Zeile
    $blocked = '{"buchung_moeglich":false,"buchungszeilen":[]}' | ConvertFrom-Json
    Assert ($null -eq (ConvertTo-DatevRow -Booking $blocked -Fields $fields)) 'blockiert -> keine DATEV-Zeile'
    # 7% -> BU 8
    $b7 = '{"buchung_moeglich":true,"buchungszeilen":[{"konto":"3300","soll":100.0,"haben":0},{"konto":"1571","soll":7.0,"haben":0},{"konto":"1600","soll":0,"haben":107.0}]}' | ConvertFrom-Json
    $f7 = '{"rechnungsnummer":"R-2","rechnungsdatum":"2025-03-08","lieferant":{"name":"M"},"betraege":{"brutto":107.0,"steuersatz_prozent":7}}' | ConvertFrom-Json
    Assert ((($row2 = ConvertTo-DatevRow -Booking $b7 -Fields $f7) -split ';')[8] -eq '8') 'BU-Schlüssel 8 (7%)'

    Write-Host "== Dublettenprüfung (Register) =="
    $reg = Join-Path $tmp 'reg.json'
    $key = Get-InvoiceKey -Fields $fields
    Assert (-not (Find-Duplicate -RegisterPath $reg -Key $key)) 'vor Add: keine Dublette'
    Add-ToRegister -RegisterPath $reg -Key $key -Name 'inv1' -Fields $fields
    Assert ([bool](Find-Duplicate -RegisterPath $reg -Key $key)) 'nach Add: Dublette erkannt'
    Assert ((Get-InvoiceKey -Fields $fields) -eq (Get-InvoiceKey -Fields $fields)) 'Key deterministisch'
    Assert ((Get-InvoiceKey -Fields $f7) -ne $key) 'andere Rechnung -> anderer Key'

    Write-Host "== GoBD-Manifest (Hash-Integrität) =="
    $gdir = Join-Path $tmp 'g'; New-Item -ItemType Directory -Force $gdir | Out-Null
    Set-Content (Join-Path $gdir '00_source.xml') 'ORIGINAL' -Encoding utf8
    Set-Content (Join-Path $gdir '03_fields.json') '{"a":1}' -Encoding utf8
    New-GobdManifest -Dir $gdir -OriginalName 'x.xml' -Kind 'xrechnung'
    $man = Get-Content (Join-Path $gdir 'manifest.json') -Raw | ConvertFrom-Json
    Assert ($man.aufbewahrung_bis_jahr -eq ((Get-Date).Year + 10)) 'Aufbewahrung = Jahr + 10'
    Assert ($man.artefakte.Count -eq 2) 'Manifest listet 2 Artefakte (ohne manifest.json)'
    $okHash = $true
    foreach ($a in $man.artefakte) {
        $real = (Get-FileHash (Join-Path $gdir $a.datei) -Algorithm SHA256).Hash.ToLower()
        if ($real -ne $a.sha256) { $okHash = $false }
    }
    Assert $okHash 'alle SHA-256 im Manifest stimmen mit den Dateien überein'
}
finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host ("Ergebnis: {0} PASS, {1} FAIL" -f $script:pass, $script:fail) -ForegroundColor $(if ($script:fail -eq 0) { 'Green' } else { 'Red' })
if ($script:fail -gt 0) { exit 1 }
