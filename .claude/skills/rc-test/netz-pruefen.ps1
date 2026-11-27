<#
.SYNOPSIS
    Misst, ob der Modell-Endpunkt per TCP erreichbar ist, gegen ein Kontrollziel.

.DESCRIPTION
    Vor jedem Test mit echtem Modell laufen lassen. Ein „Netzwerkfehler“ in
    agentkit mit `os error 10060` ist ein gescheiterter Verbindungsaufbau und
    kein Fehler des Kandidaten — diese Messung zeigt das ohne agentkit.

    Ausgegeben werden nur die Zeitpunkte mit Ausfall, am Ende die Summen und
    der Netzwerkadapter, über den jedes Ziel geroutet wird (VPN oder direkt).

.PARAMETER Endpoint
    Host des Endpunkts. Ohne Angabe aus `agentkit config show` gelesen.

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

# Sonst kommen Umlaute in einer Git-Bash als Ersatzzeichen an.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not $Endpoint) {
    $line = agentkit config show 2>&1 | Select-String 'AZURE_OPENAI_ENDPOINT\s+(https?://\S+)' | Select-Object -First 1
    if (-not $line) { throw 'Kein Endpunkt: -Endpoint HOST angeben (agentkit config show nennt keinen).' }
    $Endpoint = ([uri]$line.Matches[0].Groups[1].Value).Host
}

function Resolve-Ip($name) {
    ([System.Net.Dns]::GetHostAddresses($name) | Where-Object AddressFamily -eq 'InterNetwork' | Select-Object -First 1).ToString()
}

$targets = [ordered]@{ 'endpunkt' = (Resolve-Ip $Endpoint); 'kontrolle' = (Resolve-Ip $Control) }
"Endpunkt  $Endpoint -> $($targets['endpunkt'])"
"Kontrolle $Control -> $($targets['kontrolle'])"
foreach ($k in $targets.Keys) {
    $route = Find-NetRoute -RemoteIPAddress $targets[$k] -ErrorAction SilentlyContinue | Select-Object -First 1
    "Route {0,-9} über {1}" -f $k, $route.InterfaceAlias
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
if ($stat['endpunkt'].fail -gt 0 -and $stat['kontrolle'].fail -eq 0) {
    'Befund: Der Endpunkt fällt aus, die Kontrolle nicht — das Problem liegt auf dem Weg zum Endpunkt, nicht bei agentkit.'
    exit 1
}
if ($stat['endpunkt'].fail -gt 0) { 'Befund: Beide Ziele fallen aus — das Netz dieses Rechners ist gestört.'; exit 1 }
'Befund: Endpunkt durchgehend erreichbar.'
