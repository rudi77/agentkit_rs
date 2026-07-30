# agentkit-work

Persistente Runtime für Arbeit, die **einen einzelnen Agent-Lauf überlebt**: Vorhaben werden in
Work Items zerlegt, ein Worker arbeitet sie abhängigkeitsgerecht ab, und jeder Versuch, jedes
Artefakt und jeder Zustandswechsel landet in einem Journal. Stirbt der Prozess, kostet das
höchstens den laufenden Versuch — nicht das Projekt.

```text
agentkit          führt den LLM-/Tool-Loop aus
ctxman            verwaltet den Kontext EINES Agenten
agentkit-graph    speichert Wissen — dauerhaft und über Agenten hinweg
agentkit-swarm    verwaltet Agent-Actors und Peer-Kommunikation
agentkit-work     hält den ARBEITSZUSTAND am Leben — Items, Versuche, Leases, Artefakte
```

> Nicht ein Agent muss stundenlang leben. Der Arbeitszustand muss stundenlang leben.

Agenten und Schwärme dürfen kurzlebig sein. `agentkit-work` trifft keine fachlichen
Entscheidungen und löst keine Aufgaben — es weiß, welche Aufgabe offen ist, wer sie bearbeitet,
was sie erzeugt hat und wo nach einem Absturz weitergemacht wird.

## In 30 Sekunden

```bash
agentkit work create --title "Graceful Swarm Shutdown" \
  --objective "Analysiere den Lifecycle, implementiere einen kontrollierten Shutdown, ergänze Tests"
# → graceful-swarm-shutdown

agentkit work run graceful-swarm-shutdown -y      # arbeitet, bis fertig/blockiert/Budget/Ctrl-C
agentkit work status graceful-swarm-shutdown      # Was ist offen? Was blockiert?
agentkit work resume graceful-swarm-shutdown      # nach Absturz oder Pause weiter
```

Ablage, komplett unter dem Workspace:

```text
<workspace>/.agentkit/work/<projekt-id>/
    work.jsonl                  Event-Journal (+ Snapshot-Zeile nach einer Kompaktierung)
    artifacts/W-3/A-7/…         Ergebnisse der Versuche
```

Der Versuch (`A-7`) steckt mit im Pfad, nicht nur das Item (`W-3`): so kollidiert ein
Wiederholungsversuch nie mit der Datei seines Vorgängers (Retry ist ein Kernfeature dieser
Laufzeit), und der Pfad trägt zugleich die Provenance — wer `artifacts/W-3/` ansieht, sieht direkt,
welcher Versuch was erzeugt hat.

Artefakte liegen bewusst **im** Workspace: so erreicht der nächste Agent sie mit dem vorhandenen
`read_file`-Tool, und es braucht kein eigenes Lese-Tool.

## Wann dieses Crate — und wann nicht

| Situation | Werkzeug |
|---|---|
| Ein klarer Auftrag, ein Agent, wenige Minuten | `agentkit` |
| Mehrere Perspektiven, kurze Zusammenarbeit | `agentkit-swarm` |
| Mehrere Arbeitsschritte, Wiederaufnahme, lange Laufzeit | **`agentkit-work`** |

Für „erklär mir diese Datei" oder „schreib diese kleine Funktion" ist es reiner Overhead.

## Die drei Tools, die der ausführende Agent bekommt

`register_work_tools` hängt sie in die Registry des Work-Agenten — dieselbe Naht, die
`agentkit-graph` benutzt, ohne eine Zeile im Agent-Kern zu ändern.

| Tool | Wozu |
|---|---|
| `work_add_item` | Vorhaben in abgegrenzte Teilaufgaben zerlegen (Abhängigkeiten, Akzeptanzkriterien) |
| `work_artifact` | Ergebnis als Datei ablegen statt in die Antwort zu schreiben |
| `work_submit` | Versuch abschließen und jedes Akzeptanzkriterium einzeln bewerten |

`run_id`, `work_item_id`, `attempt_id` und `agent_id` haben **kein Tool-Argument**: die Identität
setzt die Laufzeit, nie das Modell. Das ist strukturell erzwungen, nicht validiert.

Mit angeschlossenem Wissensgraph kommt ein viertes Tool dazu, `work_claim` (siehe nächster
Abschnitt).

## Graph-Anbindung (Phase 4)

`agentkit-work` speichert Arbeitszustand, `agentkit-graph` speichert Wissen (§11 des Konzepts) —
das bleibt getrennt. Verbunden werden beide über einen **Port statt einer Dependency**:
[`GraphGateway`](src/graph.rs) hat zwei Methoden (`recall`, `record_claims`) und lebt in diesem
Crate, ohne dass dieses Crate `agentkit_graph` je importiert — dieselbe Einbahnrichtung wie beim
Rest des Repos (`CLAUDE.md`): `agentkit_work` kennt `agentkit`, nicht seine Geschwister. Der
Adapter, der den Port über den echten Graphen implementiert, liegt in `agentkit_app`
(`src/work_graph.rs`, `#[cfg(all(feature = "work", feature = "graph"))]`) — dem einzigen Crate,
das beide Bibliotheken kennt (§25). Die zweite Implementierung ist `FakeGraph` in den Tests
(`tests/tools.rs`, `tests/graph.rs`); das erfüllt Guidelines §2 (ein Trait braucht ≥ 2 reale
Nutzer).

**`work_claim` ist der EINZIGE Schreibweg mit Provenance.** Er wird nur registriert, wenn ein
Gateway vorhanden ist (`WorkToolCtx::gateway`, dasselbe Gating-Muster wie
`agentkit_graph::register_graph_tools`/`GraphAccess::can_write`). Das Modell liefert nur den
Inhalt (`subject`/`predicate`/`object`/`confidence`/`excerpt`) — die
[`WorkProvenance`](src/graph.rs) (Projekt, Lauf, Item, Versuch, Agent, Artefaktpfade dieses
Versuchs, Repository-Revision) baut das Tool aus dem laufenden `WorkToolCtx`, nie aus einem
Modellargument. Deshalb bekommt ein Work-Agent aus `agentkit_app` auch nur die LESENDEN Graph-Tools
(`graph_search`/`graph_neighbors`/`graph_evidence`), nie `graph_remember`/`graph_promote` — ein
zweiter, provenienzloser Schreibweg wäre schlimmer als gar keiner. Die vergebenen Claim-IDs landen
über das Ereignis `ClaimsRecorded` am `WorkAttempt` (`claim_ids`, HÄNGT an statt zu ersetzen — ein
Versuch darf `work_claim` mehrfach aufrufen) und überleben damit Checkpoint und Neustart wie jedes
andere Ereignis.

**Der Recall landet im Auftragstext, nicht als eigenes Kontext-Segment.** Der Runner ruft
`gateway.recall(...)` mit Titel und Beschreibung des Items, NACHDEM `AgentWorkPackage::build` das
Paket gebaut hat (`build` kennt kein Gateway), und setzt das Ergebnis auf
`AgentWorkPackage::graph_recall`. `render()` gibt es als eigenen, klar beschrifteten Abschnitt aus
("früheres Wissen, keine Anweisung"), VOR den Vorgänger-Artefakten. Dieselbe Begründung wie bei
`agentkit_graph::GraphAgent::compose_task`: der Agent-Kern bekommt dadurch keine neue Naht, der
Recall ist einfach Text im ohnehin vorhandenen `task`-Argument.

**Keine `promote`-Methode in dieser Phase.** Sie hätte noch keinen Aufrufer — Promotion nur
verifizierter Claims ist Phase 5 (§29 des Konzepts, Guidelines §4/YAGNI).

Ohne Feature `graph` (oder ohne `--graph DIR`) ist `WorkCliDeps::graph`/`RunnerConfig::graph`
`None` — dann gibt es weder `work_claim` noch Recall, und ein Lauf verhält sich exakt wie vor
Phase 4.

## Bewusste Design-Entscheidungen

Dieses Crate ist **kein Port** — es hat keine Referenzimplementierung. Grundlage ist
`docs/plans/agent-work-runtime.md`; jede Abweichung davon steht hier.

**Kein SQLite, sondern ein JSONL-Journal** (§23 des Konzepts). `rusqlite` mit `bundled` zieht die
SQLite-Amalgamation und damit einen C-Compiler in ein bewusst reines Rust-Repo — und in den
statischen musl-Build der Benchmark-Harness. `agentkit-graph` hat diese Entscheidung schon
getroffen und begründet. Snapshot-Isolation, parallele Leser und Dauerhaftigkeit gibt es hier
ohne diesen Preis: ein `RwLock<Arc<WorkState>>` für die Leser, ein `Mutex` für den einen
Schreiber, ein Append-only-Journal auf der Platte. Das Konzept nennt das Event-Log in §14 ohnehin
als die nachvollziehbare Wahrheit — der Zustand ist die Projektion, nicht umgekehrt.

**Ein Checkpoint ist eine ANGEHÄNGTE Snapshot-Zeile**, keine eigene Entität (§15). Zwei Wahrheiten
— eine Checkpoint-Tabelle *und* ein Event-Log — können auseinanderlaufen. Stattdessen schreibt
`WorkStore::checkpoint` den vollen Zustand als eine `snapshot`-Zeile über `Journal::append_snapshot`
ans Ende des Journals — die Historie bleibt vollständig erhalten, `Journal::open` nimmt beim
nächsten Start nur die LETZTE Snapshot-Zeile als Basis und spielt ausschließlich die Ereignisse
danach erneut ein. Wiederaufnahme bleibt dadurch O(1) zur Anzahl der Ereignisse seit dem letzten
Checkpoint, nicht O(n) zur gesamten Journallänge. `CheckpointCreated` bleibt als Ereignis erhalten.

Ein früherer Entwurf schrieb hier über `Journal::rewrite` — dieselbe Funktion, die heute
ausschließlich der echten Kompaktierung dient (siehe unten) — und ERSETZTE damit bei JEDEM
Checkpoint das gesamte Journal durch die eine Snapshot-Zeile. Der Runner checkpointet nach jedem
abgeschlossenen Work Item, also zeigte `agentkit work events` nach dem ersten Item schon nur noch
eine einzige "snapshot"-Zeile — genau die Zeitleiste war zerstört, die dieses Kommando laut Plan
(§14: vollständige Auditierbarkeit, Debugging, Timeline, Metriken, Replay von Fehlerfällen)
anzeigen soll. Echte Kompaktierung — die Historie WIRD dabei bewusst aufgegeben — passiert seitdem
nur noch an einer einzigen Stelle: `WorkStore::open`, wenn die Journallänge weit genug vor die
Anzahl der Datensätze gelaufen ist (`REWRITE_MIN_LINES`/`REWRITE_FACTOR`). Das ist die einzige
Stelle im Crate, an der Auditierbarkeit bewusst gegen unbegrenztes Wachstum getauscht wird.

**`Ready` und `Blocked` sind abgeleitete Sichten, keine gespeicherten Zustände** (§6.4).
Gespeichert werden nur `Pending | Running | Completed | Failed | Canceled`. Ein gespeichertes
`Ready` kann vom Abhängigkeitsgraphen abdriften — der Klassiker: ein Vorgänger wird
zurückgesetzt, der Nachfolger bleibt `Ready` stehen. `state::ready_items` rechnet es bei jeder
Abfrage neu und kann deshalb nicht falsch werden. Entsprechend gibt es kein `WorkItemReady`-Ereignis.

**Kein `Claimed` zwischen `Pending` und `Running`** (§6.4). Bei einem synchronen
Ein-Prozess-Worker sind Claim und Start derselbe Moment; ein Zwischenzustand hätte keine
beobachtbare Dauer. Ein erster Entwurf ließ die erste Lease-Verlängerung `Claimed → Running`
schalten — das war ein Fehler: ein Versuch, der in einem Zug fertig wird, erzeugt gar kein
Heartbeat-Ereignis und wäre auf `Claimed` hängen geblieben.

**Abhängigkeiten sind eine Liste, kein Typ** (§6.5). `dependencies: Vec<WorkItemId>` mit der
Semantik FinishToStart. `RequiresArtifact`/`RequiresDecision`/`RequiresVerification` hätten im
MVP keinen Erzeuger. Dafür werden Abhängigkeiten beim Anlegen **validiert**: unbekannte ID,
Selbstreferenz, Duplikat und Zyklus werden abgelehnt. Das ist keine Kosmetik — die Items kommen
vom Modell, und ein unbemerkter Zyklus macht `ready_items` dauerhaft leer und den Lauf lautlos
still.

**Zeit ist ein Parameter, `u64`-Millisekunden** (§6.3). `chrono` ist in keinem Manifest dieses
Repos, und Domänenfunktionen bekommen `now_ms` übergeben statt die Systemuhr zu lesen — nur so
sind Leases ohne `sleep` testbar. Kein `Clock`-Trait: er hätte genau eine Implementierung.

**Budget ohne Token und Kosten** (§17). `max_wall_time_secs`, `max_work_items`,
`max_attempts_per_item`, `max_steps_per_attempt`, `max_parallel_agents`. Der `Llm`-Trait des Kerns
gibt keine Token-Nutzung nach außen, und `rust_decimal` ist keine Dependency. Ein Budget, das
nicht gemessen werden kann, wäre gelogen.

**Ein Budget-Wechsel ist ein eigenes Ereignis, `BudgetUpdated`, kein zweites
`ProjectCreated`.** Ein früherer Entwurf ließ `agentkit work run --max-steps`
das Budget über ein zweites `ProjectCreated` ändern, weil `state::apply` das
Projekt bedingungslos überschrieb — das Journal hätte dann zweimal „Projekt
angelegt" behauptet, für ein Vorhaben, das genau einmal angelegt wurde, und
umging nebenbei die Duplikat-Absicherung, die `RunStarted`/`WorkItemCreated`/
`ArtifactCreated` längst hatten. `state::apply` lehnt ein zweites
`ProjectCreated` deshalb jetzt ebenso ab; ein Budget-Wechsel läuft über
`BudgetUpdated`, das nur `project.budget` ersetzt. Das ist zugleich der Weg
aus einem wegen `BudgetExceeded` pausierten Lauf (`agentkit work budget
<projekt-id> --max-items N …`) — vorher gab es dafür kein Kommando, obwohl der
Runner-Kommentar bei `RunPaused` genau das versprach.

**Fünf Fehlerarten statt elf** (§18): `ModelFailure`, `MaxSteps`, `InvalidOutput`, `Interrupted`,
`BudgetExceeded`. Nur diese fünf haben einen Erzeuger. Ein Retry ist `attempt_count + 1` und ein
Rücksprung nach `Pending`, ohne Backoff-Schlaf — bei einem Worker laufen andere Items dazwischen.
Was vom Retry-Konzept wirklich zählt, ist drin: die **vorherige Fehlerursache steht im nächsten
Arbeitspaket** (§12).

**Ein unterbrochener Versuch zählt nicht gegen `max_attempts`.** Ein gekillter Prozess ist kein
fachlicher Fehlversuch. Deshalb `WorkItemReleased` statt `WorkItemFailed` — die Regel steht genau
einmal im Code (`recovery::interrupt_attempt`) und wird von Runner und Recovery gemeinsam benutzt.

**Recovery VOLLENDET halb geschriebene Übergänge, statt sie zu verwerfen.** Stirbt ein Prozess
GENAU zwischen `AttemptFinished` und dem folgenden `WorkItemCompleted`/`WorkItemFailed`, steht der
Ausgang des Versuchs schon fest (Lease existiert noch, weil nur der zweite Schritt es entfernt).
`recovery::recover_matching` holt diesen zweiten Schritt jetzt nach — ein erfolgreich beendeter
Versuch macht das Item `Completed`, ein gescheiterter erhöht `attempt_count` und journalt
`WorkItemFailed`, genau wie ein regulärer (nicht abgestürzter) Fehlschlag. Vorher wurde das Item in
diesem Fall pauschal auf `Pending` zurückgeworfen und komplett neu ausgeführt — ein Widerspruch zum
Versprechen oben, dass ein Absturz höchstens den LAUFENDEN Versuch kostet.

**Eine Sperrdatei (`work.lock`) statt PID-Lebendprüfung.** `WorkStore` sperrte bisher nur
PROZESSINTERN (`RwLock`/`Mutex`) — nichts verhinderte ein zweites `agentkit work run` auf demselben
Verzeichnis; beide Prozesse hätten IDs aus ihrem je eigenen Snapshot berechnet und dieselbe ID
doppelt ans Journal angehängt, und der nächste `WorkStore::open` wäre beim Replay in die (korrekte)
Duplikat-Ablehnung in `state::apply` gelaufen — DAUERHAFT, denn jeder weitere Öffnungsversuch liest
dasselbe kaputte Journal erneut. `WorkStore::open` legt jetzt `work.lock` per `create_new` an (die
Atomizität selbst, kein Exists-Check als TOCTOU) und entfernt sie beim `Drop` wieder; ein zweiter
`open`-Versuch scheitert mit `WorkError::Locked`, solange der erste Store lebt. Bewusst KEIN
automatisches "PID lebt nicht mehr, also aufräumen": das Betriebssystem vergibt PIDs irgendwann neu,
"lebt Prozess X noch" ist plattformübergreifend (Windows/Linux) nicht zuverlässig zu beantworten,
und ein falsches "tot" würde eine Sperre entfernen, die tatsächlich noch einen lebenden Schreiber
schützt — genau der Schaden, den die Sperre verhindern soll. Der Ausweg ist deshalb explizit:
`agentkit work run/resume --force` entfernt eine zurückgebliebene Sperre gewaltsam (mit Warnung auf
stderr), als bewusste Entscheidung des Bedieners, nie automatisch.

**Der Heartbeat läuft im Ereignis-Callback des Agenten**, nicht in einem eigenen Thread (§7). Das
braucht keine Nebenläufigkeit, und ein Agent, der keine Ereignisse mehr liefert, ist genau der
Fall, den das Lease abdecken soll. Ins Journal kommt nur eine echte Verlängerung — ein Eintrag
alle 30 Sekunden über Stunden wäre reiner Müll.

**`recover_all` gibt alle Leases frei, nicht nur abgelaufene** (§15). Beim Start eines
Vordergrund-Laufs gibt es genau einen Worker; ein vorhandenes Lease kann dann nur von einem toten
Prozess stammen. Ohne das müsste ein Neustart bis zum Ablauf des Leases warten. Bei verteilten
Workern wäre diese Annahme falsch — deshalb steht sie ausdrücklich im Code.

**Flache Modulstruktur statt vier Schichten** (§24). Die hexagonale Trennung bleibt inhaltlich:
die Domäne (`model`, `event`, `state`) kennt weder Dateisystem noch LLM, die Adapter liegen außen
(`store`, `executor`, `cli`). Aber zwanzig Dateien für ein Crate, dessen Geschwister mit neun
auskommen, widerspricht den Coding-Guidelines §1/§3.

**ID-Vergabe nur innerhalb des Schreiber-Locks.** `WorkStore::submit_with` baut das Ereignis erst
im Lock aus dem dann gültigen Zustand. Der Grund ist konkret: der Agent-Kern führt mehrere
Tool-Aufrufe *einer* Modellantwort parallel aus, und zwei gleichzeitige `work_add_item` hätten
sonst dieselbe ID berechnet — das zweite Item hätte das erste überschrieben und wäre lautlos
verschwunden.

**Verifikation, Human Gates, Swarm-Executor und Worktrees sind nicht enthalten** (§10, §13, §19,
§20) — sie sind Phase 5–7 des Konzepts. `acceptance_criteria` wird aber ab Tag 1 mitgeführt und
ins Arbeitspaket gerendert; das ist der Anker, an dem die Verifikation später ansetzt.

## Grenzen des heutigen Stands

- **`--dry-run` gilt auch für Work-Läufe.** `agentkit work run <projekt-id> --dry-run` reicht das
  Flag bis zu `CodingAgentExecutor`/`CodingAgentConfig::dry_run` durch — `build_coding_agent`
  wendet die Sperre (`ToolRegistry::dry_run_blocking(is_likely_destructive)`) auf JEDE Registry an,
  auch die von `task`/`swarm` erzeugten. Kein eigener Nachbau der Heuristik in diesem Crate.
- **Ein Worker, ein schreibender Agent.** `max_parallel_agents` ist 1.
- **`work.lock` sperrt das GANZE Verzeichnis, auch für Lese-Kommandos.** `status`/`items`/`events`
  auf ein Projekt, das gerade in einem anderen Terminal läuft, scheitern ebenfalls mit
  `WorkError::Locked` — die Sperre unterscheidet nicht zwischen Leser und Schreiber. Einfacher als
  ein separates Lese-Lock, und für das MVP (ein Worker) ausreichend; ein abgestürzter Halter lässt
  sich mit `agentkit work run/resume --force` übernehmen (siehe oben, keine automatische
  PID-Lebendprüfung). `agentkit work list` überspringt ein gesperrtes Projekt ebenfalls, nennt
  seinen Namen aber auf stderr — sonst sähe "fehlt in der Liste" wie "existiert nicht" aus.
- **Ein zweiter Ctrl-C beendet den Prozess ohne Aufräumen.** Das erste Ctrl-C setzt den
  Cancel-Schalter (sauberer Lauf-Abbruch, `WorkStore::drop` gibt die Sperre normal frei); ein
  zweites erzwingt in `agentkit_app` einen sofortigen `std::process::exit`, der KEINE Destruktoren
  mehr laufen lässt — `work.lock` bleibt dann liegen, genau wie nach `SIGKILL`. Derselbe Ausweg gilt:
  `--force` beim nächsten `run`/`resume`.
- **`work pause` wirkt nur auf einen nicht laufenden Lauf.** Einen laufenden Vordergrund-Run
  pausiert man mit Ctrl-C; prozessübergreifendes Pausieren bräuchte Polling.
- **Kein `fsync`.** Das Journal wird geschrieben und geflusht, aber nicht synchronisiert — ein
  Stromausfall kann die letzte Zeile abschneiden. Eine abgeschnittene letzte Zeile wird beim
  Öffnen erkannt, verworfen und überschrieben; eine kaputte Zeile mitten im Journal ist ein
  harter Fehler.
- **Die Planungsvorlage steht im Code.** Sobald ein zweiter Planungsstil gebraucht wird, gehört
  sie in Daten (`WorkProject`/`RunnerConfig`) statt in weitere Zweige im Runner.
- **Kein Token-/Kostenbudget** (siehe oben).

## Tests

Alles offline, kein Test berührt das Netz — der Agentenpfad läuft über `agentkit::testing::FakeLlm`.

```bash
cargo test --manifest-path agentkit_work/Cargo.toml
cargo clippy --manifest-path agentkit_work/Cargo.toml --all-targets -- -D warnings
cargo build --manifest-path agentkit_work/Cargo.toml --features "openai ctxman"   # Feature-Durchleitung
```

Die Testdateien lesen sich als Spezifikation: `state.rs` (Statusmaschine, Readiness,
Abhängigkeitsprüfung), `journal.rs` (Replay, Kompaktierung, beschädigte Zeilen, parallele Leser),
`recovery.rs` (abgelaufene Leases, halb geschriebene Fehlschläge, Idempotenz), `scheduler.rs`
(Reihenfolge, Budget, Blockade), `tools.rs` (was das Modell darf und was nicht, inklusive
Pfadausbruch), `runner.rs` (vollständige Läufe, Retry, Abbruch, Neustart mitten im Lauf),
`cli.rs` (Argumente, stdout-Kontrakt, Exit-Codes).
