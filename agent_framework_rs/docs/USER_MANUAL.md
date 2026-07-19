# agentkit — Benutzerhandbuch

Ein praktisches Handbuch für **Endanwender**, die das `agentkit`-Kommando im Terminal nutzen —
für einzelne Aufgaben und für **komplette Workflows** in PowerShell oder Bash (wie den
Accounts-Payable-Prozess). Du brauchst dafür **keine Programmierkenntnisse in Rust** und musst
nichts über die interne Bibliothek wissen. Alles hier bezieht sich auf das Kommando.

> Kurzformel: **Ein Agent ist ein Sprachmodell (LLM) in einer Schleife mit Werkzeugen.**
> agentkit ist zugleich ein **Coding-/Automations-Agent** und ein **Unix-Filter**, den du mit
> Pipes zu beliebigen Abläufen zusammensteckst.

---

## Inhalt

1. [Was agentkit ist und kann](#1-was-agentkit-ist-und-kann)
2. [Installation](#2-installation)
3. [LLM-Zugang einrichten (.env, Provider, Demo)](#3-llm-zugang-einrichten)
4. [Die Betriebsarten](#4-die-betriebsarten)
5. [Die Werkzeuge des Agenten](#5-die-werkzeuge-des-agenten)
6. [Der Unix-Filter-Vertrag: stdin, stdout, stderr, Exit-Codes](#6-der-unix-filter-vertrag)
7. [Optionen-Referenz](#7-optionen-referenz)
8. [Strategien: react, plan, plain](#8-strategien)
9. [Sub-Agenten und Rollen (`task`, `--agents`)](#9-sub-agenten-und-rollen)
10. [Skills (`--skills`)](#10-skills)
11. [Gedächtnis (`--memory`)](#11-gedächtnis)
12. [MCP: externe Werkzeug-Server](#12-mcp-externe-werkzeug-server)
13. [Sicherheit](#13-sicherheit)
14. [Workflows bauen — das Kochbuch](#14-workflows-bauen--das-kochbuch)
15. [Vollständiges Mini-Beispiel](#15-vollständiges-mini-beispiel)
16. [Profile (`--profile`)](#16-profile)
17. [REPL-Befehle und TUI-Tasten](#17-repl-befehle-und-tui-tasten)
18. [Shell-Completions](#18-shell-completions)
19. [Fehlerbehebung (FAQ)](#19-fehlerbehebung-faq)
20. [Anhang: Referenztabellen](#20-anhang-referenztabellen)

---

## 1. Was agentkit ist und kann

agentkit bekommt eine Aufgabe in natürlicher Sprache und arbeitet sie selbstständig ab. Dabei
darf es **Werkzeuge** benutzen: Dateien lesen/schreiben, Verzeichnisse durchsuchen,
Shell-Befehle ausführen, PDFs auslesen, Teilaufgaben an Sub-Agenten delegieren u. v. m. Der
Ablauf ist immer derselbe:

```text
solange das Modell ein Werkzeug aufrufen will:
    Werkzeug ausführen  ->  Ergebnis anhängen  ->  Modell erneut fragen
sonst:
    finale Antwort ausgeben
```

Typische Einsätze:

- **Einmal-Aufgaben:** „Fasse diese Log-Datei zusammen“, „Schreibe ein Python-Skript, das …“.
- **Interaktives Arbeiten** an einem Projekt (REPL oder Terminal-UI).
- **Automations-Bausteine** in Skripten: eine Rechnung extrahieren, Text klassifizieren,
  Daten nach JSON umwandeln, Reports erzeugen — als Teil einer Pipe.

Weil sich agentkit an die klassischen Terminal-Konventionen hält (stdin/stdout/stderr,
Exit-Codes), kannst du es mit `|`, `>`, `&&`, Schleifen usw. zu **beliebig großen Workflows**
kombinieren — genau wie `grep`, `jq` oder `curl`.

---

## 2. Installation

Voraussetzung: [Rust/Cargo](https://rustup.rs). Danach:

```bash
# Mit PDF-Support und Terminal-UI (empfohlen):
cargo install --path agent_framework_rs --bin agentkit --features "pdf tui"

# Minimal (ohne TUI), PDF behalten:
cargo install --path agent_framework_rs --bin agentkit --features pdf
```

Die Executable landet in `~/.cargo/bin` (Windows: `%USERPROFILE%\.cargo\bin`) — dieses
Verzeichnis muss im `PATH` liegen (rustup richtet das normalerweise ein). Prüfen:

```bash
agentkit --version
agentkit --demo "Was ist 17 + 25?"     # netzfreier Demo-Modus, kein API-Key nötig
```

Fertige Release-Binaries (Windows & Linux) hängen an den GitHub-Releases; siehe `INSTALL.md`.

> **Features:** `pdf` schaltet das `read-pdf`-Kommando und das `read_pdf`-Werkzeug frei; `tui`
> das Terminal-UI. Ohne diese Features fehlen die jeweiligen Funktionen.

---

## 3. LLM-Zugang einrichten

Ohne API-Key läuft ein eingebauter **Demo-Modus** (netzfrei, kleiner Werkzeugkasten) — gut zum
Ausprobieren, aber nicht „intelligent“. Für echte Arbeit brauchst du **Azure OpenAI**,
**OpenAI** oder einen **lokalen OpenAI-kompatiblen Server** (Ollama, LM Studio, vLLM,
llama.cpp, …). agentkit liest die Zugangsdaten aus **Umgebungsvariablen**:

| Variable | Zweck |
|---|---|
| `OPENAI_API_KEY` | aktiviert den OpenAI-Pfad |
| `OPENAI_MODEL` | Modellname (Default `gpt-4o-mini`) |
| `OPENAI_BASE_URL` | aktiviert einen lokalen/kompatiblen Server (z. B. `http://localhost:11434/v1`); API-Key dann optional |
| `AZURE_OPENAI_API_KEY` | aktiviert den Azure-Pfad |
| `AZURE_OPENAI_ENDPOINT` | Azure-Endpoint-URL |
| `AZURE_OPENAI_DEPLOYMENT` | Name des Azure-Deployments |
| `AZURE_OPENAI_API_VERSION` | optional (Default `2024-10-21`) |

**Bequemer:** Lege eine Datei `.env` in dein Arbeitsverzeichnis. agentkit lädt sie beim Start
automatisch (nur Variablen, die noch nicht gesetzt sind):

```dotenv
# .env
AZURE_OPENAI_ENDPOINT=https://dein-endpoint.openai.azure.com
AZURE_OPENAI_API_KEY=dein-key
AZURE_OPENAI_DEPLOYMENT=dein-deployment
# oder für OpenAI:
# OPENAI_API_KEY=sk-...
# OPENAI_MODEL=gpt-4o-mini

# oder für ein lokales Modell (Ollama, LM Studio, vLLM, llama.cpp, …):
# OPENAI_BASE_URL=http://localhost:11434/v1
# OPENAI_MODEL=qwen2.5-coder
# OPENAI_API_KEY kann entfallen — lokale Server verlangen meist keinen.
```

**Lokale Modelle** laufen über denselben OpenAI-Pfad: jeder Server, der die
Chat-Completions-API spricht, funktioniert — nur die Base-URL zeigt auf `localhost`.
Beispiele:

| Server | typische `OPENAI_BASE_URL` |
|---|---|
| Ollama | `http://localhost:11434/v1` |
| LM Studio | `http://localhost:1234/v1` |
| vLLM | `http://localhost:8000/v1` |
| llama.cpp (`llama-server`) | `http://localhost:8080/v1` |

`OPENAI_MODEL` muss zum geladenen Modell passen (bei Ollama z. B. `llama3.1` oder
`qwen2.5-coder` — was `ollama list` anzeigt). Tool-Calling braucht ein Modell, das
Function-Calling beherrscht; kleine Modelle rufen Werkzeuge oft unzuverlässig auf.

**Provider-Wahl** über `--provider`:

- `auto` (Default): Azure, wenn `AZURE_OPENAI_*` gesetzt ist, sonst OpenAI bzw. lokaler
  Server (`OPENAI_API_KEY` **oder** `OPENAI_BASE_URL` gesetzt), sonst Demo.
- `azure` / `openai`: erzwingt den jeweiligen Pfad (`openai` deckt auch lokale Server ab).
- `demo`: erzwingt den netzfreien Demo-Modus (auch via `--demo`).

> Wichtig: `.env` wird aus dem **aktuellen Verzeichnis** geladen. Rufst du agentkit aus einem
> Skript in einem anderen Ordner auf, setze die Variablen vorher in der Umgebung oder lies die
> `.env` selbst ein (siehe [Kochbuch](#14-workflows-bauen--das-kochbuch)). Halte Keys aus
> Skripten und aus der Versionsverwaltung heraus (`.gitignore`).

---

## 4. Die Betriebsarten

agentkit hat **drei interaktive Betriebsarten** und **zwei Utility-Unterbefehle**.

### One-shot (eine Aufgabe, dann Ende)

```bash
agentkit "Erkläre den Unterschied zwischen TCP und UDP in 3 Sätzen."
agentkit -w ./projekt "Finde alle TODOs und liste sie mit Datei:Zeile."
```

Läuft die Aufgabe, streamt die Antwort ins Terminal und beendet sich. Das ist die Betriebsart,
die du in **Skripten** verwendest.

### Interaktive Session (REPL)

```bash
agentkit                 # ohne Aufgabe -> Zeilen-REPL, Gedächtnis bleibt erhalten
```

Du tippst Aufgaben zeilenweise ein; der Kontext der Unterhaltung bleibt bestehen.
Slash-Befehle wie `/tools`, `/plan`, `/mcp` steuern die Session (siehe
[Abschnitt 17](#17-repl-befehle-und-tui-tasten)). `Ctrl-C` bricht die laufende Aufgabe ab,
`Ctrl-D` bzw. `/exit` beendet.

### Terminal-UI (TUI)

```bash
agentkit --tui -w .      # nur mit Feature `tui`
```

Ein vollwertiges Terminal-UI: Schritte, Werkzeugaufrufe und gestreamte Token live, mit einem
Freigabedialog für Shell-Befehle. Tasten siehe [Abschnitt 17](#17-repl-befehle-und-tui-tasten).

### Utility-Unterbefehle (deterministisch, ohne LLM)

```bash
agentkit read-pdf rechnung.pdf            # extrahiert den PDF-Text auf stdout (Feature pdf)
agentkit completions bash                 # gibt ein Shell-Completion-Skript aus
```

Diese Unterbefehle rufen **kein** Modell auf — sie sind schnelle, kostenlose Werkzeuge, die du
in Pipelines einsetzt (z. B. `agentkit read-pdf x.pdf | agentkit -p "Extrahiere die Summe"`).

---

## 5. Die Werkzeuge des Agenten

Mit einem echten Modell (nicht Demo) ist agentkit der **volle Coding-Agent**. Der Agent
entscheidet selbst, welche Werkzeuge er einsetzt; du steuerst über den Auftrag und die
Optionen. Die eingebauten Werkzeuge:

| Werkzeug | Was es tut | Rückfrage? |
|---|---|---|
| `list_files` | Verzeichnisinhalt auflisten | nein |
| `glob_files` | Dateien per Muster finden (`**/*.py`) | nein |
| `grep` | Dateiinhalte per Regex durchsuchen (`pfad:zeile: text`) | nein |
| `read_file` | Textdatei lesen | nein |
| `read_pdf` | Text aus einer PDF extrahieren (Feature `pdf`) | nein |
| `write_file` | Datei schreiben/überschreiben | ja* |
| `edit_file` | eindeutigen Textabschnitt ersetzen | ja* |
| `run_shell` | Shell-Befehl ausführen (PowerShell auf Windows, sonst bash) | **ja** |
| `update_plan` | einen Arbeitsplan mit Schritten führen/aktualisieren | nein |
| `list_skills` / `read_skill` | verfügbare Skills auflisten / laden (nur mit `--skills`) | nein |
| `remember` / `recall` | Fakten ins Langzeitgedächtnis schreiben/abrufen (nur mit `--memory`) | nein |
| `task` | eine Teilaufgabe an einen Sub-Agenten delegieren (nur mit Sub-Agenten) | – |

\* Alle Schreib-/Ausführ-Werkzeuge laufen **in einer Sandbox** (siehe
[Sicherheit](#13-sicherheit)); `run_shell` fragt vor der Ausführung nach (außer mit `--yes`).

Zusätzlich kann der Agent **MCP-Werkzeuge** externer Server nutzen (siehe
[Abschnitt 12](#12-mcp-externe-werkzeug-server)). Welche Werkzeuge in einer Session aktiv sind,
zeigt im REPL `/tools`.

---

## 6. Der Unix-Filter-Vertrag

Das ist das Herzstück fürs Workflow-Bauen. agentkit trennt die drei Standard-Streams sauber:

| Stream | Inhalt |
|---|---|
| **stdin** | *Kontext/Daten.* Ist stdin kein Terminal (also per Pipe/Umleitung), wird der **gesamte Inhalt gelesen und an die Aufgabe angehängt**. |
| **stdout** | Sobald die Ausgabe gepipt wird, im `--format json`- oder `-p`-Modus läuft: **nur das finale, bereinigte Resultat** — nichts sonst. Darauf kann sich das nächste Tool (`jq`, `ConvertFrom-Json`, ein zweiter Agent) verlassen. |
| **stderr** | Alles andere: Status, Werkzeug-Spur, Gedanken, Fehler. |

Praktisch heißt das:

```bash
# stdin = Kontext, stdout = reines Resultat, Spur sichtbar auf stderr:
cat daten.json | agentkit --format json "Extrahiere die Summe" | jq .summe

# Antwort in Datei, Spur weiter im Terminal:
agentkit "Fasse zusammen" < bericht.txt > ergebnis.txt
```

- `-p`/`--print` unterdrückt zusätzlich die Fortschritts-Spur — Ausgabe = **nur** die finale
  Antwort. Ideal für Skripte. (Zum Debuggen eines Fehlers `-p` weglassen, dann siehst du die
  Spur auf stderr.)
- Beim Schreiben ins Terminal (kein Pipe) wird die Antwort live gestreamt und die Spur farbig
  angezeigt.

### Exit-Codes

Für zuverlässiges Verketten (`set -e` in Bash, `$LASTEXITCODE` in PowerShell):

| Code | Bedeutung |
|---|---|
| `0` | Erfolg — Resultat auf stdout |
| `1` | unerwarteter Laufzeitfehler |
| `2` | Modell nicht erreichbar / Netz / Rate-Limit |
| `3` | Kontext zu groß oder Prompt leer/ungültig |
| `4` | erzwungenes `--format json` trotz Retries nicht erzeugbar |
| `130` | mit `Ctrl-C` abgebrochen |

Auf Unix beendet ein geschlossenes Pipe-Ende (`… | head`) den Prozess sauber (kein Absturz).

---

## 7. Optionen-Referenz

Optionen stehen **vor** dem Auftrag. `--flag value` und `--flag=value` sind gleichwertig; `--`
beendet die Optionen (danach ist alles wörtlicher Auftrag, auch wenn es mit `-` beginnt).

| Option | Bedeutung |
|---|---|
| `[AUFTRAG …]` | die Aufgabe (mehrere Wörter erlaubt) |
| `-w, --workspace DIR` | Sandbox-/Arbeitsverzeichnis (Default `.`) |
| `-s, --strategy S` | `react` \| `plan` \| `plain` (Default `react`) |
| `--plan` / `--plain` / `--react` | Kurzform für die Strategie |
| `--skills DIR` | Skills-Verzeichnis aktivieren (Ordner mit `SKILL.md`) |
| `--agents DIR` | eigene Sub-Agenten-Rollen aus `*.md` laden |
| `--memory FILE` | Langzeitgedächtnis (JSONL) für `remember`/`recall` |
| `--provider P` | `auto` \| `azure` \| `openai` \| `demo` (Default `auto`) |
| `--demo` | Demo-Modus erzwingen (netzfrei) |
| `--max-steps N` | max. Schleifen-Schritte (Default 160) |
| `--no-subagents` | das `task`-Werkzeug deaktivieren |
| `-y, --yes` | Shell-Befehle ohne Rückfrage ausführen |
| `--steps` | Schritt-Grenzen anzeigen |
| `--no-color` | Farbausgabe aus (auch via `NO_COLOR`-Umgebungsvariable) |
| `-p, --print` | One-shot: nur die finale Antwort ausgeben (Spur unterdrücken) |
| `--format T` | `text` \| `json` — `json` erzwingt + validiert strukturierten Output |
| `--dry-run` | zerstörerische Schreib-/MCP-Aktionen blockieren (nur auf stderr protokollieren) |
| `--max-context N` | Kontext-Limit in Tokens (Default 128000) → sonst Exit 3 |
| `--json-retries N` | Versuche für gültiges JSON (Default 3) → sonst Exit 4 |
| `--mcp-config FILE` | MCP-Server aus `.mcp.json` laden (sonst Auto-Discovery) |
| `--mcp NAME` | nur diesen MCP-Server aktiv (mehrfach möglich = Allowlist) |
| `--no-mcp` | MCP komplett deaktivieren |
| `--system TEXT` | agenten-spezifischer Zusatz-System-Prompt (Persona/Format je Stufe) |
| `--system-file FILE` | System-Prompt aus Datei (überschreibt `--system`) |
| `--profile FILE` | Config-Bündel (JSON) je Agent; explizite Flags gewinnen |
| `--tui` | Terminal-UI starten (Feature `tui`) |
| `--repl` | interaktive Session erzwingen (auch bei gepiptem stdin) — scriptbare Sitzung inkl. Folge-Antworten auf Rückfragen via stdin |
| `-h, --help` / `-V, --version` | Hilfe / Version |

Vollständige, immer aktuelle Liste: `agentkit --help`.

---

## 8. Strategien

Die Strategie steuert, **wie** der Agent denkt:

- **`react`** (Default) — „Reasoning + Acting“: das Modell denkt sichtbar, ruft Werkzeuge auf,
  wertet Ergebnisse aus, wiederholt. Beste Wahl für **offene Aufgaben** und Coding.
- **`plan`** — erstellt zuerst einen expliziten Plan (Schrittliste) und arbeitet ihn ab. Gut
  für **mehrstufige, klar strukturierte** Aufgaben; der Plan ist im REPL via `/plan` sichtbar.
- **`plain`** — kein spezielles Denk-Gerüst: das Modell antwortet möglichst direkt. Beste Wahl
  für **reine Transformationen** (Text → JSON, Zusammenfassen, Klassifizieren) in Pipelines,
  wo du kein Werkzeug-Herumprobieren willst.

Faustregel für Workflows: **Transformationsstufen → `plain`**, Coding/Recherche → `react`.

---

## 9. Sub-Agenten und Rollen

Mit einem echten Modell hat der Haupt-Agent das Werkzeug **`task`**: Er kann eine abgegrenzte
Teilaufgabe an einen **Sub-Agenten** mit eigener Rolle delegieren (eigener Kontext, ggf.
eingeschränkte Werkzeuge). Das hält den Hauptkontext schlank und erlaubt parallele Teilarbeit.

Eingebaute Rollen (Parameter `subagent_type` des `task`-Werkzeugs):

| Rolle | Zweck | Werkzeuge |
|---|---|---|
| `general` | beliebige Teilaufgabe, voller Coding-Zugriff | alle |
| `explorer` | read-only Repo-Erkundung: relevante Stellen finden/zusammenfassen | nur lesend |
| `reviewer` | read-only Begutachtung: Bugs/Risiken/Qualität mit Findings | nur lesend |
| `tester` | Tests/Befehle ausführen und Pass/Fail berichten (kein Code-Edit) | lesend + `run_shell` |

Im REPL zeigt `/agents` die verfügbaren Rollen. `--no-subagents` schaltet das `task`-Werkzeug
ab (schlanker, wenn du keine Delegation willst).

**Eigene Rollen** legst du als Markdown-Dateien an und lädst sie mit `--agents DIR`:

```markdown
---
name: security
description: Sicht auf Sicherheitslücken; nur lesend.
tools: read_only
strategy: plain
---
Du bist ein Security-Reviewer. Prüfe den Code auf Injection, unsichere Deserialisierung,
Secrets im Klartext … und liefere konkrete Findings mit Datei:Zeile.
```

`tools: read_only` beschränkt auf die lesenden Werkzeuge; ohne Angabe bekommt die Rolle alle.

### Human-in-the-Loop (ohne Sonderwerkzeug)

Im **REPL** und **TUI** braucht der Agent **kein Spezialwerkzeug**, um dich einzubeziehen: Hat er
eine Rückfrage (Freigabe, fehlendes Firmenwissen, Grenzfall), **stellt er sie als Antwort und
beendet seinen Zug**. Deine nächste Eingabe beantwortet sie, und er macht mit **vollem
Gesprächsverlauf** weiter — die Kurzzeit-Memory bleibt über die Züge erhalten. So bleibt die
agentische Schleife die *eine* Schleife, ohne blockierende Sonderpfade. Das TUI-Eingabefeld ist
**mehrzeilig** (Alt-Enter fügt eine Zeile ein), sodass auch lange Antworten oder Korrekturen
bequem eingegeben werden können. **Sub-Agenten** sprechen nie direkt mit dir — sie melden
Unklarheiten an den Orchestrator zurück, der mit dir redet.

Mit **`--repl`** wird die interaktive Session **scriptbar**: agentkit liest Kommandos *und* die
Folge-Antworten auf Rückfragen von stdin, auch wenn dieser gepipt ist:

```powershell
# Eine Aufgabe, dann die Antwort auf die erwartete Rückfrage, dann beenden:
"Verarbeite rechnung.txt`nKostenstelle 4930, Freigabe Herr Klein`n/exit`n" |
  agentkit --repl -w . --agents .\roles --system-file .\orchestrator.md
```

Ein vollständiges Vorbild (Orchestrator, der Sub-Agenten managt, nachfragt und einen
Wissensgraph aufbaut) ist der **interaktive Modus** des Accounts-Payable-Beispiels:
`examples/accounts_payable/` → `.\Invoke-Ap.ps1 -Mode Interactive`.

---

## 10. Skills

Ein **Skill** ist eine wiederverwendbare Anleitung, die der Agent bei Bedarf lädt
(*progressive disclosure*: erst wird nur der Titel angezeigt, die volle Anleitung holt der
Agent per `read_skill`). So bleibt der Kontext schlank, und du gibst dem Agenten
Spezialwissen an die Hand.

Ein Skill ist ein **Ordner mit einer `SKILL.md`**:

```markdown
---
name: pdf-report
description: Erzeugt aus Rohdaten einen sauberen Markdown-Report nach Hausstil.
---
Wenn du einen Report erstellst:
1. Beginne mit einer Executive Summary (max. 3 Sätze).
2. Danach eine Tabelle der Kennzahlen …
```

Aktivieren mit `--skills DIR` (der Ordner enthält je Skill einen Unterordner mit `SKILL.md`).
Im REPL listet `/skills` sie auf.

```bash
agentkit --skills ./skills "Erstelle den Quartalsreport aus daten.csv"
```

---

## 11. Gedächtnis

- **Kurzzeitgedächtnis** ist die laufende Unterhaltung (im REPL/TUI erhalten; per `/reset`
  leeren). Bei sehr langen Sitzungen wird älterer Verlauf automatisch verdichtet.
- **Langzeitgedächtnis** aktivierst du mit `--memory FILE` (eine JSONL-Datei). Dann hat der
  Agent die Werkzeuge `remember` (Fakt speichern) und `recall` (Fakten abrufen). Die Datei
  überdauert Sessions — nützlich, um Präferenzen oder Projektfakten festzuhalten.

```bash
agentkit --memory ./mem.jsonl "Merke dir: unser Standard-Konto für Bürobedarf ist SKR03 4930."
agentkit --memory ./mem.jsonl "Auf welches Konto buche ich Bürobedarf?"
```

---

## 12. MCP: externe Werkzeug-Server

Über das **Model Context Protocol (MCP)** kann agentkit Werkzeuge von externen Servern nutzen
(z. B. Git, Dateisystem, Datenbanken). Die Server beschreibst du deklarativ in einer
`.mcp.json` (Claude-Code-Format), die im Workspace/CWD automatisch gefunden wird:

```jsonc
// .mcp.json
{
  "mcpServers": {
    "git": { "command": "uvx", "args": ["mcp-server-git", "--repo", "."] },
    "fs":  { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "extra": { "command": "node", "args": ["server.js"], "disabled": true }
  }
}
```

Die Server-Werkzeuge erscheinen **namespaced** als `mcp__<server>__<tool>` (keine Kollision mit
lokalen Werkzeugen).

```bash
agentkit --mcp-config .mcp.json "Nutze das git-Tool und fasse die letzten Commits zusammen"
agentkit --mcp git "…"     # nur den Server 'git' aktiv (Allowlist)
agentkit --no-mcp "…"      # MCP komplett aus
```

Im REPL: `/mcp` listet die Server, `/mcp on <name>` / `/mcp off <name>` schaltet live um. Im
TUI öffnet **F2** das MCP-Panel.

---

## 13. Sicherheit

- **Sandbox:** Alle Datei-/Ausführ-Werkzeuge sind auf das Arbeitsverzeichnis (`-w`) beschränkt.
  Pfade außerhalb werden abgelehnt.
- **Freigabe für Shell-Befehle:** `run_shell` fragt standardmäßig **vor jeder Ausführung** nach
  (im REPL über stdin, im TUI per Dialog). `--yes`/`-y` erlaubt automatisch — nur nutzen, wenn
  du dem Auftrag und der Umgebung vertraust (z. B. in einer isolierten CI).
- **`--dry-run`:** führt den Loop aus, **blockiert aber zerstörerische** Schreib-/MCP-Aktionen
  (Heuristik nach Werkzeugnamen) und protokolliert nur, was versucht wurde. Gut zum
  gefahrlosen Ausprobieren eines Auftrags.
- **Secrets:** Lege API-Keys in `.env` (nicht einchecken) oder in Umgebungsvariablen — nie in
  Skripte/Prompts. Bedenke: `run_shell` kann alles, was deine Shell kann.

---

## 14. Workflows bauen — das Kochbuch

Die eigentliche Stärke: Du baust **ganze Abläufe**, indem du agentkit-Aufrufe mit den
Terminal-Mitteln (Pipes, Schleifen, Dateien, Exit-Codes) verkettest. Leitidee:

> **Ein Werkzeug bzw. ein Agent pro Schritt.** Nutze für Fakten deterministische Werkzeuge
> (`read-pdf`, `curl`/`Invoke-RestMethod`, Hashing, CSV) und für *Urteile* LLM-Agenten. Verkette
> sie über stdin/stdout; speichere Zwischenergebnisse als Dateien.

### Baustein A — Einzeltransformation

```powershell
# PowerShell
Get-Content bericht.txt -Raw | agentkit -p "Fasse in 5 Stichpunkten zusammen." > punkte.txt
```
```bash
# Bash
agentkit -p "Fasse in 5 Stichpunkten zusammen." < bericht.txt > punkte.txt
```

### Baustein B — Strukturierter Output (JSON) und weiterverarbeiten

`--format json` erzwingt gültiges JSON auf stdout (mit Validierung + Wiederholungen). Danach
liest ein normales Tool weiter:

```powershell
$json = Get-Content quelle.txt -Raw | agentkit -p --format json `
  --system "Antworte NUR mit {\"summe\": number, \"posten\": string[]}." "Extrahiere Summe und Posten."
$obj = $json | ConvertFrom-Json
"Summe: $($obj.summe)"
```
```bash
cat quelle.txt | agentkit -p --format json \
  --system 'Antworte NUR mit {"summe": number, "posten": string[]}.' \
  "Extrahiere Summe und Posten." | jq .summe
```

### Baustein C — Mehrstufige Pipe (jede Stufe ein spezialisierter Agent)

Jede Stufe bekommt ihren eigenen System-Prompt und macht genau eine Sache. Die Ausgabe der
einen ist die Eingabe der nächsten:

```bash
cat src/lib.rs \
 | agentkit -p --system-file prompts/extract.md  "Extrahiere alle öffentlichen Funktionen" \
 | agentkit -p --system-file prompts/rate.md      "Bewerte jede nach Komplexität" \
 | agentkit -p --system-file prompts/report.md    "Schreibe einen Markdown-Report" > report.md
```

### Baustein D — Fan-out über viele Dateien, ein Ordner pro Element

Das Muster des AP-Workflows: über eine Menge iterieren, je Element einen Ergebnisordner mit
allen Zwischenständen anlegen.

```powershell
# PowerShell
$OutDir = ".\out"
foreach ($f in Get-ChildItem .\inbox -Filter *.txt) {
    $dir = Join-Path $OutDir $f.BaseName
    New-Item -ItemType Directory -Force $dir | Out-Null
    $text = Get-Content $f.FullName -Raw
    # Stufe 1: Zusammenfassung
    $text | agentkit -p --strategy plain "Fasse zusammen." | Set-Content (Join-Path $dir '1_summary.txt') -Encoding utf8
    # Stufe 2: Klassifizierung als JSON
    Get-Content (Join-Path $dir '1_summary.txt') -Raw |
        agentkit -p --format json --system 'Antworte NUR mit {"prioritaet":"hoch|mittel|niedrig"}.' "Priorisiere." |
        Set-Content (Join-Path $dir '2_priority.json') -Encoding utf8
}
```
```bash
# Bash
for f in inbox/*.txt; do
  name="$(basename "${f%.*}")"; dir="out/$name"; mkdir -p "$dir"
  agentkit -p --strategy plain "Fasse zusammen." < "$f" > "$dir/1_summary.txt"
  agentkit -p --format json --system 'Antworte NUR mit {"prioritaet":"hoch|mittel|niedrig"}.' \
    "Priorisiere." < "$dir/1_summary.txt" > "$dir/2_priority.json"
done
```

### Baustein E — Fehlerbehandlung über Exit-Codes

```powershell
# PowerShell: nach jedem Aufruf $LASTEXITCODE prüfen
$out = Get-Content daten.txt -Raw | agentkit -p --format json "…"
if ($LASTEXITCODE -ne 0) { Write-Warning "Stufe fehlgeschlagen (Exit $LASTEXITCODE)"; continue }
```
```bash
# Bash: set -e bricht bei Fehler ab; oder gezielt prüfen
if ! result=$(agentkit -p --format json "…" < daten.txt); then
  echo "Stufe fehlgeschlagen ($?)" >&2; exit 1
fi
```

### Baustein F — Externe HTTP-API als eigene Stufe

Nicht alles ist eine LLM-Aufgabe. Eine Stufe kann auch ein Dienst sein — z. B. eine
Validierungs-API:

```powershell
$resp = Invoke-RestMethod -Method Post -Uri 'http://localhost:5080/api/v1/validate' `
  -Headers @{ 'x-api-key' = $env:MY_KEY } -ContentType 'application/xml' -InFile rechnung.xml
$resp | ConvertTo-Json | Set-Content 'check.json' -Encoding utf8
```
```bash
curl -sS -X POST http://localhost:5080/api/v1/validate \
  -H "x-api-key: $MY_KEY" -H "Content-Type: application/xml" \
  --data-binary @rechnung.xml > check.json
```

### Baustein G — Deterministisch vor LLM (Kosten & Genauigkeit)

Wo eine feste Regel genügt, kein Modell fragen. Beispiel PDF-Text: `agentkit read-pdf` ist
kostenlos und exakt; erst das *Verstehen* übernimmt ein Agent:

```bash
agentkit read-pdf rechnung.pdf | agentkit -p --format json --system-file extract.md "Extrahiere Felder"
```

### Baustein H — Idempotenz / Dubletten über ein Register

Führe eine kleine JSON-/CSV-„Datenbank“ mit, um bereits Verarbeitetes zu erkennen (verhindert
z. B. Doppelbuchungen). Schlüssel bilden, im Register nachschlagen, nach Erfolg eintragen —
alles mit Bordmitteln (`ConvertFrom-Json`/`ConvertTo-Json` bzw. `jq`).

### Baustein I — Audit/Unveränderbarkeit über Hashes

Für nachvollziehbare Abläufe: Original schreibgeschützt ablegen und einen SHA-256 je Artefakt
in ein `manifest.json` schreiben.

```powershell
(Get-FileHash .\out\job\00_original.pdf -Algorithm SHA256).Hash | Out-File .\out\job\hash.txt
Set-ItemProperty .\out\job\00_original.pdf -Name IsReadOnly -Value $true
```

> Alle diese Bausteine zusammen ergeben genau den **Accounts-Payable-Workflow**
> (`examples/accounts_payable/`): PDF/E-Rechnung einlesen → EN-16931 per API prüfen → Felder
> extrahieren → validieren → buchen → DATEV exportieren → GoBD-Manifest → Report. Schau ihn dir
> als vollständiges, laufendes Vorbild an.

---

## 15. Vollständiges Mini-Beispiel

Ein kleiner, kompletter Workflow „**Support-Tickets triagieren**“ — von Rohtext zu einem
priorisierten Report — als eigenständiges PowerShell-Skript. Er zeigt alle Kernmuster:
Fan-out, `--format json`, `--system-file`, Exit-Code-Prüfung, Zwischendateien.

```powershell
# Triage-Tickets.ps1
[CmdletBinding()] param([string]$InboxDir = ".\tickets", [string]$OutDir = ".\out")
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8   # Umlaute korrekt durch die Pipe

$classify = @'
Du klassifizierst Support-Tickets. Antworte NUR mit gültigem JSON:
{ "kategorie": "bug|frage|feature|abrechnung",
  "prioritaet": "hoch|mittel|niedrig",
  "kurzfassung": string }
'@

New-Item -ItemType Directory -Force $OutDir | Out-Null
$rows = @()
foreach ($f in Get-ChildItem $InboxDir -Filter *.txt) {
    $dir = Join-Path $OutDir $f.BaseName
    New-Item -ItemType Directory -Force $dir | Out-Null
    $text = Get-Content $f.FullName -Raw

    # Stufe 1 (Agent, plain): JSON-Klassifikation
    $json = $text | agentkit -p --strategy plain --format json --system $classify "Klassifiziere dieses Ticket."
    if ($LASTEXITCODE -ne 0) { Write-Warning "$($f.Name): Klassifikation fehlgeschlagen"; continue }
    $json | Set-Content (Join-Path $dir 'klass.json') -Encoding utf8
    $obj = $json | ConvertFrom-Json

    # Stufe 2 (Agent, plain): Antwortentwurf
    $text | agentkit -p --strategy plain --system "Formuliere eine freundliche erste Antwort (max. 5 Sätze)." `
        "Entwirf eine Antwort." | Set-Content (Join-Path $dir 'antwort.txt') -Encoding utf8

    $rows += [pscustomobject]@{ Ticket = $f.BaseName; Kategorie = $obj.kategorie; Prio = $obj.prioritaet; Kurz = $obj.kurzfassung }
}

# Übersicht (deterministisch, kein LLM nötig)
$rows | Sort-Object Prio | Format-Table -AutoSize | Out-String | Set-Content (Join-Path $OutDir 'uebersicht.txt') -Encoding utf8
$rows | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $OutDir 'alle.json') -Encoding utf8
Write-Host "Fertig. Ergebnisse in $OutDir"
```

Bash-Variante desselben Kerns (eine Stufe):

```bash
#!/usr/bin/env bash
set -euo pipefail
CLASSIFY='Antworte NUR mit JSON {"kategorie":"bug|frage|feature|abrechnung","prioritaet":"hoch|mittel|niedrig","kurzfassung":string}.'
mkdir -p out
for f in tickets/*.txt; do
  name="$(basename "${f%.*}")"; mkdir -p "out/$name"
  agentkit -p --strategy plain --format json --system "$CLASSIFY" "Klassifiziere." < "$f" > "out/$name/klass.json"
  jq -r '"\(.prioritaet)\t\(.kategorie)\t\(.kurzfassung)"' "out/$name/klass.json"
done | sort > out/uebersicht.tsv
echo "Fertig -> out/"
```

Von hier aus baust du beliebig weiter: weitere Stufen anhängen, externe APIs einbinden
(Baustein F), Dubletten über ein Register vermeiden (Baustein H), Ergebnisse als CSV/DATEV
exportieren usw.

---

## 16. Profile

Statt vieler Einzel-Flags kannst du ein **Config-Bündel je Stufe** in einer JSON-Datei ablegen
und mit `--profile FILE` laden. **Explizite CLI-Flags überschreiben** die Profilwerte.

```jsonc
// extractor.json
{
  "system": "Du extrahierst Struktur. Antworte NUR mit gültigem JSON.",
  // "system_file": "prompts/extractor.md",   // Alternative
  "strategy": "plain",           // react | plan | plain
  "provider": "azure",           // auto | azure | openai | demo
  "format":   "json",            // text | json
  "workspace": ".",
  "skills":   "./skills/extract",
  "agents":   "./roles/extract",
  "memory":   "./mem/extractor.jsonl",
  "mcp_config": "./mcp/git.json",
  "mcp":      ["git"],
  "no_mcp":   false,
  "no_subagents": true,
  "max_steps": 80,
  "dry_run":  false,
  "demo":     false
}
```

```bash
cat src.rs \
 | agentkit -p --profile agents/extractor.json "Extrahiere alle öffentlichen Funktionen" \
 | agentkit -p --profile agents/rater.json     "Bewerte jede nach Komplexität" \
 | agentkit -p --profile agents/writer.json    "Schreibe einen Markdown-Report"
```

So wird jede Pipe-Stufe zu einem klar definierten, wiederverwendbaren Agenten.

---

## 17. REPL-Befehle und TUI-Tasten

**REPL-Slash-Befehle** (in der interaktiven Session):

| Befehl | Wirkung |
|---|---|
| `/help` | Hilfe anzeigen |
| `/clear` | Bildschirm leeren |
| `/reset` | Unterhaltung vergessen (neues Kurzzeitgedächtnis) |
| `/plan` | aktuellen Plan anzeigen |
| `/tools` | registrierte Werkzeuge auflisten |
| `/skills` | verfügbare Skills auflisten |
| `/agents` | verfügbare Sub-Agenten-Rollen auflisten |
| `/mcp` | MCP-Server auflisten; `/mcp on\|off <name>` schaltet um |
| `/exit` | beenden (auch `/quit`, `Ctrl-D`) |

`Ctrl-C` bricht die laufende Aufgabe ab (zweimal = Programm beenden).

**TUI-Tasten:** `Enter` senden · `Esc` abbrechen/beenden · `Ctrl-Tab` Freigabemodus umschalten
(Nachfragen ↔ Auto) · `F2` MCP-Panel · `↑↓/PgUp/PgDn/End` scrollen · `Ctrl-C` beenden.

---

## 18. Shell-Completions

Tab-Vervollständigung für Flags und Werte:

```bash
# bash — sofort:
source <(agentkit completions bash)
# bash — dauerhaft:
agentkit completions bash > ~/.local/share/bash-completion/completions/agentkit
# zsh:
agentkit completions zsh > "${fpath[1]}/_agentkit"
# fish:
agentkit completions fish > ~/.config/fish/completions/agentkit.fish
```
```powershell
# PowerShell — aktuelle Sitzung / dauerhaft:
agentkit completions powershell | Out-String | Invoke-Expression
agentkit completions powershell >> $PROFILE
```

---

## 19. Fehlerbehebung (FAQ)

**„HTTP 401“ / Exit 2.** Der API-Key ist falsch/abgelaufen oder der falsche Provider ist aktiv.
Prüfe `--provider` und die Umgebungsvariablen. Zum Sehen der genauen Fehlermeldung den Aufruf
**ohne `-p`** wiederholen (dann erscheint die Spur auf stderr).

**Keine Ausgabe bei `-p`.** `-p` unterdrückt die Spur; kommt zusätzlich Exit ≠ 0, ist etwas
schiefgelaufen — ohne `-p` erneut aufrufen, um zu sehen, was.

**Exit 4 (kein gültiges JSON).** Das Modell lieferte trotz `--json-retries` kein sauberes JSON.
Schärfe den System-Prompt („Antworte NUR mit gültigem JSON, keine Code-Fences“) oder erhöhe
`--json-retries`.

**Exit 3 (Kontext zu groß).** Die Eingabe überschreitet `--max-context`. Kürze die Eingabe,
teile sie auf, oder erhöhe `--max-context` (falls dein Modell mehr kann).

**Umlaute kaputt in der Pipe (PowerShell).** Setze am Skriptanfang
`[Console]::OutputEncoding = [Text.Encoding]::UTF8` und schreibe Dateien mit
`Set-Content -Encoding utf8`.

**„PDF nicht lesbar / kein Text“.** Reine Scan-Bilder ohne Textebene liefern keinen Text
(kein OCR). Nutze eine PDF mit Textebene. `read-pdf` verlangt das Feature `pdf`.

**`agentkit read-pdf`/`completions` sagt „kein PDF-Support“.** Mit `--features pdf` neu bauen.

**Der Agent will ein Werkzeug nutzen, das ich nicht will.** Für reine Transformationen
`--strategy plain` verwenden und im System-Prompt „nutze keine Werkzeuge“ vorgeben; oder mit
`--no-subagents` die Delegation abschalten.

**Windows PowerShell vs. pwsh.** Nutze möglichst **PowerShell 7+ (`pwsh`)** — dort ist die
Standard-Dateikodierung UTF-8, und `-Form` (Multipart-Upload) ist verfügbar.

---

## 20. Anhang: Referenztabellen

### Exit-Codes

| Code | Bedeutung |
|---|---|
| 0 | Erfolg |
| 1 | Laufzeitfehler |
| 2 | API/Netz (Modell unerreichbar, Rate-Limit, Auth) |
| 3 | Kontext zu groß / Prompt ungültig |
| 4 | `--format json` nicht erzeugbar |
| 130 | mit Ctrl-C abgebrochen |

### Wichtige Umgebungsvariablen

| Variable | Zweck |
|---|---|
| `OPENAI_API_KEY`, `OPENAI_MODEL` | OpenAI-Zugang |
| `OPENAI_BASE_URL` | lokaler/kompatibler OpenAI-Server (Ollama, LM Studio, vLLM, …); Key optional |
| `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_DEPLOYMENT`, `AZURE_OPENAI_API_VERSION` | Azure-Zugang |
| `NO_COLOR` | Farbausgabe global abschalten |

### Unterbefehle

| Befehl | Zweck |
|---|---|
| `agentkit read-pdf <datei>` | PDF-Text auf stdout (kein LLM; Feature `pdf`) |
| `agentkit completions <shell>` | Completion-Skript (bash\|zsh\|fish\|powershell) |
| `agentkit --help` / `--version` | Hilfe / Version |

### Wo weiterlesen

- **Pipe-Details & Beispiele:** `agent_framework_rs/README.md`
- **Großer Praxis-Workflow:** `agent_framework_rs/examples/accounts_payable/`
- **Installation & Deployment:** `INSTALL.md`
- Live-Hilfe: `agentkit --help`

---

*Viel Erfolg beim Automatisieren. Wenn du im Terminal denken kannst, kannst du es mit agentkit
bauen: kleine, klare Schritte — deterministisch wo möglich, mit einem Agenten wo Urteilskraft
nötig ist — über Pipes und Dateien zu einem Ganzen verkettet.*
