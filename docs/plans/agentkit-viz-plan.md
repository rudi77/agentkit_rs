# Implementierungsplan: `agentkit viz` — Beobachtbarkeit und ein Debug-Werkzeug

## Context

`agentkit_work` ist fertig und in `main` (Phasen 1–8, 240 Tests). Jetzt soll das Vorhandene **intensiv getestet und debuggt** werden — und dafür fehlt das Instrument. Heute ist ein Lauf eine Blackbox: was im Kontext eines Agenten steht, was Schwarm-Mitglieder sich zugerufen haben, warum ein Work Item scheiterte, sieht man nur an menschenlesbaren stderr-Zeilen, die nach dem Lauf weg sind.

Ziel: ein Web-Werkzeug, das fünf Dinge zeigt — Kontext je Agent, Verlauf je Agent, Schwarm-Kommunikation, Wissensgraph, Work-Zustand — live während eines Laufs und nachträglich aus Dateien. Es ist ausdrücklich ein **Debug-Werkzeug**, kein Produkt: localhost, keine Auth, kein Multi-User.

## Ist schon alles da? Nein — aber die Lücke ist klein und scharf umrissen

Die Erkundung hat einen Kanal gefunden, auf dem fast alles schon zusammenläuft:

- `agentkit_swarm/src/dynamic.rs::forward_swarm_events` leitet **Schwarm-Ereignisse bereits in den `EventBus` des Orchestrators** — aber durch `swarm_event_line` plattgemacht zu einer Textzeile in `EventData::ToolResult`. Wer schickte wem was, welcher `MessageKind`, welche Message-ID: alles verloren.
- Sub-Agenten (`task`) und Schwarm-Mitglieder publizieren ihre `AgentEvent`s ohnehin in denselben Bus, getaggt über `source` (`dynamic.rs:898` gibt den Orchestrator-Bus als `agent_bus` weiter).

Es braucht also **keinen neuen Transportweg**, sondern nur: den Bus serialisierbar in eine Datei schreiben, und die Schwarm-Ereignisse strukturiert statt platt hineinlegen.

| Ansicht | Stand | Lücke |
|---|---|---|
| **Work** | ✅ fertig | keine. `WorkStore::open_read_only` (sperrfrei, genau dafür gebaut), `work.jsonl`, vier `--format json`-Kommandos |
| **Graph** | 🟡 Daten vollständig serde in `graph.jsonl`; `GraphIndex::entities()/claims()/episodes()` traversierbar | `GraphStore::open` kann beim Öffnen kompaktieren (schreibt!) → kein nebenwirkungsfreies Lesen; keine `sources()`-Iteration; `Subgraph`/`render()` liefern nur Text, kein JSON |
| **Verlauf je Agent** | 🟡 Session-JSON = volles ReAct-Protokoll | wird erst **nach Abschluss eines ganzen Zugs** geschrieben → nicht live |
| **Kontext je Agent** | 🟡 nur mit `--ctx`: ctxman-`snapshot.json` mit Segmenten, Zustand, Inhalten | ohne ctxman ist `ContextReport` nicht serde und nur prozessintern; `Agent.memory` ist aber `pub` → dumpbar |
| **Schwarm-Kommunikation** | ❌ | `SwarmEvent`/`SwarmResult` haben kein serde, werden nirgends persistiert; Struktur geht in `forward_swarm_events` verloren |
| **Live-Kanal überhaupt** | ❌ | `EventBus` ist `std::sync::mpsc`, `AgentEvent` hat kein serde, kein NDJSON-Trace |

## Entschiedene Architektur

**Ein Kanal, ein Sink.** Alles fließt über den vorhandenen `EventBus`; ein Trace-Sink schreibt ihn als NDJSON. Kein zweiter Weg, keine IPC-Erfindung.

**Der Kern lernt den Schwarm nicht kennen.** Damit strukturierte Fremd-Nutzlast über den Bus kann, bekommt `EventData` eine generische Variante:

```rust
/// Strukturierte Nutzlast eines Frontends/Erweiterungs-Crates. Der Kern kennt
/// den Inhalt bewusst nicht — `kind` benennt ihn, `payload` trägt ihn. So kann
/// agentkit-swarm seine Ereignisse verlustfrei über den Bus schicken, ohne dass
/// der Agent-Kern den Schwarm kennt (Abhängigkeitsrichtung).
Structured { kind: String, payload: serde_json::Value },
```

`serde_json` ist im Kern schon Dependency (Tool-Argumente sind `Value`). Laut `agent_framework_rs/CLAUDE.md` ist genau das der vorgesehene Weg, ein Ereignis hinzuzufügen: Konstante + `EventData`-Variante, dann Renderer und TUI (der Compiler zeigt beide Stellen).

**Nur `Serialize`, kein `Deserialize` im Kern.** `AgentEvent::etype` ist `&'static str` und wäre nicht deserialisierbar. Der Viewer definiert eigene Spiegeltypen mit `String`. Das hält den Kern unangetastet und den Viewer unabhängig vom Kern-Release.

**Der Viewer ist ein eigenes Crate**, das nichts in den Kern zurückspiegelt. Hexagonal: Domäne (Trace-Modell, Projektionen) kennt weder HTTP noch Dateisystem; Adapter außen (`fs`-Leser, HTTP-Server).

**Eine neue Dependency, bewusst und eingehegt:** `tiny_http` im Viz-Crate (synchron, kein tokio, kein TLS). Ein HTTP/1.1-Server mit SSE von Hand ist machbar, aber ~250 Zeilen Fehlerquelle für ein Debug-Werkzeug. Die Dependency landet **nie** im Kern und **nie** in den Release-Binaries — das Viz-Crate hängt an einem eigenen Feature.

## Sicherheit — ausdrücklich, weil es hier wehtut

Ein Trace enthält **alles, was der Agent gelesen und geschrieben hat**: Dateiinhalte, Shell-Ausgaben, Modellantworten. In einem Repo mit `.env` heißt das: potenziell Secrets im Klartext.

- Der Trace entsteht **nur** mit `--trace DIR`, niemals von sich aus.
- Ablage unter `<workspace>/.agentkit/trace/`, und `create` legt dort eine `.gitignore` mit `*` an — dieselbe Idee wie beim Journal.
- Beim ersten Schreiben eine **Warnung auf stderr**, die sagt, was in der Datei landet.
- Der Viz-Server bindet **ausschließlich** an `127.0.0.1`, mit einem beim Start erzeugten Zufalls-Token in der URL. Er liefert Dateiinhalte aus — ohne das wäre er ein Leseloch ins Repo.
- Kein Redaktions-Versprechen. Redaktion wäre nie vollständig zuverlässig; die Warnung ist ehrlicher als ein Filter, dem man vertraut.

## Phase 1 — Der Trace-Kanal (das Instrument)

**`agent_framework_rs`**
- `src/events.rs`: `#[derive(Serialize)]` auf `AgentEvent`/`EventData`; neue Variante `Structured { kind, payload }` + Konstante `STRUCTURED`. `planning::Step` braucht dafür ebenfalls `Serialize`.
- Neu `src/trace.rs`: `TraceWriter` — öffnet eine NDJSON-Datei, `write_event(&AgentEvent)`, `write_record(kind, &Value)`. Synchron, `write_all` + `flush`, eine Zeile je Ereignis mit `seq`, `at_ms`, `source`. Große Nutzlasten (Tool-Ergebnisse) ab einer Konstante gekürzt, mit Vermerk der Originalgröße — ein Trace darf nicht größer werden als das Repo. Muster für Zeilenform und Robustheit: `agentkit_work/src/store/journal.rs`.
- `Renderer` (CLI) und `src/tui.rs` behandeln `Structured` (kompakte Zeile, damit der Compiler-Zwang aus CLAUDE.md erfüllt ist).

**`agentkit_app/src/bin/agentkit.rs`**
- Neues Flag `--trace DIR`. Der vorhandene Event-Callback schreibt zusätzlich in den `TraceWriter` — dort laufen Haupt-Agent, Sub-Agenten und Schwarm-Mitglieder schon zusammen.
- Nach jedem Zug ein Kontext-Datensatz: `Agent.memory` ist `pub` (`agent.rs`), also `messages` plus `context_report(&agent)` als `Structured { kind: "context_snapshot", … }`. Das ist die Antwort auf „was steht im Kontext dieses Agenten" **ohne** dass `--ctx` nötig ist. Für Sub-Agenten und Schwarm-Mitglieder gibt es keinen Zugriff auf deren `memory` — ihr Kontext wird im Viewer aus dem Ereignisstrom rekonstruiert; das ist eine dokumentierte Grenze, keine Lücke, die stillschweigend bleibt.
- `ContextReport`/`ContextSegment` in `app.rs` bekommen `Serialize`.

**`agentkit_swarm`**
- `Serialize` auf `SwarmEvent`, `DeliveryResult`, `CompletionReason`, `SwarmResult`, `ProposalOutcome`, `DeadLetter` (`SwarmMessage` hat es schon).
- `forward_swarm_events` publiziert **zusätzlich** zur bisherigen Textzeile ein `Structured { kind: "swarm_event", payload }` mit dem serialisierten Ereignis. Die Textzeile bleibt für den menschlichen Renderer — die Struktur ist für den Trace. Am Ende ebenso ein `swarm_result`-Datensatz.
- `agentkit_app/src/work_swarm.rs`: die dokumentierte MVP-Grenze („Mitglieder-Events gehen verloren") auflösen, indem der Orchestrator-Bus durchgereicht wird — sonst ist ein Schwarm-Work-Item im Viewer blind.

**Tests:** Trace-Roundtrip (Ereignisse rein, NDJSON-Zeilen raus, wieder parsbar), Kürzung großer Nutzlasten, `Structured` überlebt Serialisierung, Schwarm-Ereignis verlustfrei im Trace (Regressionsschutz gegen die platte Textzeile), `.gitignore` wird angelegt, ohne `--trace` entsteht keine Datei. Alles offline mit `FakeLlm`.

## Phase 2 — `agentkit viz`: Server und die drei fertigen Ansichten

Neues Crate `agentkit_viz/` (Muster: `agentkit_graph`/`agentkit_work` — eigenes Manifest, `default = []`, offline testbar).

```text
agentkit_viz/src/
  lib.rs
  model.rs        Spiegeltypen des Trace (owned String) + Deserialize
  trace.rs        NDJSON lesen, tailen (Offset-basiert, wie `events --tail`)
  project.rs      Projektionen: Agenten-Liste, Verlauf je Agent, Kontext je Agent, Zeitleiste
  api.rs          JSON-Endpunkte (reine Funktionen Request→Value, ohne HTTP)
  server.rs       tiny_http-Adapter: Routing, SSE, statische Assets
  assets/         index.html, app.js, style.css (per include_str! eingebettet)
```

Endpunkte, alle read-only:
`GET /api/runs` · `/api/agents` · `/api/agents/<id>/history` · `/api/agents/<id>/context` · `/api/timeline` · `/api/work/<projekt>` (über `WorkStore::open_read_only`) · `/api/events` (SSE, tailt den Trace)

Frontend bewusst schlicht: **kein npm-Toolchain**, kein Framework. Eine `index.html`, eine `app.js` mit `fetch` + `EventSource`. Drei Ansichten in dieser Phase:
1. **Agenten-Liste** mit Live-Status, aus `source`-Tags gruppiert.
2. **Verlauf je Agent** — Nachrichtenkette mit Tool-Aufrufen und -Ergebnissen, aufklappbar.
3. **Kontext je Agent** — Segmente mit Tokenzahlen aus `context_snapshot`; mit `--ctx` zusätzlich der ctxman-Zustand (Live/Externalized/Compacted) aus `snapshot.json`.
4. **Work-Board** — Items, Zustände, Versuche, Budget, Zeitleiste. Die Daten sind fertig, das ist reine Anzeige.

CLI-Anbindung: `agentkit viz [--trace DIR] [--work DIR] [--port N] [--open]` als verb-first-Dispatch in `agentkit_app/src/bin/agentkit.rs`, hinter Feature `viz`, in derselben Machart wie `work` (inklusive der Meldung „ohne Feature gebaut"). Die vier Completion-Generatoren und `cli_help_text()` nachziehen.

**Tests:** Projektionen gegen einen Beispiel-Trace (Fixture-Datei, kein Netz), Tailen liefert nur neue Zeilen, abgeschnittene letzte Zeile wird toleriert, jeder Endpunkt liefert gültiges JSON, Server bindet nur auf Loopback, ein Request ohne Token wird abgewiesen.

## Phase 3 — Systematisch testen und debuggen

Erst hier wird das Werkzeug benutzt, wofür es gebaut ist. Der Testauftrag steht schon in `docs/plans/agent-work-runtime.md` §30 und wird jetzt mit echtem Modell gefahren:

> Analysiere `agentkit_swarm`, identifiziere die drei wichtigsten Lifecycle-Risiken, implementiere die wichtigste Verbesserung, ergänze Tests, führe ein unabhängiges Review durch, dokumentiere die Entscheidung.

Bedingungen aus §30, jede einzeln zu prüfen: mindestens fünf Work Items, zwei Rollen, ein Schwarm, Claims im Working Graph, Promotion in den Canonical Graph, ein Git-Artefakt, ein Review, **Prozess mitten in der Arbeit abschießen**, nach Neustart fortsetzen, abgeschlossene Analyse nicht wiederholen, Evidence Trail am Ende.

Erfolgskriterien wörtlich aus §30: kein Item verloren, kein abgeschlossenes Item wiederholt, abgelaufene Leases freigegeben, Lauf korrekt fortgesetzt, Claims mit vollständiger Provenance, nur verifizierte Claims promotet, Endergebnis mit Artefakten, Tests und Review.

Befunde werden als Issues im GitHub-Projekt erfasst (Repo-Konvention) und der Reihe nach behoben. Dass dabei etwas gefunden wird, ist die Erwartung, nicht der Ausnahmefall — die Handproben dieser Session haben jedes Mal etwas gefunden, was 240 Tests nicht gefunden haben.

## Phase 4 — Schwarm-Kommunikation

Aus den `swarm_event`-Datensätzen: **Sequenzdiagramm** (Zeit nach unten, Mitglieder als Spalten, Nachrichten als Pfeile, `MessageKind` als Farbe) plus **Abstimmungsansicht** (Vorschläge, Zustimmungen, Ausgang) und **Dead Letters**. Alles reine Frontend-Arbeit, weil Phase 1 die Daten schon liefert.

## Phase 5 — Graph-Ansicht

**`agentkit_graph`** (die zwei fehlenden Nähte):
- `GraphStore::open_read_only(dir)` — liest ohne die automatische Kompaktierung von `open()`. Muster und Begründung eins zu eins wie `WorkStore::open_read_only`: ein Leser darf die Datei eines lebenden Schreibers nicht anfassen.
- `GraphIndex::sources()` als Iterator (es gibt heute nur Einzel-Lookup) und `pub fn export(index) -> GraphExport` — eine serde-Projektion des vollständigen Graphen (Entities, Claims, Sources, Episodes). `Subgraph`/`render()` bleiben, wie sie sind: sie sind fürs Modell, nicht fürs Frontend.

**Frontend:** Knoten = `GraphEntity`, Kanten = `GraphClaim` (Subjekt→Objekt, Prädikat als Label). Filter nach `layer` (working/canonical), `scope`, `status`. Klick auf eine Kante zeigt Provenance über `source_ids` — bei Work-Claims also Projekt, Lauf, Item, Versuch, Agent. Layout als handgeschriebene Kraftsimulation in SVG, kein d3/cytoscape: bei Debug-Graphen mit Dutzenden bis Hunderten Knoten reicht das, und es hält einen vendorten JS-Blob aus dem Repo. Wird es zu langsam, ist eine Bibliothek der dokumentierte nächste Schritt.

## Verifikation

```bash
cargo test --manifest-path agent_framework_rs/Cargo.toml --no-default-features
cargo test --manifest-path agentkit_swarm/Cargo.toml
cargo test --manifest-path agentkit_graph/Cargo.toml
cargo test --manifest-path agentkit_work/Cargo.toml
cargo test --manifest-path agentkit_viz/Cargo.toml
cargo test --manifest-path agentkit_app/Cargo.toml --no-default-features --features "graph work viz"
cargo clippy --manifest-path agentkit_viz/Cargo.toml --all-targets -- -D warnings
```

Von Hand, der eigentliche Beweis: einen Lauf mit `--trace .agentkit/trace` starten, in einem zweiten Terminal `agentkit viz --open`, und während der Lauf arbeitet prüfen, dass Agenten-Liste, Verlauf und Kontext live wachsen. Danach den Prozess abschießen und denselben Trace nachträglich laden — die Ansicht muss identisch sein. Für Work zusätzlich gegen `agentkit work status --format json` gegenlesen, damit Viewer und CLI nicht auseinanderlaufen.

## Was der Plan bewusst nicht enthält

- **Keine Redaktion von Secrets** — siehe Sicherheitsabschnitt: eine Warnung ist ehrlicher als ein Filter, dem man vertraut.
- **Kein Schreibzugriff.** Der Viewer beobachtet; er startet, stoppt und ändert nichts. Ein „Freigabe erteilen"-Knopf wäre verlockend, macht aus einem Leseloch aber ein Schreibloch.
- **Keine Multi-User-/Remote-Fähigkeit**, keine Auth über das Loopback-Token hinaus.
- **Kein Ersatz für die TUI.** Die bleibt das Werkzeug für den interaktiven Lauf; der Viewer ist für Analyse und lange Läufe.