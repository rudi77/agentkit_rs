<#
.SYNOPSIS
    agentkit lokal aus dem Quellcode bauen — inklusive TUI, Wissensgraph und
    Context-Management.

.DESCRIPTION
    Baut die `agentkit`-Executable aus `agentkit_app` mit denselben
    Feature-Kombinationen, die auch das Release baut
    (.github/workflows/release.yml):

        voll : tui pdf ctxman tiktoken graph   — der interaktive Alltag
        cli  : pdf ctxman tiktoken graph       — ohne ratatui, für Skripte

    Im Unterschied zu `install.ps1` (das per `cargo install` in den PATH legt)
    bleibt das Ergebnis hier im `target/`-Verzeichnis des Repos — gedacht zum
    Entwickeln und Testen. Mit -Install wandert es zusätzlich in den PATH.

    Das Repo hat KEINEN Cargo-Workspace im Root; jeder Aufruf geht deshalb über
    --manifest-path.

    Nach dem Bau laufen dieselben Rauchtests wie im Release-Workflow: Demo-Lauf,
    ctxman-Snapshot, `swarm`-Tool, Graph-Tools, und bei der cli-Variante die
    Prüfung, dass `--tui` sich selbst abweist.

.PARAMETER Variant
    'voll' (Default) oder 'cli'. Wird von -Features überstimmt.

.PARAMETER Features
    Eigene Feature-Liste, space-getrennt, z. B. "tui graph". Überstimmt -Variant.

.PARAMETER Dev
    Debug-Build statt Release — deutlich schneller kompiliert, langsamer zur
    Laufzeit. Für den Entwicklungszyklus.

.PARAMETER Test
    Vor dem Bau die Testsuiten der beteiligten Crates laufen lassen (offline).

.PARAMETER Lint
    Zusätzlich `cargo clippy` über die beteiligten Crates.

.PARAMETER Install
    Nach erfolgreichem Bau zusätzlich `cargo install` in den PATH.

.PARAMETER Run
    Nach erfolgreichem Bau das TUI starten (nur mit Feature `tui`).

.PARAMETER SkipSmoke
    Die Rauchtests überspringen.

.EXAMPLE
    .\scripts\build.ps1
    Volle Variante als Release-Build, mit Rauchtests.

.EXAMPLE
    .\scripts\build.ps1 -Dev -Run
    Schneller Debug-Build, danach startet das TUI.

.EXAMPLE
    .\scripts\build.ps1 -Variant cli -Test
    Schlanke Variante, vorher alle Tests.

.EXAMPLE
    .\scripts\build.ps1 -Features "tui graph" -SkipSmoke
#>
[CmdletBinding()]
param(
    [ValidateSet('voll', 'cli')]
    [string]$Variant = 'voll',
    [string]$Features,
    [switch]$Dev,
    [switch]$Test,
    [switch]$Lint,
    [switch]$Install,
    [switch]$Run,
    [switch]$SkipSmoke
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 gibt sonst UTF-8-Ausgaben (dieses Skript UND agentkit)
# als Zeichensalat aus ("Â»" statt "»").
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$AppManifest = Join-Path $RepoRoot 'agentkit_app\Cargo.toml'

function Write-Info($m) { Write-Host "» $m" -ForegroundColor Cyan }
function Write-Ok($m) { Write-Host "  ok: $m" -ForegroundColor Green }
function Write-Warn2($m) { Write-Host "  ! $m" -ForegroundColor Yellow }

# stdout UND stderr eines nativen Programms einsammeln, ohne dass PowerShell
# daraus einen Fehler macht.
#
# Windows PowerShell 5.1 wirft bei `$ErrorActionPreference = 'Stop'` für JEDE
# stderr-Zeile eines nativen Programms einen NativeCommandError — und agentkit
# schreibt seinen Banner planmäßig auf stderr. Ohne das Herabsetzen bräche der
# Rauchtest also ab, obwohl nichts falsch ist.
function Invoke-Capture {
    param([string[]]$ExeArgs, $StdIn = $null)
    $vorher = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        # `2>&1` liefert stderr-Zeilen als ErrorRecord-Objekte; `.ToString()`
        # holt den reinen Text, sonst rendert PowerShell jede Zeile mit
        # "At C:\...", "CategoryInfo" und Konsorten in die Ausgabe.
        #
        # stdin wird als PARAMETER übergeben und erst hier drinnen gepipt: eine
        # Pipe IN diese Funktion landet in deren `$input` und erreicht das native
        # Programm nie — `agentkit --repl` quittierte das mit "Eingabefehler:
        # Incorrect function. (os error 1)".
        #
        # IMMER pipen, auch ohne Eingabe (`$null`): agentkit liest ein nicht-TTY
        # stdin bis EOF. Ein geerbtes, nie geschlossenes stdin lässt den Aufruf
        # hängen — siehe agent_framework_rs/CLAUDE.md, "Always pipe something".
        $zeilen = $StdIn | & $Exe @ExeArgs 2>&1
        return (($zeilen | ForEach-Object { $_.ToString() }) -join "`n")
    }
    finally {
        $ErrorActionPreference = $vorher
    }
}

# Native Kommandos melden Fehler über den Exit-Code, nicht über Exceptions —
# $ErrorActionPreference greift dort nicht.
function Invoke-Checked {
    param([string]$What, [string[]]$CargoArgs)
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) { throw "$What fehlgeschlagen (Exit-Code $LASTEXITCODE)" }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo nicht gefunden. Rust installieren: https://rustup.rs'
}

# ------------------------------------------------------------------ Features

$FeatureSets = @{
    voll = 'tui pdf ctxman tiktoken graph'
    cli  = 'pdf ctxman tiktoken graph'
}
if (-not $Features) { $Features = $FeatureSets[$Variant] }

$HasTui = $Features -match '(^|\s)tui(\s|$)'
$HasGraph = $Features -match '(^|\s)graph(\s|$)'
# tiktoken zieht ctxman mit (siehe agentkit_app/Cargo.toml).
$HasCtxman = $Features -match '(^|\s)(ctxman|tiktoken)(\s|$)'

$BuildProfile = if ($Dev) { 'debug' } else { 'release' }

# Das Zielverzeichnis von cargo ERFRAGEN statt es zu raten: `[build] target-dir`
# in ~/.cargo/config.toml oder $env:CARGO_TARGET_DIR verschiebt es aus dem Repo
# heraus (üblich, wenn mehrere Crates ohne gemeinsamen Workspace sonst jede
# Abhängigkeit mehrfach bauen). Ein fest angenommener Pfad wäre dann falsch.
$TargetRoot = (cargo metadata --no-deps --format-version 1 --manifest-path $AppManifest |
    ConvertFrom-Json).target_directory
if ($LASTEXITCODE -ne 0 -or -not $TargetRoot) {
    throw 'cargo metadata lieferte kein target_directory'
}
$Exe = Join-Path $TargetRoot "$BuildProfile\agentkit.exe"

Write-Info "Variante '$Variant' | Features: $Features | Profil: $BuildProfile"

# --------------------------------------------------------------------- Tests

if ($Test) {
    Write-Info 'Tests (offline, ohne Netz)…'
    Invoke-Checked 'agentkit-Tests' @(
        'test', '--manifest-path', (Join-Path $RepoRoot 'agent_framework_rs\Cargo.toml'),
        '--no-default-features')
    Invoke-Checked 'agentkit-swarm-Tests' @(
        'test', '--manifest-path', (Join-Path $RepoRoot 'agentkit_swarm\Cargo.toml'))
    if ($HasCtxman) {
        Invoke-Checked 'ctxman-Tests' @(
            'test', '--manifest-path', (Join-Path $RepoRoot 'ctxman_rs\Cargo.toml'))
    }
    if ($HasGraph) {
        Invoke-Checked 'agentkit-graph-Tests' @(
            'test', '--manifest-path', (Join-Path $RepoRoot 'agentkit_graph\Cargo.toml'))
    }
    $appTest = @('test', '--manifest-path', $AppManifest, '--no-default-features')
    if ($HasGraph) { $appTest += @('--features', 'graph') }
    Invoke-Checked 'agentkit_app-Tests' $appTest
    Write-Ok 'alle Testsuiten grün'
}

# ---------------------------------------------------------------------- Lint

if ($Lint) {
    Write-Info 'clippy…'
    foreach ($crate in @('agent_framework_rs', 'agentkit_swarm', 'agentkit_graph', 'agentkit_app')) {
        Invoke-Checked "clippy ($crate)" @(
            'clippy', '--manifest-path', (Join-Path $RepoRoot "$crate\Cargo.toml"), '--all-targets')
    }
    Write-Ok 'clippy sauber'
}

# ---------------------------------------------------------------------- Bauen

Write-Info "Baue 'agentkit'…"
$buildArgs = @('build', '--manifest-path', $AppManifest, '--bin', 'agentkit', '--features', $Features)
if (-not $Dev) { $buildArgs += '--release' }
Invoke-Checked 'Build agentkit' $buildArgs

# Das separate `tui`-Binary ist eine Entwickler-Bequemlichkeit; der reguläre Weg
# ins TUI ist `agentkit --tui`. Nur bauen, wenn das Feature überhaupt da ist.
if ($HasTui) {
    Write-Info "Baue 'tui'…"
    $tuiArgs = @('build', '--manifest-path', $AppManifest, '--bin', 'tui', '--features', $Features)
    if (-not $Dev) { $tuiArgs += '--release' }
    Invoke-Checked 'Build tui' $tuiArgs
}

if (-not (Test-Path $Exe)) { throw "Binary nicht gefunden: $Exe" }
$sizeMb = [math]::Round((Get-Item $Exe).Length / 1MB, 1)
Write-Ok "$Exe ($sizeMb MB)"

# ----------------------------------------------------------------- Rauchtests

function Invoke-Smoke {
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) "agentkit-smoke-$PID"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        # 1) Startet die Binary und läuft der Loop? Demo-Modus, kein Netz, kein Key.
        Write-Info 'Rauchtest: Demo-Lauf…'
        $out = Invoke-Capture @('--demo', 'Was ist 17 + 25?')
        if ($out -notmatch '42') { throw "Demo-Lauf lieferte keine 42:`n$out" }
        Write-Ok 'Demo-Loop läuft'

        # 2) Ohne Feature quittiert --ctx nur mit einer Warnung ("ohne Feature").
        if ($HasCtxman) {
            Write-Info 'Rauchtest: ctxman…'
            $ctxDir = Join-Path $tmp 'ctx'
            $out = Invoke-Capture @('--demo', '--ctx', $ctxDir, 'Was ist 2 + 2?')
            if ($out -match 'ohne Feature') { throw "Binary ohne ctxman gebaut:`n$out" }
            if (-not (Test-Path $ctxDir)) { throw 'ctxman legte keinen Snapshot an' }
            Write-Ok 'ctxman aktiv'
        }

        # 3) `/tools` baut den Coding-Agenten und listet die Registry, ohne das
        #    Modell zu befragen — kostet also nichts. NICHT mit --demo: der
        #    Demo-Modus ersetzt die Registry durch Spielzeug-Tools (add, reverse,
        #    wetter) und hat gar keinen Coding-Agenten. Der Dummy-Key sorgt dafür,
        #    dass auch ohne konfigurierte Zugangsdaten nicht der Demo-Modus greift.
        Write-Info 'Rauchtest: swarm- und Graph-Tools…'
        $savedKey = $env:OPENAI_API_KEY
        $savedModel = $env:OPENAI_MODEL
        try {
            if (-not $env:OPENAI_API_KEY) {
                $env:OPENAI_API_KEY = 'sk-smoketest'
                $env:OPENAI_MODEL = 'gpt-4o'
            }
            $wsDir = Join-Path $tmp 'ws'
            New-Item -ItemType Directory -Force -Path $wsDir | Out-Null
            $replArgs = @('--repl', '-w', $wsDir)
            if ($HasGraph) { $replArgs += @('--graph', (Join-Path $tmp 'graph')) }
            $out = Invoke-Capture $replArgs -StdIn "/tools`n/exit`n"

            if ($out -match 'add, reverse, wetter') {
                Write-Warn2 'Demo-Modus statt Coding-Agent — Tool-Check übersprungen (keine Zugangsdaten?)'
            }
            else {
                if ($out -notmatch '\bswarm\b') { throw "kein swarm-Tool im Build:`n$out" }
                Write-Ok 'swarm-Tool aktiv'
                if ($HasGraph) {
                    if ($out -match 'ohne Feature') { throw "Binary ohne graph gebaut:`n$out" }
                    foreach ($t in @('graph_search', 'graph_remember')) {
                        if ($out -notmatch $t) { throw "kein $t im Build:`n$out" }
                    }
                    Write-Ok 'Wissensgraph aktiv'
                }
            }
        }
        finally {
            $env:OPENAI_API_KEY = $savedKey
            $env:OPENAI_MODEL = $savedModel
        }

        # 4) Die schlanke Variante darf kein TUI enthalten — sonst wäre sie
        #    versehentlich doch die volle. Ohne das Feature weist `--tui` sich
        #    selbst ab und kehrt zurück; die volle Variante würde hier ein UI
        #    starten und hängen, deshalb nur für die cli-Variante.
        if (-not $HasTui) {
            Write-Info 'Rauchtest: kein TUI in der schlanken Variante…'
            $out = Invoke-Capture @('--tui')
            if ($out -notmatch 'kein TUI') { throw "cli-Variante enthält doch ein TUI:`n$out" }
            Write-Ok '--tui wird korrekt abgewiesen'
        }
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

if (-not $SkipSmoke) { Invoke-Smoke } else { Write-Warn2 'Rauchtests übersprungen' }

# ------------------------------------------------------------------ Installieren

if ($Install) {
    Write-Info 'cargo install in den PATH…'
    Invoke-Checked 'cargo install' @(
        'install', '--path', (Join-Path $RepoRoot 'agentkit_app'),
        '--bin', 'agentkit', '--features', $Features, '--force')
    Write-Ok "installiert nach $(Join-Path $env:USERPROFILE '.cargo\bin\agentkit.exe')"
}

# ---------------------------------------------------------------------- Fertig

Write-Host ''
Write-Host 'Fertig.' -ForegroundColor Green
Write-Host "  Binary : $Exe"
Write-Host "  Features: $Features"
Write-Host ''
if ($HasTui) {
    Write-Host '  TUI starten     : ' -NoNewline; Write-Host "$Exe --tui" -ForegroundColor White
    Write-Host '  (Schwarm ist ab Werk an; --no-swarm schaltet ihn aus.)'
}
Write-Host '  Werkzeuge sehen : ' -NoNewline; Write-Host "$Exe --repl" -ForegroundColor White
if ($HasGraph) {
    # Die schlanke Variante hat kein TUI — dort auf den REPL verweisen.
    $graphMode = if ($HasTui) { '--tui' } else { '--repl' }
    Write-Host '  Mit Graph       : ' -NoNewline; Write-Host "$Exe $graphMode --graph .agentkit-graph" -ForegroundColor White
}

if ($Run) {
    if (-not $HasTui) { throw 'kein TUI in diesem Build — -Run braucht das Feature tui' }
    Write-Host ''
    Write-Info 'Starte TUI…'
    & $Exe --tui
}
