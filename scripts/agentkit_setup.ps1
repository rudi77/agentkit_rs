<#
.SYNOPSIS
    agentkit-Setup für Windows — herunterladen, installieren, konfigurieren.

.DESCRIPTION
    Ein Aufruf, drei Dinge:

      1. `agentkit.exe` aus dem GitHub-Release holen und nach
         %LOCALAPPDATA%\Programs\agentkit\bin legen,
      2. dieses Verzeichnis in den PATH des *Benutzers* aufnehmen (kein Admin,
         kein Installer, keine Uninstall-/COM-Registry-Einträge — nur die
         PATH-Variable des angemeldeten Nutzers, die `-Uninstall` wieder aufräumt),
      3. die Konfiguration unter %USERPROFILE%\.agentkit\config.json anlegen.

    Danach trägt der Anwender dort nur noch seine Azure-Werte ein (endpoint,
    api_key, deployment) — `agentkit config show` prüft, ob es passt.

.PARAMETER Version
    Release-Tag (z. B. v0.1.0) oder 'latest' (Default).

.PARAMETER InstallDir
    Zielverzeichnis (Default: %LOCALAPPDATA%\Programs\agentkit). Die Executable
    landet in dessen Unterordner `bin`.

.PARAMETER NoPath
    PATH unangetastet lassen (die Executable ist dann nur per vollem Pfad erreichbar).

.PARAMETER NoCompletions
    Keine PowerShell-Vervollständigung an $PROFILE anhängen.

.PARAMETER FromSource
    Nicht herunterladen, sondern lokal mit cargo bauen (braucht Rust; ohne Klon
    zusätzlich git). Fallback, solange es noch kein Release gibt.

.PARAMETER Uninstall
    Executable und PATH-Eintrag entfernen. Die Konfiguration bleibt erhalten.

.EXAMPLE
    # Der Normalfall — herunterladen und ausführen:
    irm https://raw.githubusercontent.com/rudi77/fsod/main/scripts/agentkit_setup.ps1 | iex

.EXAMPLE
    # Mit Optionen (iex kann keine Parameter durchreichen -> Scriptblock):
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/rudi77/fsod/main/scripts/agentkit_setup.ps1))) -Version v0.1.0

.EXAMPLE
    .\scripts\agentkit_setup.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\agentkit'),
    [switch]$NoPath,
    [switch]$NoCompletions,
    [switch]$FromSource,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$Repo  = 'rudi77/fsod'
$Asset = 'agentkit-rust-windows-x86_64.exe'
$BinDir = Join-Path $InstallDir 'bin'
$ExePath = Join-Path $BinDir 'agentkit.exe'

function Write-Info($m) { Write-Host "» $m" -ForegroundColor Cyan }
function Write-Ok($m)   { Write-Host "✓ $m" -ForegroundColor Green }
function Write-Warn2($m){ Write-Host "! $m" -ForegroundColor Yellow }
function Have($cmd)     { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

# --------------------------------------------------------------------- PATH
# Der Benutzer-PATH (HKCU\Environment) ist die einzige Stelle, die wir anfassen —
# das ist es, was "in den PATH aufnehmen" unter Windows heißt. Idempotent, ohne Admin.

function Get-UserPathEntries {
    $raw = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $raw) { return @() }
    return @($raw -split ';' | Where-Object { $_ -ne '' })
}

function Add-ToUserPath($dir) {
    $entries = Get-UserPathEntries
    if ($entries -contains $dir) {
        Write-Ok "PATH enthält bereits: $dir"
    } else {
        $new = (@($entries) + $dir) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $new, 'User')
        Write-Ok "PATH (Benutzer) ergänzt um: $dir"
    }
    # Aktuelle Sitzung sofort nutzbar machen (sonst erst nach Neustart der Shell).
    if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
}

function Remove-FromUserPath($dir) {
    $entries = Get-UserPathEntries
    if ($entries -notcontains $dir) { return }
    $new = ($entries | Where-Object { $_ -ne $dir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $new, 'User')
    Write-Ok "PATH-Eintrag entfernt: $dir"
}

# ---------------------------------------------------------------- Uninstall
if ($Uninstall) {
    Write-Info "Deinstalliere agentkit …"
    Remove-FromUserPath $BinDir
    if (Test-Path $InstallDir) {
        try {
            Remove-Item -Recurse -Force $InstallDir
            Write-Ok "Entfernt: $InstallDir"
        } catch {
            Write-Warn2 "Konnte $InstallDir nicht löschen (läuft agentkit noch?): $($_.Exception.Message)"
        }
    } else {
        Write-Warn2 "Nicht installiert: $InstallDir"
    }
    $cfg = Join-Path $env:USERPROFILE '.agentkit'
    if (Test-Path $cfg) { Write-Warn2 "Konfiguration bleibt erhalten: $cfg (bei Bedarf selbst löschen)" }
    Write-Ok 'Fertig.'
    return
}

# ------------------------------------------------------------------ Vorprüfung
if ($PSVersionTable.PSVersion.Major -lt 5) {
    throw "PowerShell 5+ nötig (gefunden: $($PSVersionTable.PSVersion))."
}
# Windows PowerShell 5 spricht ohne diesen Schalter noch TLS 1.0 -> GitHub lehnt ab.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# Läuft die Executable gerade? Dann ist sie gesperrt und das Kopieren schlüge mittendrin fehl.
if ((Test-Path $ExePath) -and (Get-Process -Name 'agentkit' -ErrorAction SilentlyContinue)) {
    throw "agentkit läuft gerade — bitte beenden und erneut ausführen."
}

# ------------------------------------------------------- Executable besorgen
function Install-FromRelease {
    $url = if ($Version -eq 'latest') {
        "https://github.com/$Repo/releases/latest/download/$Asset"
    } else {
        "https://github.com/$Repo/releases/download/$Version/$Asset"
    }
    Write-Info "Lade $Asset ($Version) …"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) "agentkit-$([guid]::NewGuid()).exe"
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
    } catch {
        Remove-Item $tmp -ErrorAction SilentlyContinue
        throw "Download fehlgeschlagen ($url): $($_.Exception.Message)`n" +
              "  Gibt es schon ein Release? Sonst aus dem Quellcode bauen:`n" +
              "  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/$Repo/main/scripts/agentkit_setup.ps1))) -FromSource"
    }
    Move-Item -Force $tmp $ExePath
    Write-Ok "Installiert: $ExePath"
}

function Install-FromSource {
    if (-not (Have 'cargo')) { throw "cargo nicht gefunden. Rust installieren: https://rustup.rs" }

    # Aus einem Klon heraus (Skript liegt in <repo>\scripts\) bauen wir direkt; wird das
    # Skript per `irm | iex` ausgeführt, gibt es kein $PSScriptRoot -> flach klonen.
    $rustDir = if ($PSScriptRoot) { Join-Path (Split-Path -Parent $PSScriptRoot) 'agent_framework_rs' } else { $null }
    $clone = $null
    if (-not ($rustDir -and (Test-Path (Join-Path $rustDir 'Cargo.toml')))) {
        if (-not (Have 'git')) { throw "git nicht gefunden (nötig, um die Quellen zu holen)." }
        $clone = Join-Path ([IO.Path]::GetTempPath()) "agentkit-src-$([guid]::NewGuid())"
        Write-Info "Klone $Repo …"
        git clone --depth 1 "https://github.com/$Repo.git" $clone 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "git clone fehlgeschlagen." }
        $rustDir = Join-Path $clone 'agent_framework_rs'
    }

    Write-Info "Baue agentkit (Release, mit TUI + PDF) — das dauert ein paar Minuten …"
    cargo build --release --manifest-path (Join-Path $rustDir 'Cargo.toml') --bin agentkit --features "tui pdf"
    if ($LASTEXITCODE -ne 0) { throw "cargo build fehlgeschlagen." }

    Copy-Item -Force (Join-Path $rustDir 'target\release\agentkit.exe') $ExePath
    Write-Ok "Installiert: $ExePath"
    if ($clone) { Remove-Item -Recurse -Force $clone -ErrorAction SilentlyContinue }
}

if ($FromSource) { Install-FromSource } else { Install-FromRelease }

# ---------------------------------------------------------------------- PATH
if ($NoPath) {
    Write-Warn2 "PATH unverändert (-NoPath). Aufruf über: $ExePath"
} else {
    Add-ToUserPath $BinDir
}

# -------------------------------------------------------------- Konfiguration
# Die Vorlage kommt aus der Executable selbst (`agentkit config init`) — so gibt es
# genau eine Quelle der Wahrheit für das Config-Format, nicht zwei.
Write-Info "Lege die Konfiguration an …"
& $ExePath config init
$ConfigPath = (& $ExePath config path)

# ---------------------------------------------------------------- Completions
if (-not $NoCompletions) {
    $marker = '# agentkit completions (auto)'
    try {
        if ((Test-Path $PROFILE) -and (Select-String -Path $PROFILE -SimpleMatch $marker -Quiet)) {
            Write-Ok 'PowerShell-Vervollständigung bereits eingerichtet.'
        } else {
            $dir = Split-Path -Parent $PROFILE
            if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
            Add-Content -Path $PROFILE -Value "`n$marker"
            & $ExePath completions powershell | Add-Content -Path $PROFILE
            Write-Ok "PowerShell-Vervollständigung an `$PROFILE angehängt (neue Shell starten)."
        }
    } catch {
        Write-Warn2 "Vervollständigung übersprungen: $($_.Exception.Message)"
    }
}

# ---------------------------------------------------------------- Abschluss
$ver = (& $ExePath --version)
Write-Host ''
Write-Ok "$ver ist installiert."
Write-Host ''
Write-Host 'Noch ein Schritt — Azure-Werte eintragen:' -ForegroundColor White
Write-Host "  notepad `"$ConfigPath`"" -ForegroundColor White
Write-Host '      endpoint    https://<deine-ressource>.openai.azure.com'
Write-Host '      api_key     <dein Azure-API-Key>'
Write-Host '      deployment  <dein Deployment-Name>'
Write-Host ''
Write-Host 'Danach (neue Shell, damit der PATH greift):' -ForegroundColor White
Write-Host '  agentkit config show          # prüft die Konfiguration'
Write-Host '  agentkit "Was ist 17 + 25?"   # One-shot'
Write-Host '  agentkit                      # interaktive Session'
Write-Host '  agentkit --tui                # Terminal-UI'
Write-Host ''
Write-Host 'Ohne Azure-Werte läuft agentkit im netzfreien Demo-Modus.' -ForegroundColor DarkGray
Write-Host "Deinstallieren: agentkit_setup.ps1 -Uninstall" -ForegroundColor DarkGray
