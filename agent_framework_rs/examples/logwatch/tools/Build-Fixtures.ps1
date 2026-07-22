<#
.SYNOPSIS
    Erzeugt die Beispiel-Logs (fixtures/*.log) für logwatch.

.DESCRIPTION
    Die Logs sind so gebaut, dass sie GENAU die drei Verhaltensweisen prüfbar machen, auf die
    es bei einem lernenden Filter ankommt:

      Tag 1 (iis_tag1.log)
        * Rauschen            404 /favicon.ico, /robots.txt, Scanner auf /wp-login.php, 304er
        * Befund A            HTTP 500.19 auf /api/v2/orders (web.config kaputt), 47x
        * Befund B            Traversal-Versuch auf /api/v2/files — mit 404 ABGEWEHRT

      Tag 2 (iis_tag2.log)
        * dasselbe Rauschen   -> muss weiterhin ignoriert werden
        * Befund A LÄUFT WEITER (52x, andere Zeiten, andere IPs)
                              -> muss als BEKANNT erkannt und NICHT erneut gemeldet werden
        * Befund B ESKALIERT  derselbe Traversal-Versuch bekommt jetzt 200 statt 404
                              -> muss als NEU gemeldet werden (Verschlechterung von Bekanntem)
        * Befund C NEU        503-Serie auf /api/v2/checkout, sc-win32-status 1236

      postgres_tag1.log       ein zweiter Logtyp — zwingt den Agenten, den RICHTIGEN Skill zu
                              wählen (postgres-logs statt iis-logs)

    Die Skills nennen die Muster (was ist Rauschen, was ist Alarm), aber KEINE konkreten Routen
    oder Zahlen aus diesen Dateien. Der Agent muss lesen, nicht abschreiben.

.EXAMPLE
    pwsh -File .\tools\Build-Fixtures.ps1
#>
[CmdletBinding()]
param([string]$OutDir)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $OutDir) { $OutDir = Join-Path (Split-Path -Parent $here) 'fixtures' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$UA = 'Mozilla/5.0+(Windows+NT+10.0;+Win64;+x64)'
$BOT = 'python-requests/2.31.0'

function IisHeader([string]$datum) {
    @(
        '#Software: Microsoft Internet Information Services 10.0'
        '#Version: 1.0'
        "#Date: $datum 00:00:00"
        '#Fields: date time s-ip cs-method cs-uri-stem cs-uri-query s-port cs-username c-ip cs(User-Agent) sc-status sc-substatus sc-win32-status time-taken'
    )
}
function Row($datum, $zeit, $method, $stem, $query, $cip, $ua, $status, $sub, $win32, $taken) {
    "$datum $zeit 10.0.0.5 $method $stem $query 443 - $cip $ua $status $sub $win32 $taken"
}

# =====================================================================================
# Tag 1
# =====================================================================================
$d1 = '2026-07-11'
$t1 = IisHeader $d1

# --- Rauschen: normaler Verkehr, Favicons, Crawler ---
$normal = @('/index.html', '/app/dashboard', '/assets/app.js', '/assets/app.css', '/api/v2/health')
$min = 0
foreach ($i in 0..29) {
    $zeit = ([datetime]::ParseExact("$d1 08:00:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddSeconds($i * 47).ToString('HH:mm:ss')
    $stem = $normal[$i % $normal.Count]
    $status = if ($i % 7 -eq 0) { 304 } else { 200 }
    $t1 += Row $d1 $zeit 'GET' $stem '-' "203.0.113.$((10 + $i % 30))" $UA $status 0 0 (12 + $i % 40)
}
foreach ($i in 0..7) {
    $zeit = ([datetime]::ParseExact("$d1 08:05:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddMinutes($i * 13).ToString('HH:mm:ss')
    $t1 += Row $d1 $zeit 'GET' '/favicon.ico' '-' "203.0.113.$((10 + $i))" $UA 404 0 2 3
}
$t1 += Row $d1 '08:09:14' 'GET' '/robots.txt' '-' '66.249.66.1' 'Googlebot/2.1' 404 0 2 2
foreach ($z in @('03:11:02', '03:11:03', '03:11:05')) {
    $t1 += Row $d1 $z 'GET' '/wp-login.php' '-' '185.220.101.7' $BOT 404 0 2 4
}

# --- Befund A: die Anwendung ist kaputt. 47x 500.19 auf derselben Route. ---
$aStart = [datetime]::ParseExact("$d1 09:14:03", 'yyyy-MM-dd HH:mm:ss', $null)
foreach ($i in 0..46) {
    $zeit = $aStart.AddSeconds($i * 73).ToString('HH:mm:ss')
    $t1 += Row $d1 $zeit 'POST' '/api/v2/orders' '-' "203.0.113.$((20 + $i % 25))" $UA 500 19 0 (2100 + $i * 3)
}

# --- Befund B: Traversal-Versuch — ABGEWEHRT (404). ---
foreach ($i in 0..4) {
    $zeit = ([datetime]::ParseExact("$d1 22:41:10", 'yyyy-MM-dd HH:mm:ss', $null)).AddSeconds($i * 9).ToString('HH:mm:ss')
    $t1 += Row $d1 $zeit 'GET' '/api/v2/files' 'path=../../../../windows/win.ini' '198.51.100.77' $BOT 404 0 2 6
}

Set-Content -Path (Join-Path $OutDir 'iis_tag1.log') -Value $t1 -Encoding utf8
Write-Host ("  {0,-20} {1,4} Zeilen" -f 'iis_tag1.log', $t1.Count) -ForegroundColor Green

# =====================================================================================
# Tag 2
# =====================================================================================
$d2 = '2026-07-12'
$t2 = IisHeader $d2

# --- Dasselbe Rauschen. Muss weiter ignoriert werden. ---
foreach ($i in 0..29) {
    $zeit = ([datetime]::ParseExact("$d2 08:00:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddSeconds($i * 51).ToString('HH:mm:ss')
    $stem = $normal[$i % $normal.Count]
    $status = if ($i % 6 -eq 0) { 304 } else { 200 }
    $t2 += Row $d2 $zeit 'GET' $stem '-' "203.0.113.$((40 + $i % 30))" $UA $status 0 0 (11 + $i % 35)
}
foreach ($i in 0..6) {
    $zeit = ([datetime]::ParseExact("$d2 08:07:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddMinutes($i * 15).ToString('HH:mm:ss')
    $t2 += Row $d2 $zeit 'GET' '/favicon.ico' '-' "203.0.113.$((40 + $i))" $UA 404 0 2 3
}

# --- Befund A LÄUFT WEITER: 52x, andere Uhrzeiten, andere IPs — aber DIESELBE Sache. ---
$a2Start = [datetime]::ParseExact("$d2 07:02:11", 'yyyy-MM-dd HH:mm:ss', $null)
foreach ($i in 0..51) {
    $zeit = $a2Start.AddSeconds($i * 61).ToString('HH:mm:ss')
    $t2 += Row $d2 $zeit 'POST' '/api/v2/orders' '-' "203.0.113.$((60 + $i % 20))" $UA 500 19 0 (2000 + $i * 5)
}

# --- Befund B ESKALIERT: derselbe Traversal — jetzt mit 200. Er hat die Datei bekommen. ---
foreach ($i in 0..2) {
    $zeit = ([datetime]::ParseExact("$d2 02:17:44", 'yyyy-MM-dd HH:mm:ss', $null)).AddSeconds($i * 11).ToString('HH:mm:ss')
    $t2 += Row $d2 $zeit 'GET' '/api/v2/files' 'path=../../../../windows/win.ini' '198.51.100.77' $BOT 200 0 0 31
}

# --- Befund C NEU: 503-Serie beim Checkout, Verbindung abgebrochen (win32 1236). ---
$cStart = [datetime]::ParseExact("$d2 11:31:09", 'yyyy-MM-dd HH:mm:ss', $null)
foreach ($i in 0..18) {
    $zeit = $cStart.AddSeconds($i * 41).ToString('HH:mm:ss')
    $t2 += Row $d2 $zeit 'POST' '/api/v2/checkout' '-' "203.0.113.$((80 + $i % 15))" $UA 503 0 1236 (30000 + $i * 120)
}

Set-Content -Path (Join-Path $OutDir 'iis_tag2.log') -Value $t2 -Encoding utf8
Write-Host ("  {0,-20} {1,4} Zeilen" -f 'iis_tag2.log', $t2.Count) -ForegroundColor Green

# =====================================================================================
# PostgreSQL — ein anderer Logtyp. Zwingt zur richtigen Skill-Wahl.
# =====================================================================================
$pg = @()
function Pg($zeit, $pid_, $level, $text) { "2026-07-11 $zeit UTC [$pid_] ${level}:  $text" }

# Rauschen: Checkpoints, Autovacuum, normale Verbindungen.
foreach ($i in 0..5) {
    $z = ([datetime]::ParseExact("2026-07-11 06:00:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddMinutes($i * 15).ToString('HH:mm:ss.fff')
    $pg += Pg $z (2100 + $i) 'LOG' 'checkpoint starting: time'
    $pg += Pg $z (2100 + $i) 'LOG' 'checkpoint complete: wrote 412 buffers (2.5%); 0 WAL file(s) added'
}
foreach ($i in 0..3) {
    $z = ([datetime]::ParseExact("2026-07-11 06:05:00", 'yyyy-MM-dd HH:mm:ss', $null)).AddMinutes($i * 20).ToString('HH:mm:ss.fff')
    $pg += Pg $z (2200 + $i) 'LOG' 'automatic vacuum of table "shop.public.orders": index scans: 1'
}

# Alarm 1: Verbindungslimit erreicht — die App sieht das als "Datenbank weg".
$limStart = [datetime]::ParseExact("2026-07-11 09:14:20", 'yyyy-MM-dd HH:mm:ss', $null)
foreach ($i in 0..23) {
    $z = $limStart.AddSeconds($i * 7).ToString('HH:mm:ss.fff')
    $pg += Pg $z (3300 + $i) 'FATAL' 'sorry, too many clients already'
}

# Alarm 2: Deadlock.
$pg += Pg '10:02:41.881' 3410 'ERROR' 'deadlock detected'
$pg += Pg '10:02:41.881' 3410 'DETAIL' 'Process 3410 waits for ShareLock on transaction 88213; blocked by process 3417.'
$pg += Pg '10:02:41.881' 3410 'STATEMENT' 'UPDATE orders SET status = $1 WHERE id = $2'

# Alarm 3: langsame Abfrage.
$pg += Pg '10:44:03.112' 3502 'LOG' 'duration: 61240.882 ms  statement: SELECT * FROM orders o JOIN order_items i ON i.order_id = o.id WHERE o.created_at > now() - interval ''90 days'''

Set-Content -Path (Join-Path $OutDir 'postgres_tag1.log') -Value ($pg | Sort-Object) -Encoding utf8
Write-Host ("  {0,-20} {1,4} Zeilen" -f 'postgres_tag1.log', $pg.Count) -ForegroundColor Green

Write-Host "`nFixtures in: $OutDir" -ForegroundColor Cyan
Write-Host "Probe:  .\Invoke-LogWatch.ps1 -Demo" -ForegroundColor DarkGray
