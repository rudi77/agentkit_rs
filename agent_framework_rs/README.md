# agentkit (Rust)

Rust-Port des Python-`agentkit` aus [`../agent_framework`](../agent_framework) —
**so strukturgleich wie möglich**, damit sich Rust und Python direkt vergleichen
lassen. Kernidee bleibt: **Ein Agent ist ein LLM in einer Schleife mit Tools.**

```text
solange das Modell ein Tool aufruft:
    Tool ausführen -> Ergebnis anhängen -> Modell erneut fragen
sonst:
    finale Antwort
```

## Was drin ist (1:1 zum Python-Original)

| Baustein | Datei | Python-Pendant |
|---|---|---|
| **Agentic Loop** | `src/agent.rs` | `agentkit/agent.py` — streamend, event-basiert; ReAct/Plan/Plain über `Strategy`; parallele Tool-Calls; Harness (max_steps, Retries, Fehlertoleranz, Compaction, Stop-Knopf) |
| **Tools** | `src/tools.rs` | `tools.py` — `ToolRegistry` (Schema explizit; Rust hat keine Laufzeit-Reflection) |
| **Coding-Tools** | `src/coding.rs` | `coding.py` — `CodingTools` mit Sandbox + Approval; `glob_files`/`grep` (read-only Suche), `READ_ONLY_TOOLS`-Teilmenge, `register(only)` |
| **Skills** | `src/skills.rs` | `skills.py` — `Skills` + `list_skills`/`read_skill`, progressive disclosure, `body_after_frontmatter` |
| **Planning** | `src/planning.rs` | `planning.py` — `Plan` + `update_plan` |
| **Sub-Agents** | `src/subagents.rs` | `subagents.py` — `add_subagent` (Funktion, kein eigener Typ: ein Sub-Agent *ist* ein `Agent`) |
| **Rollen / task-Tool** | `src/roles.rs` | `roles.py` — `AgentRole`, `builtin_roles` (explorer/reviewer/tester/general), `add_task_tool`, `load_roles_from_dir` (Claude-Code-Stil) |
| **Events** | `src/events.rs` | `events.py` — `AgentEvent` + `EventBus` (mpsc-Kanäle) |
| **Memory** | `src/memory.rs` | `memory.py` — `ShortTermMemory` + `LongTermMemory` |
| **MCP** | `src/mcp.rs` | `mcp.py` — `MCPClient` (synchrone stdio-Session, ohne async-Runtime) |
| **LLM** | `src/llm.rs` | `llm.py` — `Llm`-Trait + `OpenAiLlm` (Azure/OpenAI über `ureq`) |
| **FakeLlm** | `src/testing.rs` | der `FakeLLM` aus den Python-Tests |

### Bewusste Unterschiede zu Python

- **Tool-Schemas explizit.** Python leitet das Schema per `@tool()` aus Typ-Hints
  + Docstring ab. Rust hat keine Laufzeit-Reflection — das Schema wird als
  `serde_json::Value` übergeben (`registry.add(...)`). `add_typed` deserialisiert
  die Argumente typsicher.
- **Events typisiert.** Statt `data: Any` eine `EventData`-Enum; die `type`-Strings
  (`"step"`, `"tool_call"`, …) sind identisch.
- **Streaming per Callback statt Generator.** `run_iter` (Python-Generator) wird zu
  `run_with_events(task, cancel, |ev| ...)`. Darauf bauen `run`, `run_cb` und
  `run_on_bus` auf.
- **Parallele Tools** über `std::thread::scope` (Python: `ThreadPoolExecutor`).
- **MCP synchron.** Der stdio-Transport ist zeilengetrenntes JSON-RPC; in Rust
  genügt eine `Mutex`-geschützte Session — keine asyncio-Schleife im Thread nötig.
- **Größeres Tool-Output-Limit.** `ShortTermMemory`-`TRUNCATE_LIMIT` ist `16000`
  Zeichen statt der `2000` des Python-Originals — großzügig gewählt, damit der
  Coding-Agent ganze Dateien sowie `grep`-/`tree`-Ausgaben sieht, statt nach ~500
  Tokens abzubrechen.
- **PLAN-Event trägt strukturierte Daten.** Statt eines vorgerenderten Strings
  überträgt `EventData::Plan` die Schrittliste (`Vec<Step>`); das jeweilige Frontend
  rendert sie selbst (CLI mehrzeilig, TUI einzeilig) via `render_steps`.
- **Benutzer-Config `~/.agentkit/config.json`** (`src/config.rs`, kein Python-Pendant).
  Die Rust-Variante wird als Executable *installiert* und läuft damit außerhalb des
  Projektverzeichnisses, wo keine `.env` liegt. Kein zweites Config-System: die Datei
  wird auf dieselben `AZURE_OPENAI_*`-Variablen abgebildet und setzt nur, was noch nicht
  gesetzt ist — echte Umgebung > `.env` > User-Config.
- **Scriptbare interaktive Session (`--repl`) + mehrzeilige TUI-Eingabe.** Über die Python-Vorlage
  hinaus erzwingt `--repl` die interaktive Session auch bei gepiptem stdin (Kommandos und
  Folge-Antworten von stdin — für Automatisierung/Tests), und das TUI-Eingabefeld ist mehrzeilig
  (Alt-Enter fügt eine Zeile ein). **Human-in-the-Loop braucht kein Sonderwerkzeug:** der Agent
  stellt eine Rückfrage, indem er seinen Zug beendet; die nächste Eingabe beantwortet sie, und er
  macht mit vollem Gesprächsverlauf weiter. Motiviert vom interaktiven Accounts-Payable-Orchestrator
  (siehe unten).

## In 12 Zeilen (ohne Netz, FakeLlm)

```rust
use std::sync::Arc;
use agentkit::{Agent, ToolRegistry};
use agentkit::testing::FakeLlm;
use agentkit::llm::Chunk;
use serde_json::json;

let mut tools = ToolRegistry::new();
tools.add("add", "Addiert zwei Zahlen.",
    json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
    |args| Ok((args["a"].as_i64().unwrap() + args["b"].as_i64().unwrap()).to_string()));

let llm = Arc::new(FakeLlm::new(vec![
    vec![Chunk::tool(0, "c1", "add", "{\"a\":17,\"b\":25}")],
    vec![Chunk::text("Das Ergebnis ist 42.")],
]));
let mut agent = Agent::new(llm, tools);
println!("{}", agent.run("Was ist 17 + 25?"));
```

Mit echtem Modell (Feature `openai`, Default an):

```rust
let llm = std::sync::Arc::new(agentkit::azure_from_env()?); // oder openai_from_env()
let mut agent = agentkit::Agent::new(llm, tools);
```

## Bauen, Testen, Beispiele

```bash
cargo test --no-default-features          # Tests ohne Netz/TLS-Abhängigkeiten
cargo build                               # mit Feature `openai` (ureq + rustls)
cargo run --example react_fake --no-default-features
cargo run --example parallel_subagents --no-default-features
```

## Benutzerhandbuch

Für Endanwender, die nur das `agentkit`-Kommando nutzen (kein Rust nötig), gibt es ein
vollständiges **[Benutzerhandbuch](docs/USER_MANUAL.md)** — inkl. Kochbuch zum Bauen ganzer
Workflows in PowerShell/Bash.

## Als Executable `agentkit` installieren

Das Crate liefert ein installierbares Binary `agentkit` (CLI + optionales TUI) — mit
echtem LLM ist es der **volle Coding-Agent** (Sandbox-Tools inkl. `glob`/`grep`, Skills,
Plan, `task`-Tool für Sub-Agenten), ohne Key ein netzfreier Demo-Modus:

```powershell
# Windows, ohne Rust-Toolchain: Release-Binary holen, in den PATH legen, Config anlegen
irm https://raw.githubusercontent.com/rudi77/fsod/main/scripts/agentkit_setup.ps1 | iex
```

```bash
cargo install --path . --bin agentkit --features "pdf tui"   # nach ~/.cargo/bin
agentkit "Was ist 17 + 25?"          # One-shot im aktuellen Verzeichnis
agentkit                             # interaktive Session (REPL)
agentkit --tui                       # interaktives Terminal-UI (Feature `tui`)
agentkit --demo "3 + 4"              # Demo-Modus erzwingen (kein Key nötig)
agentkit config show                 # Konfiguration prüfen (~/.agentkit/config.json)
```

Wichtige Optionen (wie die Python-CLI): `-w/--workspace`, `-s/--strategy react|plan|plain`,
`--skills DIR`, `--agents DIR` (Custom-Rollen als `*.md`), `--memory FILE`,
`--provider auto|azure|openai|demo`, `--max-steps N`, `--no-subagents`, `-y/--yes`
(Shell ohne Rückfrage), `--steps`, `--no-color`, `-p/--print`, für MCP
`--mcp-config FILE`, `--mcp NAME` (mehrfach) und `--no-mcp` (siehe **MCP** unten), sowie
für per-Agent-Config `--system TEXT`, `--system-file FILE` und `--profile FILE`
(Config-Bündel je Pipe-Stage — siehe **Pro-Agent-Config** unten).
Slash-Befehle in der Session: `/help /clear /reset /plan /tools /skills /agents /mcp /exit`.
`Ctrl-C` bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).

**Konfiguration** (`agentkit config init|path|show`, siehe [`src/config.rs`](src/config.rs)):
die Zugangsdaten stehen in `~/.agentkit/config.json` (Windows: `%USERPROFILE%\.agentkit\`),
damit die installierte Executable überall läuft — nicht nur im Projektverzeichnis. Sie ist
die unterste Ebene einer Kette, jede darüber gewinnt:

```text
echte Umgebungsvariable  >  .env im Arbeitsverzeichnis  >  ~/.agentkit/config.json
```

Alle drei speisen dieselben Variablen (`AZURE_OPENAI_*` / `OPENAI_API_KEY`) — der Rest des
Codes liest weiter nur die Umgebung. Platzhalter (`<…>`) in der Config gelten als *nicht
gesetzt*, eine frische Vorlage landet also sauber im Demo-Modus.

Setup-Skript, Install-Skripte (Windows & Linux) und fertige CI-Release-Binaries:
siehe **[../INSTALL.md](../INSTALL.md)**.

## Unix-Pipe-Kompatibilität — `agentkit` als nativer Filter

Zusätzlich zum interaktiven Coding-CLI verhält sich die `agentkit`-Executable wie ein
ordentlicher Unix-Filter. Die Standard-Streams sind die primären I/O-Adapter
(hexagonale Architektur — der Agent-Kern bleibt unberührt):

| Stream | Inhalt |
|---|---|
| **`stdin`** | *nur* Kontext/Datenströme. Ist `stdin` nicht interaktiv (Pipe/Umleitung), wird der gesamte Inhalt gelesen und an die Query angehängt. |
| **`stdout`** | sobald die Ausgabe gepipt wird, im `--format json`- oder `-p/--print`-Modus läuft: *nur* das finale, bereinigte Resultat. So kann ein nachfolgendes `jq`/`awk`/ein zweiter Agent sich auf Format-Treue verlassen. |
| **`stderr`** | alles andere: Status, Tool-Spur, ReAct-Gedanken, Fehler. |

```bash
# stdin = Kontext, stdout = reines Resultat, Denkprozess sichtbar auf stderr:
cat daten.json | agentkit --format json "Extrahiere die Summe" | jq .summe

# In einer Pipe streamt die Spur auf stderr (beobachtbar), stdout bleibt sauber:
agentkit -p "Fasse zusammen" < bericht.txt > ergebnis.txt
```

### Pipe-Parameter

| Parameter | Bedeutung |
|---|---|
| `[AUFTRAG]…` | Hauptargument (mehrere Wörter ok). Optionen stehen **vor** dem Prompt. |
| `--format <text\|json>` | Erzwingt das Ausgabeformat. `json` aktiviert den OpenAI/Azure JSON-Mode plus Validierung; gelingt das trotz `--json-retries` nicht, Exit-Code 4. |
| `--dry-run` | Führt den Loop aus, blockiert aber zerstörerische Schreib-/MCP-Vorgänge (Heuristik per Tool-Name) und loggt die versuchten Aktionen nur auf `stderr`. |
| `--max-context <TOKENS>` | Kontext-Limit (Default 128000); größer ⇒ Exit-Code 3. |
| `-p`/`--print` | One-shot: nur die finale Antwort auf `stdout`. |
| `--system <TEXT>` | Agenten-spezifischer Zusatz-System-Prompt (Persona/Format je Pipe-Stage). |
| `--system-file <FILE>` | Wie `--system`, aber aus Datei (überschreibt `--system`). |
| `--profile <FILE>` | **Config-Bündel je Agent** (JSON) — siehe unten. Explizite Flags gewinnen. |

### Pro-Agent-Config: `--profile` (eine Datei je Pipe-Stage)

Damit jede Stage einer Pipe *ein* eigenes Config-Bündel bekommt (eigener System-Prompt,
Tools/Skills, MCP-Server, Strategie …), statt vieler Einzel-Flags, liest `--profile FILE`
eine JSON-Datei. **Explizite CLI-Flags überschreiben** die Profilwerte (Profil = Basis).

```jsonc
// extractor.json — eine spezialisierte Pipe-Stage
{
  "system": "Du extrahierst Struktur. Antworte NUR mit gültigem JSON, keine Prosa.",
  // "system_file": "prompts/extractor.md",   // Alternative: Prompt aus Datei
  "strategy": "plain",           // react | plan | plain
  "provider": "azure",           // auto | azure | openai | demo
  "skills":   "./skills/extract",
  "agents":   "./roles/extract", // Custom-Sub-Agent-Rollen (*.md)
  "mcp_config": "./mcp/git.json",
  "mcp":      ["git"],           // Allowlist aktiver MCP-Server
  "no_mcp":   false,
  "no_subagents": false,
  "workspace": ".",
  "memory":   "./mem/extractor.jsonl",
  "max_steps": 80,
  "format":   "json",            // text | json
  "dry_run":  false
}
```

Alle Felder sind optional. Damit wird die Pipe zu einer Kette klar getrennter Agenten:

```bash
cat src/lib.rs \
 | agentkit -p --profile agents/extractor.json "Extrahiere alle öffentlichen Funktionen" \
 | agentkit -p --profile agents/rater.json     "Bewerte jede nach Komplexität" \
 | agentkit -p --profile agents/writer.json    "Schreibe einen Markdown-Report"
```

Hinweis: `--system`/`--system-file`/`profile.system` **erweitern** den Coding-System-Prompt
(als Block „Agenten-spezifische Instruktionen" am Ende) — die Tool-Instruktionen bleiben
erhalten, du steuerst Persona/Format/Fokus. Für reine LLM-Transform-Stages kombiniere das
mit `"strategy": "plain"` und ggf. `"no_subagents": true`.

Die übrigen Optionen (`--workspace`, `--provider`, `--skills`, `--agents`, `--memory`,
`--max-steps`, `--no-subagents`, `-y`, `--steps`, `--no-color`, `--demo`, `--plan`/
`--plain`, `--tui`) sind unter `agentkit --help` dokumentiert.

### Exit-Codes (für `set -e`-Pipelines)

| Code | Bedeutung |
|---|---|
| `0` | Erfolg — Resultat auf `stdout` geflusht. |
| `1` | Unerwarteter Laufzeitfehler. |
| `2` | API/Netz (Modell unerreichbar, Rate-Limit). |
| `3` | Kontext zu groß oder Prompt ungültig/leer. |
| `4` | Erzwungenes `--format` trotz Retries nicht erzeugbar. |

Die Pipe-Bausteine (Exit-Codes, Format, stdin-/JSON-Helfer) liegen entkoppelt und
testbar in `src/cli.rs`; das Argument-Parsing selbst im `agentkit`-Binary.

### Argument-Konventionen (GNU/POSIX)

`agentkit` hält sich an die üblichen Shell-Konventionen, damit es sich wie ein natives
Kommando anfühlt:

- **`--flag=value`** und **`--flag value`** sind gleichwertig (`--workspace=/tmp` ⇔
  `--workspace /tmp`).
- **`--`** beendet die Optionen — alles danach ist wörtlicher Auftrag, auch wenn es mit
  `-` beginnt: `agentkit -- "-p ist hier nur Text"`.
- **Unbekannte Optionen** werden nicht still verschluckt, sondern auf `stderr` gemeldet
  (`[WARN] unbekannte Option ignoriert: …`) — Tippfehler fallen auf.
- **Broken Pipe:** In einer Pipe wie `agentkit … | head -1` endet der Prozess sauber
  (SIGPIPE) statt mit einem Panic — ein ordentlicher Unix-Filter.

### Shell-Completions

Tab-Vervollständigung für **bash, zsh, fish und PowerShell** — das Skript wird zur
Laufzeit erzeugt und auf `stdout` ausgegeben:

```bash
agentkit completions bash        # bash
agentkit completions zsh         # zsh
agentkit completions fish        # fish
agentkit completions powershell  # PowerShell (auch pwsh)
```

Einbinden:

```bash
# bash — sofort in der aktuellen Shell:
source <(agentkit completions bash)
# bash — dauerhaft (User-Verzeichnis, XDG):
agentkit completions bash > ~/.local/share/bash-completion/completions/agentkit

# zsh — in einen fpath-Ordner legen, dann `compinit`:
agentkit completions zsh > "${fpath[1]}/_agentkit"

# fish:
agentkit completions fish > ~/.config/fish/completions/agentkit.fish
```

```powershell
# PowerShell — aktuelle Sitzung:
agentkit completions powershell | Out-String | Invoke-Expression
# PowerShell — dauerhaft:
agentkit completions powershell >> $PROFILE
```

Vervollständigt werden Flags samt Werten (`--strategy` → `react|plan|plain`,
`--provider` → `auto|azure|openai|demo`, `--format` → `text|json`) sowie Datei-/
Verzeichnispfade für `-w/--workspace`, `--skills`, `--profile`, `--mcp-config` etc. Die
`install.sh`/`install.ps1`-Skripte richten die passende Completion beim Rust-Build
automatisch ein (best effort).

### PDF lesen — `read-pdf` (Feature `pdf`)

Mit dem Feature `pdf` bringt agentkit die PDF-Textextraktion mit — in zwei Formen:

- **`agentkit read-pdf <datei.pdf>`** — ein deterministisches, tokenfreies Utility, das den
  reinen Text auf `stdout` schreibt. Perfekt komponierbar: `agentkit read-pdf rg.pdf > rg.txt`.
- **`read_pdf`-Tool** — dasselbe innerhalb der Sandbox, damit Agenten PDFs lesen können
  (read-only, Teil der `READ_ONLY_TOOLS`).

```bash
cargo build --release --bin agentkit --features pdf      # oder: --features "pdf tui"
agentkit read-pdf rechnung.pdf | agentkit -p --format json --system-file extract.md "Extrahiere Felder"
```

### Beispiel: Accounts-Payable-Pipeline (Kompositionsprinzip)

Ein vollständiges, praxisnahes Beispiel — Eingangsrechnungen (Papier-PDF, **XRechnung** und
**ZUGFeRD**) einlesen, bei E-Rechnungen die **EN-16931-Konformität** über die
**xcheck-API** (separates Repo `rudi77/xcheck`) prüfen, §14-UStG-Merkmale extrahieren, validieren, nach SKR03
verbuchen, einen **DATEV-Buchungsstapel** exportieren, **GoBD-konform** (SHA-256-Manifest,
schreibgeschützt) ablegen und Dubletten erkennen — als **PowerShell-Pipeline aus einzelnen
agentkit-Agenten** (ein Agent bzw. Werkzeug pro Schritt) liegt unter
[`examples/accounts_payable`](examples/accounts_payable/README.md). Es zeigt Komposition,
`--format json`-Format-Treue zwischen Stufen und „das richtige Werkzeug pro Schritt“
(deterministisches `read-pdf`/xcheck fürs Faktische, LLM-Agenten fürs Urteilen).

### Beispiel: interaktiver AP-Orchestrator (Human-in-the-Loop + lernender Wissensgraph)

Dasselbe Beispiel läuft in **zwei Modi** (`.\Invoke-Ap.ps1 -Mode Batch|Interactive|Repl`), die
sich Fach-Logik, Compliance-Werkzeuge und Seeds teilen. Im **interaktiven** Modus managt ein
**Orchestrator-Agent** („Leiterin der Buchhaltung“) die Fach-Agenten
(`extractor`/`validator`/`booker`) über das `task`-Werkzeug, **ruft dieselben deterministischen
Compliance-Werkzeuge** (xcheck/GoBD/DATEV/Dublette) auf, **fragt bei Unklarheiten beim Menschen
nach** (indem er seinen Zug mit der Frage beendet — die nächste Eingabe beantwortet sie) und baut
dabei einen **Company Knowledge Graph im OKF-Format** (Markdown-Entitäten mit Frontmatter +
`[[links]]`) auf — die Buchhaltung **lernt dazu** und fragt bekannte Lieferanten kein zweites Mal.
Läuft im TUI (mehrzeilige Eingabe) oder im scriptbaren `--repl` und ist damit ein **Superset** der
Batch-Fähigkeiten.

## MCP — Tools über das Model Context Protocol

Der Agent kann Tools von externen **MCP-Servern** beziehen (stdio-Transport, JSON-RPC) —
für den Haupt-Agenten **und** die Sub-Agenten (`task`-Tool). Die Server werden
deklarativ in einer `.mcp.json` beschrieben (Claude-Code-Format) und je Agent
**ein-/ausschaltbar** — statisch per Flag im Pipe-Modus, live im REPL/TUI.

```jsonc
// .mcp.json (im Workspace oder CWD — wird automatisch gefunden)
{
  "mcpServers": {
    "git":  { "command": "uvx", "args": ["mcp-server-git", "--repo", "."] },
    "fs":   { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
              "env": { "FOO": "bar" } },
    "extra":{ "command": "node", "args": ["server.js"], "disabled": true }
  }
}
```

Die Server-Tools erscheinen **namespaced** als `mcp__<server>__<tool>` (keine Kollision
mit lokalen Tools). Auto-Discovery sucht `.mcp.json` (dann `mcp.json`) im Workspace und
CWD; ein expliziter Pfad geht via `--mcp-config FILE`.

```bash
agentkit --mcp-config .mcp.json "Nutze das git-Tool und fasse die letzten Commits zusammen"
agentkit --mcp git "…"        # Allowlist: nur den Server 'git' aktiv (mehrfach möglich)
agentkit --no-mcp "…"         # MCP komplett aus
```

**Enable/Disable**

- **Pipe/One-shot (statisch):** Ohne `--mcp` sind alle nicht als `"disabled": true`
  markierten Server aktiv. `--mcp NAME` schaltet eine **Allowlist** (nur die genannten),
  `--no-mcp` alles ab. `--dry-run` blockiert zusätzlich zerstörerische MCP-Aufrufe.
- **REPL (live):** `/mcp` listet die Server samt Status, `/mcp on <name>` bzw.
  `/mcp off <name>` schaltet sie für den laufenden Agenten um (ohne Neustart).
- **TUI (live):** **F2** öffnet das MCP-Panel — `↑↓` wählen, `Space` schalten; die
  Titelzeile zeigt `MCP <aktiv>/<gesamt>`.

Technisch hält ein geteilter `McpHub` die (einmal aufgebauten) stdio-Sessions; nur ein
atomares `enabled`-Flag je Server wird umgeschaltet. Der Haupt-Agent wird dabei aus
seiner MCP-freien Basis-Registry neu verdrahtet, neu gespawnte Sub-Agenten lesen den
aktuellen Stand direkt. MCP bleibt **synchron** (kein async-Runtime): der stdio-Transport
ist zeilengetrenntes JSON-RPC über eine `Mutex`-geschützte Session.

## TUI — interaktives Terminal-UI

Ein vollwertiges Terminal-UI für den Agenten (Binary `tui`, Feature `tui`). Es ist
**nur ein weiterer Consumer** des bestehenden Event-Stroms: Der Agent läuft in einem
Worker-Thread und ruft `run_on_bus`; das UI abonniert den `EventBus` und rendert
Schritte, Tool-Calls und gestreamte Tokens live. `Esc` setzt den kooperativen
Stop-Knopf (`Cancel`). Kein async-Runtime — nur `ratatui` als Extra-Abhängigkeit
(crossterm kommt re-exportiert über `ratatui::crossterm`), und nur wenn das Feature
aktiv ist; der Standard-Build bleibt schlank.

Mit echtem LLM ist das TUI der **volle Coding-Agent** (wie das CLI): Sandbox-Tools
inkl. `glob`/`grep`, Skills, Plan und das `task`-Tool für Sub-Agenten. Da `ratatui`
das Terminal belegt, läuft die `run_shell`-Freigabe nicht über stdin, sondern über
einen **In-TUI-Dialog**; mit **Ctrl-Tab** (oder `Shift-Tab`) schaltet man zwischen
*Nachfragen* und *Auto-Freigabe* um — wie der Permission-Mode in der Claude-Code-CLI.

```bash
cargo run --bin tui --features tui                       # mit Azure/OpenAI (Default)
cargo run --bin tui --no-default-features --features tui  # nur Demo-Modus (kein Netz)
cargo run --bin tui --features tui -- --demo             # Demo-Modus erzwingen
cargo run --bin tui --features tui -- --help             # Optionen & Tasten
# oder über die Haupt-Executable:
agentkit --tui -w . --skills ./skills
```

Optionen wie im CLI: `-w/--workspace`, `--skills`, `--agents`, `--memory`,
`--no-subagents`, `--max-steps`, `-y/--yes` (Freigabe initial auf AUTO), `--plan`/`--plain`.
Konfiguration wie im CLI (`.env` im Arbeitsverzeichnis, sonst `~/.agentkit/config.json`).
LLM-Auswahl (ohne `--demo`): `AZURE_OPENAI_*` → Azure, sonst `OPENAI_API_KEY` (+ optional
`OPENAI_MODEL`) → OpenAI, sonst der netzfreie **Demo-LLM**. MCP-Optionen (`--mcp-config`, `--mcp`, `--no-mcp`) gelten
auch hier; **F2** öffnet im UI das MCP-Panel zum Ein-/Ausschalten der Server. Tasten:
`Enter` senden, `Esc` abbrechen/beenden, `Ctrl-Tab` Freigabe-Modus umschalten, `F2`
MCP-Panel, `Ctrl-C` beenden, `↑↓/PgUp/PgDn/End` scrollen.

## Performance: Rust vs. Python

Die Benchmarks messen **reinen Framework-Overhead** mit einem FakeLlm (kein Netz —
bei echten Calls dominiert die LLM-Latenz und ist für beide identisch). Beide Seiten
fahren **dieselben Szenarien mit denselben Iterationszahlen**; die Token-Zählung
nutzt beidseitig den `len//4`-Fallback (kein tiktoken).

```bash
python3 ../benchmarks/compare.py          # baut Rust-Release + führt beide aus
python3 ../benchmarks/compare.py --scale 0.2   # schneller
```

Beispiel-Lauf (Linux, Python 3.11; vollständige Tabelle in
[`../benchmarks/RESULTS.md`](../benchmarks/RESULTS.md)):

| Szenario | Python | Rust | Speedup |
|---|---:|---:|---:|
| Agent-Loop (1 Tool + Antwort) | 17.6 µs | 6.4 µs | 2.8× |
| 8 parallele Tool-Calls | 876 µs | 261 µs | 3.4× |
| Tool-Dispatch (Registry.call) | 271 ns | 105 ns | 2.6× |
| Token-Zählung (20 Msgs) | 2.03 µs | 430 ns | 4.7× |
| Skill-Frontmatter parsen | 1.15 µs | 220 ns | 5.3× |
| JSON dump+parse | 4.72 µs | 1.18 µs | 4.0× |

**Geometrisches Mittel ≈ 3.6× schneller.** Einordnung:

- Rechenlastige, allokationsarme Pfade (Token-Zählung, Frontmatter-Parsing) profitieren
  am stärksten (~5×).
- Der volle Agent-Loop liegt niedriger (~2.8×): Ein großer Teil ist `serde_json`-Value-
  Allokation/-Klonen und Thread-Aufbau — beide Sprachen allozieren hier. Dafür ist
  die Speichernutzung in Rust deutlich kompakter und ohne GC-Pausen.
- Bei **echten** LLM-Calls verschwindet dieser Overhead im Netzwerk-Rauschen — der
  Rust-Vorteil zählt v. a. bei hohem Tool-/Event-Durchsatz, vielen parallelen
  Sub-Agents und vorhersagbarer Latenz (kein GC).
