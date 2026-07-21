# PR-Review — GitHub **und** Azure DevOps

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
