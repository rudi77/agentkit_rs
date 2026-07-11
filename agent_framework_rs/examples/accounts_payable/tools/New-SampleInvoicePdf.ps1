<#
.SYNOPSIS
    Erzeugt eine einfache, gültige PDF-Rechnung (nur PowerShell, keine externen Libs) —
    optional als ZUGFeRD/Factur-X (PDF mit eingebettetem Rechnungs-XML).

.DESCRIPTION
    Baut ein minimales, standardnahes PDF (PDF 1.4) mit einer Helvetica-Textebene in
    WinAnsi-Kodierung (CP1252), damit Umlaute/€ vom `read-pdf`-Tool korrekt gelesen werden.
    Mit -EmbedXml wird zusätzlich eine XML-Datei als /EmbeddedFile eingebettet (UTF-8),
    sodass die PDF wie eine ZUGFeRD-Rechnung aussieht: sichtbarer Beleg + strukturierte
    Daten. Der Aufbau erfolgt auf Byte-Ebene, weil die PDF-Struktur CP1252, das eingebettete
    XML aber UTF-8 ist. Nur zum Erzeugen der Beispielrechnungen — nicht Teil der Pipeline.

.PARAMETER Lines
    Die sichtbaren Textzeilen der Rechnung.
.PARAMETER OutFile
    Zielpfad der PDF.
.PARAMETER EmbedXml
    Optional: Pfad zu einer XML-Datei, die als factur-x.xml eingebettet wird (ZUGFeRD).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [AllowEmptyString()] [string[]]$Lines,
    [Parameter(Mandatory)] [string]$OutFile,
    [string]$EmbedXml
)

$ErrorActionPreference = 'Stop'
$enc = [System.Text.Encoding]::GetEncoding(1252)   # WinAnsi: 1 Byte/Zeichen -> Offset == Byteoffset
$utf8 = New-Object System.Text.UTF8Encoding($false) # ohne BOM, für das eingebettete XML

function Escape-PdfText([string]$s) {
    ($s -replace '\\', '\\\\') -replace '\(', '\(' -replace '\)', '\)'
}

# Sichtbaren Content-Stream (Textebene) aufbauen.
$sb = [System.Text.StringBuilder]::new()
[void]$sb.AppendLine('BT'); [void]$sb.AppendLine('/F1 11 Tf'); [void]$sb.AppendLine('16 TL')
[void]$sb.AppendLine('50 800 Td')
$first = $true
foreach ($ln in $Lines) {
    $txt = Escape-PdfText $ln
    if ($first) { [void]$sb.AppendLine("($txt) Tj"); $first = $false }
    else { [void]$sb.AppendLine("T* ($txt) Tj") }
}
[void]$sb.AppendLine('ET')
$content = $sb.ToString()
$contentBytes = $enc.GetBytes($content)

$embed = [bool]$EmbedXml
$xmlBytes = $null
if ($embed) {
    if (-not (Test-Path $EmbedXml)) { throw "EmbedXml nicht gefunden: $EmbedXml" }
    $xmlBytes = $utf8.GetBytes((Get-Content -Path $EmbedXml -Raw))
}

# --- Byte-Assembler: PDF aus Segmenten zusammensetzen, Offsets in Bytes tracken. -------
$doc = [System.Collections.Generic.List[byte]]::new()
function Add-Cp1252([string]$s) { $script:doc.AddRange($script:enc.GetBytes($s)) }
function Add-Raw([byte[]]$b)     { $script:doc.AddRange($b) }
$offsets = @{}
function Mark([int]$objNum) { $script:offsets[$objNum] = $script:doc.Count }

Add-Cp1252 "%PDF-1.4`n%öäüß`n"   # Binär-Marker (hohe Bytes) für „echtes" Binär-PDF

# Objekt-Reihenfolge im Byte-Strom: Katalog, [EmbeddedFile, Filespec], Pages, Page, Font, Contents.
# EmbeddedFile-Stream steht bewusst VOR dem Contents-Stream, damit die Byte-Scan-Extraktion
# (wie im xcheck-Extractor) direkt das XML findet.
$catalog = if ($embed) {
    "1 0 obj`n<< /Type /Catalog /Pages 2 0 R /AF [7 0 R] " +
    "/Names << /EmbeddedFiles << /Names [(factur-x.xml) 7 0 R] >> >> >>`nendobj`n"
} else {
    "1 0 obj`n<< /Type /Catalog /Pages 2 0 R >>`nendobj`n"
}
Mark 1; Add-Cp1252 $catalog

if ($embed) {
    # Objekt 6: der eingebettete XML-Stream (UTF-8, unkomprimiert).
    Mark 6
    Add-Cp1252 "6 0 obj`n<< /Type /EmbeddedFile /Subtype /text#2Fxml /Length $($xmlBytes.Length) >>`nstream`n"
    Add-Raw $xmlBytes
    Add-Cp1252 "`nendstream`nendobj`n"
    # Objekt 7: Filespec, das auf den eingebetteten Stream zeigt (ZUGFeRD-Relationship).
    Mark 7
    Add-Cp1252 ("7 0 obj`n<< /Type /Filespec /F (factur-x.xml) /UF (factur-x.xml) " +
                "/AFRelationship /Alternative /EF << /F 6 0 R >> >>`nendobj`n")
}

Mark 2; Add-Cp1252 "2 0 obj`n<< /Type /Pages /Kids [3 0 R] /Count 1 >>`nendobj`n"
Mark 3; Add-Cp1252 ("3 0 obj`n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] " +
                    "/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>`nendobj`n")
Mark 4; Add-Cp1252 "4 0 obj`n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>`nendobj`n"
Mark 5
Add-Cp1252 "5 0 obj`n<< /Length $($contentBytes.Length) >>`nstream`n"
Add-Raw $contentBytes
Add-Cp1252 "`nendstream`nendobj`n"

# xref-Tabelle (Objekte in Nummern-Reihenfolge; Offsets aus der Map).
$count = if ($embed) { 8 } else { 6 }   # inkl. Objekt 0 (frei)
$xrefStart = $doc.Count
$xref = [System.Text.StringBuilder]::new()
[void]$xref.Append("xref`n0 $count`n")
[void]$xref.Append("0000000000 65535 f `n")
for ($i = 1; $i -lt $count; $i++) {
    [void]$xref.Append(('{0:D10} 00000 n ' -f $offsets[$i]) + "`n")
}
[void]$xref.Append("trailer`n<< /Size $count /Root 1 0 R >>`nstartxref`n$xrefStart`n%%EOF`n")
Add-Cp1252 $xref.ToString()

$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
[System.IO.File]::WriteAllBytes($OutFile, $doc.ToArray())
$kind = if ($embed) { 'ZUGFeRD (mit eingebettetem XML)' } else { 'PDF' }
Write-Host "PDF geschrieben: $OutFile ($($doc.Count) Bytes) — $kind"
