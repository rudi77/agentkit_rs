<#
    Hilfsfunktionen für die Accounts-Payable-Pipeline (dot-sourced von Invoke-Ap.ps1 und tools/*.ps1).
    Reines PowerShell — E-Rechnungs-Erkennung, xcheck-Aufruf (EN 16931), GoBD-Manifest,
    DATEV-Buchungsstapel-Export und Dublettenprüfung.
#>

# --- .env laden (Werte aus Datei haben Vorrang) --------------------------------------
function Import-DotEnv {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return $false }
    foreach ($line in Get-Content -Path $Path) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('#')) { continue }
        $kv = $t -split '=', 2
        if ($kv.Count -ne 2) { continue }
        Set-Item -Path ("Env:" + $kv[0].Trim()) -Value ($kv[1].Trim().Trim('"').Trim("'"))
    }
    return $true
}

# --- Byte-Suche (für /EmbeddedFile-Marker in PDFs) -----------------------------------
function Test-BytesContain {
    param([byte[]]$Haystack, [string]$Needle)
    $n = [System.Text.Encoding]::ASCII.GetBytes($Needle)
    $limit = $Haystack.Length - $n.Length
    for ($i = 0; $i -le $limit; $i++) {
        $ok = $true
        for ($j = 0; $j -lt $n.Length; $j++) {
            if ($Haystack[$i + $j] -ne $n[$j]) { $ok = $false; break }
        }
        if ($ok) { return $true }
    }
    return $false
}

# --- Eingangsformat bestimmen: xrechnung | zugferd | pdf | text ----------------------
# `text` = formlose Text-/Papierrechnung (.txt) — nur der interaktive Orchestrator sieht sie;
# die Batch-Pipeline verarbeitet ausschließlich *.pdf/*.xml.
function Get-InvoiceKind {
    param([string]$Path)
    $ext = [System.IO.Path]::GetExtension($Path).ToLower()
    if ($ext -eq '.xml') { return 'xrechnung' }
    if ($ext -eq '.txt') { return 'text' }
    if ($ext -eq '.pdf') {
        $bytes = [System.IO.File]::ReadAllBytes($Path)
        if (Test-BytesContain -Haystack $bytes -Needle '/EmbeddedFile') { return 'zugferd' }
        return 'pdf'
    }
    return 'pdf'
}

# --- EN-16931-Konformitätsprüfung über die xcheck-API --------------------------------
# Liefert immer ein Objekt mit .available (bool). Bei available=$true zusätzlich
# isValid/formatDetected/syntaxValid/semanticErrors/creditsRemaining.
function Invoke-XCheck {
    param(
        [string]$FilePath,
        [string]$Kind,
        [string]$Url,
        [string]$ApiKey
    )
    if (-not $Url -or -not $ApiKey) {
        return [pscustomobject]@{ available = $false; reason = 'xcheck nicht konfiguriert (XCheckUrl/XCheckApiKey fehlen)' }
    }
    $endpoint = ($Url.TrimEnd('/')) + '/api/v1/validate'
    try {
        if ($Kind -eq 'xrechnung') {
            $resp = Invoke-RestMethod -Method Post -Uri $endpoint -Headers @{ 'x-api-key' = $ApiKey } `
                -ContentType 'application/xml' -InFile $FilePath
        }
        else {
            $resp = Invoke-RestMethod -Method Post -Uri $endpoint -Headers @{ 'x-api-key' = $ApiKey } `
                -Form @{ file = Get-Item -LiteralPath $FilePath }
        }
        return [pscustomobject]@{
            available        = $true
            isValid          = [bool]$resp.isValid
            formatDetected   = [string]$resp.formatDetected
            syntaxValid      = [bool]$resp.syntaxValid
            semanticErrors   = @($resp.semanticErrors)
            creditsRemaining = $resp.meta.creditsRemaining
        }
    }
    catch {
        return [pscustomobject]@{ available = $false; reason = "xcheck-Aufruf fehlgeschlagen: $($_.Exception.Message)" }
    }
}

# --- GoBD: unveränderbare Ablage — SHA-256 je Artefakt + Manifest --------------------
function New-GobdManifest {
    param(
        [string]$Dir,
        [string]$OriginalName,
        [string]$Kind
    )
    $artefakte = Get-ChildItem -Path $Dir -File | Where-Object { $_.Name -ne 'manifest.json' } |
        Sort-Object Name | ForEach-Object {
            [pscustomobject]@{
                datei         = $_.Name
                sha256        = (Get-FileHash -Path $_.FullName -Algorithm SHA256).Hash.ToLower()
                groesse_bytes = $_.Length
            }
        }
    $manifest = [pscustomobject]@{
        original_dateiname    = $OriginalName
        format                = $Kind
        erfasst_am            = (Get-Date -Format 'o')
        aufbewahrung_bis_jahr = (Get-Date).Year + 10
        gobd_hinweis          = 'Originalbeleg unveraendert aufbewahren (Belegfunktion, Aufbewahrungsfrist 10 Jahre). SHA-256 dokumentiert die Unveraenderbarkeit; das Original ist schreibgeschuetzt abgelegt.'
        artefakte             = @($artefakte)
    }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -Path (Join-Path $Dir 'manifest.json') -Encoding utf8
}

# --- Dublettenprüfung: Register (out/_register.json) ---------------------------------
# Liest Lieferantenname/Bruttobetrag tolerant — akzeptiert das verschachtelte Schema der
# Fach-Agenten (lieferant.name / betraege.brutto) UND flache Varianten (lieferant_name /
# brutto_betrag), die ein Orchestrator-LLM beim Re-Serialisieren erzeugen kann.
function Get-FieldLieferantName {
    param($Fields)
    if ($Fields.lieferant -and $Fields.lieferant.name) { return [string]$Fields.lieferant.name }
    if ($Fields.lieferant_name) { return [string]$Fields.lieferant_name }
    return $null
}
function Get-FieldBrutto {
    param($Fields)
    if ($Fields.betraege -and $null -ne $Fields.betraege.brutto) { return [double]$Fields.betraege.brutto }
    if ($null -ne $Fields.brutto_betrag) { return [double]$Fields.brutto_betrag }
    if ($null -ne $Fields.brutto) { return [double]$Fields.brutto }
    return $null
}
function Get-FieldSteuersatz {
    param($Fields)
    if ($Fields.betraege -and $null -ne $Fields.betraege.steuersatz_prozent) { return $Fields.betraege.steuersatz_prozent }
    if ($null -ne $Fields.ust_satz) { return $Fields.ust_satz }
    if ($null -ne $Fields.steuersatz_prozent) { return $Fields.steuersatz_prozent }
    return $null
}

function Get-InvoiceKey {
    param($Fields)
    $nr = if ($Fields.rechnungsnummer) { [string]$Fields.rechnungsnummer } else { '?' }
    $liefName = Get-FieldLieferantName -Fields $Fields
    $lief = if ($liefName) { $liefName } else { '?' }
    $bruttoVal = Get-FieldBrutto -Fields $Fields
    $brutto = if ($null -ne $bruttoVal) { [string]$bruttoVal } else { '?' }
    return ("{0}|{1}|{2}" -f $nr.Trim(), $lief.Trim(), $brutto.Trim()).ToLower()
}

function Read-Register {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return @() }
    try { return @(Get-Content -Path $Path -Raw | ConvertFrom-Json) } catch { return @() }
}

function Find-Duplicate {
    param([string]$RegisterPath, [string]$Key)
    Read-Register -Path $RegisterPath | Where-Object { $_.key -eq $Key } | Select-Object -First 1
}

function Add-ToRegister {
    param([string]$RegisterPath, [string]$Key, [string]$Name, $Fields)
    $reg = @(Read-Register -Path $RegisterPath)
    $reg += [pscustomobject]@{
        key             = $Key
        rechnung        = $Name
        rechnungsnummer = $Fields.rechnungsnummer
        lieferant       = (Get-FieldLieferantName -Fields $Fields)
        brutto          = (Get-FieldBrutto -Fields $Fields)
        erfasst_am      = (Get-Date -Format 'o')
    }
    $reg | ConvertTo-Json -Depth 5 | Set-Content -Path $RegisterPath -Encoding utf8
}

# --- DATEV-Buchungsstapel (EXTF) -----------------------------------------------------
function Format-DeAmount {
    param([double]$Value)
    return $Value.ToString('0.00', [System.Globalization.CultureInfo]::GetCultureInfo('de-DE'))
}

# Kopf- + Spaltenzeile eines DATEV-EXTF-Buchungsstapels (vereinfacht, Demo).
function Get-DatevHeaderLines {
    param([int]$Year = (Get-Date).Year)
    $ts = (Get-Date -Format 'yyyyMMddHHmmssfff')
    $wj = "{0}0101" -f $Year
    $von = "{0}0101" -f $Year
    $bis = "{0}1231" -f $Year
    $header = ('"EXTF";700;21;"Buchungsstapel";12;{0};;"";"";"";1;1;{1};4;{2};{3};"AP Demo (agentkit)";"";1;0;;"EUR";;;;;;"";;' -f $ts, $wj, $von, $bis)
    $caption = 'Umsatz (ohne Soll/Haben-Kz);Soll/Haben-Kennzeichen;WKZ Umsatz;Kurs;Basis-Umsatz;WKZ Basis-Umsatz;Konto;Gegenkonto (ohne BU-Schlüssel);BU-Schlüssel;Belegdatum;Belegfeld 1;Belegfeld 2;Skonto;Buchungstext'
    return @($header, $caption)
}

# Aus einem Buchungs-JSON (04/05) eine DATEV-Datenzeile ableiten. $null, wenn nicht buchbar.
function ConvertTo-DatevRow {
    param($Booking, $Fields)
    if (-not $Booking -or -not $Booking.buchung_moeglich) { return $null }

    $zeilen = @($Booking.buchungszeilen)
    if ($zeilen.Count -eq 0) { return $null }

    # Aufwandskonto = Soll-Zeile, die weder Vorsteuer (157x) noch Kreditor (1600) ist.
    $expense = $zeilen | Where-Object { $_.soll -gt 0 -and $_.konto -notmatch '^157' -and $_.konto -ne '1600' } | Select-Object -First 1
    $vorsteuer = $zeilen | Where-Object { $_.konto -match '^157' } | Select-Object -First 1
    $kreditor = $zeilen | Where-Object { $_.konto -eq '1600' } | Select-Object -First 1
    if (-not $expense) { return $null }

    $bruttoVal = Get-FieldBrutto -Fields $Fields
    $brutto = if ($null -ne $bruttoVal) { $bruttoVal } else { [double]$expense.soll }
    $satz = Get-FieldSteuersatz -Fields $Fields
    # SKR03-Vorsteuer-Automatik: BU-Schlüssel 9 = 19 %, 8 = 7 %. Ohne Vorsteuer kein Schlüssel.
    $bu = if ($vorsteuer) { if ("$satz" -eq '7') { '8' } else { '9' } } else { '' }

    $beleg = ''
    if ($Fields.rechnungsdatum -match '^(\d{4})-(\d{2})-(\d{2})$') { $beleg = $Matches[3] + $Matches[2] }  # TTMM
    $konto = [string]$expense.konto
    $gegen = if ($kreditor) { '1600' } else { '' }
    $liefName = Get-FieldLieferantName -Fields $Fields
    $text = ('{0} {1}' -f $liefName, $Fields.rechnungsnummer).Trim()
    $text = ($text -replace '"', "'")
    $belegfeld1 = [string]$Fields.rechnungsnummer

    # Spalten: Umsatz;S/H;WKZ;Kurs;Basis;WKZ-Basis;Konto;Gegenkonto;BU;Belegdatum;Belegfeld1;Belegfeld2;Skonto;Buchungstext
    return ('{0};S;EUR;;;;{1};{2};{3};{4};{5};;;"{6}"' -f (Format-DeAmount $brutto), $konto, $gegen, $bu, $beleg, $belegfeld1, $text)
}

function Write-DatevCsv {
    param([string]$Path, [string[]]$DataRows, [int]$Year = (Get-Date).Year)
    $lines = @(Get-DatevHeaderLines -Year $Year) + @($DataRows)
    # DATEV erwartet Windows-1252/CRLF; wir schreiben UTF-8 (für die Demo ausreichend, Umlaute korrekt).
    Set-Content -Path $Path -Value $lines -Encoding utf8
}
