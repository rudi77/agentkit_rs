# review_pr.ps1 — Automatisches PR-Review für Azure DevOps mit agentkit.
#
# Ablauf (Pipeline-Prinzip wie examples/accounts_payable):
#   1. FETCH   PR-Metadaten per REST, Branches lokal fetchen (deterministisch)
#   2. REVIEW  agentkit one-shot mit Review-Profil -> strukturiertes JSON
#   3. ACT     Findings als Kommentar-Threads posten; Vote setzt NICHT der
#              Agent, sondern ein deterministisches Policy-Gate.
#
# Auth: PAT mit Scope "Code (Read & Write)" in $env:ADO_PAT (oder -Pat).
# Kein MCP nötig — der Agent läuft read-only mit --no-mcp; nur dieses Skript
# spricht schreibend mit Azure DevOps.
#
# Beispiele:
#   Echter PR:   ./review_pr.ps1 -Org myorg -Project Proj -Repo repo -PrId 123 `
#                    -RepoPath C:\src\repo -DryRun
#   Nur lokal (ohne ADO, zum Testen von Review + Gate):
#                ./review_pr.ps1 -LocalRange "main..HEAD" -RepoPath C:\src\repo

[CmdletBinding()]
param(
    [string]$Org,
    [string]$Project,
    [string]$Repo,
    [int]$PrId,
    [string]$Pat = $env:ADO_PAT,
    [string]$RepoPath = ".",
    [string]$AgentkitPath = "agentkit",
    [string]$ProfilePath = "$PSScriptRoot\..\profile.json",
    [int]$MaxSteps = 60,
    # Policy-Gate: Freigabe nur unterhalb dieser Diff-Größe ...
    [int]$MaxDiffLines = 800,
    # ... und nur, wenn keine geschützten Pfade berührt sind (Wildcards).
    [string[]]$ProtectedPaths = @("*azure-pipelines*", "*.vsts-ci*", "infra/*", "*/secrets/*", "*.pem", "*.pfx"),
    # Nichts schreiben, nur zeigen, was passieren würde.
    [switch]$DryRun,
    # Auch abgeschlossene/abgebrochene PRs analysieren (nur Review + Gate,
    # niemals Kommentare/Vote — nachträgliche Analyse, z. B. zum Kalibrieren).
    [switch]$AllowCompleted,
    # Ohne ADO: nur Review + Policy-Gate für einen lokalen Range (Testmodus).
    [string]$LocalRange,
    # .env mit Provider-Variablen (AZURE_OPENAI_* / AGENTKIT_PROVIDER); wird in
    # die Prozess-Umgebung geladen, ohne bereits gesetzte Variablen zu
    # überschreiben. Default: die .env des agentkit-Crates.
    [string]$EnvFile = "$PSScriptRoot\..\..\..\.env"
)

$ErrorActionPreference = "Stop"
$ado = $null

# Provider-Env laden (echte Umgebung gewinnt gegen die .env-Datei).
if ($EnvFile -and (Test-Path $EnvFile)) {
    Get-Content $EnvFile | Where-Object { $_ -match "^[A-Za-z_]+=" } | ForEach-Object {
        $k, $v = $_ -split "=", 2
        if (-not (Test-Path "env:$k")) { Set-Item "env:$k" $v.Trim('"') }
    }
}

# agentkit auflösen: PATH, sonst Release-/Debug-Build relativ zu diesem Skript.
if (-not (Get-Command $AgentkitPath -ErrorAction SilentlyContinue)) {
    $candidates = @(
        "$PSScriptRoot\..\..\..\target\release\agentkit.exe",
        "$PSScriptRoot\..\..\..\target\debug\agentkit.exe"
    )
    $found = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $found) {
        throw "agentkit nicht gefunden: weder '$AgentkitPath' im PATH noch ein Build unter target\{release,debug}. Erst bauen: cargo build --release --bin agentkit (oder -AgentkitPath angeben)."
    }
    $AgentkitPath = (Resolve-Path $found).Path
    Write-Host "(agentkit: $AgentkitPath)"
}

function Invoke-Ado {
    param([string]$Method, [string]$Path, $Body = $null)
    $headers = @{ Authorization = "Basic " + [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes(":$Pat")) }
    $uri = "https://dev.azure.com/$Org/$Project/_apis/$Path"
    $args = @{ Method = $Method; Uri = $uri; Headers = $headers; ContentType = "application/json" }
    if ($null -ne $Body) { $args.Body = ($Body | ConvertTo-Json -Depth 10) }
    Invoke-RestMethod @args
}

function Get-GitOutput {
    param([string[]]$GitArgs)
    $out = & git -C $RepoPath @GitArgs 2>&1
    if ($LASTEXITCODE -ne 0) { throw "git $($GitArgs -join ' ') fehlgeschlagen: $out" }
    $out
}

# --------------------------------------------------------------- 1. FETCH
if ($LocalRange) {
    $range = $LocalRange
    $prTitle = "(lokaler Testlauf)"
    $prDescription = ""
} else {
    foreach ($p in @("Org", "Project", "Repo")) {
        if (-not (Get-Variable $p -ValueOnly)) { throw "Parameter -$p fehlt (oder -LocalRange für den Testmodus nutzen)." }
    }
    if (-not $Pat) {
        # Fallback ohne PAT: gespeicherte Git-Zugangsdaten (Git Credential
        # Manager) — funktioniert lokal, agiert aber als der angemeldete
        # Benutzer, nicht als Bot. Für Pipelines PAT bzw. System.AccessToken.
        foreach ($gitHost in @("dev.azure.com", "$Org.visualstudio.com")) {
            $lines = "protocol=https`nhost=$gitHost`n`n" | git credential fill 2>$null
            $cand = ($lines | Where-Object { $_ -like "password=*" }) -replace "^password=", ""
            if ($cand) { $Pat = $cand; Write-Host "(Auth: gespeicherte Git-Zugangsdaten für $gitHost)"; break }
        }
        if (-not $Pat) { throw "PAT fehlt: -Pat oder `$env:ADO_PAT setzen (Scope: Code Read & Write) — und keine Git-Zugangsdaten im Credential Manager gefunden." }
    }
    if (-not $PrId) { throw "Parameter -PrId fehlt." }

    $pr = Invoke-Ado GET "git/repositories/$Repo/pullRequests/$PrId`?api-version=7.1"
    $analyzeOnly = $false
    if ($pr.status -ne "active") {
        if (-not $AllowCompleted) { Write-Host "PR !$PrId ist '$($pr.status)' — nichts zu tun (-AllowCompleted für nachträgliche Analyse)."; exit 0 }
        $analyzeOnly = $true
        Write-Host "PR !$PrId ist '$($pr.status)' — Analyse-Modus (keine Kommentare, kein Vote)."
    }
    $prTitle = $pr.title
    $prDescription = $pr.description
    $sourceCommit = $pr.lastMergeSourceCommit.commitId
    $targetRef = $pr.targetRefName -replace "^refs/heads/", ""

    if (Get-GitOutput @("status", "--porcelain")) {
        throw "Working Tree in $RepoPath ist nicht sauber — bitte committen/stashen (das Skript checkt den PR-Commit aus)."
    }
    # Ausgangszustand merken, um nach dem Review zurückzuwechseln.
    $prevRef = (& git -C $RepoPath symbolic-ref --short -q HEAD)
    if (-not $prevRef) { $prevRef = (Get-GitOutput @("rev-parse", "HEAD")).Trim() }

    Get-GitOutput @("fetch", "origin", $targetRef, $sourceCommit) | Out-Null
    # Detached auschecken, damit der Agent die PR-Fassung der Dateien liest.
    Get-GitOutput @("checkout", "--quiet", $sourceCommit) | Out-Null
    # Merge-Base gegen den Target-Stand der PR-Merge-Berechnung — NICHT gegen
    # origin/<target>: Bei completed PRs ist der Source dort schon enthalten
    # (merge-base == source -> leerer Diff), und auch bei aktiven PRs kann der
    # Target-Branch inzwischen weitergewandert sein.
    $targetCommit = $pr.lastMergeTargetCommit.commitId
    if (-not $targetCommit) { $targetCommit = "origin/$targetRef" }
    $mergeBase = (Get-GitOutput @("merge-base", $targetCommit, $sourceCommit)).Trim()
    $range = "$mergeBase..$sourceCommit"
}

function Restore-Head {
    if ($prevRef) {
        try { Get-GitOutput @("checkout", "--quiet", $prevRef) | Out-Null
              Write-Host "(Repo zurück auf '$prevRef')" } catch {}
    }
}

# Diff-Kennzahlen deterministisch erheben (Policy-Gate urteilt darauf, nicht das LLM).
$changedFiles = @(Get-GitOutput @("diff", "--name-only", $range) | Where-Object { $_ })
$numstat = Get-GitOutput @("diff", "--numstat", $range)
$diffLines = ($numstat | ForEach-Object {
    $f = $_ -split "\t"
    if ($f.Length -ge 2) { [int]($f[0] -replace "-", "0") + [int]($f[1] -replace "-", "0") } else { 0 }
} | Measure-Object -Sum).Sum
$protectedHits = @($changedFiles | Where-Object { $f = $_; ($ProtectedPaths | Where-Object { $f -like $_ }).Count -gt 0 })

Write-Host "PR: $prTitle"
Write-Host "Range: $range  ($($changedFiles.Count) Dateien, $diffLines geänderte Zeilen)"

# Ab hier läuft alles im try/finally: Der Clone wird auch bei Fehlern
# (z. B. LLM nicht erreichbar) auf den Ausgangs-Branch zurückgesetzt.
try {

# --------------------------------------------------------------- 2. REVIEW
$task = "Reviewe die Änderungen $range in diesem Repository. " +
        "PR-Titel: $prTitle"
$context = if ($prDescription) { "PR-Beschreibung:`n$prDescription" } else { "" }

$stdout = $context | & $AgentkitPath -w $RepoPath --profile $ProfilePath `
    --format json --no-mcp -y --no-color --max-steps $MaxSteps -- $task
$code = $LASTEXITCODE
if ($code -ne 0) {
    throw "agentkit-Review fehlgeschlagen (Exit $code; 2=API/Netz, 3=Kontext, 4=Format)."
}
$review = $stdout | ConvertFrom-Json
$findings = @($review.findings)
$errors = @($findings | Where-Object { $_.severity -eq "error" })

Write-Host "`nReview: risk=$($review.risk) verdict=$($review.verdict) findings=$($findings.Count) (davon $($errors.Count) error)"
Write-Host $review.summary

# --------------------------------------------------------------- 3. POLICY-GATE
# Die Freigabe ist eine deterministische Entscheidung des Skripts, nicht des
# Agenten: Der Agent EMPFIEHLT (verdict), das Gate ENTSCHEIDET.
$gateReasons = @()
if ($review.verdict -ne "approve") { $gateReasons += "Agent-Verdict ist '$($review.verdict)'" }
if ($review.risk -ne "low") { $gateReasons += "Risiko '$($review.risk)'" }
if ($errors.Count -gt 0) { $gateReasons += "$($errors.Count) error-Finding(s)" }
if ($diffLines -gt $MaxDiffLines) { $gateReasons += "Diff zu groß ($diffLines > $MaxDiffLines Zeilen)" }
if ($protectedHits.Count -gt 0) { $gateReasons += "geschützte Pfade berührt: $($protectedHits -join ', ')" }

if ($gateReasons.Count -eq 0) {
    $vote = 10; $voteLabel = "Approve"
} elseif ($review.verdict -eq "request_changes" -or $errors.Count -gt 0) {
    $vote = -5; $voteLabel = "Waiting for author"
} else {
    $vote = 0; $voteLabel = "No vote (nur Kommentare)"
}
Write-Host "`nPolicy-Gate: $voteLabel" -NoNewline
if ($gateReasons.Count) { Write-Host "  [$($gateReasons -join '; ')]" } else { Write-Host "" }

if ($LocalRange) {
    Write-Host "`n(Testmodus: kein ADO-Zugriff. Review-JSON folgt.)"
    $review | ConvertTo-Json -Depth 6
    exit 0
}
if ($analyzeOnly) {
    Write-Host "`n(Analyse-Modus: PR ist '$($pr.status)' — nichts wird gepostet. Review-JSON folgt.)"
    $review | ConvertTo-Json -Depth 6
    exit 0
}

# --------------------------------------------------------------- 4. ACT
$marker = "[agentkit-review $sourceCommit]"

# Idempotenz: bereits gepostete Bot-Threads dieses Commits nicht duplizieren.
$existing = Invoke-Ado GET "git/repositories/$Repo/pullRequests/$PrId/threads?api-version=7.1"
$posted = @{}
foreach ($t in $existing.value) {
    foreach ($c in $t.comments) {
        if ($c.content -like "*$marker*") {
            $key = "$($t.threadContext.filePath):$($t.threadContext.rightFileStart.line)"
            $posted[$key] = $true
        }
    }
}

function Post-Thread {
    param($Content, $FilePath = $null, $Line = 0)
    $body = @{ comments = @(@{ parentCommentId = 0; content = $Content; commentType = 1 }); status = "active" }
    if ($FilePath) {
        $body.threadContext = @{ filePath = "/$($FilePath -replace '^/','')" }
        if ($Line -gt 0) {
            $body.threadContext.rightFileStart = @{ line = $Line; offset = 1 }
            $body.threadContext.rightFileEnd = @{ line = $Line; offset = 1 }
        }
    }
    if ($DryRun) { Write-Host "[DryRun] Thread ${FilePath}:${Line}: $($Content -split "`n" | Select-Object -First 1)"; return }
    Invoke-Ado POST "git/repositories/$Repo/pullRequests/$PrId/threads?api-version=7.1" $body | Out-Null
}

# Zusammenfassungs-Thread (einmal je Quell-Commit).
if (-not $posted.Contains(":")) {
    $missing = if ($review.missing_tests) { "`n`nFehlende Tests:`n- " + ($review.missing_tests -join "`n- ") } else { "" }
    Post-Thread "$marker **agentkit-Review** — Risiko: $($review.risk), Empfehlung: $($review.verdict)`n`n$($review.summary)$missing`n`nPolicy-Gate: $voteLabel"
}

foreach ($f in $findings) {
    $line = [int]$f.line
    $key = "/$($f.file -replace '^/',''):$line"
    if ($posted.Contains($key)) { continue }
    Post-Thread "$marker **$($f.severity)**: $($f.comment)" $f.file $line
}

# Vote setzen (Bot-Identität aus dem PAT ableiten).
$me = (Invoke-RestMethod -Uri "https://dev.azure.com/$Org/_apis/connectionData" -Headers @{
    Authorization = "Basic " + [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes(":$Pat")) }).authenticatedUser.id
if ($DryRun) {
    Write-Host "[DryRun] Vote: $vote ($voteLabel) als Reviewer $me"
} else {
    Invoke-Ado PUT "git/repositories/$Repo/pullRequests/$PrId/reviewers/$me`?api-version=7.1" @{ vote = $vote } | Out-Null
    Write-Host "Vote gesetzt: $vote ($voteLabel)"
}

} finally { Restore-Head }
