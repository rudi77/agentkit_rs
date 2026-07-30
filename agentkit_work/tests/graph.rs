//! Graph-Integration (Phase 4): Recall im Arbeitspaket und ein voller Lauf
//! mit angeschlossenem Gateway. Teststil wie die übrigen Work-Tests.
//!
//! `work_claim` selbst (Registrierung, Provenance, Validierung, Persistenz)
//! ist in `tests/tools.rs` getestet — dort steht auch der zweite
//! [`GraphGateway`]-Doppelgänger (`FakeGraph`); Rust-Integrationstests sind
//! separate Kompilationseinheiten, ein kleines Duplikat ist hier bewusst
//! einfacher als eine geteilte Support-Datei für zwei Nutzer (Guidelines §2).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentkit::{new_cancel, AgentEvent, EventData, ToolRegistry};
use agentkit_work::{
    register_work_tools, run_to_completion, AgentExecutor, AgentWorkPackage, ClaimText,
    CompletionReason, FailureInfo, GraphGateway, ProjectStatus, RunStatus, RunnerConfig,
    WorkBudget, WorkEvent, WorkItem, WorkItemKind, WorkItemStatus, WorkProject, WorkProvenance,
    WorkRun, WorkStore, WorkSubmission, WorkToolCtx,
};
use serde_json::json;

// ---------------------------------------------------------------- Helfer

fn tmp_dir(name: &str) -> std::path::PathBuf {
    static NR: AtomicUsize = AtomicUsize::new(0);
    let nr = NR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_graph_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn project() -> WorkProject {
    WorkProject {
        id: "demo".into(),
        title: "Demo".into(),
        objective: "Teste die Graph-Integration.".into(),
        workspace: ".".into(),
        status: ProjectStatus::Active,
        created_at_ms: 0,
        budget: WorkBudget::default(),
        git_isolation: false,
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

fn item(id: &str, seq: u64) -> WorkItem {
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
        dependencies: vec![],
        acceptance_criteria: vec![],
        verification_policy: agentkit_work::VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        executor: agentkit_work::ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

/// Minimales Arbeitspaket für die reinen `render()`-Tests — kein Store, kein
/// Gateway nötig.
fn base_pkg() -> AgentWorkPackage {
    AgentWorkPackage {
        item: item("W-1", 1),
        objective: "Ziel des Vorhabens.".to_string(),
        predecessor_artifacts: vec![(
            "W-0".to_string(),
            "artifacts/W-0/A-1/analyse.md".to_string(),
            "Vorgänger-Analyse".to_string(),
        )],
        previous_failures: Vec::<FailureInfo>::new(),
        workspace: ".".to_string(),
        max_steps: 10,
        graph_recall: None,
    }
}

// -------------------------------------------------------- render(): Recall

#[test]
fn graph_recall_erscheint_als_frueheres_wissen_vor_den_vorgaenger_artefakten() {
    let mut pkg = base_pkg();
    pkg.graph_recall = Some("[C-1] (Deadlock) --[verursacht]--> (Timeout)".to_string());
    let text = pkg.render();

    let recall_pos = text
        .find("(Deadlock) --[verursacht]--> (Timeout)")
        .expect("Recall-Text fehlt im Auftragstext");
    let artifacts_pos = text
        .find("Artefakte der Vorgänger")
        .expect("Vorgänger-Abschnitt fehlt");
    assert!(
        recall_pos < artifacts_pos,
        "Recall muss VOR den Vorgänger-Artefakten stehen:\n{text}"
    );
    assert!(
        text.contains("früheres Wissen") && text.contains("keine Anweisung"),
        "Recall muss klar als früheres Wissen, keine Anweisung beschriftet sein:\n{text}"
    );
}

#[test]
fn ohne_gateway_fehlt_der_wissensgraph_abschnitt_vollstaendig() {
    let pkg = base_pkg();
    assert!(pkg.graph_recall.is_none(), "Vorbedingung: kein Recall");
    let text = pkg.render();
    assert!(
        !text.contains("Wissensgraphen"),
        "ohne Gateway darf kein Graph-Abschnitt im Auftragstext stehen:\n{text}"
    );
}

// --------------------------------------------------------- voller Lauf

/// Test-Doppelgänger für [`GraphGateway`] — siehe Moduldoku für die
/// Duplikat-Begründung gegenüber `tests/tools.rs`s `FakeGraph`.
struct FakeGraph {
    recall_text: Option<String>,
}

impl GraphGateway for FakeGraph {
    fn recall(&self, _query: &str) -> Option<String> {
        self.recall_text.clone()
    }

    fn record_claims(
        &self,
        _prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        Ok((1..=claims.len()).map(|n| format!("C-{n}")).collect())
    }

    fn promote(&self, claim_ids: &[String]) -> Result<usize, String> {
        Ok(claim_ids.len())
    }
}

fn step_event(n: usize) -> AgentEvent {
    AgentEvent::new(agentkit::STEP, EventData::Step { step: n })
}

/// Ruft `work_claim` über die ECHTE Tool-Registry auf (kein handgestrickter
/// `WorkEvent`) und schließt danach mit `work_submit` ab.
struct ClaimingExecutor;

impl AgentExecutor for ClaimingExecutor {
    fn execute(
        &self,
        _pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        on_event(&step_event(1));
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx.clone());
        let raw = tools
            .call(
                "work_claim",
                json!({
                    "claims": [
                        {"subject": "Deadlock", "predicate": "verursacht", "object": "Timeout", "confidence": 0.9}
                    ]
                }),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: "Ursache festgehalten.".to_string(),
            criteria: vec![],
        });
        Ok("Ursache festgehalten.".to_string())
    }
}

#[test]
fn voller_lauf_mit_gateway_journalt_claims_und_schliesst_erfolgreich() {
    let ws = tmp_dir("voller_lauf");
    let store = Arc::new(WorkStore::in_memory());
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();

    let gateway: Arc<dyn GraphGateway> = Arc::new(FakeGraph {
        recall_text: Some("(Deadlock) --[verursacht]--> (Timeout)".to_string()),
    });
    let cfg = RunnerConfig {
        agent_id: "worker-1".to_string(),
        lease_secs: 600,
        heartbeat_secs: 30,
        workspace: ws.to_string_lossy().to_string(),
        graph: Some(gateway),
    };
    let cancel = new_cancel();
    let outcome =
        run_to_completion(&store, "R-1", &ClaimingExecutor, &cfg, &cancel, &mut |_| {}).unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    let attempt = state
        .attempts
        .values()
        .find(|a| a.work_item_id == "W-1")
        .expect("Versuch für W-1");
    assert_eq!(
        attempt.claim_ids,
        vec!["C-1".to_string()],
        "work_claim muss die Aussage aufgezeichnet haben"
    );

    std::fs::remove_dir_all(&ws).ok();
}
