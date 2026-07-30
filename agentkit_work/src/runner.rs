//! Die Worker-Schleife: arbeitet einen Lauf ab, bis er fertig, blockiert,
//! budgetiert oder abgebrochen ist. Kein Daemon, kein zweiter Thread für den
//! Heartbeat — der läuft im Event-Callback des laufenden Agenten (bewusste
//! Plan-Entscheidung: ein Agent, der keine Events mehr produziert, ist genau
//! der Fall, den das Lease abdecken soll).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agentkit::{AgentEvent, Cancel};

use crate::error::WorkError;
use crate::event::{WorkEvent, AUTOMATED_TESTS_BY};
use crate::executor::{AgentExecutor, AgentWorkPackage};
use crate::graph::GraphGateway;
use crate::model::{
    id_order, now_ms, ArtifactKind, AttemptId, AttemptStatus, CompletionReason, ExecutorKind,
    FailureInfo, FailureKind, RunStatus, VerificationPolicy, WorkArtifact, WorkBudget, WorkItem,
    WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::scheduler::{self, Decision};
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
    Agent(AgentEvent),
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

/// Konfiguration eines Worker-Laufs.
pub struct RunnerConfig {
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
         Folge-Items, die du hier anlegst.",
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
             anderen aufbaut."
                .to_string(),
            "Jedes neue Item hat prüfbare Akzeptanzkriterien.".to_string(),
        ],
        verification_policy: VerificationPolicy::None,
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
    let budget = snapshot
        .project
        .as_ref()
        .ok_or_else(|| WorkError::NotFound("Projekt".to_string()))?
        .budget
        .clone();
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

    loop {
        if cancel.load(Ordering::SeqCst) {
            return finish_run(
                store,
                WorkEvent::RunPaused {
                    run: run_id.to_string(),
                    reason: "Lauf durch Stop-Knopf abgebrochen".to_string(),
                    at_ms: now_ms(),
                },
                CompletionReason::Canceled,
                completed,
                failed,
                attempts_started,
            );
        }

        let snapshot = store.snapshot();
        match scheduler::decide(&snapshot, run_id, &budget, started_at_ms, now_ms()) {
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
                // Derselbe `snapshot` wie für `scheduler::decide` oben — seit
                // dessen Aufruf wurde nichts submittet, ein erneutes
                // `store.snapshot()` wäre nur ein zusätzlicher, überflüssiger
                // Read-Lock/Arc-Klon für dieselben Daten.
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
            Decision::AtCapacity => {
                // Bei genau einem synchronen Worker (max_parallel_agents = 1,
                // MVP-Fixwert) heißt AtCapacity hier: nichts ist ausführbar,
                // obwohl der Lauf laut Scheduler weder fertig noch endgültig
                // blockiert ist — jedes offene Item wartet auf ein Item, das
                // GERADE WIR hätten weiterbringen müssen. Wir sind aber nicht
                // in einem Versuch (wir stehen hier in der Entscheidungs-
                // schleife), also kann sich das nie mehr von selbst auflösen.
                // Ohne diese Bremse würde die Schleife endlos dieselbe
                // Entscheidung treffen und das CLI hinge.
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
            Decision::Run(item_id) => {
                attempts_started += 1;
                let outcome = run_attempt(store, &item_id, &budget, cfg, executor, on_progress)?;
                match outcome {
                    AttemptOutcome::Succeeded => completed.push(item_id),
                    AttemptOutcome::FailedExhausted => failed.push(item_id),
                    AttemptOutcome::FailedRetryable => {}
                    // Weder abgeschlossen noch gescheitert — der Scheduler
                    // entscheidet bei der nächsten Runde, ob andere Items noch
                    // vorankommen oder der Lauf auf die Freigabe wartet.
                    AttemptOutcome::AwaitingVerification => {}
                    AttemptOutcome::Interrupted => {
                        return finish_run(
                            store,
                            WorkEvent::RunPaused {
                                run: run_id.to_string(),
                                reason: "Versuch durch Stop-Knopf unterbrochen".to_string(),
                                at_ms: now_ms(),
                            },
                            CompletionReason::Canceled,
                            completed,
                            failed,
                            attempts_started,
                        );
                    }
                }
                let seq = store.checkpoint()?;
                on_progress(WorkProgress::Checkpoint { seq });
            }
        }
    }
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
                && it.kind.is_git_isolated()
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
        let branch = format!("work/{project_id}/{id}");
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
    store.submit(WorkEvent::AttemptFinished {
        attempt: attempt_id.clone(),
        status: AttemptStatus::Succeeded,
        summary: Some(format!("{} Item(s) gemergt.", merged_ids.len())),
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
    store.submit(WorkEvent::IntegrationMerged {
        item: integration_id.clone(),
        target_branch: target_branch.clone(),
        merged_items: merged_ids.clone(),
        at_ms,
    })?;
    on_progress(WorkProgress::ItemDone {
        item: integration_id,
        ok: true,
        summary: format!("{} Item(s) in '{target_branch}' gemergt.", merged_ids.len()),
    });
    Ok(true)
}

/// Schließt einen gescheiterten Merge-Versuch des Integrations-Items ab
/// (`AttemptFinished` mit `FailureKind::MergeConflict` + `WorkItemFailed`,
/// OHNE Freigabe — `max_attempts` ist 1, siehe [`run_integration_item`]).
/// Kein Aufruf von `recovery::finish_failed_attempt`: der bliebe hier
/// stehen, weil er bei erschöpften Versuchen ohnehin nur `WorkItemFailed`
/// journalt, aber die Wiederverwendung würde einen "Freigabegrund"-Parameter
/// verlangen, den es hier nie gibt (max_attempts == 1 heißt, es ist nach dem
/// ersten Fehlschlag immer erschöpft).
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
    store.submit(WorkEvent::WorkItemFailed {
        item: item_id.clone(),
        attempt: attempt_id.clone(),
        at_ms,
    })?;
    on_progress(WorkProgress::ItemDone {
        item: item_id.clone(),
        ok: false,
        summary: reason,
    });
    Ok(())
}

/// Git-Isolation EINES Versuchs (Phase 7, §19 — siehe `agentkit_work/README.md`
/// Abschnitt „Git-Isolation" für den vollen Zuschnitt und die Begründung,
/// warum ein Branch je Item reicht und echte Worktrees noch nicht gebraucht
/// werden). `Inactive`, wenn `WorkProject::git_isolation` aus ist oder dieses
/// Item nicht schreibt (`WorkItemKind::is_git_isolated`); sonst `Active` mit
/// allem, was [`record_success`]/[`record_failure`]/[`record_interrupted`]
/// brauchen, um den Versuch zu committen bzw. zu verwerfen.
///
/// `Drop` auf `Active` sorgt dafür, dass der Arbeitsbaum NACH dem Versuch
/// IMMER auf den Ausgangsbranch zurückwechselt — auch bei einem frühen
/// `?`-Rücksprung oder einem (kooperativ abgefangenen) Ctrl-C, das innerhalb
/// desselben Funktionsaufrufs abgefangen wird (siehe `record_interrupted`).
/// Rust kennt kein try/finally; ein Drop-Guard garantiert das an GENAU einer
/// Stelle, statt es an jedem Rückgabepfad von `run_attempt` zu wiederholen.
/// Ein ERZWUNGENES zweites Ctrl-C (`std::process::exit`, siehe README
/// „Grenzen des heutigen Stands") lässt auch dieses `Drop` nicht mehr
/// laufen — dieselbe, schon dokumentierte Einschränkung wie bei `work.lock`.
enum GitAttemptCtx {
    Inactive,
    Active(ActiveGitAttempt),
}

struct ActiveGitAttempt {
    workspace: String,
    branch: String,
    start_point: String,
    previous_branch: String,
}

impl GitAttemptCtx {
    /// Bereitet die Git-Isolation für EINEN Versuch vor. Prüft zuerst, dass
    /// der Arbeitsbaum sauber ist (fremde uncommittete Änderungen dürfen
    /// nicht in einen Item-Commit geraten) — ein harter, deutscher Fehler,
    /// kein weicher Fehlschlag: das ist ein Konfigurations-/Bedienungsfehler,
    /// kein fachlicher.
    fn prepare(
        project_git_isolation: bool,
        item: &WorkItem,
        project_id: &str,
        base_revision: Option<&str>,
        workspace: &str,
    ) -> Result<Self, WorkError> {
        if !project_git_isolation || !item.kind.is_git_isolated() {
            return Ok(GitAttemptCtx::Inactive);
        }
        match crate::git::is_clean(workspace) {
            Ok(true) => {}
            Ok(false) => {
                return Err(WorkError::Invalid(format!(
                    "Item '{}': Arbeitsbaum von '{workspace}' ist nicht sauber — Git-Isolation \
                     kann keinen Item-Branch anlegen, solange uncommittete Änderungen \
                     vorhanden sind. Committe oder verwirf sie manuell, bevor der Lauf \
                     fortgesetzt wird.",
                    item.id
                )));
            }
            Err(e) => {
                return Err(WorkError::Invalid(format!(
                    "Item '{}': Arbeitsbaum-Status nicht prüfbar — {e}",
                    item.id
                )));
            }
        }
        let Some(start_point) = base_revision else {
            return Err(WorkError::Invalid(format!(
                "Item '{}': kein Ausgangs-Commit bekannt ('base_revision' des Laufs fehlt) — \
                 Git-Isolation braucht einen bestehenden Commit als Startpunkt.",
                item.id
            )));
        };
        let previous_branch = crate::git::current_branch(workspace).map_err(|e| {
            WorkError::Invalid(format!(
                "Item '{}': aktueller Branch nicht ermittelbar — {e}",
                item.id
            ))
        })?;
        let branch = format!("work/{project_id}/{}", item.id);
        crate::git::ensure_item_branch(workspace, &branch, start_point).map_err(|e| {
            WorkError::Invalid(format!(
                "Item '{}': Branch '{branch}' nicht vorbereitbar — {e}",
                item.id
            ))
        })?;
        Ok(GitAttemptCtx::Active(ActiveGitAttempt {
            workspace: workspace.to_string(),
            branch,
            start_point: start_point.to_string(),
            previous_branch,
        }))
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
    /// Arbeitsbaum. Ein No-Op bei `Inactive`.
    fn discard(&self) -> Result<(), WorkError> {
        let GitAttemptCtx::Active(a) = self else {
            return Ok(());
        };
        crate::git::discard_changes(&a.workspace, &a.start_point).map_err(WorkError::Invalid)
    }
}

impl Drop for GitAttemptCtx {
    fn drop(&mut self) {
        if let GitAttemptCtx::Active(a) = self {
            let _ = crate::git::checkout(&a.workspace, &a.previous_branch);
        }
    }
}

/// Was jede `record_*`-Journalfunktion über den Versuch braucht — nur
/// zusammengefasst, damit die drei Funktionen nicht dieselben Parameter
/// einzeln durchreichen (sonst überschritte `record_success` allein mit Git-
/// Isolation clippys `too_many_arguments`-Schwelle). Kein eigenes Verhalten,
/// reine Bündelung. `title`/`attempt_number` werden nur für die Commit-
/// Nachricht gebraucht (`record_success`), `git` (Phase 7) außerdem für den
/// Rollback in `record_failure`/`record_interrupted` — beide Konsumenten
/// bekommen trotzdem alle Felder, um nicht drei fast identische Varianten
/// dieser Struktur zu pflegen.
struct AttemptOutcomeMeta {
    item_id: WorkItemId,
    attempt_id: AttemptId,
    steps: u32,
    tool_calls: u32,
    at_ms: u64,
    title: String,
    attempt_number: u32,
    git: GitAttemptCtx,
}

/// Führt EINEN Versuch aus: claimen, Arbeitspaket bauen, den Executor rufen
/// (der dabei Schritte/Tool-Aufrufe zählt und das Lease verlängert), und das
/// Ergebnis journalen.
fn run_attempt(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    budget: &WorkBudget,
    cfg: &RunnerConfig,
    executor: &dyn AgentExecutor,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
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
    // Items VOR diesem Versuch zählen — Grundlage für die Prüfung weiter
    // unten, ob ein Planungsversuch wirklich etwas angelegt hat.
    let items_before = before.items.len();

    let claimed_at = now_ms();
    let lease_expires_ms = claimed_at + cfg.lease_secs * 1000;
    let agent_id = cfg.agent_id.clone();
    let item_id_for_claim = item_id.clone();
    // ID-Vergabe im Schreiber-Lock (`submit_with`), nicht aus `before`
    // (Snapshot außerhalb des Locks): im MVP gibt es zwar nur genau einen
    // Worker, aber eine ID-Vergabe außerhalb des Locks ist ein Muster, das
    // niemand aus diesem Code kopieren soll (siehe `tools.rs::work_add_item`
    // für den Fall, in dem es tatsächlich parallel passiert).
    let (_, claimed_event) = store.submit_with(move |snapshot| {
        Ok(WorkEvent::WorkItemClaimed {
            item: item_id_for_claim,
            agent: agent_id,
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
    let is_planning = pkg.item.kind == WorkItemKind::Planning;
    let project_id = snapshot
        .project
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_default();
    let repository_revision = snapshot
        .runs
        .get(&pkg.item.run_id)
        .and_then(|r| r.base_revision.clone());

    // Git-Isolation (Phase 7, §19) — VOR dem eigentlichen Agentenlauf
    // vorbereitet, damit ein Fehler hier (unsauberer Arbeitsbaum, fehlender
    // Startpunkt) den Versuch gar nicht erst beginnen lässt. `git` bleibt bis
    // zum Ende dieser Funktion in Scope: sein `Drop` wechselt IMMER zurück auf
    // den Ausgangsbranch, egal welcher Pfad unten zurückkehrt.
    let git_isolation = snapshot.project.as_ref().is_some_and(|p| p.git_isolation);
    let git = GitAttemptCtx::prepare(
        git_isolation,
        &pkg.item,
        &project_id,
        repository_revision.as_deref(),
        &cfg.workspace,
    )?;

    // Artefakte liegen im Projektverzeichnis des Journals. Ohne Journal
    // (`in_memory`, z. B. Kurztests) gibt es kein solches Verzeichnis — dann
    // ein Unterverzeichnis des Workspace, damit `work_artifact` trotzdem
    // etwas Reales zum Schreiben hat, statt gegen `None` zu scheitern.
    let artifacts_dir = match store.dir() {
        Some(dir) => dir.join("artifacts"),
        None => std::path::Path::new(&cfg.workspace)
            .join(".agentkit")
            .join("work-artifacts"),
    };

    let submission_handle: Arc<Mutex<Option<WorkSubmission>>> = Arc::new(Mutex::new(None));
    let ctx = WorkToolCtx {
        run_id: pkg.item.run_id.clone(),
        work_item_id: item_id.clone(),
        attempt_id: attempt_id.clone(),
        agent_id: cfg.agent_id.clone(),
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
    let mut last_renew_ms = claimed_at;
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
            on_progress(WorkProgress::Agent(ev.clone()));
        };
        executor.execute(&pkg, ctx, store.clone(), &mut on_event)
    };

    // Ein Fehler beim Lease-Verlängern darf nicht stillschweigend verschwinden
    // — er hätte den Versuch aber auch nicht abbrechen dürfen (das Modell
    // durfte weiterlaufen). Deshalb erst NACH `execute` prüfen und melden.
    if let Some(e) = lease_error {
        return Err(e);
    }

    let meta = AttemptOutcomeMeta {
        item_id: item_id.clone(),
        attempt_id: attempt_id.clone(),
        steps,
        tool_calls,
        at_ms: now_ms(),
        title: pkg.item.title.clone(),
        attempt_number,
        git,
    };
    let outcome = match response {
        Err(msg) => record_failure(store, meta, FailureKind::ModelFailure, msg, on_progress)?,
        Ok(answer) if answer == "(abgebrochen)" => {
            record_interrupted(store, meta, on_progress)?;
            AttemptOutcome::Interrupted
        }
        // Gleiche Form, nur Ursache und Meldung unterscheiden sich — ein
        // einzeiliger `record_failure`-Aufruf je Sentinel statt zweier
        // ausgeschriebener Blöcke.
        Ok(answer) if answer == "(max_steps erreicht)" => {
            record_failure(store, meta, FailureKind::MaxSteps, answer, on_progress)?
        }
        Ok(answer) if answer == "(keine Antwort)" => {
            record_failure(store, meta, FailureKind::InvalidOutput, answer, on_progress)?
        }
        Ok(answer) => {
            let submission = submission_handle
                .lock()
                .expect("Work-Submission-Lock nicht poisoned")
                .take();
            match submission {
                Some(sub) => record_success(
                    store,
                    meta,
                    sub.summary,
                    &pkg.item.verification_policy,
                    &cfg.workspace,
                    cfg.graph.as_ref(),
                    on_progress,
                )?,
                None if !answer.trim().is_empty() => {
                    let outcome = record_success(
                        store,
                        meta,
                        answer.clone(),
                        &pkg.item.verification_policy,
                        &cfg.workspace,
                        cfg.graph.as_ref(),
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
        if items_after <= items_before {
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
                        by: cfg.agent_id.clone(),
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

    Ok(outcome)
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
    mut meta: AttemptOutcomeMeta,
    summary: String,
    policy: &VerificationPolicy,
    workspace: &str,
    gateway: Option<&Arc<dyn GraphGateway>>,
    on_progress: &mut dyn FnMut(WorkProgress),
) -> Result<AttemptOutcome, WorkError> {
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
        record_git_commit(
            store,
            &meta.item_id,
            &meta.attempt_id,
            &commit_id,
            &meta.git,
        )?;
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
            match run_verification_command(command, workspace, VERIFY_COMMAND_TIMEOUT_SECS)? {
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
    meta: AttemptOutcomeMeta,
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

/// Journalt den Commit eines git-isolierten Versuchs (Phase 7, §9/§19):
/// EIN Artefakt (`ArtifactKind::GitCommit`, `rel_path` = Item-Branch,
/// `commit_id` = die tatsächliche Commit-ID — siehe `WorkArtifact`-Doku für
/// die Begründung der abweichenden Feldbedeutung) plus EIN eigenes Ereignis
/// (`WorkEvent::GitCommitted`), nur damit `agentkit work events` eine klar
/// benannte Zeile zeigt statt der generischen `artifact_created`. Entsteht
/// NUR hier, in der Laufzeit selbst — nie über `work_artifact` (siehe
/// `tools::ARTIFACT_KINDS`, das `git_commit` bewusst nicht anbietet).
fn record_git_commit(
    store: &Arc<WorkStore>,
    item_id: &WorkItemId,
    attempt_id: &AttemptId,
    commit_id: &str,
    git: &GitAttemptCtx,
) -> Result<(), WorkError> {
    let branch = git.branch_name().unwrap_or_default().to_string();
    let commit_id_owned = commit_id.to_string();
    let (item_id_a, attempt_id_a) = (item_id.clone(), attempt_id.clone());
    let (branch_for_artifact, commit_for_artifact) = (branch.clone(), commit_id_owned.clone());
    store.submit_with(move |snap| {
        Ok(WorkEvent::ArtifactCreated {
            artifact: WorkArtifact {
                id: snap.next_artifact_id(),
                work_item_id: item_id_a,
                attempt_id: attempt_id_a,
                kind: ArtifactKind::GitCommit,
                rel_path: branch_for_artifact,
                summary: format!("Commit {commit_for_artifact}"),
                created_at_ms: now_ms(),
                commit_id: Some(commit_for_artifact),
            },
        })
    })?;
    store.submit(WorkEvent::GitCommitted {
        item: item_id.clone(),
        attempt: attempt_id.clone(),
        branch,
        commit: commit_id_owned,
        at_ms: now_ms(),
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

    // Eindeutiger Dateiname je Aufruf (Prozess-ID + Zähler): das MVP hat zwar
    // nur einen Worker, der nie zwei Prüfkommandos gleichzeitig ausführt,
    // aber ein aufgeräumter Name verhindert trotzdem jede Kollision mit einem
    // Rest einer vorherigen, abgebrochenen Prüfung.
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
    meta: AttemptOutcomeMeta,
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
    meta: AttemptOutcomeMeta,
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
