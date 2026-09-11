<#
.SYNOPSIS
  Startet agentkit als Wissens-Agent für OKF-Bündel.

.DESCRIPTION
  Setzt die drei Dinge zusammen, die dieser Agent braucht, und nichts sonst:

    --profile     ersetzt den eingebauten Coding-Prompt durch okf-agent.md
    --skills      die OKF-Skill-Sammlung (list_skills/read_skill)
    --allow-read  dieselbe Sammlung nur-lesbar, weil ihre Skills auf eigene
                  Referenzdateien außerhalb des Bündels verweisen

  Geschrieben wird ausschließlich in -Bundle. Weitere Lesequellen (ein Wiki-Clone,
  ein Export-Verzeichnis) gibst du mit -ReadAlso an.

.EXAMPLE
  .\Invoke-Okf.ps1 -Bundle D:\repos\Wissen -Skills D:\src\okf-skills `
      -Task "Dokumentiere den Rechnungs-Import in unserer Wissensbasis."

.EXAMPLE
  .\Invoke-Okf.ps1 -Bundle D:\repos\Wissen -Skills D:\src\okf-skills `
      -ReadAlso D:\repos\Alt.wiki,D:\tmp\import -Yes `
      -Task "Importiere das Wiki unter D:/repos/Alt.wiki mit dem wiki-import Skill."
#>
[CmdletBinding()]
param(
    # Zielverzeichnis des Bündels — das EINZIGE Verzeichnis, in das geschrieben wird.
    [Parameter(Mandatory)] [string]   $Bundle,
    # Wurzel des okf-skills-Checkouts (enthält skills/ und agents/).
    [Parameter(Mandatory)] [string]   $Skills,
    # Der Auftrag. Genau eines von beiden: -Task für eine Zeile, -TaskFile für
    # einen mehrzeiligen Auftrag (der Normalfall bei einem Import — und das
    # einzige, was zuverlässig funktioniert, wenn der Aufruf aus einer anderen
    # Shell kommt, die PowerShell-Ausdrücke nicht auswertet).
    [string]   $Task,
    [string]   $TaskFile,
    # Zusätzliche NUR-LESBARE Quellen (Wiki-Clone, Export-Verzeichnis, …).
    [string[]] $ReadAlso = @(),
    # Ohne Rückfrage ausführen. Nur in einer Umgebung, der du vertraust.
    [switch]   $Yes,
    # Verlaufsdatei. Der wiki-import-Skill legt seinen Split-/Merge-Plan VOR der
    # Transformation vor und wartet auf Freigabe - im One-Shot-Lauf gibt es aber
    # niemanden, der antwortet. Mit -Session laeuft der zweite Aufruf im selben
    # Gespraech weiter, und die Freigabe ist einfach der naechste Auftrag.
    [string]   $Session,
    [string]   $Agentkit = "agentkit"
)

$ErrorActionPreference = "Stop"
$hier = Split-Path -Parent $MyInvocation.MyCommand.Path

if ($TaskFile) {
    if (-not (Test-Path $TaskFile)) { throw "-TaskFile '$TaskFile' existiert nicht." }
    $Task = Get-Content -Raw $TaskFile
}
if ([string]::IsNullOrWhiteSpace($Task)) { throw "Gib einen Auftrag an: -Task oder -TaskFile." }
if (-not (Test-Path (Join-Path $Skills "skills"))) {
    throw "unter '$Skills' liegt kein 'skills'-Verzeichnis. Zeigt -Skills auf die Wurzel des okf-skills-Checkouts?"
}
New-Item -ItemType Directory -Force -Path $Bundle | Out-Null

$argumente = @(
    "--profile",    (Join-Path $hier "profile.json")
    "-w",           $Bundle
    "--skills",     (Join-Path $Skills "skills")
    "--agents",     (Join-Path $Skills "agents")
    "--allow-read", $Skills
)
foreach ($pfad in $ReadAlso) { $argumente += @("--allow-read", $pfad) }
if ($Session) { $argumente += @("--session", $Session) }
if ($Yes) { $argumente += "-y" }
$argumente += @("--steps", $Task)

# `system_file` im Profil wird relativ zum Arbeitsverzeichnis gelesen — deshalb
# von hier aus starten, nicht vom Bündel aus.
$start = Get-Date
Push-Location $hier
try {
    # `$null |` schliesst stdin sofort. Ohne das liest agentkit bei nicht-TTY
    # stdin bis EOF (dort landet gepipter Kontext) - und ein Aufruf aus einem
    # Hintergrundjob, dessen stdin geerbt und nie geschlossen wird, HAENGT
    # dann ohne jede Ausgabe. Kostet hier nichts: dieser Wrapper reicht
    # keinen Kontext ueber stdin herein.
    $null | & $Agentkit @argumente
    $code = $LASTEXITCODE
} finally { Pop-Location }

# Ein Import-Lauf dauert Minuten bis Stunden - die Dauer gehoert deshalb zum
# Ergebnis, nicht in eine Stoppuhr, die der Aufrufer jedes Mal selbst
# drumherum baut. Auf stderr, damit sie eine `| Tee-Object`-Pipeline auf
# stdout nicht verunreinigt.
$dauer = (Get-Date) - $start
$text = "Fertig in {0:hh\:mm\:ss} - Exit {1}" -f $dauer, $code
if ($code -eq 0) { Write-Host "OK  $text" -ForegroundColor Green }
else             { Write-Host "!!  $text" -ForegroundColor Yellow }

# Exit-Code des Agenten durchreichen, sonst ist der Wrapper in einer Pipeline
# nutzlos: agentkit unterscheidet 0 ok, 1 Laufzeitfehler, 2 API/Netz,
# 3 Kontext/Prompt, 4 --format nicht erfuellbar.
exit $code
