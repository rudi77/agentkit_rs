//! Die Worker-Schleife: arbeitet einen Lauf ab, bis er fertig, blockiert,
//! budgetiert oder abgebrochen ist. Kein Daemon, kein zweiter Thread für den
//! Heartbeat — der läuft im Event-Callback des laufenden Agenten (bewusste
//! Plan-Entscheidung: ein Agent, der keine Events mehr produziert, ist genau
//! der Fall, den das Lease abdecken soll).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agentkit::{AgentEvent, Cancel};

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::executor::{AgentExecutor, AgentWorkPackage};
use crate::graph::GraphGateway;
use crate::model::{
    now_ms, AttemptId, AttemptStatus, CompletionReason, FailureInfo, FailureKind, RunStatus,
    WorkBudget, WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::scheduler::{self, Decision};
use crate::store::WorkStore;
use crate::tools::{WorkSubmission, WorkToolCtx};

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

/// Was jede `record_*`-Journalfunktion über den Versuch braucht — nur
/// zusammengefasst, damit die drei Funktionen nicht dieselben fünf Parameter
/// einzeln durchreichen. Kein eigenes Verhalten, reine Bündelung.
struct AttemptOutcomeMeta {
    item_id: WorkItemId,
    attempt_id: AttemptId,
    steps: u32,
    tool_calls: u32,
    at_ms: u64,
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
                Some(sub) => record_success(store, meta, sub.summary, on_progress)?,
                None if !answer.trim().is_empty() => {
                    let outcome = record_success(store, meta, answer.clone(), on_progress)?;
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

    Ok(outcome)
}

/// Journalt einen erfolgreichen Versuch (`AttemptFinished` + `WorkItemCompleted`).
fn record_success(
    store: &Arc<WorkStore>,
    meta: AttemptOutcomeMeta,
    summary: String,
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
    store.submit(WorkEvent::WorkItemCompleted {
        item: meta.item_id.clone(),
        attempt: meta.attempt_id,
        at_ms: meta.at_ms,
    })?;
    on_progress(WorkProgress::ItemDone {
        item: meta.item_id,
        ok: true,
        summary,
    });
    Ok(AttemptOutcome::Succeeded)
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
    on_progress(WorkProgress::ItemDone {
        item: meta.item_id,
        ok: false,
        summary: "abgebrochen".to_string(),
    });
    Ok(())
}
