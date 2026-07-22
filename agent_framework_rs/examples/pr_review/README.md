# PR-Review — GitHub **und** Azure DevOps

> **Automatischer Reviewer mit Freigabe:** `scripts/review_pr.ps1` macht aus dem
> Review-Profil einen vollständigen ADO-PR-Reviewer — Findings als
> Kommentar-Threads, Vote (Approve/Waiting for author) über ein
> **deterministisches Policy-Gate** statt LLM-Entscheidung. Details unten
> unter „6) Automatischer ADO-Reviewer".

Dieses Beispiel zeigt, wie `agentkit` Pull Requests begutachtet — lokal über die
eingebauten read-only git-Tools, und für PR-Metadaten/Kommentare über MCP-Server:
den offiziellen **GitHub MCP Server** oder den offiziellen **Azure DevOps MCP
Server** von Microsoft (`@azure-devops/mcp`).

Bausteine:

| Datei | Zweck |
|---|---|
| `mcp.json` | MCP-Server-Deklaration für GitHub und Azure DevOps (Claude-Code-Format) |
| `profile.json` | Pipe-Profil: Review-System-Prompt + Plan-Strategie |
| `roles/pr-reviewer.md` | Custom-Sub-Agent-Rolle für `task(subagent_type="pr-reviewer")` |

## 1) Lokales Review — ganz ohne MCP

Die git-Tools (`git_status`, `git_diff`, `git_log`, `git_show`) sind read-only,
workspace-gebunden und brauchen **keine** Shell-Freigabe. Ein Review des lokal
ausgecheckten PR-Branches:

```bash
git fetch origin main pr-branch && git checkout pr-branch

echo "" | agentkit -w . --profile examples/pr_review/profile.json \
  --format json "Reviewe die Änderungen main..HEAD" | jq .
```

Der Agent zieht sich den Diff über `git_diff(range="main..HEAD")`, liest die
betroffenen Dateien für Kontext und liefert das strukturierte Review auf stdout
(Exit-Codes wie überall: `0` ok, `2` API/Netz, `4` Format).

## 2) GitHub-PRs über den GitHub MCP Server

Der offizielle Server läuft als Docker-Container über stdio; er braucht ein
Personal Access Token (`GITHUB_PERSONAL_ACCESS_TOKEN`):

```bash
export GITHUB_PERSONAL_ACCESS_TOKEN=ghp_…
agentkit -w . --mcp-config examples/pr_review/mcp.json --mcp github \
  "Hole PR #42 aus owner/repo, fasse die Änderungen zusammen und reviewe den Diff."
```

Die Tools erscheinen namespaced als `mcp__github__…` (z. B. `get_pull_request`,
`get_pull_request_diff`, `create_pull_request_review`).

## 3) Azure-DevOps-PRs über `@azure-devops/mcp`

Der offizielle Microsoft-Server läuft per `npx` über stdio. Voraussetzungen:
Node.js ≥ 20 und eine Azure-CLI-Anmeldung (`az login`) — alternativ ein PAT
gemäß Server-Doku. In `mcp.json` die Organisation eintragen (Platzhalter
`<org>`), dann:

```bash
az login   # einmalig; Authentifizierung des MCP-Servers
agentkit -w . --mcp-config examples/pr_review/mcp.json --mcp azure-devops \
  "Liste die offenen Pull Requests im Repo <repo> des Projekts <projekt> \
   und reviewe PR !123: Zusammenfassung, Risiken, konkrete Anmerkungen."
```

Die Tools erscheinen als `mcp__azure-devops__…` (Domain `repositories`:
`repo_list_pull_requests_by_repo`, `repo_get_pull_request_by_id`,
Kommentar-Threads usw.). Über `--domains` in den `args` lässt sich die
Tool-Menge klein halten — das Beispiel lädt nur `core` und `repositories`.

**Empfohlener Zuschnitt:** PR-Metadaten, Beschreibung und Kommentare über MCP;
den eigentlichen Diff lokal reviewen (`git fetch` des PR-Branches + `git_diff`)
— das ist deterministischer und spart Tokens.

## 4) Als Sub-Agent im großen Lauf

`roles/pr-reviewer.md` definiert die Rolle für das `task`-Tool. Ein Orchestrator
kann damit ein Review als isolierten, read-only Sub-Agenten abspalten:

```bash
agentkit -w . --agents examples/pr_review/roles \
  "Delegiere ein Review der Änderungen main..HEAD an den pr-reviewer und \
   fasse dessen Ergebnis zusammen."
```

## 5) Lange Reviews: Kontext-Management + Resume

Große PRs sprengen naive Kontexte. Mit dem Feature `ctxman` übernimmt das
Context-Management große Diffs (Externalisierung + `expand_context_ref`) und der
Lauf überlebt Neustarts:

```bash
agentkit -w . --ctx .agentkit-ctx --session review-session.json \
  "Reviewe die Änderungen main..HEAD, Datei für Datei."
```

## 6) Automatischer ADO-Reviewer: `scripts/review_pr.ps1`

Der komplette Zyklus **Review → Kommentare → Vote** als Pipeline-Skript.
Design-Prinzip: Der Agent läuft read-only (`--no-mcp`, nur git-/Lese-Tools)
und **empfiehlt** (`verdict`); die Freigabe entscheidet ein deterministisches
**Policy-Gate** im Skript. Nur das Skript hält den PAT.

```powershell
# Voraussetzungen: agentkit im PATH (oder -AgentkitPath), Provider-Env gesetzt
# (AZURE_OPENAI_* / .env / ~/.agentkit), PAT mit Scope "Code (Read & Write)".
$env:ADO_PAT = "<pat>"
./scripts/review_pr.ps1 -Org myorg -Project MyProject -Repo my-repo -PrId 123 `
    -RepoPath C:\src\my-repo -DryRun     # erst ohne Schreibzugriff testen
```

- **Policy-Gate** (Approve nur wenn alles zutrifft): Agent-Verdict `approve`,
  Risiko `low`, keine `error`-Findings, Diff ≤ `-MaxDiffLines` (Default 800),
  keine geschützten Pfade (`-ProtectedPaths`, Default: Pipelines/Infra/Secrets).
  Sonst: `request_changes`/error-Findings → Vote −5, alles andere → nur
  Kommentare (Vote 0).
- **Idempotent:** Threads tragen einen `[agentkit-review <commit>]`-Marker;
  bei erneutem Lauf auf demselben Quell-Commit werden keine Duplikate gepostet.
- **Testmodus ohne ADO:** `-LocalRange "main..HEAD"` führt nur Review +
  Policy-Gate auf einem lokalen Range aus (kein PAT nötig) — ideal zum
  Kalibrieren von Profil und Gate.
- **Als Branch Policy:** In ADO eine Build-Validation-Pipeline anlegen, die
  das Skript pro PR ausführt (`System.PullRequest.PullRequestId` liefert die
  PR-Nummer); den Bot-Benutzer (PAT-Inhaber) als optionalen Reviewer führen,
  solange Menschen die letzte Instanz bleiben.
