# agentkit (Rust)

Rust-Port des Python-`agentkit` (aus dem [fsod-Repo](https://github.com/rudi77/fsod),
`agent_framework/`) — **so strukturgleich wie möglich**, damit sich Rust und Python
direkt vergleichen lassen. Kernidee bleibt: **Ein Agent ist ein LLM in einer Schleife mit Tools.**

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

- **`doc_ingest` — große Dokumente als Abschnitte (`src/documents.rs`).** Das Original
  kennt kein Pendant. Das Tool zerlegt ein PDF (Feature `pdf`), Markdown oder Text an
  den Überschriften deterministisch in Abschnitte von höchstens 12.000
  Zeichen und legt sie als Verzeichnis ab — eine Datei je Abschnitt mit dem Wortlaut der
  Quelle und der Fundstelle im Frontmatter, dazu `index.md`. Kein Modell fasst zusammen,
  kein Index, keine Embeddings: gelesen und gesucht wird mit `read_file` und `grep`. Das
  Layout ist ein OKF-Bundle, damit dieselben Werkzeuge wie für den Wissensgraphen darauf
  arbeiten. Siehe „Dokumente auswerten — `doc_ingest`".
- **MCP-Kommandos unter Windows über `PATHEXT`** (`src/mcp.rs`). `"command": "npx"`
  startet dort `npx.cmd`: ein Name ohne Endung wird wie in der Shell über `PATH` und
  `PATHEXT` gesucht (nur `.com`, `.exe`, `.bat`, `.cmd`; ein `PATH` im `env`-Block des
  Servers geht vor). Ohne das hängt Windows nur `.exe` an, und eine `.mcp.json` im
  üblichen Format endet mit „program not found". Auf anderen Systemen ändert sich nichts.
- **`--agents` nimmt mehrere Verzeichnisse** (`;`-getrennt wie `--skills`, das später
  genannte gewinnt). So hängt die Executable die Rollen installierter Plugins an, ohne dass
  der Kern Plugins kennt (siehe „Skills und Plugins installieren").
- **`TurnContext` — Text vor dem Auftrag des Nutzers.** `Agent::turn_context` ist eine
  optionale Funktion, deren Ergebnis `run_with_strategy` dem Auftrag voranstellt. Die
  Naht für Frontends, die dem Modell vor jedem Zug etwas mitgeben, das der Kern nicht
  kennt (die Executable: Recall aus dem Wissensgraphen). Vor der Nutzer-Nachricht statt
  im System-Prompt, damit dessen Cache-Präfix stabil bleibt. Ohne sie verhält sich der
  Loop wie das Original. Aus demselben Grund kennt das TUI `TuiCommand`: Slash-Befehle,
  die das Frontend mitbringt (`/graph`), und `TuiConfig::on_exit` für das, was nach der
  Sitzung noch geschieht.
- **Verwalteter Kontext folgt `/reset`, `/rewind` und `/fork`.**
  `Agent::reset_conversation` leert Spiegel UND ctxman-Kontext (vorher sah das Modell
  nach `/reset` weiter den ganzen Verlauf); `Agent::rewind_to_turn` baut den
  ctxman-Kontext aus dem gekürzten Spiegel neu auf, statt abzulehnen. Bleibt ein Render
  trotz Minor und Major GC über dem Budget, fragt der Schritt mit dem ungekürzten
  Verlauf weiter, statt mit „(keine Antwort)" zu enden — das Budget ist eine selbst
  gesetzte Grenze, nicht das Fenster des Modells. Im Strom steht dann das Ereignis
  `render_over_budget`.
- **Zwei Nähte zum Mitlesen und Abschließen.** `McpHub::on_tool_result` meldet jedes
  erfolgreiche MCP-Tool-Ergebnis an einen Zuhörer — MCP-Tools kommen nach den
  Frontend-Tools in die Registry und werden bei jedem Umschalten neu eingeklinkt, an
  der Registry kann man sie also nicht abhören. `Agent::on_session_end` ruft das
  ACP-Frontend, wenn der Editor die Verbindung schließt; der Kern selbst ruft es nie.
- **`FactListener` — Fakten des Kontext-Managements an einen zweiten Empfänger.**
  `ManagedContext::set_fact_listener` meldet jeden promoteten Fakt zusätzlich zur
  Fakten-Datei weiter. Der Kern weiß nicht, wohin (die Executable: in den Graphen).
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
- **Abbruch greift auch in die Tool-Ausführung.** Das Python-Original prüft das
  `threading.Event` nur zwischen den Loop-Schritten. Hier bricht der Stop-Knopf
  zusätzlich einen LAUFENDEN `run_shell`/`git`-Kindprozess sofort ab
  (`run_with_timeout` pollt den Cancel und beendet unter Unix die ganze Prozessgruppe
  des Kinds, also auch dessen eigene Kinder; unter Windows nur den direkten
  Kindprozess) und überspringt nach
  dem Abbruch noch ausstehende Tool-Aufrufe mit einem weichen `ERROR: abgebrochen.`
  (jede `tool_call`-id behält so ihr Ergebnis). Dasselbe gilt für einen Abbruch
  mitten im Stream: der schon gestreamte Text bleibt im Verlauf, ein Tool-Aufruf mit
  abgeschnittenen Argumenten fällt weg, ein vollständiger, aber nicht mehr ausgeführter
  bekommt `(abgebrochen — kein Ergebnis geliefert)` — sonst lehnte der Provider jede
  Folgeanfrage der Sitzung mit HTTP 400 ab.
- **Kindprozesse von Tools und Hooks erben kein stdin; Windows bevorzugt `pwsh`.**
  `run_with_timeout` setzt stdin auf null — im ACP-/MCP-Server-Modus wäre es sonst die
  Protokoll-Pipe (PowerShell wartet dort nach `-Command` auf ein EOF, das nie kommt).
  Unter Windows nutzt `shell_command` `pwsh` (PowerShell 7), falls auf dem PATH, sonst
  `powershell`, jeweils mit `-NoProfile -NonInteractive`. Kein Python-Pendant.
- **Timeout und Abbruch beenden den ganzen Prozessbaum, auch unter Windows.** Unter Unix
  bekommt die Prozessgruppe des Kindes SIGKILL, unter Windows läuft vor `Child::kill` ein
  `taskkill /T /F` — sonst lief ein Build unter der beendeten PowerShell weiter. Beides ohne
  Abhängigkeit; ein Enkel, dessen Elternprozess schon beendet ist, entgeht `taskkill /T`.
- **Suche überspringt mehr Ordner und ist abbrechbar.** Die Ignore-Liste von `glob_files`/`grep`
  enthält zusätzlich `target`, `.codegraph` und `.claude/worktrees` (Repo-Kopien paralleler
  Sessions; `.claude` selbst bleibt durchsuchbar) — ein `grep **/*` über GB an Build-Artefakten
  ließ ACP-Clients in ihre Leerlauf-Frist laufen. Außerdem prüfen Walk und `grep` den Stop-Knopf
  je Verzeichnis/Datei und enden mit `ERROR: abgebrochen.`; `agentkit acp` sendet für offene
  Tool-Aufrufe alle 30 s ein `tool_call_update` (`in_progress`) als Lebenszeichen.
- **Skills aus dem Claude-Code-Ökosystem lesbar** (`src/skills.rs`, `src/roles.rs`; kein
  Python-Pendant). Drei Zugeständnisse an Skill-Sammlungen, die für Claude Code geschrieben
  wurden — jedes behebt einen *stillen* Fehlschlag, keinen sichtbaren:
  - `parse_frontmatter` versteht **YAML-Block-Scalars** (`>`, `>-`, `>+`, `|`, `|-`, `|+`).
    Vorher las die einzeilige Variante bei `description: >-` den Wert als `">-"` und die
    Folgezeilen als eigene Schlüssel — der Skill-Index war damit unbrauchbar, ohne dass
    irgendwo ein Fehler entstand. Bewusst kein YAML-Crate: die Abhängigkeitsfreiheit des
    Kerns ist teurer als die paar Zeilen Faltlogik, und Frontmatter ist kein YAML-Dokument.
  - `read_skill` ersetzt **`${CLAUDE_SKILL_DIR}`** durch den absoluten Pfad des Skill-Ordners
    und **`$ARGUMENTS`** durch die (optionalen) Argumente des Aufrufs. Skills, die eigene
    Scripts mitbringen, sind sonst unbenutzbar — und auf Windows besonders tückisch, weil
    PowerShell `${…}` als *eigene* Variablensyntax zu Leerstring expandiert statt zu
    scheitern. Der eingesetzte Pfad nutzt Forward-Slashes, die beide `run_shell`-Shells
    akzeptieren.
  - `parse_tools_field` übersetzt **Claude-Code-Toolnamen** (`Read`, `Write`, `Edit`,
    `MultiEdit`, `Bash`, `Grep`, `Glob`, `LS`) auf agentkits Namen; `Task`, `TodoWrite`,
    `WebFetch`, `WebSearch`, `NotebookEdit` fallen weg (`Task` wegen der
    Ein-Ebenen-Invariante). Ohne die Übersetzung matchte kein einziger Name und die Rolle
    bekam eine leere Registry. Ein nicht-leeres `tools:`-Feld liefert seitdem nie `None` —
    `None` hieße „alle Tools", eine unauflösbare Auswahl darf keine Rechte *hinzufügen*.
- **Mehrere Skill-Wurzelverzeichnisse.** `--skills` ist mehrfach angebbar, `Skills::new`
  nimmt eine mit `;` getrennte Liste (nicht `:` — das zerrisse `D:/pfad`); bei Namensgleichheit
  gewinnt das zuerst genannte Verzeichnis. Der `;`-String statt eines `Vec<String>` ist eine
  bewusste Entscheidung gegen Rippeln: `AppConfig::skills`, das TUI und die Profil-JSON tragen
  alle EINEN String, und ein typisierter Slice hätte vier Crates angefasst, ohne dass ein
  Nutzer davon etwas merkt.
- **Zusätzliche Arbeitsverzeichnisse** (`--add-dir DIR`, wiederholbar; im REPL und TUI zur
  Laufzeit `/add-dir <pfad>`, ohne Pfad die Liste; kein Python-Pendant). Ein Workspace war
  genau ein Ordner; wer an zwei nebeneinanderliegenden Projekten arbeitete, musste `-w` über
  den gemeinsamen Elternordner aufziehen. `CodingTools::add_dir` nimmt weitere Wurzeln auf,
  in denen gelesen UND geschrieben werden darf (das Gegenstück zu den nur-lesbaren
  `read_roots` unten). Die Liste ist ein geteiltes `RwLock` statt eines Builder-Werts, weil
  sie zur Laufzeit wächst: `register()` klont das Struct in jede Tool-Closure, Sub-Agenten
  und Schwarm-Mitglieder halten eigene Klone — eine Freigabe mitten in der Sitzung muss bei
  allen ankommen. `safe()` prüft den AUFGELÖSTEN Pfad gegen Workspace und Zusatzverzeichnisse
  (derselbe `unter_wurzel`-Test samt Symlink-Prüfung); neben dem absoluten Pfad gilt deshalb
  auch `../nachbar/x`. Relative Pfade lösen weiter nur gegen den Workspace auf, und
  `run_shell` startet weiter dort. Aufgenommen wird nur ein existierendes Verzeichnis
  (kanonisiert) — ein leerer Pfad, der Präfix von allem wäre, kann so nicht in die Liste
  geraten. Freigeben kann nur der Mensch (Flag oder Slash-Befehl), ein Tool dafür gibt es
  nicht. Das Modell erfährt die Verzeichnisse über die `TurnContext`-Naht vor jedem Auftrag
  (`add_dir_turn_context`), nicht über den System-Prompt: der bleibt byte-gleich, und nach
  `/reset` oder einer Verdichtung ist die Ansage nicht verloren. Die Liste gilt für den
  laufenden Prozess — dauerhaft wird sie über einen benannten Workspace (nächster Punkt).
  `agentkit work` und `--patch` kennen sie nicht.
- **Benannte Workspaces** (`src/workspaces.rs`; `/workspace save NAME`, `--ws NAME`,
  `agentkit workspace [list | show NAME | delete NAME]`; kein Python-Pendant). Ein Workspace
  aus mehreren Ordnern müsste sonst bei jedem Start neu zusammengetippt werden. Gespeichert
  wird eine Datei je Name, `<config_dir>/workspaces/<name>.json`, mit `name`, `root` (der
  Ordner, den sonst `-w` nennt) und `dirs` (die Zusatzverzeichnisse) — benutzerweit statt
  im Projekt, weil der Name von überall aus gelten soll und ein Workspace mehrere Projekte
  umfasst. `--ws NAME` setzt beim Start den Ordner und stellt die gespeicherten Verzeichnisse
  vor die per `--add-dir` genannten; fehlt der Name oder der Ordner, bricht der Start ab
  (Exit 3), statt in einem leeren Ordner zu arbeiten. Eine Sitzung mit Namen (geladen oder
  per `/workspace save`) führt ihren Eintrag bei jedem `/add-dir` nach; ein `--add-dir`
  beim Start gilt dagegen nur für diesen Lauf. Geladen wird nur beim Start, nicht mitten in
  der Sitzung: am Workspace-Ordner hängen Sandbox, Sitzungsdatei und Gedächtnis. Der Name
  wird zum Dateinamen und darf deshalb nur aus Buchstaben, Ziffern, `-`, `_` und `.`
  bestehen. Die Logik von `/add-dir` und `/workspace` liegt in diesem Modul und liefert
  Zeilen zurück — REPL und TUI zeigen sie nur an.
- **Nur-lesbare Zusatz-Wurzeln der Sandbox** (`--allow-read DIR`, wiederholbar; kein
  Python-Pendant). Bisher prüfte `CodingTools::safe()` jeden Pfad gegen GENAU EINE Wurzel und
  wurde von lesenden wie schreibenden Werkzeugen benutzt. Skills aus fremden Sammlungen
  verweisen aber auf eigene Referenzdateien außerhalb des Workspace, und der einzige Ausweg
  war, `-w` über einen gemeinsamen Elternordner aufzuziehen — was die **Schreib**-Sandbox
  unnötig weit macht. Jetzt trennt `safe_read()` (Workspace + `read_roots`) von `safe_write()`
  (nur Workspace); `in_read_root()` ist der EINE Ort, der über die Zugehörigkeit entscheidet,
  und `unter_wurzel()` wendet die Einzel-Wurzel-Prüfung inklusive `real_ancestor`-Symlink-Test
  auf Workspace und Lese-Wurzeln identisch an. Es entscheidet der REALE Ort: Symlinks, die
  im Workspace bleiben, funktionieren; ein Link nach außen wird abgelehnt (auch ein
  hängender, an dessen Ziel `write_file` sonst eine Datei anlegte), und die Meldung nennt,
  wohin er führt. Ein Workspace, der selbst über einen Link erreichbar ist (`/tmp` unter
  macOS), nimmt absolute Pfade in beiden Schreibweisen an. Relative Pfade lösen weiterhin ausschließlich
  gegen den Workspace auf (sonst wäre `read_file("README.md")` mehrdeutig). Das ist keine neue
  Angriffsfläche: `run_shell` war nie pfadbeschränkt, eine Shell konnte diese Dateien immer
  schon lesen.
  Ein **leerer** Wurzel-Eintrag wird verworfen, und zwar in `with_read_roots` statt nur beim
  Aufrufer: ein `Path` ohne Komponenten ist Präfix von *jedem* Pfad, ein einziger leerer
  Eintrag (Tippfehler in einer Profil-Datei, leere Variable in einem Skript) hätte die
  Lese-Sandbox vollständig aufgehoben. Aus demselben Grund fällt ein leerer **Workspace** auf
  `"."` zurück — `agentkit -w` ohne Wert am Zeilenende machte sonst das ganze Dateisystem
  les- und schreibbar.
  Nebenbei behoben: `safe()` lehnte unter Windows **absolute** Pfade *innerhalb* des Workspace
  ab, weil `canonicalize()` dem Workspace-Feld das Präfix `\\?\` gab, ein vom Modell
  übergebenes `D:\…` es aber nie trägt — `starts_with` schlug fehl. `kanonisch()` entfernt das
  Präfix an allen drei Stellen.
- **`run_shell`-Timeout nennt seinen Ausweg.** Die Meldung war `ERROR: Timeout nach 120s.` —
  eine Sackgasse, das Modell probierte dasselbe Kommando erneut. Sie nennt jetzt
  `--shell-timeout`, dieselbe Linie wie der MCP-Kaltstart-Hinweis.
- **MCP-Fristen per Umgebung.** `AGENTKIT_MCP_HANDSHAKE_TIMEOUT` und
  `AGENTKIT_MCP_CALL_TIMEOUT` (Sekunden) überschreiben die Defaults 15 s / 120 s. Anlass: ein
  per `uv run`/`npx -y` gestarteter Server löst beim ERSTEN Start Abhängigkeiten auf und
  überschreitet die 15 s deutlich — er erschien dann als „nicht verbunden", obwohl warm
  derselbe Server in Sekundenbruchteilen antwortet. Die Timeout-Meldung des Handshakes nennt
  seitdem den Kaltstart und den Ausweg.
- **Benutzer-Config `~/.agentkit/config.json`** (`src/config.rs`, kein Python-Pendant).
  Die Rust-Variante wird als Executable *installiert* und läuft damit außerhalb des
  Projektverzeichnisses, wo keine `.env` liegt. Kein zweites Config-System: die Datei
  wird auf dieselben `AZURE_OPENAI_*`-Variablen abgebildet und setzt nur, was noch nicht
  gesetzt ist — echte Umgebung > `.env` > User-Config. Die `.env` gehört zum Projekt und
  darf deshalb zweierlei nicht (`load_dotenv` in `src/app.rs`): Variablen setzen, die
  Programme starten oder Rückfragen abschalten (`AGENTKIT_HOOKS`/`_ALLOW`/`_HOME`, `PATH`,
  `LD_PRELOAD`, `BASH_ENV`, … — sie werden mit Hinweis übersprungen), und dem Key aus der
  User-Config einen fremden Endpunkt unterschieben (kommt nur der Endpunkt aus der `.env`,
  wird der Config-Key nur für `localhost` ergänzt).
- **Scriptbare interaktive Session (`--repl`) + mehrzeilige TUI-Eingabe.** Über die Python-Vorlage
  hinaus erzwingt `--repl` die interaktive Session auch bei gepiptem stdin (Kommandos und
  Folge-Antworten von stdin — für Automatisierung/Tests), und das TUI-Eingabefeld ist mehrzeilig
  (Alt-Enter fügt eine Zeile ein). **Human-in-the-Loop braucht kein Sonderwerkzeug:** der Agent
  stellt eine Rückfrage, indem er seinen Zug beendet; die nächste Eingabe beantwortet sie, und er
  macht mit vollem Gesprächsverlauf weiter. Motiviert vom interaktiven Accounts-Payable-Orchestrator
  (siehe unten).
- **Echte Ausführungs-Strategie `plan_execute`** (`src/strategy.rs`, kein Python-Pendant —
  dort ist Strategie NUR ein System-Prompt-Preamble). `-s plan_execute` (CLI, REPL, TUI;
  im Profil `"strategy": "plan_execute"` + optionales `strategy_params`-Objekt) schaltet
  einen Treiber UM die unveränderte Loop-Primitive: eine Plan-Phase (JSON-Schrittliste,
  bei Parse-Fehler stiller Rückfall auf den Direktlauf), je Schritt ein voller
  ReAct-Durchlauf mit eigenem Schritt-Budget, optional eine kurze Erledigt-Prüfung mit
  begrenzter Nacharbeit (`reflect`, `max_rework_per_step`) — und begrenzter **Umplanung**
  (`max_replans`, Default 2): erweist sich der Plan als nicht zielführend, ersetzt der
  Treiber die VERBLEIBENDEN Schritte. Zwei Auslöser: die Erledigt-Prüfung darf statt
  ERLEDIGT/NACHARBEIT mit „NEUER PLAN: [JSON-Array]" antworten (kostenlos — die Prüfung
  läuft ohnehin), und nach einem gescheiterten Schritt (Budget erschöpft/keine Antwort)
  fragt der Treiber einmal nach („WEITER" oder neues JSON-Array der Restschritte).
  Entspricht pytaskforce'
  `plan_and_react`; `Agent::drive` und die bisherigen Strategien bleiben unangetastet,
  die Phasen erscheinen als `source`-getaggte Event-Ströme („plan", „schritt 1/3",
  „umplanung 1/2", …) und der Treiber schließt mit genau einem FINAL/DONE ohne `source` ab.
  Mit `--verify` gelten dessen Einwürfe nur in den Schritt- und Nacharbeits-Läufen; Plan,
  Reflexion, Umplanung und Abschluss antworten nur und laufen ohne sie — sonst zählte ein
  „NACHARBEIT" nach dem Einwurf still als ERLEDIGT.
- **Modus PLAN** (`RunStrategy::Planning` in `src/strategy.rs`, `SessionMode` in
  `src/app.rs`, kein Python-Pendant). Neben den Freigabe-Modi *nachfragen* und *auto*
  gibt es einen dritten Sitzungs-Modus: TUI `Shift-Tab` (reihum nachfragen → auto →
  plan), REPL `/mode plan`, beim Start `--mode plan`. Ein Auftrag erzeugt dann NUR einen
  Plan (Ansatz, Constraints, Regeln, Schritte mit `depends_on`, als DAG validiert). Das ist
  eine harte Grenze, keine Prompt-Bitte: der Planungslauf sieht ausschließlich Tools,
  die sich als `ToolEffect::ReadOnly` deklarieren — undeklarierte (auch `task`) und
  `update_plan` fallen für diesen Lauf weg. Danach fragt das Frontend: `1` umsetzen mit
  Freigabe *auto*, `2` umsetzen mit *nachfragen*, `3` verwerfen, jeder andere Text ist eine
  Anmerkung und führt zu einem überarbeiteten Plan. Die Umsetzung ist ein normaler Auftrag
  mit dem Plan als Vorgabe (`planned_task`) und der Sitzungs-Strategie — mit
  `-s plan_execute` also der Phasen-Treiber. Über ACP (Zed & Co.) meldet `session/new` die
  drei Modi, der Editor wechselt per `session/set_mode`; nach dem Plan fragt der Agent per
  `session/request_permission` (ja automatisch / ja mit Rückfrage / weiter planen) und
  meldet seinen eigenen Moduswechsel als `current_mode_update`. Headless (`--mode plan -p …`) gibt nur den
  Plan auf stdout aus. agentkit-work lehnt `--strategy planning` ab: ein Item, das nur
  plant, käme nie zu `work_submit`.
- **Zeileneditor im REPL** (`repl_editor` in `agentkit_app`, kein Python-Pendant). Das
  nackte `read_line` wich `rustyline` — Pfeiltasten-History über Sitzungen hinweg,
  Ctrl-R-Rückwärtssuche, readline-Kürzel, mehrzeilige Eingaben (`\` am Zeilenende oder
  offener ```-Block) und Tab-Vervollständigung für Slash-Befehle und `@pfad`.
  **Die einzige neue Fremdabhängigkeit** dieses Bereichs, und bewusst eine etablierte
  statt einer eigenen Terminal-Zustandsmaschine (CODING_GUIDELINES: Einfachheit zuerst).
  `rustyline 17`, nicht 18: 18 verlangt `unicode-width ^0.2.2`, ratatui 0.29 pinnt
  `=0.2.0` — die Versionen ließen sich nicht gemeinsam auflösen. Sie liegt in
  `agentkit_app`, nicht im Kern: der Agent-Kern bleibt frei davon. Zwei Rückfallpfade
  bleiben ohne Editor: gepipter stdin (`--repl` im Skript) liest wie bisher zeilenweise
  bis EOF, damit Pipes sich nicht anders verhalten, und ein Terminal, auf dem der Editor
  nicht startet, fällt mit Hinweis auf denselben schlichten Loop zurück.
- **Freigabe-Regeln pro Programm + `/permissions`** (`Permissions` in `agentkit_app`,
  kein Python-Pendant). Die `run_shell`-Rückfrage kennt neben Ja/Nein zwei Wege, sie sich
  zu merken: **„[i]mmer erlauben"**, das sich das *erste Wort* des Befehls nur für die
  Sitzung merkt (`cargo test` freigeben heißt danach jedes `cargo`, nicht jedes beliebige
  Kommando; bei einer Kette wie `cd x && cargo build` die ersten Wörter aller
  Teilbefehle), und **„[d]auerhaft"**, das denselben Eintrag zusätzlich in die `allow`-Liste
  von `~/.agentkit/config.json` schreibt (Env-Quelle niedrigster Priorität, abgebildet auf
  `AGENTKIT_ALLOW` — siehe `agent_framework_rs/src/config.rs`) und damit auch in
  künftigen Läufen inkl. One-Shot **und im TUI** gilt; `/permissions allow <programm>`
  geht denselben Weg.
  `/permissions` zeigt die Liste (Config-Einträge markiert mit `(config.json)`),
  `/permissions reset` leert die Sitzungsregeln. Eine Kette (`;`, `&&`, `||`, `|`, `&`,
  Zeilenumbruch) läuft nur ohne Rückfrage, wenn JEDER Teilbefehl freigegeben ist, ein
  Befehl mit Kommandosubstitution (`$(…)`, Backticks, `<(…)`) nie — dieselbe Regel gilt
  in CLI, TUI, ACP und für `allow:` in `AGENTS.md` (`ohne_rueckfrage` in `src/config.rs`).
  Ein Blanko-„alles erlauben" bleibt
  bewusst `-y` und wird nicht gespeichert — nur einzelne Programme lassen sich dauerhaft
  freigeben, das bleibt eine bewusste, sichtbare Entscheidung (Startmeldung `» Ohne
  Rückfrage (config.json): …`). Bewusst auch **kein** Gate für `write_file`/`edit_file`:
  dafür gibt es `/undo` (siehe unten), und eine Rückfrage pro Datei machte den Agenten
  unbenutzbar.
- **Benachrichtigung `--notify`** (`notify` in `agentkit_app`, kein Python-Pendant).
  Terminal-Bell plus OSC-9-Notification, wenn ein Lauf länger als `NOTIFY_AFTER` (20 s)
  gedauert hat oder eine Shell-Freigabe wartet — man kann weg-alt-tabben, ohne den
  Abschluss zu verpassen. Nur auf Verlangen (`--notify`) und nur interaktiv: eine
  Pipeline, die still laufen soll, darf nicht piepen. Über Escape-Sequenzen statt
  `notify-send`/PowerShell-Toast, damit kein Prozess gestartet wird und der statische
  musl-Build nichts dazulernt.
- **Kontext-Anzeige `/context` in TUI und REPL** (`context_report` in `src/app.rs`, kein
  Python-Pendant; die Zahlenformatierung `fmt_tokens`/`fmt_pct`/`fmt_count` teilen sich
  beide Frontends). Zeigt die Belegung des Agenten-Kontexts als farbiges Raster plus
  Legende mit Tokens pro Abschnitt (System-Prompt, Tool-Schemas, Nachrichten, …),
  Summe und Budget — ohne ctxman per Zeichen/4-Heuristik über die `ShortTermMemory`,
  mit ctxman aus den Segment-Statistiken (`ManagedContext::segment_stats`).
- **Verlaufs-Export `/export`** (`ShortTermMemory::to_markdown` in `src/app.rs`, kein
  Python-Pendant). Rendert den Gesprächsverlauf als Markdown — nach Zügen gegliedert,
  mit Tool-Aufrufen, Argumenten und Ergebnissen. Zwei Detailgrade an EINEM Schalter:
  ins Terminal gekürzt (ein Coding-Verlauf mit Ergebnissen bis `TRUNCATE_LIMIT` würde
  die Sitzung sonst zuschütten), in eine Datei vollständig. `--json` schreibt
  stattdessen die rohen Messages — dasselbe Format wie `--session`, also wieder ladbar.
- **Verlaufs-Branching `/rewind` und `/fork`** (`ShortTermMemory::turn_starts`/`rewind_to_turn`,
  kein Python-Pendant). Der Verlauf lässt sich vor einem Zug abschneiden, um ihn anders zu
  stellen; `/fork` sichert den bisherigen Ast vorher als Session-Datei, die sich mit
  `--session` weiterführen lässt. Grenze: mit `--ctx` rendert ctxman die Provider-Messages
  und `memory` ist nur ein Spiegel — ein Schnitt im Spiegel nähme dem Modell nichts weg,
  träfe aber eine mitlaufende `--session`-Datei dauerhaft vom echten Kontext ab. ctxman hat
  keine Kürzungs-API, also **lehnt** `Agent::rewind_to_turn` mit verwaltetem Kontext ab
  (`RewindOutcome::ContextManaged`), statt halb auszuführen; der Weg dorthin ist ein
  Neustart mit gekürzter Session-Datei und frischem `--ctx` (siehe nächster Punkt).
- **`Agent::adopt_history` spielt `--session` in einen frischen `--ctx` ein.** Ein
  geladener Verlauf landete bisher nur im Spiegel `memory`; der verwaltete Kontext blieb
  leer, das Modell begann trotz angezeigter Historie bei null. `adopt_history` setzt
  beides zusammen (`ManagedContext::replay`) — bei einer aus dem Snapshot fortgesetzten
  Session ein No-op, dort steht der Verlauf schon.
- **Checkpoints + `/undo` für Datei-Änderungen** (`CodingTools::checkpoint`/`undo_last`,
  kein Python-Pendant). Vor jedem `write_file`/`edit_file` wird der vorherige Zustand
  gesichert; `/undo` nimmt die jüngste Änderung zurück (neu angelegte Datei löschen,
  überschriebene wiederherstellen), `/undo alle` alles. Der Stapel ist Arc-geteilt, also
  schreiben auch Sub-Agenten und Schwarm-Mitglieder hinein. Ein abgelehnter `edit_file`
  erzeugt KEINEN Eintrag. Grenzen werden benannt statt geraten: Dateien über 1 MB und
  binäre bleiben ungesichert, und `run_shell`-Folgen kennt agentkit nicht. Der Stapel ist
  gedeckelt (50 Einträge bzw. 8 MB, `trim_checkpoints`) — ohne Deckel hielte ein
  `-p`-Lauf mit hunderten Schreibvorgängen jede Vorversion bis zum Prozessende im
  Speicher, und dort ist `/undo` nicht einmal erreichbar. Zwei Grenzen, weil die
  Byte-Grenze wenige große Dateien bremst und die Anzahl-Grenze viele kleine.
- **Projekt-Instruktionen `AGENTS.md` + `/init`** (`load_project_instructions` in
  `src/app.rs`, kein Python-Pendant). Gelesen werden `~/.agentkit/AGENTS.md` (deine
  persönlichen) und `<workspace>/AGENTS.md` (die des Projekts, gewinnt); der Prosateil
  wird beim Bau an den eingebauten System-Prompt angehängt — nicht bei eigenem
  `--system`, der ersetzt den ganzen Prompt. Ein optionales Frontmatter (`deny:`/
  `allow:`) trägt **Leitplanken** für `run_shell`: `deny` lehnt ab, bevor der
  Freigabe-Callback gefragt wird, und ist deshalb auch mit `-y` nicht aufhebbar; als
  Werkzeug-Erklärung überleben die Leitplanken — anders als der Prosateil — ein eigenes
  `--system`. Der Standardname löst das frühere `AGENTKIT.md` ab: der Schutz der
  Benchmark-Vergleichbarkeit hängt jetzt an `--no-project-instructions` (schaltet beides
  ab, die Pipeline setzt es fest) statt an einem exotischen Dateinamen. Der Start meldet
  jede geladene Datei mit Pfad und Größe. **`allow:` aus der PROJEKT-Datei gilt erst nach
  einmaliger Bestätigung** (`klaere_projekt_allow`, #57): die Datei gehört zum Repository —
  auch zu einem fremden — und der Agent kann sie selbst schreiben. REPL, One-shot am
  Terminal und TUI fragen beim Start einmal (Einträge vollständig, Vorgabe Nein); Ja wird
  in `~/.agentkit/config.json` gemerkt (`trusted_project_allow`: Hash aus Workspace-Pfad
  und Liste), eine geänderte Liste oder ein anderer Workspace fragt neu. Ein Nein gilt nur
  für den Lauf. Ohne Terminal (`-p`, Pipe, `--format`, `--each`, `mcp-serve`, `acp`, Work)
  wird nie gefragt: eine unbestätigte Liste bleibt unwirksam, eine Hinweiszeile nennt den
  Weg (`agentkit config trust` im Projektverzeichnis). `deny:` gilt immer sofort, `allow:`
  aus `~/.agentkit/AGENTS.md` wie bisher, und mit `-y` entfällt die Frage.
- **Markdown-Auszeichnung im REPL** (`MarkdownStream` in `agentkit_app`, kein
  Python-Pendant). Zeichnet die gestreamte Antwort zeilenweise aus (Überschriften,
  Aufzählungen, `**fett**`, `` `code` ``, Code-Fences) — zeilenweise, weil ein
  Block-Renderer erst am Ende rendern könnte und der gestreamte Rohtext dann doppelt
  dastünde. Greift NUR im interaktiven REPL: bei gepipter Ausgabe und mit `-p` bleibt
  stdout unverfälscht (Unix-Filter-Kontrakt). Der TUI-Renderer bleibt getrennt — er
  erzeugt ratatui-`Line`s und ließe sich nur über eine Abstraktion teilen, die zwei
  sehr verschiedene Ausgabemodelle über einen Kamm schert.
- **Modellwahl `--model NAME` + `/model`** (kein Python-Pendant). Überschreibt das Modell,
  ohne an der Umgebung zu drehen — abgebildet auf dieselbe Variable, aus der agentkit
  ohnehin liest (`OPENAI_MODEL`/`AZURE_OPENAI_DEPLOYMENT`), also kein zweites Konzept.
  `/model` zeigt nur an: das LLM steckt beim Bau auch in den Sub-Agenten (`task`) und im
  Schwarm-Werkzeug, ein Wechsel zur Laufzeit träfe nur den Haupt-Agenten und ließe die
  übrigen still auf dem alten Modell laufen. `/model <name>` nennt daher den
  Neustart-Befehl statt eine Halbwahrheit herzustellen.
- **Type-ahead im TUI** (`App::queue`/`start_task` in `src/tui.rs`, kein Python-Pendant).
  Während ein Auftrag läuft, blockiert die Eingabe nicht mehr: `Enter` merkt vor, und die
  vorgemerkten Aufträge laufen der Reihe nach, sobald der Agent zurück ist. Die
  Warteschlange geht bewusst über `start_task(String)` statt über das Eingabefeld —
  sonst überschriebe ein nachrückender Auftrag gerade getippten Text. Nur im TUI: der
  REPL blockiert während des Laufs bauartbedingt (er rendert dort den Event-Strom).
- **Aufgeräumtes TUI: kompakter Verlauf, Agenten-Feld, Start im Hintergrund** (`App::apply_event`,
  `App::draw_aktiv`, `Startup` in `src/tui.rs`, kein Python-Pendant). Das TUI führt zwei
  Verläufe parallel: den kompakten (Vorgabe — Gespräch, je Tool-Aufruf des Haupt-Agenten EINE
  Zeile, die beim Ergebnis über die `call_id` fortgeschrieben wird; der Plan nur bei
  inhaltlicher Änderung ganz, sonst als Fortschrittszeile) und den ausführlichen (alles wie
  bisher). `Ctrl-O`/`F3` schaltet um — auch rückwirkend, deshalb zwei Puffer statt eines
  Schalters für Künftiges. Events fremder Agenten (nicht-leere `source`: Sub-Agenten,
  Schwarm-Mitglieder, `plan_execute`-Phasen) stehen kompakt nicht im Verlauf, sondern als je
  eine Zeile im Agenten-Feld über der Eingabe (Schritt, Tool-Zahl, Dauer, letzte Aktion); ihr
  `done` hinterlässt eine Abschlusszeile, die Titelzeile nennt „Agent + N Sub-Agenten".
  Agent und MCP-Hub entstehen in einem eigenen Thread, die Oberfläche steht sofort; Eingaben
  bis dahin laufen über dieselbe Warteschlange wie das Type-ahead. Zusätzlich verbindet
  `McpHub::connect` die Server parallel (ein Thread je Handshake) — das gilt für alle
  Frontends: die Startzeit bestimmt der langsamste Server, nicht die Summe. Und AltGr-Zeichen
  (`\`, `@`, `{` — die Windows-Konsole meldet AltGr als Ctrl+Alt) gelten als Text, getippt wie
  eingefügt (`ist_altgr_zeichen`); vorher verschluckte sie der Ctrl-Kürzel-Zweig.
- **Kompaktierung auf Kommando `/compact`** (`Agent::compact_now`, kein Python-Pendant).
  Verdichtet sofort, statt zu warten, bis das Token-Budget (bzw. mit `--ctx` die
  Watermark) erreicht ist — nützlich vor einem großen Schritt. Ein Hinweis lenkt die
  Zusammenfassung (`ShortTermMemory::compact_with_hint`); mit `--ctx` läuft stattdessen
  ctxmans voller GC, der keinen Hinweis-Eingang hat — der Befehl sagt das dann auch.
  Die Anzeige „vorher → nachher" kommt aus `context_report`, nicht aus `memory`, sonst
  meldete sie mit ctxman stur unveränderte Zahlen.
- **Automatisch verwaltete Sitzungen** (`src/sessions.rs`, kein Python-Pendant). Eine
  REPL-Sitzung am Terminal schreibt ihren Verlauf ohne Flag nach
  `<config_dir>/sessions/<projekt>/<UTC-Zeitstempel>.json`; `--continue` setzt die jüngste
  fort, `--resume` lässt aus der Liste wählen, `/sessions` zeigt sie. Bewusst **keine**
  Index-Datei: Titel, Zug-Zahl und Alter kommen aus der Datei selbst (mtime + erste
  User-Nachricht) — ein zweiter, parallel zu pflegender Index wäre nur eine weitere
  Fehlerquelle. Das Format ist dasselbe wie `--session` und `/export --json`, die Dateien
  sind also austauschbar. **Skripte legen nichts an**: weder `-p` noch `--repl` mit
  gepiptem stdin (die Benchmark-Pipeline ruft agentkit tausendfach auf). Aufgeräumt wird
  nicht automatisch — der Verlauf soll nicht hinter dem Rücken verschwinden.
- **Read-only git-Tools** (`git_status`, `git_diff`, `git_log`, `git_show` in `src/coding.rs`,
  kein Python-Pendant). Workspace-gebunden, ohne Approval (nur lesende Subkommandos,
  strukturierte Argumente statt Shell-Strings — Refs/Pfade, die wie Optionen aussehen,
  werden abgelehnt). Weil sie ohne Rückfrage laufen, startet git dabei nichts aus der
  Repo-Konfiguration (`core.fsmonitor`, Pager, externe Diff-/Textconv-Treiber sind
  abgeschaltet), und `write_file`/`edit_file` schreiben nicht unter `.git/`. Teil von `READ_ONLY_TOOLS`, damit die `reviewer`-/`explorer`-Rollen
  Diffs und Historie sehen, ohne `run_shell`-Freigaben zu brauchen. Grundlage des
  PR-Review-Beispiels (`examples/pr_review`).
- **`edit_file`/`read_file` über das Original hinaus** (`src/coding.rs`). `edit_file`
  findet ein `old` mit `\n` auch in einer CRLF-Datei, setzt `new` mit den Zeilenenden der
  Datei ein und schreibt atomar (Temp-Datei + `rename`, Rechte bleiben). `read_file` kennt
  `von`/`bis` (nur `bis` = die ersten Zeilen); der Hinweis „bereits gelesen" gilt je
  gelesenem Bereich.
- **Eingeschränkte Rollen bekommen nur lesende MCP-Tools** (`src/roles.rs`). Eine Rolle
  mit Tool-Auswahl (`explorer`, `reviewer`, `tester`, Custom-Rollen mit `tools:`) erhält
  MCP-Tools nach der Regel von `--dry-run`: ausgeschlossen ist, was der Server als
  schreibend deklariert oder — ohne Deklaration — was `is_likely_destructive` am Namen
  erkennt. `general` und Rollen ohne `tools:` bekommen weiter alle. Ein unbekannter
  `subagent_type` ist ein weicher Fehler mit der Liste der gültigen Typen, kein stiller
  `general`. Frontmatter wird auch hinter einem UTF-8-BOM oder Leerzeilen erkannt.
- **Selbstverifikation `--verify`** (`AgentBuilder::verify_before_final`, kein
  Python-Pendant). Will das Modell nach `write_file`/`edit_file` abschließen, ohne
  danach einen `run_shell`-Check ausgeführt zu haben, injiziert der Loop einmalig
  `VERIFY_NUDGE` als User-Nachricht und läuft weiter, statt zu beenden. Motiviert
  von den Agent-Benchmarks (`../agent_benchmarks`): dominantes Fehlermuster dort
  waren unverifizierte „Fertig"-Meldungen. Default aus; interaktiv (TUI) immer aus.
- **Delegation als Default statt Selbst-Lesen** (kein Python-Pendant), auf zwei Ebenen:
  1. Der Coding-Prompt ist keine Konstante mehr, sondern `coding::coding_system(delegierend)`.
     Mit vorhandenem `task`-Tool ersetzt die delegierende Orientierungsregel die alte
     („verschaffe dir zuerst mit list_files/glob_files/grep/read_file einen Überblick"),
     statt sie nur zu ergänzen. Der Grund ist gemessen: mit beiden Absätzen im Prompt hat
     ein Modell die frühere, konkretere Anweisung befolgt — `list_files`, `glob_files`,
     `grep`, dann vier `read_file` auf einmal — und den Delegations-Hinweis weiter unten
     ignoriert. Ein „das gilt vorrangig" hinten schlägt eine Anweisung vorn nicht; der
     Widerspruch muss weg, nicht überstimmt werden.
  2. `DELEGATE_NUDGE` als Rückfalllinie: liest der Orchestrator in EINEM Lauf vier oder
     mehr Dateien selbst, wirft der Loop einmalig eine User-Nachricht ein, die auf einen
     `explorer`-Sub-Agenten verweist — dasselbe Muster wie `VERIFY_NUDGE`, weil
     Instruktionstreue modellabhängig ist, ein Mechanismus aber nicht. Nur aktiv, wenn die
     Registry ein `task`-Tool hat; Sub-Agenten und Schwarm-Mitglieder haben es nie und
     sehen den Einwurf deshalb auch nie.
  Der Zweck ist Kontext-Hygiene: was ein Sub-Agent liest, bleibt in SEINEM Kontext, und
  nur seine finale Antwort kommt zurück. Der Preis ist ehrlich zu nennen — in Summe mehr
  Tokens und mehr parallele Anfragen; auf einem knapp bemessenen Deployment steigt dadurch
  der Rate-Limit-Druck.
- **Hintergrund-Sub-Agenten** (`task` mit `run_in_background: true`, `src/background.rs`,
  kein Python-Pendant). Der Aufruf kehrt sofort mit einer ID zurück (`bg-1`, …), der
  Sub-Agent läuft in einem eigenen Thread, und sein Ergebnis landet in einer Mailbox des
  Orchestrator-Laufs (im `RunHandle`) — dasselbe Muster wie ein Actor im Schwarm
  (`agentkit_swarm`), aber ohne dessen Code: der Kern darf den Schwarm nicht kennen. Der
  Loop holt Ergebnisse an zwei Stellen ab: an jeder Schrittgrenze (dann kommen sie, während
  der Orchestrator noch anderes tut) und **blockierend**, wenn das Modell seinen Zug ohne
  Tool-Aufruf beendet, solange noch Hintergrund-Arbeit offen ist. „Warten" ist also kein
  eigenes Werkzeug, sondern einfach Zugende — das Modell entscheidet über Abhängigkeiten,
  der Harness hält den Lauf am Leben. Jedes Ergebnis kommt als User-Nachricht
  `[Hintergrund] Sub-Agent bg-N (rolle) ist fertig …` und setzt die Prüfpflicht von
  `--verify` neu (der Sub-Agent kann nach dem letzten Check geschrieben haben). Das Warten
  prüft den Stop-Knopf im 100-ms-Takt wie der Schwarm-Actor; ein panickender Sub-Agent
  meldet trotzdem ein Ergebnis, statt den Orchestrator ewig warten zu lassen. Die Mailbox
  gilt pro Lauf: ein Sub-Agent, der einen abgebrochenen Lauf überlebt, schreibt in die
  verworfene. Endet der Zug im letzten erlaubten Schritt, während noch etwas läuft, ist die
  Zwischenmeldung keine Antwort — der Lauf liefert `(max_steps erreicht)`. Und endet ein Lauf
  ohne seine Hintergrund-Agenten (Abbruch, Budget, Modellfehler), hält `drive` sie über
  einen eigenen Stop-Knopf der Mailbox an, damit sie nach dem gemeldeten Ende nicht weiter
  im geteilten Workspace schreiben. Ohne `run_in_background` ist `task` unverändert synchron. Jedes Einspielen
  erscheint zusätzlich als Ereignis `background_result` (`EventData::BackgroundResult`
  mit ID, Rolle und Ergebnis, unter der `source` des Orchestrators): CLI und TUI zeigen
  eine eigene Zeile, ACP meldet es als sofort fertigen Eintrag unter der Hintergrund-ID,
  und agentkit-viz führt es in der Zeitleiste und im rekonstruierten Kontext.
- **ctxman auch für HELFER, ohne Persistenz** (kein Python-Pendant). Mit `--ctx` bekommt
  nicht nur der Haupt-Agent einen verwalteten Kontext, sondern auch jeder Sub-Agent
  (`task`) und jedes Schwarm-Mitglied — über `ManagedContext::ephemeral`: Blobs im
  Speicher, kein Snapshot, keine Fact-Promotion. Ein Helfer lebt einen Auftrag lang und
  resumt nie; ein Snapshot wäre reine Schreiblast, und ein GEMEINSAMES `state_dir` wäre
  ein Korrektheitsfehler (parallele Helfer überschrieben sich `snapshot.json`). Was
  bleibt, ist das Einzige, was zählt: Watermark-GC und verlustfreie Externalisierung
  großer Tool-Ergebnisse — genau das, was die Anfragen klein hält. Motiviert davon, dass
  die Arbeit, die den Kontext aufbläht, überwiegend in den Helfern passiert: in einem
  gemessenen Lauf lasen drei Sub-Agenten 61 Dateien, während der Orchestrator drei las.
  Das Budget je Helfer ist ein Drittel von `--ctx-budget` (mindestens 8000).
- **Exponentieller Backoff bei Stream-Retries, aber `Retry-After` gewinnt.** Die 3 Retries
  beim Stream-Aufbau warten `retry_backoff_ms` (Default 500 ms, verdoppelt pro Versuch)
  statt sofort zu hämmern — gegen Rate-Limits (429) und kurze Netz-Aussetzer; der
  ureq-Pfad meldet dazu HTTP-Status, `Retry-After` und Body-Anfang statt nur "status code".
  Nennt der Provider ein `Retry-After`, bestimmt **dieses** die Wartezeit (`retry_after_ms`
  in `src/agent.rs`), gedeckelt auf 60 s; ein längeres Fenster bricht sofort mit dem Fehler
  ab, statt zwei weitere Anfragen gegen dasselbe Limit zu schicken. Ohne das war der Retry
  gegen echte Rate-Limits wirkungslos: Azure nennt typischerweise 30 s, der Backoff kam auf
  0 + 500 ms + 1 s und verbrannte alle drei Versuche im selben Fenster — der Lauf endete
  mit `"(keine Antwort)"`. Für den Schwarm (`../agentkit_swarm`) wiegt das doppelt: N
  Mitglieder teilen sich eine Deployment-Quota, und ein 429 im Mitglieds-Turn ist dort ein
  Fehler-Sentinel, nach dem das Mitglied verstummt. Die Wartezeit wird aus dem Fehlertext
  gelesen — ein String-Vertrag statt eines Fehler-Enums, das durch jede `Llm`-Implementierung
  müsste; `agent::tests::retry_after_wird_geparst` pinnt ihn. `retry_backoff_ms = 0` heißt
  weiterhin "gar nicht warten" (Tests). Der Stop-Knopf greift auch während des Wartens.
  Ein HTTP-4xx außer 408/429 (falscher Key, unbekanntes Modell, zu großer Kontext) wird
  gar nicht wiederholt — er wäre beim nächsten Versuch derselbe; erkannt am `HTTP <code>`
  am Anfang des Fehlertexts (`agent::wiederholbar`, derselbe String-Vertrag).
- **Harness für lange Läufe** (kein Python-Pendant). Mehrere Stellen, an denen ein langer
  Lauf vorher still Genauigkeit verlor oder komplett scheiterte:
  1. *Kompaktierung* (`ShortTermMemory::compact_keeping_task`). Eine frühere Notiz geht
     in den Digest und damit in die neue Zusammenfassung ein; vorher fiel sie bei der
     zweiten Kompaktierung ersatzlos weg. Scheitert der Zusammenfassungs-Call, bleibt
     der Verlauf unverändert; vorher ersetzte eine leere Notiz alles. Der Digest enthält
     die Tool-Aufrufe (Name + Argumente); vorher sah die Zusammenfassung nicht, welche
     Dateien geschrieben wurden. Der Auftrag des laufenden Laufs bleibt wörtlich
     erhalten. Die Zusammenfassung ist gegliedert (Ziel, geänderte Dateien,
     Erkenntnisse, Fehlversuche, Plan, Offenes) statt „3-5 Stichpunkte". Ausgelöst wird
     auch dann, wenn die vom Provider **gemessene** Prompt-Größe (abzüglich der
     Tool-Schemas) das Budget überschreitet — Zeichen/4 unterschätzt Code deutlich.
     Nach einem Versuch (auch einem gescheiterten) pausiert die automatische
     Kompaktierung, bis acht neue Nachrichten dazugekommen sind (`COMPACT_PAUSE`):
     liegen schon die behaltenen Nachrichten über dem Budget, lief sie sonst in jedem
     Schritt (gemessen 7 Zusammenfassungs-Calls in 9 Schritten, jetzt 2).
  2. *Kürzung an der Mitte* (`memory::kuerze_mitte`). Tool-Ergebnisse behalten Anfang
     und Ende. `run_shell` teilt den Platz zwischen STDOUT und STDERR auf, STDERR
     bekommt bis zur Hälfte; vorher schnitt es still bei 16000 Zeichen ab, und bei langer
     Ausgabe fiel STDERR mitsamt dem Fehler komplett weg.
  3. *Abgeschnittene Antworten* (`Chunk::truncated`, aus `finish_reason: "length"` bzw.
     `stop_reason: "max_tokens"`). Ein Tool-Aufruf mit unvollständigen Argumenten wird
     nicht ausgeführt, sondern bekommt `ABGESCHNITTENER_AUFRUF` als weiches Ergebnis;
     kaputtes JSON ohne Abschneiden bekommt den Parser-Fehler. Vorher wurde beides still
     zu `{}`. Eine abgeschnittene Antwort ohne Tool-Aufruf wird einmal nachgefordert
     (`ABGESCHNITTEN_NUDGE`).
  4. *Abriss mitten im Stream* wird bis zu zweimal wiederholt (`MAX_STREAM_ABRISSE`,
     gefahrlos, weil im Schritt noch kein Tool lief); vorher endete der Lauf sofort mit
     `"(keine Antwort)"`. Bereits gestreamte Text-Deltas erscheinen im Frontend dann
     doppelt — die Antwort im Verlauf nicht. Scheitert der Schritt endgültig, bleibt
     der zuletzt gestreamte Text als Assistant-Nachricht im Verlauf (die nächste
     Eingabe kann daran anknüpfen); das Ergebnis des Laufs bleibt `"(keine Antwort)"`.
     Der OpenAI-Pfad meldet dabei zwei Fälle als Fehler, die vorher als leeres Delta
     durchgingen: ein `{"error": …}`-Payload mitten im Stream (wird wie ein Abriss
     wiederholt) und `finish_reason: "content_filter"` (wird wie eine Ablehnung NICHT
     wiederholt; Meldung „Der Inhaltsfilter des Anbieters hat die Antwort
     abgeschnitten", Exit 2). `data:`-Zeilen gelten auch ohne Leerzeichen.
  5. *Wiederholungs-Einwurf* (`WIEDERHOLUNG_NUDGE`): liefert derselbe Tool-Aufruf
     (gleicher Name, gleiche Argumente) dreimal in Folge dasselbe Ergebnis, kommt
     einmal je Lauf ein Einwurf. Ein geändertes Ergebnis setzt die Zählung zurück.
- **Session-Persistenz (`--session FILE`).** Der Verlauf wird nach jedem Auftrag als JSON
  gespeichert und beim Start geladen — Resume über Prozessgrenzen für One-shot-Ketten,
  REPL und TUI (`ShortTermMemory::save`/`load`). Geschrieben wird atomar (Temp-Datei +
  `rename`, ebenso die `allow`-Liste der `config.json`), damit ein Abbruch mitten im
  Schreiben keine halbe Datei hinterlässt; `load` schließt offene Tool-Aufrufe einer
  früher kaputt gespeicherten Sitzung.
- **TUI-Parität: Sitzung und Slash-Befehle** (`TuiConfig::session`, `App::handle_slash` in
  `src/tui.rs`, kein Python-Pendant). Das TUI lädt und speichert `--session` wie der REPL
  und beantwortet `/help`, `/context`, `/tools`, `/reset`, `/compact` und `/export` selbst.
  Bewusst **nicht** der volle REPL-Satz: `/clear` und `/exit` haben Tasten, MCP hat das
  F2-Panel, und `/rewind`/`/fork` bräuchten eine Auswahlliste, die das Transcript-Fenster
  nicht hergibt. Unbekannte `/`-Eingaben gehen als normale Frage ans Modell statt abgewiesen
  zu werden — im TUI ist ein `/` am Zeilenanfang oft einfach ein Pfad. Geladen wird über
  `Agent::adopt_history`, damit ein frisches `--ctx` den Verlauf ebenfalls bekommt.
  `--continue`/`--resume` wirken hier ebenfalls: die Auswahl läuft **vor**
  `ratatui::init()`, solange das Terminal noch normal ist. Nur die *gewählte* Datei —
  anders als der REPL legt das TUI ohne Flag keine Sitzung an (`chosen_session` statt
  `resolve_session`), sonst schriebe ein kurzer Blick ins TUI stillschweigend Dateien.
- **agentkit als MCP-Server und als ACP-Agent** (`src/mcp_server.rs`, `src/acp.rs`,
  `agentkit mcp-serve` / `agentkit acp`, kein Python-Pendant). Zwei weitere Frontends —
  wie CLI und TUI nur Konsumenten des Event-Busses, der Agent-Kern bleibt unberührt.
  `mcp_server` stellt eine beliebige `ToolRegistry` über stdio bereit (synchron, eine
  Anfrage nach der anderen); welche Tools, entscheidet die Executable (Standard: ein
  `agentkit`-Tool, das einen Auftrag an einen frischen Coding-Agenten delegiert). `acp`
  braucht dagegen Threads: während ein Auftrag läuft, muss es weiter auf `session/cancel`
  und die Antwort zur eigenen Freigabe-Frage (`session/request_permission`) hören — ein
  Lese-Thread verteilt die Zeilen, jeder Auftrag läuft auf einem eigenen Thread wie der
  Worker der CLI. Keine async-Runtime. `acp::serve` nimmt beliebige `BufRead`/`Write`;
  darauf setzt die Executable ACP über TCP (`agentkit acp --listen` / `--connect`,
  `agentkit_app/src/acp_netz.rs`): eine Vorspann-Zeile mit Token vor dem Protokoll, danach
  dieselben Nachrichten wie über stdio. Der Server ruft `acp::serve_netz`, das laufende
  Aufträge am Ende der Verbindung abbricht — über stdio endet mit dem Editor auch der
  Agent, ein Server liefe sonst unbeaufsichtigt weiter. Kein TLS (keine neue Abhängigkeit);
  gedacht für Loopback, Tunnel oder VPN.
- **Deklarierte Tool-Wirkung statt Namens-Heuristik** (`ToolEffect`, `ToolRegistry::declare`
  in `src/tools.rs`, kein Python-Pendant). Ein Tool sagt selbst, ob es nur liest oder
  schreibt; `dry_run_blocking` folgt der Deklaration und rät nur bei Tools ohne Angabe am
  Namen (`is_likely_destructive`). Die Coding-Tools leiten ihre Angabe aus `READ_ONLY_TOOLS`
  ab (eine Liste bleibt die Wahrheit), MCP-Tools aus den `readOnlyHint`/`destructiveHint`-
  Annotationen ihres Servers. Behoben ist damit der dokumentierte Fehltreffer `update_plan`,
  den `--dry-run` wegen „update" blockierte.
- **Hooks vor und nach Coding-Tools** (`src/hooks.rs`, `--hooks FILE`, kein Python-Pendant).
  Shell-Kommandos aus `~/.agentkit/hooks.json` bzw. `--hooks`; Exit 2 blockiert (vorher) oder
  meldet zurück (nachher), dieselben Codes wie Claude Code. Sie hängen an `CodingTools` wie
  die Leitplanken — eine Regel nur für den Orchestrator wäre keine — und erreichen so jeden
  Klon (Sub-Agenten, Schwarm, Work). Übergeben wird über `AGENTKIT_HOOKS` statt über
  `CodingAgentConfig`: jeder Bauweg liest dieselbe Umgebung, genau wie bei `AGENTKIT_ALLOW`.
  Nie aus dem Repository geladen: ein Hook führt Code ohne Rückfrage aus.
- **Native Anthropic-Anbindung** (`AnthropicLlm` in `src/llm/anthropic.rs`, `--provider
  anthropic`, kein Python-Pendant). Kein OpenAI-Shim, sondern die Messages API direkt —
  weil erst sie drei Dinge liefert, auf die ein Agent-Loop angewiesen ist: Prompt-Caching
  (drei `cache_control`-Marken: letztes Tool, System, letzter Block — der Loop schickt bei
  jedem Schritt den ganzen Verlauf, der wiederholte Teil kostet dann den Cache-Preis),
  unverändert zurückgespielte Denk-Blöcke (sonst verliert das Modell seine Überlegung
  mitten in einer Tool-Runde) und den gemessenen Verbrauch samt Cache-Anteil. Der Verlauf
  bleibt im OpenAI-Format; übersetzt wird nur an der Leitung. Weil dieses Format keinen
  Platz für Denk-Blöcke hat, merkt sich der Adapter die Original-Antworten (Schlüssel:
  Tool-Call-IDs bzw. Text) und setzt sie beim nächsten Request wieder ein; ändert eine
  Kompaktierung den Verlauf, verwirft die API unpassende Blöcke
  (`prefix_mismatch_behavior: "drop_block"`) statt den Request abzulehnen. Hängt am
  Feature `openai` (dasselbe `ureq`), in `auto` hinter Azure und OpenAI. Nicht
  eingebaut: serverseitige Modell-Fallbacks — eine Ablehnung (`refusal`) endet als
  Fehler mit Hinweis auf `--model`, weil ein Fallback mitten im Stream bereits
  gemeldete Tool-Aufrufe zurücknehmen müsste, was der Loop nicht kann.
- **Gemessener Token-Verbrauch als Ereignis** (`Usage` in `src/llm.rs`, Ereignistyp
  `token_usage`, kein Python-Pendant). Der Provider meldet am Stream-Ende, was ein Call
  gekostet hat (OpenAI/Azure über `stream_options.include_usage`, Anthropic über
  `message_delta`); `consume_stream` reicht das als EIN Ereignis je Call weiter. Damit
  landet der Verbrauch ohne weitere Verdrahtung im Trace und in agentkit-viz (Summe je
  Agent), im TUI (Titelzeile) und in der CLI (Bilanzzeile, `--token-limit N` bricht
  kooperativ über den Stop-Knopf ab). Ein Ereignistyp statt `structured`, weil jedes
  Frontend ihn auswertet. Bewusst **keine** Kostenschätzung in Geld: die bräuchte eine
  Preistabelle, die beim nächsten Preisschritt falsch wäre. Nicht erfasst sind die
  nicht gestreamten `complete()`-Calls (Kompaktierung) — `Message` trägt keinen Verbrauch.
- **Schema-Prüfung ohne Abhängigkeit** (`src/schema.rs`, kein Python-Pendant). `--schema`
  und `--check` der CLI prüfen die Antwort gegen die Teilmenge von JSON Schema, die
  Extraktions-Schemas tatsächlich nutzen (`type`, `enum`, `const`, `properties`,
  `required`, `additionalProperties`, `items`, Längen- und Zahlengrenzen, `anyOf`/`oneOf`/
  `allOf`, lokale `$ref`). Unbekannte Schlüsselwörter werden ignoriert statt abgelehnt.
  `native_compatible` entscheidet, ob der Anbieter das Schema selbst erzwingen darf, und
  zwar als Schnittmenge der Regeln von OpenAI (strikt) und Anthropic: lieber ein Versuch
  mehr als ein HTTP 400.
- **Erweiterungspunkt `extra_tools`** (`CodingAgentConfig`/`TuiConfig`, `ExtraToolCtx` in
  `src/app.rs`, kein Python-Pendant). Eine Closure, die beim Bau des Coding-Agenten die
  Registry und den Lauf-Kontext bekommt und eigene Tools registrieren darf. Sie existiert
  für genau einen realen Nutzer: das `swarm`-Tool aus `../agentkit_swarm`, das der
  Agent-Kern **nicht** kennen darf (die Abhängigkeit läuft nur in eine Richtung). Ein
  Datenfeld genügte nicht — `RunHandle`, `ApproveFn` und im TUI auch das LLM entstehen erst
  *in* `build_coding_agent`, und eine `ToolRegistry` lässt sich nicht mit einer zweiten
  verschmelzen. Der Aufruf sitzt vor `mcp.apply`, damit das Tool automatisch Teil der
  MCP-freien Basis-Registry ist und ein `/mcp`-Toggle überlebt.
- **Context-Management über ctxman (Feature `ctxman`, `--ctx DIR`).** Ersetzt die naive
  Compaction durch das volle ctxman-Modell aus `../ctxman_rs`: Watermarks (soft ⇒ Minor GC
  mit verlustfreier Externalisierung großer Tool-Ergebnisse in einen Blob Store, hard ⇒
  LLM-Compaction mit vorgelagerter Fact-Promotion), das `expand_context_ref`-Tool (Page
  Fault) und Snapshot-Persistenz des kompletten Kontexts. Promotete Fakten landen als
  JSONL im `LongTermMemory`-Format (mit `--memory` direkt in der `recall`-Datei). Siehe
  `src/context.rs`.

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
cargo test --no-default-features --features ctxman   # inkl. ctxman-Integration
cargo build                               # mit Feature `openai` (ureq + rustls)
cargo run --example react_fake --no-default-features
cargo run --example parallel_subagents --no-default-features
cargo run --example coding_swarm --no-default-features
```

## Benutzerhandbuch

Für Endanwender, die nur das `agentkit`-Kommando nutzen (kein Rust nötig), gibt es ein
vollständiges **[Benutzerhandbuch](docs/USER_MANUAL.md)** — inkl. Kochbuch zum Bauen ganzer
Workflows in PowerShell/Bash.

## Als Executable `agentkit` installieren

Die installierbare Executable `agentkit` (CLI + optionales TUI) liegt im Schwester-Crate
[`../agentkit_app`](../agentkit_app) — dort, weil sie zusätzlich das `swarm`-Tool aus
`agentkit-swarm` einklinkt und ein Binary hier ein Cargo-Paketzyklus wäre. Die gesamte
Logik bleibt in diesem Crate. Mit echtem LLM ist die Executable der **volle Coding-Agent**
(Sandbox-Tools inkl. `glob`/`grep`, Skills, Plan, `task`-Tool für Sub-Agenten, dynamische
Schwärme), ohne Key ein netzfreier Demo-Modus:

```powershell
# Windows, ohne Rust-Toolchain: Release-Binary holen, in den PATH legen, Config anlegen
irm https://raw.githubusercontent.com/rudi77/agentkit_rs/main/scripts/agentkit_setup.ps1 | iex
```

```bash
cargo install --path ../agentkit_app --bin agentkit --features "pdf tui"   # nach ~/.cargo/bin
agentkit "Was ist 17 + 25?"          # One-shot im aktuellen Verzeichnis
agentkit                             # interaktive Session (REPL)
agentkit --tui                       # interaktives Terminal-UI (Feature `tui`)
agentkit --demo "3 + 4"              # Demo-Modus erzwingen (kein Key nötig)
agentkit config show                 # Konfiguration prüfen (~/.agentkit/config.json)
```

Wichtige Optionen (wie die Python-CLI): `-w/--workspace`, `-s/--strategy react|plan|plain|plan_execute`,
`--skills DIR`, `--agents DIR` (Custom-Rollen als `*.md`), `--memory FILE`,
`--session FILE` (Verlauf laden/speichern — Resume über Prozessgrenzen),
`--ctx DIR`/`--ctx-budget N` (ctxman-Kontext-Management, Feature `ctxman`),
`--provider auto|azure|openai|anthropic|demo`, `--max-steps N`, `--token-limit N`, `--no-subagents`,
`--no-swarm` (dynamische Agenten-Schwärme abschalten — siehe
[`../agentkit_swarm`](../agentkit_swarm/README.md#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)),
`-y/--yes` (Shell ohne Rückfrage), `--steps`, `--no-color`, `-p/--print`, für MCP
`--mcp-config FILE`, `--mcp NAME` (mehrfach) und `--no-mcp` (siehe **MCP** unten), sowie
für per-Agent-Config `--system TEXT`, `--system-file FILE` und `--profile FILE`
(Config-Bündel je Pipe-Stage — siehe **Pro-Agent-Config** unten).
Slash-Befehle in der Session: `/help /clear /reset /plan /tools /skills /agents /export /context /model /compact
/sessions /rewind /fork /mcp /exit`.
`Ctrl-C` bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).

**Konfiguration** (`agentkit config init|path|show|trust`, siehe [`src/config.rs`](src/config.rs)):
die Zugangsdaten stehen in `~/.agentkit/config.json` (Windows: `%USERPROFILE%\.agentkit\`),
damit die installierte Executable überall läuft — nicht nur im Projektverzeichnis. Sie ist
die unterste Ebene einer Kette, jede darüber gewinnt:

```text
echte Umgebungsvariable  >  .env im Arbeitsverzeichnis  >  ~/.agentkit/config.json
```

Alle drei speisen dieselben Variablen (`AZURE_OPENAI_*` / `OPENAI_API_KEY` /
`OPENAI_BASE_URL` / `ANTHROPIC_*`) — der Rest des Codes liest weiter nur die Umgebung. Platzhalter (`<…>`)
in der Config gelten als *nicht gesetzt*, eine frische Vorlage landet also sauber im
Demo-Modus.

Zwei Ausnahmen bei der `.env`, weil sie zum Projekt gehört (auch zu einem fremden, frisch
geklonten): Variablen, die Programme starten oder Rückfragen abschalten
(`AGENTKIT_HOOKS`, `AGENTKIT_ALLOW`, `AGENTKIT_HOME`, `PATH`, `LD_PRELOAD`, `BASH_ENV`, …),
werden von dort nicht übernommen — agentkit nennt sie beim Start; setze sie als echte
Umgebungsvariable (Hooks auch per `--hooks`). Und steht in der `.env` nur ein Endpunkt
(`OPENAI_BASE_URL` & Co.) ohne Key, ergänzt agentkit den Key aus der `config.json` nur für
`localhost` — sonst den Key mit in die `.env` schreiben oder exportieren. Alle anderen
Variablen der `.env` gelten wie bisher und werden an `run_shell` vererbt.

**Lokale Modelle** (Ollama, LM Studio, vLLM, llama.cpp, …) laufen über denselben
OpenAI-Pfad: `OPENAI_BASE_URL` auf den lokalen Server zeigen lassen (z. B.
`http://localhost:11434/v1` für Ollama) und `OPENAI_MODEL` auf das geladene Modell —
ein API-Key ist dann optional. Details: [USER_MANUAL](docs/USER_MANUAL.md#3-llm-zugang-einrichten).

Setup-Skript, Install-Skripte (Windows & Linux) und fertige CI-Release-Binaries:
siehe **[../INSTALL.md](../INSTALL.md)**.

## Unix-Pipe-Kompatibilität — `agentkit` als nativer Filter

Zusätzlich zum interaktiven Coding-CLI verhält sich die `agentkit`-Executable wie ein
ordentlicher Unix-Filter. Die Standard-Streams sind die primären I/O-Adapter
(hexagonale Architektur — der Agent-Kern bleibt unberührt):

| Stream | Inhalt |
|---|---|
| **`stdin`** | *nur* Kontext/Datenströme. Ist `stdin` nicht interaktiv (Pipe/Umleitung), wird der gesamte Inhalt gelesen und an die Query angehängt. |
| **`stdout`** | sobald die Ausgabe gepipt wird, im `--format json`- oder `-p/--print`-Modus läuft: *nur* das finale, bereinigte Resultat — bei `--format json` **genau ein** JSON-Dokument. So kann ein nachfolgendes `jq`/`awk`/ein zweiter Agent sich auf Format-Treue verlassen. In diesen Modi wird die Antwort auch NICHT mehr live auf `stderr` mitgeschrieben: sie stand sonst zweimal im Terminal (gestreamt auf stderr, fertig auf stdout) und sah bei `--format json` wie zwei aufeinanderfolgende JSON-Dokumente aus. Der Tool-Trace (`--steps`) bleibt davon unberührt. |
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
| `--dry-run` | Führt den Loop aus, blockiert aber zerstörerische Schreib-/MCP-Vorgänge (deklarierte Tool-Wirkung, sonst Heuristik per Tool-Name) und loggt die versuchten Aktionen nur auf `stderr`. |
| `--max-context <TOKENS>` | Kontext-Limit (Default 128000); größer ⇒ Exit-Code 3. |
| `-p`/`--print` | One-shot: nur die finale Antwort auf `stdout`. |
| `--system <TEXT>` | System-Prompt; ERSETZT den eingebauten vollständig. |
| `--system-file <FILE>` | Wie `--system`, aber aus Datei (überschreibt `--system`). |
| `--profile <FILE>` | **Config-Bündel je Agent** (JSON) — siehe unten. Explizite Flags gewinnen. |

### Unix-Werkzeug: `--tools`, `--check`, `--schema`, `--each` & Co.

Diese Optionen machen agentkit zu einem Baustein wie `grep`, `jq` oder `xargs`.

| Option | Wirkung |
|---|---|
| `--tools none` | Reiner Modell-Aufruf: keine Werkzeuge, keine MCP-Server, keine Projekt-Instruktionen, keine Rückfrage, Strategie `plain`. Das ist die schnelle und billige Variante für Filter wie Übersetzen oder Zusammenfassen. |
| `--tools LISTE` | Nur diese Werkzeuge, zum Beispiel `read_file,grep,glob_files` oder `read_only`. Die Schreibweise ist dieselbe wie bei Rollen, Claude-Code-Namen (`Read`, `Grep`) werden übersetzt. Unbekannte Namen stehen als Warnung auf stderr. |
| `--check` | Ja/Nein-Prüfung. Exit `0` heißt Ja, Exit `1` heißt Nein, die Begründung steht auf stderr, stdout bleibt leer wie bei `grep -q`. Intern ist es ein festes Schema `{ergebnis, begruendung}`. |
| `--schema FILE` | Antwort als JSON nach einem JSON-Schema. Erfüllt das Schema die Regeln der Anbieter (jedes Objekt `additionalProperties: false`, alle Felder in `required`, keine Zahlen- oder Längengrenzen), erzwingt der Anbieter die Struktur selbst: OpenAI/Azure über `response_format` (`json_schema`, strikt), Anthropic über `output_config.format`. Sonst prüft agentkit die Antwort (`src/schema.rs`) und wiederholt sie bis zu `--json-retries` Mal, jeweils mit den Verstößen als Rückmeldung. Gelingt das nicht, endet der Lauf mit Exit `4`. |
| `--each` | Jede nicht leere stdin-Zeile ist ein eigener Auftrag. `{}` im Prompt wird durch die Zeile ersetzt, ohne `{}` hängt sie als Kontext an. Auf stdout steht JSONL in Eingabe-Reihenfolge, je Zeile `index`, `input`, `exit`, `result` und `usage`. Der Exit-Code ist `0`, wenn alle Zeilen gelangen, sonst der Code der ersten fehlgeschlagenen. |
| `-j N` / `--jobs N` | Mit `--each` laufen N Aufträge gleichzeitig. `--token-limit` und `--timeout` gelten für den ganzen Lauf. |
| `--patch` | Der Agent arbeitet auf einer Kopie des Workspaces. Auf stdout steht ein Unified Diff für `git apply`, die Antwort des Agenten geht auf stderr, das Original bleibt unberührt. Im Git-Repo wird kopiert, was Git kennt oder nicht ignoriert. `run_shell` läuft in der Kopie, ist aber keine Sandbox. |
| `--format stream-json` | Jedes Ereignis steht als eigene JSON-Zeile auf stdout (`{"type", "source", "data"}`, dieselbe Serialisierung wie im Trace, aber mit `text_delta`). Zum Schluss folgt ein `{"type": "result", "exit", "result", "usage"}`. |
| `--cache DIR` | Bei gleichem Modell, Zusatz-Prompt, Werkzeugen, Format, Schema und Auftrag kommt das gespeicherte Ergebnis, ohne dass das Modell erneut gefragt wird. Gespeichert werden nur erfolgreiche Läufe. Mit Werkzeugen sieht der Cache den Stand des Workspaces nicht, am besten passt er zu `--tools none`. |
| `--timeout DAUER` | Obergrenze der Laufzeit (`90`, `30s`, `5m`, `1h`). Danach wird der Lauf abgebrochen und endet mit Exit `124` wie bei `timeout(1)`. Reagiert er nach zwei Sekunden nicht, endet der Prozess hart. |
| `-f DATEI` | Datei mit Namen als Kontext (`--- Datei: NAME ---`), mehrfach möglich. |
| `-o DATEI` | Das Resultat geht in DATEI statt auf stdout. Die Datei wird erst beim ersten Schreiben angelegt, ein gescheiterter Lauf lässt sie also unverändert. |
| `-q` | stderr ist komplett stumm, der Exit-Code trägt die Information. Shell-Befehle laufen nur mit `-y` oder über die allow-Liste, weil niemand eine Rückfrage sähe. |

```bash
# Ja/Nein als Exit-Code, ideal für Git-Hooks und CI
git diff --cached | agentkit --tools none --check "Ist der Diff frei von Secrets und Debug-Ausgaben?" && git commit
# LLM-xargs: eine Zeile je Auftrag, acht parallel, JSONL in Eingabe-Reihenfolge
ls src/*.rs | agentkit --each -j 8 --tools read_file "Fasse {} in einem Satz zusammen" > summaries.jsonl
# Verlässliche Struktur für jq
agentkit --tools none --schema invoice.schema.json "Extrahiere die Rechnungsdaten" < rechnung.txt | jq .betrag
# Erst ansehen, dann anwenden
agentkit -y --patch "Ersetze println! durch log::info!" > fix.diff && git apply --check fix.diff
```

#### Eigene Befehle: `agentkit run NAME` und Symlinks

Eine Datei `~/.agentkit/commands/NAME.md` (bzw. `$AGENTKIT_HOME/commands/`) definiert einen
eigenen Befehl. Der Frontmatter enthält die Einstellungen mit denselben Schlüsseln wie
`--profile`, der Text darunter ist der System-Prompt.

```markdown
---
tools: none
format: text
---
Fasse den Text in genau einem Satz zusammen.
```

Aufruf: `agentkit run summarize` oder als Symlink `ln -s "$(which agentkit)" ~/bin/summarize`,
dann funktioniert `cat log | summarize`. Explizite Flags schlagen `--profile`, und `--profile`
schlägt den Befehl. `true`/`false` und Zahlen werden typisiert, `mcp` und `allow_read` sind
Komma-Listen, relative Pfade in `schema` und `system_file` gelten ab der Befehlsdatei.
`agentkit run` ohne Namen listet die vorhandenen Befehle. `agentkit run NAME acp` bzw.
`agentkit run NAME mcp-serve` wendet das Befehlsprofil auch auf diese Server an — so bekommt
ein Editor einen eigenen Modus.

#### Skills und Plugins installieren: `agentkit skills …`, `agentkit plugins …`

Installierte Skills lädt jeder Lauf mit, ohne `--skills`. Das gilt für REPL, One-shot,
`--tui`, das `tui`-Binary, `acp` und `mcp-serve`.

```bash
agentkit skills install fehlersuche       # aus dem Release-Paket agentkit-skills.json
agentkit skills install --all
agentkit skills install ./mein-skill      # Ordner mit SKILL.md, Ordner voller Skills oder .json-Paket
agentkit skills list | remove NAME | pack DIR…

agentkit plugins install dokumente        # aus agentkit-plugins.json
agentkit plugins list | remove NAME | enable NAME | disable NAME | pack DIR…
agentkit run dokumente                    # der Befehl, den das Plugin mitbringt
```

Ein **Plugin** bündelt Skills, Befehle und Rollen und besteht nur aus Text:

```text
<plugin>/
  plugin.json          name, version, description (optional author, license, homepage)
  README.md            optional
  skills/<s>/SKILL.md  wie bei --skills
  commands/<b>.md      wie ~/.agentkit/commands/<b>.md: Frontmatter = Einstellungen, Text = System-Prompt
  agents/<r>.md        wie bei --agents
```

Abgelegt wird alles unter `<config_dir>/skills/<name>/` bzw. `<config_dir>/plugins/<name>/`.
Ein ausgeschaltetes Plugin liegt in `plugins-disabled/`. Für die Reihenfolge gilt:

- **Skills:** zuerst die aus `--skills`, dann die installierten, dann die der Plugins. Bei
  gleichem Namen gewinnt der zuerst gefundene.
- **Rollen:** die Rollen der Plugins werden zuerst geladen, `--agents` danach. `--agents`
  nimmt dafür mehrere Verzeichnisse mit `;`, und bei gleichem Namen überschreibt das später
  geladene. Mit `--agents-only` gelten nur die Rollen aus `--agents`.
- **Befehle:** eigene Befehle aus `~/.agentkit/commands` gehen vor die eines Plugins.

Sobald `--tools` eine Auswahl trifft, wird nichts Installiertes mitgeladen. Ein solcher Lauf
ist ein Werkzeug in einer Pipeline, und ein Skill-Hinweis im Prompt würde ihn verändern.

Für die Sicherheit gelten diese Regeln:

- **Quellen:** nur das eigene Release (`releases/latest/download/…`, über den HTTP-Adapter
  von `--upgrade`, Feature `upgrade`) oder ein lokaler Pfad.
- **Paketformat:** JSON, je Eintrag relative Pfade auf UTF-8-Text. Ein Archiv gibt es nicht,
  Symlinks und Binärdaten kann das Format also gar nicht darstellen.
- **Pfade:** jeder Pfadbestandteil muss `[A-Za-z0-9._-]` sein, ohne führenden oder
  abschließenden Punkt. Das schließt `..`, absolute Pfade, `\`, versteckte Dateien und
  reservierte Windows-Namen aus.
- **Namen:** nur `[a-z0-9-]`.
- **Schreiben:** nur unter dem Config-Verzeichnis. Die Dateien landen zuerst in einer
  Zwischenablage, dann wird getauscht, und sie werden nie ausführbar angelegt.
- **Ausführbares wird abgelehnt, nicht übergangen:** Hooks (`hooks/`, `hooks.json`,
  `"hooks"`), MCP-Server (`.mcp.json`, `"mcpServers"`), Claude-Code-Plugins
  (`.claude-plugin/`) und unbekannte Bestandteile.
- **Plugin-Befehle** dürfen nur unkritische Einstellungen setzen: `strategy`, `max_steps`,
  `token_limit`, `model`, `format`, `dry_run`, `verify`, `check`, `shell_timeout`,
  `timeout`, `no_subagents`, `no_swarm`, `system` sowie
  `graph`/`ctx` nur als `false`. Gesperrt ist alles, was Freigaben aufhebt (`yes`), Code
  startet (`hooks`, `mcp_config`, `mcp`), die Sandbox erweitert (`allow_read`, `workspace`)
  oder Dateien außerhalb des Plugins liest; ebenso `no_project_instructions`, das die
  `deny:`-Leitplanken des Projekts abschalten würde. Gesperrt ist auch `tools`: eine
  Werkzeug-Auswahl schaltet das Mitladen installierter Skills ab, und der Befehl verlöre so
  die Skills seines eigenen Plugins. Geprüft wird bei der Installation und bei jedem Laden.

Wie weit das zu Claude Code passt: Skills (`SKILL.md`, `${CLAUDE_SKILL_DIR}`) und Rollen
(Claude-Code-Toolnamen werden übersetzt) haben dasselbe Format. Befehle haben es **nicht**:
bei Claude Code ist ihr Text ein Nutzer-Prompt, hier ein System-Prompt. Ein Claude-Code-Plugin
lässt sich deshalb nicht direkt installieren.

Das erste Plugin ist [`examples/dokumente/plugin`](examples/dokumente/plugin). Das erste
Skill-Paket liegt in [`../agentkit_app/skills`](../agentkit_app/skills).

### Pro-Agent-Config: `--profile` (eine Datei je Pipe-Stage)

Damit jede Stage einer Pipe *ein* eigenes Config-Bündel bekommt (eigener System-Prompt,
Tools/Skills, MCP-Server, Strategie …), statt vieler Einzel-Flags, liest `--profile FILE`
eine JSON-Datei. **Explizite CLI-Flags überschreiben** die Profilwerte (Profil = Basis).

```jsonc
// extractor.json — eine spezialisierte Pipe-Stage
{
  "system": "Du extrahierst Struktur. Antworte NUR mit gültigem JSON, keine Prosa.",
  // "system_file": "prompts/extractor.md",   // Alternative: Prompt aus Datei
  "strategy": "plain",           // react | plan | plain | plan_execute
  // nur für plan_execute (alle optional): Feintuning des Phasen-Treibers
  // "strategy_params": { "max_plan_steps": 12, "plan_max_steps": 4,
  //                      "step_max_steps": 20, "reflect": true,
  //                      "max_rework_per_step": 1, "max_replans": 2 },
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
  "format":   "json",            // text | json | stream-json
  "dry_run":  false,
  // Unix-Werkzeug (siehe oben): tools ("none" oder Liste), schema, check,
  // cache, timeout (Sekunden oder "5m"), model
  "tools":    "none"
}
```

Alle Felder sind optional. Damit wird die Pipe zu einer Kette klar getrennter Agenten:

```bash
cat src/lib.rs \
 | agentkit -p --profile agents/extractor.json "Extrahiere alle öffentlichen Funktionen" \
 | agentkit -p --profile agents/rater.json     "Bewerte jede nach Komplexität" \
 | agentkit -p --profile agents/writer.json    "Schreibe einen Markdown-Report"
```

Wichtig: `--system`/`--system-file`/`profile.system` **ERSETZEN** den eingebauten
System-Prompt — dein Text ist dann der ganze Prompt, ohne Coding-Anleitung, ohne
Shell-Hinweis, ohne Sub-Agenten-Block und ohne den Prosateil der `AGENTS.md` (deren
Leitplanken bleiben — sie beschreiben das Werkzeug, nicht die Arbeitsweise). Es gibt genau einen
System-Prompt, und du entscheidest, ob es der eingebaute oder deiner ist. Für reine
LLM-Transform-Stages ist das genau richtig (kombiniert mit `"strategy": "plain"`);
brauchst du dort Werkzeuge, schreib die nötigen Hinweise selbst hinein — etwa welche
Shell dich erwartet.

Die übrigen Optionen (`--workspace`, `--provider`, `--skills`, `--agents`, `--memory`,
`--max-steps`, `--no-subagents`, `-y`, `--steps`, `--no-color`, `--demo`, `--plan`/
`--plain`, `--tui`) sind unter `agentkit --help` dokumentiert.

### Exit-Codes (für `set -e`-Pipelines)

| Code | Bedeutung |
|---|---|
| `0` | Erfolg — Resultat auf `stdout` geflusht. |
| `1` | Unerwarteter Laufzeitfehler. |
| `2` | API/Netz (Modell unerreichbar, Rate-Limit) — **beim Orchestrator**. |
| `3` | Kontext zu groß oder Prompt ungültig/leer. |
| `4` | Erzwungenes `--format` bzw. `--schema` trotz Retries nicht erzeugbar. |
| `124` | `--timeout` abgelaufen (wie `timeout(1)`). |

Mit `--check` bedeutet `1` „Nein". Ein Laufzeitfehler ergibt ebenfalls `1`, ein Skript mit
`… --check … || fallback` fällt also auf die sichere Seite. Die Unterscheidung steht auf stderr.

Code `2` zählt nur Modellfehler des **Orchestrators** (Events mit leerer `source`,
`ist_harter_fehler` im `agentkit`-Binary) — dieselbe Unterscheidung wie beim `DONE`.
Ein transienter Fehler in einem Sub-Agenten (`task`) oder Schwarm-Mitglied
(`../agentkit_swarm`) beendet den Lauf nicht: sonst verwirft ein einzelner 429 in
einem von N Mitgliedern das fertige Ergebnis des Orchestrators, und `stdout` bliebe
leer, obwohl der Lauf erfolgreich war. Beobachtet an einem Schwarm-Lauf, der per
Konsens abschloss und trotzdem mit Code 2 und leerem `stdout` endete.

Die Pipe-Bausteine (Exit-Codes, Format, stdin-/JSON-Helfer) liegen entkoppelt und
testbar in `src/cli.rs`; das Argument-Parsing selbst im `agentkit`-Binary.

### Argument-Konventionen (GNU/POSIX)

`agentkit` hält sich an die üblichen Shell-Konventionen, damit es sich wie ein natives
Kommando anfühlt:

- **`--flag=value`** und **`--flag value`** sind gleichwertig (`--workspace=/tmp` ⇔
  `--workspace /tmp`).
- **`--`** beendet die Optionen — alles danach ist wörtlicher Auftrag, auch wenn es mit
  `-` beginnt: `agentkit -- "-p ist hier nur Text"`.
- **Unbekannte Optionen, fehlende und ungültige Werte** beenden den Aufruf mit Exit 3
  und einer Meldung auf `stderr` — bei einem Tippfehler samt Vorschlag
  (`Unbekannte Option --dry-rnu. Meintest du --dry-run?`). Ein Lauf ohne den verlangten
  Schutz wäre schlimmer als eine Absage.
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

Vervollständigt werden Flags samt Werten (`--strategy` → `react|plan|plain|plan_execute`,
`--provider` → `auto|azure|openai|anthropic|demo`, `--format` → `text|json`) sowie Datei-/
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
cargo build --release --manifest-path ../agentkit_app/Cargo.toml --bin agentkit --features pdf   # oder: --features "pdf tui"
agentkit read-pdf rechnung.pdf | agentkit -p --format json --system-file extract.md "Extrahiere Felder"
```

### Dokumente auswerten — `doc_ingest`

`read_pdf` liefert ein Dokument am Stück. Für eine Rechnung reicht das; ein Vertrag mit 80
Seiten passt in kein Tool-Ergebnis, und wer ihn am Stück liest, kann keine Fundstelle nennen.
`doc_ingest` legt ein Dokument deshalb einmal als Verzeichnis ab:

```text
doc_ingest(path: "vertrag.pdf")        → .agentkit/docs/vertrag/
  index.md                               Gliederung: Titel, Fundstelle, Größe je Abschnitt
  001-anfang.md                          Text vor der ersten Überschrift
  002-1-vertragsgegenstand.md            „§ 1 Vertragsgegenstand" mit seinem Text, wörtlich
  003-2-laufzeit-und-kuendigung.md
  …
```

Ein Abschnitt ist immer **eine Überschrift mit ihrem Text**; `parent` nennt die Überschriften
darüber. Nur die Fundstelle unterscheidet sich:

| Quelle | Was als Überschrift gilt | Fundstelle im Frontmatter |
|---|---|---|
| Markdown, Text | `#` bis `######` | `lines: "120-180"` |
| PDF (Feature `pdf`) | eine kurze Zeile, die größer gesetzt ist als der Fließtext oder mit einer Gliederungsnummer beginnt (`§ 7`, `3.2 Titel`, `Artikel 5`, `Anlage 2`) | `pages: "12-13"` |

Ein PDF kennt keine Gliederung, nur Zeichen an Positionen — die Überschriften werden also
**erkannt**, nicht ausgelesen (`documents::split_pdf_lines`):

- Ein Abschnitt läuft über den Seitenumbruch hinweg; die Seiten sind nur noch die Fundstelle.
- Was sich auf vielen Seiten wiederholt (Kopf- und Fußzeilen), ist keine Überschrift. Der Text
  bleibt aber stehen — entfernt wird nichts.
- **Fett allein wird nicht erkannt**: die Extraktion liefert Schriftgröße und Position, nicht
  den Schriftschnitt. Eine fette Überschrift in Textgröße zählt nur mit Gliederungsnummer.
- Eine einzelne Zahl (`1. Äpfel`) zählt nicht als Nummer — das ist ebenso oft ein Listenpunkt.
- Erkennt das nichts (eine Rechnung, ein Brief), bleibt es bei **einem Abschnitt je Seite**.

Regeln, die man kennen sollte:

- **Verlustfrei und deterministisch.** Der Text eines Abschnitts steht wörtlich da; dieselbe
  Quelle ergibt dieselben Dateien. Erst das macht ein Zitat daraus zu einem Beleg — der
  Wissensgraph erkennt eine Belegstelle nur an, wenn sie wörtlich in einem Tool-Ergebnis steht.
- **Höchstens 12.000 Zeichen je Abschnitt** (`documents::MAX_SECTION_CHARS`). Größere werden an
  Absätzen geteilt (`… (Teil 2/3)`). Die Grenze liegt unter der Kürzung von Tool-Ergebnissen,
  damit das Modell jeden Abschnitt ganz sieht.
- **Wiederholbar.** Das Bundle wird im Speicher gerendert und mit dem Ziel verglichen — ohne
  Zustandsdatei, weil die Zerlegung deterministisch ist. Gleich: nichts geschieht. Ein Bundle
  derselben Quelle (Zeile `# <quelle>` in `index.md`) mit anderem Inhalt: die Quelle hat sich
  geändert, ihre Bundle-Dateien (`index.md`, `NNN-<titel>.md`, sonst nichts) werden entfernt
  und neu geschrieben, und das Ergebnis sagt dem Modell, dass früher Gelesenes nicht mehr gilt.
  Jedes andere nicht leere Ziel bleibt unangetastet und ist ein weicher Fehler.
- **Fundstelle im Ergebnis.** Das Tool-Ergebnis weist das Modell an, jede Aussage mit Seite bzw.
  Zeilen aus dem Abschnittskopf zu nennen. Die Anweisung steht dort statt im gemessenen
  Coding-System-Prompt und gilt so genau dann, wenn ein Dokument im Spiel ist. Aus demselben
  Grund verweist die Beschreibung von `read_pdf` für große PDFs auf `doc_ingest`.
- **Kein OCR.** Ein gescanntes PDF ohne Text-Ebene ist ein weicher Fehler.
- Eine Überschrift, auf die direkt eine tiefere folgt, wird kein eigener Abschnitt; sie steht
  als `parent` an ihren Kindern.

Das Verzeichnis ist ein OKF-Bundle (`index.md` mit `okf_version`, je Abschnitt `type`, `title`,
`description`, `resource`; das Eigene unter `agentkit:`) und besteht den Referenz-Validator aus
okf-skills. Die Auswertung selbst — Aussagen mit Fundstelle festhalten, Dokumente
gegenüberstellen — ist Sache des Wissensgraphen (`graph_remember`, `graph_compare`) und der
Skills in [`examples/dokumente`](examples/dokumente).

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

### Beispiel: Coding-Swarm — ein Software-Dev-Team als Agent-Schwarm

[`examples/coding_swarm`](examples/coding_swarm/README.md) baut aus dem `task`-Tool und
Markdown-Rollen (`--agents`) ein ganzes Entwicklungsteam: ein **Tech-Lead-Orchestrator**
delegiert an **architect** (read-only Analyse), **developer** (Umsetzung),
**tester** und **reviewer** (parallel, beide read-only) und iteriert auf deren Befunde —
komplett aus Daten, ohne neuen Framework-Code. Dasselbe Team läuft headless gegen die
Benchmarks in [`../agent_benchmarks`](../agent_benchmarks/README.md) (SWE-bench Lite,
Terminal-Bench 2.0, Aider Polyglot) via `AGENTKIT_SWARM=1`. Eine Offline-Demo der
kompletten Verdrahtung: `cargo run --example coding_swarm --no-default-features`. Das
README diskutiert auch die Alternativen (deterministische Pipeline, fester Peer-Schwarm
via `add_subagent`) und wann welche Form die richtige ist.

### Beispiel: GitHub Action — Review und Issue-Fix im Workflow

[`examples/github_action/`](examples/github_action/README.md): die Action aus `action.yml` im
Repo-Wurzelverzeichnis lädt das Release-Binary und führt einen Auftrag im Workflow aus. Zwei
Workflows zum Kopieren: ein nur lesendes PR-Review als Kommentar und ein Issue-Fix, der per
Label startet und einen Pull Request öffnet.

### Beispiel: PR-Review — GitHub und Azure DevOps

[`examples/pr_review`](examples/pr_review/README.md) zeigt PR-Reviews mit agentkit:
lokal über die eingebauten read-only **git-Tools** (`git_status`/`git_diff`/`git_log`/
`git_show` — ohne Shell-Freigaben), für PR-Metadaten und Kommentare über MCP — den
offiziellen **GitHub MCP Server** oder Microsofts **Azure DevOps MCP Server**
(`@azure-devops/mcp`). Dazu eine `pr-reviewer`-Rolle für das `task`-Tool und ein
`--profile` für strukturierte JSON-Reviews in Pipelines.

## Context-Management für lange Läufe — ctxman (Feature `ctxman`)

Für lange Coding-Sessions und große Reviews reicht die naive Compaction nicht: sie ist
verlustbehaftet und wirft irgendwann Relevantes weg. Mit dem Feature `ctxman` übernimmt
der Rust-Port aus [`../ctxman_rs`](../ctxman_rs) das Kontext-Management
(`--ctx DIR`, Budget via `--ctx-budget N`):

- **Watermarks statt harter Grenze:** bei 60 % des Budgets externalisiert ein **Minor GC**
  große Tool-Ergebnisse verlustfrei in einen Blob Store (im Kontext bleibt Summary +
  Referenz); bei 80 % läuft ein **Major GC** — LLM-Compaction mit vorgelagerter
  **Fact-Promotion** (dauerhafte Fakten landen als JSONL im `LongTermMemory`-Format;
  mit `--memory FILE` direkt in der `recall`-Datei).
- **Page Fault:** über das Tool `expand_context_ref` holt sich das Modell ausgelagerte
  Inhalte bei Bedarf vollständig zurück.
- **Snapshot-Resume:** der komplette Kontext (inkl. Blob-Referenzen) liegt als
  `DIR/snapshot.json` — ein Neustart macht exakt dort weiter. Die Policy ist dabei
  **eingefroren** (ctxman-Spec): `--ctx-policy`/`--ctx-budget` wirken nur auf eine
  NEUE Session; bei Resume weist eine Meldung darauf hin.
- **Eigene Lebensdauer für Tool-Ergebnisse:** `tool_result` lebt 12 Züge statt der
  2 aus der ctxman-Spec-Policy. Die Spec-Zahl stammt aus einer anderen Domäne; ein
  Coding-Agent liest eine Datei und arbeitet mehrere Züge daran, und war der Inhalt
  dann weg, las er sie erneut. Gemessen im SWE-bench-Lauf `ctxfix-25`: 280
  wiederholte `read_file`-Aufrufe auf dieselbe Datei, Median-Abstand 3 Züge, 56 %
  jenseits der alten TTL; in einem Explorer waren 567 von 649 KiB Kontext
  Dubletten. Teurer wird der Kontext dadurch nicht — `tool_result` ist
  externalisierbar, große Ergebnisse wandern weiter in den Blob Store und bleiben
  als Ref-Hinweis erreichbar, statt ganz zu verschwinden. Über `--ctx-policy`
  überschreibbar.
- **Policy konfigurierbar (`--ctx-policy FILE`):** ein partielles JSON-Overlay über
  die Default-Policy — nur die angegebenen Felder werden überschrieben, Objekte
  rekursiv gemergt. Unbekannte Felder und inkonsistente Watermarks (soft < hard <
  emergency ≤ 1) sind harte Fehler statt stiller No-ops. Beispiel:

  ```json
  { "watermarks": { "soft": 0.5 },
    "kinds": { "tool_result": { "ttl_turns": 5 } },
    "compaction": { "max_share": 0.4 },
    "tokenizer": "o200k" }
  ```

- **Echte Token-Zählung (Feature `tiktoken`):** der Policy-Wert `tokenizer` wählt
  den Counter wirklich aus — `heuristic` (Zeichen/4), `o200k` (GPT-4o/5-Familie)
  oder `cl100k`. Mit Feature `tiktoken` ist `o200k` der Default (Release-Builds
  aktivieren es); ohne Feature bleibt es ehrlich bei `heuristic`.
- **Separates Compaction-LLM (`--ctx-compaction-model NAME`):** Summarization und
  Fact-Extraction laufen über ein eigenes (günstiges) Modell — bei Azure der
  Deployment-Name, sonst der OpenAI-Modellname, aus derselben Provider-Umgebung.
  Ohne das Flag übernimmt das Agent-LLM, und `compaction.model` in der Policy
  trägt dessen echtes Label (die irreführenden Beispielwerte der Spec-Vorlage —
  `"claude"`-Tokenizer, `"claude-haiku"`-Modell — werden nicht mehr geschrieben).
- **Kind-Semantik statt Einheitsbrei:** große `read_skill`-Ergebnisse werden als
  `skill_content`-Segment angehängt (refetchable, TTL 8 — nach Ablauf per
  Clean-Page-Eviction entfernt; das Modell lädt den Skill bei Bedarf einfach neu),
  Sub-Agent-Ergebnisse (`task`-Tool) als `task`-Segment mit TTL ∞ (teuer zu
  reproduzieren, überleben den GC). Das gepaarte `tool_result` bleibt als kleiner
  Zeiger erhalten, damit das tool_call/tool_result-Pairing intakt bleibt.
  `mcp_resource` bleibt reserviert, bis agentkit MCP-*Resources* (nicht nur
  MCP-Tools) unterstützt; `decision` ist Vokabular für Host-eigene Segmente.

```bash
cargo install --path ../agentkit_app --bin agentkit --features "pdf tui ctxman tiktoken"
agentkit --ctx .agentkit-ctx --memory notizen.jsonl "Reviewe main..HEAD, Datei für Datei."
agentkit --ctx .agentkit-ctx --ctx-policy policy.json --ctx-compaction-model gpt-5.4-nano --tui
```

Ohne das Feature bleibt alles beim Alten (naive Compaction + `--session`-Persistenz);
`--ctx` weist dann per Warnung auf das fehlende Feature hin. Nicht verdrahtet (bewusst):
die `retention`-Felder der Policy — der Blob-Sweep ist im ctxman-Port nicht enthalten
(siehe dessen README, „Bewusste Unterschiede").

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
    "extra":{ "command": "node", "args": ["server.js"], "disabled": true },
    "azure":{ "command": "npx", "args": ["-y", "@azure/mcp"],
              "tools": ["group_list", "storage_blob_list"] }
  }
}
```

Die Server-Tools erscheinen **namespaced** als `mcp__<server>__<tool>` (keine Kollision
mit lokalen Tools). Auto-Discovery sucht `.mcp.json` (dann `mcp.json`) im Workspace und
CWD; ein expliziter Pfad geht via `--mcp-config FILE`.

Pro Server lässt sich zusätzlich ein **Tool-Filter** setzen: `"tools": ["a", "b"]` ist
eine Allowlist — fehlt sie oder ist sie leer, sind alle Tools des Servers aktiv. Ein
Eintrag, den der Server gar nicht anbietet (Tippfehler), fällt beim Start als `[WARN]`
auf, statt still zu verschwinden.

Zusätzlich zur Projekt-/expliziten Config gibt es eine **benutzerweite** Ebene:
`<config_dir>/.mcp.json` (`~/.agentkit/.mcp.json` bzw.
`%USERPROFILE%\.agentkit\.mcp.json`, `$AGENTKIT_HOME` überschreibbar). Diese Ebene
wird **immer** mit der Projekt-/expliziten Config gemerged — bei gleichem
Servernamen gewinnt die Projekt-/explizite Ebene. Nur `--no-mcp` schaltet beide
Ebenen komplett ab.

```bash
agentkit --mcp-config .mcp.json "Nutze das git-Tool und fasse die letzten Commits zusammen"
agentkit --mcp git "…"        # Allowlist: nur den Server 'git' aktiv (mehrfach möglich)
agentkit --no-mcp "…"         # MCP komplett aus
```

**Enable/Disable**

- **Pipe/One-shot (statisch):** Ohne `--mcp` sind alle nicht als `"disabled": true`
  markierten Server aktiv. `--mcp NAME` schaltet eine **Allowlist** (nur die genannten),
  `--no-mcp` alles ab. `--dry-run` blockiert zusätzlich zerstörerische MCP-Aufrufe.
- **REPL (live):** `/mcp` listet die Server samt Status (bei aktivem Tool-Filter als
  `<aktiv>/<angeboten> Tools`), `/mcp on <name>` bzw. `/mcp off <name>` schaltet sie für
  den laufenden Agenten um (ohne Neustart). `/mcp tools <name>` listet die Tools EINES
  Servers samt Status; `/mcp on <name> <tool>` bzw. `/mcp off <name> <tool>` schaltet
  ein einzelnes Tool um — beides wirkt sofort, ohne Reconnect.
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
inkl. `glob`/`grep`, Skills, Plan, das `task`-Tool für Sub-Agenten und das `swarm`-Tool,
mit dem der Agent sich zur Laufzeit einen ganzen Agenten-Schwarm baut (siehe
[`../agentkit_swarm`](../agentkit_swarm/README.md#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)). Da `ratatui`
das Terminal belegt, läuft die `run_shell`-Freigabe nicht über stdin, sondern über
einen **In-TUI-Dialog**; mit **Shift-Tab** (oder `Ctrl-Tab`) schaltet man reihum zwischen
*Nachfragen*, *Auto-Freigabe* und *Plan* um — wie der Permission-Mode in der Claude-Code-CLI
(Modus PLAN: siehe „Bewusste Unterschiede zu Python").

Die Binaries liegen in [`../agentkit_app`](../agentkit_app) (siehe oben), das UI selbst
in `src/tui.rs` dieses Crates:

```bash
cargo run --manifest-path ../agentkit_app/Cargo.toml --bin tui --features tui                        # mit Azure/OpenAI (Default)
cargo run --manifest-path ../agentkit_app/Cargo.toml --bin tui --no-default-features --features tui  # nur Demo-Modus (kein Netz)
cargo run --manifest-path ../agentkit_app/Cargo.toml --bin tui --features tui -- --demo              # Demo-Modus erzwingen
cargo run --manifest-path ../agentkit_app/Cargo.toml --bin tui --features tui -- --help              # Optionen & Tasten
# oder über die Haupt-Executable:
agentkit --tui -w . --skills ./skills
```

Optionen wie im CLI: `-w/--workspace`, `--skills`, `--agents`, `--memory`,
`--no-subagents`, `--max-steps`, `-y/--yes` (Freigabe initial auf AUTO), `--mode
nachfragen|auto|plan` (Startmodus), `--plan`/`--plain`
sowie `--ctx DIR`/`--ctx-budget N`/`--ctx-policy FILE`/`--ctx-compaction-model NAME`
(ctxman-Kontext-Management, Feature `ctxman` — beim Start bestätigt eine Verlaufs-Notiz
die Aktivierung samt Tokenizer; bei Resume weist sie auf die eingefrorene Policy hin).
Konfiguration wie im CLI (`.env` im Arbeitsverzeichnis, sonst `~/.agentkit/config.json`).
LLM-Auswahl (ohne `--demo`): `AZURE_OPENAI_*` → Azure, sonst `OPENAI_API_KEY` oder
`OPENAI_BASE_URL` (+ optional `OPENAI_MODEL`) → OpenAI bzw. lokaler OpenAI-kompatibler
Server, sonst `ANTHROPIC_API_KEY` → Anthropic, sonst der netzfreie **Demo-LLM**. MCP-Optionen (`--mcp-config`, `--mcp`, `--no-mcp`) gelten
auch hier; **F2** öffnet im UI das MCP-Panel zum Ein-/Ausschalten der Server. Tasten:
`Enter` senden, `Esc` abbrechen/beenden, `Shift-Tab` Modus wechseln (nachfragen/auto/plan), `F2`
MCP-Panel, `Ctrl-C` beenden, `↑↓/PgUp/PgDn/End` scrollen.

**`/context`** (Alias `/ctx`) zeigt die aktuelle **Kontext-Belegung** des Agenten —
visuell wie in der Claude-Code-CLI als farbiges Raster plus textuelle Legende: pro
Abschnitt (System-Prompt, Tool-Schemas, User-Nachrichten, Assistant-Antworten,
Tool-Aufrufe, Tool-Ergebnisse, …) die geschätzten Tokens, der Anteil am Budget und
die Anzahl der Einträge, dazu Summe, Budget und freier Platz. Ohne ctxman zählt die
Zeichen/4-Heuristik über die `ShortTermMemory`; mit aktivem Context-Management
(Feature `ctxman`, `--ctx`) kommen die Zahlen aus den ctxman-Segmenten — inklusive
Hinweisen, wie viele Segmente extern ausgelagert bzw. kompaktiert sind. Der Befehl
wird lokal beantwortet (kein Modell-Call). Datenbasis ist `context_report(&agent)`
aus `src/app.rs` — auch für eigene Frontends nutzbar.

Die Belegung sagt, WIE VIEL im Kontext steht. Was tatsächlich drinsteht, zeigen die
Erweiterungen desselben Befehls:

| Befehl | zeigt |
|---|---|
| `/context alles` | eine Zeile je Nachricht: Nummer, Rolle, Tokens, Anfang des Inhalts |
| `/context <n>` | Nachricht `n` vollständig und roh (JSON) |
| `/context <agent>` | dasselbe für einen **Sub-Agenten** oder ein Schwarm-Mitglied (nur TUI) |

Den Kontext eines Sub-Agenten gäbe es sonst nirgends: er stirbt mit dem Tool-Aufruf,
der ihn erzeugt hat. Deshalb legt **jeder** Agent am Ende seines Laufs einen
`context_snapshot`-Datensatz auf den Bus ([`CONTEXT_SNAPSHOT`], `Agent::run_on_bus`) —
das TUI schneidet ihn mit, der Trace schreibt ihn, und **agentkit-viz** zeigt ihn im
Kontext-Reiter für jeden Agenten des Laufs. Geschickt wird nur der Zuwachs seit dem
letzten Datensatz (`messages_from`), sonst wäre ein langes Gespräch quadratisch.

Der Datensatz kommt nicht nur am Ende, sondern auch **vor jedem Modell-Call** (mit
`step`) — erst damit sieht man, wie der Kontext während eines Laufs wächst und wann
eine Verdichtung ihn wieder schrumpfen lässt (nach einer Verdichtung geht der Verlauf
ab `messages_from = 0` neu raus). Mit ctxman trägt er zusätzlich jedes Segment
einzeln samt Zustand (live/ausgelagert/verdichtet/verdrängt) und Policy (`ctx`), und
die GC-Ereignisse der Session (`segment_externalized`, `compaction_completed`,
`watermark_crossed`, …) gehen je Schritt als [`CTX_EVENTS`]-Datensatz auf den Bus —
vorher landeten sie nur in `<ctx>/events.jsonl`, bei Helfern ohne `state_dir` nirgends.
Daraus baut der Kontext-Reiter von agentkit-viz Speicherkarte, Token-Verlauf und
GC-Strom. Python kennt beides nicht; es ist reine Diagnose neben dem Ereignisstrom und
ändert am Loop nichts.

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
