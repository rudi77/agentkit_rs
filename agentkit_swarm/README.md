# agentkit-swarm

Ein Actor-basiertes Agent-to-Agent-System auf [`agentkit`](../agent_framework_rs). Kernprinzip:

> **Ein Schwarm-Agent ist ein normaler `agentkit::Agent`, der exklusiv von einem Actor besessen wird. Der Actor fügt Mailbox, Identität und Peer-Kommunikation hinzu, ohne den Agent-Loop zu verändern.**

Es gibt keinen zentralen Agenten, der die Zusammenarbeit semantisch steuert: Agenten kommunizieren peer-to-peer über Tools, jeder besitzt eine eigene Mailbox, keiner greift auf den Zustand eines anderen zu. Der Agent-Kern (`agent_framework_rs`) bleibt vollständig unverändert — `agentkit_swarm` hängt von `agentkit` ab, nie umgekehrt.

Ein Schwarm kann **statisch** in Rust gebaut werden (Schnellstart unten) oder **dynamisch zur Laufzeit** von einem Agenten selbst — dafür gibt es das `swarm`-Tool ([eigener Abschnitt](#dynamischer-schwarm-zur-laufzeit--das-swarm-tool)), das in der `agentkit`-Executable ab Werk verdrahtet ist.

## Architektur

```text
SwarmRuntime (Builder → Start → Monitor → Shutdown)
    │  erstellt Mailboxes, verteilt erlaubte ActorRefs, injiziert Tools
    ▼
AgentActor "architect"        AgentActor "developer"        AgentActor "tester"
├── Agent (LLM-Loop, Memory)  ├── Agent                     ├── Agent
├── Mailbox (bounded)         ├── Mailbox                   ├── Mailbox
└── erlaubte Peers ───────────┴──── swarm_send / reply ─────┘
                                     │
                 swarm_propose / swarm_vote
                                     ▼
                              CompletionActor (zählt Votes, kein LLM)

Zwei Event-Ebenen:  agentkit::EventBus   → AgentEvents (Steps, Tools, Deltas; source = Agent-ID)
                    SwarmEventBus        → SwarmEvents (Lifecycle, Zustellungen, Abschluss)
```

Ein Actor = ein OS-Thread = ein `&mut Agent`: pro Agent wird immer genau eine Nachricht gleichzeitig verarbeitet (nicht reentrant). Neue Nachrichten sammeln sich währenddessen in der Mailbox. Die Agent-Memory persistiert über alle Nachrichten — jeder Turn ist für den Agenten einfach die nächste User-Nachricht im `[SWARM MESSAGE]`-Format.

## Schnellstart

```rust
use agentkit_swarm::{CompletionPolicy, SwarmBuilder};

let handle = SwarmBuilder::new("coding-swarm")
    .agent("architect", architect)          // fertige agentkit::Agents (Move!)
    .agent("developer", developer)
    .agent("tester", tester)
    .connect_bidirectional("architect", "developer")
    .connect_bidirectional("developer", "tester")
    .completion(CompletionPolicy::Consensus { required_approvals: 2 })
    .mailbox_capacity(32)
    .max_messages(100)
    .build()?
    .start()?;

handle.send_initial("architect", "Finde und behebe die Race Condition im MCP-Client.")?;
let result = handle.join();                 // blockiert bis Konsens/Limit/Fehler
```

Nach `.agent(id, agent)` gehört der Agent der Laufzeit (Move-Semantik); ab `start()` besitzt ihn exklusiv sein Actor-Thread.

Lauffähige Beispiele liegen in [`examples/`](examples/README.md) — alle offline, ohne Key. Wer wissen will, **wann** sich ein Schwarm überhaupt lohnt, fängt bei diesen dreien an:

| Beispiel | Grund für einen Schwarm |
|---|---|
| `parallel_research_swarm` | **Zeit** — drei Spezialisten arbeiten echt gleichzeitig (ein Actor = ein OS-Thread); das Beispiel misst 1350 ms Arbeit in ~600 ms Wanduhrzeit |
| `red_team_swarm` | **Qualität** — der Abschluss hängt an einem Quorum, das der Autor nicht selbst öffnen kann: die erste Fassung fällt durch |
| `codemod_swarm` | **Durchsatz** — echte Datei-Werkzeuge im geteilten Workspace, disjunkt aufgeteilt, mit eigenem Prüfer |

Dazu die Mechanik-Beispiele: `discussion_swarm` (Broadcast & Konsens), `dev_team_swarm` (Ketten-Topologie), `dynamic_swarm` (das `swarm`-Tool) und `openai_swarm --features openai` (echtes LLM).

## Nachrichtenmodell & Tools

Der Agent sieht den Schwarm ausschließlich über sechs Tools; die `AgentActorRef`s stecken in den Tool-Closures. `from` setzt immer die Closure — ein Agent kann sich nicht als anderer ausgeben. Alle Sendepfade sind fire-and-forget: das Tool liefert sofort ein Status-JSON, Antworten treffen später als neue Nachrichten ein.

| Tool | Zweck |
|---|---|
| `swarm_peers` | Erreichbare Nachbarn auflisten |
| `swarm_send` | Nachricht an einen Nachbarn (`to`, `content`, `kind?`, `correlation_id?`) |
| `swarm_reply` | Dem Absender der gerade bearbeiteten Nachricht antworten |
| `swarm_broadcast` | Nachricht an alle Nachbarn (jede Zustellung zählt gegen das Limit) |
| `swarm_propose` | Abschluss vorschlagen (geht an Peers **und** CompletionActor) |
| `swarm_vote` | Über einen Vorschlag abstimmen (geht **nur** an den CompletionActor) |

Statuswerte im Tool-Ergebnis (`DeliveryResult`): `zugestellt`, `postfach_voll` (retryable), `empfaenger_weg`, `nicht_erlaubt`, `limit_erreicht`, `schwarm_beendet` — weiche Fehler als Werte, das Modell korrigiert sich selbst. Bei `limit_erreicht` kommt ein `hinweis` dazu, der auf den budgetfreien Abschluss über `swarm_propose` verweist. Topologie ist als **Capability** umgesetzt: wer keinen `ActorRef` bekommen hat, kann nicht senden — stärker als jede Laufzeitprüfung.

Sonderfall Kickoff: die Initialaufgabe kommt von der Laufzeit (`from: "runtime"`), und die steht in keiner PeerDirectory — ein `swarm_reply` darauf *kann* nicht ankommen. Der gerenderte Prompt der Initialaufgabe nennt deshalb `swarm_send`/`swarm_broadcast`/`swarm_propose` statt `swarm_reply`, und ein Reply darauf liefert den erklärenden Status `initialaufgabe_ohne_absender` statt eines irreführenden `nicht_erlaubt`.

## Completion & Limits

Der Schwarm endet deterministisch, nie „semantisch":

- **Konsens**: `CompletionPolicy::Consensus { required_approvals }` — der CompletionActor zählt zustimmende Votes je Proposal (doppelte Stimmen desselben Agenten zählen einfach). Votes und Proposal-Einreichung sind budgetfrei, damit der Abschluss nicht am Nachrichtenlimit scheitert.
- **Limits**: `max_messages` (globales Zustellbudget über einen geteilten Zähler, kein zentraler Router; gezählt werden nur **erfolgreiche** Zustellungen — abgewiesene, z. B. `postfach_voll`, werden erstattet), `max_hops` (Länge einer Reply-/Send-Kette), `mailbox_capacity` (Backpressure).
- **Keine Laufzeitgrenze im Default — stattdessen Leerlauf.** `max_runtime` ist optional und ab Werk AUS: ein Schwarm darf beliebig lange arbeiten, solange er arbeitet. Die knappe Ressource ist das Modell-Kontingent, nicht die Wanduhr. Der Hänge-Schutz ist `max_idle` (Default 300 s): verstummen die Mitglieder, ohne vorzuschlagen, greift sonst kein Abschluss — kein Konsens, kein erschöpftes Budget — und `join()` wartete ewig. Maßgeblich ist dabei „nichts in Arbeit UND kein Event": ein Mitglied im LLM-Stream erzeugt keine Schwarm-Events, ein reiner Stille-Detektor hielte es fälschlich für untätig. Wer eine harte Frist braucht, setzt `max_runtime` bzw. `max_laufzeit_s` weiterhin selbst.
- **Erschöpftes Nachrichtenbudget ist eine Bremse, kein Not-Aus**: die erste Zustellung über Budget liefert `limit_erreicht` und stoppt weitere Sends, beendet den Schwarm aber nicht. Der Monitor wartet, bis das bereits Zugestellte abgearbeitet ist (Bilanz aus `MessageQueued`/`TurnCompleted`) — bis dahin kann der Schwarm über die budgetfreien `swarm_propose`/`swarm_vote` noch regulär per Konsens enden. Erst wenn nichts mehr aussteht, meldet er `MessageLimitReached`; hängt ein Turn dauerhaft, greift `max_runtime` — sofern gesetzt (siehe oben), sonst bleibt der Stop-Knopf.
- **Fehler**: Panict ein Actor-Thread, stoppt der Supervisor den ganzen Schwarm kontrolliert (`ActorFailure`). Ein fehlgeschlagener *Turn* (Abbruch, kein Stream, `max_steps`) stoppt dagegen nichts: `TurnCompleted { success: false }`, der Actor verarbeitet die nächste Nachricht.
- **Dead Letters**: abgelehnte Zustellungen, beim Shutdown gedrainte Mailboxen und Votes auf unbekannte Proposals landen im `SwarmResult` — nichts geht stillschweigend verloren. Sends NACH dem Abschluss sind davon ausgenommen: sie liefern `schwarm_beendet` und gelten nicht als Fehlzustellung. Ein Mitglied, das seinen Turn zu Ende bringt, während der Konsens schon steht, hat nichts falsch gemacht.
- **Ausklingen statt Abschneiden**: ein PLANMÄSSIGES Ende (Konsens, Nachrichtenlimit) lässt laufende Turns bis zu `QUIESCE_GRACE` (20 s) zu Ende laufen, bevor abgebrochen wird. Vorher riss der Konsens jeden gerade laufenden LLM-Stream mitten im Satz ab; im Trace stand ein „⛔ abgebrochen" direkt neben dem Erfolg und sah aus wie ein Fehler. Ein Abbruch durch den Nutzer (`stop`) und ein Actor-Absturz klingen NICHT aus — dort ist Sofortigkeit der Zweck.

Deadlock-frei by construction: alle Sendepfade sind `try_send` (blockieren nie), volle Mailboxen erzeugen Dead Letters, und der Shutdown erreicht jeden Actor über den `recv_timeout`-Takt — selbst mit voller Mailbox. Einziger blockierender Send ist `send_initial` auf die garantiert leere Mailbox beim Kickoff.

## Dynamischer Schwarm zur Laufzeit — das `swarm`-Tool

Der Schnellstart oben ist die *statische* Variante: der Schwarm steht im Rust-Code. Mit
`add_swarm_tool` bekommt ein ganz normaler agentkit-Agent stattdessen **ein Tool**, mit dem
er sich seinen Schwarm zur Laufzeit selbst baut — Mitglieder, System-Prompts, Tool-Zugriff,
Topologie und Quorum kommen aus dem Modell. In der Executable ist das ab Werk verdrahtet:

```bash
agentkit --tui        # der Agent im TUI kann jetzt Schwärme erzeugen
agentkit --no-swarm   # … oder eben nicht
```

Verdrahtung als Bibliothek (siehe `../agentkit_app/src/lib.rs`):

```rust
use agentkit_swarm::{add_swarm_tool, SwarmLimits, SwarmToolConfig};

add_swarm_tool(&mut registry, SwarmToolConfig {
    run: run.clone(),          // Lauf-Kontext des Orchestrators: Bus + Stop-Knopf
    llm: llm.clone(),          // ein Modell für alle Mitglieder
    workspace: ws.to_string(),
    approve: Some(approve),    // Freigabe-Callback für run_shell
    shell_timeout: 120,
    skills: None,
    roles: Vec::new(),         // vordefinierte Rollen, im Spec per Name referenzierbar
    mcp: mcp.clone(),
    limits: SwarmLimits::default(),
});
```

Registrierung **vor** `AgentBuilder::build()`, und derselbe `RunHandle` an den Builder —
sonst sieht das Tool zur Laufzeit weder Bus noch Stop-Knopf (agentkits `RunHandle`-Vertrag).

Was das Modell schreibt (Tool-Argumente, gekürzt):

```json
{
  "auftrag": "Legt das Retry-Verhalten fest und einigt euch auf eine Empfehlung.",
  "topologie": "kette",
  "agenten": [
    {"id": "architekt", "system": "Du entwirfst die Lösung."},
    {"id": "kritiker",  "system": "Du suchst Schwachstellen.", "tools": "read_only"}
  ],
  "erforderliche_zustimmungen": 1
}
```

| Feld | Bedeutung |
|---|---|
| `auftrag` | Mission des Schwarms; geht als Initialaufgabe an `start_agent` (Default: erstes Mitglied) |
| `agenten[].system` / `.rolle` | eigener System-Prompt oder Name einer vordefinierten Rolle (`--agents DIR`); eigener Prompt gewinnt |
| `agenten[].tools` | `read_only` (**Default**), `alle` oder Komma-Liste — dieselbe Schreibweise wie im Rollen-Markdown |
| `agenten[].strategie`, `.skills` | Strategie (Default `react`) bzw. Skills-Werkzeuge dazugeben |
| `topologie` / `verbindungen` | `mesh` (Default), `kette`, `stern` — oder explizite Paare, die das Preset schlagen |
| `erforderliche_zustimmungen` | Quorum; **Obergrenze** sind die Nachbarn des am schwächsten verbundenen Mitglieds (ein Vorschlag geht nur an direkte Nachbarn — bei `mesh` also alle anderen, bei `kette`/`stern` weniger). **Default** ist die Mehrheit davon, nicht das Maximum: im Mesh wäre das Maximum Einstimmigkeit, und ein einziges enthaltenes Mitglied hätte jeden Konsens verhindert |
| `max_nachrichten`, `max_laufzeit_s` | eigene Limits, gedeckelt auf `SwarmLimits`; Default von `max_nachrichten` sind **40 Zustellungen je Mitglied** (ein Broadcast kostet eine Zustellung pro Nachbar, im Mesh also n-1 je Turn — ein fester Wert hätte mit jedem Mitglied weniger Turns erlaubt) |

Der Aufruf **blockiert** bis zum Abschluss und liefert deutsches JSON zurück:
`{"status":"konsens","ergebnis":"…","zustimmungen":1,"turns":{…},"nachrichten":4,"unzustellbar":0}` —
bzw. `nachrichtenlimit`, `laufzeitlimit`, `actor_fehler`, `abgebrochen` samt `hinweis`.
Fehlerhafte Spezifikationen sind **weiche Fehler** (`ERROR: …`), das Modell korrigiert sich selbst.

Grenzen, die nicht verhandelbar sind (`SwarmLimits`, Default): höchstens 6 Mitglieder,
höchstens 300 Zustellungen (Regelwert: 40 je Mitglied), 900 s Laufzeit, 40 Loop-Schritte
je Mitglied. **Keine Rekursion:** die
Registry eines Mitglieds entsteht von Grund auf aus den Coding-Tools und enthält weder
`swarm` noch `task` — dieselbe Invariante wie bei agentkits Sub-Agenten. Alle Mitglieder
teilen sich **einen** Workspace; der `read_only`-Default ist die Antwort darauf.

Lauffähig ohne Netz: `cargo run --example dynamic_swarm`.

## Bewusste Design-Entscheidungen

Dieses Crate ist **kein Port** — es gibt kein Python-/C#-Gegenstück, das Design stammt aus dem agentkit_swarm-Designdokument. Abweichungen und Festlegungen, die erklärungsbedürftig sind:

- **Threads & Channels trotz Guidelines §4.** Die Guidelines verbieten Nebenläufigkeit „auf Vorrat" — hier ist das Actor-Modell der konkrete heutige Bedarf: ein Actor = ein Thread = ein `&mut Agent` ist zugleich das Nicht-Reentranz-Argument (keine Mutexe um Agenten, keine verschachtelten Runs, deterministischer Verlauf pro Agent). Kein Tokio, kein Async-Framework: agentkit ist durchgehend synchron, ein async Actor-System müsste den Agenten permanent über `spawn_blocking` schieben.
- **Sentinel-basierte Turn-Fehlererkennung.** `Agent::run_on_bus` liefert `String`, kein `Result`; die Rückgaben `"(abgebrochen)"`, `"(keine Antwort)"` und `"(max_steps erreicht)"` sind agentkits stabile Verhaltenskontrakte. Exact-Match darauf ist die einfachste robuste Erkennung; das theoretische Restrisiko (ein Modell antwortet wortwörtlich so) ist akzeptiert. Die Alternative — ERROR-Events vom geteilten Agent-Bus nach `source` filtern — wäre schwerer und racy.
- **`recv_timeout`-Polling statt Sender-Drop als Shutdown-Signal.** Peer-Refs leben in den Tool-Closures der anderen Agenten; alle Sender zu droppen ist unmöglich, und ein `Shutdown`-Kommando passt nicht in eine volle Mailbox. Der 100-ms-Takt ist die langweilige Antwort, `Shutdown` bleibt der schnelle Pfad.
- **`default = []` statt `default = ["openai"]`** (Abweichung vom Designdokument): Repo-Konvention „offline by default" — `cargo test` läuft ohne HTTP/TLS-Abhängigkeiten; `openai`/`ctxman` werden nur an agentkit durchgereicht.
- **Deutsche Tool-Beschreibungen** (Abweichung vom Designdokument, das englische Texte skizziert): Sprachkonvention des Repos — alles Nutzersichtbare deutsch, Bezeichner englisch.
- **Kein `panic = "abort"`** im Release-Profil (anders als agentkits eigenes Profil): der Supervisor erkennt Actor-Panics über Thread-Unwinding.
- **`DeliveryResult::LimitReached`** als fünfte Variante (das Designdokument nennt vier): hält das Status-JSON ehrlich, statt das Limit unter `NotAllowed` zu verstecken.
- **Broadcast-Budget pro Zustellung**: `max_messages` deckelt echten Traffic, nicht logische Nachrichten — und nur erfolgreichen: fehlgeschlagene Zustellversuche geben ihr Budget zurück. Weil die Einheit damit eine Zustellung ist und nicht eine Konversation, muss der *Default* mit der Mitgliederzahl skalieren (`MESSAGES_PER_AGENT = 40`): im Mesh kostet jeder Turn n-1, ein fester Wert hätte den Spielraum je Mitglied mit jedem zusätzlichen Mitglied gedrittelt. Die harte Obergrenze (`SwarmLimits::max_messages`) bleibt fest.
- **Erschöpftes Budget bremst, es tötet nicht.** Die erste Zustellung über Budget setzt nur eine Flagge; der Monitor beendet den Schwarm erst, wenn nichts Zugestelltes mehr aussteht. Vorher publizierte der Sendepfad sofort `SwarmCompleted{MessageLimitReached}` — damit brach der Shutdown jeden laufenden Turn ab und verwarf alle Mailboxen als Dead Letters, also genau die Arbeit, die das Budget gerade bezahlt hatte, und der Orchestrator bekam statt eines Ergebnisses einen Hinweistext. Ausstehende Arbeit wird über die Bilanz `MessageQueued` minus `TurnCompleted` gezählt (abgewiesene Zustellungen publizieren beides nicht, die Bilanz geht auf); ein hängender Turn läuft in `max_runtime`.
- **Peer-Kopien eines `swarm_propose` sind budgetfrei**, nicht nur der Weg zum CompletionActor. Abstimmen kann nur, wer den Vorschlag sieht — schluckte das Limit die Kopien, wäre der Abschluss ausgerechnet am Limit unmöglich. Missbrauch bleibt begrenzt: jede Kopie muss weiterhin in eine endliche Mailbox passen, und `max_runtime` ist die äußere Schranke.
- **Quorum über den kleinsten Knotengrad statt je Vorschlagendem, und die Mehrheit davon statt des Maximums.** Abstimmen kann nur, wer einen Vorschlag sieht, und `swarm_propose` stellt ihn ausschließlich den direkten Nachbarn zu. Die *Obergrenze* ist deshalb der kleinste Knotengrad des Graphen — so ist das Quorum erfüllbar, ganz gleich wer vorschlägt, und ein Schwarm läuft nie stumm bis zur Laufzeitgrenze. Der *Default* ist die Mehrheit davon: im Mesh ist der Knotengrad `n-1`, das Maximum als Default hieß also Einstimmigkeit, und ein einziges Mitglied, das sich enthält oder gerade in einem langen Turn steckt, machte jeden Konsens unmöglich — der Schwarm lief zwangsläufig in ein Limit statt in ein Ergebnis. Einstimmigkeit bleibt über `erforderliche_zustimmungen` anforderbar. Bewusst in Kauf genommen: bei `stern` gilt für den gut verbundenen Nabe dieselbe niedrige Schwelle wie für ein Blatt, und ein per `verbindungen` isoliertes Mitglied (Grad 0) senkt das Quorum für alle auf 0. Die genaue Variante wäre ein Quorum je Vorschlagendem im CompletionActor (dem `msg.from` vorliegt); das kostet einen Grad-Map-Durchstich durch die Completion-Policy und ist erst nötig, wenn jemand nicht-vermaschte Topologien wirklich fährt.
- **Die Initialaufgabe ist kein Reply-Ziel.** `send_initial` setzt `from: "runtime"`, und die Laufzeit steht in keiner `PeerDirectory` — ein `swarm_reply` darauf kann strukturell nicht ankommen. Das Mitgliedsprotokoll verlangt aber „antworte nie ins Leere"; der Kickoff führte das Startmitglied damit ausgerechnet bei seiner ersten Nachricht in den einen Sendepfad, der nicht funktionieren *kann*. Statt „runtime" künstlich zu einem Peer zu machen (das hieße: ein Mitglied könnte an die Laufzeit senden, die niemanden hat, der zuhört), rendert der Kickoff einen eigenen Hinweis und `swarm_reply` liefert darauf den erklärenden Status `initialaufgabe_ohne_absender`.
- **`join_with_cancel` statt eines zweiten Abbruchpfads.** Das `swarm`-Tool blockiert im Tool-Thread des Orchestrators; der Stop-Knopf des Nutzers (Esc im TUI) muss dort ankommen. Die Prüfung sitzt im vorhandenen 100-ms-Takt des Monitors — vor Laufzeit- und Fehlerprüfung, damit ein extern gestoppter Schwarm `Stopped` meldet und nicht zufällig `MaxRuntimeReached`. `join()` ist derselbe Loop mit `None`.
- **Schwarm-Verkehr wird auf VORHANDENE `AgentEvent`-Typen gespiegelt** (`tool_result` namens `"schwarm"`, `source` = Agent-ID), statt einen neuen Event-Typ einzuführen. Ein neuer Typ ist in agentkit ein Verhaltenskontrakt: er zöge Änderungen im CLI-`Renderer` und im TUI nach sich, obwohl ein Tool-Ergebnis dieselbe Information trägt und im TUI schon richtig gerendert wird (`[coder] ↳ schwarm: …`).
- **`read_only` als Tool-Default der Mitglieder** — Abweichung von agentkits Rollen-Semantik, wo „kein `tools`-Feld" *alle* Tools bedeutet. Ein vom Modell erfundener Agent soll Schreibrechte nur bekommen, wenn sie ausdrücklich verlangt wurden; eine benannte Rolle bringt weiterhin ihre eigene Teilmenge mit.
- **`SwarmResult` bleibt serde-frei.** Das `swarm`-Tool formatiert sein Ergebnis-JSON selbst. Ein `Serialize` auf den Laufzeit-Typen hätte heute genau einen Nutzer und würde sie an ein Ausgabeformat binden.
- **`SwarmToolConfig::extra_member_tools` statt Vererbung der Orchestrator-Registry.** Ein dynamisch erzeugtes Mitglied baut seine Registry in `build_member` von Grund auf aus `CodingTools` — das ist die Nicht-Rekursions-Invariante (kein `swarm`, kein `task` in Mitgliedern) und bleibt so. Damit erbt ein Mitglied aber auch die *Frontend*-Tools des Erzeugers nicht, und mit dem Wissensgraphen (`agentkit-graph`) gab es erstmals eine Fähigkeit, die genau dort hingehört: ein Schwarm, dessen Erzeuger ein gemeinsames Gedächtnis hat, dessen Mitglieder aber nicht, ist das Gegenteil von dem, wofür ein Schwarm da ist. Die Naht ist deshalb eine Closure `(&mut ToolRegistry, &str)` — der Schwarm weiß nicht, was registriert wird, genauso wenig wie agentkit den Schwarm kennt. Die Agent-ID geht mit, damit ein Tool den echten Autor kennt (dasselbe Prinzip wie `from` bei `swarm_send`); aufgerufen wird sie VOR `dry_run_blocking`, damit `--dry-run` auch für sie gilt.
- **Eine eigene Testdatei `tests/dynamic.rs`** (statt alles in `integration.rs`): sie bringt mit `PerAgentLlm` ein eigenes Test-Modell mit, das je Agent-ID skriptet — `FakeLlm` zählt Turns global, was bei nebenläufigen Actors nicht deterministisch ist. Erkannt wird der Fragesteller an der Zeile „Deine Agent-ID ist '…'" aus seinem System-Prompt; das braucht **keine** Test-Naht im Produktivcode.

## Nicht im Umfang

Tokio, Remote-Actors, Cluster, A2A-Protokoll, Actor-Restart mit Memory-Recovery,
Prioritäts-Mailboxen, Message-Persistenz, verteilte Konsens-Verfahren, mehrere gleichzeitige
Turns pro Agent.

**Dynamische Topologie** heißt hier weiterhin: ein *laufender* Schwarm ändert seine Topologie
nicht — die `PeerDirectory` jedes Agenten wird beim Start in Capabilities übersetzt und danach
nie neu gelesen. Dass pro `swarm`-Tool-Aufruf ein frisch spezifizierter Schwarm entsteht, ist
davon unberührt und ausdrücklich vorgesehen.

## Build & Test

```bash
cargo test  --manifest-path agentkit_swarm/Cargo.toml                       # offline, kein Netz
cargo test  --manifest-path agentkit_swarm/Cargo.toml --test dynamic        # nur das swarm-Tool
cargo build --manifest-path agentkit_swarm/Cargo.toml --features "openai ctxman"
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example parallel_research_swarm
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example red_team_swarm
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example codemod_swarm
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example discussion_swarm
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example dynamic_swarm
```

Alle Beispiele und die Entscheidungshilfe „wann Schwarm, wann Sub-Agenten, wann Pipeline": [`examples/README.md`](examples/README.md).
