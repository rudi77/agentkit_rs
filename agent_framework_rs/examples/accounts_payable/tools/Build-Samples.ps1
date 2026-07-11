<#
.SYNOPSIS
    Erzeugt die Beispiel-Rechnungen (PDF) für den Accounts-Payable-Demo.
.DESCRIPTION
    Legt zwei PDFs unter ..\inbox an:
      - rechnung_sauber.pdf   : §14-konform, Arithmetik stimmt (Happy Path)
      - rechnung_maengel.pdf  : fehlende USt-IdNr + falsche Bruttosumme (Validierung schlägt an)
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$gen     = Join-Path $here 'New-SampleInvoicePdf.ps1'
$inbox   = Join-Path (Split-Path -Parent $here) 'inbox'
New-Item -ItemType Directory -Force -Path $inbox | Out-Null

# --- 1) Saubere, §14-konforme Rechnung -------------------------------------------------
$sauber = @(
    'Tischlerei Thomas Berg - Innenausbau & Möbel'
    'Lindenstraße 12, 80331 München'
    'USt-IdNr.: DE812345678   Steuernummer: 143/815/08151'
    ''
    'RECHNUNG'
    ''
    'Rechnungsempfänger:'
    'Kreativagentur Sonnenschein GmbH'
    'Marienplatz 8, 80331 München'
    ''
    'Rechnungsnummer: 2025-0042'
    'Rechnungsdatum: 15.06.2025'
    'Leistungsdatum: 10.06.2025'
    ''
    'Pos  Bezeichnung                          Menge    Einzelpreis      Betrag'
    '1    Konferenztisch Eiche massiv          1 Stk     1.200,00 €    1.200,00 €'
    '2    Montage vor Ort                      6 Std        65,00 €      390,00 €'
    ''
    'Nettobetrag (19% USt):                                          1.590,00 €'
    'zzgl. Umsatzsteuer 19%:                                           302,10 €'
    'Gesamtbetrag (brutto):                                         1.892,10 €'
    ''
    'Zahlbar innerhalb von 14 Tagen ohne Abzug auf das Konto'
    'IBAN DE12 5001 0517 0648 4898 90, Verwendungszweck 2025-0042.'
    'Vielen Dank für Ihren Auftrag!'
)
& $gen -Lines $sauber -OutFile (Join-Path $inbox 'rechnung_sauber.pdf')

# --- 2) Rechnung mit Mängeln (fehlende USt-IdNr, falsche Bruttosumme) -------------------
$maengel = @(
    'Webdesign Petra Klein'
    'Sonnenallee 99, 12045 Berlin'
    'Steuernummer: 30/123/45678'          # keine USt-IdNr angegeben
    ''
    'RECHNUNG'
    ''
    'Rechnungsempfänger:'
    'Kreativagentur Sonnenschein GmbH'
    'Marienplatz 8, 80331 München'
    ''
    'Rechnungsnummer: WD-2025-777'
    'Rechnungsdatum: 03.07.2025'
    # Leistungsdatum absichtlich weggelassen
    ''
    'Pos  Bezeichnung                          Menge    Einzelpreis      Betrag'
    '1    Gestaltung Landingpage               1 Pausch  2.000,00 €    2.000,00 €'
    ''
    'Nettobetrag (19% USt):                                          2.000,00 €'
    'zzgl. Umsatzsteuer 19%:                                           380,00 €'
    'Gesamtbetrag (brutto):                                         2.480,00 €'   # falsch: 2.000 + 380 = 2.380
    ''
    'Zahlbar sofort nach Erhalt.'
)
& $gen -Lines $maengel -OutFile (Join-Path $inbox 'rechnung_maengel.pdf')

Write-Host ''
Write-Host 'Beispielrechnungen erzeugt in:' $inbox
