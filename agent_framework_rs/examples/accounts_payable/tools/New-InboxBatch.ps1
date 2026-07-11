<#
.SYNOPSIS
    Erzeugt einen Stapel Beispiel-Eingangsrechnungen in gemischten Formaten (PDF, XRechnung-
    XML, ZUGFeRD-PDF) und legt sie in die Inbox des Batch-Demos.

.DESCRIPTION
    Datengetrieben: eine Liste von Rechnungs-Datensätzen wird je nach `Kind` als
    - `pdf`     : sichtbare PDF (kein eingebettetes XML),
    - `xml`     : reine XRechnung (UN/CEFACT CII, EN-16931-konform),
    - `zugferd` : PDF mit eingebettetem CII-XML (ZUGFeRD/Factur-X)
    erzeugt. Das CII-XML wird aus einer erprobt konformen Vorlage befüllt; die Beträge
    rechnen konsistent (Netto + USt = Brutto), damit die EN-16931-Prüfung besteht.

.PARAMETER InboxDir  Zielordner (Default: ..\inbox).
#>
[CmdletBinding()]
param([string]$InboxDir)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$gen = Join-Path $here 'New-SampleInvoicePdf.ps1'
if (-not $InboxDir) { $InboxDir = Join-Path (Split-Path -Parent $here) 'inbox' }
New-Item -ItemType Directory -Force -Path $InboxDir | Out-Null
$ci = [System.Globalization.CultureInfo]::InvariantCulture
$de = [System.Globalization.CultureInfo]::GetCultureInfo('de-DE')
function A2([double]$v) { $v.ToString('0.00', $ci) }        # 1234.50  (XML)
function D2([double]$v) { $v.ToString('N2', $de) }          # 1.234,50 (Sichtbeleg)
function IsoToDe([string]$yyyymmdd) { '{0}.{1}.{2}' -f $yyyymmdd.Substring(6, 2), $yyyymmdd.Substring(4, 2), $yyyymmdd.Substring(0, 4) }

# Konformes CII-XML (XRechnung 3.0) aus einem Datensatz bauen.
function New-CiiXml($r) {
    $net = [double]$r.Net; $rate = [double]$r.Rate
    $tax = [math]::Round($net * $rate / 100, 2); $gross = [math]::Round($net + $tax, 2)
    @"
<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice
    xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100"
    xmlns:ram="urn:un:unece:uncefact:data:standard:ReusableAggregateBusinessInformationEntity:100"
    xmlns:udt="urn:un:unece:uncefact:data:standard:UnqualifiedDataType:100">
  <rsm:ExchangedDocumentContext>
    <ram:BusinessProcessSpecifiedDocumentContextParameter><ram:ID>urn:fdc:peppol.eu:2017:poacc:billing:01:1.0</ram:ID></ram:BusinessProcessSpecifiedDocumentContextParameter>
    <ram:GuidelineSpecifiedDocumentContextParameter><ram:ID>urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0</ram:ID></ram:GuidelineSpecifiedDocumentContextParameter>
  </rsm:ExchangedDocumentContext>
  <rsm:ExchangedDocument>
    <ram:ID>$($r.Nr)</ram:ID>
    <ram:TypeCode>380</ram:TypeCode>
    <ram:IssueDateTime><udt:DateTimeString format="102">$($r.IssueDate)</udt:DateTimeString></ram:IssueDateTime>
  </rsm:ExchangedDocument>
  <rsm:SupplyChainTradeTransaction>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>1</ram:LineID></ram:AssociatedDocumentLineDocument>
      <ram:SpecifiedTradeProduct><ram:Name>$($r.Item)</ram:Name></ram:SpecifiedTradeProduct>
      <ram:SpecifiedLineTradeAgreement><ram:NetPriceProductTradePrice><ram:ChargeAmount>$(A2 $net)</ram:ChargeAmount></ram:NetPriceProductTradePrice></ram:SpecifiedLineTradeAgreement>
      <ram:SpecifiedLineTradeDelivery><ram:BilledQuantity unitCode="C62">1</ram:BilledQuantity></ram:SpecifiedLineTradeDelivery>
      <ram:SpecifiedLineTradeSettlement>
        <ram:ApplicableTradeTax><ram:TypeCode>VAT</ram:TypeCode><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>$(A2 $rate)</ram:RateApplicablePercent></ram:ApplicableTradeTax>
        <ram:SpecifiedTradeSettlementLineMonetarySummation><ram:LineTotalAmount>$(A2 $net)</ram:LineTotalAmount></ram:SpecifiedTradeSettlementLineMonetarySummation>
      </ram:SpecifiedLineTradeSettlement>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:ApplicableHeaderTradeAgreement>
      <ram:BuyerReference>04011000-12345-34</ram:BuyerReference>
      <ram:SellerTradeParty>
        <ram:Name>$($r.SellerName)</ram:Name>
        <ram:DefinedTradeContact>
          <ram:PersonName>$($r.ContactName)</ram:PersonName>
          <ram:TelephoneUniversalCommunication><ram:CompleteNumber>$($r.ContactPhone)</ram:CompleteNumber></ram:TelephoneUniversalCommunication>
          <ram:EmailURIUniversalCommunication><ram:URIID>$($r.ContactMail)</ram:URIID></ram:EmailURIUniversalCommunication>
        </ram:DefinedTradeContact>
        <ram:PostalTradeAddress><ram:PostcodeCode>$($r.SellerZip)</ram:PostcodeCode><ram:LineOne>$($r.SellerStreet)</ram:LineOne><ram:CityName>$($r.SellerCity)</ram:CityName><ram:CountryID>DE</ram:CountryID></ram:PostalTradeAddress>
        <ram:URIUniversalCommunication><ram:URIID schemeID="EM">$($r.ContactMail)</ram:URIID></ram:URIUniversalCommunication>
        <ram:SpecifiedTaxRegistration><ram:ID schemeID="VA">$($r.SellerVat)</ram:ID></ram:SpecifiedTaxRegistration>
      </ram:SellerTradeParty>
      <ram:BuyerTradeParty>
        <ram:Name>Kreativagentur Sonnenschein GmbH</ram:Name>
        <ram:PostalTradeAddress><ram:PostcodeCode>80331</ram:PostcodeCode><ram:LineOne>Marienplatz 8</ram:LineOne><ram:CityName>München</ram:CityName><ram:CountryID>DE</ram:CountryID></ram:PostalTradeAddress>
        <ram:URIUniversalCommunication><ram:URIID schemeID="EM">rechnung@sonnenschein.de</ram:URIID></ram:URIUniversalCommunication>
      </ram:BuyerTradeParty>
    </ram:ApplicableHeaderTradeAgreement>
    <ram:ApplicableHeaderTradeDelivery>
      <ram:ActualDeliverySupplyChainEvent><ram:OccurrenceDateTime><udt:DateTimeString format="102">$($r.DeliveryDate)</udt:DateTimeString></ram:OccurrenceDateTime></ram:ActualDeliverySupplyChainEvent>
    </ram:ApplicableHeaderTradeDelivery>
    <ram:ApplicableHeaderTradeSettlement>
      <ram:InvoiceCurrencyCode>EUR</ram:InvoiceCurrencyCode>
      <ram:SpecifiedTradeSettlementPaymentMeans><ram:TypeCode>58</ram:TypeCode><ram:PayeePartyCreditorFinancialAccount><ram:IBANID>$($r.Iban)</ram:IBANID></ram:PayeePartyCreditorFinancialAccount></ram:SpecifiedTradeSettlementPaymentMeans>
      <ram:ApplicableTradeTax><ram:CalculatedAmount>$(A2 $tax)</ram:CalculatedAmount><ram:TypeCode>VAT</ram:TypeCode><ram:BasisAmount>$(A2 $net)</ram:BasisAmount><ram:CategoryCode>S</ram:CategoryCode><ram:RateApplicablePercent>$(A2 $rate)</ram:RateApplicablePercent></ram:ApplicableTradeTax>
      <ram:SpecifiedTradePaymentTerms><ram:DueDateDateTime><udt:DateTimeString format="102">$($r.DueDate)</udt:DateTimeString></ram:DueDateDateTime></ram:SpecifiedTradePaymentTerms>
      <ram:SpecifiedTradeSettlementHeaderMonetarySummation>
        <ram:LineTotalAmount>$(A2 $net)</ram:LineTotalAmount>
        <ram:TaxBasisTotalAmount>$(A2 $net)</ram:TaxBasisTotalAmount>
        <ram:TaxTotalAmount currencyID="EUR">$(A2 $tax)</ram:TaxTotalAmount>
        <ram:GrandTotalAmount>$(A2 $gross)</ram:GrandTotalAmount>
        <ram:DuePayableAmount>$(A2 $gross)</ram:DuePayableAmount>
      </ram:SpecifiedTradeSettlementHeaderMonetarySummation>
    </ram:ApplicableHeaderTradeSettlement>
  </rsm:SupplyChainTradeTransaction>
</rsm:CrossIndustryInvoice>
"@
}

# Sichtbare Belegzeilen (für pdf/zugferd).
function New-VisualLines($r) {
    $net = [double]$r.Net; $rate = [double]$r.Rate
    $tax = [math]::Round($net * $rate / 100, 2); $gross = [math]::Round($net + $tax, 2)
    @(
        $r.SellerName
        "$($r.SellerStreet), $($r.SellerZip) $($r.SellerCity)"
        "USt-IdNr.: $($r.SellerVat)"
        ''
        $(if ($r.Kind -eq 'zugferd') { 'RECHNUNG (ZUGFeRD / E-Rechnung)' } else { 'RECHNUNG' })
        ''
        'Rechnungsempfänger:'
        'Kreativagentur Sonnenschein GmbH'
        'Marienplatz 8, 80331 München'
        ''
        "Rechnungsnummer: $($r.Nr)"
        "Rechnungsdatum: $(IsoToDe $r.IssueDate)"
        "Leistungsdatum: $(IsoToDe $r.DeliveryDate)"
        ''
        ('Pos 1: {0,-40} {1,12} €' -f $r.Item, (D2 $net))
        ''
        ('Nettobetrag ({0}% USt): {1,28} €' -f ([int]$rate), (D2 $net))
        ('zzgl. Umsatzsteuer {0}%: {1,25} €' -f ([int]$rate), (D2 $tax))
        ('Gesamtbetrag (brutto): {0,26} €' -f (D2 $gross))
        ''
        'Zahlbar innerhalb von 14 Tagen ohne Abzug.'
    )
}

# --- Die 10 Datensätze (3x pdf, 3x xml, 4x zugferd) ----------------------------------
$records = @(
    @{ Kind = 'pdf'; Nr = 'MS-2025-1001'; IssueDate = '20250703'; DeliveryDate = '20250630'; DueDate = '20250717'; SellerName = 'Malerbetrieb Sommer'; SellerStreet = 'Farbweg 3'; SellerZip = '80333'; SellerCity = 'München'; SellerVat = 'DE111222333'; ContactName = 'Anja Sommer'; ContactPhone = '+49 89 1112223'; ContactMail = 'info@maler-sommer.de'; Item = 'Innenanstrich Büroräume (2 Räume)'; Net = 850; Rate = 19; Iban = 'DE02120300000000202051' }
    @{ Kind = 'pdf'; Nr = 'ITN-2025-4402'; IssueDate = '20250705'; DeliveryDate = '20250704'; DueDate = '20250719'; SellerName = 'IT-Service Nowak'; SellerStreet = 'Serverstraße 12'; SellerZip = '20095'; SellerCity = 'Hamburg'; SellerVat = 'DE222333444'; ContactName = 'Piotr Nowak'; ContactPhone = '+49 40 2223334'; ContactMail = 'support@it-nowak.de'; Item = 'Wartung Netzwerk + Support (Juni)'; Net = 1240; Rate = 19; Iban = 'DE02100500000054540402' }
    @{ Kind = 'pdf'; Nr = 'GB-2025-0088'; IssueDate = '20250708'; DeliveryDate = '20250707'; DueDate = '20250722'; SellerName = 'Gärtnerei Blatt'; SellerStreet = 'Grünweg 7'; SellerZip = '50667'; SellerCity = 'Köln'; SellerVat = 'DE333444555'; ContactName = 'Rosa Blatt'; ContactPhone = '+49 221 3334445'; ContactMail = 'kontakt@gaertnerei-blatt.de'; Item = 'Bepflanzung Innenhof + Pflege'; Net = 560; Rate = 19; Iban = 'DE02370400440532013000' }
    @{ Kind = 'xml'; Nr = 'HM-2025-2001'; IssueDate = '20250710'; DeliveryDate = '20250709'; DueDate = '20250724'; SellerName = 'Schreinerei Holzmann'; SellerStreet = 'Sägewerkstraße 5'; SellerZip = '70173'; SellerCity = 'Stuttgart'; SellerVat = 'DE444555666'; ContactName = 'Karl Holzmann'; ContactPhone = '+49 711 4445556'; ContactMail = 'buero@schreinerei-holzmann.de'; Item = 'Massivholz-Regalwand nach Maß'; Net = 2100; Rate = 19; Iban = 'DE02600501010002034304' }
    @{ Kind = 'xml'; Nr = 'RS-2025-0311'; IssueDate = '20250711'; DeliveryDate = '20250710'; DueDate = '20250725'; SellerName = 'Reinigung Sauber GmbH'; SellerStreet = 'Putzallee 21'; SellerZip = '10115'; SellerCity = 'Berlin'; SellerVat = 'DE555666777'; ContactName = 'Mila Frisch'; ContactPhone = '+49 30 5556667'; ContactMail = 'office@sauber-gmbh.de'; Item = 'Unterhaltsreinigung Büro (Monat)'; Net = 480; Rate = 19; Iban = 'DE02100100100006820101' }
    @{ Kind = 'xml'; Nr = 'CG-2025-7777'; IssueDate = '20250712'; DeliveryDate = '20250711'; DueDate = '20250726'; SellerName = 'Catering Genuss'; SellerStreet = 'Kochgasse 9'; SellerZip = '60311'; SellerCity = 'Frankfurt am Main'; SellerVat = 'DE666777888'; ContactName = 'Ben Kuchen'; ContactPhone = '+49 69 6667778'; ContactMail = 'bestellung@catering-genuss.de'; Item = 'Team-Lunch Buffet (20 Personen)'; Net = 690; Rate = 7; Iban = 'DE02500105170137075030' }
    @{ Kind = 'zugferd'; Nr = 'EV-2025-3001'; IssueDate = '20250714'; DeliveryDate = '20250713'; DueDate = '20250728'; SellerName = 'Elektro Volt GmbH'; SellerStreet = 'Stromweg 4'; SellerZip = '90402'; SellerCity = 'Nürnberg'; SellerVat = 'DE777888999'; ContactName = 'Uwe Volt'; ContactPhone = '+49 911 7778889'; ContactMail = 'service@elektro-volt.de'; Item = 'Elektroinstallation Besprechungsraum'; Net = 1560; Rate = 19; Iban = 'DE02760300800001234567' }
    @{ Kind = 'zugferd'; Nr = 'SP-2025-3002'; IssueDate = '20250715'; DeliveryDate = '20250714'; DueDate = '20250729'; SellerName = 'Spedition Schnell'; SellerStreet = 'Frachtstraße 88'; SellerZip = '44135'; SellerCity = 'Dortmund'; SellerVat = 'DE888999000'; ContactName = 'Tom Rad'; ContactPhone = '+49 231 8889990'; ContactMail = 'dispo@spedition-schnell.de'; Item = 'Möbeltransport + Aufbau'; Net = 2340; Rate = 19; Iban = 'DE02440501990000123456' }
    @{ Kind = 'zugferd'; Nr = 'WP-2025-3003'; IssueDate = '20250716'; DeliveryDate = '20250715'; DueDate = '20250730'; SellerName = 'Werbeagentur Pixel'; SellerStreet = 'Kreativplatz 1'; SellerZip = '40213'; SellerCity = 'Düsseldorf'; SellerVat = 'DE999000111'; ContactName = 'Lea Pixel'; ContactPhone = '+49 211 9990001'; ContactMail = 'hello@agentur-pixel.de'; Item = 'Kampagnen-Konzept + Design'; Net = 3200; Rate = 19; Iban = 'DE02300501100007654321' }
    @{ Kind = 'zugferd'; Nr = 'BK-2025-3004'; IssueDate = '20250717'; DeliveryDate = '20250716'; DueDate = '20250731'; SellerName = 'Büromöbel König'; SellerStreet = 'Stuhlgasse 14'; SellerZip = '04109'; SellerCity = 'Leipzig'; SellerVat = 'DE100200300'; ContactName = 'Otto König'; ContactPhone = '+49 341 1002003'; ContactMail = 'verkauf@bueromoebel-koenig.de'; Item = 'Höhenverstellbare Schreibtische (4 Stk)'; Net = 1120; Rate = 19; Iban = 'DE02860555920000123456' }
)

function Slug([string]$s) {
    ($s.ToLower() -replace 'ä', 'ae' -replace 'ö', 'oe' -replace 'ü', 'ue' -replace 'ß', 'ss' -replace '[^a-z0-9]+', '-').Trim('-')
}

$tmp = Join-Path $env:TEMP ("ciibatch_" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force $tmp | Out-Null
$i = 0
foreach ($r in $records) {
    $i++
    $slug = Slug $r.SellerName
    $stem = 'eingang_{0:D2}_{1}' -f $i, $slug
    switch ($r.Kind) {
        'pdf' {
            & $gen -Lines (New-VisualLines $r) -OutFile (Join-Path $InboxDir "$stem.pdf") | Out-Null
        }
        'xml' {
            (New-CiiXml $r) | Set-Content -Path (Join-Path $InboxDir "$stem.xml") -Encoding utf8
            Write-Host "XML geschrieben: $stem.xml"
        }
        'zugferd' {
            $x = Join-Path $tmp "$stem.xml"
            (New-CiiXml $r) | Set-Content -Path $x -Encoding utf8
            & $gen -Lines (New-VisualLines $r) -OutFile (Join-Path $InboxDir "$stem.pdf") -EmbedXml $x | Out-Null
        }
    }
}
Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
Write-Host "Fertig — 10 Rechnungen (3 PDF, 3 XRechnung-XML, 4 ZUGFeRD) in: $InboxDir"
