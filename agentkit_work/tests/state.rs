//! Die Projektion als Spezifikation: was ready ist, was blockiert und was ein
//! verbotener Übergang bedeutet.

use agentkit_work::{
    AttemptStatus, CompletionReason, FailureInfo, FailureKind, ProjectStatus, RunStatus,
    WorkArtifact, WorkBudget, WorkError, WorkEvent, WorkItem, WorkItemKind, WorkItemStatus,
    WorkProject, WorkRun, WorkState,
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

fn claim_and_start(state: &mut WorkState, item: &str, attempt: &str) {
    state
        .apply(&WorkEvent::WorkItemClaimed {
            item: item.into(),
            agent: "agent-1".into(),
            attempt: attempt.into(),
            lease_expires_ms: 1_000,
            at_ms: 0,
        })
        .unwrap();
    state
        .apply(&WorkEvent::LeaseRenewed {
            item: item.into(),
            attempt: attempt.into(),
            lease_expires_ms: 2_000,
            at_ms: 100,
        })
        .unwrap();
}

fn complete_item(state: &mut WorkState, item: &str, attempt: &str) {
    claim_and_start(state, item, attempt);
    state
        .apply(&WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt".into()),
            failure: None,
            steps: 3,
            tool_calls: 1,
            at_ms: 200,
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCompleted {
            item: item.into(),
            attempt: attempt.into(),
            at_ms: 200,
        })
        .unwrap();
}

/// Bringt ein Item nach `AwaitingVerification`: claimt, startet, schließt den
/// Versuch erfolgreich ab und journalt `WorkItemSubmittedForVerification` —
/// dieselbe Ereignisfolge wie `runner::record_success` bei einer Policy ≠
/// `None`.
fn submit_for_verification(state: &mut WorkState, item: &str, attempt: &str) {
    claim_and_start(state, item, attempt);
    state
        .apply(&WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt, wartet auf Prüfung".into()),
            failure: None,
            steps: 2,
            tool_calls: 1,
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

fn fail_item(state: &mut WorkState, item: &str, attempt: &str) {
    claim_and_start(state, item, attempt);
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
fn item_mit_unerfuellter_abhaengigkeit_ist_nicht_ready() {
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

    let ready: Vec<&str> = state
        .ready_items("R-1")
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(ready, vec!["W-1"]);
}

#[test]
fn nach_completed_des_vorgaengers_ist_es_ready() {
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

    complete_item(&mut state, "W-1", "A-1");

    let ready: Vec<&str> = state
        .ready_items("R-1")
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(ready, vec!["W-2"]);
}

#[test]
fn gescheiterter_vorgaenger_mit_verbleibenden_versuchen_wartet_nur() {
    // Standardfall (max_attempts default 3, ein Fehlschlag = attempt_count 1):
    // der Nachfolger wartet noch, ist aber nicht ENDGÜLTIG blockiert — W-1
    // könnte beim nächsten Anlauf noch gelingen.
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

    fail_item(&mut state, "W-1", "A-1");

    assert_eq!(state.waiting_on("W-2"), vec!["W-1".to_string()]);
    assert!(
        state.blocked_by("W-2").is_empty(),
        "noch retrybar, also nicht endgültig blockiert"
    );
    assert!(state.waiting_on("W-1").is_empty());
    assert!(state.blocked_by("W-1").is_empty());
}

#[test]
fn endgueltig_gescheiterter_vorgaenger_blockiert_den_nachfolger_dauerhaft() {
    // Versuche ausgeschöpft (max_attempts 1): W-1 kommt nie mehr — der
    // Nachfolger ist nicht nur wartend, sondern endgültig blockiert.
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

    assert_eq!(state.waiting_on("W-2"), vec!["W-1".to_string()]);
    assert_eq!(state.blocked_by("W-2"), vec!["W-1".to_string()]);
}

#[test]
fn validate_dependencies_lehnt_unbekannte_id_ab() {
    let state = WorkState::default();
    let err = state
        .validate_dependencies("W-2", &["W-99".to_string()])
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
}

#[test]
fn validate_dependencies_lehnt_selbstreferenz_ab() {
    let state = WorkState::default();
    let err = state
        .validate_dependencies("W-1", &["W-1".to_string()])
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
}

#[test]
fn validate_dependencies_lehnt_duplikate_ab() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();

    let err = state
        .validate_dependencies("W-2", &["W-1".to_string(), "W-1".to_string()])
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
}

#[test]
fn validate_dependencies_lehnt_zyklus_ab() {
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

    // W-2 hängt bereits von W-1 ab; W-1 zusätzlich von W-2 abhängen zu lassen
    // würde den Zyklus W-1 -> W-2 -> W-1 schließen.
    let err = state
        .validate_dependencies("W-1", &["W-2".to_string()])
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
}

#[test]
fn validate_dependencies_terminiert_bei_bereits_vorhandenem_zyklus() {
    // Ein im Graphen schon vorhandener Zyklus (z. B. aus einem fehlerhaften
    // Batch-Import) darf die Suche nicht in eine Endlosschleife schicken —
    // die Besucht-Menge muss auch das abfangen.
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec!["W-2"]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec!["W-1"]),
        })
        .unwrap();

    // W-3 kommt in dem bestehenden Zyklus nicht vor -> keine Ablehnung, aber
    // vor allem: der Aufruf muss überhaupt zurückkehren.
    assert!(state
        .validate_dependencies("W-3", &["W-1".to_string()])
        .is_ok());
}

#[test]
fn versuch_ohne_lease_renewed_laeuft_glatt_durch() {
    // Regressionstest: ein kurzer Zug (z. B. FakeLlm antwortet in einem
    // Schritt) erzeugt gar keinen Heartbeat. Vor der Entfernung von `Claimed`
    // blieb das Item ohne `LeaseRenewed` auf `Claimed` stehen, und
    // `WorkItemCompleted` lief in den verbotenen Übergang `Claimed -> Completed`.
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();

    state
        .apply(&WorkEvent::WorkItemClaimed {
            item: "W-1".into(),
            agent: "agent-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 1_000,
            at_ms: 0,
        })
        .unwrap();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Running);

    state
        .apply(&WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
            status: AttemptStatus::Succeeded,
            summary: Some("ok".into()),
            failure: None,
            steps: 1,
            tool_calls: 0,
            at_ms: 50,
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 50,
        })
        .unwrap();

    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
}

#[test]
fn erschoepfte_max_attempts_nimmt_das_item_aus_ready_items() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    let mut only_one_try = item("W-1", 1, 5, vec![]);
    only_one_try.max_attempts = 1;
    state
        .apply(&WorkEvent::WorkItemCreated { item: only_one_try })
        .unwrap();

    fail_item(&mut state, "W-1", "A-1");
    // Retry-Übergang Failed -> Pending, wie ihn der Runner nach einem
    // fachlichen Fehlschlag auslösen würde.
    state
        .apply(&WorkEvent::WorkItemReleased {
            item: "W-1".into(),
            reason: "retry".into(),
            at_ms: 300,
        })
        .unwrap();

    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(state.items["W-1"].attempt_count, 1);
    assert!(
        state.ready_items("R-1").is_empty(),
        "Item mit erschöpften Versuchen darf nicht ready sein"
    );
}

#[test]
fn ready_items_sortiert_nach_prioritaet_dann_seq() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 1, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec![]),
        })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-3", 3, 5, vec![]),
        })
        .unwrap();

    let ready: Vec<&str> = state
        .ready_items("R-1")
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    // Priorität 5 vor Priorität 1; innerhalb der Priorität 5 W-2 vor W-3 (seq).
    assert_eq!(ready, vec!["W-2", "W-3", "W-1"]);
}

#[test]
fn verbotener_statusuebergang_liefert_err() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();

    // Pending -> Pending (per "Release") steht nicht in der Übergangsmatrix.
    let err = state
        .apply(&WorkEvent::WorkItemReleased {
            item: "W-1".into(),
            reason: "unsinn".into(),
            at_ms: 0,
        })
        .unwrap_err();
    assert!(matches!(err, WorkError::Transition(_)), "{err}");
}

#[test]
fn apply_ist_deterministisch() {
    let events: Vec<WorkEvent> = vec![
        WorkEvent::ProjectCreated { project: project() },
        WorkEvent::RunStarted { run: run("R-1") },
        WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        },
        WorkEvent::WorkItemClaimed {
            item: "W-1".into(),
            agent: "agent-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 1_000,
            at_ms: 0,
        },
        WorkEvent::LeaseRenewed {
            item: "W-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 2_000,
            at_ms: 100,
        },
        WorkEvent::ArtifactCreated {
            artifact: WorkArtifact {
                id: "AR-1".into(),
                work_item_id: "W-1".into(),
                attempt_id: "A-1".into(),
                kind: agentkit_work::ArtifactKind::Analysis,
                rel_path: "artifacts/W-1/analyse.md".into(),
                summary: "Befund".into(),
                created_at_ms: 150,
            },
        },
        WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt".into()),
            failure: None,
            steps: 4,
            tool_calls: 2,
            at_ms: 200,
        },
        WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 200,
        },
        WorkEvent::RunCompleted {
            run: "R-1".into(),
            reason: CompletionReason::AllItemsDone,
            at_ms: 210,
        },
    ];

    let mut a = WorkState::default();
    let mut b = WorkState::default();
    for event in &events {
        a.apply(event).unwrap();
        b.apply(event).unwrap();
    }
    assert_eq!(a, b);
    assert_eq!(a.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(a.runs["R-1"].status, RunStatus::Completed);
}

#[test]
fn apply_lehnt_work_item_created_mit_bereits_vergebener_id_ab() {
    // Zweite Verteidigungslinie hinter `WorkStore::submit_with` (ID-Vergabe im
    // Schreiber-Lock): sollte trotzdem irgendwo dieselbe ID zweimal auftauchen,
    // darf `apply` den zweiten Datensatz nicht über den ersten schreiben.
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();

    let err = state
        .apply(&WorkEvent::WorkItemCreated {
            // Anderer Titel, gleiche ID — genau der Fall, den ein Rennen
            // zwischen zwei parallelen `work_add_item`-Aufrufen erzeugen würde.
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
    assert_eq!(
        state.items.len(),
        1,
        "der zweite Versuch darf das erste Item nicht überschreiben"
    );
}

#[test]
fn apply_lehnt_zweites_project_created_ab() {
    // Genau die Lücke, die 'run --max-steps' früher ausgenutzt hat: ein
    // zweites 'ProjectCreated' würde behaupten, das Vorhaben sei zweimal
    // angelegt worden. Ein Budget-Wechsel läuft über `BudgetUpdated`.
    let mut state = WorkState::default();
    state
        .apply(&WorkEvent::ProjectCreated { project: project() })
        .unwrap();

    let mut zweites = project();
    zweites.title = "Anderer Titel".into();
    let err = state
        .apply(&WorkEvent::ProjectCreated { project: zweites })
        .unwrap_err();
    assert!(matches!(err, WorkError::Invalid(_)), "{err}");
    assert_eq!(
        state.project.as_ref().unwrap().title,
        "Demo",
        "das zweite ProjectCreated darf das erste Projekt nicht überschreiben"
    );
}

#[test]
fn budget_updated_ersetzt_nur_das_budget_und_erfordert_ein_vorhandenes_projekt() {
    let mut state = WorkState::default();
    let err = state
        .apply(&WorkEvent::BudgetUpdated {
            budget: WorkBudget {
                max_steps_per_attempt: 80,
                ..WorkBudget::default()
            },
            at_ms: 0,
        })
        .unwrap_err();
    assert!(matches!(err, WorkError::NotFound(_)), "{err}");

    state
        .apply(&WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    let neues_budget = WorkBudget {
        max_steps_per_attempt: 80,
        ..WorkBudget::default()
    };
    state
        .apply(&WorkEvent::BudgetUpdated {
            budget: neues_budget.clone(),
            at_ms: 10,
        })
        .unwrap();
    assert_eq!(state.project.as_ref().unwrap().budget, neues_budget);
    assert_eq!(state.project.as_ref().unwrap().title, "Demo");
}

/// Die Items aller Läufe liegen in EINER Map — ohne den `run_id`-Filter würde
/// ein zweiter Lauf die offenen Reste des ersten mit einplanen.
#[test]
fn ready_items_liefert_nur_items_des_gefragten_laufs() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::RunStarted { run: run("R-2") })
        .unwrap();
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    let mut zweiter = item("W-2", 2, 5, vec![]);
    zweiter.run_id = "R-2".into();
    state
        .apply(&WorkEvent::WorkItemCreated { item: zweiter })
        .unwrap();

    let r1: Vec<&str> = state
        .ready_items("R-1")
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    let r2: Vec<&str> = state
        .ready_items("R-2")
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(r1, vec!["W-1"]);
    assert_eq!(r2, vec!["W-2"]);
}

// -------------------------------------------------------- ClaimsRecorded

#[test]
fn claims_recorded_haengt_die_ids_an_den_versuch() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    claim_and_start(&mut state, "W-1", "A-1");

    state
        .apply(&WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-1".into(), "C-2".into()],
            at_ms: 200,
        })
        .unwrap();

    assert_eq!(
        state.attempts["A-1"].claim_ids,
        vec!["C-1".to_string(), "C-2".to_string()]
    );
}

#[test]
fn zwei_claims_recorded_ereignisse_haengen_an_statt_zu_ersetzen() {
    // Ein Versuch darf 'work_claim' mehrfach aufrufen (event.rs-Moduldoku) —
    // das zweite Ereignis darf das erste nicht überschreiben.
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    claim_and_start(&mut state, "W-1", "A-1");

    state
        .apply(&WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-1".into()],
            at_ms: 200,
        })
        .unwrap();
    state
        .apply(&WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-2".into(), "C-3".into()],
            at_ms: 300,
        })
        .unwrap();

    assert_eq!(
        state.attempts["A-1"].claim_ids,
        vec!["C-1".to_string(), "C-2".to_string(), "C-3".to_string()]
    );
}

#[test]
fn claims_recorded_fuer_unbekannten_versuch_ist_ein_fehler() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    let err = state.apply(&WorkEvent::ClaimsRecorded {
        attempt: "A-99".into(),
        claim_ids: vec!["C-1".into()],
        at_ms: 0,
    });
    assert!(matches!(err, Err(WorkError::NotFound(_))), "{err:?}");
}

// ------------------------------------------------- Phase 5a: Verifikation

#[test]
fn awaiting_verification_ist_weder_ready_noch_terminal() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");

    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification
    );
    assert!(
        state.ready_items("R-1").is_empty(),
        "ein wartendes Item darf nicht erneut ausgeführt werden"
    );
    assert!(
        !state.is_run_complete("R-1"),
        "ein Lauf mit einem wartenden Item ist nicht fertig"
    );
}

#[test]
fn nachfolger_eines_wartenden_items_ist_nicht_endgueltig_blockiert() {
    // Ein Vorgänger in AwaitingVerification kann noch Completed ODER Pending
    // (Retry) werden — "irgendwann noch möglich" ist die richtige Antwort,
    // nicht "nie".
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

    assert_eq!(state.waiting_on("W-2"), vec!["W-1".to_string()]);
    assert!(state.blocked_by("W-2").is_empty());
}

#[test]
fn run_canceled_kaskadiert_auch_auf_ein_wartendes_item() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");

    state
        .apply(&WorkEvent::RunCanceled {
            run: "R-1".into(),
            at_ms: 500,
        })
        .unwrap();

    assert_eq!(state.items["W-1"].status, WorkItemStatus::Canceled);
    assert!(
        !state.leases.contains_key("W-1"),
        "das Lease muss beim Abbruch mit entfernt werden"
    );
}

#[test]
fn work_item_completed_und_failed_sind_auch_aus_awaiting_verification_erlaubt() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");
    state
        .apply(&WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 600,
        })
        .unwrap();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);

    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-2", 2, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-2", "A-2");
    state
        .apply(&WorkEvent::WorkItemFailed {
            item: "W-2".into(),
            attempt: "A-2".into(),
            at_ms: 600,
        })
        .unwrap();
    assert_eq!(state.items["W-2"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-2"].attempt_count, 1);
}

#[test]
fn expired_leases_schliesst_ein_wartendes_item_strukturell_aus() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");
    // Lease existiert weiterhin (siehe event.rs), seine Ablauffrist liegt
    // längst in der Vergangenheit — trotzdem darf `expired_leases` es nicht
    // melden.
    assert!(state.leases.contains_key("W-1"));
    assert!(
        state.expired_leases(u64::MAX).is_empty(),
        "ein wartendes Item darf nie als 'abgelaufen' gelten"
    );
}

#[test]
fn verification_approved_und_rejected_haengen_am_versuch() {
    let mut state = WorkState::default();
    project_and_run(&mut state);
    state
        .apply(&WorkEvent::WorkItemCreated {
            item: item("W-1", 1, 5, vec![]),
        })
        .unwrap();
    submit_for_verification(&mut state, "W-1", "A-1");
    state
        .apply(&WorkEvent::VerificationApproved {
            item: "W-1".into(),
            attempt: "A-1".into(),
            by: "automated_tests".into(),
            reason: None,
            at_ms: 200,
        })
        .unwrap();
    assert_eq!(
        state.attempts["A-1"].verification,
        Some(agentkit_work::AttemptVerification::Approved {
            by: "automated_tests".into(),
            reason: None,
        })
    );
    // Der Statusübergang läuft NICHT über VerificationApproved selbst.
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification
    );
}
