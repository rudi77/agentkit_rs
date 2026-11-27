#Requires -Version 7
<#
.SYNOPSIS
    Misst, ob der Modell-Endpunkt per TCP erreichbar ist, gegen ein Kontrollziel.

.DESCRIPTION
    Vor jedem Test mit echtem Modell laufen lassen. Ein „Netzwerkfehler“ in
    agentkit mit `os error 10060` ist ein gescheiterter Verbindungsaufbau und
    kein Fehler des Kandidaten — diese Messung zeigt das ohne agentkit.

    Ausgegeben werden nur die Zeitpunkte mit Ausfall, am Ende die Summen und
    der Netzwerkadapter, über den jedes Ziel geroutet wird (VPN oder direkt).
    Zum Kontrollziel wird nur eine TCP-Verbindung geöffnet; gesendet wird nichts.

    Exit 0 nur, wenn der Endpunkt in jeder Probe erreichbar war. Exit 1 bei
    jedem Ausfall und auch dann, wenn sich nichts messen ließ (Name nicht
    auflösbar, keine Probe).

.PARAMETER Endpoint
    Hostname des Endpunkts (keine URL). Ohne Angabe wird AZURE_OPENAI_ENDPOINT
    aus `agentkit config show` gelesen; für andere Anbieter den Host angeben.

.PARAMETER Minutes
    Dauer der Messung (Default: 2).

.EXAMPLE
    pwsh .claude/skills/rc-test/netz-pruefen.ps1 -Minutes 3
#>
[CmdletBinding()]
param(
    [string]$Endpoint,
    [double]$Minutes = 2,
    [string]$Control = '1.1.1.1'
)

$ErrorActionPreference = 'Stop'
# Sonst kommen Umlaute in einer Git-Bash als Ersatzzeichen an.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not $Endpoint) {
    if (-not (Get-Command agentkit -ErrorAction SilentlyContinue)) {
        throw 'agentkit ist nicht im PATH: -Endpoint HOST angeben.'
    }
    $line = agentkit config show 2>&1 | Select-String 'AZURE_OPENAI_ENDPOINT\s+(https?://\S+)' | Select-Object -First 1
    if (-not $line) { throw 'Kein Azure-Endpunkt in `agentkit config show`: -Endpoint HOST angeben.' }
    $Endpoint = ([uri]$line.Matches[0].Groups[1].Value).Host
}

# Eine IPv4-Adresse oder $null — ein nicht auflösbarer Name ist ein Befund,
# kein Abbruch mit einer .NET-Ausnahme.
function Resolve-Ip($name) {
    try {
        $ip = [System.Net.Dns]::GetHostAddresses($name) |
            Where-Object AddressFamily -eq 'InterNetwork' | Select-Object -First 1
        if ($ip) { return $ip.ToString() }
    } catch {}
    return $null
}

$targets = [ordered]@{ 'endpunkt' = (Resolve-Ip $Endpoint); 'kontrolle' = (Resolve-Ip $Control) }
"Endpunkt  $Endpoint -> $($targets['endpunkt'])"
"Kontrolle $Control -> $($targets['kontrolle'])"
foreach ($k in $targets.Keys) {
    if (-not $targets[$k]) {
        "Befund: $k lässt sich nicht in eine IPv4-Adresse auflösen — nichts gemessen. Ohne DNS (etwa bei getrenntem VPN) ist auch der Endpunkt nicht erreichbar."
        exit 1
    }
    # Find-NetRoute gibt es nur unter Windows; die Route ist Zusatzinformation.
    $route = Find-NetRoute -RemoteIPAddress $targets[$k] -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($route) { "Route {0,-9} über {1}" -f $k, $route.InterfaceAlias }
}
''

$stat = @{}; foreach ($k in $targets.Keys) { $stat[$k] = @{ ok = 0; fail = 0 } }
$end = (Get-Date).AddMinutes($Minutes)
while ((Get-Date) -lt $end) {
    $line = Get-Date -Format 'HH:mm:ss'
    $tries = foreach ($k in $targets.Keys) {
        $client = [System.Net.Sockets.TcpClient]::new()
        [pscustomobject]@{ k = $k; c = $client; t = $client.ConnectAsync($targets[$k], 443) }
    }
    Start-Sleep -Seconds 3
    $bad = $false
    foreach ($try in $tries) {
        $ok = $try.t.IsCompletedSuccessfully -and $try.c.Connected
        if ($ok) { $stat[$try.k].ok++ } else { $stat[$try.k].fail++; $bad = $true }
        $line += '  {0}={1}' -f $try.k, $(if ($ok) { 'ok' } else { 'FEHL' })
        $try.c.Dispose()
    }
    if ($bad) { $line }
}

''
foreach ($k in $targets.Keys) { '{0,-9} ok={1} FEHL={2}' -f $k, $stat[$k].ok, $stat[$k].fail }
$e = $stat['endpunkt']; $c = $stat['kontrolle']
if ($e.ok + $e.fail -eq 0) {
    'Befund: keine Probe gelaufen (-Minutes zu klein) — nichts gemessen.'
    exit 1
}
if ($e.fail -eq 0) { 'Befund: Endpunkt durchgehend erreichbar.'; exit 0 }
if ($c.ok -eq 0) {
    "Befund: Der Endpunkt fällt aus, und die Kontrolle $Control war nie erreichbar (in diesem Netz gesperrt?). Mit -Control ein erreichbares Ziel angeben, sonst lässt sich die Ursache nicht eingrenzen."
} elseif ($c.fail -eq 0) {
    'Befund: Der Endpunkt fällt aus, die Kontrolle nicht — das Problem liegt auf dem Weg zum Endpunkt, nicht bei agentkit.'
} else {
    'Befund: Beide Ziele fallen aus — das Netz dieses Rechners ist gestört.'
}
exit 1
