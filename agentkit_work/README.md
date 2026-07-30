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
Agenten) setzt bewusst KEINE Policy — ein zur Laufzeit erzeugtes Folge-Item bekommt `None`, wie
bisher; die Entscheidung, was geprüft werden muss, bleibt beim Operator, nicht beim Modell. Ein vom
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

### Bewusst NICHT enthalten: `Composite`, `PeerReview`

§10 des Konzepts nennt zusätzlich `PeerReview` und `Composite(Vec<…>)`. Beide entfallen weiterhin
(Guidelines §4, YAGNI):

- **`PeerReview`** setzt mehrere Agenten/Rollen voraus, die sich gegenseitig prüfen — das gehört zur
  Swarm-Integration (Phase 6), nicht zu dieser Phase.
- **`Composite(Vec<VerificationRequirement>)`** würde mehrere Policies kombinieren (z. B. Tests UND
  ein Review) — additiv nachrüstbar, sobald mindestens zwei der Einzel-Policies real im Einsatz
  sind und ein konkreter Fall genau diese Kombination braucht. Ohne diesen zweiten Nutzer wäre es
  Konfigurierbarkeit auf Vorrat.

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
Abhängigkeitsprüfung, inklusive der neuen `AwaitingVerification`-Übergänge), `journal.rs` (Replay,
Kompaktierung, beschädigte Zeilen, parallele Leser, Vorwärtskompatibilität ohne
`verification_policy`), `recovery.rs` (abgelaufene Leases, halb geschriebene Fehlschläge, Idempotenz,
die beiden Verifikations-Absturzlücken, ein wartendes `HumanApproval`-Item übersteht `recover_all`
unangetastet), `scheduler.rs` (Reihenfolge, Budget, Blockade, `Decision::AwaitingVerification`),
`tools.rs` (was das Modell darf und was nicht, inklusive Pfadausbruch), `runner.rs` (vollständige
Läufe, Retry, Abbruch, Neustart mitten im Lauf, `AutomatedTests`/`HumanApproval`-Policies),
`cli.rs` (Argumente, stdout-Kontrakt, Exit-Codes, `approve`/`reject`).
