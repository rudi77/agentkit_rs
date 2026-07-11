<#
.SYNOPSIS
    Erzeugt die Beispiel-Ereignisse (fixtures/*.json) für die Windows-Incident-Triage.

.DESCRIPTION
    Die Fixtures erzählen EINEN zusammenhängenden Störfall auf einem fiktiven Server
    SRV-WWS-01 — plus zwei Dinge, die NICHT dazugehören und die der Agent auseinander-
    halten muss:

      * die Ursachenkette      Treiber-Update -> Gerätereset -> Bluescheen -> Neustart ->
                               Absturzabbild füllt C: -> PostgreSQL startet nicht ->
                               Warenwirtschaft stürzt seither im 5-Minuten-Takt ab
      * ein eigener Vorfall    Brute-Force gegen 'administrator' (Stunden vorher, abgewehrt)
      * Rauschen               Routine-Ereignisse (Dienst-Start/Stop, Defender-Updates,
                               Dienstanmeldungen) — die Masse, die ein Mensch überliest

    Die Werte hier sind BEWUSST andere als die Beispielwerte in prompts/*.md. Schreibt ein
    Agent die Prompt-Werte ab (KB5041234, iaStorVD.sys, 0x7E …), statt die Ereignisse zu
    lesen, fällt das sofort auf. Der Prompt zeigt das Format — nicht die Antwort.

.EXAMPLE
    pwsh -File .\tools\Build-Fixtures.ps1
#>
[CmdletBinding()]
param([string]$OutDir)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $OutDir) { $OutDir = Join-Path (Split-Path -Parent $here) 'fixtures' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# Der Störfall spielt in der Nacht auf den 11.07.2026.
$tag = '2026-07-11'
function T([string]$hhmmss) { "${tag}T$hhmmss" }
function Ev($zeit, $id, $quelle, $level, $log, $text) {
    [pscustomobject]@{ zeit = $zeit; id = [int]$id; quelle = $quelle; level = $level; log = $log; text = $text }
}

# =====================================================================================
# System — die Ursachenkette, versteckt in Routine-Rauschen
# =====================================================================================
$system = @()

# Rauschen: der Dienste-Manager protokolliert jeden Start/Stop (7036). Davon gibt es immer
# Dutzende; sie sind bedeutungslos und sollen den Blick verstellen.
$rauschDienste = @('Windows Update', 'Windows Modules Installer', 'Background Intelligent Transfer Service',
    'Windows Defender Antivirus Network Inspection', 'Application Experience', 'Windows-Sicherungsdienst')
$rauschZeiten = @('01:03:11', '01:22:47', '02:05:33', '02:31:08', '03:14:52', '05:02:19', '06:17:40', '07:01:05')
for ($i = 0; $i -lt $rauschZeiten.Count; $i++) {
    $d = $rauschDienste[$i % $rauschDienste.Count]
    $system += Ev (T $rauschZeiten[$i]) 7036 'Service Control Manager' 'Informationen' 'System' `
        "Der Dienst `"$d`" befindet sich jetzt im Status `"Wird ausgeführt`"."
}

# 04:07 — der Vorbote: der Speichercontroller muss dreimal zurückgesetzt werden.
# Der Treiber, der sich hier meldet, ist DERSELBE, der um 03:47 eingespielt wurde und
# gleich den Bluescheen auslöst. Diese Kohärenz ist der Kern der Aufgabe: nur wenn
# Update, Vorbote und Absturzmodul zusammenpassen, IST da eine Kette zu finden.
foreach ($t in @('04:07:31', '04:07:44', '04:08:02')) {
    $system += Ev (T $t) 129 'iaStorVD' 'Warnung' 'System' `
        'Zuruecksetzen des Geraets \Device\RaidPort1 wurde ausgegeben.'
}

# 04:09 — der Absturz.
$system += Ev (T '04:09:18') 41 'Microsoft-Windows-Kernel-Power' 'Kritisch' 'System' `
    'Das System wurde neu gestartet, ohne dass es zuvor ordnungsgemaess heruntergefahren wurde. Dieser Fehler kann auftreten, wenn das System nicht mehr reagierte, abgestuerzt ist oder unerwartet die Stromzufuhr unterbrochen wurde. BugcheckCode: 209. BugcheckParameter1: 0x28.'

$system += Ev (T '04:10:55') 6008 'EventLog' 'Fehler' 'System' `
    "Das System wurde zuvor am 11.07.2026 um 04:09:18 unerwartet heruntergefahren."

$system += Ev (T '04:11:02') 1001 'Microsoft-Windows-WER-SystemErrorReporting' 'Fehler' 'System' `
    'Der Computer wurde nach einem schwerwiegenden Fehler neu gestartet. Der Fehlercode war: 0x000000d1 (DRIVER_IRQL_NOT_LESS_OR_EQUAL). Das fehlerhafte Modul war: iaStorVD.sys (Intel RAID/VMD Controller, Version 20.10.1.1023). Ein vollstaendiges Abbild wurde gespeichert unter: C:\Windows\MEMORY.DMP.'

# 04:12 — nach dem Neustart kommt die Datenbank nicht hoch; die Warenwirtschaft hängt daran.
$system += Ev (T '04:12:40') 7000 'Service Control Manager' 'Fehler' 'System' `
    'Der Dienst "postgresql-x64-16" wurde aufgrund folgenden Fehlers nicht gestartet: Das Zeitlimit (30000 ms) wurde beim Verbinden mit dem Dienst erreicht.'

$system += Ev (T '04:12:41') 7001 'Service Control Manager' 'Fehler' 'System' `
    'Der Dienst "WWS-AppServer" ist vom Dienst "postgresql-x64-16" abhaengig, der aufgrund folgenden Fehlers nicht gestartet werden konnte: Das Zeitlimit (30000 ms) wurde beim Verbinden mit dem Dienst erreicht.'

# Der Datenbankdienst versucht es weiter — und scheitert weiter.
foreach ($t in @('04:22:40', '04:32:41', '04:42:40')) {
    $system += Ev (T $t) 7031 'Service Control Manager' 'Fehler' 'System' `
        'Der Dienst "postgresql-x64-16" wurde unerwartet beendet. Dies ist bereits 3-mal vorgekommen. Folgende Korrekturmassnahme wird in 60000 Millisekunden durchgefuehrt: Dienst neu starten.'
}

# =====================================================================================
# Security — ein EIGENER Vorfall, Stunden vor dem Absturz. Darf NICHT verknüpft werden.
# =====================================================================================
$security = @()

# Rauschen: routinemäßige Dienstanmeldungen.
foreach ($t in @('00:12:03', '02:00:07', '04:11:44', '06:00:09')) {
    $security += Ev (T $t) 4624 'Microsoft-Windows-Security-Auditing' 'Informationen' 'Security' `
        'Ein Konto wurde erfolgreich angemeldet. Sicherheits-ID: NT-AUTORITAET\SYSTEM. Kontoname: SRV-WWS-01$. Anmeldetyp: 5 (Dienst).'
}
$security += Ev (T '00:12:03') 4672 'Microsoft-Windows-Security-Auditing' 'Informationen' 'Security' `
    'Spezielle Anmeldung. Neue Anmeldung: NT-AUTORITAET\SYSTEM. Zugewiesene Berechtigungen: SeSecurityPrivilege, SeBackupPrivilege.'

# 01:47–02:03 — 96 Fehlversuche gegen 'administrator' von EINER Quelle. Identischer Text
# -> die Verdichtung fasst sie zu EINEM Befund mit anzahl=96 zusammen.
$start = [datetime]::ParseExact("$tag 01:47:12", 'yyyy-MM-dd HH:mm:ss', $null)
for ($i = 0; $i -lt 96; $i++) {
    $t = $start.AddSeconds($i * 10.5)
    $security += Ev ($t.ToString('yyyy-MM-ddTHH:mm:ss')) 4625 'Microsoft-Windows-Security-Auditing' 'Informationen' 'Security' `
        'Fehler beim Anmelden eines Kontos. Kontoname: administrator. Anmeldetyp: 3 (Netzwerk). Fehlerursache: Unbekannter Benutzername oder ungueltiges Kennwort. Status: 0xC000006D. Quellnetzwerkadresse: 198.51.100.42. Quellport: 51422.'
}

# 02:04 — die Sperrschwelle greift. Kein 4624 dieser Quelle: der Versuch war erfolglos.
$security += Ev (T '02:04:01') 4740 'Microsoft-Windows-Security-Auditing' 'Informationen' 'Security' `
    'Ein Benutzerkonto wurde gesperrt. Kontoname: administrator. Aufrufercomputername: WORKSTATION-7. Der Grund ist das Ueberschreiten der zulaessigen Anzahl fehlgeschlagener Anmeldeversuche.'

# =====================================================================================
# Application — das Symptom: die Warenwirtschaft stirbt im 5-Minuten-Takt.
# =====================================================================================
$application = @()

# Rauschen vor dem Vorfall.
$application += Ev (T '01:15:22') 1001 'Windows Error Reporting' 'Informationen' 'Application' `
    'Fehlerbucket 1234567890, Typ 5. Ereignisname: WindowsUpdateFailure3. Antwort: Nicht verfuegbar.'
$application += Ev (T '03:30:04') 1000 'Microsoft-Windows-Defrag' 'Informationen' 'Application' `
    'Die Datentraegeroptimierung wurde erfolgreich fuer Volume (C:) abgeschlossen.'

# 04:14 bis 07:44 — alle 5 Minuten derselbe Absturz. 43 Ereignisse, identischer Text
# -> verdichtet zu EINEM Befund mit anzahl=43 und Zeitraum. Genau die Information, die zählt.
$crashStart = [datetime]::ParseExact("$tag 04:14:07", 'yyyy-MM-dd HH:mm:ss', $null)
for ($i = 0; $i -lt 43; $i++) {
    $t = $crashStart.AddMinutes($i * 5)
    $application += Ev ($t.ToString('yyyy-MM-ddTHH:mm:ss')) 1000 'Application Error' 'Fehler' 'Application' `
        'Name der fehlerhaften Anwendung: WWS-AppServer.exe, Version: 7.3.1.0. Name des fehlerhaften Moduls: KERNELBASE.dll, Version: 10.0.20348.2527. Ausnahmecode: 0xe0434352. Ausnahme: Npgsql.NpgsqlException: Die Verbindung zu 127.0.0.1:5432 wurde abgelehnt (Connection refused).'
}

# =====================================================================================
# Update — was sich kurz vorher geändert hat. Ein Treiber und ein harmloses Rollup.
# =====================================================================================
$update = @()
# Das harmlose Rollup — der Ablenker. Steht früh und hat mit nichts zu tun.
$update += Ev (T '02:14:40') 19 'Microsoft-Windows-WindowsUpdateClient' 'Informationen' 'Microsoft-Windows-WindowsUpdateClient/Operational' `
    'Installation erfolgreich: Das folgende Update wurde erfolgreich installiert: 2026-07 Kumulatives Update fuer Windows Server 2022 fuer x64-basierte Systeme (KB5061980).'
$update += Ev (T '03:02:11') 43 'Microsoft-Windows-WindowsUpdateClient' 'Informationen' 'Microsoft-Windows-WindowsUpdateClient/Operational' `
    'Installation wird gestartet: Sicherheitsintelligenz-Update fuer Microsoft Defender Antivirus - KB2267602 (Version 1.415.883.0).'
$update += Ev (T '03:04:57') 19 'Microsoft-Windows-WindowsUpdateClient' 'Informationen' 'Microsoft-Windows-WindowsUpdateClient/Operational' `
    'Installation erfolgreich: Das folgende Update wurde erfolgreich installiert: Sicherheitsintelligenz-Update fuer Microsoft Defender Antivirus - KB2267602.'

# 03:47 — DER Verdächtige: ein Speichercontroller-Treiber, 22 Minuten vor dem Absturz.
# Derselbe Treiber (iaStorVD), der um 04:07 Geräteresets meldet und um 04:09 abstürzt.
$update += Ev (T '03:47:05') 19 'Microsoft-Windows-WindowsUpdateClient' 'Informationen' 'Microsoft-Windows-WindowsUpdateClient/Operational' `
    'Installation erfolgreich: Das folgende Update wurde erfolgreich installiert: Intel Corporation - SCSIAdapter - iaStorVD.sys 20.10.1.1023 (KB5062170). Betroffenes Geraet: Intel RAID/VMD Controller.'

# =====================================================================================
# Inventar — der Zustand JETZT. Hier steckt die Rückkopplung: das 9,7-GB-Abbild.
# =====================================================================================
$inventory = [pscustomobject]@{
    rechner           = 'SRV-WWS-01'
    letzter_start     = (T '04:11:30')
    uptime_stunden    = 3.6
    volumes           = @(
        [pscustomobject]@{ laufwerk = 'C:'; groesse_gb = 237.4; frei_gb = 3.1; frei_prozent = 1.3 }
        [pscustomobject]@{ laufwerk = 'D:'; groesse_gb = 931.5; frei_gb = 402.8; frei_prozent = 43.2 }
    )
    haengende_dienste = @(
        [pscustomobject]@{ dienst = 'postgresql-x64-16'; anzeigename = 'PostgreSQL Server 16'; status = 'Stopped' }
        [pscustomobject]@{ dienst = 'WWS-AppServer'; anzeigename = 'Warenwirtschaft Anwendungsserver'; status = 'Stopped' }
    )
    absturzabbilder   = @(
        [pscustomobject]@{ pfad = 'C:\Windows\MEMORY.DMP'; groesse_gb = 9.7; geaendert = (T '04:11:02') }
    )
    letzte_updates    = @(
        [pscustomobject]@{ kb = 'KB5062170'; installiert = $tag }
        [pscustomobject]@{ kb = 'KB5061980'; installiert = $tag }
        [pscustomobject]@{ kb = 'KB2267602'; installiert = $tag }
    )
}

# --- Schreiben ------------------------------------------------------------------------
$dateien = @{
    'system.json'      = ($system | Sort-Object zeit)
    'security.json'    = ($security | Sort-Object zeit)
    'application.json' = ($application | Sort-Object zeit)
    'update.json'      = ($update | Sort-Object zeit)
    'inventory.json'   = $inventory
}
foreach ($name in ($dateien.Keys | Sort-Object)) {
    $pfad = Join-Path $OutDir $name
    ($dateien[$name] | ConvertTo-Json -Depth 6) | Set-Content -Path $pfad -Encoding utf8
    $n = if ($name -eq 'inventory.json') { '—' } else { @($dateien[$name]).Count }
    Write-Host ("  {0,-18} {1,4} Ereignis(se)" -f $name, $n) -ForegroundColor Green
}
Write-Host "`nFixtures in: $OutDir" -ForegroundColor Cyan
Write-Host "Probe:  .\Invoke-WinTriage.ps1 -UseFixtures" -ForegroundColor DarkGray
