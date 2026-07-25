# agentkit-swarm

Ein Actor-basiertes Agent-to-Agent-System auf [`agentkit`](../agent_framework_rs). Kernprinzip:

> **Ein Schwarm-Agent ist ein normaler `agentkit::Agent`, der exklusiv von einem Actor besessen wird. Der Actor fügt Mailbox, Identität und Peer-Kommunikation hinzu, ohne den Agent-Loop zu verändern.**

Es gibt keinen zentralen Agenten, der die Zusammenarbeit semantisch steuert: Agenten kommunizieren peer-to-peer über Tools, jeder besitzt eine eigene Mailbox, keiner greift auf den Zustand eines anderen zu. Der Agent-Kern (`agent_framework_rs`) bleibt vollständig unverändert — `agentkit_swarm` hängt von `agentkit` ab, nie umgekehrt.

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

Nach `.agent(id, agent)` gehört der Agent der Laufzeit (Move-Semantik); ab `start()` besitzt ihn exklusiv sein Actor-Thread. Lauffähige Beispiele: `cargo run --example discussion_swarm` und `cargo run --example dev_team_swarm` (beide offline mit `FakeLlm`), `cargo run --example openai_swarm --features openai` (echtes LLM).

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

Statuswerte im Tool-Ergebnis (`DeliveryResult`): `zugestellt`, `postfach_voll` (retryable), `empfaenger_weg`, `nicht_erlaubt`, `limit_erreicht` — weiche Fehler als Werte, das Modell korrigiert sich selbst. Topologie ist als **Capability** umgesetzt: wer keinen `ActorRef` bekommen hat, kann nicht senden — stärker als jede Laufzeitprüfung.

## Completion & Limits

Der Schwarm endet deterministisch, nie „semantisch":

- **Konsens**: `CompletionPolicy::Consensus { required_approvals }` — der CompletionActor zählt zustimmende Votes je Proposal (doppelte Stimmen desselben Agenten zählen einfach). Votes und Proposal-Einreichung sind budgetfrei, damit der Abschluss nicht am Nachrichtenlimit scheitert.
- **Limits**: `max_messages` (globales Zustellbudget über einen geteilten Zähler, kein zentraler Router; gezählt werden nur **erfolgreiche** Zustellungen — abgewiesene, z. B. `postfach_voll`, werden erstattet), `max_hops` (Länge einer Reply-/Send-Kette), `max_runtime`, `mailbox_capacity` (Backpressure).
- **Fehler**: Panict ein Actor-Thread, stoppt der Supervisor den ganzen Schwarm kontrolliert (`ActorFailure`). Ein fehlgeschlagener *Turn* (Abbruch, kein Stream, `max_steps`) stoppt dagegen nichts: `TurnCompleted { success: false }`, der Actor verarbeitet die nächste Nachricht.
- **Dead Letters**: abgelehnte Zustellungen, beim Shutdown gedrainte Mailboxen und Votes auf unbekannte Proposals landen im `SwarmResult` — nichts geht stillschweigend verloren.

Deadlock-frei by construction: alle Sendepfade sind `try_send` (blockieren nie), volle Mailboxen erzeugen Dead Letters, und der Shutdown erreicht jeden Actor über den `recv_timeout`-Takt — selbst mit voller Mailbox. Einziger blockierender Send ist `send_initial` auf die garantiert leere Mailbox beim Kickoff.

## Bewusste Design-Entscheidungen

Dieses Crate ist **kein Port** — es gibt kein Python-/C#-Gegenstück, das Design stammt aus dem agentkit_swarm-Designdokument. Abweichungen und Festlegungen, die erklärungsbedürftig sind:

- **Threads & Channels trotz Guidelines §4.** Die Guidelines verbieten Nebenläufigkeit „auf Vorrat" — hier ist das Actor-Modell der konkrete heutige Bedarf: ein Actor = ein Thread = ein `&mut Agent` ist zugleich das Nicht-Reentranz-Argument (keine Mutexe um Agenten, keine verschachtelten Runs, deterministischer Verlauf pro Agent). Kein Tokio, kein Async-Framework: agentkit ist durchgehend synchron, ein async Actor-System müsste den Agenten permanent über `spawn_blocking` schieben.
- **Sentinel-basierte Turn-Fehlererkennung.** `Agent::run_on_bus` liefert `String`, kein `Result`; die Rückgaben `"(abgebrochen)"`, `"(keine Antwort)"` und `"(max_steps erreicht)"` sind agentkits stabile Verhaltenskontrakte. Exact-Match darauf ist die einfachste robuste Erkennung; das theoretische Restrisiko (ein Modell antwortet wortwörtlich so) ist akzeptiert. Die Alternative — ERROR-Events vom geteilten Agent-Bus nach `source` filtern — wäre schwerer und racy.
- **`recv_timeout`-Polling statt Sender-Drop als Shutdown-Signal.** Peer-Refs leben in den Tool-Closures der anderen Agenten; alle Sender zu droppen ist unmöglich, und ein `Shutdown`-Kommando passt nicht in eine volle Mailbox. Der 100-ms-Takt ist die langweilige Antwort, `Shutdown` bleibt der schnelle Pfad.
- **`default = []` statt `default = ["openai"]`** (Abweichung vom Designdokument): Repo-Konvention „offline by default" — `cargo test` läuft ohne HTTP/TLS-Abhängigkeiten; `openai`/`ctxman` werden nur an agentkit durchgereicht.
- **Deutsche Tool-Beschreibungen** (Abweichung vom Designdokument, das englische Texte skizziert): Sprachkonvention des Repos — alles Nutzersichtbare deutsch, Bezeichner englisch.
- **Kein `panic = "abort"`** im Release-Profil (anders als agentkits eigenes Profil): der Supervisor erkennt Actor-Panics über Thread-Unwinding.
- **`DeliveryResult::LimitReached`** als fünfte Variante (das Designdokument nennt vier): hält das Status-JSON ehrlich, statt das Limit unter `NotAllowed` zu verstecken.
- **Broadcast-Budget pro Zustellung**: `max_messages` deckelt echten Traffic, nicht logische Nachrichten — und nur erfolgreichen: fehlgeschlagene Zustellversuche geben ihr Budget zurück.

## Nicht im Umfang von v0.1

Tokio, Remote-Actors, Cluster, A2A-Protokoll, Actor-Restart mit Memory-Recovery, dynamische Topologie, Prioritäts-Mailboxen, Message-Persistenz, verteilte Konsens-Verfahren, mehrere gleichzeitige Turns pro Agent.

## Build & Test

```bash
cargo test  --manifest-path agentkit_swarm/Cargo.toml                       # offline, kein Netz
cargo build --manifest-path agentkit_swarm/Cargo.toml --features "openai ctxman"
cargo run   --manifest-path agentkit_swarm/Cargo.toml --example discussion_swarm
```
