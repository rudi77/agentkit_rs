//! Der Runner: arbeitet einen Lauf ab, bis er fertig, blockiert, budgetiert
//! oder abgebrochen ist.
//!
//! Ein Koordinator und bis zu `WorkBudget::max_parallel_agents` Worker. Der
//! Koordinator ist der Thread des Aufrufers: nur er befragt den Scheduler,
//! claimt Items und ruft `on_progress`. Jeder Versuch läuft in einem eigenen
//! Worker-Thread (`std::thread::scope` — kein Pool, keine Runtime, kein
//! Worker überlebt den Aufruf) und meldet Fortschritt und Ergebnis über einen
//! Kanal zurück. Mit einem Worker ist der Ablauf derselbe wie vor der
//! Parallelität, nur dass der Versuch in einem eigenen Thread läuft.
//!
//! Kein Daemon, kein eigener Thread für den Heartbeat — der läuft im
//! Event-Callback des laufenden Agenten (bewusste Plan-Entscheidung: ein
//! Agent, der keine Events mehr produziert, ist genau der Fall, den das Lease
//! abdecken soll).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};

use agentkit::{AgentEvent, Cancel};

use crate::error::WorkError;
use crate::event::{WorkEvent, AUTOMATED_TESTS_BY};
use crate::executor::{AgentExecutor, AgentWorkPackage};
use crate::graph::GraphGateway;
use crate::model::{
    id_order, item_branch_name, now_ms, ArtifactKind, AttemptId, AttemptStatus, CompletionReason,
    ExecutorKind, FailureInfo, FailureKind, RunStatus, VerificationPolicy, WorkArtifact,
    WorkBudget, WorkItem, WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::scheduler::{self, Decision};
use crate::state::WorkState;
use crate::store::WorkStore;
use crate::tools::{WorkSubmission, WorkToolCtx};

/// `by`-Wert der Laufzeit für einen deterministischen Schritt, den sie SELBST
/// ausführt (Phase 7, §31: „Deterministische Runtime, agentische
/// Problemlösung") — bisher nur das Integrations-Item. Dieselbe Idee wie
/// `event::AUTOMATED_TESTS_BY`/`event::HUMAN_BY`: nie ein Modellargument.
const RUNTIME_AGENT_ID: &str = "runtime";

/// Zeitlimit für das Prüfkommando einer `VerificationPolicy::AutomatedTests`.
/// Kein eigenes CLI-Flag dafür (Guidelines §4, YAGNI): `--shell-timeout` ist
/// konzeptionell etwas anderes — ein Limit je `run_shell`-Aufruf DES MODELLS
/// im Kern-Agenten. Ein zweites, unabhängiges Limit für dieses deterministische
/// Prüfkommando bekäme erst mit einem zweiten konkreten Bedarf (Rule of Three)
/// ein eigenes Flag; bis dahin reicht eine feste, großzügige Grenze.
const VERIFY_COMMAND_TIMEOUT_SECS: u64 = 300;

/// Fortschrittsmeldungen für CLI/TUI. Kein Logging im Crate selbst — der
/// Aufrufer entscheidet, was er anzeigt und wohin (stdout/stderr-Kontrakt).
pub enum WorkProgress {
    ItemStarted {
        item: WorkItemId,
        title: String,
        attempt: u32,
        max_attempts: u32,
    },
    /// Ein Ereignis des Agenten, der `item` in dessen `attempt`-tem Versuch
    /// bearbeitet. Das Item steht dabei, weil mehrere Versuche gleichzeitig
    /// laufen können — aus der Reihenfolge der Meldungen lässt sich nicht
    /// mehr schließen, zu welchem ein Ereignis gehört.
    Agent {
        item: WorkItemId,
        attempt: u32,
        event: AgentEvent,
    },
    ItemDone {
        item: WorkItemId,
        ok: bool,
        summary: String,
    },
    /// Wird vom AUFRUFER nach `recovery::recover_all` gemeldet (siehe
    /// `run_to_completion`-Doku) — `run_to_completion` selbst erzeugt diese
    /// Meldung nicht, sie gehört zum gemeinsamen Vokabular für die CLI.
    Recovered {
        released: usize,
    },
    Checkpoint {
        seq: u64,
    },
    /// Hinweis, den der Nutzer sehen soll (z. B. fehlendes `work_submit`).
    Note(String),
}

/// Konfiguration eines Laufs.
pub struct RunnerConfig {
    /// Kennung des ersten Workers, wie sie im Journal am Versuch steht. Die
    /// weiteren Worker eines parallelen Laufs heißen `worker-2`, `worker-3`, …
    pub agent_id: String,
    pub lease_secs: u64,
    pub heartbeat_secs: u64,
    pub workspace: String,
    /// Zugang zum Wissensgraphen (Phase 4) — `None` ohne `--graph DIR` oder
    /// ohne das Feature `graph`. Der Runner reicht ihn in `WorkToolCtx`
    /// weiter (`work_claim`) und ruft `recall` vor jedem Versuch.
    pub graph: Option<Arc<dyn GraphGateway>>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        RunnerConfig {
            agent_id: "worker-1".to_string(),
            lease_secs: 600,
            heartbeat_secs: 30,
            workspace: ".".to_string(),
            graph: None,
        }
    }
}

/// Ergebnis eines `run_to_completion`-Aufrufs.
pub struct RunOutcome {
    pub reason: CompletionReason,
    pub completed: Vec<WorkItemId>,
    pub failed: Vec<WorkItemId>,
    pub attempts: u32,
}

/// Was ein einzelner Versuch für die Schleife bedeutet.
enum AttemptOutcome {
    Succeeded,
    FailedRetryable,
    FailedExhausted,
    /// Der Agent hat selbst auf einen Abbruch reagiert (`"(abgebrochen)"`) —
    /// der Versuch zählt nicht gegen `max_attempts`, und die Schleife muss
    /// genauso enden wie bei einem von außen gesetzten `cancel`.
    Interrupted,
    /// Der Versuch war erfolgreich, aber `VerificationPolicy::HumanApproval`
    /// hält das Item auf `AwaitingVerification` (Phase 5a) — weder
    /// abgeschlossen noch gescheitert. Die Schleife rührt es nicht weiter an;
    /// ob der Lauf deswegen anhält, entscheidet `scheduler::decide` bei der
    /// nächsten Runde (`Decision::AwaitingVerification`), NICHT dieser
    /// Rückgabewert selbst — andere, unabhängige Items dürfen währenddessen
    /// weiterlaufen.
    AwaitingVerification,
}

/// Legt das Planungs-Item an, wenn der Lauf noch kein einziges Item hat. Gibt
/// dessen ID zurück, oder `None`, wenn schon Items existieren (auch nach einem
/// Neustart — dann bleibt es bei den bestehenden, nichts wird verdoppelt).
pub fn ensure_plan_item(
    store: &Arc<WorkStore>,
    run_id: &str,
) -> Result<Option<WorkItemId>, WorkError> {
    create_plan_item(store, run_id, VerificationPolicy::None)
}

/// Wie [`ensure_plan_item`], aber die Zerlegung wartet nach ihrem Versuch auf
/// eine menschliche Freigabe (`VerificationPolicy::HumanApproval`) — der
/// Modus PLAN von `agentkit work run --mode plan`. Solange sie wartet, startet
/// [`crate::scheduler::decide`] kein Folge-Item: die angelegten Items SIND der
/// Plan. `work approve` gibt ihn frei, `work reject --reason` verwirft die
/// angelegten Items und lässt mit der Begründung neu zerlegen (siehe
/// `state::apply`, `VerificationRejected`).
pub fn ensure_reviewed_plan_item(
    store: &Arc<WorkStore>,
    run_id: &str,
) -> Result<Option<WorkItemId>, WorkError> {
    create_plan_item(store, run_id, VerificationPolicy::HumanApproval)
}

fn create_plan_item(
    store: &Arc<WorkStore>,
    run_id: &str,
    verification_policy: VerificationPolicy,
) -> Result<Option<WorkItemId>, WorkError> {
    let snapshot = store.snapshot();
    if snapshot.items.values().any(|it| it.run_id == run_id) {
        return Ok(None);
    }
    let project = snapshot
        .project
        .as_ref()
        .ok_or_else(|| WorkError::NotFound("Projekt".to_string()))?;

    let id = snapshot.next_item_id();
    let description = format!(
        "{}\n\nZerlege dieses Vorhaben mit 'work_add_item' in abgegrenzte, einzeln \
         überprüfbare Teilaufgaben mit Abhängigkeiten, wo eine Teilaufgabe auf einer \
         anderen aufbaut. Nimm selbst KEINE Implementierung vor — das ist Aufgabe der \
         Folge-Items, die du hier anlegst. Zerlege nur, was das Vorhaben verlangt: \
         erfinde keine Teilaufgaben dazu, die niemand gefordert hat (etwa eigene \
         zusätzliche Tests), nur damit es mehrere Items gibt. Passt das Vorhaben in \
         ein Item, lege genau eines an.\n\nMehrere Agenten arbeiten die Items \
         gleichzeitig ab, jeder für sich und ohne die Arbeit der anderen zu sehen. \
         Schneide deshalb Arbeit, die nicht aufeinander aufbaut, in eigene Items OHNE \
         Abhängigkeit — nur sie können nebeneinander laufen. Items, die dieselben Dateien \
         ändern, bekommen dagegen eine Abhängigkeit untereinander, auch wenn sie fachlich \
         getrennt sind: sonst überschreiben sie einander oder lassen sich am Ende nicht \
         zusammenführen.",
        project.objective.trim()
    );
    let item = crate::model::WorkItem {
        id: id.clone(),
        run_id: run_id.to_string(),
        title: "Vorhaben zerlegen".to_string(),
        description,
        kind: WorkItemKind::Planning,
        status: WorkItemStatus::Pending,
        priority: 9,
        seq: crate::model::id_order(&id),
        required_role: None,
        dependencies: Vec::new(),
        acceptance_criteria: vec![
            "Mindestens ein Work Item wurde mit 'work_add_item' angelegt.".to_string(),
            "Abhängigkeiten zwischen den neuen Items sind gesetzt, wo eines auf einem \
             anderen aufbaut oder dieselben Dateien ändert — und nur dort."
                .to_string(),
            "Jedes neue Item hat prüfbare Akzeptanzkriterien.".to_string(),
        ],
        verification_policy,
        verifies: None,
        claims_promoted: false,
        // Das automatisch angelegte Planungs-Item ist immer ein Einzelagent —
        // Zerlegung ist kein Fall für einen Schwarm (§13), und der Operator
        // hatte hier ohnehin keine Gelegenheit, etwas anderes zu wählen.
        executor: ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts: project.budget.max_attempts_per_item,
        updated_at_ms: now_ms(),
    };
    store.submit(WorkEvent::WorkItemCreated { item })?;
    Ok(Some(id))
}

/// Arbeitet den Lauf ab, bis er fertig, blockiert, budgetiert oder
/// abgebrochen ist. Erholt sich NICHT selbst — der Aufrufer ruft vorher
/// `recovery::recover_all`, damit er den Report anzeigen kann, BEVOR der
/// Runner lostritt.
///
/// Bis zu `WorkBudget::max_parallel_agents` Versuche laufen gleichzeitig.
/// `on_progress` wird ausschließlich auf dem Thread des Aufrufers gerufen;
/// `executor` teilen sich alle Worker.
///
/// Was nichts Neues mehr starten lässt, bricht laufende Versuche nicht ab:
/// - **Abbruch** (`cancel`): die Agenten beobachten denselben Schalter und
///   beenden ihren Versuch selbst; jeder wird als unterbrochen journalt und
///   zählt nicht gegen `max_attempts`.
/// - **Harter Fehler** eines Versuchs: die übrigen laufen zu Ende und werden
///   journalt, dann kommt der ERSTE Fehler zurück.
/// - **Budget, Freigabe-Halt, Blockade:** der Lauf pausiert bzw. endet erst,
///   wenn kein Versuch mehr läuft.
pub fn run_to_completion(
    store: &Arc<WorkStore>,
    run_id: &str,
    executor: &dyn AgentExecutor,
    cfg: &RunnerConfig,
    cancel: &Cancel,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<RunOutcome, WorkError> {
    ensure_plan_item(store, run_id)?;

    let snapshot = store.snapshot();
    let run = snapshot
        .runs
        .get(run_id)
        .cloned()
        .ok_or_else(|| WorkError::NotFound(format!("Run '{run_id}'")))?;
    let project = snapshot
        .project
        .as_ref()
        .ok_or_else(|| WorkError::NotFound("Projekt".to_string()))?;
    let budget = project.budget.clone();
    // Früh statt erst nach Stunden bei der Integration: die Items arbeiten
    // in eigenen Worktrees, aber gemergt wird am Ende HIER.
    if project.git_isolation {
        ensure_integration_target_ready(&cfg.workspace)?;
    }
    // Der Lauf-Start, NICHT `now_ms()`: sonst setzt jeder Resume die
    // Wall-Time-Uhr zurück und `max_wall_time_secs` wäre wirkungslos gegen
    // einen Lauf, der immer wieder kurz angehalten und fortgesetzt wird.
    let started_at_ms = run.started_at_ms;

    if run.status == RunStatus::Paused {
        store.submit(WorkEvent::RunResumed {
            run: run_id.to_string(),
            at_ms: now_ms(),
        })?;
    }

    let mut completed = Vec::new();
    let mut failed = Vec::new();
    let mut attempts_started: u32 = 0;

    // Worker-Plätze, der kleinste zuerst vergeben. Mehr Plätze als
    // `max_parallel_agents` braucht es nie: `scheduler::decide` liefert kein
    // `Run`, solange schon so viele Items laufen.
    let slots = budget.max_parallel_agents.max(1);
    let mut free_slots: Vec<u32> = (1..=slots).rev().collect();
    let mut in_flight: u32 = 0;
    // Warum nichts Neues mehr startet. Laufende Versuche enden trotzdem
    // regulär und werden journalt — bezahlte Arbeit wird nicht weggeworfen.
    let mut stop: Option<Stop> = None;
    let (tx, rx) = mpsc::channel::<WorkerMessage>();

    std::thread::scope(|scope| loop {
        if stop.is_none() && cancel.load(Ordering::SeqCst) {
            stop = Some(Stop::Canceled("Lauf durch Stop-Knopf abgebrochen"));
        }

        let snapshot = store.snapshot();
        let decided_seq = snapshot.seq;
        let decision = match &stop {
            Some(_) => None,
            None => Some(scheduler::decide(
                &snapshot,
                run_id,
                &budget,
                started_at_ms,
                now_ms(),
            )),
        };

        if let (Some(Decision::Run(item_id)), Some(slot)) = (&decision, free_slots.last().copied())
        {
            // Der Claim wird HIER journalt, nicht im Worker: die nächste
            // Runde von `decide` muss das Item schon als laufend sehen, sonst
            // vergäbe sie es ein zweites Mal.
            let agent_id = worker_agent_id(cfg, slot);
            let claim = match claim_attempt(store, item_id, &agent_id, cfg.lease_secs, on_progress)
            {
                Ok(claim) => claim,
                Err(e) => {
                    stop = Some(Stop::Failed(e));
                    continue;
                }
            };
            free_slots.pop();
            attempts_started += 1;
            in_flight += 1;
            let tx = tx.clone();
            let budget = &budget;
            // Stack wie der Haupt-Thread: bis hierher lief jeder Versuch
            // dort, ein Agent darf im Worker nicht früher an die Grenze stoßen.
            let spawned = std::thread::Builder::new()
                .name(agent_id)
                .stack_size(WORKER_STACK_BYTES)
                .spawn_scoped(scope, move || {
                    run_worker(store, claim, slot, budget, started_at_ms, cfg, executor, tx)
                });
            if let Err(e) = spawned {
                // Der Claim steht im Journal, läuft aber nirgends — die
                // Lease-Recovery beim nächsten Start gibt das Item frei.
                in_flight -= 1;
                free_slots.push(slot);
                stop = Some(Stop::Failed(WorkError::Io(format!(
                    "Worker-Thread nicht startbar: {e}"
                ))));
            }
            continue;
        }

        if in_flight > 0 {
            // Warten, bis ein Versuch endet — oder bis sich am Journal etwas
            // ändert, während ein Platz frei ist (ein Agent hat mit
            // `work_add_item` ein Item angelegt, das sofort laufen kann).
            loop {
                let message = rx.recv().expect("der Koordinator hält selbst einen Sender");
                match message {
                    WorkerMessage::Progress(progress) => {
                        on_progress(progress);
                        let may_start_more = stop.is_none() && !free_slots.is_empty();
                        if may_start_more && store.snapshot().seq != decided_seq {
                            break;
                        }
                    }
                    WorkerMessage::Done {
                        slot,
                        item_id,
                        result,
                    } => {
                        in_flight -= 1;
                        free_slots.push(slot);
                        free_slots.sort_unstable_by(|a, b| b.cmp(a));
                        match result {
                            Ok(AttemptOutcome::Succeeded) => completed.push(item_id),
                            Ok(AttemptOutcome::FailedExhausted) => failed.push(item_id),
                            // Weder abgeschlossen noch gescheitert — der
                            // Scheduler entscheidet bei der nächsten Runde,
                            // ob andere Items noch vorankommen oder der Lauf
                            // auf die Freigabe wartet.
                            Ok(AttemptOutcome::FailedRetryable)
                            | Ok(AttemptOutcome::AwaitingVerification) => {}
                            Ok(AttemptOutcome::Interrupted) => {
                                stop.get_or_insert(Stop::Canceled(
                                    "Versuch durch Stop-Knopf unterbrochen",
                                ));
                            }
                            // Der ERSTE harte Fehler gewinnt — auch über
                            // einen Abbruch, der schon vorgemerkt ist.
                            Err(e) => {
                                if !matches!(stop, Some(Stop::Failed(_))) {
                                    stop = Some(Stop::Failed(e));
                                }
                            }
                        }
                        match store.checkpoint() {
                            Ok(seq) => on_progress(WorkProgress::Checkpoint { seq }),
                            Err(e) => {
                                if !matches!(stop, Some(Stop::Failed(_))) {
                                    stop = Some(Stop::Failed(e));
                                }
                            }
                        }
                        break;
                    }
                }
            }
            continue;
        }

        // Ab hier läuft kein Versuch mehr — der Lauf endet oder steht.
        match stop.take() {
            Some(Stop::Failed(e)) => return Err(e),
            Some(Stop::Canceled(reason)) => {
                return finish_run(
                    store,
                    WorkEvent::RunPaused {
                        run: run_id.to_string(),
                        reason: reason.to_string(),
                        at_ms: now_ms(),
                    },
                    CompletionReason::Canceled,
                    completed,
                    failed,
                    attempts_started,
                );
            }
            None => {}
        }
        match decision.expect("ohne Stopp wurde der Scheduler befragt") {
            Decision::Done => {
                // Git-Isolation (Phase 7, §19/§26 Phase 7): BEVOR der Lauf
                // tatsächlich als erledigt gilt, bekommt er — falls noch
                // nicht geschehen — sein deterministisches Integrations-Item
                // (siehe `run_integration_item`-Doku für die Begründung,
                // warum das NICHT über die Executor-Naht läuft). Das Item
                // wird darin sofort bis zum Ende ausgeführt (gemergt oder
                // an einem Konflikt gescheitert); die nächste Runde von
                // `scheduler::decide` bewertet den Lauf danach neu.
                if run_integration_item(store, run_id, &cfg.workspace, on_progress)? {
                    let seq = store.checkpoint()?;
                    on_progress(WorkProgress::Checkpoint { seq });
                    continue;
                }
                return finish_run(
                    store,
                    WorkEvent::RunCompleted {
                        run: run_id.to_string(),
                        reason: CompletionReason::AllItemsDone,
                        at_ms: now_ms(),
                    },
                    CompletionReason::AllItemsDone,
                    completed,
                    failed,
                    attempts_started,
                );
            }
            Decision::BudgetExhausted(msg) => {
                // Pausiert, nicht abgeschlossen: der Nutzer kann das Budget
                // erhöhen und den Lauf fortsetzen.
                return finish_run(
                    store,
                    WorkEvent::RunPaused {
                        run: run_id.to_string(),
                        reason: msg,
                        at_ms: now_ms(),
                    },
                    CompletionReason::BudgetExceeded,
                    completed,
                    failed,
                    attempts_started,
                );
            }
            Decision::Blocked(_ids) => {
                return finish_run(
                    store,
                    WorkEvent::RunCompleted {
                        run: run_id.to_string(),
                        reason: CompletionReason::Blocked,
                        at_ms: now_ms(),
                    },
                    CompletionReason::Blocked,
                    completed,
                    failed,
                    attempts_started,
                );
            }
            Decision::AwaitingVerification(ids) => {
                // Nicht blockiert, nicht fertig — der Lauf hält an, bis eine
                // Freigabe entschieden wird (§20 des Konzepts, Phase 5a). Der
                // zurückgegebene `RunOutcome::reason` (unten geprüft von der
                // CLI) trägt `CompletionReason::AwaitingVerification` — exakt
                // wie beim Budget-Pausenfall setzt `RunPaused` selbst aber
                // KEIN `WorkRun::completion_reason` (das tun nur
                // `RunCompleted`/`RunCanceled`, siehe `state::apply`); ein
                // späteres `agentkit work status` zeigt den Grund also nicht
                // an, nur der unmittelbare `run`-Aufruf. Bestehende
                // Einschränkung, keine neue dieser Phase.
                let mut ids_sorted = ids;
                ids_sorted.sort_by_key(|id| id_order(id));
                let project_id = snapshot
                    .project
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_default();
                let list = ids_sorted.join(", ");
                on_progress(WorkProgress::Note(format!(
                    "wartet auf Freigabe für: {list}. Freigeben mit 'agentkit work approve \
                     <item-id> -p {project_id}', ablehnen mit 'agentkit work reject <item-id> \
                     -p {project_id} --reason <text>'."
                )));
                return finish_run(
                    store,
                    WorkEvent::RunPaused {
                        run: run_id.to_string(),
                        reason: format!("wartet auf Freigabe: {list}"),
                        at_ms: now_ms(),
                    },
                    CompletionReason::AwaitingVerification,
                    completed,
                    failed,
                    attempts_started,
                );
            }
            // `Run` ohne freien Platz kann nur heißen, dass im Journal ein
            // Item als laufend steht, das dieser Prozess nicht fährt — auch
            // das löst sich nicht von selbst.
            Decision::AtCapacity | Decision::Run(_) => {
                // Nichts ist ausführbar, obwohl der Lauf laut Scheduler weder
                // fertig noch endgültig blockiert ist: jedes offene Item
                // wartet auf eines, das ein Worker DIESES Prozesses hätte
                // weiterbringen müssen. Hier läuft aber keiner mehr, also
                // kann sich das nie von selbst auflösen. Ohne diese Bremse
                // träfe die Schleife endlos dieselbe Entscheidung und das CLI
                // hinge.
                return finish_run(
                    store,
                    WorkEvent::RunPaused {
                        run: run_id.to_string(),
                        reason: "kein ausführbares Item, Lauf steht".to_string(),
                        at_ms: now_ms(),
                    },
                    CompletionReason::Blocked,
                    completed,
                    failed,
                    attempts_started,
                );
            }
        }
    })
}

/// Stack eines Worker-Threads — die Größe, die der Haupt-Thread üblicherweise
/// hat. Der Standard für neue Threads ist ein Viertel davon.
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Warum der Koordinator keine neuen Versuche mehr startet.
enum Stop {
    /// Abbruch durch den Nutzer; der Text ist der Grund im `RunPaused`.
    Canceled(&'static str),
    /// Ein harter Fehler (Konfiguration, Journal, Git) — er wird zurückgegeben,
    /// sobald die übrigen Versuche beendet sind.
    Failed(WorkError),
}

/// Was ein Worker dem Koordinator meldet. Nur der Koordinator ruft
/// `on_progress` — der Callback des Aufrufers ist an dessen Thread gebunden.
enum WorkerMessage {
    Progress(WorkProgress),
    /// Der Versuch ist beendet und journalt; der Platz `slot` ist wieder frei.
    Done {
        slot: u32,
        item_id: WorkItemId,
        result: Result<AttemptOutcome, WorkError>,
    },
}

/// Kennung des Workers auf Platz `slot`, wie sie im Journal am Versuch steht.
/// Der erste trägt die konfigurierte Kennung, die weiteren sind durchgezählt.
fn worker_agent_id(cfg: &RunnerConfig, slot: u32) -> String {
    if slot == 1 {
        cfg.agent_id.clone()
    } else {
        format!("worker-{slot}")
    }
}

/// Der Rumpf eines Worker-Threads: einen schon geclaimten Versuch ausführen,
/// bewerten und journalen, und dem Koordinator das Ergebnis melden.
#[allow(clippy::too_many_arguments)]
fn run_worker(
    store: &Arc<WorkStore>,
    claim: ClaimedAttempt,
    slot: u32,
    budget: &WorkBudget,
    started_at_ms: u64,
    cfg: &RunnerConfig,
    executor: &dyn AgentExecutor,
    tx: mpsc::Sender<WorkerMessage>,
) {
    // Meldet `Done` auch dann, wenn der Versuch panict: der Koordinator
    // wartet auf genau diese Nachricht und hinge sonst für immer. Die Panik
    // selbst reicht `std::thread::scope` danach an den Aufrufer weiter.
    struct DoneOnDrop {
        tx: mpsc::Sender<WorkerMessage>,
        slot: u32,
        item_id: WorkItemId,
        result: Option<Result<AttemptOutcome, WorkError>>,
    }
    impl Drop for DoneOnDrop {
        fn drop(&mut self) {
            let result = self.result.take().unwrap_or_else(|| {
                Err(WorkError::Invalid(format!(
                    "Worker für Item '{}' ist abgestürzt",
                    self.item_id
                )))
            });
            let _ = self.tx.send(WorkerMessage::Done {
                slot: self.slot,
                item_id: self.item_id.clone(),
                result,
            });
        }
    }
    let mut done = DoneOnDrop {
        tx: tx.clone(),
        slot,
        item_id: claim.item_id.clone(),
        result: None,
    };

    let mut on_progress = |progress: WorkProgress| {
        let _ = tx.send(WorkerMessage::Progress(progress));
    };
    let result = execute_attempt(
        store,
        &claim,
        budget,
        started_at_ms,
        cfg,
        executor,
        &mut on_progress,
    )
    .and_then(|executed| record_attempt(store, &claim, executed, cfg, executor, &mut on_progress));
    done.result = Some(result);
}

/// Beendet den Lauf: journalt das Abschluss-/Pause-Ereignis (der `run_id` steckt
/// schon in `event`), kompaktiert IMMER per `checkpoint()` und baut das
/// `RunOutcome`. Zusammengezogen aus fünf ehemals identischen Dreierfolgen —
/// nicht der Kürze wegen, sondern damit ein künftiger Rückgabe-Zweig den
/// Checkpoint nicht vergessen kann.
fn finish_run(
    store: &Arc<WorkStore>,
    event: WorkEvent,
    reason: CompletionReason,
    completed: Vec<WorkItemId>,
    failed: Vec<WorkItemId>,
    attempts: u32,
) -> Result<RunOutcome, WorkError> {
    store.submit(event)?;
    store.checkpoint()?;
    Ok(RunOutcome {
        reason,
        completed,
        failed,
        attempts,
    })
}

/// Legt (falls noch nicht vorhanden) das deterministische Integrations-Item
/// an und führt es SOFORT selbst aus (Phase 7, §19/§26 Phase 7) — kein Agent,
/// kein `AgentExecutor::execute`: ein Merge ist Mechanik, keine fachliche
/// Entscheidung (§31 „Deterministische Runtime, agentische Problemlösung").
///
/// Eingehängt NICHT über die Executor-Naht — die ist für Agenten-/
/// Schwarmversuche gedacht, die ein `AgentWorkPackage` bekommen und Tools
/// aufrufen; ein deterministischer Merge braucht beides nicht und hätte über
/// diese Naht nur einen Fake-Agenten vorgetäuscht. Stattdessen ruft
/// `run_to_completion` diese Funktion direkt, GENAU wenn `scheduler::decide`
/// als Nächstes `Decision::Done` melden würde (siehe dort) — also alle
/// anderen Items terminal sind, aber git_isolation an ist und noch kein
/// Integrations-Item existiert. Claim, Merge-Ausführung und Abschluss laufen
/// über dieselben `WorkEvent`s wie ein normaler Versuch (`WorkItemClaimed`,
/// `AttemptFinished`, `WorkItemCompleted`/`WorkItemFailed`) — nur mit der
/// Laufzeit selbst als Agent (`RUNTIME_AGENT_ID`), damit `agentkit work
/// items`/`events` es genauso anzeigen wie jedes andere Item.
///
/// Gibt `true` zurück, wenn ein Integrations-Item entstanden ist und
/// bis zum Ende gelaufen ist (erfolgreich ODER gescheitert) — der Aufrufer
/// muss dann die Schleife erneut durchlaufen, statt den Lauf sofort
/// abzuschließen. `false`, wenn git_isolation aus ist oder schon ein
/// Integrations-Item existiert (nichts zu tun, der Lauf endet wie bisher).
fn run_integration_item(
    store: &Arc<WorkStore>,
    run_id: &str,
    workspace: &str,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<bool, WorkError> {
    let snapshot = store.snapshot();
    let Some(project) = snapshot.project.clone() else {
        return Ok(false);
    };
    if !project.git_isolation {
        return Ok(false);
    }
    // Ein Integrations-Item entsteht je Lauf höchstens EINMAL — auch nach
    // einem Fehlschlag (Merge-Konflikt) gibt es keinen automatischen zweiten
    // Versuch (§28: automatische Konfliktauflösung ausdrücklich nicht im
    // Umfang; `scheduler::decide` liest ein gescheitertes Integrations-Item
    // als `Blocked`, siehe dort).
    if snapshot
        .items
        .values()
        .any(|it| it.run_id == run_id && it.kind == WorkItemKind::Integration)
    {
        return Ok(false);
    }

    // VOR dem Anlegen des Items: scheitert die Prüfung, bricht der Lauf mit
    // einer klaren Meldung ab, und ein `resume` versucht die Integration
    // erneut — ein schon angelegtes Integrations-Item bekäme keinen zweiten
    // Versuch.
    ensure_integration_target_ready(workspace)?;

    // Erfolgreich abgeschlossene, git-isolierte Items dieses Laufs, in
    // Erzeugungsreihenfolge — nur sie haben einen eigenen Item-Branch (siehe
    // `run_attempt`/`GitAttemptCtx`); Review-/Planungs-Items liefen auf dem
    // Ausgangsbranch selbst und brauchen keinen Merge.
    let mut completed: Vec<&WorkItem> = snapshot
        .items
        .values()
        .filter(|it| {
            it.run_id == run_id
                && it.status == WorkItemStatus::Completed
                && it.kind.writes_workspace()
        })
        .collect();
    completed.sort_by_key(|it| it.seq);
    let merged_ids: Vec<WorkItemId> = completed.iter().map(|it| it.id.clone()).collect();

    let project_id = project.id.clone();
    let description = if merged_ids.is_empty() {
        "Keine git-isolierten Items abgeschlossen — nichts zu mergen.".to_string()
    } else {
        format!(
            "Merge der Item-Branches in den Ausgangsbranch, in Reihenfolge: {}.",
            merged_ids.join(", ")
        )
    };
    let run_id_owned = run_id.to_string();
    let (_, created_event) = store.submit_with(move |snap| {
        let id = snap.next_item_id();
        Ok(WorkEvent::WorkItemCreated {
            item: WorkItem {
                id: id.clone(),
                run_id: run_id_owned,
                title: "Integration".to_string(),
                description,
                kind: WorkItemKind::Integration,
                status: WorkItemStatus::Pending,
                priority: 9,
                seq: id_order(&id),
                required_role: None,
                dependencies: Vec::new(),
                acceptance_criteria: Vec::new(),
                verification_policy: VerificationPolicy::None,
                verifies: None,
                claims_promoted: false,
                executor: ExecutorKind::SingleAgent,
                attempt_count: 0,
                // Kein automatischer zweiter Versuch (§28) — ein Konflikt
                // beendet den Lauf blockiert, siehe `scheduler::decide`.
                max_attempts: 1,
                updated_at_ms: now_ms(),
            },
        })
    })?;
    let integration_id = match created_event {
        WorkEvent::WorkItemCreated { item } => item.id,
        _ => unreachable!("submit_with liefert das gebaute Ereignis unverändert zurück"),
    };

    let claimed_at = now_ms();
    let integration_id_for_claim = integration_id.clone();
    let (_, claimed_event) = store.submit_with(move |snap| {
        Ok(WorkEvent::WorkItemClaimed {
            item: integration_id_for_claim,
            agent: RUNTIME_AGENT_ID.to_string(),
            attempt: snap.next_attempt_id(),
            lease_expires_ms: claimed_at + 60_000,
            at_ms: claimed_at,
        })
    })?;
    let attempt_id = match claimed_event {
        WorkEvent::WorkItemClaimed { attempt, .. } => attempt,
        _ => unreachable!("submit_with liefert das gebaute Ereignis unverändert zurück"),
    };
    on_progress(WorkProgress::ItemStarted {
        item: integration_id.clone(),
        title: "Integration".to_string(),
        attempt: 1,
        max_attempts: 1,
    });

    let target_branch = match crate::git::current_branch(workspace) {
        Ok(b) => b,
        Err(e) => {
            return fail_integration(
                store,
                &integration_id,
                &attempt_id,
                format!("Ausgangsbranch nicht ermittelbar: {e}"),
                on_progress,
            )
            .map(|()| true);
        }
    };

    for id in &merged_ids {
        let branch = item_branch_name(&project_id, id);
        let message = format!("Integration: {id} in {target_branch} mergen");
        match crate::git::merge(workspace, &branch, &message) {
            Ok(crate::git::MergeOutcome::Merged) => {
                on_progress(WorkProgress::Note(format!(
                    "Integration '{integration_id}': '{id}' gemergt."
                )));
            }
            Ok(crate::git::MergeOutcome::Conflict) => {
                let files = crate::git::conflicted_files(workspace).unwrap_or_default();
                // Kein automatischer Auflösungsversuch (§28) — abbrechen und
                // dem Bediener die betroffenen Dateien nennen.
                let _ = crate::git::abort_merge(workspace);
                let reason = format!(
                    "Merge-Konflikt beim Zusammenführen von '{id}' ({branch}) in \
                     '{target_branch}': {}",
                    files.join(", ")
                );
                return fail_integration(store, &integration_id, &attempt_id, reason, on_progress)
                    .map(|()| true);
            }
            Err(e) => {
                return fail_integration(
                    store,
                    &integration_id,
                    &attempt_id,
                    format!("Merge von '{id}' ({branch}) fehlgeschlagen: {e}"),
                    on_progress,
                )
                .map(|()| true);
            }
        }
    }

    let at_ms = now_ms();
    // Code-Review-Befund 2: ein eigenes `WorkEvent::IntegrationMerged` war ein
    // No-op in `state::apply` und duplizierte nur, was hier schon feststeht —
    // Zielbranch und gemergte Items landen deshalb direkt in der `summary`
    // dieses `AttemptFinished`, die für das Integrations-Item ohnehin schon
    // eine Zusammenfassung trägt. `agentkit work status`/`items`/`events`
    // zeigen so weiterhin, was gemergt wurde: `items`/`status` über
    // `WorkItem`/`WorkAttempt` (Item bleibt `Completed`, siehe
    // `WorkEvent::WorkItemCompleted` unten), `events` über genau diese Zeile.
    store.submit(WorkEvent::AttemptFinished {
        attempt: attempt_id.clone(),
        status: AttemptStatus::Succeeded,
        summary: Some(format!(
            "{} Item(s) in '{target_branch}' gemergt: {}.",
            merged_ids.len(),
            merged_ids.join(", ")
        )),
        failure: None,
        steps: 0,
        tool_calls: 0,
        at_ms,
    })?;
    store.submit(WorkEvent::WorkItemCompleted {
        item: integration_id.clone(),
        attempt: attempt_id,
        at_ms,
    })?;
    on_progress(WorkProgress::ItemDone {
        item: integration_id,
        ok: true,
        summary: format!("{} Item(s) in '{target_branch}' gemergt.", merged_ids.len()),
    });
    Ok(true)
}

/// Prüft, dass der Arbeitsbaum des Bedieners die Integration aufnehmen kann:
/// sauber und auf einem Branch. Die Items selbst arbeiten in eigenen
/// Worktrees und berühren diesen Arbeitsbaum nicht — der Merge am Ende schon.
///
/// - **Nicht sauber:** uncommittete Änderungen sähe kein Agent (ein Worktree
///   startet auf einem Commit), und der Merge könnte an ihnen scheitern.
///   `.agentkit` (die Buchführung der Laufzeit) zählt nicht, siehe
///   `git::is_clean`.
/// - **Losgelöster HEAD:** ohne ausgecheckten Branch gibt es nichts, wohinein
///   die Item-Branches gemergt werden könnten.
///
/// Beides ist ein Bedienungsfehler, kein fachlicher Fehlschlag — ein harter
/// `WorkError`, kein Versuch wird verbraucht.
fn ensure_integration_target_ready(workspace: &str) -> Result<(), WorkError> {
    match crate::git::is_clean(workspace) {
        Ok(true) => {}
        Ok(false) => {
            return Err(WorkError::Invalid(format!(
                "Arbeitsbaum von '{workspace}' ist nicht sauber — unter Git-Isolation arbeiten \
                 die Items in eigenen Worktrees auf dem committeten Stand, und ihre Branches \
                 werden am Ende hierher gemergt. Committe oder sichere die Änderungen ('git \
                 stash'), bevor der Lauf fortgesetzt wird."
            )));
        }
        Err(e) => {
            return Err(WorkError::Invalid(format!(
                "Arbeitsbaum-Status von '{workspace}' nicht prüfbar — {e}"
            )));
        }
    }
    match crate::git::current_branch(workspace) {
        Ok(branch) if branch == "HEAD" => Err(WorkError::Invalid(format!(
            "Das Repository von '{workspace}' steht auf einem losgelösten HEAD (kein Branch \
             ausgecheckt) — Git-Isolation braucht einen Ausgangsbranch, in den die \
             Item-Branches am Ende gemergt werden. Wechsle auf einen Branch ('git switch \
             <branch>' oder 'git switch -c <neuer-branch>') und setze den Lauf fort."
        ))),
        Ok(_) => Ok(()),
        Err(e) => Err(WorkError::Invalid(format!(
            "Aktueller Branch von '{workspace}' nicht ermittelbar — {e}"
        ))),
    }
}

/// Schließt einen gescheiterten Merge-Versuch des Integrations-Items ab —
/// derselbe Mechanismus wie jeder andere fachliche Fehlschlag
/// (`recovery::finish_failed_attempt`, siehe dort). `max_attempts` ist beim
/// Integrations-Item immer 1 (siehe [`run_integration_item`]): die
/// Freigabe-Closure wird von `finish_failed_attempt` nur aufgerufen, wenn
/// nach dem gerade journalten Fehlschlag noch Versuche übrig sind — das ist
/// hier nie der Fall, ein Merge-Konflikt bekommt keinen automatischen
/// zweiten Versuch (§28).
fn fail_integration(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    attempt_id: &AttemptId,
    reason: String,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<(), WorkError> {
    let at_ms = now_ms();
    store.submit(WorkEvent::AttemptFinished {
        attempt: attempt_id.clone(),
        status: AttemptStatus::Failed,
        summary: None,
        failure: Some(FailureInfo {
            kind: FailureKind::MergeConflict,
            message: reason.clone(),
        }),
        steps: 0,
        tool_calls: 0,
        at_ms,
    })?;
    crate::recovery::finish_failed_attempt(
        store,
        item_id,
        attempt_id,
        |_item| {
            unreachable!("Integrations-Item hat max_attempts == 1 — nach dem ersten Fehlschlag immer erschöpft")
        },
        at_ms,
    )?;
    on_progress(WorkProgress::ItemDone {
        item: item_id.clone(),
        ok: false,
        summary: reason,
    });
    Ok(())
}

/// Git-Isolation EINES Versuchs (§19 — siehe `agentkit_work/README.md`
/// Abschnitt „Git-Isolation"). `Inactive`, wenn `WorkProject::git_isolation`
/// aus ist oder dieses Item nicht schreibt (`WorkItemKind::writes_workspace`);
/// sonst `Active`: das Item hat einen eigenen Git-Worktree auf seinem Branch,
/// und der Agent arbeitet DORT, nicht im Arbeitsbaum des Bedieners.
///
/// Ein eigener Arbeitsbaum je Item ist die Voraussetzung dafür, dass mehrere
/// Versuche gleichzeitig laufen können — ein Branchwechsel im EINEN
/// Arbeitsbaum (der Zuschnitt vor den parallelen Workern) ließ nur einen
/// Schreiber zu. Er gilt bewusst auch bei einem einzigen Worker: zwei
/// Isolationsmechanismen nebeneinander wären doppelter Code für dasselbe
/// Ergebnis.
enum GitAttemptCtx {
    Inactive,
    Active(ActiveGitAttempt),
}

struct ActiveGitAttempt {
    /// Workspace des Projekts im Arbeitsbaum des Bedieners — von hier aus
    /// verwaltet Git die Worktrees.
    repo_workspace: String,
    /// Wurzel des Worktrees dieses Items.
    worktree: std::path::PathBuf,
    /// Workspace des Agenten: der Worktree, plus derselbe Unterordner, in dem
    /// der Projekt-Workspace im Repository liegt.
    workspace: String,
    branch: String,
    start_point: String,
}

impl GitAttemptCtx {
    /// Bereitet die Git-Isolation für EINEN Versuch vor: der Worktree des
    /// Items wird angelegt oder — bei einem Retry bzw. nach einem Absturz —
    /// wiederverwendet und von Resten befreit (`git::ensure_worktree`).
    ///
    /// Der Item-Branch startet auf dem Stand seiner VORGÄNGER, nicht auf
    /// `base_revision` allein: deren Branches ([`dependency_branches`]) werden
    /// in Reihenfolge der Item-ID hineingemergt, mit Vorspulen. Ohne Vorgänger
    /// bleibt es bei `base_revision`; mit genau einem steht der Branch danach
    /// exakt auf dessen Stand. Vorher sah ein Item „Tests für W-2" den Code
    /// von W-2 nicht — der lag bis zur Integration nur auf dessen Branch.
    /// Das Mergen ist bei jedem Versuch dasselbe und ab dem zweiten ein No-Op;
    /// der Rollback-Punkt ist deshalb einfach der Stand NACH dem Mergen, ohne
    /// dass ihn jemand speichern müsste.
    ///
    /// Das innere `Err` ist ein Merge-Konflikt zwischen den Vorgängern: kein
    /// Bedienungsfehler, sondern ein fachlicher Fehlschlag des Versuchs
    /// (`FailureKind::MergeConflict`) — der Worktree ist dann wieder entfernt.
    fn prepare(
        state: &WorkState,
        item: &WorkItem,
        project_id: &str,
        base_revision: Option<&str>,
        workspace: &str,
        worktrees_dir: &std::path::Path,
    ) -> Result<Result<Self, String>, WorkError> {
        let project_git_isolation = state.project.as_ref().is_some_and(|p| p.git_isolation);
        if !project_git_isolation || !item.kind.writes_workspace() {
            return Ok(Ok(GitAttemptCtx::Inactive));
        }
        let Some(start_point) = base_revision else {
            return Err(WorkError::Invalid(format!(
                "Item '{}': kein Ausgangs-Commit bekannt ('base_revision' des Laufs fehlt) — \
                 Git-Isolation braucht einen bestehenden Commit als Startpunkt.",
                item.id
            )));
        };
        let invalid =
            |what: &str, e: String| WorkError::Invalid(format!("Item '{}': {what} — {e}", item.id));
        let branch = item_branch_name(project_id, &item.id);
        let worktree = worktrees_dir.join(&item.id);
        let prefix = crate::git::workspace_prefix(workspace)
            .map_err(|e| invalid("Lage des Workspace im Repository nicht ermittelbar", e))?;
        crate::git::ensure_worktree(workspace, &worktree, &branch, start_point)
            .map_err(|e| invalid(&format!("Worktree für '{branch}' nicht vorbereitbar"), e))?;
        let agent_workspace = worktree.join(&prefix);
        // Ein Workspace-Unterordner ohne verfolgte Datei existiert im frisch
        // ausgecheckten Worktree noch nicht.
        std::fs::create_dir_all(&agent_workspace)
            .map_err(|e| invalid("Workspace im Worktree nicht anlegbar", e.to_string()))?;
        let mut active = ActiveGitAttempt {
            repo_workspace: workspace.to_string(),
            worktree,
            workspace: agent_workspace.to_string_lossy().to_string(),
            branch,
            start_point: start_point.to_string(),
        };
        let dependencies = dependency_branches(state, item, project_id);
        for (dep_id, dep_branch) in &dependencies {
            // Ein Vorgänger ohne Branch (z. B. aus einem Lauf, der ihn nie
            // angelegt hat) hat nichts beizutragen.
            if !crate::git::branch_exists(workspace, dep_branch).unwrap_or(false) {
                continue;
            }
            let message = format!("Vorgänger {dep_id} in {} übernehmen", active.branch);
            match crate::git::merge_fast_forward(&active.workspace, dep_branch, &message) {
                Ok(crate::git::MergeOutcome::Merged) => {}
                Ok(crate::git::MergeOutcome::Conflict) => {
                    let files = crate::git::conflicted_files(&active.workspace).unwrap_or_default();
                    let branch = active.branch.clone();
                    crate::git::abort_merge(&active.workspace).map_err(WorkError::Invalid)?;
                    GitAttemptCtx::Active(active)
                        .release()
                        .map_err(WorkError::Invalid)?;
                    return Ok(Err(format!(
                        "Merge-Konflikt beim Übernehmen des Vorgängers '{dep_id}' ({dep_branch}) \
                         in '{branch}': {}. Die Vorgänger dieses Items widersprechen sich in \
                         diesen Dateien — das kann nur von Hand aufgelöst werden.",
                        files.join(", ")
                    )));
                }
                Err(e) => {
                    return Err(invalid(
                        &format!("Vorgänger '{dep_id}' ({dep_branch}) nicht übernehmbar"),
                        e,
                    ));
                }
            }
        }
        if !dependencies.is_empty() {
            active.start_point =
                crate::git::current_commit(&active.workspace).map_err(WorkError::Invalid)?;
        }
        Ok(Ok(GitAttemptCtx::Active(active)))
    }

    fn is_active(&self) -> bool {
        matches!(self, GitAttemptCtx::Active(_))
    }

    fn branch_name(&self) -> Option<&str> {
        match self {
            GitAttemptCtx::Active(a) => Some(&a.branch),
            GitAttemptCtx::Inactive => None,
        }
    }

    /// Workspace des Agenten im Worktree — `None` ohne Git-Isolation, dann
    /// arbeitet der Versuch im Workspace des Projekts.
    fn workspace(&self) -> Option<&str> {
        match self {
            GitAttemptCtx::Active(a) => Some(&a.workspace),
            GitAttemptCtx::Inactive => None,
        }
    }

    /// Stagt und committet — Aufrufer ist [`record_success`], NACH dem
    /// zugrundeliegenden Agenten-Versuch, aber VOR jeder Verifikationspolicy:
    /// der Commit gehört zum Versuch selbst, unabhängig davon, was eine
    /// eventuell folgende Prüfung später entscheidet (sie prüft das
    /// Ergebnis, sie erzeugt es nicht). `Ok(None)` bei `Inactive` oder wenn
    /// nichts geändert wurde — beides kein Fehler.
    fn commit(&self, item_title: &str, attempt_number: u32) -> Result<Option<String>, WorkError> {
        let GitAttemptCtx::Active(a) = self else {
            return Ok(None);
        };
        let message = format!("{item_title} (Versuch {attempt_number})");
        crate::git::commit_all(&a.workspace, &message).map_err(WorkError::Invalid)
    }

    /// Verwirft die Änderungen des Versuchs (Rollback aus §19) — Aufrufer ist
    /// [`record_failure`]/[`record_interrupted`]: ein gescheiterter oder
    /// unterbrochener Versuch darf den NÄCHSTEN nicht vergiften. Verworfen,
    /// nicht aufgehoben: die Diagnose steht ohnehin im Journal (`Failure` am
    /// Attempt) und in den bereits journalten Artefakten, nicht im
    /// Arbeitsbaum. Trifft nur den Worktree des Items. Ein No-Op bei
    /// `Inactive`.
    fn discard(&self) -> Result<(), WorkError> {
        let GitAttemptCtx::Active(a) = self else {
            return Ok(());
        };
        crate::git::discard_changes(&a.workspace, &a.start_point).map_err(WorkError::Invalid)
    }

    /// Entfernt den Worktree des Items; der Branch bleibt (die Integration
    /// mergt ihn, die Provenance verweist auf ihn). Aufrufer ist
    /// [`run_attempt`], sobald für dieses Item kein weiterer Versuch
    /// unmittelbar ansteht — ein Worktree ist eine volle Arbeitskopie samt
    /// Build-Verzeichnis und soll nicht liegen bleiben. Ein No-Op bei
    /// `Inactive`.
    fn release(&self) -> Result<(), String> {
        let GitAttemptCtx::Active(a) = self else {
            return Ok(());
        };
        crate::git::remove_worktree(&a.repo_workspace, &a.worktree)
    }
}

/// Die Item-Branches, auf denen `item` aufbaut: seine abgeschlossenen,
/// git-isolierten Abhängigkeiten, nach Item-ID sortiert (deterministisch).
/// Durch eine NICHT isolierte Abhängigkeit (Review, Planung) wird
/// hindurchgegriffen — sie hat keinen eigenen Branch, ihre Vorgänger aber
/// schon. Transitiv Entferntes braucht keine eigene Suche: der Branch eines
/// Vorgängers enthält dessen Vorgänger bereits.
fn dependency_branches(
    state: &WorkState,
    item: &WorkItem,
    project_id: &str,
) -> Vec<(WorkItemId, String)> {
    let mut found: Vec<WorkItemId> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<&WorkItemId> = item.dependencies.iter().collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(dep) = state.items.get(id) else {
            continue;
        };
        if !dep.kind.writes_workspace() {
            stack.extend(dep.dependencies.iter());
        } else if dep.status == WorkItemStatus::Completed {
            found.push(id.clone());
        }
    }
    found.sort_by_key(|id| id_order(id));
    found
        .into_iter()
        .map(|id| {
            let branch = item_branch_name(project_id, &id);
            (id, branch)
        })
        .collect()
}

/// Was jede `record_*`-Journalfunktion über den Versuch braucht — nur
/// zusammengefasst, damit die drei Funktionen nicht dieselben Parameter
/// einzeln durchreichen (sonst überschritte `record_success` allein mit Git-
/// Isolation clippys `too_many_arguments`-Schwelle). Kein eigenes Verhalten,
/// reine Bündelung. `title`/`attempt_number` werden nur für die Commit-
/// Nachricht gebraucht (`record_success`), `git` (Phase 7) außerdem für den
/// Rollback in `record_failure`/`record_interrupted` — beide Konsumenten
/// bekommen trotzdem alle Felder, um nicht drei fast identische Varianten
/// dieser Struktur zu pflegen. `workspace` ist das Verzeichnis, in dem der
/// Versuch TATSÄCHLICH gearbeitet hat — unter Git-Isolation der Worktree des
/// Items, sonst der Workspace des Projekts; dort läuft auch das Prüfkommando.
struct AttemptOutcomeMeta<'a> {
    item_id: WorkItemId,
    attempt_id: AttemptId,
    steps: u32,
    tool_calls: u32,
    at_ms: u64,
    title: String,
    attempt_number: u32,
    workspace: String,
    git: &'a GitAttemptCtx,
}

/// Was der Claim über einen Versuch festhält — alles, was die folgenden
/// Schritte brauchen, ohne den Zustand noch einmal zu befragen.
struct ClaimedAttempt {
    item_id: WorkItemId,
    attempt_id: AttemptId,
    attempt_number: u32,
    /// Kennung des Workers, der den Versuch fährt (`WorkAttempt::agent_id`).
    agent_id: String,
    claimed_at_ms: u64,
    /// Zahl der Items VOR diesem Versuch — Grundlage für die Prüfung, ob ein
    /// Planungsversuch wirklich etwas angelegt hat. Der Vergleich ist
    /// verlässlich, weil eine Zerlegung exklusiv läuft (siehe
    /// `scheduler::decide`): währenddessen legt kein anderer Agent Items an.
    items_before: usize,
}

/// Was die Ausführung eines Versuchs hinterlässt: das Rohergebnis, noch
/// unbewertet. Die Bewertung (Erfolg, Fehlschlag, Abbruch) und das Journalen
/// sind Sache von [`record_attempt`].
struct ExecutedAttempt {
    pkg: AgentWorkPackage,
    git: GitAttemptCtx,
    result: AttemptResult,
    steps: u32,
    tool_calls: u32,
}

enum AttemptResult {
    /// Der Executor ist zurückgekehrt: seine Antwort (bzw. der Grund, aus dem
    /// der Lauf gar nicht zustande kam) und ein eventuelles `work_submit`.
    Finished {
        response: Result<String, String>,
        submission: Option<WorkSubmission>,
    },
    /// Die Vorgänger dieses Items lassen sich nicht konfliktfrei
    /// zusammenführen — der Versuch scheitert, BEVOR ein Agent auf einem
    /// halben Stand losarbeitet.
    MergeConflict(String),
}

/// Beansprucht `item_id` für `agent_id`: legt Versuch und Lease an und meldet
/// den Start. Atomar — `state::apply` lehnt einen Claim auf ein Item ab, das
/// schon läuft.
fn claim_attempt(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    agent_id: &str,
    lease_secs: u64,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<ClaimedAttempt, WorkError> {
    let before = store.snapshot();
    // Nur die Referenz halten: das ganze Item (Beschreibung, Abhängigkeiten,
    // Kriterien) klonen wäre hier verschwendet, gebraucht werden nur diese
    // drei Felder für die `ItemStarted`-Meldung.
    let item_before = before
        .items
        .get(item_id)
        .ok_or_else(|| WorkError::NotFound(format!("WorkItem '{item_id}'")))?;
    let attempt_number = item_before.attempt_count + 1;
    let max_attempts_before = item_before.max_attempts;
    let title_before = item_before.title.clone();
    let items_before = before.items.len();

    let claimed_at = now_ms();
    let lease_expires_ms = claimed_at + lease_secs * 1000;
    let item_id_for_claim = item_id.clone();
    let agent_for_claim = agent_id.to_string();
    // ID-Vergabe im Schreiber-Lock (`submit_with`), nicht aus `before`
    // (Snapshot außerhalb des Locks): Agenten anderer Versuche schreiben
    // gleichzeitig ins Journal (`work_add_item`, `work_artifact`).
    let (_, claimed_event) = store.submit_with(move |snapshot| {
        Ok(WorkEvent::WorkItemClaimed {
            item: item_id_for_claim,
            agent: agent_for_claim,
            attempt: snapshot.next_attempt_id(),
            lease_expires_ms,
            at_ms: claimed_at,
        })
    })?;
    let attempt_id: AttemptId = match claimed_event {
        WorkEvent::WorkItemClaimed { attempt, .. } => attempt,
        _ => unreachable!("submit_with liefert das gebaute Ereignis unverändert zurück"),
    };
    on_progress(WorkProgress::ItemStarted {
        item: item_id.clone(),
        title: title_before,
        attempt: attempt_number,
        max_attempts: max_attempts_before,
    });
    Ok(ClaimedAttempt {
        item_id: item_id.clone(),
        attempt_id,
        attempt_number,
        agent_id: agent_id.to_string(),
        claimed_at_ms: claimed_at,
        items_before,
    })
}

/// Baut das Arbeitspaket, bereitet die Git-Isolation vor und ruft den
/// Executor. Zählt dabei Schritte und Tool-Aufrufe und verlängert das Lease
/// im Event-Callback des laufenden Agenten.
fn execute_attempt(
    store: &Arc<WorkStore>,
    claim: &ClaimedAttempt,
    budget: &WorkBudget,
    started_at_ms: u64,
    cfg: &RunnerConfig,
    executor: &dyn AgentExecutor,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<ExecutedAttempt, WorkError> {
    let item_id = &claim.item_id;
    let attempt_id = &claim.attempt_id;

    let snapshot = store.snapshot();
    let mut pkg = AgentWorkPackage::build(
        &snapshot,
        item_id,
        &cfg.workspace,
        budget.max_steps_per_attempt,
    )?;
    // Recall NACH `build` setzen (siehe `AgentWorkPackage::graph_recall`-Doku)
    // — die Anfrage ist Titel plus Beschreibung des Items, nichts Cleveres:
    // beides steht schon im Auftragstext, der Recall soll dieselbe Frage
    // beantworten, die der Agent gleich liest.
    if let Some(gateway) = &cfg.graph {
        let query = format!("{}\n\n{}", pkg.item.title, pkg.item.description);
        pkg.graph_recall = gateway.recall(&query);
    }
    // Befund 2 der Handprobe: die verbleibende Wandzeit DES LAUFS, nicht das
    // volle Budget — sonst könnte ein einzelnes Item das Budget allein
    // überziehen. `scheduler::decide` hätte diesen Versuch gar nicht erst
    // gestartet, wäre das Budget schon aufgebraucht (Reihenfolge „Budget vor
    // Auswahl", siehe dort) — die Subtraktion bleibt trotzdem
    // `saturating_sub`, statt sich auf diese Invariante zu verlassen.
    if let Some(max_secs) = budget.max_wall_time_secs {
        let elapsed_secs = now_ms().saturating_sub(started_at_ms) / 1000;
        pkg.remaining_wall_secs = Some(max_secs.saturating_sub(elapsed_secs));
    }
    let project_id = snapshot
        .project
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_default();
    let repository_revision = snapshot
        .runs
        .get(&pkg.item.run_id)
        .and_then(|r| r.base_revision.clone());

    // Git-Isolation (§19) — VOR dem eigentlichen Agentenlauf vorbereitet,
    // damit ein Fehler hier (fehlender Startpunkt, Worktree nicht anlegbar)
    // den Versuch gar nicht erst beginnen lässt. Ein früher Rücksprung unten
    // lässt den Worktree stehen; der nächste Versuch übernimmt ihn bereinigt.
    let git = match GitAttemptCtx::prepare(
        &snapshot,
        &pkg.item,
        &project_id,
        repository_revision.as_deref(),
        &cfg.workspace,
        &absolute(&runtime_dir(store, &cfg.workspace).join("worktrees")),
    )? {
        Ok(git) => git,
        Err(reason) => {
            return Ok(ExecutedAttempt {
                pkg,
                git: GitAttemptCtx::Inactive,
                result: AttemptResult::MergeConflict(reason),
                steps: 0,
                tool_calls: 0,
            });
        }
    };

    // Artefakte liegen im Projektverzeichnis des Journals. Ohne Journal
    // (`in_memory`, z. B. Kurztests) gibt es kein solches Verzeichnis — dann
    // ein Unterverzeichnis des Workspace, damit `work_artifact` trotzdem
    // etwas Reales zum Schreiben hat, statt gegen `None` zu scheitern.
    let mut artifacts_dir = match store.dir() {
        Some(dir) => dir.join("artifacts"),
        None => std::path::Path::new(&cfg.workspace)
            .join(".agentkit")
            .join("work-artifacts"),
    };
    // Unter Git-Isolation arbeitet der Agent im Worktree seines Items. Die
    // Artefakte bleiben, wo sie sind (im Projektverzeichnis des Journals,
    // außerhalb jedes Worktrees) — der Agent bekommt sie als Lese-Wurzel und
    // unter absoluten Pfaden, weil ein Pfad relativ zum Worktree sie nicht
    // mehr erreicht.
    if let Some(worktree_workspace) = git.workspace() {
        pkg.workspace = worktree_workspace.to_string();
        artifacts_dir = absolute(&artifacts_dir);
        for (_, path, _) in &mut pkg.predecessor_artifacts {
            let below = path.strip_prefix("artifacts/").unwrap_or(path);
            *path = artifacts_dir
                .join(below)
                .to_string_lossy()
                .replace('\\', "/");
        }
        pkg.read_roots
            .push(artifacts_dir.to_string_lossy().to_string());
    }

    let submission_handle: Arc<Mutex<Option<WorkSubmission>>> = Arc::new(Mutex::new(None));
    let ctx = WorkToolCtx {
        run_id: pkg.item.run_id.clone(),
        work_item_id: item_id.clone(),
        attempt_id: attempt_id.clone(),
        agent_id: claim.agent_id.clone(),
        max_attempts: pkg.item.max_attempts,
        project_id,
        repository_revision,
        artifacts_dir,
        submission: submission_handle.clone(),
        gateway: cfg.graph.clone(),
        verifies: pkg.item.verifies.clone(),
    };

    let mut steps: u32 = 0;
    let mut tool_calls: u32 = 0;
    let mut last_renew_ms = claim.claimed_at_ms;
    let mut lease_error: Option<WorkError> = None;
    let heartbeat_ms = cfg.heartbeat_secs.saturating_mul(1000);
    let heartbeat_store = store.clone();
    let heartbeat_item = item_id.clone();
    let heartbeat_attempt: AttemptId = attempt_id.clone();

    let response = {
        let mut on_event = |ev: &AgentEvent| {
            match ev.etype {
                agentkit::STEP => steps += 1,
                agentkit::TOOL_CALL => tool_calls += 1,
                _ => {}
            }
            let now = now_ms();
            if now.saturating_sub(last_renew_ms) >= heartbeat_ms {
                let renewed_expires_ms = now + cfg.lease_secs * 1000;
                match heartbeat_store.submit(WorkEvent::LeaseRenewed {
                    item: heartbeat_item.clone(),
                    attempt: heartbeat_attempt.clone(),
                    lease_expires_ms: renewed_expires_ms,
                    at_ms: now,
                }) {
                    Ok(_) => last_renew_ms = now,
                    Err(e) => lease_error = Some(e),
                }
            }
            on_progress(WorkProgress::Agent {
                item: heartbeat_item.clone(),
                attempt: claim.attempt_number,
                event: ev.clone(),
            });
        };
        executor.execute(&pkg, ctx, store.clone(), &mut on_event)
    };

    // Ein Fehler beim Lease-Verlängern darf nicht stillschweigend verschwinden
    // — er hätte den Versuch aber auch nicht abbrechen dürfen (das Modell
    // durfte weiterlaufen). Deshalb erst NACH `execute` prüfen und melden.
    if let Some(e) = lease_error {
        return Err(e);
    }

    let submission = submission_handle
        .lock()
        .expect("Work-Submission-Lock nicht poisoned")
        .take();
    Ok(ExecutedAttempt {
        pkg,
        git,
        result: AttemptResult::Finished {
            response,
            submission,
        },
        steps,
        tool_calls,
    })
}

/// Bewertet das Ergebnis eines ausgeführten Versuchs und journalt es:
/// Erfolg (samt Commit und Verifikation), Fehlschlag (samt Rollback und
/// Freigabe für einen weiteren Versuch) oder Abbruch.
fn record_attempt(
    store: &Arc<WorkStore>,
    claim: &ClaimedAttempt,
    executed: ExecutedAttempt,
    cfg: &RunnerConfig,
    executor: &dyn AgentExecutor,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
    let ExecutedAttempt {
        pkg,
        git,
        result,
        steps,
        tool_calls,
    } = executed;
    let item_id = &claim.item_id;
    let attempt_id = &claim.attempt_id;
    let is_planning = pkg.item.kind == WorkItemKind::Planning;

    let meta = AttemptOutcomeMeta {
        item_id: item_id.clone(),
        attempt_id: attempt_id.clone(),
        steps,
        tool_calls,
        at_ms: now_ms(),
        title: pkg.item.title.clone(),
        attempt_number: claim.attempt_number,
        workspace: pkg.workspace.clone(),
        git: &git,
    };
    let outcome = match result {
        // Derselbe Ausgang wie beim Integrations-Item
        // (`FailureKind::MergeConflict`), nur früher.
        AttemptResult::MergeConflict(reason) => {
            record_failure(store, meta, FailureKind::MergeConflict, reason, on_progress)?
        }
        AttemptResult::Finished {
            response: Err(msg), ..
        } => record_failure(store, meta, FailureKind::ModelFailure, msg, on_progress)?,
        AttemptResult::Finished {
            response: Ok(answer),
            submission,
        } => {
            match submission {
                // Ein Versuch, der seine eigenen Akzeptanzkriterien als NICHT
                // erfüllt meldet, ist nicht gelungen.
                //
                // Das klingt selbstverständlich und war es nicht: `work_submit`
                // sammelte `criteria[].met`, sagte dem Agenten sogar, wie viele
                // offen sind — und der Runner nahm trotzdem die
                // Zusammenfassung und buchte Erfolg. Beobachtet an einem
                // Polyglot-Task, dessen Prüf-Item ordentlich meldete „ein Test
                // schlägt weiterhin fehl" (2 von 3 Kriterien `met: false`) und
                // als `completed` geschlossen wurde. Der Agent hatte recht, die
                // Laufzeit hat ihn überstimmt.
                //
                // Als Fehlschlag greift stattdessen genau die Maschinerie, die
                // dafür da ist: der nächste Versuch bekommt einen FRISCHEN
                // Agenten, der die offenen Kriterien in `previous_failures`
                // vorfindet und einen anderen Weg suchen kann.
                Some(sub) if sub.criteria.iter().any(|c| !c.met) => {
                    let offen: Vec<String> = sub
                        .criteria
                        .iter()
                        .filter(|c| !c.met)
                        .map(|c| {
                            if c.evidence.trim().is_empty() {
                                format!("'{}'", c.criterion)
                            } else {
                                format!("'{}' ({})", c.criterion, c.evidence)
                            }
                        })
                        .collect();
                    let grund = format!(
                        "Der Versuch hat {} Akzeptanzkriterium/-kriterien selbst als nicht \
                         erfüllt gemeldet: {}. Zusammenfassung: {}",
                        offen.len(),
                        offen.join("; "),
                        sub.summary
                    );
                    record_failure(
                        store,
                        meta,
                        FailureKind::VerificationFailure,
                        grund,
                        on_progress,
                    )?
                }
                Some(sub) => record_success(
                    store,
                    meta,
                    sub.summary,
                    &pkg.item.verification_policy,
                    cfg,
                    executor,
                    on_progress,
                )?,
                // Die Sentinels des Kerns entscheiden NUR, wenn keine Submission
                // vorliegt. Früher standen sie an erster Stelle: ein Agent, der
                // 'work_submit' in seinem letzten erlaubten Schritt rief, endete
                // mit "(max_steps erreicht)" — und die Laufzeit verwarf seine
                // fertige, als erfüllt gemeldete Arbeit.
                None if answer == "(abgebrochen)" => {
                    record_interrupted(store, meta, on_progress)?;
                    AttemptOutcome::Interrupted
                }
                None if answer == "(max_steps erreicht)" => {
                    record_failure(store, meta, FailureKind::MaxSteps, answer, on_progress)?
                }
                None if answer == "(keine Antwort)" => {
                    record_failure(store, meta, FailureKind::InvalidOutput, answer, on_progress)?
                }
                None if !answer.trim().is_empty() => {
                    let outcome = record_success(
                        store,
                        meta,
                        answer.clone(),
                        &pkg.item.verification_policy,
                        cfg,
                        executor,
                        on_progress,
                    )?;
                    on_progress(WorkProgress::Note(format!(
                        "Item '{item_id}': 'work_submit' wurde nicht aufgerufen — die Antwort \
                         des Agenten wurde als Zusammenfassung übernommen."
                    )));
                    outcome
                }
                None => record_failure(
                    store,
                    meta,
                    FailureKind::InvalidOutput,
                    "Agent lieferte weder eine Antwort noch einen 'work_submit'-Aufruf".to_string(),
                    on_progress,
                )?,
            }
        }
    };

    // Ein Planungsversuch, der erfolgreich war, aber kein einziges neues Item
    // angelegt hat, darf den Lauf nicht stillschweigend als "erledigt"
    // durchgehen lassen — ohne Folge-Items ist das Vorhaben unbearbeitet
    // geblieben, auch wenn der Scheduler danach `Decision::Done` meldet (der
    // Lauf hat dann schlicht keine offenen Items mehr).
    if is_planning && matches!(outcome, AttemptOutcome::Succeeded) {
        let items_after = store.snapshot().items.len();
        if items_after <= claim.items_before {
            on_progress(WorkProgress::Note(format!(
                "Item '{item_id}': Planungsversuch abgeschlossen, hat aber kein einziges Work \
                 Item angelegt — das Vorhaben bleibt unbearbeitet."
            )));
        }
    }

    // Phase 5b: ein Prüf-Item, das erfolgreich abschließt (`work_submit`
    // gerufen), OHNE je `work_verdict` gerufen zu haben, ließe das geprüfte
    // Item sonst für IMMER in `AwaitingVerification` hängen — ein echter
    // Stillstand (der Scheduler wartet auf eine Freigabe, die nie kommt, siehe
    // `Decision::AwaitingVerification`). Der Runner erkennt das HIER, direkt
    // nach dem Versuch, und wertet es als Ablehnung mit einem deutschen Grund
    // — derselbe Mechanismus wie eine echte `work_verdict`-Ablehnung
    // (`recovery::finish_failed_attempt`), nur mit fester Begründung. Trifft
    // NICHT zu, wenn `work_verdict` bereits entschieden hat: dann steht das
    // geprüfte Item längst nicht mehr auf `AwaitingVerification`.
    if matches!(outcome, AttemptOutcome::Succeeded) {
        if let Some(reviewed_id) = &pkg.item.verifies {
            let snap = store.snapshot();
            let still_awaiting = snap
                .items
                .get(reviewed_id)
                .is_some_and(|it| it.status == WorkItemStatus::AwaitingVerification);
            if still_awaiting {
                if let Some(lease) = snap.leases.get(reviewed_id) {
                    let reviewed_attempt_id = lease.attempt_id.clone();
                    let reject_at = now_ms();
                    store.submit(WorkEvent::VerificationRejected {
                        item: reviewed_id.clone(),
                        attempt: reviewed_attempt_id.clone(),
                        by: claim.agent_id.clone(),
                        reason: "Prüfer hat kein Urteil abgegeben".to_string(),
                        at_ms: reject_at,
                    })?;
                    crate::recovery::finish_failed_attempt(
                        store,
                        reviewed_id,
                        &reviewed_attempt_id,
                        |item| {
                            format!(
                                "Wiederholung {}/{} (Prüfer hat kein Urteil abgegeben)",
                                item.attempt_count + 1,
                                item.max_attempts
                            )
                        },
                        reject_at,
                    )?;
                    on_progress(WorkProgress::Note(format!(
                        "Item '{reviewed_id}': Prüf-Item '{item_id}' hat 'work_submit' ohne \
                         'work_verdict' aufgerufen — als Ablehnung gewertet."
                    )));
                }
            }
        }
    }

    // Der Worktree bleibt nur stehen, wenn gleich ein weiterer Versuch
    // DESSELBEN Items folgt (Retry, Fortsetzung nach Abbruch) — der spart
    // sich damit den Checkout und behält seine Build-Zwischenstände.
    let retry_expected = matches!(
        outcome,
        AttemptOutcome::FailedRetryable | AttemptOutcome::Interrupted
    );
    if !retry_expected {
        if let Err(e) = git.release() {
            on_progress(WorkProgress::Note(format!(
                "Item '{item_id}': Worktree nicht entfernbar ({e}) — er bleibt liegen und kann \
                 von Hand gelöscht werden."
            )));
        }
    }

    Ok(outcome)
}

/// Verzeichnis für die Buchführung der Laufzeit neben dem Journal: das
/// Projektverzeichnis des Stores, oder — ohne Journal (`in_memory`, z. B.
/// Kurztests) — `.agentkit` im Workspace.
fn runtime_dir(store: &WorkStore, workspace: &str) -> std::path::PathBuf {
    match store.dir() {
        Some(dir) => dir.to_path_buf(),
        None => std::path::Path::new(workspace).join(".agentkit"),
    }
}

/// `path` als absoluter Pfad, ohne ihn aufzulösen (er muss noch nicht
/// existieren). Git und der Agent im Worktree haben ein anderes
/// Arbeitsverzeichnis als dieser Prozess; ein relativer Pfad meinte dort
/// einen anderen Ort.
fn absolute(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Journalt einen erfolgreichen Versuch (`AttemptFinished`, immer `Succeeded`
/// — das ist die Bewertung des AGENTEN, unabhängig von einer eventuellen
/// späteren Prüfung) und schließt ihn dann gemäß `policy` ab (Phase 5a, §10):
///
/// - `None` — wie vor Phase 5a: direkt `WorkItemCompleted`.
/// - `AutomatedTests { command }` — `WorkItemSubmittedForVerification`, dann
///   SOFORT und deterministisch das Kommando im Workspace des Laufs prüfen
///   (siehe [`run_verification_command`]): Exit 0 → `VerificationApproved` +
///   `WorkItemCompleted`; sonst → `VerificationRejected` mit der letzten
///   Ausgabezeile als Grund, danach derselbe Mechanismus wie ein regulärer
///   fachlicher Fehlschlag (`recovery::finish_failed_attempt`,
///   `FailureKind::VerificationFailure`).
/// - `HumanApproval` — `WorkItemSubmittedForVerification`, sonst nichts: der
///   Runner rührt das Item nicht mehr an, bis `agentkit work approve`/`reject`
///   entscheidet.
/// - `IndependentAgent` (Phase 5b) — `WorkItemSubmittedForVerification`, dann
///   legt [`spawn_review_item`] automatisch ein Prüf-Item an; der Runner
///   rührt das geprüfte Item selbst nicht mehr an, bis `work_verdict`
///   (aufgerufen VOM Prüf-Item) entscheidet.
fn record_success(
    store: &Arc<WorkStore>,
    mut meta: AttemptOutcomeMeta<'_>,
    mut summary: String,
    policy: &VerificationPolicy,
    cfg: &RunnerConfig,
    executor: &dyn AgentExecutor,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
    let gateway = cfg.graph.as_ref();
    // Ein Prüfkommando, das das MODELL gesetzt hat (`work_add_item`), läuft
    // nur nach Freigabe und nie unter `--dry-run` — sonst wäre es ein Weg, an
    // der Freigabe von `run_shell` vorbei beliebige Programme zu starten.
    // Eines vom Bediener läuft wie bisher ohne Rückfrage. Ohne Freigabe
    // schließt das Item OHNE Prüfung ab (keine Policy, also auch keine
    // Promotion); der Grund steht in der Zusammenfassung des Versuchs und
    // damit im Journal, nicht nur auf stderr.
    let skipped = match policy {
        VerificationPolicy::AutomatedTests { command }
            if store.snapshot().is_agent_added(&meta.item_id) =>
        {
            executor.approve_verify_command(command).err().map(|grund| {
                format!(
                    "Prüfkommando '{command}' NICHT ausgeführt ({grund}) — Item ohne \
                     maschinelle Prüfung abgeschlossen."
                )
            })
        }
        _ => None,
    };
    let policy = match &skipped {
        Some(note) => {
            summary.push_str("\n\n");
            summary.push_str(note);
            on_progress(WorkProgress::Note(format!(
                "Item '{}': {note}",
                meta.item_id
            )));
            &VerificationPolicy::None
        }
        None => policy,
    };
    store.submit(WorkEvent::AttemptFinished {
        attempt: meta.attempt_id.clone(),
        status: AttemptStatus::Succeeded,
        summary: Some(summary.clone()),
        failure: None,
        steps: meta.steps,
        tool_calls: meta.tool_calls,
        at_ms: meta.at_ms,
    })?;

    // Git-Isolation (Phase 7, §19): der Commit gehört zum AGENTEN-Versuch
    // selbst — unabhängig davon, was eine eventuell folgende Verifikation
    // später entscheidet (sie prüft das Ergebnis, sie erzeugt es nicht).
    // Deshalb HIER, vor jeder Policy-Verzweigung unten. Ein Fehlschlag beim
    // Committen selbst ist ein KONFIGURATIONSFEHLER (dieselbe Haltung wie bei
    // `run_verification_command`), kein fachlicher — `?` bricht den Lauf
    // sichtbar ab, statt es als Retry misszudeuten.
    if let Some(commit_id) = meta.git.commit(&meta.title, meta.attempt_number)? {
        record_git_commit(store, &meta.item_id, &meta.attempt_id, &commit_id, meta.git)?;
        on_progress(WorkProgress::Note(format!(
            "Item '{}': Änderungen committet ({commit_id}).",
            meta.item_id
        )));
    } else if meta.git.is_active() {
        on_progress(WorkProgress::Note(format!(
            "Item '{}': Versuch erfolgreich, aber keine Änderungen im Workspace — kein Commit \
             erzeugt.",
            meta.item_id
        )));
    }

    if matches!(policy, VerificationPolicy::None) {
        return finish_completed(store, meta, summary, policy, gateway, on_progress);
    }
    // Ab hier verlangt JEDE verbleibende Policy dasselbe erste Ereignis —
    // einmal statt in jedem Zweig einzeln journalt (die beiden Zweige
    // unterschieden sich vorher nur in Text/Folgeaktion, nicht in diesem Schritt).
    store.submit(WorkEvent::WorkItemSubmittedForVerification {
        item: meta.item_id.clone(),
        attempt: meta.attempt_id.clone(),
        at_ms: meta.at_ms,
    })?;

    match policy {
        VerificationPolicy::None => unreachable!("oben schon behandelt"),
        VerificationPolicy::HumanApproval => {
            on_progress(WorkProgress::Note(format!(
                "Item '{}': Versuch erfolgreich, wartet jetzt auf manuelle Freigabe \
                 ('agentkit work approve|reject {} -p <projekt-id>').",
                meta.item_id, meta.item_id
            )));
            Ok(AttemptOutcome::AwaitingVerification)
        }
        VerificationPolicy::IndependentAgent => {
            let review_id = spawn_review_item(store, &meta.item_id, &meta.attempt_id, meta.at_ms)?;
            on_progress(WorkProgress::Note(format!(
                "Item '{}': Versuch erfolgreich, unabhängige Prüfung '{review_id}' wurde \
                 angelegt.",
                meta.item_id
            )));
            Ok(AttemptOutcome::AwaitingVerification)
        }
        VerificationPolicy::AutomatedTests { command } => {
            // Ein `WorkError` hier (Kommando nicht startbar, `try_wait`
            // scheitert) ist ein KONFIGURATIONSFEHLER des Operators — kein
            // fachlicher Fehlschlag des Versuchs. Würde er wie ein
            // abgelehntes Ergebnis behandelt, verbrauchte jeder weitere
            // Versuch sinnlos ein `max_attempts`-Kontingent an einem Kommando,
            // das nie startet (Befund des Code-Reviews), und der agentische
            // Anteil landete fälschlich als „vorheriger Fehlversuch" im
            // nächsten Arbeitspaket. `?` propagiert stattdessen bis zum
            // Aufrufer von `run_to_completion` — der Lauf bricht sichtbar ab,
            // statt den Fehler stillschweigend in einen Retry umzudeuten.
            match run_verification_command(command, &meta.workspace, VERIFY_COMMAND_TIMEOUT_SECS)? {
                VerificationRun::Passed => {
                    // Frischer Zeitstempel NACH dem (ggf. minutenlangen)
                    // Prüfkommando — `meta.at_ms` stammt noch von VOR der
                    // Prüfung; ihn hier weiterzuverwenden ließe
                    // `WorkItemCompleted` vor `VerificationApproved` datieren
                    // (Befund des Code-Reviews).
                    let approved_at = now_ms();
                    store.submit(WorkEvent::VerificationApproved {
                        item: meta.item_id.clone(),
                        attempt: meta.attempt_id.clone(),
                        by: AUTOMATED_TESTS_BY.to_string(),
                        reason: None,
                        at_ms: approved_at,
                    })?;
                    meta.at_ms = approved_at;
                    finish_completed(store, meta, summary, policy, gateway, on_progress)
                }
                VerificationRun::Failed(reason) => record_verification_rejected(
                    store,
                    &meta.item_id,
                    &meta.attempt_id,
                    reason,
                    on_progress,
                ),
            }
        }
    }
}

/// Schließt einen VERIFIZIERTEN (oder gar nicht verifikationspflichtigen)
/// Versuch ab: `WorkItemCompleted` + Promotion (nur bei `policy != None`,
/// siehe [`crate::graph::promote_after_completion`]) + Fortschrittsmeldung.
/// Gemeinsamer Kern der beiden Policy-Zweige in [`record_success`], die beide
/// auf demselben Weg enden.
fn finish_completed(
    store: &Arc<WorkStore>,
    meta: AttemptOutcomeMeta<'_>,
    summary: String,
    policy: &VerificationPolicy,
    gateway: Option<&Arc<dyn GraphGateway>>,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
    store.submit(WorkEvent::WorkItemCompleted {
        item: meta.item_id.clone(),
        attempt: meta.attempt_id.clone(),
        at_ms: meta.at_ms,
    })?;
    if let Some(msg) =
        crate::graph::promote_after_completion(store, gateway, &meta.item_id, policy, meta.at_ms)
    {
        on_progress(WorkProgress::Note(msg));
    }
    on_progress(WorkProgress::ItemDone {
        item: meta.item_id,
        ok: true,
        summary,
    });
    Ok(AttemptOutcome::Succeeded)
}

/// Journalt den Commit eines git-isolierten Versuchs (Phase 7, §9/§19) als
/// EIN Artefakt (`ArtifactKind::GitCommit`, `rel_path` = Item-Branch,
/// `commit_id` = die tatsächliche Commit-ID — siehe `WorkArtifact`-Doku für
/// die Begründung der abweichenden Feldbedeutung). Entsteht NUR hier, in der
/// Laufzeit selbst — nie über `work_artifact` (siehe `tools::ARTIFACT_KINDS`,
/// das `git_commit` bewusst nicht anbietet). Ein eigenes `WorkEvent` dafür
/// gibt es bewusst NICHT (mehr) — Code-Review-Befund 1: ein zweites Ereignis
/// war ein No-op in `state::apply` und duplizierte nur, was hier schon steht.
/// Eine sprechende Zeile in `agentkit work events` liefert stattdessen die
/// Anzeige (`cli::format_event_line`), die dieses Artefakt an `kind ==
/// "git_commit"` erkennt.
fn record_git_commit(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    attempt_id: &AttemptId,
    commit_id: &str,
    git: &GitAttemptCtx,
) -> Result<(), WorkError> {
    let branch = git.branch_name().unwrap_or_default().to_string();
    let commit_id_owned = commit_id.to_string();
    let (item_id, attempt_id) = (item_id.clone(), attempt_id.clone());
    store.submit_with(move |snap| {
        Ok(WorkEvent::ArtifactCreated {
            artifact: WorkArtifact {
                id: snap.next_artifact_id(),
                work_item_id: item_id,
                attempt_id,
                kind: ArtifactKind::GitCommit,
                rel_path: branch,
                summary: format!("Commit {commit_id_owned}"),
                created_at_ms: now_ms(),
                commit_id: Some(commit_id_owned),
            },
        })
    })?;
    Ok(())
}

/// Legt automatisch das Prüf-Item für `VerificationPolicy::IndependentAgent`
/// an (Phase 5b, §10/§26 Phase 6): Kind `Review`, hohe Priorität (kommt beim
/// nächsten Scheduler-Durchlauf sofort dran, da keine andere Abhängigkeit
/// mehr offen ist), Rolle `reviewer` (read-only, siehe `agent_framework_rs::
/// roles::builtin_roles`), `verification_policy: None` (siehe Doku an
/// `VerificationPolicy::IndependentAgent` — sonst unendlicher Regress) und
/// OHNE Abhängigkeit auf das geprüfte Item (ebenfalls dort begründet).
///
/// Die Beschreibung enthält die Akzeptanzkriterien des geprüften Items
/// WÖRTLICH sowie die Artefaktpfade DIESES Versuchs — ausdrücklich OHNE den
/// Gesprächsverlauf des Autors (§26 Phase 6): der Prüfer soll die Kriterien
/// anhand der Artefakte beurteilen, nicht die Arbeit noch einmal machen.
fn spawn_review_item(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    attempt_id: &AttemptId,
    at_ms: u64,
) -> Result<WorkItemId, WorkError> {
    let snapshot = store.snapshot();
    let reviewed = snapshot
        .items
        .get(item_id)
        .ok_or_else(|| WorkError::NotFound(format!("WorkItem '{item_id}'")))?;
    let reviewed_run_id = reviewed.run_id.clone();
    let reviewed_title = reviewed.title.clone();
    let acceptance_criteria = reviewed.acceptance_criteria.clone();
    let max_attempts = snapshot
        .project
        .as_ref()
        .map(|p| p.budget.max_attempts_per_item)
        .unwrap_or_else(|| WorkBudget::default().max_attempts_per_item);

    let mut artifacts: Vec<_> = snapshot
        .artifacts
        .values()
        .filter(|a| &a.attempt_id == attempt_id)
        .collect();
    artifacts.sort_by_key(|a| id_order(&a.id));

    let mut description = format!(
        "Prüfe den Versuch '{attempt_id}' von Work Item '{item_id}' ({reviewed_title}).\n\n\
         Du bekommst NICHT den Gesprächsverlauf des Autors — nur diese Angaben und die unten \
         genannten Artefakte. Prüfe die folgenden Akzeptanzkriterien anhand der Artefakte; mach \
         die Arbeit nicht neu.\n\n### Akzeptanzkriterien von {item_id}\n\n"
    );
    if acceptance_criteria.is_empty() {
        description.push_str(
            "Keine Akzeptanzkriterien definiert — bewerte das Ergebnis nach eigenem fachlichen \
             Urteil.\n\n",
        );
    } else {
        for crit in &acceptance_criteria {
            description.push_str("- ");
            description.push_str(crit);
            description.push('\n');
        }
        description.push('\n');
    }
    description.push_str(
        "### Artefakte des geprüften Versuchs\n\nLies diese Dateien mit 'read_file' — rate \
         ihren Inhalt nicht.\n\n",
    );
    if artifacts.is_empty() {
        description.push_str("Keine Artefakte abgelegt.\n\n");
    } else {
        for artifact in &artifacts {
            writeln!(
                description,
                "- `{}` — {}",
                artifact.rel_path, artifact.summary
            )
            .unwrap();
        }
        description.push('\n');
    }
    description.push_str(
        "Melde dein Urteil ausschließlich über 'work_verdict' (approved: ob alle Kriterien \
         erfüllt sind, reason: Begründung — Pflicht, auch bei Zustimmung). Schließe danach den \
         Versuch normal mit 'work_submit' ab.\n",
    );

    let reviewed_id = item_id.clone();
    let (_, event) = store.submit_with(move |snap| {
        let id = snap.next_item_id();
        Ok(WorkEvent::WorkItemCreated {
            item: WorkItem {
                id: id.clone(),
                run_id: reviewed_run_id,
                title: format!("Prüfung: {reviewed_title}"),
                description,
                kind: WorkItemKind::Review,
                status: WorkItemStatus::Pending,
                priority: 9,
                seq: id_order(&id),
                required_role: Some("reviewer".to_string()),
                // Bewusst OHNE Abhängigkeit auf das geprüfte Item (siehe
                // Doku an `VerificationPolicy::IndependentAgent`).
                dependencies: Vec::new(),
                acceptance_criteria: vec![
                    "Jedes Akzeptanzkriterium des geprüften Items wurde einzeln bewertet \
                     (erfüllt oder nicht, mit Beleg)."
                        .to_string(),
                    "'work_verdict' wurde mit einer Begründung aufgerufen.".to_string(),
                ],
                verification_policy: VerificationPolicy::None,
                verifies: Some(reviewed_id),
                attempt_count: 0,
                max_attempts,
                updated_at_ms: at_ms,
                claims_promoted: false,
                // Ein automatisch angelegtes Prüf-Item ist immer ein
                // Einzelagent — dieselbe Begründung wie beim Planungs-Item
                // oben: der Operator wählt den Executor beim Anlegen, nicht
                // die Laufzeit für ihre eigenen Folge-Items.
                executor: ExecutorKind::SingleAgent,
            },
        })
    })?;
    match event {
        WorkEvent::WorkItemCreated { item } => Ok(item.id),
        _ => unreachable!("submit_with liefert das gebaute Ereignis unverändert zurück"),
    }
}

/// Journalt eine ABGELEHNTE automatisierte Prüfung: `VerificationRejected` an
/// den (selbst erfolgreichen) Versuch, dann derselbe „fachlicher Fehlschlag"-
/// Mechanismus wie [`record_failure`] — `recovery::finish_failed_attempt`
/// erhöht `attempt_count` und gibt das Item frei, wenn noch Versuche übrig
/// sind, genau wie bei einem regulären, nicht-verifikationsbedingten
/// Fehlschlag. So bleibt „ist `max_attempts` erschöpft?" eine EINZIGE
/// Entscheidung im Code, egal ob der Agent selbst scheiterte oder nur seine
/// Prüfung.
fn record_verification_rejected(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    attempt_id: &AttemptId,
    reason: String,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
    let at_ms = now_ms();
    store.submit(WorkEvent::VerificationRejected {
        item: item_id.clone(),
        attempt: attempt_id.clone(),
        by: AUTOMATED_TESTS_BY.to_string(),
        reason: reason.clone(),
        at_ms,
    })?;
    let released = crate::recovery::finish_failed_attempt(
        store,
        item_id,
        attempt_id,
        |item| {
            format!(
                "Wiederholung {}/{} (automatisierte Prüfung abgelehnt)",
                item.attempt_count + 1,
                item.max_attempts
            )
        },
        at_ms,
    )?;
    on_progress(WorkProgress::ItemDone {
        item: item_id.clone(),
        ok: false,
        summary: format!("Prüfung abgelehnt: {reason}"),
    });
    Ok(if released {
        AttemptOutcome::FailedRetryable
    } else {
        AttemptOutcome::FailedExhausted
    })
}

/// Ausgang eines ausgeführten Prüfkommandos. Getrennt von `WorkError`, weil
/// „Prüfung nicht bestanden" ein fachliches Ergebnis ist (führt zu
/// `record_verification_rejected`, zählt gegen `max_attempts`) — anders als
/// ein `WorkError` aus [`run_verification_command`], der einen
/// KONFIGURATIONSFEHLER meldet (Kommando nicht startbar, Prozess-Handling
/// scheitert): siehe Aufrufer in `record_success`.
enum VerificationRun {
    Passed,
    Failed(String),
}

/// Führt das Prüfkommando einer `VerificationPolicy::AutomatedTests` im
/// Workspace des Laufs aus — BEWUSST ohne Shell: `std::process::Command`
/// startet das Programm direkt, das Kommando läuft nie durch `sh -c`/
/// `cmd /C`. Das Kommando kommt vom Operator (`--verify-command`/`--items`-
/// Datei), nicht vom Modell — aber selbst ein vertrauenswürdiger String wird
/// nicht interpoliert, das schließt Injection über Metazeichen strukturell
/// aus, nicht nur durch Vertrauen in die Quelle. Dafür wird es naiv an
/// Leerraum in Programm + Argumente gesplittet (keine Anführungszeichen-/
/// Escaping-Unterstützung) — für die erwarteten Fälle ('cargo test',
/// 'npm test', ein einzelnes Skript) reicht das; eine echte
/// Shell-Wort-Zerlegung wäre eine neue Abhängigkeit ohne zweiten konkreten
/// Bedarf (Guidelines §4).
///
/// Ausgabe geht in temporäre DATEIEN, nicht in `Stdio::piped()` (Befund des
/// Code-Reviews): eine Pipe verlangt, dass ein eigener Thread sie GLEICHZEITIG
/// leerliest, sonst blockiert das Kind am vollen Puffer — das allein wäre mit
/// zwei Drain-Threads noch lösbar, aber `child.try_wait()` meldet den
/// direkten Kindprozess als beendet, auch wenn ein GROSSKIND (z. B. ein vom
/// Testlauf gestarteter Hintergrundprozess) dieselben Pipe-Enden geerbt hat
/// und offen hält — dann blockiert `JoinHandle::join()` auf den Drain-Threads
/// UNBEGRENZT, obwohl der Timeout oben längst gegriffen hätte. Zwei Dateien
/// umgehen das strukturell: kein Thread liest, solange der Prozessbaum lebt.
///
/// Timeout-Mechanik EIGENSTÄNDIG, nicht über `agent_framework_rs::coding`
/// wiederverwendet: die dortige `run_with_timeout` ist eine private
/// Modulfunktion (kein Teil der öffentlichen API von agentkit) und an den
/// Abbruch-Kanal (`Cancel`) des Kern-Agenten gekoppelt, den dieses Crate hier
/// nicht braucht — ein neuer, öffentlicher Export in agentkit nur für diesen
/// einen Aufrufer wäre mehr Kopplung als der kurze, eigenständige Poll-Loop
/// hier (Rule of Three: noch kein zweiter Nutzer in diesem Crate).
fn run_verification_command(
    command: &str,
    workspace: &str,
    timeout_secs: u64,
) -> Result<VerificationRun, WorkError> {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return Ok(VerificationRun::Failed("Prüfkommando ist leer".to_string()));
    };
    let args: Vec<&str> = parts.collect();

    // Eindeutiger Dateiname je Aufruf (Prozess-ID + Zähler): mehrere Worker
    // führen ihre Prüfkommandos gleichzeitig aus, und ein Rest einer
    // vorherigen, abgebrochenen Prüfung darf nicht kollidieren.
    static VERIFY_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = VERIFY_SEQ.fetch_add(1, Ordering::Relaxed);
    let stdout_path = std::env::temp_dir().join(format!(
        "agentkit_work_verify_{}_{seq}_stdout.txt",
        std::process::id()
    ));
    let stderr_path = std::env::temp_dir().join(format!(
        "agentkit_work_verify_{}_{seq}_stderr.txt",
        std::process::id()
    ));
    let cleanup = || {
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
    };
    let open_output = |path: &std::path::Path| {
        std::fs::File::create(path).map_err(|e| {
            WorkError::Io(format!(
                "Verifikations-Ausgabedatei '{}': {e}",
                path.display()
            ))
        })
    };
    let stdout_file = open_output(&stdout_path)?;
    let stderr_file = open_output(&stderr_path)?;

    let mut cmd = Command::new(program);
    cmd.args(&args);
    cmd.current_dir(workspace);
    cmd.stdout(Stdio::from(stdout_file));
    cmd.stderr(Stdio::from(stderr_file));

    // Ein nicht startbares Kommando ist ein KONFIGURATIONSFEHLER des
    // Operators (Tippfehler, falscher Programmname), kein fachlicher
    // Fehlschlag des Versuchs — deshalb `WorkError`, nicht
    // `VerificationRun::Failed` (Befund des Code-Reviews: sonst würde jeder
    // weitere Versuch sinnlos `max_attempts` an einem Kommando verbrauchen,
    // das nie startet, während der Agent selbst korrekt arbeitet).
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            cleanup();
            return Err(WorkError::Invalid(format!(
                "Prüfkommando '{command}' nicht startbar: {e}"
            )));
        }
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    cleanup();
                    return Ok(VerificationRun::Failed(format!(
                        "Prüfkommando '{command}' nach {timeout_secs}s abgebrochen (Timeout)"
                    )));
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(20));
            }
            Err(e) => {
                cleanup();
                return Err(WorkError::Io(format!(
                    "Prüfkommando '{command}': Prozessstatus nicht abfragbar: {e}"
                )));
            }
        }
    };

    let stdout = std::fs::read(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read(&stderr_path).unwrap_or_default();
    cleanup();

    if status.success() {
        Ok(VerificationRun::Passed)
    } else {
        Ok(VerificationRun::Failed(last_meaningful_line(
            &stdout, &stderr,
        )))
    }
}

/// Die letzte nicht-leere Ausgabezeile — bevorzugt stderr, sonst stdout —,
/// gekürzt auf eine knappe Länge (`agentkit::one_line`, dasselbe Muster wie
/// die Fortschrittsanzeige der Haupt-CLI): der Grund landet im nächsten
/// Arbeitspaket (§12), er soll knapp und lesbar sein, kein ganzer Log-Dump.
fn last_meaningful_line(stdout: &[u8], stderr: &[u8]) -> String {
    let last_non_empty = |bytes: &[u8]| -> Option<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(str::to_string)
    };
    let line = last_non_empty(stderr)
        .or_else(|| last_non_empty(stdout))
        .unwrap_or_else(|| "Prüfkommando fehlgeschlagen (keine Ausgabe)".to_string());
    agentkit::one_line(&line, 200)
}

/// Journalt einen fachlich gescheiterten Versuch (`AttemptFinished` +
/// `WorkItemFailed`) und gibt das Item für einen weiteren Versuch frei, wenn
/// `max_attempts` das noch erlaubt — sonst bleibt es `Failed`, und der
/// Scheduler zählt es (und alles, was von ihm abhängt) als endgültig
/// blockiert (`state::blocked_by`).
fn record_failure(
    store: &Arc<WorkStore>,
    meta: AttemptOutcomeMeta<'_>,
    kind: FailureKind,
    message: String,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
    store.submit(WorkEvent::AttemptFinished {
        attempt: meta.attempt_id.clone(),
        status: AttemptStatus::Failed,
        summary: None,
        failure: Some(FailureInfo {
            kind,
            message: message.clone(),
        }),
        steps: meta.steps,
        tool_calls: meta.tool_calls,
        at_ms: meta.at_ms,
    })?;
    // Rollback aus §19: ein gescheiterter Versuch darf den nächsten nicht
    // vergiften — verwerfen statt aufheben, die Diagnose steht ohnehin im
    // gerade journalten `Failure` und in den bereits abgelegten Artefakten,
    // nicht im Arbeitsbaum.
    meta.git.discard()?;
    // `finish_failed_attempt` (siehe dort) ist der gemeinsame Kern mit
    // `recovery::recover_matching`s nachgeholtem Fehlschlag (Befund 2 des
    // Code-Reviews) — dieselbe "ist `max_attempts` erschöpft?"-Entscheidung
    // soll nur an einer Stelle stehen.
    let released = crate::recovery::finish_failed_attempt(
        store,
        &meta.item_id,
        &meta.attempt_id,
        |item| {
            format!(
                "Wiederholung {}/{}",
                item.attempt_count + 1,
                item.max_attempts
            )
        },
        meta.at_ms,
    )?;
    on_progress(WorkProgress::ItemDone {
        item: meta.item_id,
        ok: false,
        summary: message,
    });
    Ok(if released {
        AttemptOutcome::FailedRetryable
    } else {
        AttemptOutcome::FailedExhausted
    })
}

/// Journalt einen durch Abbruch unterbrochenen Versuch über die gemeinsame
/// [`crate::recovery::interrupt_attempt`] — dieselbe Journalfolge
/// (`AttemptFinished` `Interrupted` + `WorkItemReleased`) wie
/// `recovery::recover_matching` bei einem abgelaufenen Lease. Kein fachlicher
/// Fehlversuch, zählt NICHT gegen `max_attempts`; die Regel dafür existiert
/// dank der gemeinsamen Funktion nur noch an einer Stelle im Code.
fn record_interrupted(
    store: &Arc<WorkStore>,
    meta: AttemptOutcomeMeta<'_>,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<(), WorkError> {
    crate::recovery::interrupt_attempt(
        store,
        crate::recovery::InterruptedAttempt {
            item_id: &meta.item_id,
            attempt_id: &meta.attempt_id,
            steps: meta.steps,
            tool_calls: meta.tool_calls,
            failure_message: "Versuch durch Stop-Knopf abgebrochen".to_string(),
            release_reason: "Versuch abgebrochen — Item für einen neuen Versuch freigegeben"
                .to_string(),
            at_ms: meta.at_ms,
        },
    )?;
    // Derselbe Rollback wie bei einem regulären Fehlschlag (§19) — ein
    // unterbrochener Versuch soll den nächsten genauso wenig vergiften.
    meta.git.discard()?;
    on_progress(WorkProgress::ItemDone {
        item: meta.item_id,
        ok: false,
        summary: "abgebrochen".to_string(),
    });
    Ok(())
}
