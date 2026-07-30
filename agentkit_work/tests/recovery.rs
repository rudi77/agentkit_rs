//! Recovery als Spezifikation: was ein abgelaufenes Lease übersteht, was ein
//! zweiter Aufruf ändert (nichts) und was ein echter Prozess-Neustart über
//! ein Journal hinweg wiederherstellt.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_work::{
    decide, recover, recover_all, recover_pending_promotions, AttemptStatus, ClaimText, Decision,
    FailureInfo, FailureKind, GraphGateway, ProjectStatus, RunStatus, WorkBudget, WorkEvent,
    WorkItem, WorkItemKind, WorkItemStatus, WorkProject, WorkProvenance, WorkRun, WorkStore,
};

/// Test-Doppelgänger für [`GraphGateway`] (Duplikat-Begründung siehe
/// `tests/graph.rs`/`tests/tools.rs`): zeichnet jeden `promote`-Aufruf auf und
/// kann optional fehlschlagen, um zu prüfen, dass eine scheiternde Promotion
/// den Lauf nicht abbricht.
#[derive(Default)]
struct FakeGraph {
    promoted: Mutex<Vec<String>>,
    calls: AtomicUsize,
    fail: bool,
}

impl FakeGraph {
    fn new() -> Self {
        FakeGraph::default()
    }
}

impl GraphGateway for FakeGraph {
    fn recall(&self, _query: &str) -> Option<String> {
        None
    }

    fn record_claims(
        &self,
        _prov: &WorkProvenance,
        _claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn promote(&self, claim_ids: &[String]) -> Result<usize, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        description: String::new(),
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

/// Wie `item`, aber mit einer expliziten `VerificationPolicy` — eigene
/// Hilfsfunktion statt `item()`s Signatur zu ändern (das würde jeden
/// bestehenden Aufruf berühren, die Policy interessiert nur die neuen Tests
/// dieser Datei).
fn item_with_policy(id: &str, seq: u64, policy: agentkit_work::VerificationPolicy) -> WorkItem {
    let mut it = item(id, seq);
    it.verification_policy = policy;
    it
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

// ------------------------------------------------- Phase 5a: Verifikation

/// Bringt ein Item bis kurz vor die Verifikation: geclaimt, Versuch
/// erfolgreich abgeschlossen (`AttemptFinished{Succeeded}`), aber noch OHNE
/// `WorkItemSubmittedForVerification` — der Aufrufer entscheidet, was danach
/// (nicht) passiert.
fn succeed_up_to_attempt_finished(store: &WorkStore, item: &str, attempt: &str) {
    claim(store, item, attempt, 1_000);
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: attempt.into(),
            status: AttemptStatus::Succeeded,
            summary: Some("erledigt, wartet auf Prüfung".into()),
            failure: None,
            steps: 2,
            tool_calls: 1,
            at_ms: 100,
        })
        .unwrap();
}

/// Kernpunkt aus Vorgabe 5: ein Item, das wegen `HumanApproval` legitim
/// wartet, darf `recover_all` NICHT anfassen — auch wenn sein Lease (das
/// `WorkItemSubmittedForVerification` bewusst nicht entfernt) längst
/// abgelaufen wäre.
#[test]
fn wartendes_human_approval_item_uebersteht_recover_all_unangetastet() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy("W-1", 1, agentkit_work::VerificationPolicy::HumanApproval),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();

    // now_ms weit in der Zukunft — ein normales Lease wäre hier längst abgelaufen.
    let report = recover_all(&store, 999_999_999).unwrap();

    assert!(report.released_items.is_empty(), "{report:?}");
    assert!(report.interrupted_attempts.is_empty(), "{report:?}");
    let state = store.snapshot();
    assert_eq!(
        state.items["W-1"].status,
        WorkItemStatus::AwaitingVerification
    );
    assert!(
        state.leases.contains_key("W-1"),
        "das Lease bleibt für die spätere Freigabe erhalten"
    );
}

/// Lücke 1 aus Vorgabe 5: Absturz zwischen `WorkItemSubmittedForVerification`
/// und dem Prüfergebnis. Bei `AutomatedTests` löst die Prüfung SYNCHRON im
/// selben Versuch auf (`runner::record_success`) — ein Item darf diesen
/// Zustand über einen Neustart hinweg also nie mit `verification == None`
/// erreichen. Recovery behandelt das wie einen unterbrochenen Versuch: zurück
/// nach `Pending`, ohne `attempt_count` zu erhöhen.
#[test]
fn absturz_zwischen_submitted_for_verification_und_pruefergebnis_gibt_das_item_frei() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "cargo --version".into(),
                },
            ),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();
    // Absturz GENAU hier — kein VerificationApproved/Rejected mehr.

    let report = recover_all(&store, 5_000).unwrap();

    assert_eq!(report.released_items, vec!["W-1".to_string()]);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(
        state.items["W-1"].attempt_count, 0,
        "ein Absturz mitten in der Prüfung ist kein fachlicher Fehlschlag"
    );
    assert!(state.leases.is_empty());
}

/// Lücke 2 aus Vorgabe 5: Absturz zwischen `VerificationApproved` und dem
/// folgenden `WorkItemCompleted` — Recovery holt den fehlenden zweiten
/// Schritt nach, statt die schon genehmigte Arbeit zu verwerfen.
#[test]
fn absturz_zwischen_verification_approved_und_work_item_completed_vollendet_das_item() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy("W-1", 1, agentkit_work::VerificationPolicy::HumanApproval),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();
    store
        .submit(WorkEvent::VerificationApproved {
            item: "W-1".into(),
            attempt: "A-1".into(),
            by: "human".into(),
            reason: None,
            at_ms: 200,
        })
        .unwrap();
    // Absturz GENAU hier — kein WorkItemCompleted mehr.

    let report = recover_all(&store, 5_000).unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert!(
        !report.released_items.contains(&"W-1".to_string()),
        "ein VOLLENDETES Item gilt nicht als 'freigegeben': {report:?}"
    );
    assert!(state.leases.is_empty());
}

/// Symmetrischer Fall zu oben: Absturz zwischen `VerificationRejected` und dem
/// folgenden `WorkItemFailed` — derselbe „fachlicher Fehlschlag"-Mechanismus
/// wie ein regulärer, nicht-verifikationsbedingter Fehlschlag.
#[test]
fn absturz_zwischen_verification_rejected_und_work_item_failed_traegt_den_fehlschlag_nach() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy("W-1", 1, agentkit_work::VerificationPolicy::HumanApproval),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();
    store
        .submit(WorkEvent::VerificationRejected {
            item: "W-1".into(),
            attempt: "A-1".into(),
            by: "human".into(),
            reason: "sieht nicht fertig aus".into(),
            at_ms: 200,
        })
        .unwrap();
    // Absturz GENAU hier — kein WorkItemFailed mehr.

    let report = recover_all(&store, 5_000).unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Pending);
    assert_eq!(state.items["W-1"].attempt_count, 1);
    assert_eq!(report.released_items, vec!["W-1".to_string()]);
}

// ---------------------------------------------- Promotion (Phase 5b, §11)

/// Dieselbe Lücke 2 wie oben (`VerificationApproved` ohne folgendes
/// `WorkItemCompleted`), diesmal mit aufgezeichneten Claims: `recover_all`
/// holt den fehlenden `WorkItemCompleted`-Schritt nach — GENAU der Zustand,
/// den `recover_pending_promotions` danach als "fertig, aber noch nicht
/// promotet" erkennt und behebt.
#[test]
fn absturz_zwischen_verification_approved_und_work_item_completed_wird_danach_promotet() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "true".into(),
                },
            ),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-1".to_string()],
            at_ms: 90,
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();
    store
        .submit(WorkEvent::VerificationApproved {
            item: "W-1".into(),
            attempt: "A-1".into(),
            by: "automated_tests".into(),
            reason: None,
            at_ms: 200,
        })
        .unwrap();
    // Absturz GENAU hier — kein WorkItemCompleted mehr.

    recover_all(&store, 5_000).unwrap();
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert!(
        !state.items["W-1"].claims_promoted,
        "recover_all promotet selbst nicht — das ist Aufgabe von recover_pending_promotions"
    );

    let gateway = Arc::new(FakeGraph::new());
    let gw: Arc<dyn GraphGateway> = gateway.clone();
    let warnings = recover_pending_promotions(&store, Some(&gw), 5_000);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(*gateway.promoted.lock().unwrap(), vec!["C-1".to_string()]);
    assert!(store.snapshot().items["W-1"].claims_promoted);
}

/// Die zweite, neue Lücke aus Phase 5b: ein Absturz zwischen
/// `WorkItemCompleted` und `ClaimsPromoted` selbst — die Promotion war noch
/// nicht einmal versucht. `recover_pending_promotions` erkennt das rein am
/// Item (`Completed`, Policy != `None`, `claims_promoted == false`), unabhängig
/// von jedem Lease.
#[test]
fn absturz_zwischen_work_item_completed_und_claims_promoted_wird_beim_naechsten_resume_nachgeholt()
{
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "true".into(),
                },
            ),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-7".to_string()],
            at_ms: 90,
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemSubmittedForVerification {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();
    store
        .submit(WorkEvent::VerificationApproved {
            item: "W-1".into(),
            attempt: "A-1".into(),
            by: "automated_tests".into(),
            reason: None,
            at_ms: 200,
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 200,
        })
        .unwrap();
    // Absturz GENAU hier — 'WorkItemCompleted' liegt vor, 'ClaimsPromoted' fehlt.

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert!(!state.items["W-1"].claims_promoted);

    let gateway = Arc::new(FakeGraph::new());
    let gw: Arc<dyn GraphGateway> = gateway.clone();
    let warnings = recover_pending_promotions(&store, Some(&gw), 9_999);

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(*gateway.promoted.lock().unwrap(), vec!["C-7".to_string()]);
    assert!(store.snapshot().items["W-1"].claims_promoted);
}

/// `VerificationPolicy::None` promotet NICHTS — auch wenn Claims am Versuch
/// hängen: es gab nie eine Prüfung, die eine Promotion rechtfertigt.
#[test]
fn recover_pending_promotions_laesst_items_ohne_verifikationspolicy_unangetastet() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    claim(&store, "W-1", "A-1", 1_000);
    store
        .submit(WorkEvent::ClaimsRecorded {
            attempt: "A-1".into(),
            claim_ids: vec!["C-9".to_string()],
            at_ms: 50,
        })
        .unwrap();
    store
        .submit(WorkEvent::AttemptFinished {
            attempt: "A-1".into(),
            status: AttemptStatus::Succeeded,
            summary: Some("ok".into()),
            failure: None,
            steps: 1,
            tool_calls: 0,
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

    let gateway = Arc::new(FakeGraph::new());
    let gw: Arc<dyn GraphGateway> = gateway.clone();
    let warnings = recover_pending_promotions(&store, Some(&gw), 200);

    assert!(warnings.is_empty());
    assert_eq!(
        gateway.calls.load(Ordering::SeqCst),
        0,
        "VerificationPolicy::None darf das Gateway nie aufrufen"
    );
    assert!(!store.snapshot().items["W-1"].claims_promoted);
}

/// Ohne angebundenen Graphen (`gateway: None`, z. B. ohne '--graph DIR') ist
/// `recover_pending_promotions` ein reines No-Op — kein Scan, keine Warnung.
#[test]
fn recover_pending_promotions_ohne_gateway_ist_ein_no_op() {
    let store = WorkStore::in_memory();
    setup_project_and_run(&store);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item_with_policy(
                "W-1",
                1,
                agentkit_work::VerificationPolicy::AutomatedTests {
                    command: "true".into(),
                },
            ),
        })
        .unwrap();
    succeed_up_to_attempt_finished(&store, "W-1", "A-1");
    store
        .submit(WorkEvent::WorkItemCompleted {
            item: "W-1".into(),
            attempt: "A-1".into(),
            at_ms: 100,
        })
        .unwrap();

    let warnings = recover_pending_promotions(&store, None, 200);

    assert!(warnings.is_empty());
    assert!(!store.snapshot().items["W-1"].claims_promoted);
}
