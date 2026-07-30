//! Der Scheduler als Spezifikation: was als Nächstes läuft, was wartet und
//! was nie mehr voran kommt.

use agentkit_work::{
    decide, AttemptStatus, Decision, FailureInfo, FailureKind, ProjectStatus, RunStatus,
    WorkBudget, WorkEvent, WorkItem, WorkItemKind, WorkItemStatus, WorkProject, WorkRun, WorkState,
};

fn project() -> WorkProject {
    WorkProject {
        id: "demo".into(),
        title: "Demo".into(),
        objective: "Testen".into(),
        workspace: ".".into(),
        status: ProjectStatus::Active,
        created_at_ms: 0,
        budget: WorkBudget::default(),
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

fn item(id: &str, seq: u64, priority: u8, deps: Vec<&str>) -> WorkItem {
    WorkItem {
        id: id.into(),
        run_id: "R-1".into(),
        title: format!("Item {id}"),
        description: String::new(),
        kind: WorkItemKind::Implementation,
        status: WorkItemStatus::Pending,
        priority,
        seq,
        required_role: None,
        dependencies: deps.into_iter().map(String::from).collect(),
        acceptance_criteria: vec![],
        verification_policy: agentkit_work::VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn project_and_run(state: &mut WorkState) {
    state
        .apply(&WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    state
        .apply(&WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
}

fn claim(state: &mut WorkState, item: &str, attempt: &str) {
    state
        .apply(&WorkEvent::WorkItemClaimed {
            item: item.into(),
            agent: "agent-1".into(),
            attempt: attempt.into(),
            lease_expires_ms: 1_000,
            at_ms: 0,
        })
        .unwrap();
}

fn fail_item(state: &mut WorkState, item: &str, attempt: &str) {
    claim(state, item, attempt);
    state
        .apply(&WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Failed,
            summary: None,
            failure: Some(FailureInfo {
                kind: FailureKind::ModelFailure,
                message: "boom".into(),
            }),
            steps: 1,
            tool_calls: 0,
            at_ms: 200,
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemFailed {
            item: item.into(),
            attempt: attempt.into(),
            at_ms: 200,
        })
        .unwrap();
}

#[test]
fn leerer_lauf_ist_sofort_fertig() {
    let mut state = WorkState::default();
    project_and_run(&mut state);

    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::Done);
}

#[test]
fn hoehere_prioritaet_gewinnt() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 1, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 9, vec![]),
        })
        .unwrap();

    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::Run("W-2".into()));
}

#[test]
fn bei_gleicher_prioritaet_gewinnt_kleinere_seq() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 5, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec![]),
        })
        .unwrap();

    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::Run("W-2".into()));
}

#[test]
fn laufendes_item_fuehrt_bei_max_parallel_agents_1_zu_at_capacity() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec![]),
        })
        .unwrap();
    claim(&mut state, "W-1", "A-1");

    // W-2 wäre ready, aber max_parallel_agents = 1 ist schon durch W-1 belegt.
    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::AtCapacity);
}

#[test]
fn ueberschrittene_wall_time_liefert_budget_exhausted_trotz_bereitem_item() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();

    let budget = WorkBudget {
        max_wall_time_secs: Some(10),
        ..WorkBudget::default()
    };
    // 20s seit Start, Limit 10s — überzogen, obwohl W-1 ready wäre.
    let decision = decide(&state, "R-1", &budget, 0, 20_000);
    assert!(
        matches!(decision, Decision::BudgetExhausted(_)),
        "{decision:?}"
    );
}

#[test]
fn ueberschrittene_max_work_items_liefert_budget_exhausted() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec![]),
        })
        .unwrap();

    let budget = WorkBudget {
        max_work_items: Some(2),
        ..WorkBudget::default()
    };
    let decision = decide(&state, "R-1", &budget, 0, 0);
    assert!(
        matches!(decision, Decision::BudgetExhausted(_)),
        "{decision:?}"
    );
}

#[test]
fn item_mit_endgueltig_gescheiterter_abhaengigkeit_ist_blocked_mit_genau_diesem_item() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    let mut only_one_try = item("W-1", 1, 5, vec![]);
    only_one_try.max_attempts = 1;
    state
        .apply(&WorkEvent::WorkItemCreated { item: only_one_try })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec!["W-1"]),
        })
        .unwrap();
    fail_item(&mut state, "W-1", "A-1");

    // W-1 ist terminal Failed (Versuche ausgeschöpft) und zählt nicht mehr als
    // offen; W-2 ist das einzige offene Item und kann nie mehr laufen.
    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::Blocked(vec!["W-2".into()]));
}

#[test]
fn item_das_auf_laufendes_item_wartet_ist_at_capacity_nicht_blocked() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec!["W-1"]),
        })
        .unwrap();
    claim(&mut state, "W-1", "A-1");

    // max_parallel_agents = 2: die Kapazitätsprüfung (Schritt 3) greift noch
    // nicht, trotzdem kann W-2 nicht laufen (W-1 nicht Completed) — das ist
    // Warten, keine endgültige Blockade.
    let budget = WorkBudget {
        max_parallel_agents: 2,
        ..WorkBudget::default()
    };
    let decision = decide(&state, "R-1", &budget, 0, 0);
    assert_eq!(decision, Decision::AtCapacity);
}

/// Bringt ein Item nach `AwaitingVerification` (claimen, erfolgreich
/// abschließen, `WorkItemSubmittedForVerification` journalen) — dieselbe
/// Ereignisfolge wie `runner::record_success` bei einer Policy ≠ `None`.
fn submit_for_verification(state: &mut WorkState, item: &str, attempt: &str) {
    claim(state, item, attempt);
    state
        .apply(&WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt".into()),
            failure: None,
            steps: 1,
            tool_calls: 0,
            at_ms: 150,
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemSubmittedForVerification {
            item: item.into(),
            attempt: attempt.into(),
            at_ms: 150,
        })
        .unwrap();
}

/// Phase 5a: nichts ist ausführbar, aber ein Item wartet in
/// `AwaitingVerification` — der Lauf soll das als eigenen Grund erkennen,
/// nicht als `Blocked` (kommt nie mehr voran) oder generisches `AtCapacity`.
#[test]
fn nur_noch_wartende_items_liefert_awaiting_verification_nicht_blocked() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec!["W-1"]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");

    // W-2 wartet auf W-1 (AwaitingVerification, nicht Completed) — nichts ist
    // ready, aber der Lauf ist nicht endgültig blockiert.
    let decision = decide(&state, "R-1", &WorkBudget::default(), 0, 0);
    assert_eq!(decision, Decision::AwaitingVerification(vec!["W-1".into()]));
}
