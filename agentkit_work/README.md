# agentkit-work

Persistente Runtime für Arbeit, die **einen einzelnen Agent-Lauf überlebt**: Vorhaben werden in
Work Items zerlegt, ein oder mehrere Worker arbeiten sie abhängigkeitsgerecht ab, und jeder Versuch, jedes
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
agentkit work run graceful-swarm-shutdown -y --parallel 4   # … mit bis zu vier Agenten gleichzeitig
agentkit work status graceful-swarm-shutdown      # Was ist offen? Was blockiert?
agentkit work watch graceful-swarm-shutdown       # Live-Ansicht in einem ZWEITEN Terminal
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
braucht keinen zusätzlichen Thread je Versuch, und ein Agent, der keine Ereignisse mehr liefert,
ist genau der Fall, den das Lease abdecken soll. Ins Journal kommt nur eine echte Verlängerung — ein Eintrag
alle 30 Sekunden über Stunden wäre reiner Müll.

**`recover_all` gibt alle Leases frei, nicht nur abgelaufene** (§15). Alle Worker leben in EINEM
Prozess (`work.lock`); beim Start eines Laufs hat der noch keinen Versuch begonnen, ein vorhandenes
Lease kann dann nur von einem toten Prozess stammen. Ohne das müsste ein Neustart bis zum Ablauf
des Leases warten. Bei Workern in mehreren Prozessen wäre diese Annahme falsch — deshalb steht sie
ausdrücklich im Code.

**Flache Modulstruktur statt vier Schichten** (§24). Die hexagonale Trennung bleibt inhaltlich:
die Domäne (`model`, `event`, `state`) kennt weder Dateisystem noch LLM, die Adapter liegen außen
(`store`, `executor`, `cli`). Aber zwanzig Dateien für ein Crate, dessen Geschwister mit neun
auskommen, widerspricht den Coding-Guidelines §1/§3.

**ID-Vergabe nur innerhalb des Schreiber-Locks.** `WorkStore::submit_with` baut das Ereignis erst
im Lock aus dem dann gültigen Zustand. Der Grund ist konkret: der Agent-Kern führt mehrere
Tool-Aufrufe *einer* Modellantwort parallel aus, und zwei gleichzeitige `work_add_item` hätten
sonst dieselbe ID berechnet — das zweite Item hätte das erste überschrieben und wäre lautlos
verschwunden.

**Ein Prüfkommando des Modells braucht eine Freigabe, eines vom Bediener nicht.** Der Runner
startet ein `AutomatedTests`-Kommando direkt als Prozess. Setzt es das Modell (`work_add_item`,
`verify_command`), wäre das ein Weg an der Freigabe von `run_shell` und an `--dry-run` vorbei.
Deshalb fragt der Runner für solche Kommandos den Executor (`AgentExecutor::
approve_verify_command`): `CodingAgentExecutor` lehnt unter `--dry-run` ab und reicht das Kommando
sonst unverändert an dieselbe `ApproveFn` wie `run_shell` (Allowlist, `-y`, Rückfrage). Ein
Kommando aus `--items`/`--verify-command` läuft wie bisher ohne Rückfrage. Die Herkunft steht im
Journal: `work_add_item` journalt `WorkItemAddedByAgent` statt `WorkItemCreated` (wirkt gleich,
merkt sich das Item zusätzlich in `WorkState`). Ein eigenes Ereignis statt eines Feldes am Item,
weil Anlegen und Herkunft EIN Journal-Eintrag sein müssen. Ohne Freigabe schließt das Item OHNE
Prüfung ab — kein Fehlschlag, der Agent hat nichts falsch gemacht —, und der Grund steht in der
Zusammenfassung des Versuchs (also im Journal) und als Hinweis auf stderr. Der Default der
Trait-Methode lehnt ab: ein Executor ohne eigene Freigabe winkt nichts durch. Items aus einem
Journal von vor dieser Änderung gelten als vom Bediener angelegt.

**Journal und Sperre sind für den Item-Agenten gesperrt.** Beide liegen im Workspace, in dem der
Agent schreiben darf. `agentkit work run` hängt deshalb `work.jsonl` und `work.lock` fest an die
`protect_paths` jedes Item-Agenten — als bloße Dateinamen, damit die Sperre unabhängig von der
Schreibweise des Pfades greift. Das hält `write_file`/`edit_file` zuverlässig ab. `run_shell`
deckt die Sperre des Kerns nur heuristisch ab (Umleitungen, `sed -i`, `cp`/`mv`), und für Pfade
unter einem Punkt-Verzeichnis wie `.agentkit/` greift diese Heuristik derzeit gar nicht; ein
`rm` oder `git clean` erkennt sie nie. Zusätzlich lehnt `Journal::open` ein Ereignis ab, dessen
`seq` nicht hinter seinem Vorgänger liegt — eine solche Zeile hat nicht der Store geschrieben.

**Eine Submission sticht den Sentinel.** Hat der Versuch `work_submit` gerufen, wertet der Runner
die Submission aus; `"(abgebrochen)"`/`"(keine Antwort)"`/`"(max_steps erreicht)"` entscheiden
nur, wenn keine vorliegt. Vorher verwarf die Laufzeit fertige Arbeit, wenn der Agent `work_submit`
in seinem letzten erlaubten Schritt rief.

**`max_work_items` gilt schon beim Anlegen.** `work_add_item` lehnt mit einem weichen Fehler ab,
sobald der Lauf das Limit erreicht hat — vorher pausierte erst der Scheduler den Lauf, nachdem ein
einziger Versuch beliebig viele Items angelegt hatte. Items, die die Laufzeit selbst anlegt
(Prüf-, Integrations-Item), sind davon nicht betroffen.

**Die Ausführungs-Strategie ist eine Executor-Eigenschaft, kein neues Konzept** (`work run
--strategy plan_execute`). `CodingAgentExecutor::strategy` legt agentkits vorhandenen
`plan_execute`-Phasen-Treiber (`agentkit::run_with_strategy`) um jeden Item-Versuch — planen,
Schritt für Schritt arbeiten, reflektieren, bei Bedarf umplanen; die Klassifikation des Runners
(Sentinels, `work_submit`) bleibt unverändert, weil der Treiber die Sentinels verbatim
durchreicht. Zwei bewusste Einschränkungen: **Planungs-Items laufen immer direkt** (ihr Auftrag
IST das Zerlegen — ein Treiber darum plante nur das Planen), und mit `plan_execute` begrenzen die
Phasen-Budgets des Treibers (`plan_max_steps`/`step_max_steps`) die Schrittzahl je Phase, nicht
`max_steps_per_attempt` je Versuch — das Wall-Time-Budget des Laufs greift unverändert. Technisch
braucht der Treiber einen `EventBus`, der Runner hört aber über einen Callback zu (Lease-
Verlängerung WÄHREND des Laufs): der Executor überbrückt das mit einem Scope-Thread und gibt den
`RunHandle` danach explizit frei (`RunHandle::release`), weil sonst der zuletzt aktive Bus einen
Sender am Leben hielte und der Event-Drain nie endete.

**Verifikation ist seit Phase 5a enthalten** (§10, siehe Abschnitt „Verifikation" unten) — **Human
Gates als eigenständiges Konzept, Swarm-Executor und Worktrees bleiben offen** (§13, §19, §20,
Phase 5b–7). `acceptance_criteria` wird ab Tag 1 mitgeführt und ins Arbeitspaket gerendert; das war
schon vor Phase 5a der Anker, an dem die Verifikation ansetzt.

## Verifikation (Phase 5a/5b)

Ein Agent ist Autor, nicht automatisch Prüfer seiner eigenen Arbeit (§31 des Konzepts). Jedes
`WorkItem` trägt dafür eine `VerificationPolicy`:

```rust
pub enum VerificationPolicy {
    None,
    AutomatedTests { command: String },
    IndependentAgent,
    HumanApproval,
}
```

- **`None`** (Default) — der Regelfall: ein erfolgreicher Versuch schließt das Item direkt ab, wie
  vor Phase 5a.
- **`AutomatedTests { command }`** — ein Kommando läuft SOFORT nach einem erfolgreichen Versuch im
  Workspace des Laufs, **ohne Shell** (`std::process::Command` direkt, naiv an Leerraum in Programm
  + Argumente gesplittet — keine Anführungszeichen-/Escaping-Unterstützung). Exit 0 schließt das
  Item ab; sonst gilt die letzte, gekürzte Ausgabezeile als Grund, und der Versuch wird wie ein
  regulärer fachlicher Fehlschlag behandelt (`attempt_count` steigt, `FailureKind::VerificationFailure`).
  Ein eigenes Zeitlimit (`VERIFY_COMMAND_TIMEOUT_SECS`, `runner.rs`) schützt vor einem hängenden
  Kommando.
- **`IndependentAgent`** (Phase 5b) — nach einem erfolgreichen Versuch legt die Laufzeit AUTOMATISCH
  ein eigenes Prüf-Item an (`runner::spawn_review_item`): Kind `Review`, hohe Priorität (kommt beim
  nächsten Scheduler-Durchlauf sofort dran, da es keine offene Abhängigkeit hat), Rolle `reviewer`
  (read-only, siehe `agent_framework_rs::roles::builtin_roles`) und — zwingend — selbst
  `verification_policy: None`. Letzteres ist keine Nebensächlichkeit: ein Prüf-Item, das seinerseits
  geprüft werden müsste, wäre ein unendlicher Regress. Das Prüf-Item bekommt außerdem bewusst KEINE
  Abhängigkeit auf das geprüfte Item — Abhängigkeiten sind FinishToStart auf `Completed`
  (`state::ready_items`), das geprüfte Item steht zu diesem Zeitpunkt aber erst auf
  `AwaitingVerification`; eine Abhängigkeit würde das Prüf-Item dauerhaft unbereit machen. Es entsteht
  ohnehin erst, wenn die zu prüfende Arbeit schon vorliegt. Die Beschreibung des Prüf-Items enthält
  die Akzeptanzkriterien des geprüften Items WÖRTLICH sowie die Artefaktpfade des geprüften Versuchs
  (mit dem Hinweis, sie über `read_file` zu lesen) und die ausdrückliche Ansage, dass NICHT der
  Gesprächsverlauf des Autors vorliegt (§26 Phase 6) — der Prüfer soll die Kriterien anhand der
  Artefakte beurteilen, nicht die Arbeit noch einmal machen. Sein Urteil meldet der Prüf-Agent über
  das Tool `work_verdict` (`{approved, reason}`, `reason` PFLICHT auch bei Zustimmung) — registriert
  NUR in einem Prüf-Item (`WorkItem::verifies` gesetzt), dasselbe Fähigkeit-bei-Registrierung-Muster
  wie `work_claim`/`ctx.gateway`. Zustimmung schließt das geprüfte Item ab (`Completed`); Ablehnung
  läuft über denselben Mechanismus wie jeder andere fachliche Fehlschlag
  (`recovery::finish_failed_attempt`, `FailureKind::VerificationFailure`). Ruft ein Prüf-Item
  `work_submit`, OHNE je `work_verdict` gerufen zu haben, bliebe das geprüfte Item sonst für immer in
  `AwaitingVerification` hängen — der Runner erkennt das nach dem Versuch und wertet es als Ablehnung
  mit dem Grund „Prüfer hat kein Urteil abgegeben".
- **`HumanApproval`** — der Runner rührt das Item nach einem erfolgreichen Versuch nicht mehr an; es
  wartet in `AwaitingVerification`, bis `agentkit work approve <item> -p <projekt>` oder
  `agentkit work reject <item> -p <projekt> --reason TEXT` entscheidet. Andere, unabhängige Items
  laufen währenddessen weiter; erst wenn NICHTS anderes mehr vorankommt, hält der Lauf mit
  `CompletionReason::AwaitingVerification` an (CLI-Exit ≠ 0 — der Auftrag ist nicht erledigt) und
  nennt genau die wartenden Items samt Freigabebefehl.

Gesetzt wird die Policy nur beim Anlegen über die CLI: `agentkit work create --verify-command "…"`
setzt `AutomatedTests` für jedes Item aus `--items`, das keine eigene Angabe trägt; die
`--items`-Datei selbst kennt ein Feld `verification` (`"none"`, `{"automated_tests": "…"}`,
`"human_approval"`, `"independent_agent"`) je Eintrag. `work_add_item` (das Tool des ausführenden
Agenten) kann über `verify_command` ebenfalls `AutomatedTests` setzen — ohne das prüfte niemand
ein zur Laufzeit erzeugtes Folge-Item nach. Ein Kommando vom MODELL ist aber kein Kommando vom
Bediener (siehe „Ein Prüfkommando des Modells braucht eine Freigabe" oben). Ein vom
Scheduler automatisch angelegtes Prüf-Item zählt außerdem NICHT als "echter Projektfortschritt"
(`scheduler::decide`, `has_real_progress`) — dieselbe Ausnahme wie bei einem Planungs-Item: sonst
würde ein geprüftes Item, dessen Versuche komplett ausgeschöpft sind (`Failed`), den Lauf trotzdem
als `AllItemsDone` melden, nur weil sein eigenes, automatisch erzeugtes Prüf-Item erfolgreich
abgeschlossen hat.

### Neuer Zustand `AwaitingVerification`, bewusst KEIN `Verified`

`AwaitingVerification` hat echte Dauer — ein Human-Gate oder ein automatisiertes Prüfkommando kann
Sekunden bis Tage offen bleiben. Ein `Verified`-Zwischenzustand hätte dagegen keine eigene Dauer:
die Prüfung schlägt entweder fehl (zurück nach `Pending`, ein neuer Versuch) oder das Item ist
fertig (`Completed`) — genau dieselbe Linie wie beim bereits entfernten `Claimed` zwischen `Pending`
und `Running` (siehe oben).

Die Übergangsmatrix bekommt `Running → AwaitingVerification` sowie `AwaitingVerification →
{Completed, Pending, Canceled}` — plus, über die vier im Konzept genannten Pfeile hinaus, bewusst
auch `AwaitingVerification → Failed`: eine ABGELEHNTE Prüfung ist fachlich derselbe
„Fehlschlag"-Mechanismus wie ein regulärer `WorkItemFailed` (`recovery::finish_failed_attempt`
entscheidet einheitlich, ob noch ein Versuch übrig ist — CLI `reject` und der Runner-Pfad für
`AutomatedTests` teilen sich denselben Code, statt die Entscheidung zweimal zu pflegen).

Ein Item in `AwaitingVerification` ist weder `ready` noch terminal: `state::ready_items` lässt es
aus (es ist nicht `Pending`), `state::is_run_complete` ebenso (es steht nicht in der
Terminal-Liste) — beide Funktionen brauchten dafür keine Änderung, die Eigenschaft ergibt sich
allein daraus, dass der neue Zustand in keiner der beiden Aufzählungen auftaucht. `scheduler::decide`
bekommt dagegen eine neue `Decision::AwaitingVerification(Vec<WorkItemId>)`: läuft nichts mehr und
ist nichts endgültig blockiert, aber mindestens ein offenes Item steckt in `AwaitingVerification`,
ist der Lauf weder `Done` noch `Blocked` — er wartet.

### Lease bleibt bewusst stehen

Anders als `WorkItemCompleted`/`WorkItemFailed`/`WorkItemReleased` entfernt
`WorkItemSubmittedForVerification` das Lease NICHT: solange ein Item wartet, ist das Lease der Weg,
den zugehörigen Versuch wiederzufinden (CLI `approve`/`reject`, der Recovery-Lückenschluss unten).
Damit ein tagelang legitim wartendes `HumanApproval`-Gate nie durch einen Zeitablauf zerstört wird,
schließen `state::expired_leases` und `recovery::recover_matching` Leases von
`AwaitingVerification`-Items STRUKTURELL aus — unabhängig von `expires_at_ms` und unabhängig davon,
ob `recover` oder das großzügigere `recover_all` läuft.

### Absturz-Lücken, die Recovery schließt

Derselbe Grundsatz wie überall in diesem Crate — Recovery VOLLENDET halb geschriebene Übergänge,
statt sie zu verwerfen — gilt auch für die Verifikation:

1. **Zwischen `WorkItemSubmittedForVerification` und dem Prüfergebnis.** Bei `AutomatedTests` löst
   die Prüfung SYNCHRON im selben Versuch auf — ein Item darf diesen Zustand über einen Neustart
   hinweg also nie mit `verification == None` erreichen. Taucht das doch auf, ist das der
   Fußabdruck eines Absturzes mitten in der Prüfung: kein fachlicher Fehlschlag, also Freigabe nach
   `Pending` ohne `attempt_count`-Erhöhung (derselbe Grundsatz wie bei jedem anderen unterbrochenen
   Versuch). Bei `HumanApproval` ist `verification == None` dagegen der NORMALE, legitim wartende
   Fall — Recovery fasst ihn nicht an.
2. **Zwischen `VerificationApproved`/`VerificationRejected` und dem folgenden
   `WorkItemCompleted`/`WorkItemFailed`.** Recovery holt den fehlenden zweiten Schritt nach — ein
   genehmigter Versuch wird `Completed`, ein abgelehnter durchläuft
   `recovery::finish_failed_attempt`, genau wie ein regulärer (nicht abgestürzter) Fehlschlag.
3. **Zwischen `WorkItemCompleted` und `ClaimsPromoted` (Phase 5b).** Die Promotion war noch nicht
   einmal versucht — oder ein vorheriger Versuch ist am Gateway gescheitert (journalt bei einem
   Fehlschlag bewusst nichts, siehe oben). `recover_pending_promotions` erkennt das rein am Item
   (`Completed`, Policy `!= None`, `claims_promoted == false`), unabhängig von jedem Lease, und
   versucht die Promotion erneut. Bewusst eine EIGENE Funktion, nicht in `recover`/`recover_all`
   verdrahtet: die beiden bräuchten sonst ein `GraphGateway`-Argument, das ihre bestehenden,
   graph-unabhängigen Aufrufer nicht betrifft — `agentkit work run`/`resume` ruft sie direkt NACH
   `recover_all` auf, nur wenn ein Graph angebunden ist.

### Promotion verifizierter Claims (§11, Phase 5b)

Ein Item, das eine ECHTE Verifikation durchlaufen hat (`verification_policy != None`, gleich
welche), promotet nach seinem `Completed` die Claim-IDs ALLER seiner Versuche in den Canonical
Graph — über `GraphGateway::promote(&self, claim_ids: &[String]) -> Result<usize, String>` und den
gemeinsamen Kern `graph::promote_after_completion`. Mit `VerificationPolicy::None` gab es nie eine
Prüfung, die eine Promotion rechtfertigt — dort promotet die Laufzeit strukturell NICHTS. Erfolg
journalt `WorkEvent::ClaimsPromoted` und setzt `WorkItem::claims_promoted`; ein Fehlschlag journalt
bewusst NICHTS (dieselbe Haltung wie `GraphAgent::record_episode` in `agentkit_graph`: ein nicht
erreichbarer oder nicht schreibbarer Graph — z. B. `--graph-readonly` — darf bereits abgeschlossene
Arbeit nicht entwerten) und erscheint stattdessen als Warnung (`WorkProgress::Note` im Runner, eine
stderr-Zeile in der CLI). Drei Aufrufer: der Runner selbst (`AutomatedTests`, `IndependentAgent` über
`work_verdict`), `agentkit work approve` (`HumanApproval`) und
`recovery::recover_pending_promotions` — letzteres schließt zwei weitere Absturzlücken, siehe unten.
Der Adapter in `agentkit_app/src/work_graph.rs` setzt `promote` über
`agentkit_graph::GraphWriteCommand::PromoteClaim` um; ohne Promotionsziel (`--graph-readonly`) ist
das ein sofortiger, weicher Fehlschlag, kein Absturz.

### Plan freigeben: `work run --mode plan`

Derselbe Modus PLAN wie beim Einzelagenten (`agentkit --mode plan`), übertragen auf die
Zerlegung: das Planungs-Item entsteht mit `HumanApproval` (`ensure_reviewed_plan_item`), und
`scheduler::decide` startet kein Folge-Item, solange es auf Freigabe wartet. Ohne diesen Halt
liefen die angelegten Items sofort los — sie hängen nicht vom Planungs-Item ab. Der Lauf endet
mit `awaiting_verification` und zeigt den Plan: die angelegten Items mit Abhängigkeiten und
Akzeptanzkriterien.

```bash
agentkit work run mein-vorhaben --mode plan       # zerlegt, zeigt den Plan, hält an
agentkit work approve W-1 -p mein-vorhaben        # freigeben …
agentkit work reject W-1 -p mein-vorhaben --reason "erst die Tests"   # … oder neu zerlegen lassen
agentkit work run mein-vorhaben                   # weiter
```

Ein `reject` verwirft die Items, die die Zerlegung angelegt hat (`Pending` → `Canceled`), als
Kaskade in `state::apply` an `VerificationRejected`. Es gibt also kein neues Ereignis, genau wie
der Laufabbruch an `RunCanceled` kaskadiert. Der nächste Planungsversuch bekommt die Begründung
über `previous_failures` und zerlegt neu. `--mode plan` wirkt nur beim ersten Lauf: die Policy
des Planungs-Items steht dann im Journal. `work run --strategy planning` lehnt die CLI ab, weil
ein Item, das nur plant, nie zu `work_submit` käme.

### Bewusst NICHT enthalten: `Composite`, `PeerReview`

§10 des Konzepts nennt zusätzlich `PeerReview` und `Composite(Vec<…>)`. Beide entfallen weiterhin
(Guidelines §4, YAGNI):

- **`PeerReview`** setzt mehrere Agenten/Rollen voraus, die sich gegenseitig prüfen — das gehört zur
  Swarm-Integration (Phase 6), nicht zu dieser Phase.
- **`Composite(Vec<VerificationRequirement>)`** würde mehrere Policies kombinieren (z. B. Tests UND
  ein Review) — additiv nachrüstbar, sobald mindestens zwei der Einzel-Policies real im Einsatz
  sind und ein konkreter Fall genau diese Kombination braucht. Ohne diesen zweiten Nutzer wäre es
  Konfigurierbarkeit auf Vorrat.

## Schwarm-Anbindung (Phase 6)

Ein Work Item kann statt von einem einzelnen Agenten von einem kurzlebigen Schwarm bearbeitet
werden (§13 des Konzepts): „Der Schwarm bearbeitet eine begrenzte Arbeitsphase. `agentkit-work`
verwaltet das gesamte langfristige Vorhaben." Dafür trägt jedes `WorkItem` ein Feld

```rust
pub enum ExecutorKind {
    SingleAgent,
    Swarm { template: String },
}
```

mit `#[serde(default)]` (Default `SingleAgent`), damit ein Journal aus der Zeit vor Phase 6 weiter
lesbar bleibt.

**Auch hier: Port statt Dependency.** Dieses Crate importiert `agentkit_swarm` weiterhin NICHT
(CLAUDE.md, Einbahnrichtung) — der Kniff ist, dass der nötige Port schon existiert:
[`AgentExecutor`](src/executor.rs). Der Runner (`runner::run_attempt`) ruft ihn unverändert als
`&dyn AgentExecutor` auf und weiß nicht, ob dahinter ein einzelner Agent oder ein ganzer Schwarm
steckt — an `run_to_completion`/`run_attempt` ändert sich für Phase 6 nichts. Wer den Executor
tatsächlich baut, entscheidet `agentkit_app` (das einzige Crate, das sowohl `agentkit_work` als
auch `agentkit_swarm` kennt, §25): `SwarmWorkExecutor` (`agentkit_app/src/work_swarm.rs`) baut PRO
VERSUCH einen frischen Schwarm aus einer Vorlage und übersetzt das `SwarmResult` in die
Executor-Antwort; `DispatchingExecutor` wählt anhand von `pkg.item.executor` zwischen ihm und dem
gewöhnlichen `CodingAgentExecutor`. Die Naht dorthin ist `WorkCliDeps::build_executor`
(`cli.rs`) — eine optionale Closure, die `cmd_run`s Einzelagenten-Executor in den tatsächlich
benutzten überführt; `None` (z. B. in den Tests dieses Crates) lässt `cmd_run` exakt wie vor
Phase 6 laufen.

**Kein Tool-Argument dafür.** `work_add_item` (das Tool des ausführenden Agenten) bekommt KEIN
`executor`-Feld — welche Vorlagen es überhaupt gibt, weiß nur das Frontend, dieses Crate validiert
den Vorlagennamen nicht einmal. Ein Modell, das sich selbst einen Schwarm verordnen könnte, wäre
genau die Eskalation, die diese Laufzeit deterministisch halten soll (§31: „Deterministische
Runtime, agentische Problemlösung"). Ein zur Laufzeit erzeugtes Folge-Item bekommt deshalb immer
`SingleAgent` — der Executor wird ausschließlich vom OPERATOR gesetzt: über `--items`
(Feld `executor`: `"single_agent"` oder `{"swarm": "<vorlage>"}`) bzw. eine künftige CLI-Option.

**Die Vorlagen selbst liegen im Frontend, nicht hier.** Ob es die Vorlage `"review"` oder
`"architecture"` gibt, zwei Mitglieder oder vier, mesh oder Kette — das alles ist eine Entscheidung
von `agentkit_app::work_swarm`, nicht von `agentkit_work`. Genau wie beim Graph-Gateway
(„agentkit_work kennt seine Geschwister nicht") bliebe dieses Crate sonst an die Existenz und
Namensgebung von `agentkit_swarm`-Konzepten gekettet. Ein unbekannter Vorlagenname ist deshalb ein
Fehler des KOMPONIERENDEN Executors (`SwarmWorkExecutor`), nicht dieses Crates.

**Ehrliche Degradation statt Abbruch.** Ist kein Schwarm verfügbar (praktisch: `--no-swarm` des
Frontends), lässt `DispatchingExecutor` ein `Swarm`-Item NICHT scheitern — es läuft mit dem
Einzelagenten weiter und meldet die Degradation über die vorhandene `on_event`-Naht: der Runner
reicht jedes `AgentEvent` unverändert als `WorkProgress::Agent` an den Aufrufer weiter
(`runner::run_attempt`), unabhängig von der CLI-Anzeigeoption `--steps` — dieselbe Naht, über die
auch Schritte/Tool-Aufrufe für die Lease-Verlängerung gezählt werden. `agentkit_work` musste dafür
NICHTS Neues anbieten; das ist der Sinn von „sorge nur dafür, dass die Note-Naht dafür reicht".

**Verhaltenskontrakte des Kerns wiederverwendet, nicht neu erfunden.** `SwarmWorkExecutor`
übersetzt `CompletionReason::Consensus` in den Vorschlagstext (plus Zustimmungszahl) als
Versuchsergebnis, ein erreichtes Nachrichten-/Laufzeitlimit in den Sentinel `"(max_steps
erreicht)"` und einen Abbruch in `"(abgebrochen)"` — dieselben Sentinel-Strings, die
`CodingAgentExecutor`/`runner::run_attempt` schon für den Einzelagenten kennen (§ „Verhaltenskontrakte
sind API" in `CODING_GUIDELINES.md`). Kein zweiter Klassifikationspfad für „das Item ist am Limit
gescheitert" oder „wurde abgebrochen".

**Die Laufzeitgrenze kommt aus dem Budget des Vorhabens, nicht aus einer festen Konstanten**
(Befund 2 der Handprobe, §17/§28). `SwarmWorkExecutor` baute den Schwarm zuvor immer mit
`agentkit_swarm::dynamic::SwarmLimits::default().max_runtime_s` (900s) — einem Wert, der für den
GANZ ANDEREN Anwendungsfall des dynamischen `swarm`-Tools gedacht ist und das Budget des Vorhabens
(`WorkBudget::max_wall_time_secs`) vollständig ignorierte: ein einzelnes Schwarm-Item konnte damit
einen Lauf mit einem viel knapperen Budget klaglos überziehen. Ist `max_wall_time_secs` gesetzt,
trägt `AgentWorkPackage::remaining_wall_secs` jetzt die verbleibende RESTLAUFZEIT des Laufs (der
Runner setzt es aus `WorkRun::started_at_ms`/`WorkBudget`, bevor er den Executor ruft) — bewusst die
Restzeit, nicht das volle Budget, sonst könnte ein einzelnes Item es allein aufbrauchen. Das
Arbeitspaket ist dafür die passende Naht: `agentkit_work` muss `agentkit_swarm` dazu nicht
kennenlernen (CLAUDE.md, Einbahnrichtung), derselbe Mechanismus, über den auch der Wissensgraph-
Recall den Executor erreicht. Ohne Wall-Time-Budget gilt eine dokumentierte Obergrenze
(`SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS = 900s` in `agentkit_app::work_swarm`, dieselbe
Größenordnung wie `SwarmLimits::default()`, aber eine eigene, unabhängig begründete Konstante für
diesen Anwendungsfall) — ein Schwarm-Item soll auch OHNE jedes Budget nie unbegrenzt laufen. MIT
einem Budget ist die Grenze bewusst NICHT auf 900s gedeckelt (Befund des Code-Reviews: ein Deckel
`min(Restzeit, 900s)` hätte einem großzügig konfigurierten Lauf denselben Fehler wieder
eingebaut, den diese Korrektur beheben soll) — dann bestimmt allein die vom BEDIENER gewählte
`max_wall_time_secs` die Obergrenze, so groß wie er sie selbst gewählt hat; die 900s-Konstante
gilt nur, wenn er GAR keine Wahl getroffen hat. Läuft die Grenze ab, meldet der Schwarm
`CompletionReason::MaxRuntimeReached`, das genau wie `MessageLimitReached` auf den Sentinel
`"(max_steps erreicht)"` abgebildet wird — eine Zeitüberschreitung zählt damit als Limit, nicht
als Absturz, und verbraucht regulär einen der `max_attempts`-Versuche.

## Git-Isolation (Phase 7)

Unter `--git-isolation` bekommt jedes schreibende Work Item einen eigenen Git-Branch UND einen
eigenen Git-Worktree, in dem sein Agent arbeitet (§19 des Konzepts). Ein erfolgreicher Versuch
committet auf dem Item-Branch, ein gescheiterter verwirft seine Änderungen, und ein
deterministischer Integrationsschritt mergt am Ende alle Branches in den Ausgangsbranch. Der
Arbeitsbaum des Bedieners wird bis zu dieser Integration nicht angefasst.

**Worktree statt Branchwechsel.** Die erste Fassung dieser Phase wechselte den Branch im EINEN
Arbeitsbaum und wieder zurück — das reichte, solange es genau einen Worker gab, ließ aber nie zwei
Schreiber gleichzeitig zu. Seit den parallelen Workern (Abschnitt unten) arbeitet jedes Item in
`<projektverzeichnis>/worktrees/<item-id>` (`runner::GitAttemptCtx`, `git::ensure_worktree`). Das
gilt bewusst auch mit einem einzigen Worker: zwei Isolationsmechanismen nebeneinander wären
doppelter Code und doppelte Tests für dasselbe Ergebnis. Die Branch-Namen sind dieselben geblieben,
ein älteres Vorhaben setzt nahtlos fort.

Der Grund, aus dem Worktrees zunächst zurückgestellt waren, ist damit gelöst: **die Artefakte der
Vorgänger liegen außerhalb des Worktrees**, und `read_file` ist auf den Workspace eingehegt. Der
Runner gibt dem Item-Agenten deshalb das Artefaktverzeichnis als nur lesbare Wurzel mit
(`AgentWorkPackage::read_roots` → `CodingTools::with_read_roots`) und nennt die Pfade der
Vorgänger-Artefakte im Arbeitspaket absolut.

### Zuschnitt

`WorkProject` bekommt ein Feld `git_isolation: bool` (`#[serde(default)]`, Default `false`) —
gesetzt über `agentkit work create --git-isolation`, nie nachträglich änderbar. Aus ist der
Default: ein Vorhaben, das nicht in einem Git-Repository liegt, darf davon nichts merken. `create`
prüft `git rev-parse --is-inside-work-tree` und lehnt sonst sofort ab (kein Verzeichnis entsteht),
`run`/`resume` prüfen dieselbe Bedingung noch einmal (`WorkProject::workspace` kann sich zwischen
Anlage und Lauf ändert haben) — beide Male mit einer klaren deutschen Meldung statt eines
kryptischen Git-Fehlers mitten im ersten Versuch.

Ist Isolation an, gilt für jedes Item, dessen Art NICHT rein lesend ist —
[`WorkItemKind::writes_workspace`](src/model.rs) schließt `Review` (bewertet nur) und `Planning`
(zerlegt nur) aus, dazu `Integration` selbst (siehe unten: es MERGT, statt etwas zu produzieren,
das seinerseits gemergt werden müsste). Alle anderen Arten (`Discovery`, `Analysis`,
`Implementation`, `Test`, `Documentation`) gelten als schreibend, auch wenn nicht jede von ihnen
tatsächlich Dateien ändert — die Entscheidung fällt an der Art, nicht am beobachteten Verhalten,
damit sie deterministisch und ohne Sonderfälle bleibt.

**Beim Start des Laufs und vor der Integration** (`runner::ensure_integration_target_ready`) muss
der Arbeitsbaum des Bedieners die Integration aufnehmen können — sonst ein harter, deutscher Fehler,
der den Lauf abbricht. Das ist ein Konfigurations-/Bedienungsfehler, kein fachlicher, und wird
deshalb nicht wie ein gescheiterter Versuch behandelt (kein Retry, kein `attempt_count`-Verbrauch):

- **Der Arbeitsbaum ist nicht sauber** (`git status --porcelain` nicht leer). Uncommittete
  Änderungen sähe kein Agent — ein Worktree startet auf einem Commit —, und der Merge am Ende
  könnte an ihnen scheitern. Geprüft wird beim Start, damit das nicht erst nach Stunden bei der
  Integration auffällt, und unmittelbar vor dem Merge noch einmal (der Bediener darf während des
  Laufs im eigenen Arbeitsbaum weiterarbeiten, muss ihn bis zur Integration aber committen). Die
  zweite Prüfung liegt VOR dem Anlegen des Integrations-Items: scheitert sie, versucht ein
  `resume` die Integration erneut.
- **Ein losgelöster HEAD.** Ohne ausgecheckten Branch gibt es nichts, wohinein die Integration
  mergen könnte. `create --git-isolation` lehnt das schon beim Anlegen ab.

Entfallen ist die frühere Prüfung „keine uncommittete Änderung im Repository AUSSERHALB des
Workspace": sie schützte davor, dass der Rollback eines Fehlversuchs (`git reset --hard`) fremde
Arbeit nebenan löscht. Der Rollback trifft jetzt nur noch den Worktree des Items.

**Die eigene Buchführung der Laufzeit (`.agentkit/`, siehe „Ablage" oben) ist von dieser Prüfung
UND vom Commit ausgenommen — bewusst, nicht nur beim Staging.** `agentkit work create` legt
`work.jsonl` schon VOR dem ersten Versuch im Workspace an, also innerhalb desselben Arbeitsbaums,
den die Sauberkeitsprüfung betrachtet — und dort liegen auch die Worktrees der Items. Ohne Ausnahme
wäre der Arbeitsbaum nie sauber, sobald ein Vorhaben existiert, und `--git-isolation` wäre ohne ein
vom Bediener selbst gepflegtes `.gitignore` für `.agentkit/` unbenutzbar — schon der Start schlüge
mit „Arbeitsbaum nicht sauber" fehl. `git::is_clean` und `git::commit_all` schließen `.agentkit` deshalb über dieselbe
Pathspec-Ausschlussregel aus (`git status`/`git add` mit `-- . ":(exclude).agentkit"`), nicht per
Nachfilterung der Textausgabe — das ist der Mechanismus, den `git status --porcelain` selbst dafür
anbietet, robuster als ein String-Filter auf die Statuszeilen. Ein Item-Commit enthält dadurch NUR
die fachliche Änderung, nie das Journal, das sie beschreibt — sonst würde jeder Commit um den
kompletten Laufzeitzustand wachsen, und zwei Item-Branches, die beide `work.jsonl` ändern, hätten
bei JEDEM Merge einen garantierten Konflikt im Journal.

**Vor dem Versuch** (`runner::GitAttemptCtx::prepare`) wird der Worktree des Items auf dem Branch
`work/<projekt-id>/<item-id>` angelegt (erster Versuch) oder wiederverwendet (Retry, Fortsetzung
nach einem Abbruch oder Absturz) — dabei werden uncommittete Reste verworfen, der letzte Commit
des Branches bleibt. Der Agent arbeitet im Worktree; liegt der Projekt-Workspace in einem
Unterordner des Repositories, arbeitet er im selben Unterordner des Worktrees. Ohne Abhängigkeiten ist der Startpunkt
`WorkRun::base_revision` (schon vor Phase 7 vorhanden, per `git rev-parse HEAD` bei `create`
erfasst).

**Ein abhängiges Item startet auf dem Stand seiner Vorgänger** (Issue #79). Vorher startete JEDER
Item-Branch auf `base_revision`; ein Item „Tests für W-2" sah den Code von W-2 nicht, weil der bis
zur Integration nur auf dessen Branch lag — es baute ihn nach, und die Integration endete im
Konflikt. Jetzt mergt `prepare` die Branches der abgeschlossenen, git-isolierten Abhängigkeiten
in den Item-Branch, in Reihenfolge der Item-ID und mit Vorspulen (`git::merge_fast_forward`):

- Mit genau EINEM Vorgänger steht der Branch danach exakt auf dessen Stand, ohne eigenen
  Merge-Commit. Mit mehreren entsteht je weiterem Vorgänger ein Merge-Commit der Laufzeit.
- Transitiv braucht es keine eigene Suche: der Branch von B enthält A bereits. Durch eine nicht
  isolierte Abhängigkeit (`Review`, `Planning` — sie hat keinen Branch) wird hindurchgegriffen auf
  DEREN Abhängigkeiten.
- Das Mergen läuft bei jedem Versuch gleich und ist ab dem zweiten ein No-Op. Deshalb wird der
  Startpunkt nirgends gespeichert: Rollback-Ziel ist der Stand nach dem Mergen (kein neues Feld im
  Journal, laufende ältere Vorhaben brechen nicht; ihre noch offenen Items bekommen ihre Vorgänger
  beim nächsten Versuch dazu).
- **Widersprechen sich die Vorgänger** (Merge-Konflikt), bricht `prepare` den Merge ab
  (`git merge --abort`), wechselt zurück auf den Ausgangsbranch und lässt den Versuch scheitern,
  BEVOR ein Agent läuft: `FailureKind::MergeConflict` mit den betroffenen Dateien — derselbe
  Ausgang wie beim Integrations-Item, über denselben Mechanismus wie jeder fachliche Fehlschlag
  (`recovery::finish_failed_attempt`). Der Konflikt ist deterministisch, die verbleibenden
  Versuche scheitern also sofort ebenso; danach ist das Item `Failed`, alles davon Abhängige
  endgültig blockiert, und die Integration scheitert am selben Konflikt — der Lauf endet
  `Blocked`.
- Die Integration bleibt unverändert: sie mergt weiter alle abgeschlossenen Item-Branches in
  Erzeugungsreihenfolge. Ein Branch, der schon in einem Nachfolger steckt, kommt dabei zuerst
  dran; der Nachfolger bringt danach nur noch seine eigenen Commits mit.
- Eine automatisierte Prüfung (`AutomatedTests`) läuft wie bisher auf dem Item-Branch und sieht
  damit jetzt auch die Vorgänger.

**Nach einem ERFOLGREICHEN Versuch** (`runner::record_success`, noch VOR jeder
Verifikationspolicy — der Commit gehört zum Agenten-Versuch selbst, unabhängig davon, was eine
eventuell folgende Prüfung später entscheidet): alles außer `.agentkit` wird gestagt und
committet (`git::commit_all`). `.agentkit` ist von Staging und `git clean` explizit
ausgeschlossen — es enthält das eigene, gerade OFFENE Journal dieser Laufzeit; ein Commit oder
`clean`, der es mit anfasst, würde dem laufenden Prozess seine eigene Datenbasis unter den Füßen
wegziehen. Der Commit wird als Artefakt festgehalten: neuer `ArtifactKind::GitCommit`. `rel_path`
trägt hier bewusst KEINEN Dateipfad — dieses Artefakt entsteht nie über `work_artifact`, also gibt
es keine Datei —, sondern den Namen des Item-Branches; die tatsächliche Commit-ID steht im neuen
Feld `WorkArtifact::commit_id` (ein sauber benanntes zweites Feld statt einer stillen Umnutzung
von `rel_path`, wie es die Aufgabenstellung verlangt). `AgentWorkPackage::build` schließt
`GitCommit`-Artefakte deshalb explizit aus der Liste der Vorgänger-Artefakte aus, die dort als
„mit `read_file` lesen" angekündigt wird — ein Branchname wäre dort irreführend. Ein eigenes
`WorkEvent` für die Anzeige gibt es dafür bewusst NICHT (Code-Review-Befund 1): ein zweites
Ereignis wäre ein No-op in `state::apply` gewesen und hätte nur dupliziert, was im Artefakt schon
steht. Eine sprechende Zeile in `agentkit work events` liefert stattdessen die Anzeige selbst
(`cli::journal_entry_kind` erkennt `artifact_created` mit `kind == "git_commit"` und nennt Commit
und Branch), statt die generische `artifact_created`-Zeile zu zeigen — Anzeigelogik gehört in die
Anzeige, nicht ins Journal-Schema. Hat der
Versuch NICHTS geändert (`git diff --cached --quiet` nach dem Staging leer), entsteht KEIN
Commit — das ist kein Fehler (eine Analyse ändert oft keine Dateien), sondern wird als
`WorkProgress::Note` gemeldet.

**Nach einem GESCHEITERTEN Versuch** (`record_failure`/`record_interrupted`): die Änderungen
werden verworfen (`git reset --hard <startpunkt>` + `git clean -fd -e .agentkit`), nicht
aufgehoben — das ist der Rollback aus §19. Ein gescheiterter Versuch soll den nächsten nicht
vergiften; die Diagnose steht ohnehin im Journal (`FailureInfo` am Attempt) und in den bereits
abgelegten Artefakten, nicht im Arbeitsbaum. Der nächste Versuch desselben Items (Retry) startet
dadurch garantiert sauber vom selben Startpunkt.

**Nach dem Versuch** wird der Worktree entfernt (`git worktree remove --force`), sobald für das
Item kein weiterer Versuch unmittelbar ansteht — ein Worktree ist eine volle Arbeitskopie samt
Build-Verzeichnis und soll nicht liegen bleiben. Der Branch bleibt: die Integration mergt ihn, die
Provenance verweist auf ihn. Stehen bleibt der Worktree nur nach einem Fehlschlag mit verbleibenden
Versuchen und nach einem Abbruch; der nächste Versuch übernimmt ihn bereinigt und spart sich den
Checkout.

**Ein hart beendeter Prozess** (SIGKILL, Stromausfall, Absturz — in der Praxis auch das zweite,
ERZWUNGENE Ctrl-C dieses Programms) lässt die Worktrees der gerade laufenden Items liegen. Das ist
unkritisch: der Arbeitsbaum des Bedieners ist davon nicht berührt, die Lease-Recovery gibt die
Items beim nächsten `run`/`resume` frei, und ihr nächster Versuch übernimmt den Worktree und
verwirft die uncommitteten Reste. Wurde ein Worktree-Verzeichnis von Hand gelöscht, räumt
`git worktree prune` die Verwaltung auf, bevor es neu angelegt wird.

`recovery::recover_git_branch` und der Hinweis `git_stray_branch_note` in `status`/`watch` sind
geblieben, obwohl dieser Zustand im laufenden Betrieb nicht mehr entsteht: sie holen den
Arbeitsbaum des Bedieners von einem Item-Branch zurück auf den Ausgangsbranch
(`WorkRun::base_branch`). Dorthin gerät er nur noch von Hand — oder als Hinterlassenschaft eines
Laufs, der vor der Umstellung auf Worktrees hart abgebrochen ist. Ein solches älteres Vorhaben wäre
ohne die Rückholung nicht fortsetzbar: sein Item-Branch gälte Git als „schon ausgecheckt", und der
Worktree ließe sich nicht anlegen.

### Das Integrations-Item

Sind alle Items eines Laufs terminal und ist Isolation an, legt die Laufzeit — falls noch nicht
geschehen — ein abschließendes Integrations-Item an: eine neue Variante `WorkItemKind::Integration`.
Das ist KEIN Agenten-Item: ein Merge ist Mechanik, keine fachliche Entscheidung (§31
„Deterministische Runtime, agentische Problemlösung"), die Laufzeit führt es SELBST und
SOFORT aus (`runner::run_integration_item`), ohne je einen `AgentExecutor` zu rufen. Eingehängt
NICHT über die Executor-Naht (die ist für Agenten-/Schwarmversuche gedacht, die ein
`AgentWorkPackage` bekommen und Tools aufrufen — ein deterministischer Merge braucht beides
nicht und hätte über diese Naht nur einen Fake-Agenten vorgetäuscht), sondern direkt im Runner:
`run_to_completion` ruft die Funktion GENAU dann, wenn `scheduler::decide` als Nächstes
`Decision::Done` melden würde — also alle anderen Items terminal sind. Claim, Merge-Ausführung
und Abschluss laufen trotzdem über dieselben `WorkEvent`s wie ein normaler Versuch
(`WorkItemClaimed`, `AttemptFinished`, `WorkItemCompleted`/`WorkItemFailed`), nur mit der
Laufzeit selbst als Agent (`RUNTIME_AGENT_ID = "runtime"`, dieselbe Idee wie
`AUTOMATED_TESTS_BY`/`HUMAN_BY`) — damit `agentkit work items`/`events` es genauso anzeigen wie
jedes andere Item. Gemergt werden die Branches aller erfolgreich abgeschlossenen, schreibenden
Items, in Erzeugungsreihenfolge (`seq`), mit `git merge --no-ff` in den Ausgangsbranch. Zielbranch
und gemergte Item-IDs stehen in der `summary` des `AttemptFinished` dieses Versuchs — ein eigenes
`WorkEvent::IntegrationMerged` dafür gab es früher, war aber ebenfalls ein No-op in `state::apply`
(derselbe Befund 2 wie bei `GitCommitted` oben): der Statusübergang läuft ohnehin über das normale
`WorkItemCompleted`, und eine zweite, sonst ungenutzte `summary` wäre reine Duplikation gewesen.

**Merge-Konflikt: kein automatischer Auflösungsversuch.** §28 nennt automatische Git-Merges
ausdrücklich als nicht im Umfang — ein Konflikt kann nur ein Mensch (oder ein Agent mit vollem
Kontext) sinnvoll auflösen, ein automatischer Versuch könnte fachlich falsche Ergebnisse
still-schweigend verschmelzen. Ein Konflikt bricht den Merge stattdessen ab (`git merge --abort`),
das Integrations-Item scheitert mit den betroffenen Dateien in der Fehlermeldung
(`FailureKind::MergeConflict`, eine neue, sechste Fehlerart mit einem echten Erzeuger — siehe
„Fünf Fehlerarten statt elf" oben, jetzt sechs), und der Lauf endet `Blocked` — nicht `AllItemsDone`,
selbst wenn jedes einzelne Work Item selbst erfolgreich war. `scheduler::decide` erkennt das
explizit: ein Integrations-Item mit Status `Failed` erzwingt `Decision::Blocked`, unabhängig vom
Fortschritt aller anderen Items. Das Integrations-Item hat `max_attempts: 1` — kein
automatischer zweiter Versuch, ein Merge-Konflikt ist eine Entscheidung für den Bediener, kein
Retry-Kandidat.

### Bewusst nicht gelöst in dieser Phase

- **Verifikation (`HumanApproval`/`IndependentAgent`) sieht unter Git-Isolation nur Artefakte,
  nicht den Diff.** Ein Prüf-Item (Kind `Review`) ist selbst nicht git-isoliert und läuft auf dem
  Ausgangsbranch — Code-Änderungen, die NUR auf dem Item-Branch committet sind (noch nicht
  gemergt), sind dort nicht sichtbar. Der Prüfer bekommt wie bisher die Artefaktpfade und
  Akzeptanzkriterien des geprüften Versuchs (`work_artifact`-Dateien liegen ohnehin außerhalb der
  Git-Versionierung, siehe `.agentkit`-Ausschluss oben, und bleiben deshalb über einen
  Branchwechsel hinweg erreichbar) — aber keinen Diff des tatsächlichen Quellcodes. Mit den
  Worktrees ist die Lösung einfach geworden (ein eigener, nur lesbarer Worktree auf dem
  Item-Branch für den Prüfer), sie ist aber ein eigener Schritt und noch nicht gebaut.
- **Kein Löschen alter Item-Branches.** `work/<projekt>/<item>` bleibt nach dem Merge bestehen —
  nützlich zur Nachverfolgung (Provenance, siehe `WorkArtifact::commit_id`), aber ein
  langlebiges Projekt mit vielen Items sammelt entsprechend viele Branches an. Kein Erzeuger für
  automatisches Aufräumen in diesem MVP.
- **Nicht verfolgte Dateien des Workspace fehlen im Worktree.** Ein Worktree enthält, was
  committet ist. Eine `AGENTS.md`, die nur lokal liegt, oder eine nicht eingecheckte
  Konfigurationsdatei sieht der Agent eines isolierten Items nicht — und damit auch nicht die
  Leitplanken aus einer solchen `AGENTS.md`. Was für die Agenten gelten soll, gehört ins Repository.
- **Jeder Worktree baut für sich.** Er ist eine volle Arbeitskopie mit eigenem Build-Verzeichnis
  (bei einem Rust-Projekt ein eigenes `target/`); der erste Build eines Items ist kalt. Worktrees
  terminaler Items werden entfernt, mehr unternimmt die Laufzeit nicht.
- **Ein Worktree bleibt liegen,** wenn ein unterbrochenes oder retrybares Item nie wieder
  aufgenommen wird. `git worktree remove <pfad>` entfernt ihn von Hand.

## Parallele Worker (Phase 9)

`agentkit work run <projekt-id> --parallel N` arbeitet bis zu N Work Items gleichzeitig ab. N ist
eine Obergrenze, kein Team: gestartet wird nur, was laut Abhängigkeiten bereit ist, und jedes Item
bekommt wie bisher einen frischen Agenten. Der Wert steht im Budget (`max_parallel_agents`, auch
über `create`/`budget --max-parallel N` setzbar) und gilt damit für spätere Läufe weiter. Der
Default ist 1 — dann läuft ein Vorhaben wie vor dieser Phase.

**Die Teamgröße folgt aus dem Plan, nicht umgekehrt.** Wie viele Agenten ein Vorhaben beschäftigen
kann, bestimmen die Abhängigkeiten seiner Items: zehn unabhängige Items beschäftigen zehn Agenten,
eine Kette aus zehn immer nur einen. Die Plan-Freigabe (`work run --mode plan`) und `work status`
nennen deshalb zwei Zahlen (`WorkState::plan_shape`):

```text
Bis zu 4 Items gleichzeitig, 3 Stufen nacheinander.
```

Die Stufe eines Items ist die Länge der längsten Abhängigkeitskette, die zu ihm führt;
„gleichzeitig" ist die Zahl der Items auf der vollsten Stufe. Das ist bewusst die einfache Rechnung
und nicht die exakte größte Menge voneinander unabhängiger Items — sie stimmt, wenn die Items einer
Stufe etwa gleich lang laufen, und ist für die Wahl von N genau genug. N selbst richtet sich nach
dem, was der Bediener sich leisten will: Modell-Kontingent, Kosten, Plattenplatz für die Worktrees.
Ein zu großes N bleibt ungenutzt. Die Planungsvorlage (`runner::create_plan_item`) zerlegt dazu
passend: unabhängige Arbeit in Items ohne Abhängigkeit, Items, die dieselben Dateien ändern, mit
Abhängigkeit untereinander.

### Ein Koordinator, N Worker

Der Koordinator ist der Thread, der `run_to_completion` aufruft. Nur er befragt den Scheduler,
claimt Items und ruft `on_progress`. Jeder Versuch läuft in einem eigenen Worker-Thread und meldet
Fortschritt und Ergebnis über einen `std::sync::mpsc`-Kanal zurück.

- **`std::thread::scope`, kein Pool, keine Runtime.** Ein Thread je laufendem Versuch, gestartet im
  Scope von `run_to_completion` — das Muster, das die Guidelines §4 für Nebenläufigkeit nennen. Der
  Scope garantiert, dass kein Worker den Aufruf überlebt. Auch mit N = 1 läuft der Versuch in einem
  eigenen Thread: ein Pfad statt zweier.
- **Der Claim bleibt beim Koordinator.** Er wird synchron journalt, bevor `scheduler::decide`
  erneut gefragt wird. Damit sieht die nächste Entscheidung das Item schon als laufend, und
  `decide` bleibt eine reine Funktion des Zustands — es gibt kein „schon vergeben"-Verzeichnis
  neben dem Journal. Deshalb ist `run_attempt` in drei benannte Schritte zerlegt: `claim_attempt`
  (Koordinator), `execute_attempt` und `record_attempt` (Worker).
- **Der Executor ist `Sync` und wird geteilt** (`AgentExecutor: Sync`). Alles Versuchsbezogene
  kommt als Argument; `CodingAgentExecutor`, `SwarmWorkExecutor` und `DispatchingExecutor` hielten
  ohnehin keinen Zustand je Versuch.
- **Jedes Agenten-Ereignis nennt sein Item** (`WorkProgress::Agent { item, attempt, event }`). Die
  CLI schloss das Item früher aus dem letzten `ItemStarted` — das geht nicht mehr, sobald sich die
  Ereignisse mehrerer Versuche mischen. Das Trace-Tag (`W-1#2`) ist dasselbe geblieben, der
  Betrachter (`agentkit_viz`) brauchte keine Änderung.
- **Jeder Worker hat eine eigene Kennung** (`worker-1` … `worker-N`), die im Journal am Versuch
  steht; `status`/`watch` zeigen alle laufenden Items (im JSON `running_items`; `running_item`
  bleibt für bestehende Leser das erste).

### Was nichts Neues mehr startet, bricht Laufendes nicht ab

Für jede Entscheidung des Schedulers außer `Run` gilt dieselbe Regel: läuft noch ein Versuch, wartet
der Koordinator auf dessen Ende und fragt neu; läuft keiner mehr, gilt die Entscheidung. Bezahlte
Arbeit wird damit nie weggeworfen:

- **Budget, Freigabe-Halt, Blockade:** der Lauf pausiert bzw. endet erst, wenn kein Versuch mehr
  läuft.
- **Harter Fehler** eines Versuchs (Konfiguration, Journal, Git): die übrigen laufen zu Ende und
  werden journalt, dann kommt der ERSTE Fehler zurück.
- **Abbruch (Ctrl-C):** wirkt wie bisher sofort — die Agenten beobachten denselben Schalter und
  beenden ihren Versuch selbst. Jeder wird als unterbrochen journalt und zählt nicht gegen
  `max_attempts`; ein `resume` setzt alle fort.

`Decision::AtCapacity` hieß mit einem Worker „der Lauf steht" und pausierte ihn. Jetzt heißt es
„gerade kann nichts Weiteres starten"; ob der Lauf steht, entscheidet der Koordinator daran, ob in
seinem Prozess noch etwas läuft.

### Zwei Regeln des Schedulers

Beide sind reine Eigenschaften des Zustands und stehen deshalb in `scheduler::decide`:

- **Ohne Git-Isolation höchstens ein Schreiber.** Dann teilen sich alle Items einen Arbeitsbaum;
  zwei schreibende Agenten überschrieben einander die Dateien. Lesende Items (`Review`, `Planning`)
  dürfen daneben laufen (§19 des Konzepts: `max_parallel_writers = 1`). Ob ein Item als schreibend
  gilt, sagt `WorkItemKind::writes_workspace` — dieselbe Eigenschaft, an der unter Git-Isolation
  der eigene Worktree hängt. Echte Parallelität bei Code-Arbeit braucht also `--git-isolation`.
- **Eine laufende Zerlegung läuft exklusiv.** Die Items, die ein Planungs-Item anlegt, hängen nicht
  von ihm ab und wären sofort bereit. Ein zweiter Worker finge sonst an, einen halben Plan
  auszuführen, während der Planer noch Items und Abhängigkeiten ergänzt; scheitert der
  Planungsversuch danach, liefe Arbeit an einem Plan, den es so nie gab. (Im Plan zu dieser Phase
  stand stattdessen, die Prüfung „Planungsversuch hat nichts angelegt" solle nur eigene Items
  zählen. Mit der Exklusivität ist der vorhandene Vergleich der Item-Zahl wieder verlässlich und
  blieb unverändert.)

### Freigabe-Rückfragen

Ohne `-y` fragt jeder Agent vor einem Shell-Kommando nach. Mit mehreren Workern überlagerten sich
diese Fragen auf dem Terminal, und eine Antwort landete bei der falschen. `cmd_run` hüllt den
Callback bei N > 1 deshalb in einen `Mutex`: eine Frage zur Zeit. Welches Item fragt, zeigt die
Rückfrage dabei nicht — `ApproveFn` bekommt nur das Kommando. Für autonome Läufe ist `-y` oder eine
Allowlist ohnehin der Normalfall.

### Bewusst nicht gelöst in dieser Phase

- **Kein gemeinsamer Modell-Scheduler, kein Token-Budget.** Jeder Agent wiederholt ein Rate-Limit
  (HTTP 429) für sich mit Backoff. Das trägt bei kleinem N und wird bei großem zum Engpass. Wie
  groß „groß" ist, ist nicht gemessen: eine Handprobe mit echtem Modell (`--parallel 1` gegen
  `--parallel 4`, mit Wanduhrzeit, 429-Antworten und Integrationskonflikten) steht noch aus.
  Geprüft ist bisher der Ablauf selbst — offline in `tests/parallel.rs` und über die Executable mit
  dem Demo-Modell.
- **Konflikte zeigen sich erst bei der Integration.** Zwei parallele Items, die dieselbe Stelle
  ändern, mergen einzeln sauber und kollidieren am Ende des Laufs; der Lauf endet dann `Blocked`.
  Mit mehr parallelen Branches wird das wahrscheinlicher. Dagegen stehen heute nur die
  Abhängigkeiten, die der Planer setzt. Eine Integration nach jedem Item und eine automatische
  Auflösung sind eigene Schritte.
- **Kein Wachhund für hängende Worker.** Ein Versuch, der keine Ereignisse mehr erzeugt, blockiert
  wie bisher seinen Platz; sein Lease läuft ab, aber im laufenden Prozess gibt es niemand frei.
- **Ein Prozess.** `work.lock` bleibt: mehrere Prozesse oder Maschinen auf einem Projekt gibt es
  nicht. Deshalb bleibt auch `recovery::recover_all` beim Start richtig.
- **Keine Neuplanung im laufenden Vorhaben.** Der Plan steht nach der Zerlegung; Agenten können
  Items nachlegen, eine geordnete Überarbeitung gibt es nicht.

## Beobachten (Phase 8, §22)

Mehrstündige Läufe sollen ohne Rätselraten nachvollziehbar sein (§22 des Konzepts, „Mehrstündige
Runs sind für den Benutzer nachvollziehbar und steuerbar"). Zwei Bausteine dafür:

**Sperrfreies Lesen (`WorkStore::open_read_only`).** Vor dieser Phase nahm JEDER Aufruf von
`WorkStore::open` — auch die reinen Lesebefehle — dieselbe Sperrdatei wie ein Schreiber. Für eine
Laufzeit, deren erklärter Zweck stundenlange Läufe sind, war genau das der wichtigste Moment, in
dem `agentkit work status <projekt-id>` scheiterte: solange `agentkit work run` lief. Die Sperre
soll Schreiber gegen Schreiber schützen, nicht Leser aussperren. `status`/`items`/`events`/`list`/
`watch` lesen deshalb jetzt über `WorkStore::open_read_only`, das das Journal genauso abspielt wie
`WorkStore::open`, aber NIE `work.lock` anlegt. Schreibende Kommandos (`create`/`run`/`resume`/
`retry`/`approve`/`reject`/`budget`/`pause`) bleiben unverändert bei `WorkStore::open` — zwei
gleichzeitige Schreiber sind weiterhin der Fall, den `work.lock` verhindern muss.

Strukturell schreibgeschützt statt nur per Konvention: `open_read_only` gibt einen [`WorkState`]-
WERT zurück, keinen `WorkStore` — es gibt schlicht keine `submit`/`submit_with`/`checkpoint`-
Methode, die ein Aufrufer aufrufen könnte. Ein Versuch zu schreiben ist damit kein `WorkError` zur
Laufzeit, sondern ein Compile-Fehler — stärker als ein Wrapper-Typ mit einer internen
„schreibgeschützt"-Markierung, der bei jedem Aufruf erst zur Laufzeit prüfen müsste, ob er das
darf. Eine unvollständige letzte Journal-Zeile (ein Absturz — oder schlicht ein GERADE laufendes
`append` eines Schreibers) wird dabei genauso toleriert wie beim schreibenden Pfad (der Zustand bis
zur letzten VOLLSTÄNDIGEN Zeile kommt zurück), aber NICHT auf der Platte „repariert": nur der
schreibende Pfad darf die Datei fixen, ein Leser darf sie nie anfassen, während irgendwo noch ein
Schreiber leben könnte.

**`agentkit work watch <projekt-id>`** ist die Live-Ansicht für ein ZWEITES Terminal neben einem
laufenden `agentkit work run` (deshalb kam das sperrfreie Lesen zuerst). Sie zeigt Kopfzeile
(Projekt/Lauf/Status), das Work-Item-Board (Status/Priorität/Versuche/Executor/
Verifikationsrichtlinie), das gerade laufende Item (Agent, Versuch, verbleibende Lease-Zeit),
Budgetverbrauch (Wandzeit/Items gegen die Limits, Versuche, Artefakte), die letzten
Zeitleisten-Einträge und ausdrücklich, worauf der Lauf gerade wartet (Freigabe, Blockade, Budget —
über `scheduler::decide`, denselben deterministischen Kern, den auch der Runner je Runde befragt).
`--interval SEKUNDEN` (Default 2) bestimmt den Abstand zwischen zwei Aktualisierungen, `--tail N`
(Default 10) die Anzahl der Zeitleisten-Einträge.

**Bewusst KEIN ratatui.** Das hängt am Feature `tui`, das die schlanke `cli`-Release-Variante
bewusst NICHT hat (siehe `.github/workflows/release.yml`) — und gerade dort (Skripte, Server, CI)
laufen die langen Vorhaben, die `watch` beobachten soll. Eine schlichte, bei jedem Intervall
komplett neu gezeichnete ANSI-Ansicht (Bildschirm löschen + Cursor Zeile 1) genügt für dieses eine
Bild und funktioniert in beiden Release-Varianten, ohne eine neue Dependency einzuführen. Beendet
sich mit Ctrl-C sauber (derselbe kooperative Stop-Knopf wie `run`) und stellt den Cursor wieder her.

`--format json` gibt — wie jedes andere `--format json` dieser CLI (stdout-Kontrakt) — GENAU EIN
Dokument aus und kehrt zurück, KEINE Endlosschleife. Ist stdout kein Terminal (z. B. eine Pipe oder
Datei), verhält sich `watch` ebenfalls wie ein einmaliges `status`: ein Prozess, dessen stdout
umgeleitet ist, soll nicht endlos ANSI-Escape-Sequenzen in diese Senke schreiben — das wäre reiner
Müll für jeden nachgeschalteten Konsumenten.

## Grenzen des heutigen Stands

- **`--dry-run` gilt auch für Work-Läufe.** `agentkit work run <projekt-id> --dry-run` reicht das
  Flag bis zu `CodingAgentExecutor`/`CodingAgentConfig::dry_run` durch — `build_coding_agent`
  wendet die Sperre (`ToolRegistry::dry_run_blocking(is_likely_destructive)`) auf JEDE Registry an,
  auch die von `task`/`swarm` erzeugten. Kein eigener Nachbau der Heuristik in diesem Crate.
- **Mehrere Worker nur in einem Prozess.** `max_parallel_agents` ist 1, solange niemand
  `--parallel N` setzt; was mit mehreren Workern offen bleibt, steht im Abschnitt „Parallele
  Worker".
- **`work.lock` sperrt nur noch SCHREIBER, seit Phase 8 (Beobachtung).** `create`/`run`/`resume`/
  `retry`/`approve`/`reject`/`budget`/`pause` nehmen weiterhin `WorkStore::open` und damit die
  Sperre — zwei Schreiber gleichzeitig blieben der Grund für `work.lock` (siehe oben). Die reinen
  Lesebefehle (`status`/`items`/`events`/`list`/`watch`) laufen dagegen über
  `WorkStore::open_read_only` und funktionieren jetzt AUCH während ein `agentkit work run` im
  selben Verzeichnis die Sperre hält — siehe Abschnitt „Beobachten" unten für die Begründung. Ein
  abgestürzter Schreiber-Halter lässt sich weiterhin mit `agentkit work run/resume --force`
  übernehmen (keine automatische PID-Lebendprüfung).
- **Ein zweiter Ctrl-C beendet den Prozess ohne Aufräumen.** Das erste Ctrl-C setzt den
  Cancel-Schalter (sauberer Lauf-Abbruch, `WorkStore::drop` gibt die Sperre normal frei); ein
  zweites erzwingt in `agentkit_app` einen sofortigen `std::process::exit`, der KEINE Destruktoren
  mehr laufen lässt — `work.lock` bleibt dann liegen, genau wie nach `SIGKILL`. Derselbe Ausweg gilt:
  `--force` beim nächsten `run`/`resume`. Unter Git-Isolation bleiben dabei auch die Worktrees der
  laufenden Items liegen; der nächste Versuch übernimmt sie.
- **`work pause` wirkt nur auf einen nicht laufenden Lauf.** Einen laufenden Vordergrund-Run
  pausiert man mit Ctrl-C; prozessübergreifendes Pausieren bräuchte Polling.
- **Kein `fsync` beim Anhängen.** Das Journal wird geschrieben und geflusht, aber nicht
  synchronisiert — ein Stromausfall kann die letzte Zeile abschneiden. Jede Zeile entsteht samt
  Umbruch in EINEM Schreibvorgang; eine abgeschnittene letzte Zeile wird beim Öffnen erkannt und
  abgeschnitten (`set_len`, die gültigen Zeilen davor werden nicht neu geschrieben), ein
  fehlender Umbruch nach einer vollständigen letzten Zeile ergänzt. Eine kaputte Zeile mitten im
  Journal ist ein harter Fehler. Die Kompaktierung synchronisiert ihre Temp-Datei vor dem
  Umbenennen (`sync_all`) — dort stünde sonst das ganze Journal auf dem Spiel.
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
Abhängigkeitsprüfung, inklusive der neuen `AwaitingVerification`-Übergänge), `journal.rs` (Replay,
Kompaktierung, beschädigte Zeilen, parallele Leser, Vorwärtskompatibilität ohne
`verification_policy`, dazu Phase 8: `open_read_only` legt keine Sperrdatei an, ändert das Journal
nie — auch nicht bei einer abgeschnittenen letzten Zeile —, und gelingt, während ein anderer Store
schreibend offen ist), `recovery.rs` (abgelaufene Leases, halb geschriebene Fehlschläge, Idempotenz,
die beiden Verifikations-Absturzlücken, ein wartendes `HumanApproval`-Item übersteht `recover_all`
unangetastet), `scheduler.rs` (Reihenfolge, Budget, Blockade, `Decision::AwaitingVerification`),
`tools.rs` (was das Modell darf und was nicht, inklusive Pfadausbruch), `runner.rs` (vollständige
Läufe, Retry, Abbruch, Neustart mitten im Lauf, `AutomatedTests`/`HumanApproval`-Policies),
`cli.rs` (Argumente, stdout-Kontrakt, Exit-Codes, `approve`/`reject`, `--items`-Feld `executor`
inklusive unbekannter Formen, Anzeige in `items`/`status`, die `--git-isolation`-Ablehnung
außerhalb eines Git-Repos, sowie Phase 8: `status`/`items`/`events`/`list` funktionieren, während
ein anderer Store schreibend offen ist (`run` scheitert dabei weiterhin an der Sperre),
`watch --format json` liefert genau ein Dokument, `watch` ohne TTY verhält sich wie ein einmaliges
`status`, und der JSON-Inhalt von `status`/`watch` trägt Budgetverbrauch, Versuche und Artefakte
nach einem echten Lauf). Der Schwarm-Executor selbst (`SwarmWorkExecutor`/`DispatchingExecutor`)
hat keine Tests in diesem Crate — er lebt in `agentkit_app` (`tests/work_swarm.rs`), das einzige
Crate, das `agentkit_swarm` kennt. `git_dependencies.rs` beschreibt die Git-Isolation mit
Abhängigkeiten auf echten Repositories (Kette, Diamant, Konflikt zwischen Vorgängern, Rollback,
Integration, uncommittete Änderungen neben dem Workspace, losgelöster HEAD). `parallel.rs`
beschreibt die parallelen Worker: zwei Versuche laufen wirklich gleichzeitig (sie treffen sich an
einem gemeinsamen Punkt, statt zu schlafen), nie mehr als Worker, Abhängigkeiten gelten trotz
freiem Worker, die Schreiber-Regel ohne Git-Isolation, zwei gleichzeitige Schreiber in eigenen
Worktrees, ein Konflikt bei der Integration, und was Abbruch, harter Fehler und erschöpftes Budget
mit laufenden Versuchen machen. `hardening.rs` hält die Härtungen fest: Freigabe für
Prüfkommandos des Modells, Schutz von Journal und Sperre, Submission vor Sentinel, das Item-Budget
in `work_add_item` und die Absturzfälle des Journals.

`git.rs` hat eigene Unit-Tests (legen ein echtes `git init`-Repo in einem Temp-Verzeichnis an,
committen mit der Laufzeit-Identität, mergen und lassen Merges bewusst konfliktieren) — offline
und deterministisch, kein Netz und keine globale `user.name`/`user.email`-Konfiguration nötig. Die
Repo-Fixture selbst (`init_repo_with_commit`) ist `pub` hinter `cfg(test)`/Feature `test-support`
und wird auch von `tests/runner.rs` und `tests/cli.rs` aufgerufen — eine Stelle statt dreier
Kopien, siehe `Cargo.toml` für die selbstreferenzierende Dev-Dependency, die das Feature während
`cargo test` freischaltet.
`runner.rs` deckt die vollständige Git-Isolation ab (§19, Abschnitt „Git-Isolation" oben): ein
Lauf ohne `--git-isolation` bleibt unverändert, ein erfolgreicher Versuch committet und erzeugt
das `GitCommit`-Artefakt, ein Versuch ohne Änderung committet nichts, ein gescheiterter Versuch
verwirft seine Änderungen und der nächste startet sauber, der Agent arbeitet in einem eigenen
Worktree und lässt den Arbeitsbaum des Bedieners in Ruhe, ein abhängiges Item liest das Artefakt
seines Vorgängers aus dem Worktree heraus mit dem echten `read_file`, ein unsauberer Arbeitsbaum
wird vor dem ersten Versuch abgelehnt, zwei Items mit unterschiedlichen Dateien werden vom Integrations-Item
gemergt, und zwei Items mit einem echten Konflikt lassen die Integration scheitern (Lauf endet
`Blocked`, Konfliktdatei in der Fehlermeldung, Arbeitsbaum danach sauber und zurück auf dem
Ausgangsbranch).

Befund 1 der Handprobe (Ausgangsbranch übersteht einen harten Prozessabbruch) hat eigene Tests:
`journal.rs` prüft, dass ein Journal ohne `base_branch`-Feld weiter lädt (`None`, exakt das
Verhalten vor dieser Korrektur) und dass `base_branch` einen Neustart des Stores übersteht (steht
im Journal, nicht nur im Speicher). `cli.rs` stellt den Fußabdruck eines vor den Worktrees hart
abgebrochenen Laufs nach, indem der Item-Branch über die Test-Fixture `git::ensure_item_branch` im
Arbeitsbaum ausgecheckt wird, und deckt beide Fälle ab: ein Item-Branch DIESES Projekts wird beim nächsten `run` automatisch auf den Ausgangsbranch
zurückgeholt, ein FREMDER Branch bleibt stehen und `run` warnt nur.

Befund 2 der Handprobe (Laufzeitgrenze für Schwarm-Items aus dem Vorhabenbudget) hat Tests in
beiden betroffenen Crates: `agentkit_app::work_swarm` selbst (drei reine Unit-Tests für
`swarm_item_max_runtime_secs` — Restlaufzeit übernommen, Fallback-Konstante ohne Budget, eine
erschöpfte Restlaufzeit auf mindestens eine Sekunde geklemmt) und `agentkit_app/tests/work_swarm.rs`
(ein Schwarm ohne Konsens-Aktivität läuft mit `remaining_wall_secs = Some(1)` in
`CompletionReason::MaxRuntimeReached`, das auf den Sentinel `"(max_steps erreicht)"` abgebildet
wird — eine sehr kleine Grenze statt echter Minuten, damit der Test nicht selbst am Fallback-Wert
900s hängen bleibt, käme die Verdrahtung nicht an).
