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
  (`run_with_timeout` pollt den Cancel und killt das Child) und überspringt nach
  dem Abbruch noch ausstehende Tool-Aufrufe mit einem weichen `ERROR: abgebrochen.`
  (jede `tool_call`-id behält so ihr Ergebnis).
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
  kein Python-Pendant). Die `run_shell`-Rückfrage kennt neben Ja/Nein ein
  **„[i]mmer erlauben"**, das sich das *erste Wort* des Befehls für die Sitzung merkt —
  `cargo test` freigeben heißt danach jedes `cargo`, nicht jedes beliebige Kommando.
  `/permissions` zeigt die Liste, `/permissions reset` leert sie. Bewusst **nur im
  Speicher**, nicht in `~/.agentkit/config.json`: eine persistierte Allowlist wäre eine
  Sicherheitsentscheidung, die man Wochen später nicht mehr erinnert — und `-y` gibt es
  für den Fall, dass wirklich alles erlaubt sein soll. Bewusst auch **kein** Gate für
  `write_file`/`edit_file`: dafür gibt es `/undo` (siehe unten), und eine Rückfrage pro
  Datei machte den Agenten unbenutzbar.
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
- **Projekt-Instruktionen `AGENTKIT.md` + `/init`** (`load_project_instructions` in
  `src/app.rs`, kein Python-Pendant). Eine Datei dieses Namens im Workspace wird beim
  Bau an den eingebauten System-Prompt angehängt (nicht bei eigenem `--system`: der
  ersetzt den ganzen Prompt). Bewusst **nur dieser eine Name**, kein Rückfall auf `CLAUDE.md`: agentkit
  läuft auch in fremden Repos — die Benchmark-Pipeline lädt es in jeden Task-Container —
  und eine dort zufällig vorhandene Datei würde still den System-Prompt verändern und
  die Läufe unvergleichbar machen. Der Start meldet sichtbar, wenn geladen wurde.
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
  werden abgelehnt). Teil von `READ_ONLY_TOOLS`, damit die `reviewer`-/`explorer`-Rollen
  Diffs und Historie sehen, ohne `run_shell`-Freigaben zu brauchen. Grundlage des
  PR-Review-Beispiels (`examples/pr_review`).
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
- **Session-Persistenz (`--session FILE`).** Der Verlauf wird nach jedem Auftrag als JSON
  gespeichert und beim Start geladen — Resume über Prozessgrenzen für One-shot-Ketten,
  REPL und TUI (`ShortTermMemory::save`/`load`).
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

Wichtige Optionen (wie die Python-CLI): `-w/--workspace`, `-s/--strategy react|plan|plain`,
`--skills DIR`, `--agents DIR` (Custom-Rollen als `*.md`), `--memory FILE`,
`--session FILE` (Verlauf laden/speichern — Resume über Prozessgrenzen),
`--ctx DIR`/`--ctx-budget N` (ctxman-Kontext-Management, Feature `ctxman`),
`--provider auto|azure|openai|demo`, `--max-steps N`, `--no-subagents`,
`--no-swarm` (dynamische Agenten-Schwärme abschalten — siehe
[`../agentkit_swarm`](../agentkit_swarm/README.md#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)),
`-y/--yes` (Shell ohne Rückfrage), `--steps`, `--no-color`, `-p/--print`, für MCP
`--mcp-config FILE`, `--mcp NAME` (mehrfach) und `--no-mcp` (siehe **MCP** unten), sowie
für per-Agent-Config `--system TEXT`, `--system-file FILE` und `--profile FILE`
(Config-Bündel je Pipe-Stage — siehe **Pro-Agent-Config** unten).
Slash-Befehle in der Session: `/help /clear /reset /plan /tools /skills /agents /export /context /model /compact
/sessions /rewind /fork /mcp /exit`.
`Ctrl-C` bricht die laufende Aufgabe kooperativ ab (zweimal = beenden).

**Konfiguration** (`agentkit config init|path|show`, siehe [`src/config.rs`](src/config.rs)):
die Zugangsdaten stehen in `~/.agentkit/config.json` (Windows: `%USERPROFILE%\.agentkit\`),
damit die installierte Executable überall läuft — nicht nur im Projektverzeichnis. Sie ist
die unterste Ebene einer Kette, jede darüber gewinnt:

```text
echte Umgebungsvariable  >  .env im Arbeitsverzeichnis  >  ~/.agentkit/config.json
```

Alle drei speisen dieselben Variablen (`AZURE_OPENAI_*` / `OPENAI_API_KEY` /
`OPENAI_BASE_URL`) — der Rest des Codes liest weiter nur die Umgebung. Platzhalter (`<…>`)
in der Config gelten als *nicht gesetzt*, eine frische Vorlage landet also sauber im
Demo-Modus.

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
| `--dry-run` | Führt den Loop aus, blockiert aber zerstörerische Schreib-/MCP-Vorgänge (Heuristik per Tool-Name) und loggt die versuchten Aktionen nur auf `stderr`. |
| `--max-context <TOKENS>` | Kontext-Limit (Default 128000); größer ⇒ Exit-Code 3. |
| `-p`/`--print` | One-shot: nur die finale Antwort auf `stdout`. |
| `--system <TEXT>` | System-Prompt; ERSETZT den eingebauten vollständig. |
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

Wichtig: `--system`/`--system-file`/`profile.system` **ERSETZEN** den eingebauten
System-Prompt — dein Text ist dann der ganze Prompt, ohne Coding-Anleitung, ohne
Shell-Hinweis, ohne Sub-Agenten-Block und ohne `AGENTKIT.md`. Es gibt genau einen
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
| `4` | Erzwungenes `--format` trotz Retries nicht erzeugbar. |

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
cargo build --release --manifest-path ../agentkit_app/Cargo.toml --bin agentkit --features pdf   # oder: --features "pdf tui"
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
inkl. `glob`/`grep`, Skills, Plan, das `task`-Tool für Sub-Agenten und das `swarm`-Tool,
mit dem der Agent sich zur Laufzeit einen ganzen Agenten-Schwarm baut (siehe
[`../agentkit_swarm`](../agentkit_swarm/README.md#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)). Da `ratatui`
das Terminal belegt, läuft die `run_shell`-Freigabe nicht über stdin, sondern über
einen **In-TUI-Dialog**; mit **Ctrl-Tab** (oder `Shift-Tab`) schaltet man zwischen
*Nachfragen* und *Auto-Freigabe* um — wie der Permission-Mode in der Claude-Code-CLI.

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
`--no-subagents`, `--max-steps`, `-y/--yes` (Freigabe initial auf AUTO), `--plan`/`--plain`
sowie `--ctx DIR`/`--ctx-budget N`/`--ctx-policy FILE`/`--ctx-compaction-model NAME`
(ctxman-Kontext-Management, Feature `ctxman` — beim Start bestätigt eine Verlaufs-Notiz
die Aktivierung samt Tokenizer; bei Resume weist sie auf die eingefrorene Policy hin).
Konfiguration wie im CLI (`.env` im Arbeitsverzeichnis, sonst `~/.agentkit/config.json`).
LLM-Auswahl (ohne `--demo`): `AZURE_OPENAI_*` → Azure, sonst `OPENAI_API_KEY` oder
`OPENAI_BASE_URL` (+ optional `OPENAI_MODEL`) → OpenAI bzw. lokaler OpenAI-kompatibler
Server, sonst der netzfreie **Demo-LLM**. MCP-Optionen (`--mcp-config`, `--mcp`, `--no-mcp`) gelten
auch hier; **F2** öffnet im UI das MCP-Panel zum Ein-/Ausschalten der Server. Tasten:
`Enter` senden, `Esc` abbrechen/beenden, `Ctrl-Tab` Freigabe-Modus umschalten, `F2`
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
