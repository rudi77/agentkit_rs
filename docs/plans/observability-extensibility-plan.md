# Observability, Traceability, Extensibility — was agentkit von pytaskforce und dem Cordis-Paper übernehmen sollte

**Status:** Vorschlag (Konzept + Roadmap, keine API-Festlegung). Abschnitt 4 wird durch [`spatiotemporal-composability-plan.md`](spatiotemporal-composability-plan.md) abgelöst und vertieft.
**Quellen:** Analyse von [pytaskforce](https://github.com/rudi77/pytaskforce) (Stand 2026-08), Paper *„A Programming Paradigm for Spatiotemporal Composability"* (Shi/Zhang/Cui, das „Cordis-Paper"), Ist-Stand dieses Repos.

---

## 1. Anlass und Vorgehen

pytaskforce hat in drei Bereichen ein durchdachteres Design als agentkit: **Observability** (Token-Verbrauch, Tracing-Infrastruktur), **Traceability** (nachvollziehbare Läufe mit Metriken) und **Extensibility** (Protokoll-Seams, Override-Hooks, Plugin-Discovery, deklarative Tool-Metadaten). Das Cordis-Paper liefert dazu den theoretischen Überbau für dynamische Komposition: *revertible effects* (jede Registrierung trägt ihre Inverse) und *reactive coeffects* (deklarierte Abhängigkeiten, auf die der Runtime reagiert).

Dieses Dokument gleicht die drei Quellen ab und schlägt vor, was agentkit übernimmt — und, genauso wichtig, was **bewusst nicht**. Maßstab ist `CODING_GUIDELINES.md`: Einfachheit zuerst, Abstraktion erst ab zwei echten Nutzern, offline-by-default bleibt unangetastet.

Ein wiederkehrender Befund vorweg: **einiges aus dem Paper ist in beiden Systemen längst da, nur unter anderem Namen.** Die Tabellen in Abschnitt 4 machen das explizit, damit wir nichts nachbauen, was schon existiert.

---

## 2. Ist-Stand agentkit

### 2.1 Observability

Vorhanden und tragfähig:

- **EventBus** (`agent_framework_rs/src/events.rs`): typisierte `AgentEvent`s (`step`, `text_delta`, `tool_call`, `tool_result`, `plan`, `final`, `error`, `cancelled`, `done`, `structured`), Korrelation über `task_id` (Sub-Agent), `source` (Agent-Label) und `call_id` (Tool-Aufruf). Mehrere Konsumenten am selben Strom — CLI, TUI, Trace.
- **NDJSON-Trace** (`agent_framework_rs/src/trace.rs`): opt-in (`--trace DIR`), `schema_version` je Zeile, `seq`/`at_ms`, Kürzung langer Texte mit Vermerk, bewusst keine Redaktion (ehrliche Warnung statt trügerischem Filter), `.gitignore` daneben.
- **agentkit-viz**: liest den Trace als Datei (die Naht ist die Datei, nicht ein API), Reiter für Agenten/Verlauf/Kontext/Timeline/Schwarm/Graph/Work.

Lücken:

| Lücke | Konsequenz |
|---|---|
| **Kein Token-Usage-Tracking** — `src/llm.rs` liest das `usage`-Feld des Streams nicht aus | Kein Kosten-/Budget-Überblick je Lauf; die viz-Timeline kann Dauer zeigen, aber nicht Verbrauch; `token_budget`-Kompaktierung schätzt statt zu messen |
| **Keine Dauer an den Ereignissen selbst** — nur `at_ms` je Trace-Zeile | Ein Konsument muss Paare (`tool_call`→`tool_result`) selbst korrelieren und subtrahieren; live am Bus (ohne Trace) fehlt die Information ganz |
| **Kein Run-Manifest** — Modell, Konfiguration, Strategy, Exit-Klassifikation stehen nirgends als Datensatz | Ein Trace ist erst nachvollziehbar, wenn man weiß, *womit* der Lauf lief; heute muss man das aus der Shell-History rekonstruieren |

### 2.2 Traceability

Der Trace selbst ist gut (eine Zeile je Ereignis, eine Form für alle Zeilen, Robustheit nach dem Muster des Work-Journals). Was fehlt, ist oben schon genannt: Usage, Dauern, Manifest. Traceability ist damit bei agentkit kein eigenes Baufeld, sondern die Summe der Observability-Lücken — die Vorschläge in Abschnitt 3 schließen beide Themen zusammen.

### 2.3 Extensibility

Vorhanden:

- **Der `extra_tools`-Seam** (`CodingAgentConfig::extra_tools` mit `ExtraToolCtx`): der einzige Erweiterungspunkt des Kerns, per Closure-Chaining von agentkit_app, agentkit_work etc. genutzt. Funktioniert, ist aber implizit: wer die Kette erweitert, muss die Aufruf-Reihenfolge und das „vor `build()`, vor `mcp.apply`"-Timing kennen.
- **Rollen als `.md`-Dateien** (`--agents DIR`), **Skills**, **`--profile FILE`** — Daten statt Code, gut.
- **MCP mit Live-on/off**: `McpHub::rewire` baut die Registry aus einer MCP-freien Basis neu auf.
- **`--dry-run`**: baut jede Registry mit `dry_run_blocking(is_likely_destructive)` neu — Blockade per **Namens-Heuristik** (Verb-Präfix eines `_`-Segments). Dokumentierte Überraschung: `update_plan` wird mitgeblockt, weil „update" als destruktives Verb zählt.

Lücken:

| Lücke | Konsequenz |
|---|---|
| **Tool-Eigenschaften sind Heuristik statt Deklaration** — `is_likely_destructive` rät am Namen, `READ_ONLY_TOOLS` ist eine handgepflegte Parallel-Liste in `coding.rs` | Fehltreffer (`update_plan`), zwei Wahrheiten (Liste + Heuristik), MCP-Tools sind für die Heuristik unsichtbar |
| **Registry-Änderungen nur als Voll-Neuaufbau** — `rewire` und `dry_run_blocking` bauen alles neu | Funktioniert bei der heutigen Größe; skaliert aber nicht auf mehrere unabhängige Erweiterungen, die einzeln kommen und gehen sollen (genau das „coarse-grained workaround"-Muster, das das Cordis-Paper als Kostenstelle benennt) |
| **Erweiterungen haben keinen beobachtbaren Zustand** — ein MCP-Server ist „enabled" (Atomic-Flag) oder nicht; ob er tatsächlich antwortet, sieht niemand | Diagnose läuft über stderr-Meldungen statt über den Bus/Trace |

---

## 3. Vorschläge aus pytaskforce

### 3.1 P1 — Token-Usage als Event und Trace-Zeile

pytaskforce emittiert je LLM-Call ein `EventType.TOKEN_USAGE`-Ereignis mit `prompt_tokens`/`completion_tokens`/`total_tokens` (`react_loop.py`; Modell `TokenUsage` in `core/domain/models.py` mit Budget-Methoden `exceeds_budget`/`remaining`). Das ist die mit Abstand billigste Observability-Verbesserung, die agentkit fehlt.

**Übernahme:**
- Neuer Event-Typ `token_usage` + `EventData`-Variante (Konstante + Variante, wie in „Adding things" der Crate-CLAUDE.md beschrieben; der Compiler zeigt auf CLI-Renderer und TUI).
- Quelle ist das `usage`-Feld, das der Provider-Stream am Ende liefert (`stream_options: {"include_usage": true}` bzw. das letzte SSE-Chunk bei Azure/OpenAI) — `src/llm.rs` muss es nur durchreichen statt verwerfen.
- Landet automatisch im Trace (der `TraceWriter` schreibt jedes Bus-Ereignis) und damit in agentkit-viz; die Timeline kann Verbrauch je Schritt zeigen, die Agenten-Ansicht Summen je Lauf.
- Folge-Nutzen: die `token_budget`-Kompaktierung kann auf gemessene statt geschätzte Werte umgestellt werden — als eigener, späterer Schritt.

**Passt zur Architektur, weil:** ein Ereignistyp mehr, kein neuer Kanal, kein neues Subsystem. Frontends, die ihn nicht kennen, ignorieren ihn.

### 3.2 P1 — Deklarative Tool-Metadaten statt Namens-Heuristik

pytaskforce deklariert am Tool selbst: `requires_approval`, `approval_risk_level` (Enum in `core/interfaces/tools.py`), `supports_parallelism`, `tool_result_store_threshold`. Registry und Wrapper (Approval-, Caching-Wrapper in `infrastructure/tools/wrappers.py`) lesen die Deklaration — nirgends wird am Namen geraten.

**Übernahme:**
- `ToolRegistry::add` nimmt (per Builder oder kleinem Metadaten-Struct) zwei bis drei deklarierte Eigenschaften entgegen: `read_only`, `destructive`, ggf. `needs_approval`. Default bleibt konservativ (nicht read-only, nicht destruktiv), damit bestehende `add`-Aufrufe unverändert weiterlaufen können, bis sie migriert sind.
- `--dry-run` blockt dann nach Deklaration; `is_likely_destructive` bleibt nur noch als Fallback für MCP-Tools ohne Deklaration. Der dokumentierte `update_plan`-Fehltreffer verschwindet.
- `READ_ONLY_TOOLS` in `coding.rs` wird aus den Deklarationen abgeleitet statt parallel gepflegt — eine Wahrheit weniger.
- **Nicht** übernehmen: `supports_parallelism` (agentkit parallelisiert bereits über `std::thread::scope`, und es gibt keinen dokumentierten Fall, wo das ein Tool bricht — YAGNI, bis einer auftritt) und den Result-Store-Threshold (ctxman-Externalisierung deckt das ab, siehe 3.6).

**Passt zur Architektur, weil:** es die bestehenden Mechanismen (`dry_run_blocking`, Rollen-Subsets) auf eine bessere Datenquelle stellt, statt neue zu bauen. Achtung Portabilitäts-Konvention: das ist eine bewusste Abweichung vom Python-Original und gehört in die „Bewusste Unterschiede"-Liste der README.

### 3.3 P2 — Dauer-Felder und Run-Manifest

**Dauern:** `tool_result` bekommt ein `duration_ms` (der Loop kennt Beginn und Ende des Aufrufs ohnehin), `final`/`done` die Gesamtdauer. Damit ist die Information live am Bus verfügbar, nicht erst nach Trace-Korrelation. pytaskforce hat das nicht explizit — hier ist agentkit-viz der Treiber (die Timeline rechnet Dauern heute aus `at_ms`-Differenzen zusammen).

**Run-Manifest:** eine `structured`-Trace-Zeile (`kind: "run_manifest"`) am Anfang jedes Laufs: Modell/Deployment, Strategy, aktive Features (ctxman ja/nein, dry-run ja/nein), Tool-Namen, Rollen-/Skill-Verzeichnisse, und am Ende eine Abschluss-Zeile mit der Exit-Klassifikation (die Kategorien existieren schon: `classify_outcome` in `src/cli.rs` und die Failure-Sentinels). Der Mechanismus dafür existiert komplett — `AgentEvent::structured` und der `context_snapshot`-Präzedenzfall; es ist nur eine weitere `kind`.

**Passt zur Architektur, weil:** `structured` genau dafür gebaut wurde: „der Kern legt Diagnose-Daten daneben", Frontends überspringen Unbekanntes.

### 3.4 P2 — Den `extra_tools`-Seam zur kleinen Extension-Schnittstelle formalisieren

pytaskforce' Stärke ist nicht ein einzelner Mechanismus, sondern dass Erweiterung **ein benannter Vertrag** ist: Protokolle in `core/interfaces/`, Override-Hooks in `application/infrastructure_overrides.py` (über 20 `set_*_override`-Paare), Entry-Point-Discovery (ADR-026). agentkit hat funktional dasselbe erreicht — mit Cargo-Features, Path-Dependencies und dem `extra_tools`-Closure — aber der Vertrag ist implizit (Timing-Regeln, Closure-Chaining, `SwarmToolConfig::extra_member_tools` als Sonderweg für Schwarm-Mitglieder).

**Übernahme (bewusst klein):** kein Plugin-System, sondern den bestehenden Seam explizit machen. Eine Erweiterung ist ein Wert mit drei Fähigkeiten: *Tools registrieren* (bekommt `ExtraToolCtx` wie heute), *optional am Bus mithören* (heute muss eine Erweiterung dafür an den Frontends vorbei), *optional aufräumen* (heute: nichts — siehe 4.2). `FrontendTools` in `agentkit_app/src/lib.rs` wird der erste Nutzer; agentkit_work chained heute schon Closures und wäre der zweite — damit ist die Zwei-Nutzer-Regel der Guidelines erfüllt.

**Nicht übernehmen:** Entry-Point-artige *Discovery* (Laufzeit-Suche nach Erweiterungen). Rust löst das mit Features + Path-Dependencies zur Compile-Zeit; genau so ist das Repo verdrahtet, und die statische musl-Binary ist ein Release-Artefakt, das dynamisches Laden ausschließt.

### 3.5 P3 — Phase-Hints für die Modellwahl

pytaskforce' LLM-Router (ADR-012) lässt Strategien Phasen-Hinweise (`planning`, `reasoning`, `summarizing`, …) als Modellparameter emittieren; eine Konfiguration mappt Hints auf Modelle, abgeschaltet ist alles backward-kompatibel auf ein Modell gemappt.

agentkit hat den Präzedenzfall schon: `--ctx-compaction-model` wählt ein eigenes Modell für die Kompaktierung. Ein allgemeiner Mechanismus lohnt erst, wenn ein zweiter Anwendungsfall real wird (z. B. billigeres Modell für Sub-Agent-Rollen wie `explorer`). Bis dahin: notiert, nicht gebaut.

### 3.6 Bereits vorhanden — nur anders benannt

Damit nichts doppelt gebaut wird:

| pytaskforce | agentkit-Äquivalent |
|---|---|
| Tool-Result-Store + `fetch_result` (ADR-025, Kontext-Isolation) | ctxman-Externalisierung großer Tool-Ergebnisse + `expand_context_ref`-Tool (Feature `ctxman`) |
| Kompaktierungs-/Summarizing-Modellwahl | `--ctx-compaction-model` |
| Sub-Agent-Kontext-Snapshots (`register_sub_agent_context`) | `context_snapshot`-Ereignis je Agent am Lauf-Ende |
| `execute_mission` delegiert an `execute_mission_streaming` (ein Pfad) | `run`/`run_cb`/`run_with_events`/`run_on_bus` als dünne Wrapper über `Agent::drive` |
| Structured Logging (structlog) | der EventBus *ist* das strukturierte Log; ein zweiter Logging-Kanal daneben wäre eine zweite Wahrheit |
| OTEL/Phoenix-Tracing | der NDJSON-Trace + agentkit-viz. OTEL würde HTTP/OTLP-Abhängigkeiten in den Kern ziehen und offline-by-default brechen. Falls je ein OTEL-Backend gebraucht wird: ein **Konverter** Trace→OTLP als eigenes Werkzeug, der Kern bleibt unberührt (dieselbe Naht wie bei viz: die Datei) |

---

## 4. Vorschläge aus dem Cordis-Paper

> **Hinweis:** Dieser Abschnitt ist durch [`spatiotemporal-composability-plan.md`](spatiotemporal-composability-plan.md) abgelöst — dort wird die hier zurückgestellte volle Coeffect-Variante für die langlaufenden Oberflächen (MCP, Skills, Schwarm, Work-Runtime) durchdesignt; 4.2 und 4.3 gehen dort als Phase 1 auf.

### 4.1 Abgleich: Paper ↔ pytaskforce ↔ agentkit

| Paper-Konzept | pytaskforce heute | agentkit heute |
|---|---|---|
| **Revertible Effects** (Registrierung liefert Inverse; LIFO-Disposer-Akkumulator; Entladen = Inverse anwenden) | Nicht vorhanden — Plugins/Tools werden beim Build aufgelöst, Entladen zur Laufzeit gibt es nicht | Als „coarse-grained workaround" (so nennt es das Paper wörtlich): `McpHub::rewire` und `dry_run_blocking` bauen die ganze Registry aus einer Basis neu, statt einzelne Registrierungen zurückzunehmen |
| **Reactive Coeffects** (deklarierte Dependencies; Aktivierung erst bei Erfüllung, Deaktivierung bei Wegfall) | Statisch: Registry-Eintrag schlägt bei Tool-Build fehl, wenn das Agent-Paket fehlt („fails to resolve at tool-build time") — Deklaration ja, Reaktivität nein | Statisch: Rollen/Profile nennen Tool-Subsets; ob ein MCP-Server tatsächlich verfügbar ist, prüft niemand deklarativ |
| **Capability-based Access** (deklarierte inject-Menge = prüfbare Berechtigungsanforderung; Interception für Policy) | Tool-Listen je Profil; `ApprovalRiskLevel` + Approval-Wrapper als Interception | Prinzip gelebt: Rollen bekommen Tool-Subsets, Sub-Agenten bekommen **nie** `task`, Schwarm-Mitglieder nie `swarm` — aber als Konvention im Code, nicht als geprüfte Deklaration. `ApproveFn` + `--dry-run` sind Interception |
| **Deklarativer Loader + Reconciliation** (Ziel-Konfiguration als Datenstruktur, Runtime gleicht inkrementell an) | Ansätze: Settings-Hydration läuft nach jedem PUT erneut; Deployment-Manifest als Allowlist | `--profile FILE` ist rein statisch (einmal beim Start); Live-Änderung nur MCC on/off |
| **Fiber-Lifecycle** (LOADING/ACTIVE/UNLOADING/INACTIVE/FAILED, beobachtbar) | Nicht vorhanden | Nicht vorhanden (MCP: ein Atomic-Flag `enabled`) |
| **HMR / Component Loader** | Nicht vorhanden | Nicht vorhanden |

Lesart: **agentkit ist dem Paper näher, als es aussieht** — die Kapselung „Sub-Agenten kriegen nie `task`" ist gelebte Capability-Disziplin, und der Registry-Neuaufbau ist die grobe Version von Entladbarkeit. Das Paper liefert die Begriffe, um die nächste Ausbaustufe klein und gezielt zu wählen.

### 4.2 P2 — Disposer-basierte Registrierung (revertible effects „light")

Der Kern des Papers, auf agentkit-Maß gebracht: **jede Registrierung, die eine Erweiterung am Agenten vornimmt, gibt eine Rücknahme zurück**; die Erweiterung akkumuliert ihre Rücknahmen; Entladen heißt, sie in LIFO-Reihenfolge anzuwenden. In Rust ist das kein Framework, sondern ein Rückgabewert (Guard/Closure) — das Paper selbst nennt RAII als den statischen Spezialfall; hier geht es um die dynamische Variante, deren Lebensdauer nicht lexikalisch ist.

**Was das konkret kauft:**
- `McpHub::rewire` (Neuaufbau der Gesamt-Registry aus `base.clone()`) kann durch gezieltes Entfernen der Tools *eines* Servers ersetzt werden — Voraussetzung dafür, dass später mehrere Erweiterungen unabhängig kommen und gehen (Schwarm-Tools, Graph-Tools, Work-Tools sind heute schon drei getrennte Registranten).
- Die Extension-Schnittstelle aus 3.4 bekommt ihr *Teardown* geschenkt: eine Erweiterung, die nur über die Registrierungs-Primitive arbeitet, ist automatisch vollständig entladbar — „locality of concern", das zentrale Argument des Papers gegen getrennte activate/deactivate-Hooks.
- Kein Verhalten ändert sich, solange niemand entlädt: der Disposer ist nur ein bislang weggeworfener Rückgabewert.

**Bewusst nicht:** der volle Apparat (Effekt-Kontext 𝜕Γ, Unabhängigkeits-Beweise, Interleaving-Garantien). agentkit hat eine Handvoll Registranten mit klarer Reihenfolge; LIFO je Erweiterung reicht.

### 4.3 P3 — Lifecycle-Zustände als beobachtbare Ereignisse

Die Fiber-Zustände des Papers, reduziert auf das, was agentkit heute schon *hat, aber nicht zeigt*: MCP-Server (verbunden/gestört/abgeschaltet) und Erweiterungen (registriert/entladen). Ein `structured`-Ereignis (`kind: "extension_state"`) bei jedem Übergang genügt — es landet im Trace, agentkit-viz kann einen Zustands-Reiter oder Timeline-Markierungen daraus machen, das TUI kann `/mcp` mit echtem Zustand statt nur dem Flag anreichern.

Das verbindet das Cordis-Thema mit dem Observability-Thema: Zustand, den es gibt, gehört auf den Bus — derselbe Grundsatz, nach dem der Kern schon `context_snapshot` publiziert.

### 4.4 Bewusst nicht übernehmen

- **Reactive Coeffects in voller Form** (Aktivierung/Deaktivierung bei Provider-Wechsel zur Laufzeit): agentkit-Läufe sind kurzlebig; Komponenten kommen nicht während eines Laufs dazu. Der Bedarf entsteht erst mit langlaufenden Prozessen (agentkit_work wäre der Kandidat) — bis dahin YAGNI. Die kleine Vorstufe, die sich lohnt: **deklarierte Tool-Anforderungen an Rollen/Skills beim Laden prüfen** (eine Rolle, die `read_pdf` nennt, ohne dass das Feature gebaut ist, soll beim Laden warnen statt zur Laufzeit soft-erroren). Das ist die Satisfaktions-Prüfung des Papers ohne den Reaktivitäts-Apparat.
- **HMR / Component Loader / Reconciliation**: agentkit ist ein CLI-Werkzeug, kein Daemon. Neustart ist billig; die Kostenrechnung des Papers (verlorener Prozesszustand) greift nicht.
- **Coeffect-Isolation/Interception als Mechanismus**: die zwei realen Interception-Fälle (Approval, dry-run) sind bereits gelöst.

---

## 5. Roadmap

| Prio | Vorschlag | Abschnitt | Hängt ab von |
|---|---|---|---|
| **P1** | `token_usage`-Ereignis (Bus + Trace + viz) | 3.1 | — |
| **P1** | Deklarative Tool-Metadaten; `--dry-run` und `READ_ONLY_TOOLS` darauf umstellen | 3.2 | — |
| **P2** | `duration_ms` an `tool_result`; Run-Manifest + Abschluss-Zeile im Trace | 3.3 | — |
| **P2** | Extension-Schnittstelle (Formalisierung von `extra_tools`) | 3.4 | sinnvoll zusammen mit 4.2 |
| **P2** | Disposer-basierte Registrierung; `McpHub::rewire` darauf umstellen | 4.2 | — |
| **P3** | `extension_state`-Ereignisse + viz-Anzeige | 4.3 | 4.2 |
| **P3** | Rollen-/Skill-Tool-Anforderungen beim Laden prüfen | 4.4 | 3.2 (Metadaten) |
| **P3** | Phase-Hints für Modellwahl | 3.5 | zweiten realen Anwendungsfall abwarten |

Jede P1/P2-Zeile ist unabhängig ausrollbar und einzeln testbar (FakeLlm-Skripte für die Ereignisse, Registry-Tests für Metadaten und Disposer). Nichts davon berührt den Default-Build-Pfad oder die offline-Konvention; die Tool-Metadaten sind die einzige Abweichung vom Python-Original und werden in der README-Liste „Bewusste Unterschiede zu Python" dokumentiert.
