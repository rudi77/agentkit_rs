<#
    Hilfsfunktionen für logwatch (dot-sourced von Invoke-LogWatch.ps1).

    Reines PowerShell. Zwei Dinge, die deterministisch sind und deshalb KEIN Modell brauchen:

      1. Welche Zeilen sind NEU?   -> Zeilen-Offset je Datei in state/offsets.json (wie `tail -f`).
      2. Welcher Logtyp ist das?   -> Erkennung am Kopf/Namen der Datei.

    Alles andere — was davon eine Anomalie ist, was schon gemeldet wurde, was Rauschen ist —
    ist Urteilsarbeit und gehört dem Agenten (mit --skills und --memory).
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
function Write-Neu($m) { Write-Host "  [NEU] $m" -ForegroundColor Yellow }
function Write-Ruhig($m) { Write-Host "  [--] $m" -ForegroundColor DarkGray }
function Write-Fail($m) { Write-Host "  [!!] $m" -ForegroundColor Red }

# --- Logtyp erkennen: bestimmt, WELCHEN Skill der Agent laden soll --------------------
# Bewusst simpel und deterministisch — der Agent soll den Skill WÄHLEN, nicht raten müssen,
# womit er es zu tun hat.
function Get-LogType {
    param([string]$Path)
    $name = (Split-Path -Leaf $Path).ToLower()
    if ($name -like '*iis*' -or $name -like 'u_ex*') { return 'iis' }
    if ($name -like '*postgres*' -or $name -like '*pg*') { return 'postgres' }
    if ($name -like '*evt*' -or $name -like '*eventlog*') { return 'windows-eventlog' }

    # Fallback: in die Datei schauen.
    $kopf = Get-Content -Path $Path -TotalCount 20 -ErrorAction SilentlyContinue
    if ($kopf -match '^#Software: Microsoft Internet Information Services') { return 'iis' }
    if ($kopf -match 'LOG:|FATAL:|STATEMENT:') { return 'postgres' }
    return 'unbekannt'
}

# --- Offsets: was wurde in einem früheren Lauf schon gelesen? ------------------------
function Get-OffsetStore {
    param([string]$StateDir)
    $p = Join-Path $StateDir 'offsets.json'
    if (-not (Test-Path $p)) { return @{} }
    $obj = Get-Content -Path $p -Raw | ConvertFrom-Json
    $h = @{}
    foreach ($prop in $obj.PSObject.Properties) { $h[$prop.Name] = [int]$prop.Value }
    return $h
}

function Save-OffsetStore {
    param([string]$StateDir, [hashtable]$Store)
    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    ([pscustomobject]$Store | ConvertTo-Json) | Set-Content -Path (Join-Path $StateDir 'offsets.json') -Encoding utf8
}

# --- Die neuen Zeilen einer Datei (wie `tail -f`) ------------------------------------
# Gibt @{ neu = @(...); gesamt = N; ab_zeile = M } zurück. Schrumpft die Datei (Rotation),
# wird von vorn gelesen.
function Get-NewLines {
    param([string]$Path, [hashtable]$Store, [switch]$Replay)

    $key = Split-Path -Leaf $Path
    $alle = @(Get-Content -Path $Path -ErrorAction Stop)
    $gesehen = if ($Replay) { 0 } elseif ($Store.ContainsKey($key)) { $Store[$key] } else { 0 }

    # Logrotation: Datei ist kürzer als beim letzten Mal -> von vorn.
    if ($gesehen -gt $alle.Count) { $gesehen = 0 }

    # Direkte Zuweisung, KEIN `$neu = if (…) { @(…) }`: Die Ausgabe eines if-Blocks läuft
    # über den Pipeline-Stream, und der entpackt ein einelementiges Array zum Skalar. Aus
    # @('zeile4') würde die Zeichenkette 'zeile4' — und `.neu[0]` läge dann bei 'z'.
    $neu = @()
    if ($gesehen -lt $alle.Count) { $neu = @($alle[$gesehen..($alle.Count - 1)]) }

    # Bewusst eine Hashtable und KEIN [pscustomobject]: dessen Konvertierung entpackt
    # ebenfalls Arrays — ein einelementiges würde zum String, ein leeres zu $null (und
    # `@($null).Count` ist 1, nicht 0). Beides sind genau die Fälle des Dauerbetriebs:
    # eine einzelne neue Logzeile bzw. gar keine. Eine Hashtable gibt zurück, was man
    # hineingelegt hat.
    @{
        datei    = $key
        neu      = $neu
        gesamt   = $alle.Count
        ab_zeile = $gesehen + 1
    }
}

# --- Kommentarzeilen raus (IIS-Header etc.) ------------------------------------------
function Remove-LogComments {
    param([string[]]$Lines)
    @($Lines | Where-Object { $_ -and -not $_.StartsWith('#') })
}

# --- Ins Langzeitgedächtnis schreiben ------------------------------------------------
# Das Format ist das von agentkits LongTermMemory (src/memory.rs): eine JSON-Zeile je
# Eintrag mit `text` und `tags`. Beim nächsten agentkit-Start wird die Datei geladen, und
# `recall` findet darin per Stichwort-Überlappung.
#
# WARUM schreibt die Pipeline und nicht der Agent? Weil `recall` innerhalb EINES Laufs
# sofort sieht, was `remember` gerade geschrieben hat. Ein Agent, der pro Befund erst merkt
# und dann abfragt, findet seine eigenen frischen Einträge und hält alles für „schon
# bekannt“ — genau das ist beim ersten Entwurf dieses Beispiels passiert. Urteilen ist
# Sache des Agenten; Buch führen ist deterministisch und gehört hierher.
function Add-ToMemory {
    param([string]$MemoryPath, [object[]]$Befunde, [string]$Datum)

    if (-not $Befunde -or $Befunde.Count -eq 0) { return 0 }
    $dir = Split-Path -Parent $MemoryPath
    if ($dir) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }

    $n = 0
    foreach ($b in $Befunde) {
        if (-not $b.signatur) { continue }
        $ausgang = if ($b.ausgang) { " Ausgang: $($b.ausgang)." } else { '' }
        $text = "GEMELDET am ${Datum}: $($b.signatur) — $($b.was)$ausgang Bereits berichtet, nicht erneut melden."

        # Tags = die Wörter der Signatur. `recall` bewertet über Stichwort-Überlappung von
        # Text UND Tags — die Signaturwörter sind damit der Schlüssel zum Wiederfinden.
        $tags = @($b.signatur -split '[^\w\./]+' | Where-Object { $_.Length -gt 2 } | ForEach-Object { $_.ToLower() })

        $eintrag = [pscustomobject]@{ text = $text; tags = $tags }
        Add-Content -Path $MemoryPath -Value ($eintrag | ConvertTo-Json -Compress) -Encoding utf8
        $n++
    }
    return $n
}

# --- Bekanntes Rauschen einmalig ins Gedächtnis säen ----------------------------------
# Damit der allererste Lauf nicht das halbe Internet-Grundrauschen meldet. Diese Einträge
# stehen so auch in den Skills — hier landen sie im Gedächtnis, damit `recall` sie findet.
function Initialize-NoiseMemory {
    param([string]$MemoryPath)
    if (Test-Path $MemoryPath) { return 0 }
    $rauschen = @(
        @{ t = 'RAUSCHEN: HTTP 404 auf /favicon.ico — normales Browser-Verhalten, niemals melden.'; g = @('404', 'favicon.ico', 'favicon') },
        @{ t = 'RAUSCHEN: HTTP 404 auf /robots.txt — normales Crawler-Verhalten, niemals melden.'; g = @('404', 'robots.txt', 'robots') },
        @{ t = 'RAUSCHEN: HTTP 404 von Scannern auf /wp-login.php oder /.env — Internet-Grundrauschen, solange kein 2xx zurueckkommt.'; g = @('404', 'wp-login.php', 'scanner', 'env') },
        @{ t = 'RAUSCHEN: PostgreSQL checkpoint starting/complete — Routine, niemals melden.'; g = @('checkpoint', 'postgresql', 'postgres') },
        @{ t = 'RAUSCHEN: PostgreSQL automatic vacuum / autovacuum — Routine, niemals melden.'; g = @('vacuum', 'autovacuum', 'postgresql', 'postgres') }
    )
    foreach ($r in $rauschen) {
        $eintrag = [pscustomobject]@{ text = $r.t; tags = $r.g }
        Add-Content -Path $MemoryPath -Value ($eintrag | ConvertTo-Json -Compress) -Encoding utf8
    }
    return $rauschen.Count
}
