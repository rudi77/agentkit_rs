//! Recovery als Spezifikation: was ein abgelaufenes Lease übersteht, was ein
//! zweiter Aufruf ändert (nichts) und was ein echter Prozess-Neustart über
//! ein Journal hinweg wiederherstellt.

use agentkit_work::{
    decide, recover, recover_all, AttemptStatus, Decision, FailureInfo, FailureKind, ProjectStatus,
    RunStatus, WorkBudget, WorkEvent, WorkItem, WorkItemKind, WorkItemStatus, WorkProject, WorkRun,
    WorkStore,
};

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_recovery_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

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

fn item(id: &str, seq: u64) -> WorkItem {
    WorkItem {
        id: id.into(),
        run_id: "R-1".into(),
        title: format!("Item {id}"),
        description: String::new(),
        kind: WorkItemKind::Implementation,
        status: WorkItemStatus::Pending,
        priority: 5,
        seq,
        required_role: None,
        dependencies: vec![],
        acceptance_criteria: vec![],
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn setup_project_and_run(store: &WorkStore) {
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
}

fn claim(store: &WorkStore, item: &str, attempt: &str, lease_expires_ms: u64) {
    store
        .submit(WorkEvent::WorkItemClaimed {
            item: item.into(),
            agent: "agent-1".into(),
            attempt: attempt.into(),
            lease_expires_ms,
            at_ms: 0,
        })
        .unwrap();
}

/// Claimt, scheitert fachlich (`AttemptFinished` + `WorkItemFailed`) — und
/// journalt ABSICHTLICH kein `WorkItemReleased` danach. Simuliert einen
/// Prozess, der genau zwischen diesen beiden Ereignissen abgestürzt ist.
fn fail_without_release(store: &WorkStore, item: &str, attempt: &str) {
    claim(store, item, attempt, 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
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
    store
        .submit(WorkEvent::WorkItemFailed {
            item: item.into(),
            attempt: attempt.into(),
            at_ms: 200,
        })
        .unwrap();
}

#[test]
fn abgelaufenes_lease_gibt_item_frei_und_markiert_attempt_als_interrupted() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    claim(&store, "W-1", "A-1", 1_000);

    let report = recover(&store, 5_000).unwrap();

    assert_eq!(report.released_items, vec!["W-1".to_string()]);
    assert_eq!(report.interrupted_attempts, vec!["A-1".to_string()]);

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    // Ein unterbrochener Versuch ist kein fachlicher Fehlversuch — zählt
    // nicht gegen max_attempts.
    assert_eq!(state.items["W-1"].attempt_count, 0);
    assert_eq!(state.attempts["A-1"].status, AttemptStatus::Interrupted);
    assert_eq!(
        state.attempts["A-1"].failure.as_ref().unwrap().kind,
        FailureKind::Interrupted
    );
    assert!(state.leases.is_empty());
}

#[test]
fn zweiter_recover_aufruf_aendert_nichts_mehr() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    claim(&store, "W-1", "A-1", 1_000);

    recover(&store, 5_000).unwrap();
    let zweiter_report = recover(&store, 5_000).unwrap();

    assert!(zweiter_report.released_items.is_empty());
    assert!(zweiter_report.interrupted_attempts.is_empty());
}

#[test]
fn completed_item_wird_von_recovery_nie_angefasst() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2),
        })
        .unwrap();

    // W-1 läuft normal durch und wird fertig — kein Lease mehr danach.
    claim(&store, "W-1", "A-1", 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt".into()),
            failure: None,
            steps: 2,
            tool_calls: 1,
            at_ms: 100,
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();

    // W-2 hat ein abgelaufenes Lease — nur das darf Recovery anfassen.
    claim(&store, "W-2", "A-2", 1_000);

    let report = recover(&store, 5_000).unwrap();

    assert_eq!(report.released_items, vec!["W-2".to_string()]);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(state.attempts["A-1"].status, AttemptStatus::Succeeded);
}

#[test]
fn nicht_abgelaufenes_lease_bleibt_bei_recover_stehen_wird_aber_von_recover_all_freigegeben() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    claim(&store, "W-1", "A-1", 10_000);

    // now_ms liegt vor der Ablauffrist -> recover() lässt das Lease stehen.
    let report = recover(&store, 5_000).unwrap();
    assert!(report.released_items.is_empty());
    assert_eq!(
        store.snapshot().items["W-1"].status,
        WorkItemStatus::Running
    );

    // recover_all() gibt es trotzdem frei — MVP-Annahme "genau ein Worker".
    let report_all = recover_all(&store, 5_000).unwrap();
    assert_eq!(report_all.released_items, vec!["W-1".to_string()]);
    assert_eq!(
        store.snapshot().items["W-1"].status,
        WorkItemStatus::Pending
    );
}

#[test]
fn vollzyklus_ueber_journal_store_faellt_und_wird_wiedereroeffnet() {
    let dir = tmp_dir("full_cycle");
    {
        let store = WorkStore::open(&dir).unwrap();
        setup_project_and_run(&store);
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item("W-1", 1),
            })
            .unwrap();
        claim(&store, "W-1", "A-1", 1_000);
        // Store fällt hier aus dem Scope — simuliert den abgestürzten Prozess.
    }

    let reopened = WorkStore::open(&dir).unwrap();
    // Das Lease ist noch da (der alte Prozess hat es nie freigegeben), also
    // ist es entweder schon abgelaufen oder wird von recover_all so behandelt.
    let report = recover_all(&reopened, 5_000).unwrap();

    assert_eq!(report.released_items, vec!["W-1".to_string()]);
    assert_eq!(report.interrupted_attempts, vec!["A-1".to_string()]);

    let state = reopened.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert!(state.leases.is_empty());
    assert_eq!(state.attempts["A-1"].status, AttemptStatus::Interrupted);
    assert_eq!(state.ready_items("R-1").len(), 1, "Item ist wieder ready");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn verklemmtes_item_zwischen_work_item_failed_und_released_wird_von_recover_all_befreit() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    fail_without_release(&store, "W-1", "A-1");

    // Vor der Reparatur: Failed, noch Versuche übrig, aber KEIN Lease mehr —
    // der Lease-basierte Rundgang würde das Item nie finden.
    let before = store.snapshot();
    assert_eq!(before.items["W-1"].status, WorkItemStatus::Failed);
    assert!(before.leases.is_empty());

    let report = recover_all(&store, 5_000).unwrap();
    assert_eq!(report.released_items, vec!["W-1".to_string()]);

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(
        state.items["W-1"].attempt_count, 1,
        "der Fehlversuch selbst zählt weiter gegen max_attempts"
    );
    assert_eq!(
        decide(&state, "R-1", &WorkBudget::default(), 0, 5_000),
        Decision::Run("W-1".to_string()),
        "der Scheduler vergibt das Item wieder"
    );

    // Idempotenz: ein zweiter Aufruf ändert nichts mehr.
    let second = recover_all(&store, 5_000).unwrap();
    assert!(second.released_items.is_empty());
}

/// Simuliert einen Absturz GENAU zwischen `AttemptFinished{Succeeded}` und
/// dem folgenden `WorkItemCompleted` — journalt bewusst nur das erste.
fn succeed_without_completion(store: &WorkStore, item: &str, attempt: &str) {
    claim(store, item, attempt, 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt".into()),
            failure: None,
            steps: 3,
            tool_calls: 1,
            at_ms: 200,
        })
        .unwrap();
}

/// Befund 2 des Code-Reviews: vor der Korrektur prüfte `recover_matching` nur
/// `attempt.finished_at_ms.is_none()` und journalte im `else`-Zweig
/// bedingungslos `WorkItemReleased` — ein Absturz nach einem bereits
/// ERFOLGREICHEN `AttemptFinished` (das Lease existiert noch, weil nur
/// `WorkItemCompleted` es entfernt, siehe `state::apply`) warf die erledigte
/// Arbeit weg: das Item ging zurück auf `Pending` und wurde komplett neu
/// ausgeführt. Nach der Korrektur holt Recovery den fehlenden zweiten Schritt
/// nach, statt ihn zu verwerfen.
#[test]
fn abgestuerzter_prozess_nach_erfolgreichem_attempt_finished_vollendet_das_item_statt_es_zu_verwerfen(
) {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    succeed_without_completion(&store, "W-1", "A-1");

    // Vor der Reparatur: Item hängt auf `Running`, Lease existiert noch, der
    // Versuch hat aber schon einen feststehenden Erfolg.
    let before = store.snapshot();
    assert_eq!(before.items["W-1"].status, WorkItemStatus::Running);
    assert!(before.leases.contains_key("W-1"));
    assert_eq!(before.attempts["A-1"].status, AttemptStatus::Succeeded);

    let report = recover_all(&store, 5_000).unwrap();

    let state = store.snapshot();
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::Completed,
        "ein erfolgreich beendeter Versuch darf nicht auf 'Pending' zurückfallen"
    );
    assert_eq!(
        state.items["W-1"].attempt_count, 0,
        "ein erfolgreicher Versuch zählt nicht als Fehlversuch"
    );
    assert!(state.leases.is_empty());
    assert!(
        !report.released_items.contains(&"W-1".to_string()),
        "ein VOLLENDETES Item gilt nicht als 'freigegeben': {report:?}"
    );

    // Ein anschließender Lauf darf das Item NICHT erneut ausführen —
    // `ready_items` liefert nichts mehr, weil es `Completed` ist.
    assert!(state.ready_items("R-1").is_empty());

    // Idempotenz: ein zweiter Aufruf ändert nichts mehr.
    let second = recover_all(&store, 5_000).unwrap();
    assert!(second.released_items.is_empty());
}

/// Derselbe Absturzfall, aber mit einem fachlich GESCHEITERTEN Versuch: der
/// fehlende `WorkItemFailed`-Schritt wird nachgeholt (inkl. `attempt_count`),
/// und erst danach entscheidet sich — wie bei `runner::record_failure` —, ob
/// noch ein Versuch übrig ist.
#[test]
fn abgestuerzter_prozess_nach_gescheitertem_attempt_finished_traegt_work_item_failed_nach() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    claim(&store, "W-1", "A-1", 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
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

    // Vor der Reparatur: Lease existiert noch (nur `WorkItemFailed` entfernt
    // es), das Item hängt auf `Running`, obwohl der Versuch schon gescheitert ist.
    let before = store.snapshot();
    assert_eq!(before.items["W-1"].status, WorkItemStatus::Running);
    assert!(before.leases.contains_key("W-1"));

    let report = recover_all(&store, 5_000).unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(
        state.items["W-1"].attempt_count, 1,
        "der nachgetragene Fehlversuch zählt gegen max_attempts, genau einmal"
    );
    assert!(state.leases.is_empty());
    assert_eq!(report.released_items, vec!["W-1".to_string()]);

    // Idempotenz: ein zweiter Aufruf ändert nichts mehr.
    let second = recover_all(&store, 5_000).unwrap();
    assert!(second.released_items.is_empty());
}

/// Gegentest: ist `max_attempts` nach dem nachgetragenen Fehlversuch
/// ausgeschöpft, bleibt das Item `Failed` stehen statt freigegeben zu werden
/// — dieselbe Regel wie bei einem regulär (ohne Absturz) gescheiterten Versuch.
#[test]
fn abgestuerzter_prozess_nach_gescheitertem_attempt_finished_mit_ausgeschoepften_versuchen_bleibt_failed(
) {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    let mut exhausted = item("W-1", 1);
    exhausted.max_attempts = 1;
    store
        .submit(WorkEvent::WorkItemCreated { item: exhausted })
        .unwrap();
    claim(&store, "W-1", "A-1", 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
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

    let report = recover_all(&store, 5_000).unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Failed);
    assert_eq!(state.items["W-1"].attempt_count, 1);
    assert!(report.released_items.is_empty());
}

#[test]
fn erschoepftes_failed_item_wird_nicht_freigegeben() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    let mut exhausted = item("W-1", 1);
    exhausted.max_attempts = 1;
    store
        .submit(WorkEvent::WorkItemCreated { item: exhausted })
        .unwrap();
    fail_without_release(&store, "W-1", "A-1");

    let report = recover_all(&store, 5_000).unwrap();
    assert!(report.released_items.is_empty());
    assert_eq!(store.snapshot().items["W-1"].status, WorkItemStatus::Failed);
}
