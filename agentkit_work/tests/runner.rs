//! Der Runner als Spezifikation: was ein voller Lauf tut, was ein Fehlversuch
//! ändert, was ein Abbruch sauber hinterlässt und was ein Neustart NICHT
//! wiederholt. Teststil wie die vorhandenen Tests (deutsche Satznamen, ein
//! Verhalten pro Test).
//!
//! `ScriptedExecutor` ist der Test-Doppelgänger aus dem Plan: eine
//! vorgegebene Folge von Schritten, jeder Schritt eine Closure, die optional
//! über die mitgegebene `WorkToolCtx`/`store` echte Tool-Wirkung erzeugt
//! (Items anlegen, `work_submit` füllen) und `on_event` mit echten
//! `AgentEvent`s füttert.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::testing::FakeLlm;
use agentkit::{new_cancel, AgentEvent, Chunk, EventData, ToolRegistry};
use agentkit_work::{
    ensure_plan_item, id_order, now_ms, recovery, register_work_tools, run_to_completion,
    AgentExecutor, AgentWorkPackage, ClaimText, CodingAgentExecutor, CompletionReason,
    GraphGateway, ProjectStatus, RunStatus, RunnerConfig, WorkBudget, WorkEvent, WorkItem,
    WorkItemKind, WorkItemStatus, WorkProgress, WorkProject, WorkProvenance, WorkRun, WorkStore,
    WorkSubmission, WorkToolCtx,
};
use serde_json::json;

// ------------------------------------------------------------------ Helfer

fn tmp_dir(name: &str) -> std::path::PathBuf {
    static NR: AtomicUsize = AtomicUsize::new(0);
    let nr = NR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_runner_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn project(budget: WorkBudget) -> WorkProject {
    WorkProject {
        id: "demo".into(),
        title: "Demo".into(),
        objective: "Teste den Runner.".into(),
        workspace: ".".into(),
        status: ProjectStatus::Active,
        created_at_ms: 0,
        budget,
    }
}

fn run(id: &str) -> WorkRun {
    WorkRun {
        id: id.into(),
        project_id: "demo".into(),
        status: RunStatus::Running,
        started_at_ms: 0,
        completed_at_ms: None,
        base_revision: None,
        completion_reason: None,
    }
}

fn item(id: &str, seq: u64, deps: Vec<&str>, max_attempts: u32) -> WorkItem {
    WorkItem {
        id: id.into(),
        run_id: "R-1".into(),
        title: format!("Item {id}"),
        description: "Beschreibung".into(),
        kind: WorkItemKind::Implementation,
        status: WorkItemStatus::Pending,
        priority: 5,
        seq,
        required_role: None,
        dependencies: deps.into_iter().map(String::from).collect(),
        acceptance_criteria: vec![],
        verification_policy: agentkit_work::VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        executor: agentkit_work::ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts,
        updated_at_ms: 0,
    }
}

/// Wie `item`, aber mit einer expliziten `VerificationPolicy` (Phase 5a) —
/// eigene Hilfsfunktion statt `item()`s Signatur zu ändern.
fn item_with_policy(
    id: &str,
    seq: u64,
    deps: Vec<&str>,
    max_attempts: u32,
    policy: agentkit_work::VerificationPolicy,
) -> WorkItem {
    let mut it = item(id, seq, deps, max_attempts);
    it.verification_policy = policy;
    it
}

fn setup(store: &WorkStore, budget: WorkBudget) {
    store
        .submit(WorkEvent::ProjectCreated {
            project: project(budget),
        })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
}

fn cfg(workspace: &std::path::Path) -> RunnerConfig {
    RunnerConfig {
        agent_id: "worker-1".to_string(),
        lease_secs: 600,
        heartbeat_secs: 30,
        workspace: workspace.to_string_lossy().to_string(),
        graph: None,
    }
}

// ------------------------------------------------------- ScriptedExecutor

type Step = Box<
    dyn Fn(
            &AgentWorkPackage,
            WorkToolCtx,
            Arc<WorkStore>,
            &mut dyn FnMut(&AgentEvent),
        ) -> Result<String, String>
        + Send
        + Sync,
>;

/// Test-Doppelgänger für [`AgentExecutor`]: spielt eine vorgegebene Folge von
/// Schritten ab, einen je `execute`-Aufruf. Läuft das Skript leer, kommt eine
/// leere Antwort zurück (kein Panic) — das macht Fehlkonfigurationen in
/// Tests sichtbar (die Klassifikation behandelt das als `InvalidOutput`),
/// statt sie zu verschlucken.
struct ScriptedExecutor {
    steps: Mutex<VecDeque<Step>>,
    calls: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(steps: Vec<Step>) -> Self {
        ScriptedExecutor {
            steps: Mutex::new(steps.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl AgentExecutor for ScriptedExecutor {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.steps.lock().unwrap().pop_front();
        match next {
            Some(f) => f(pkg, ctx, store, on_event),
            None => Ok(String::new()),
        }
    }
}

fn step_event(n: usize) -> AgentEvent {
    AgentEvent::new(agentkit::STEP, EventData::Step { step: n })
}

/// Legt `items` (Titel, Beschreibung) im laufenden Vorhaben an — simuliert
/// den Planungszug, der mehrfach `work_add_item` aufruft — und schließt mit
/// `work_submit`.
fn planning_step(items: &'static [(&'static str, &'static str)]) -> Step {
    Box::new(move |pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        for (title, description) in items {
            let snapshot = store.snapshot();
            let id = snapshot.next_item_id();
            let new_item = WorkItem {
                id: id.clone(),
                run_id: pkg.item.run_id.clone(),
                title: title.to_string(),
                description: description.to_string(),
                kind: WorkItemKind::Implementation,
                status: WorkItemStatus::Pending,
                priority: 5,
                seq: id_order(&id),
                required_role: None,
                dependencies: vec![],
                acceptance_criteria: vec![],
                verification_policy: agentkit_work::VerificationPolicy::None,
                verifies: None,
                claims_promoted: false,
                executor: agentkit_work::ExecutorKind::SingleAgent,
                attempt_count: 0,
                max_attempts: ctx.max_attempts,
                updated_at_ms: now_ms(),
            };
            store
                .submit(WorkEvent::WorkItemCreated { item: new_item })
                .unwrap();
        }
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "Vorhaben zerlegt.".to_string(),
            criteria: vec![],
        });
        on_event(&step_event(2));
        Ok("Vorhaben zerlegt.".to_string())
    })
}

/// Ruft `work_submit` mit `summary` auf und beendet den Versuch erfolgreich.
fn succeed(summary: &'static str) -> Step {
    Box::new(move |_pkg, ctx, _store, on_event| {
        on_event(&step_event(1));
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: summary.to_string(),
            criteria: vec![],
        });
        Ok(summary.to_string())
    })
}

/// Schließt erfolgreich ab, OHNE `work_submit` aufzurufen — die Antwort
/// selbst wird zur Summary.
fn succeed_without_submit(answer: &'static str) -> Step {
    Box::new(move |_pkg, _ctx, _store, on_event| {
        on_event(&step_event(1));
        Ok(answer.to_string())
    })
}

/// Der Executor kam gar nicht zustande (API-/Aufbaufehler).
fn fail_model(msg: &'static str) -> Step {
    Box::new(move |_pkg, _ctx, _store, on_event| {
        on_event(&step_event(1));
        Err(msg.to_string())
    })
}

/// Der Agent ist fertig geworden, die Antwort ist aber einer der bindenden
/// Sentinel-Strings des Kerns (`agent.rs`).
fn sentinel(text: &'static str) -> Step {
    Box::new(move |_pkg, _ctx, _store, on_event| {
        on_event(&step_event(1));
        Ok(text.to_string())
    })
}

/// Legt ein Artefakt über die ECHTE Tool-Registry ab (`work_artifact`) und
/// schließt danach mit `work_submit` ab — für Tests der Phase 5b, die eine
/// Artefaktangabe im Prüf-Item-Auftragstext erwarten.
fn succeed_with_artifact(summary: &'static str, filename: &'static str) -> Step {
    Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_artifact",
                json!({
                    "kind": "analysis",
                    "filename": filename,
                    "content": "Ergebnis der Analyse.",
                    "summary": "Analyse-Ergebnis"
                }),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: summary.to_string(),
            criteria: vec![],
        });
        Ok(summary.to_string())
    })
}

/// Ruft `work_verdict` über die ECHTE Tool-Registry auf (simuliert den
/// Prüf-Agenten eines automatisch angelegten Prüf-Items, Phase 5b) und
/// schließt danach mit `work_submit` ab.
fn verdict(approved: bool, reason: &'static str) -> Step {
    Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_verdict",
                json!({"approved": approved, "reason": reason}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "Urteil erfasst.".to_string(),
            criteria: vec![],
        });
        Ok("Urteil erfasst.".to_string())
    })
}

/// Ruft `work_submit` auf, OHNE je `work_verdict` gerufen zu haben — simuliert
/// ein Prüf-Item, das seinen Versuch abschließt, ohne ein Urteil abzugeben.
fn succeed_without_verdict(summary: &'static str) -> Step {
    Box::new(move |_pkg, ctx, _store, on_event| {
        on_event(&step_event(1));
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: summary.to_string(),
            criteria: vec![],
        });
        Ok(summary.to_string())
    })
}

/// Test-Doppelgänger für [`GraphGateway`] (Duplikat-Begründung siehe
/// `tests/graph.rs`/`tests/tools.rs`): zeichnet `promote`-Aufrufe auf, kann
/// optional fehlschlagen.
#[derive(Default)]
struct FakeGraph {
    promoted: Mutex<Vec<String>>,
    fail: bool,
}

impl GraphGateway for FakeGraph {
    fn recall(&self, _query: &str) -> Option<String> {
        None
    }

    fn record_claims(
        &self,
        _prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        Ok((1..=claims.len()).map(|n| format!("C-{n}")).collect())
    }

    fn promote(&self, claim_ids: &[String]) -> Result<usize, String> {
        if self.fail {
            return Err("Graph nicht erreichbar (simuliert)".to_string());
        }
        self.promoted
            .lock()
            .unwrap()
            .extend(claim_ids.iter().cloned());
        Ok(claim_ids.len())
    }
}

// ------------------------------------------------------------ ensure_plan_item

#[test]
fn ensure_plan_item_legt_genau_ein_planungs_item_an_und_beim_zweiten_mal_keines_mehr() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());

    let id = ensure_plan_item(&store, "R-1").unwrap();
    assert!(id.is_some());
    let snapshot = store.snapshot();
    assert_eq!(snapshot.items.len(), 1);
    let plan_item = snapshot.items.values().next().unwrap();
    assert_eq!(plan_item.kind, WorkItemKind::Planning);
    assert_eq!(plan_item.title, "Vorhaben zerlegen");

    let second = ensure_plan_item(&store, "R-1").unwrap();
    assert_eq!(second, None);
    assert_eq!(
        store.snapshot().items.len(),
        1,
        "kein zweites Item entstanden"
    );
}

// -------------------------------------------------------------- run_to_completion

#[test]
fn vollstaendiger_lauf_erledigt_planung_und_beide_folge_items() {
    let ws = tmp_dir("voll");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());

    let executor = ScriptedExecutor::new(vec![
        planning_step(&[
            ("A umsetzen", "Erstes Teilstück."),
            ("B umsetzen", "Zweites Teilstück."),
        ]),
        succeed("A erledigt."),
        succeed("B erledigt."),
    ]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(outcome.attempts, 3);
    assert_eq!(outcome.completed.len(), 3);
    assert!(outcome.failed.is_empty());

    let state = store.snapshot();
    assert_eq!(state.items.len(), 3);
    assert!(state
        .items
        .values()
        .all(|it| it.status == WorkItemStatus::Completed));

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn planungs_item_ohne_neue_items_meldet_einen_hinweis_und_gilt_als_blockiert() {
    // Regressionstest zu Befund 2 (Handprobe): vor der Korrektur endete dieser
    // Lauf mit `CompletionReason::AllItemsDone` — eine falsche Erfolgsmeldung
    // (Exit 0 in der CLI), obwohl die Zerlegung nichts erzeugt hat und das
    // Vorhaben nachweislich unbearbeitet blieb. Der `Note`-Hinweis allein
    // reicht nicht, weil ein Skript nur den Exit-Code sieht.
    let ws = tmp_dir("leere_planung");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());

    // Der Planungszug schließt erfolgreich ab, legt aber KEIN einziges neues
    // Item an — der Lauf darf das nicht stillschweigend als "erledigt"
    // durchgehen lassen, ohne den Nutzer zu warnen.
    let executor = ScriptedExecutor::new(vec![succeed("Nichts zu tun gefunden.")]);
    let cancel = new_cancel();
    let mut notes = Vec::new();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |ev| {
        if let WorkProgress::Note(msg) = ev {
            notes.push(msg);
        }
    })
    .unwrap();

    assert_eq!(outcome.reason, CompletionReason::Blocked);
    assert_eq!(
        store.snapshot().items.len(),
        1,
        "nur das Planungs-Item selbst existiert"
    );
    assert!(
        notes.iter().any(|n| n.contains("kein einziges Work Item")),
        "{notes:?}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn fehlversuch_wird_wiederholt_und_das_zweite_mal_erfolgreich() {
    let ws = tmp_dir("retry");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![fail_model("boom"), succeed("passt jetzt")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(outcome.attempts, 2);

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(
        state.items["W-1"].attempt_count, 1,
        "genau ein Fehlversuch zählte"
    );

    let mut attempts: Vec<_> = state
        .attempts
        .values()
        .filter(|a| a.work_item_id == "W-1")
        .collect();
    attempts.sort_by_key(|a| id_order(&a.id));
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].status, agentkit_work::AttemptStatus::Failed);
    assert_eq!(attempts[1].status, agentkit_work::AttemptStatus::Succeeded);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn erschoepfte_max_attempts_blockiert_den_abhaengigen_nachfolger() {
    let ws = tmp_dir("erschoepft");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 2),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2, vec!["W-1"], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![fail_model("f1"), fail_model("f2")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::Blocked);
    assert_eq!(outcome.attempts, 2);
    assert_eq!(outcome.failed, vec!["W-1".to_string()]);
    assert!(outcome.completed.is_empty());

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-2"].status, WorkItemStatus::Pending);
    assert_eq!(state.runs["R-1"].status, RunStatus::Completed);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn abbruch_vor_dem_naechsten_item_pausiert_sauber_ohne_haengendes_item() {
    let ws = tmp_dir("abbruch");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2, vec![], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed("W-1 fertig.")]);
    let cancel = new_cancel();
    let cancel_setter = cancel.clone();
    let mut saw_first_done = false;
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |ev| {
        if let WorkProgress::ItemDone { .. } = ev {
            if !saw_first_done {
                saw_first_done = true;
                cancel_setter.store(true, Ordering::SeqCst);
            }
        }
    })
    .unwrap();

    assert_eq!(outcome.reason, CompletionReason::Canceled);
    assert_eq!(outcome.attempts, 1);

    let state = store.snapshot();
    assert_eq!(state.runs["R-1"].status, RunStatus::Paused);
    assert!(
        state
            .items
            .values()
            .all(|it| it.status != WorkItemStatus::Running),
        "kein Item darf nach einem sauberen Abbruch Running bleiben"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn abgebrochene_antwort_setzt_interrupted_und_pausiert_ohne_attempt_zu_verbrauchen() {
    let ws = tmp_dir("interrupted");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![sentinel("(abgebrochen)")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::Canceled);
    let state = store.snapshot();
    assert_eq!(state.runs["R-1"].status, RunStatus::Paused);
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(
        state.items["W-1"].attempt_count, 0,
        "ein unterbrochener Versuch zählt nicht gegen max_attempts"
    );
    let attempt = state
        .attempts
        .values()
        .find(|a| a.work_item_id == "W-1")
        .unwrap();
    assert_eq!(attempt.status, agentkit_work::AttemptStatus::Interrupted);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn fehlendes_work_submit_gilt_trotzdem_als_erfolg_mit_hinweis() {
    let ws = tmp_dir("kein_submit");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed_without_submit(
        "Alles erledigt, aber ohne work_submit.",
    )]);
    let cancel = new_cancel();
    let mut notes = Vec::new();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |ev| {
        if let WorkProgress::Note(msg) = ev {
            notes.push(msg);
        }
    })
    .unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    let attempt = state
        .attempts
        .values()
        .find(|a| a.work_item_id == "W-1")
        .unwrap();
    assert_eq!(
        attempt.summary.as_deref(),
        Some("Alles erledigt, aber ohne work_submit.")
    );
    assert_eq!(
        notes.len(),
        1,
        "genau ein Hinweis auf fehlenden work_submit"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn heartbeat_secs_0_verlaengert_bei_jedem_ereignis_das_lease() {
    let ws = tmp_dir("heartbeat");
    let store_dir = ws.join(".agentkit").join("work").join("demo");
    let store = Arc::new(WorkStore::open(&store_dir).unwrap());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    // `run_to_completion` kompaktiert das Journal direkt nach diesem einen
    // Versuch (`store.checkpoint()` im `Run`-Zweig) — eine einzelne
    // `lease_renewed`-Zeile wäre danach schon wieder verschwunden. Deshalb
    // liest der Schritt das Journal MITTEN im Versuch, vor dem Abschluss.
    let captured_journal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured = captured_journal.clone();
    let journal_path = store_dir.join("work.jsonl");
    let step: Step = Box::new(move |_pkg, ctx, _store, on_event| {
        on_event(&step_event(1));
        on_event(&step_event(2));
        *captured.lock().unwrap() = std::fs::read_to_string(&journal_path).ok();
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "fertig".to_string(),
            criteria: vec![],
        });
        Ok("fertig".to_string())
    });
    let executor = ScriptedExecutor::new(vec![step]);

    let mut runner_cfg = cfg(&ws);
    runner_cfg.heartbeat_secs = 0;
    let cancel = new_cancel();
    run_to_completion(&store, "R-1", &executor, &runner_cfg, &cancel, &mut |_| {}).unwrap();

    let journal = captured_journal
        .lock()
        .unwrap()
        .clone()
        .expect("Journal wurde während des Versuchs gelesen");
    assert!(
        journal.contains("\"kind\":\"lease_renewed\""),
        "Journal enthält keine Lease-Verlängerung:\n{journal}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn neustart_mitten_im_lauf_wiederholt_das_erste_item_nicht() {
    let ws = tmp_dir("neustart");
    let store_dir = ws.join(".agentkit").join("work").join("demo");

    // Ein einziger Doppelgänger über beide "Prozesse" hinweg — seine
    // Aufrufzahl beweist, dass das erste Item im zweiten Lauf NICHT erneut
    // ausgeführt wird.
    let executor = ScriptedExecutor::new(vec![succeed("W-1 fertig."), succeed("W-2 fertig.")]);

    {
        let store = Arc::new(WorkStore::open(&store_dir).unwrap());
        setup(&store, WorkBudget::default());
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item("W-1", 1, vec![], 3),
            })
            .unwrap();
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item("W-2", 2, vec![], 3),
            })
            .unwrap();

        let cancel = new_cancel();
        let cancel_setter = cancel.clone();
        let mut saw_first_done = false;
        let outcome = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |ev| {
            if let WorkProgress::ItemDone { .. } = ev {
                if !saw_first_done {
                    saw_first_done = true;
                    cancel_setter.store(true, Ordering::SeqCst);
                }
            }
        })
        .unwrap();
        assert_eq!(outcome.reason, CompletionReason::Canceled);
        // Store fällt hier aus dem Scope — simuliert den Neustart.
    }

    let reopened = Arc::new(WorkStore::open(&store_dir).unwrap());
    // Pflicht laut Doku: der Aufrufer erholt sich, BEVOR er weiterläuft.
    recovery::recover_all(&reopened, now_ms()).unwrap();

    let cancel2 = new_cancel();
    let outcome2 = run_to_completion(
        &reopened,
        "R-1",
        &executor,
        &cfg(&ws),
        &cancel2,
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome2.reason, CompletionReason::AllItemsDone);
    assert_eq!(outcome2.completed, vec!["W-2".to_string()]);
    assert_eq!(
        executor.calls(),
        2,
        "insgesamt genau zwei Versuche über beide Läufe"
    );

    let state = reopened.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(state.items["W-2"].status, WorkItemStatus::Completed);

    std::fs::remove_dir_all(&ws).ok();
}

/// `runner::record_interrupted` (Stop-Knopf) und `recovery::recover_matching`
/// (abgelaufenes Lease) journalen seit dem Refactoring beide über die
/// gemeinsame `recovery::interrupt_attempt` — dieser Test belegt, dass beide
/// Wege in den relevanten Feldern denselben Zustand hinterlassen, statt zwei
/// eigenständig gepflegte Kopien derselben Regel zu vergleichen.
#[test]
fn abgebrochener_versuch_und_lease_recovery_hinterlassen_denselben_zustand() {
    let ws = tmp_dir("interrupt_gleichheit");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2, vec![], 3),
        })
        .unwrap();

    // W-1: Stop-Knopf mitten im Versuch (`runner::record_interrupted`).
    let executor = ScriptedExecutor::new(vec![sentinel("(abgebrochen)")]);
    let cancel = new_cancel();
    run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    // W-2: claimt, dann läuft das Lease ab, ohne dass je ein Versuch endet —
    // `recovery::recover` räumt das über den Lease-Ablauf-Pfad auf.
    store
        .submit(WorkEvent::WorkItemClaimed {
            item: "W-2".into(),
            agent: "worker-1".into(),
            attempt: "A-2".into(),
            lease_expires_ms: 1_000,
            at_ms: 0,
        })
        .unwrap();
    recovery::recover(&store, 5_000).unwrap();

    let state = store.snapshot();
    let item1 = &state.items["W-1"];
    let item2 = &state.items["W-2"];
    assert_eq!(item1.status, WorkItemStatus::Pending);
    assert_eq!(item1.status, item2.status);
    assert_eq!(
        item1.attempt_count, 0,
        "ein unterbrochener Versuch zählt nicht gegen max_attempts"
    );
    assert_eq!(item1.attempt_count, item2.attempt_count);

    let attempt1 = state
        .attempts
        .values()
        .find(|a| a.work_item_id == "W-1")
        .unwrap();
    let attempt2 = &state.attempts["A-2"];
    assert_eq!(attempt1.status, agentkit_work::AttemptStatus::Interrupted);
    assert_eq!(attempt1.status, attempt2.status);
    assert_eq!(
        attempt1.failure.as_ref().unwrap().kind,
        agentkit_work::FailureKind::Interrupted
    );
    assert_eq!(
        attempt1.failure.as_ref().unwrap().kind,
        attempt2.failure.as_ref().unwrap().kind
    );
    assert!(attempt1.finished_at_ms.is_some());
    assert!(attempt2.finished_at_ms.is_some());

    std::fs::remove_dir_all(&ws).ok();
}

/// Belegt, dass das Arbeitspaket des Nachfolgers weiterhin einen ECHTEN,
/// auf der Platte lesbaren Pfad zum Vorgänger-Artefakt nennt — auch nach der
/// Pfadkorrektur (Versuch statt nur Item im Pfad). Der Vorgänger legt sein
/// Artefakt über die echte `work_artifact`-Registry ab (kein handgestrickter
/// `rel_path`), das Arbeitspaket des Nachfolgers wird über den echten
/// `AgentWorkPackage::build`-Pfad gebaut (via `run_to_completion`), und am
/// Ende wird die Datei unter `workspace.join(rel_path)` TATSÄCHLICH gelesen.
///
/// Workspace UND Store-Verzeichnis sind hier bewusst dasselbe Verzeichnis
/// (wie in `tests/tools.rs`s `registry()`-Helfer): `rel_path` ist relativ zu
/// `ctx.artifacts_dir`s Elternverzeichnis (`artifacts/…`, siehe `tools.rs`),
/// und `ctx.artifacts_dir` ist `store.dir().join("artifacts")` — das
/// Auseinanderhalten von "wo der Store liegt" und "wo der Agent-Workspace
/// liegt" ist eine separate CLI-Verdrahtungsfrage (`project.workspace` vs.
/// `--dir`), nicht Gegenstand dieser Pfadkorrektur.
#[test]
fn nachfolger_bekommt_vorgaenger_artefakt_als_tatsaechlich_lesbaren_pfad() {
    let ws = tmp_dir("vorgaenger_pfad");
    let store = Arc::new(WorkStore::open(&ws).unwrap());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2, vec!["W-1"], 3),
        })
        .unwrap();

    let captured_rel_path: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured = captured_rel_path.clone();

    // W-1: legt das Artefakt über die echte Tool-Registry ab.
    let artifact_step: Step = Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_artifact",
                json!({
                    "kind": "analysis",
                    "filename": "befund.md",
                    "content": "Ursache gefunden.",
                    "summary": "Analyse der Ursache"
                }),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "W-1 fertig.".to_string(),
            criteria: vec![],
        });
        Ok("W-1 fertig.".to_string())
    });

    // W-2: liest sein Arbeitspaket und merkt sich den genannten Pfad des
    // Vorgänger-Artefakts, statt ihn zu erraten.
    let read_step: Step = Box::new(move |pkg, ctx, _store, on_event| {
        on_event(&step_event(1));
        assert_eq!(
            pkg.predecessor_artifacts.len(),
            1,
            "{:?}",
            pkg.predecessor_artifacts
        );
        let (item_id, path, _summary) = &pkg.predecessor_artifacts[0];
        assert_eq!(item_id, "W-1");
        *captured.lock().unwrap() = Some(path.clone());
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "W-2 fertig.".to_string(),
            criteria: vec![],
        });
        Ok("W-2 fertig.".to_string())
    });

    let executor = ScriptedExecutor::new(vec![artifact_step, read_step]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();
    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);

    let rel_path = captured_rel_path
        .lock()
        .unwrap()
        .clone()
        .expect("W-2 hat den Pfad des Vorgänger-Artefakts gesehen");
    // Versuch steckt mit im Pfad (Regressionskorrektur) — nicht nur
    // "artifacts/W-1/befund.md".
    assert!(
        rel_path.starts_with("artifacts/W-1/A-") && rel_path.ends_with("/befund.md"),
        "erwarte artifacts/W-1/<attempt>/befund.md: {rel_path}"
    );
    assert!(!rel_path.contains('\\'));

    // Der Pfad ist workspace-relativ (siehe `WorkToolCtx::artifacts_dir`-Doku)
    // — `read_file` fände die Datei genau hier. Wir lesen sie hier direkt,
    // um den Pfad wirklich auf der Platte zu prüfen statt nur den String.
    let full_path = ws.join(&rel_path);
    assert_eq!(
        std::fs::read_to_string(&full_path)
            .expect("Artefakt-Datei unter dem genannten Pfad lesbar"),
        "Ursache gefunden."
    );

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------------------- E2E

/// Ende-zu-Ende mit einem echten agentkit-Agenten (`CodingAgentExecutor`) und
/// `FakeLlm`: das Modell ruft `work_add_item`, `work_artifact` und
/// `work_submit` wirklich über die Registry auf. `approve` lehnt alles ab,
/// damit kein `run_shell` startet — braucht kein Netz.
#[test]
fn e2e_mit_codingagentexecutor_und_fakellm_legt_das_artefakt_wirklich_an() {
    let ws = tmp_dir("e2e");
    let store_dir = ws.join(".agentkit").join("work").join("demo");
    let store = Arc::new(WorkStore::open(&store_dir).unwrap());
    setup(&store, WorkBudget::default());

    // Turn 1 (Planungs-Item, Schritt 1): work_add_item.
    // Turn 2 (Planungs-Item, Schritt 2, nach dem Tool-Ergebnis): fertig, ohne
    //         work_submit — zählt als Erfolg mit Hinweis (siehe Runner-Klassifikation).
    // Turn 3 (Umsetzungs-Item, Schritt 1): work_artifact.
    // Turn 4 (Umsetzungs-Item, Schritt 2): work_submit.
    // Turn 5 (Umsetzungs-Item, Schritt 3): fertig.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "work_add_item",
            &json!({
                "title": "Ergebnis ablegen",
                "description": "Lege das Ergebnis als Artefakt ab.",
                "kind": "implementation"
            })
            .to_string(),
        )],
        vec![Chunk::text("Vorhaben in ein Teilitem zerlegt.")],
        vec![Chunk::tool(
            0,
            "c2",
            "work_artifact",
            &json!({
                "kind": "code",
                "filename": "ergebnis.txt",
                "content": "fertig!",
                "summary": "Ergebnis abgelegt"
            })
            .to_string(),
        )],
        vec![Chunk::tool(
            0,
            "c3",
            "work_submit",
            &json!({"summary": "Datei geschrieben.", "criteria": []}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ]));

    let executor = CodingAgentExecutor {
        llm: llm.clone(),
        approve: Arc::new(|_: &str| false),
        extra_tools: None,
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
        system_extra: None,
    };

    let cancel = new_cancel();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {})
        .unwrap_or_else(|e| panic!("Lauf gescheitert: {e}"));

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);

    let state = store.snapshot();
    let impl_item = state
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Implementation)
        .expect("Umsetzungs-Item wurde von work_add_item angelegt");
    assert_eq!(impl_item.status, WorkItemStatus::Completed);

    // `rel_path` trägt seit der Pfadkorrektur den Versuch mit
    // (`artifacts/<item>/<attempt>/<datei>`), dessen ID hier nicht vorab
    // bekannt ist — deshalb über den journalten Artefakt-Datensatz auflösen
    // statt den Pfad zu erraten.
    let artifact = state
        .artifacts
        .values()
        .find(|a| a.work_item_id == impl_item.id)
        .expect("work_artifact hat ein Artefakt journalt");
    let artifact_path = store_dir.join(&artifact.rel_path);
    assert_eq!(
        std::fs::read_to_string(&artifact_path).expect("Artefakt-Datei existiert"),
        "fertig!"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------------- Phase 5a: Verifikation
//
// Testkommando `cargo --version`/`cargo <bogus>`: `cargo` ist auf JEDER
// Maschine vorhanden, auf der `cargo test` überhaupt läuft — plattform-
// unabhängig (Windows/Linux/musl) und ohne Netzzugriff. `cargo --version`
// liefert deterministisch Exit 0; ein erfundenes Unterkommando liefert
// deterministisch einen Fehler (Cargo löst Unterkommandos rein lokal auf,
// kein Netz nötig) — beides ohne eine einzige neue Dependency oder ein
// eigens für den Test angelegtes Skript.

#[test]
fn policy_none_verhaelt_sich_exakt_wie_vor_phase_5a() {
    // Regressionsschutz: Standard-Policy `None` ändert nichts am bisherigen
    // Ablauf — direkt `Completed`, kein `verification`-Ausgang am Versuch.
    let ws = tmp_dir("policy_none");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed("fertig")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(
        state.attempts["A-1"].verification, None,
        "ohne Verifikationspolicy bleibt das Feld leer, wie vor Phase 5a"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn automated_tests_mit_erfolgreichem_kommando_schliesst_item_ab_und_journalt_verification_approved()
{
    let ws = tmp_dir("verify_ok");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                vec![],
                3,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "cargo --version".to_string(),
                },
            ),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed("Alles fertig.")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(
        state.attempts["A-1"].verification,
        Some(agentkit_work::AttemptVerification::Approved {
            by: "automated_tests".to_string(),
            reason: None,
        })
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn automated_tests_abgelehntes_kommando_erhoeht_attempt_count_setzt_pending_und_der_grund_erscheint_im_naechsten_arbeitspaket(
) {
    let ws = tmp_dir("verify_reject");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                vec![],
                3,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "cargo this-subcommand-does-not-exist-xyz".to_string(),
                },
            ),
        })
        .unwrap();

    // Der ZWEITE Anlauf (nach der ersten Ablehnung) beobachtet sein eigenes
    // Arbeitspaket, statt nur den Endzustand des Laufs zu prüfen — genau dort
    // muss der Ablehnungsgrund laut §12 auftauchen.
    let previous_failures_count: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let mentions_verification: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let count_handle = previous_failures_count.clone();
    let mentions_handle = mentions_verification.clone();
    let second_step: Step = Box::new(move |pkg, ctx, _store, on_event| {
        on_event(&step_event(1));
        *count_handle.lock().unwrap() = Some(pkg.previous_failures.len());
        *mentions_handle.lock().unwrap() = pkg.render().contains("Verifikation abgelehnt");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "zweiter Versuch".to_string(),
            criteria: vec![],
        });
        Ok("zweiter Versuch".to_string())
    });
    let executor = ScriptedExecutor::new(vec![succeed("erster Versuch"), second_step]);
    let cancel = new_cancel();
    // Ergebnis des Gesamtlaufs ist hier nicht der Punkt (das Skript ist nach
    // zwei Einträgen erschöpft, der dritte Anlauf scheitert dann als
    // `InvalidOutput`) — beobachtet wird nur der zweite Anlauf.
    let _ = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    assert_eq!(
        *previous_failures_count.lock().unwrap(),
        Some(1),
        "genau eine vorherige Fehlerursache beim zweiten Anlauf"
    );
    assert!(
        *mentions_verification.lock().unwrap(),
        "das Arbeitspaket des zweiten Anlaufs muss die abgelehnte Verifikation nennen"
    );

    let state = store.snapshot();
    match &state.attempts["A-1"].verification {
        Some(agentkit_work::AttemptVerification::Rejected { by, reason }) => {
            assert_eq!(by, "automated_tests");
            assert!(!reason.trim().is_empty(), "der Grund darf nicht leer sein");
        }
        other => panic!("erwarte Rejected, war {other:?}"),
    }
    assert!(
        state.attempts.contains_key("A-2"),
        "ein zweiter Versuch fand nur statt, weil W-1 nach der Ablehnung wieder 'pending' wurde"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn automated_tests_scheitert_wiederholt_bis_max_attempts_item_wird_failed_lauf_blocked() {
    let ws = tmp_dir("verify_exhausted");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                vec![],
                2,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "cargo this-subcommand-does-not-exist-xyz".to_string(),
                },
            ),
        })
        .unwrap();

    // Zwei erfolgreiche Agentenzüge, die BEIDE an derselben (immer
    // fehlschlagenden) Prüfung scheitern — max_attempts=2 ist danach erschöpft.
    let executor =
        ScriptedExecutor::new(vec![succeed("erster Versuch"), succeed("zweiter Versuch")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::Blocked);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-1"].attempt_count, 2);

    std::fs::remove_dir_all(&ws).ok();
}

/// Befund des Code-Reviews: ein nicht startbares Prüfkommando (Tippfehler,
/// falscher Programmname) ist ein Konfigurationsfehler des Operators, kein
/// fachlicher Fehlschlag — er darf nicht wie eine abgelehnte Prüfung
/// `attempt_count` erhöhen (der Agent selbst hat korrekt gearbeitet), sondern
/// muss den Lauf sichtbar mit einem harten Fehler abbrechen.
#[test]
fn automated_tests_mit_nicht_startbarem_kommando_bricht_den_lauf_mit_hartem_fehler_ab() {
    let ws = tmp_dir("verify_unstartable");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                vec![],
                3,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "agentkit-work-totally-nonexistent-program-xyz".to_string(),
                },
            ),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed("fertig")]);
    let cancel = new_cancel();
    let result = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    assert!(
        result.is_err(),
        "erwarte einen harten Fehler statt eines RunOutcome"
    );
    let state = store.snapshot();
    assert_eq!(
        state.attempts["A-1"].status,
        agentkit_work::AttemptStatus::Succeeded,
        "der Versuch des Agenten selbst war korrekt und bleibt das auch"
    );
    assert_eq!(
        state.items["W-1"].attempt_count, 0,
        "ein Konfigurationsfehler des Prüfkommandos zaehlt nicht als Fehlversuch"
    );
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn human_approval_laesst_lauf_mit_awaiting_verification_enden() {
    let ws = tmp_dir("human_approval");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                vec![],
                3,
                agentkit_work::VerificationPolicy::HumanApproval,
            ),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![succeed("Fertig, wartet auf Freigabe.")]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AwaitingVerification);
    let state = store.snapshot();
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification
    );
    assert_eq!(state.runs["R-1"].status, RunStatus::Paused);
    // `WorkRun::completion_reason` wird nur von `RunCompleted`/`RunCanceled`
    // gesetzt, nicht von `RunPaused` (bestehende Einschränkung, gilt
    // gleichermaßen für den Budget- und den Stop-Knopf-Pausenfall — kein
    // neues Verhalten dieser Phase). Der tatsächliche Grund steckt im
    // zurückgegebenen `RunOutcome::reason` (oben geprüft) und in der
    // `RunPaused.reason`-Zeichenkette im Journal.

    std::fs::remove_dir_all(&ws).ok();
}

// --------------------------------------------------- IndependentAgent (Phase 5b)

/// Item mit den beiden gebräuchlichen Feldern für die Phase-5b-Tests: eigene
/// Akzeptanzkriterien statt der leeren Liste aus `item()`.
fn item_for_review(id: &str, seq: u64, max_attempts: u32) -> WorkItem {
    let mut it = item_with_policy(
        id,
        seq,
        vec![],
        max_attempts,
        agentkit_work::VerificationPolicy::IndependentAgent,
    );
    it.acceptance_criteria = vec![
        "Kriterium A erfüllt".to_string(),
        "Kriterium B belegt".to_string(),
    ];
    it
}

#[test]
fn independent_agent_erzeugt_nach_erfolg_genau_ein_pruef_item_mit_verifies_rolle_reviewer_ohne_abhaengigkeiten(
) {
    let ws = tmp_dir("independent_agent_spawn");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let executor =
        ScriptedExecutor::new(vec![succeed_with_artifact("Analyse fertig.", "analyse.md")]);
    let cancel = new_cancel();
    let _ = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    let state = store.snapshot();
    let review_items: Vec<&WorkItem> = state
        .items
        .values()
        .filter(|it| it.kind == WorkItemKind::Review)
        .collect();
    assert_eq!(
        review_items.len(),
        1,
        "genau ein Prüf-Item, kein Regress: {review_items:?}"
    );
    let review = review_items[0];
    assert_eq!(review.verifies, Some("W-1".to_string()));
    assert_eq!(review.required_role, Some("reviewer".to_string()));
    assert!(
        review.dependencies.is_empty(),
        "ein Prüf-Item hat bewusst keine Abhängigkeit auf das geprüfte Item"
    );
    assert_eq!(
        review.verification_policy,
        agentkit_work::VerificationPolicy::None,
        "ein Prüf-Item wird nicht selbst geprüft (kein Regress)"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn pruef_item_beschreibung_enthaelt_akzeptanzkriterien_und_artefaktpfade_des_geprueften_versuchs() {
    let ws = tmp_dir("independent_agent_beschreibung");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let executor =
        ScriptedExecutor::new(vec![succeed_with_artifact("Analyse fertig.", "analyse.md")]);
    let cancel = new_cancel();
    let _ = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    let state = store.snapshot();
    let review = state
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Review)
        .expect("Prüf-Item fehlt");
    assert!(
        review.description.contains("Kriterium A erfüllt")
            && review.description.contains("Kriterium B belegt"),
        "Akzeptanzkriterien fehlen wörtlich in der Beschreibung:\n{}",
        review.description
    );
    assert!(
        review.description.contains("artifacts/W-1/A-1/analyse.md"),
        "Artefaktpfad des geprüften Versuchs fehlt in der Beschreibung:\n{}",
        review.description
    );
    assert!(
        review.description.contains("read_file"),
        "Hinweis, die Artefakte über read_file zu lesen, fehlt"
    );
    assert!(
        review.description.contains("NICHT")
            && review
                .description
                .to_lowercase()
                .contains("gesprächsverlauf"),
        "die ausdrückliche Ansage, dass der Gesprächsverlauf des Autors nicht vorliegt, fehlt:\n{}",
        review.description
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn zustimmung_schliesst_geprueftes_item_und_pruef_item_ab_und_lauf_endet_all_items_done() {
    let ws = tmp_dir("independent_agent_approve");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![
        succeed_with_artifact("Analyse fertig.", "analyse.md"),
        verdict(true, "Beide Kriterien erfüllt und belegt."),
    ]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    let review = state
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Review)
        .expect("Prüf-Item fehlt");
    assert_eq!(review.status, WorkItemStatus::Completed);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn ablehnung_setzt_geprueftes_item_zurueck_auf_pending_und_grund_erscheint_im_naechsten_arbeitspaket(
) {
    let ws = tmp_dir("independent_agent_reject");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let previous_failures_count: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let mentions_verification: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let attempt_count_at_second_claim: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let count_handle = previous_failures_count.clone();
    let mentions_handle = mentions_verification.clone();
    let attempt_count_handle = attempt_count_at_second_claim.clone();
    let second_step: Step = Box::new(move |pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        *count_handle.lock().unwrap() = Some(pkg.previous_failures.len());
        *mentions_handle.lock().unwrap() = pkg.render().contains("Verifikation abgelehnt");
        *attempt_count_handle.lock().unwrap() = Some(store.snapshot().items["W-1"].attempt_count);
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "zweiter Versuch".to_string(),
            criteria: vec![],
        });
        Ok("zweiter Versuch".to_string())
    });
    let executor = ScriptedExecutor::new(vec![
        succeed_with_artifact("Analyse fertig.", "analyse.md"),
        verdict(false, "Kriterium A nicht belegt."),
        second_step,
    ]);
    let cancel = new_cancel();
    // Der Gesamtausgang des Laufs ist hier nicht der Punkt (das zweite W-1
    // legt wieder ein Prüf-Item an, das Skript ist danach erschöpft) —
    // beobachtet wird nur, was der zweite Anlauf von W-1 sieht.
    let _ = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    assert_eq!(
        *previous_failures_count.lock().unwrap(),
        Some(1),
        "genau eine vorherige Fehlerursache beim zweiten Anlauf von W-1"
    );
    assert!(
        *mentions_verification.lock().unwrap(),
        "das Arbeitspaket des zweiten Anlaufs muss die abgelehnte Verifikation nennen"
    );
    assert_eq!(
        *attempt_count_at_second_claim.lock().unwrap(),
        Some(1),
        "die Ablehnung hat attempt_count genau einmal erhöht — der zweite Anlauf \
         fand nur statt, weil W-1 danach wieder 'pending' wurde"
    );

    let state = store.snapshot();
    match &state.attempts["A-1"].verification {
        Some(agentkit_work::AttemptVerification::Rejected { by, reason }) => {
            assert_eq!(
                by, "worker-1",
                "'by' ist die agent_id des Prüf-Agenten, nie ein Argument"
            );
            assert_eq!(reason, "Kriterium A nicht belegt.");
        }
        other => panic!("erwarte Rejected, war {other:?}"),
    }
    assert!(
        state.attempts.contains_key("A-3"),
        "ein zweiter Versuch von W-1 fand statt: {:?}",
        state.attempts.keys().collect::<Vec<_>>()
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn ablehnung_bis_max_attempts_erschoepft_geprueftes_item_wird_failed_lauf_endet_blockiert() {
    let ws = tmp_dir("independent_agent_exhausted");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 1),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![
        succeed_with_artifact("Analyse fertig.", "analyse.md"),
        verdict(false, "reicht nicht."),
    ]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::Blocked);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-1"].attempt_count, 1);
    let review = state
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Review)
        .expect("Prüf-Item fehlt");
    assert_eq!(
        review.status,
        WorkItemStatus::Completed,
        "das Prüf-Item selbst schließt normal ab, auch wenn es ablehnt"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn pruef_item_ohne_work_verdict_gilt_als_ablehnung_statt_das_geprueftre_item_haengen_zu_lassen() {
    let ws = tmp_dir("independent_agent_no_verdict");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    // max_attempts = 1: die automatische Ablehnung erschöpft W-1 SOFORT
    // (Failed), statt es zurück auf 'pending' zu setzen — sonst würde das
    // Skript (nur zwei Einträge) beim nächsten Anlauf von W-1 leerlaufen und
    // ein WEITERER, hier nicht interessierender Fehlschlag (InvalidOutput)
    // den `attempt_count` erneut erhöhen.
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 1),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![
        succeed_with_artifact("Analyse fertig.", "analyse.md"),
        succeed_without_verdict("Sieht gut aus."),
    ]);
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {}).unwrap();

    let state = store.snapshot();
    // Das Prüf-Item selbst schließt normal ab (es hat 'work_submit' gerufen) —
    // aber OHNE 'work_verdict' bleibt das geprüfte Item nicht für immer in
    // 'awaiting_verification' hängen: der Runner wertet das als Ablehnung.
    assert_ne!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification,
        "ein Prüf-Item ohne Urteil darf das geprüfte Item nicht für immer hängen lassen"
    );
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-1"].attempt_count, 1);
    assert_eq!(outcome.reason, CompletionReason::Blocked);
    match &state.attempts["A-1"].verification {
        Some(agentkit_work::AttemptVerification::Rejected { by, reason }) => {
            assert_eq!(by, "worker-1");
            assert_eq!(reason, "Prüfer hat kein Urteil abgegeben");
        }
        other => panic!("erwarte eine automatische Ablehnung, war {other:?}"),
    }

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn independent_agent_promotet_aufgezeichnete_claims_nach_der_zustimmung() {
    let ws = tmp_dir("independent_agent_promote");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    // Der Autor hält eine Erkenntnis fest ('work_claim'), die nach der
    // Zustimmung promotet werden soll.
    let claiming_and_artifact: Step = Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_artifact",
                json!({"kind": "analysis", "filename": "analyse.md", "content": "…", "summary": "Analyse"}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        let raw = tools
            .call(
                "work_claim",
                json!({"claims": [{"subject": "X", "predicate": "verursacht", "object": "Y"}]}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "Analyse fertig.".to_string(),
            criteria: vec![],
        });
        Ok("Analyse fertig.".to_string())
    });

    let executor = ScriptedExecutor::new(vec![claiming_and_artifact, verdict(true, "Passt.")]);
    let mut cfg = cfg(&ws);
    let gateway = Arc::new(FakeGraph::default());
    cfg.graph = Some(gateway.clone());
    let cancel = new_cancel();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg, &cancel, &mut |_| {}).unwrap();

    // Ein Fehlschlag der Promotion darf den Lauf nicht abbrechen: erst
    // erfolgreich (diese Ausführung), dann würde ein zweiter Lauf mit
    // fehlschlagendem Gateway trotzdem 'AllItemsDone' bleiben — geprüft über
    // den Adapter-Vertrag in `graph::promote_after_completion` (siehe
    // `agentkit_work/tests/recovery.rs`, dieselbe Zusicherung dort explizit
    // für `recover_pending_promotions`).
    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert!(
        state.items["W-1"].claims_promoted,
        "ein Item mit Verifikation und aufgezeichneten Claims promotet sie nach dem Abschluss"
    );
    assert_eq!(gateway.promoted.lock().unwrap().len(), 1);

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn scheiternde_promotion_bricht_den_lauf_nicht_ab() {
    let ws = tmp_dir("independent_agent_promote_fails");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let claiming: Step = Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_claim",
                json!({"claims": [{"subject": "X", "predicate": "verursacht", "object": "Y"}]}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "fertig".to_string(),
            criteria: vec![],
        });
        Ok("fertig".to_string())
    });

    // `verdict()` prüft nur, dass `work_verdict` keinen "ERROR"-Präfix
    // liefert — die Promotionswarnung steckt aber in GENAU diesem
    // Rückgabetext des Tools (siehe `tools::register_work_tools`s
    // `work_verdict`), nicht in einem `WorkProgress::Note` (das sieht nur der
    // Runner selbst, nicht ein Tool-Aufruf, den der Test manuell über die
    // Registry auslöst). Deshalb hier ein eigener Schritt, der den Rohtext
    // festhält.
    let verdict_raw: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let raw_handle = verdict_raw.clone();
    let verdict_step: Step = Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_verdict",
                json!({"approved": true, "reason": "Passt."}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *raw_handle.lock().unwrap() = raw;
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "Urteil erfasst.".to_string(),
            criteria: vec![],
        });
        Ok("Urteil erfasst.".to_string())
    });

    let executor = ScriptedExecutor::new(vec![claiming, verdict_step]);
    let mut cfg = cfg(&ws);
    let gateway = Arc::new(FakeGraph {
        fail: true,
        ..Default::default()
    });
    cfg.graph = Some(gateway);
    let cancel = new_cancel();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg, &cancel, &mut |_| {}).unwrap();

    assert_eq!(
        outcome.reason,
        CompletionReason::AllItemsDone,
        "eine scheiternde Promotion bricht den Lauf nicht ab"
    );
    let state = store.snapshot();
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::Completed,
        "das Item bleibt trotz gescheiterter Promotion abgeschlossen"
    );
    assert!(
        !state.items["W-1"].claims_promoted,
        "eine gescheiterte Promotion setzt das Flag nicht"
    );
    assert!(
        verdict_raw.lock().unwrap().contains("nicht promotet"),
        "eine Warnung muss im Tool-Ergebnis auftauchen: {}",
        verdict_raw.lock().unwrap()
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn item_mit_verification_policy_none_promotet_nichts() {
    let ws = tmp_dir("none_policy_no_promotion");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, vec![], 3),
        })
        .unwrap();

    let claiming: Step = Box::new(move |_pkg, ctx, store, on_event| {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_claim",
                json!({"claims": [{"subject": "X", "predicate": "verursacht", "object": "Y"}]}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "fertig".to_string(),
            criteria: vec![],
        });
        Ok("fertig".to_string())
    });

    let executor = ScriptedExecutor::new(vec![claiming]);
    let mut cfg = cfg(&ws);
    let gateway = Arc::new(FakeGraph::default());
    cfg.graph = Some(gateway.clone());
    let cancel = new_cancel();
    let outcome = run_to_completion(&store, "R-1", &executor, &cfg, &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert!(!state.items["W-1"].claims_promoted);
    assert!(
        gateway.promoted.lock().unwrap().is_empty(),
        "VerificationPolicy::None darf niemals promoten"
    );

    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn pruef_item_erzeugt_kein_weiteres_pruef_item_kein_regress() {
    let ws = tmp_dir("independent_agent_no_regress");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_for_review("W-1", 1, 3),
        })
        .unwrap();

    let executor = ScriptedExecutor::new(vec![
        succeed_with_artifact("Analyse fertig.", "analyse.md"),
        verdict(true, "Passt."),
    ]);
    let cancel = new_cancel();
    let _ = run_to_completion(&store, "R-1", &executor, &cfg(&ws), &cancel, &mut |_| {});

    let state = store.snapshot();
    let review_items = state
        .items
        .values()
        .filter(|it| it.kind == WorkItemKind::Review)
        .count();
    assert_eq!(
        review_items, 1,
        "das Prüf-Item selbst hat 'verification_policy: None' — es erzeugt kein zweites"
    );

    std::fs::remove_dir_all(&ws).ok();
}
