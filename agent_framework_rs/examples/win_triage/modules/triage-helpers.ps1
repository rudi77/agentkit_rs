<#
    Hilfsfunktionen für die Windows-Incident-Triage (dot-sourced von Invoke-WinTriage.ps1).

    Reines PowerShell. Der Kern ist ein ADAPTER: `Get-TriageEvents` liefert Ereignisse
    entweder aus dem ECHTEN Windows-Event-Log oder aus den mitgelieferten Fixtures —
    beide Male in DERSELBEN normalisierten Form. Die Agenten dahinter merken keinen
    Unterschied (dasselbe hexagonale Prinzip wie agentkits stdin/stdout-Vertrag).

    Normalisiertes Ereignis:
        zeit   ISO-8601 (Sekunden)
        id     Event-ID (int)
        quelle ProviderName
        level  Kritisch | Fehler | Warnung | Informationen
        log    Logname
        text   Meldung, auf eine Zeile normalisiert und gekürzt
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

# --- agentkit-Executable auflösen ----------------------------------------------------
function Resolve-Agentkit {
    param([string]$Explicit, [string]$RepoDir)
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw "agentkit nicht gefunden: $Explicit"
    }
    foreach ($rel in @('target\release\agentkit.exe', 'target\debug\agentkit.exe', 'target/release/agentkit', 'target/debug/agentkit')) {
        $p = Join-Path $RepoDir $rel
        if (Test-Path $p) { return (Resolve-Path $p).Path }
    }
    $cmd = Get-Command agentkit -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "Keine agentkit-Executable gefunden. Baue: cargo build --release (in agent_framework_rs)."
}

# --- Ausgabe -------------------------------------------------------------------------
function Write-Head($m) { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Write-Step($m) { Write-Host "  -> $m" -ForegroundColor DarkGray }
function Write-Okay($m) { Write-Host "  [ok] $m" -ForegroundColor Green }
function Write-Warn($m) { Write-Host "  [??] $m" -ForegroundColor Yellow }
function Write-Fail($m) { Write-Host "  [!!] $m" -ForegroundColor Red }

# --- Ein Ereignis auf die normalisierte Form bringen ---------------------------------
function ConvertTo-TriageEvent {
    param($Record, [int]$MaxText = 400)
    $text = [string]$Record.Message
    if (-not $text) { $text = '(keine Meldung)' }
    $text = ($text -replace '\s+', ' ').Trim()
    if ($text.Length -gt $MaxText) { $text = $text.Substring(0, $MaxText) + ' …' }
    [pscustomobject]@{
        zeit   = $Record.TimeCreated.ToString('yyyy-MM-ddTHH:mm:ss')
        id     = [int]$Record.Id
        quelle = [string]$Record.ProviderName
        level  = [string]$Record.LevelDisplayName
        log    = [string]$Record.LogName
        text   = $text
    }
}

# --- Die vier beobachteten Subsysteme ------------------------------------------------
# Jedes ist eine eigene Fan-out-Stufe: ein spezialisierter Agent pro Subsystem.
function Get-TriageSubsystems {
    @(
        [pscustomobject]@{
            Name    = 'system'
            Titel   = 'System (Kernel, Treiber, Dienste)'
            Log     = 'System'
            Prompt  = '10_system.md'
            Fixture = 'system.json'
        },
        [pscustomobject]@{
            Name    = 'security'
            Titel   = 'Sicherheit (Anmeldungen, Sperrungen)'
            Log     = 'Security'
            Prompt  = '11_security.md'
            Fixture = 'security.json'
        },
        [pscustomobject]@{
            Name    = 'application'
            Titel   = 'Anwendungen (Abstürze, Hänger)'
            Log     = 'Application'
            Prompt  = '12_application.md'
            Fixture = 'application.json'
        },
        [pscustomobject]@{
            Name    = 'update'
            Titel   = 'Windows Update (Patches, Treiber)'
            Log     = 'Microsoft-Windows-WindowsUpdateClient/Operational'
            Prompt  = '13_update.md'
            Fixture = 'update.json'
        }
    )
}

# --- Ereignisse holen: echtes Event-Log, sonst Fixture --------------------------------
# Gibt ein Objekt zurück: @{ quelle = 'live'|'fixture'; grund = '…'; events = @(…) }
function Get-TriageEvents {
    param(
        [Parameter(Mandatory)] $Subsystem,
        [int]$Hours = 24,
        [string]$FixtureDir,
        [switch]$UseFixtures,
        [int]$MaxEvents = 300
    )
    $fixturePath = Join-Path $FixtureDir $Subsystem.Fixture

    function Read-Fixture([string]$grund) {
        if (-not (Test-Path $fixturePath)) {
            return [pscustomobject]@{ quelle = 'leer'; grund = "keine Fixture: $fixturePath"; events = @() }
        }
        $ev = @(Get-Content -Path $fixturePath -Raw | ConvertFrom-Json)
        return [pscustomobject]@{ quelle = 'fixture'; grund = $grund; events = $ev }
    }

    if ($UseFixtures) { return Read-Fixture 'per -UseFixtures erzwungen' }
    if (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6) {
        return Read-Fixture 'kein Windows — Event-Log nicht verfügbar'
    }
    if (-not (Get-Command Get-WinEvent -ErrorAction SilentlyContinue)) {
        return Read-Fixture 'Get-WinEvent nicht verfügbar'
    }

    try {
        $filter = @{ LogName = $Subsystem.Log; StartTime = (Get-Date).AddHours(-$Hours) }
        $raw = @(Get-WinEvent -FilterHashtable $filter -MaxEvents $MaxEvents -ErrorAction Stop)
        if ($raw.Count -eq 0) {
            return Read-Fixture "keine Ereignisse in den letzten $Hours h im Log '$($Subsystem.Log)'"
        }
        $ev = @($raw | ForEach-Object { ConvertTo-TriageEvent -Record $_ })
        return [pscustomobject]@{ quelle = 'live'; grund = "$($ev.Count) Ereignis(se) aus '$($Subsystem.Log)'"; events = $ev }
    }
    catch {
        # Typisch: das Security-Log verlangt administrative Rechte.
        $msg = $_.Exception.Message
        if ($msg -match 'Zugriff|Access|denied|verweigert') { $msg = 'Zugriff verweigert (Security-Log braucht Administratorrechte)' }
        return Read-Fixture "Live-Zugriff fehlgeschlagen: $msg"
    }
}

# --- Deterministische Systemfakten (kein LLM) ----------------------------------------
# Zustand, den kein Log kennt: Uptime, freier Platz, hängende Autostart-Dienste,
# Absturzabbilder. Genau die Fakten, an denen eine Korrelation sonst scheitert.
function Get-SystemInventory {
    param([string]$FixtureDir, [switch]$UseFixtures)

    $fixturePath = Join-Path $FixtureDir 'inventory.json'
    $useFixture = $UseFixtures -or (-not $IsWindows -and $PSVersionTable.PSVersion.Major -ge 6)
    if (-not $useFixture -and -not (Get-Command Get-CimInstance -ErrorAction SilentlyContinue)) { $useFixture = $true }

    if ($useFixture) {
        if (-not (Test-Path $fixturePath)) { return [pscustomobject]@{ quelle = 'leer' } }
        $inv = Get-Content -Path $fixturePath -Raw | ConvertFrom-Json
        $inv | Add-Member -NotePropertyName quelle -NotePropertyValue 'fixture' -Force
        return $inv
    }

    try {
        $os = Get-CimInstance Win32_OperatingSystem
        $boot = $os.LastBootUpTime
        $volumes = @(Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | ForEach-Object {
                [pscustomobject]@{
                    laufwerk        = $_.DeviceID
                    groesse_gb      = [math]::Round($_.Size / 1GB, 1)
                    frei_gb         = [math]::Round($_.FreeSpace / 1GB, 1)
                    frei_prozent    = if ($_.Size) { [math]::Round(100 * $_.FreeSpace / $_.Size, 1) } else { 0 }
                }
            })
        $haenger = @(Get-Service -ErrorAction SilentlyContinue |
            Where-Object { $_.StartType -eq 'Automatic' -and $_.Status -ne 'Running' } |
            ForEach-Object { [pscustomobject]@{ dienst = $_.Name; anzeigename = $_.DisplayName; status = [string]$_.Status } })
        $dumps = @(@('C:\Windows\MEMORY.DMP') + @(Get-ChildItem 'C:\Windows\Minidump\*.dmp' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }) |
            Where-Object { Test-Path $_ } |
            ForEach-Object {
                $f = Get-Item $_
                [pscustomobject]@{ pfad = $f.FullName; groesse_gb = [math]::Round($f.Length / 1GB, 2); geaendert = $f.LastWriteTime.ToString('yyyy-MM-ddTHH:mm:ss') }
            })
        $hotfixes = @(Get-HotFix -ErrorAction SilentlyContinue | Sort-Object InstalledOn -Descending | Select-Object -First 5 |
            ForEach-Object { [pscustomobject]@{ kb = $_.HotFixID; installiert = if ($_.InstalledOn) { $_.InstalledOn.ToString('yyyy-MM-dd') } else { '?' } } })

        return [pscustomobject]@{
            quelle             = 'live'
            rechner            = $env:COMPUTERNAME
            letzter_start      = $boot.ToString('yyyy-MM-ddTHH:mm:ss')
            uptime_stunden     = [math]::Round(((Get-Date) - $boot).TotalHours, 1)
            volumes            = $volumes
            haengende_dienste  = $haenger
            absturzabbilder    = $dumps
            letzte_updates     = $hotfixes
        }
    }
    catch {
        Write-Warn "Inventar live nicht ermittelbar ($($_.Exception.Message)) — nutze Fixture."
        if (-not (Test-Path $fixturePath)) { return [pscustomobject]@{ quelle = 'leer' } }
        $inv = Get-Content -Path $fixturePath -Raw | ConvertFrom-Json
        $inv | Add-Member -NotePropertyName quelle -NotePropertyValue 'fixture' -Force
        return $inv
    }
}

# --- Ereignisse verdichten: identische Meldungen zusammenfassen ----------------------
# Ein abstürzender Dienst erzeugt dasselbe Ereignis hundertfach. Ungefiltert frisst das
# den Kontext; die INFORMATION ist „N-mal zwischen T1 und T2“. Deterministisch, kein LLM.
function Compress-TriageEvents {
    param([object[]]$Events)
    if (-not $Events -or $Events.Count -eq 0) { return @() }
    $gruppen = $Events | Group-Object { "$($_.id)|$($_.quelle)|$($_.text.Substring(0, [Math]::Min(120, $_.text.Length)))" }
    @($gruppen | ForEach-Object {
            $erst = $_.Group | Sort-Object zeit | Select-Object -First 1
            $letzt = $_.Group | Sort-Object zeit | Select-Object -Last 1
            [pscustomobject]@{
                zeit    = $erst.zeit
                bis     = if ($_.Count -gt 1) { $letzt.zeit } else { $null }
                anzahl  = $_.Count
                id      = $erst.id
                quelle  = $erst.quelle
                level   = $erst.level
                log     = $erst.log
                text    = $erst.text
            }
        } | Sort-Object zeit)
}

# --- Das Skript des Reparatur-Agenten sicher ablegen ----------------------------------
# Der Agent liefert JSON; das Skript wird NICHT ausgeführt, sondern als Datei abgelegt.
# Der Kopf macht unmissverständlich, woher es kommt und dass ein Mensch es prüfen muss.
function Write-RemediationScript {
    param([string]$Path, $Plan, [string]$Rechner)
    $kopf = @"
<#
    VORSCHLAG — NICHT AUTOMATISCH AUSGEFÜHRT.

    Erzeugt von einem agentkit-Agenten, der unter --dry-run lief: jedes verändernde
    Werkzeug (run_shell, write_file, edit_file) war für ihn ein No-Op. Dieses Skript ist
    die EINZIGE Art, wie seine Arbeit das System erreichen kann — und nur, wenn ein
    Mensch es liest und freigibt.

    Rechner:   $Rechner
    Erzeugt:   $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')
    Risiko:    $($Plan.risiko)

    PRÜFEN, DANN AUSFÜHREN:  .\Invoke-WinTriage.ps1 -Apply
#>
Set-StrictMode -Version Latest
`$ErrorActionPreference = 'Stop'

"@
    $body = [string]$Plan.skript
    Set-Content -Path $Path -Value ($kopf + $body) -Encoding utf8
}
