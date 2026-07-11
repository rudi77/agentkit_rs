<#
.SYNOPSIS
    Erzeugt eine einfache, gültige PDF-Rechnung (nur PowerShell, keine externen Libs).

.DESCRIPTION
    Baut ein minimales, aber standardkonformes PDF (PDF 1.4) mit einer Helvetica-Textebene
    in WinAnsi-Kodierung (Codepage 1252), damit Umlaute und das €-Zeichen korrekt vom
    `read_pdf`-Tool (pdf-extract) gelesen werden. Wird nur zum Erzeugen der Beispiel-
    rechnungen für den Accounts-Payable-Demo genutzt — nicht Teil der Pipeline selbst.

.PARAMETER Lines
    Die Textzeilen der Rechnung (eine je Zeile).

.PARAMETER OutFile
    Zielpfad der PDF.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [AllowEmptyString()] [string[]]$Lines,
    [Parameter(Mandatory)] [string]$OutFile
)

$ErrorActionPreference = 'Stop'

# PDF-String-Literale escapen: ( ) und \ müssen mit Backslash geschützt werden.
function Escape-PdfText([string]$s) {
    $s = $s -replace '\\', '\\\\'
    $s = $s -replace '\(', '\('
    $s = $s -replace '\)', '\)'
    return $s
}

# Content-Stream aufbauen: Text ab oben links, feste Zeilenhöhe.
$sb = [System.Text.StringBuilder]::new()
[void]$sb.AppendLine('BT')
[void]$sb.AppendLine('/F1 11 Tf')
[void]$sb.AppendLine('16 TL')          # Zeilenabstand
[void]$sb.AppendLine('50 800 Td')      # Startposition (A4: 595x842)
$first = $true
foreach ($ln in $Lines) {
    $txt = Escape-PdfText $ln
    if ($first) { [void]$sb.AppendLine("($txt) Tj"); $first = $false }
    else        { [void]$sb.AppendLine("T* ($txt) Tj") }
}
[void]$sb.AppendLine('ET')
$content = $sb.ToString()

# WinAnsi (CP1252) ist Single-Byte -> Zeichen-Offset == Byte-Offset. Das nutzen wir
# für die xref-Tabelle: Byte-Länge = GetByteCount unter CP1252.
$enc = [System.Text.Encoding]::GetEncoding(1252)
$contentLen = $enc.GetByteCount($content)

# Die sieben PDF-Objekte (Catalog, Pages, Page, Font, Contents) + Header.
$header = "%PDF-1.4`n"
$objs = @(
    "1 0 obj`n<< /Type /Catalog /Pages 2 0 R >>`nendobj`n",
    "2 0 obj`n<< /Type /Pages /Kids [3 0 R] /Count 1 >>`nendobj`n",
    "3 0 obj`n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>`nendobj`n",
    "4 0 obj`n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>`nendobj`n",
    "5 0 obj`n<< /Length $contentLen >>`nstream`n$content`nendstream`nendobj`n"
)

# Dokument zusammensetzen und dabei die Byte-Offsets der Objektstarts einsammeln.
$doc = [System.Text.StringBuilder]::new()
[void]$doc.Append($header)
$offsets = @()
foreach ($o in $objs) {
    $offsets += $enc.GetByteCount($doc.ToString())
    [void]$doc.Append($o)
}

# xref-Tabelle (10-stellige Offsets, 5-stellige Generation).
$xrefStart = $enc.GetByteCount($doc.ToString())
$xref = [System.Text.StringBuilder]::new()
[void]$xref.Append("xref`n0 6`n")
[void]$xref.Append("0000000000 65535 f `n")
foreach ($off in $offsets) {
    [void]$xref.Append(('{0:D10} 00000 n ' -f $off) + "`n")
}
[void]$xref.Append("trailer`n<< /Size 6 /Root 1 0 R >>`nstartxref`n$xrefStart`n%%EOF`n")
[void]$doc.Append($xref.ToString())

# Als CP1252-Bytes schreiben (kein BOM, korrekte Umlaut-/€-Bytes).
$bytes = $enc.GetBytes($doc.ToString())
$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
[System.IO.File]::WriteAllBytes($OutFile, $bytes)
Write-Host "PDF geschrieben: $OutFile ($($bytes.Length) Bytes)"
