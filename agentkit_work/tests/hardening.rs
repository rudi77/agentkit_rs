//! Regressionstests zu den Befunden des Code-Reviews: Prüfkommandos des
//! Modells brauchen eine Freigabe, Journal und Sperre sind vor dem Item-Agenten
//! geschützt, eine Submission sticht den Sentinel, das Journal übersteht einen
//! Absturz zwischen Zeile und Umbruch, und `work_add_item` hält das Item-Budget
//! ein. Teststil wie die übrigen Dateien (deutsche Satznamen, ein Verhalten pro
//! Test).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::testing::FakeLlm;
use agentkit::{new_cancel, AgentEvent, Chunk, ExitCode, Llm, ToolRegistry};
use agentkit_work::{
    dispatch_with_io, register_work_tools, run_to_completion, AgentExecutor, AgentWorkPackage,
    AttemptVerification, CodingAgentExecutor, CompletionReason, ExecutorKind, ProjectStatus,
    RunStatus, RunnerConfig, VerificationPolicy, WorkBudget, WorkCliDeps, WorkEvent, WorkItem,
    WorkItemKind, WorkItemStatus, WorkProgress, WorkProject, WorkRun, WorkStore, WorkSubmission,
    WorkToolCtx, JOURNAL_FILE,
};
use serde_json::json;

// ------------------------------------------------------------------ Helfer

fn tmp_dir(name: &str) -> std::path::PathBuf {
    static NR: AtomicUsize = AtomicUsize::new(0);
    let nr = NR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_hardening_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(store: &WorkStore, budget: WorkBudget) {
    store
        .submit(WorkEvent::ProjectCreated {
            project: WorkProject {
                id: "demo".into(),
                title: "Demo".into(),
                objective: "Teste die Härtung.".into(),
                workspace: ".".into(),
                status: ProjectStatus::Active,
                created_at_ms: 0,
                budget,
                git_isolation: false,
            },
        })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted {
            run: WorkRun {
                id: "R-1".into(),
                project_id: "demo".into(),
                status: RunStatus::Running,
                started_at_ms: 0,
                completed_at_ms: None,
                base_revision: None,
                base_branch: None,
                completion_reason: None,
            },
        })
        .unwrap();
}

fn item(id: &str, seq: u64, policy: VerificationPolicy) -> WorkItem {
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
        verification_policy: policy,
        verifies: None,
        claims_promoted: false,
        executor: ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn verify_policy() -> VerificationPolicy {
    VerificationPolicy::AutomatedTests {
        command: "cargo --version".to_string(),
    }
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

/// Test-Doppelgänger: ruft (optional) `work_submit`, antwortet mit `answer`
/// und entscheidet die Freigabe eines Modell-Prüfkommandos mit `gate` —
/// `None` lässt den Default des Traits greifen.
struct Exec {
    answer: &'static str,
    submit: bool,
    gate: Option<Result<(), String>>,
    asked: Mutex<Vec<String>>,
}

impl Exec {
    fn new(answer: &'static str, submit: bool, gate: Option<Result<(), String>>) -> Self {
        Exec {
            answer,
            submit,
            gate,
            asked: Mutex::new(Vec::new()),
        }
    }
}

impl AgentExecutor for Exec {
    fn execute(
        &self,
        _pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        _store: Arc<WorkStore>,
        _on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        if self.submit {
            *ctx.submission.lock().unwrap() = Some(WorkSubmission {
                summary: "Alles fertig.".to_string(),
                criteria: vec![],
            });
        }
        Ok(self.answer.to_string())
    }

    fn approve_verify_command(&self, command: &str) -> Result<(), String> {
        self.asked.lock().unwrap().push(command.to_string());
        match &self.gate {
            Some(gate) => gate.clone(),
            None => Err("dieser Executor kann keine Freigabe einholen".to_string()),
        }
    }
}

fn coding_executor(dry_run: bool, approve: bool) -> CodingAgentExecutor {
    CodingAgentExecutor {
        llm: Arc::new(FakeLlm::new(vec![])),
        approve: Arc::new(move |_: &str| approve),
        extra_tools: None,
        cancel: new_cancel(),
        dry_run,
        shell_timeout: 30,
        project_instructions: false,
        system_extra: None,
        agent_setup: None,
        protect_paths: Vec::new(),
        allow_read: Vec::new(),
        strategy: agentkit::RunStrategy::default(),
    }
}

// ------------------------------------------- Submission sticht den Sentinel

/// Ein Agent, der `work_submit` in seinem letzten erlaubten Schritt ruft,
/// endet im Kern mit "(max_steps erreicht)". Seine fertige Arbeit zählt
/// trotzdem — vorher wurde sie als Fehlschlag gebucht und verworfen.
#[test]
fn work_submit_im_letzten_schritt_zaehlt_trotz_max_steps_sentinel() {
    let ws = tmp_dir("submit_vor_sentinel");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, VerificationPolicy::None),
        })
        .unwrap();

    let executor = Exec::new("(max_steps erreicht)", true, None);
    let outcome = run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&ws),
        &new_cancel(),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(state.items["W-1"].attempt_count, 0);
    std::fs::remove_dir_all(&ws).ok();
}

// ------------------------------------------- Prüfkommando des Modells

fn store_with_agent_item() -> Arc<WorkStore> {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemAddedByAgent {
            item: item("W-1", 1, verify_policy()),
            attempt: "A-0".to_string(),
        })
        .unwrap();
    store
}

#[test]
fn pruefkommando_des_modells_laeuft_ohne_freigabe_nicht_und_das_item_schliesst_mit_vermerk_ab() {
    let ws = tmp_dir("verify_abgelehnt");
    let store = store_with_agent_item();

    let executor = Exec::new(
        "Alles fertig.",
        true,
        Some(Err("Freigabe abgelehnt".to_string())),
    );
    let mut notes = Vec::new();
    let outcome = run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&ws),
        &new_cancel(),
        &mut |p| {
            if let WorkProgress::Note(msg) = p {
                notes.push(msg);
            }
        },
    )
    .unwrap();

    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(
        *executor.asked.lock().unwrap(),
        vec!["cargo --version".to_string()],
        "die Freigabe bekommt das Kommando unverändert"
    );
    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(
        state.attempts["A-1"].verification, None,
        "das Kommando darf nicht gelaufen sein"
    );
    let summary = state.attempts["A-1"].summary.clone().unwrap();
    assert!(
        summary.contains("NICHT ausgeführt") && summary.contains("Freigabe abgelehnt"),
        "{summary}"
    );
    assert!(
        notes.iter().any(|n| n.contains("NICHT ausgeführt")),
        "{notes:?}"
    );
    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn pruefkommando_des_modells_laeuft_nach_freigabe() {
    let ws = tmp_dir("verify_freigegeben");
    let store = store_with_agent_item();

    let executor = Exec::new("Alles fertig.", true, Some(Ok(())));
    run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&ws),
        &new_cancel(),
        &mut |_| {},
    )
    .unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(
        state.attempts["A-1"].verification,
        Some(AttemptVerification::Approved {
            by: "automated_tests".to_string(),
            reason: None,
        })
    );
    std::fs::remove_dir_all(&ws).ok();
}

/// Ein Executor, der die Freigabe-Methode nicht überschreibt, winkt ein
/// Modell-Kommando nicht durch.
#[test]
fn executor_ohne_eigene_freigabe_lehnt_ein_pruefkommando_des_modells_ab() {
    struct Plain;
    impl AgentExecutor for Plain {
        fn execute(
            &self,
            _pkg: &AgentWorkPackage,
            _ctx: WorkToolCtx,
            _store: Arc<WorkStore>,
            _on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<String, String> {
            Ok("Alles fertig.".to_string())
        }
    }
    let ws = tmp_dir("verify_default");
    let store = store_with_agent_item();

    run_to_completion(&store, "R-1", &Plain, &cfg(&ws), &new_cancel(), &mut |_| {}).unwrap();

    let state = store.snapshot();
    assert_eq!(state.items["W-1"].status, WorkItemStatus::Completed);
    assert_eq!(state.attempts["A-1"].verification, None);
    std::fs::remove_dir_all(&ws).ok();
}

/// Die Usability-Seite: ein Kommando, das der BEDIENER gesetzt hat, läuft wie
/// bisher — die Freigabe wird gar nicht erst gefragt.
#[test]
fn pruefkommando_des_bedieners_laeuft_ohne_rueckfrage() {
    let ws = tmp_dir("verify_bediener");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, verify_policy()),
        })
        .unwrap();

    let executor = Exec::new(
        "Alles fertig.",
        true,
        Some(Err("Freigabe abgelehnt".to_string())),
    );
    run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&ws),
        &new_cancel(),
        &mut |_| {},
    )
    .unwrap();

    assert!(executor.asked.lock().unwrap().is_empty());
    let state = store.snapshot();
    assert_eq!(
        state.attempts["A-1"].verification,
        Some(AttemptVerification::Approved {
            by: "automated_tests".to_string(),
            reason: None,
        })
    );
    std::fs::remove_dir_all(&ws).ok();
}

#[test]
fn coding_agent_executor_gibt_nur_mit_freigabe_und_ohne_dry_run_frei() {
    assert_eq!(
        coding_executor(false, true).approve_verify_command("cargo test"),
        Ok(())
    );
    assert_eq!(
        coding_executor(false, false).approve_verify_command("cargo test"),
        Err("Freigabe abgelehnt".to_string())
    );
    assert_eq!(
        coding_executor(true, true).approve_verify_command("cargo test"),
        Err("--dry-run".to_string())
    );
}

/// `work_add_item` journalt die Herkunft, und sie übersteht Neustart UND
/// Checkpoint (Snapshot-Zeile) — sonst gälte ein Modell-Kommando nach dem
/// nächsten Resume als Bediener-Kommando.
#[test]
fn herkunft_aus_work_add_item_uebersteht_checkpoint_und_neustart() {
    let dir = tmp_dir("herkunft");
    {
        let store = Arc::new(WorkStore::open(&dir).unwrap());
        setup(&store, WorkBudget::default());
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item("W-1", 1, VerificationPolicy::None),
            })
            .unwrap();
        let ctx = tool_ctx(&dir);
        let mut tools = ToolRegistry::new();
        register_work_tools(&mut tools, store.clone(), ctx);
        let raw = tools
            .call(
                "work_add_item",
                json!({"title": "T", "description": "D", "kind": "test",
                       "verify_command": "cargo --version"}),
            )
            .unwrap();
        assert!(!raw.starts_with("ERROR"), "{raw}");
        assert!(store.snapshot().is_agent_added("W-2"));
        assert!(!store.snapshot().is_agent_added("W-1"));
    }
    {
        let store = WorkStore::open(&dir).unwrap();
        assert!(store.snapshot().is_agent_added("W-2"), "nach Replay");
        store.checkpoint().unwrap();
    }
    let state = WorkStore::open_read_only(&dir).unwrap();
    assert!(state.is_agent_added("W-2"), "nach Snapshot-Zeile");
    assert!(!state.is_agent_added("W-1"));
    std::fs::remove_dir_all(&dir).ok();
}

fn tool_ctx(dir: &std::path::Path) -> WorkToolCtx {
    WorkToolCtx {
        run_id: "R-1".to_string(),
        work_item_id: "W-1".to_string(),
        attempt_id: "A-1".to_string(),
        agent_id: "agent-1".to_string(),
        max_attempts: 3,
        project_id: "demo".to_string(),
        repository_revision: None,
        artifacts_dir: dir.join("artifacts"),
        submission: Arc::new(Mutex::new(None)),
        gateway: None,
        verifies: None,
    }
}

// ------------------------------------------------------------ Item-Budget

#[test]
fn work_add_item_lehnt_ab_sobald_max_work_items_erreicht_ist() {
    let dir = tmp_dir("max_items");
    let store = Arc::new(WorkStore::in_memory());
    setup(
        &store,
        WorkBudget {
            max_work_items: Some(2),
            ..WorkBudget::default()
        },
    );
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, VerificationPolicy::None),
        })
        .unwrap();
    let mut tools = ToolRegistry::new();
    register_work_tools(&mut tools, store.clone(), tool_ctx(&dir));
    let add = |title: &str| {
        tools
            .call(
                "work_add_item",
                json!({"title": title, "description": "D", "kind": "test"}),
            )
            .unwrap()
    };

    assert!(!add("zweites").starts_with("ERROR"));
    let third = add("drittes");
    assert!(
        third.starts_with("ERROR") && third.contains("max_work_items (2)"),
        "{third}"
    );
    assert_eq!(store.snapshot().items.len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------------------------ Journal

fn journal_with_one_item(dir: &std::path::Path) -> std::path::PathBuf {
    let store = WorkStore::open(dir).unwrap();
    setup(&store, WorkBudget::default());
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1, VerificationPolicy::None),
        })
        .unwrap();
    dir.join(JOURNAL_FILE)
}

/// Absturz zwischen Zeile und Umbruch (altes Journal): die letzte Zeile ist
/// vollständig, endet aber ohne `\n`. Das nächste Ereignis darf nicht daran
/// kleben — sonst wäre das Journal ab dem übernächsten dauerhaft unlesbar.
#[test]
fn vollstaendige_letzte_zeile_ohne_umbruch_wird_beim_oeffnen_ergaenzt() {
    let dir = tmp_dir("ohne_umbruch");
    let path = journal_with_one_item(&dir);
    let content = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, content.trim_end_matches('\n')).unwrap();

    for id in ["W-2", "W-3"] {
        let store = WorkStore::open(&dir).unwrap();
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item(id, 2, VerificationPolicy::None),
            })
            .unwrap();
    }

    let state = WorkStore::open(&dir).unwrap().snapshot();
    assert_eq!(state.items.len(), 3);
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content.lines().count(), 5);
    assert!(content.ends_with('\n'));
    std::fs::remove_dir_all(&dir).ok();
}

/// Die Reparatur einer abgeschnittenen letzten Zeile lässt die gültigen Bytes
/// davor exakt stehen (sie kürzt, statt die Datei neu zu schreiben).
#[test]
fn reparatur_einer_abgeschnittenen_zeile_laesst_die_gueltigen_bytes_unveraendert() {
    let dir = tmp_dir("reparatur");
    let path = journal_with_one_item(&dir);
    let valid = std::fs::read(&path).unwrap();
    let mut broken = valid.clone();
    broken.extend_from_slice(br#"{"schema_version":"1","seq":4,"at":0,"ev"#);
    std::fs::write(&path, broken).unwrap();

    drop(WorkStore::open(&dir).unwrap());

    assert_eq!(std::fs::read(&path).unwrap(), valid);
    std::fs::remove_dir_all(&dir).ok();
}

/// Eine Zeile, deren `seq` nicht hinter ihrem Vorgänger liegt, hat nicht der
/// Store geschrieben — das Öffnen lehnt sie ab, statt sie anzuwenden.
#[test]
fn ereignis_mit_nicht_aufsteigender_seq_wird_beim_oeffnen_abgelehnt() {
    let dir = tmp_dir("seq");
    let path = journal_with_one_item(&dir);
    let content = std::fs::read_to_string(&path).unwrap();
    let injected = json!({
        "schema_version": "1", "seq": 2, "at": 0,
        "event": {"kind": "budget_updated", "at_ms": 0, "budget": {
            "max_wall_time_secs": null, "max_work_items": null,
            "max_attempts_per_item": 99, "max_steps_per_attempt": 999,
            "max_parallel_agents": 1
        }}
    });
    std::fs::write(&path, format!("{content}{injected}\n")).unwrap();

    let err = WorkStore::open(&dir).unwrap_err().to_string();
    assert!(err.contains("seq 2"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------- Schutz von Journal und Sperre

/// Ein Item-Agent, der das Journal mit `write_file` überschreiben will, wird
/// abgewiesen: das Journal bleibt lesbar und unverändert in seinem Inhalt.
#[test]
fn item_agent_kann_das_journal_nicht_mit_write_file_ueberschreiben() {
    let ws = tmp_dir("journal_schutz");
    let ws_str = ws.to_string_lossy().to_string();
    let run_cli = |argv: &[&str], llm: Arc<dyn Llm>| {
        let f: &'static dyn Fn(&str, bool) -> Arc<dyn Llm> =
            Box::leak(Box::new(move |_p: &str, _demo: bool| llm.clone()));
        let deps = WorkCliDeps {
            llm: f,
            approve: Arc::new(|_: &str| false),
            extra_tools: None,
            cancel: new_cancel(),
            graph: None,
            build_executor: None,
            system_extra: None,
            agent_setup: None,
            protect_paths: Vec::new(),
            allow_read: Vec::new(),
            trace: None,
        };
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = dispatch_with_io(&argv, deps, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap())
    };

    let (code, out) = run_cli(
        &[
            "create",
            "--title",
            "Schutz",
            "--objective",
            "Teste den Schutz.",
            "-w",
            &ws_str,
        ],
        Arc::new(FakeLlm::new(vec![])),
    );
    assert_eq!(code, ExitCode::Success);
    let project_id = out.trim().to_string();
    let rel_journal = format!(".agentkit/work/{project_id}/{JOURNAL_FILE}");

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "write_file",
            &json!({"path": rel_journal, "content": "kaputt"}).to_string(),
        )],
        vec![Chunk::text("Fertig.")],
    ]));
    run_cli(&["run", &project_id, "-w", &ws_str, "-y"], llm);

    let journal = std::fs::read_to_string(ws.join(&rel_journal)).unwrap();
    assert!(!journal.contains("kaputt"), "{journal}");
    WorkStore::open_read_only(ws.join(".agentkit/work").join(&project_id))
        .expect("Journal bleibt lesbar");
    std::fs::remove_dir_all(&ws).ok();
}
