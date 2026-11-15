//! Git-Isolation mit Abhängigkeiten als Spezifikation (Issue #79): der Branch
//! eines abhängigen Items startet auf dem Stand seiner Vorgänger, ein Konflikt
//! zwischen den Vorgängern scheitert früh und sauber, der Rollback behält die
//! Vorgänger, und die Integration am Ende mergt nichts doppelt. Dazu die zwei
//! Ablehnungen, die Datenverlust verhindern: uncommittete Änderungen neben dem
//! Workspace und ein losgelöster HEAD. Alle Tests arbeiten auf echten,
//! temporären Git-Repositories.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit::testing::FakeLlm;
use agentkit::{new_cancel, AgentEvent, ExitCode, Llm};
use agentkit_work::{
    dispatch_with_io, run_to_completion, AgentExecutor, AgentWorkPackage, CompletionReason,
    ExecutorKind, FailureKind, ProjectStatus, RunStatus, RunnerConfig, VerificationPolicy,
    WorkBudget, WorkCliDeps, WorkEvent, WorkItem, WorkItemKind, WorkItemStatus, WorkProject,
    WorkRun, WorkStore, WorkSubmission, WorkToolCtx,
};

// ------------------------------------------------------------------ Helfer

fn tmp_dir(name: &str) -> PathBuf {
    static NR: AtomicUsize = AtomicUsize::new(0);
    let nr = NR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_gitdeps_{name}_{}_{nr}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Frisches Repo mit einem Commit auf `main`; gibt Verzeichnis und Commit zurück.
fn git_repo(name: &str) -> (PathBuf, String) {
    let dir = tmp_dir(name);
    let base = agentkit_work::git::init_repo_with_commit(&dir.to_string_lossy());
    (dir, base)
}

fn setup(store: &WorkStore, base_revision: &str) {
    store
        .submit(WorkEvent::ProjectCreated {
            project: WorkProject {
                id: "demo".into(),
                title: "Demo".into(),
                objective: "Teste die Git-Isolation.".into(),
                workspace: ".".into(),
                status: ProjectStatus::Active,
                created_at_ms: 0,
                budget: WorkBudget::default(),
                git_isolation: true,
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
                base_revision: Some(base_revision.to_string()),
                base_branch: Some("main".into()),
                completion_reason: None,
            },
        })
        .unwrap();
}

fn add_item(store: &WorkStore, id: &str, kind: WorkItemKind, deps: &[&str]) {
    let seq = agentkit_work::id_order(id);
    store
        .submit(WorkEvent::WorkItemCreated {
            item: WorkItem {
                id: id.into(),
                run_id: "R-1".into(),
                title: format!("Item {id}"),
                description: "Beschreibung".into(),
                kind,
                status: WorkItemStatus::Pending,
                priority: 5,
                seq,
                required_role: None,
                dependencies: deps.iter().map(|d| d.to_string()).collect(),
                acceptance_criteria: vec![],
                verification_policy: VerificationPolicy::None,
                verifies: None,
                claims_promoted: false,
                executor: ExecutorKind::SingleAgent,
                attempt_count: 0,
                max_attempts: 2,
                updated_at_ms: 0,
            },
        })
        .unwrap();
}

fn add(store: &WorkStore, id: &str, deps: &[&str]) {
    add_item(store, id, WorkItemKind::Implementation, deps);
}

fn cfg(workspace: &Path) -> RunnerConfig {
    RunnerConfig {
        agent_id: "worker-1".to_string(),
        lease_secs: 600,
        heartbeat_secs: 30,
        workspace: workspace.to_string_lossy().to_string(),
        graph: None,
    }
}

/// Ein Schritt bekommt den Workspace; `Ok` schließt den Versuch mit
/// `work_submit` ab, `Err` ist ein Modellfehler (gescheiterter Versuch).
type Step = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// Test-Doppelgänger: je Item eine Folge von Schritten, einer je Versuch —
/// nach Item-ID adressiert, damit ein Test nicht von der Reihenfolge des
/// Schedulers abhängt.
struct PerItem {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    calls: Mutex<Vec<String>>,
}

impl PerItem {
    fn new(steps: Vec<(&str, Vec<Step>)>) -> Self {
        PerItem {
            steps: Mutex::new(
                steps
                    .into_iter()
                    .map(|(id, s)| (id.to_string(), s.into()))
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl AgentExecutor for PerItem {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        _store: Arc<WorkStore>,
        _on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        self.calls.lock().unwrap().push(pkg.item.id.clone());
        let step = self
            .steps
            .lock()
            .unwrap()
            .get_mut(&pkg.item.id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("kein Schritt für {}", pkg.item.id));
        step(Path::new(&pkg.workspace))?;
        *ctx.submission.lock().unwrap() = Some(WorkSubmission {
            summary: format!("{} fertig.", pkg.item.id),
            criteria: vec![],
        });
        Ok("fertig".to_string())
    }
}

fn write(file: &'static str, content: &'static str) -> Step {
    Box::new(move |ws| {
        std::fs::write(ws.join(file), content).unwrap();
        Ok(())
    })
}

/// Prüft, dass alle `present` existieren, und schreibt dann `file`.
fn expect_then_write(present: &'static [&'static str], file: &'static str) -> Step {
    Box::new(move |ws| {
        for p in present {
            assert!(ws.join(p).exists(), "'{p}' fehlt im Arbeitsbaum");
        }
        std::fs::write(ws.join(file), file).unwrap();
        Ok(())
    })
}

fn run(store: &Arc<WorkStore>, executor: &PerItem, ws: &Path) -> CompletionReason {
    run_to_completion(store, "R-1", executor, &cfg(ws), &new_cancel(), &mut |_| {})
        .unwrap()
        .reason
}

fn integration_status(store: &WorkStore) -> WorkItemStatus {
    store
        .snapshot()
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Integration)
        .expect("Integrations-Item wurde angelegt")
        .status
}

// ------------------------------------------------------------------- Kette

/// B hängt an A, C an B: B startet EXAKT auf dem Stand von A (kein eigener
/// Merge-Commit), und C sieht über B auch A.
#[test]
fn kette_nachfolger_startet_auf_dem_stand_des_vorgaengers_und_sieht_ihn_transitiv() {
    let (dir, base) = git_repo("kette");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add(&store, "W-2", &["W-1"]);
    add(&store, "W-3", &["W-2"]);

    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "a")]),
        (
            "W-2",
            vec![Box::new(|ws| {
                assert!(ws.join("a.txt").exists(), "B muss den Code von A sehen");
                assert_eq!(
                    git(ws, &["rev-parse", "HEAD"]),
                    git(ws, &["rev-parse", "work/demo/W-1"]),
                    "mit genau einem Vorgänger startet der Branch auf dessen Stand"
                );
                std::fs::write(ws.join("b.txt"), "b").unwrap();
                Ok(())
            })],
        ),
        ("W-3", vec![expect_then_write(&["a.txt", "b.txt"], "c.txt")]),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::AllItemsDone);
    assert_eq!(integration_status(&store), WorkItemStatus::Completed);
    for f in ["a.txt", "b.txt", "c.txt"] {
        assert!(dir.join(f).exists(), "'{f}' fehlt nach der Integration");
    }
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    std::fs::remove_dir_all(&dir).ok();
}

/// Ohne Abhängigkeit bleibt alles wie bisher: der Branch startet auf
/// `base_revision` und sieht ein unabhängiges, früher fertiges Item NICHT.
#[test]
fn item_ohne_abhaengigkeit_startet_weiter_auf_base_revision() {
    let (dir, base) = git_repo("ohne_dep");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add(&store, "W-2", &[]);

    let expected = base.clone();
    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "a")]),
        (
            "W-2",
            vec![Box::new(move |ws| {
                assert_eq!(git(ws, &["rev-parse", "HEAD"]), expected);
                assert!(!ws.join("a.txt").exists());
                std::fs::write(ws.join("b.txt"), "b").unwrap();
                Ok(())
            })],
        ),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::AllItemsDone);
    std::fs::remove_dir_all(&dir).ok();
}

/// Durch eine nicht isolierte Abhängigkeit (Review hat keinen Branch) wird
/// hindurchgegriffen: C hängt nur am Review, sieht aber den geprüften Code.
#[test]
fn durch_ein_review_item_hindurch_sieht_der_nachfolger_den_code() {
    let (dir, base) = git_repo("durchgriff");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add_item(&store, "W-2", WorkItemKind::Review, &["W-1"]);
    add(&store, "W-3", &["W-2"]);

    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "a")]),
        ("W-2", vec![Box::new(|_| Ok(()))]),
        ("W-3", vec![expect_then_write(&["a.txt"], "c.txt")]),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::AllItemsDone);
    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------------- Diamant

/// A → {B, C} → D: D sieht alle drei Vorgänger. Die Integration mergt danach
/// A, B, C und D der Reihe nach — B und C stecken schon in D, A in allen —
/// und das bleibt konfliktfrei, auch wenn D eine Datei von A weiter ändert.
#[test]
fn diamant_sieht_alle_vorgaenger_und_die_integration_mergt_nichts_widerspruechlich() {
    let (dir, base) = git_repo("diamant");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add(&store, "W-2", &["W-1"]);
    add(&store, "W-3", &["W-1"]);
    add(&store, "W-4", &["W-2", "W-3"]);

    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "von A\n")]),
        ("W-2", vec![expect_then_write(&["a.txt"], "b.txt")]),
        ("W-3", vec![expect_then_write(&["a.txt"], "c.txt")]),
        (
            "W-4",
            vec![Box::new(|ws| {
                for f in ["a.txt", "b.txt", "c.txt"] {
                    assert!(ws.join(f).exists(), "'{f}' fehlt für D");
                }
                std::fs::write(ws.join("a.txt"), "von A, erweitert von D\n").unwrap();
                std::fs::write(ws.join("d.txt"), "d").unwrap();
                Ok(())
            })],
        ),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::AllItemsDone);
    assert_eq!(integration_status(&store), WorkItemStatus::Completed);
    for f in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        assert!(dir.join(f).exists(), "'{f}' fehlt nach der Integration");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "von A, erweitert von D\n"
    );
    assert!(agentkit_work::git::is_clean(&dir.to_string_lossy()).unwrap());
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------- Konflikt

/// Zwei Vorgänger ändern dieselbe Zeile: das abhängige Item scheitert VOR
/// jedem Agentenlauf mit `MergeConflict` und der betroffenen Datei, der
/// Arbeitsbaum steht sauber auf dem Ausgangsbranch, der Lauf endet blockiert.
#[test]
fn konflikt_zwischen_vorgaengern_scheitert_frueh_und_hinterlaesst_einen_sauberen_arbeitsbaum() {
    let (dir, base) = git_repo("vorab_konflikt");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add(&store, "W-2", &[]);
    add(&store, "W-3", &["W-1", "W-2"]);

    let executor = PerItem::new(vec![
        ("W-1", vec![write("readme.txt", "geändert von W-1\n")]),
        ("W-2", vec![write("readme.txt", "geändert von W-2\n")]),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::Blocked);
    assert_eq!(
        executor.calls(),
        vec!["W-1", "W-2"],
        "für W-3 darf kein Agent laufen"
    );

    let state = store.snapshot();
    assert_eq!(state.items["W-3"].status, WorkItemStatus::Failed);
    let failure = state
        .attempts
        .values()
        .find(|a| a.work_item_id == "W-3")
        .and_then(|a| a.failure.clone())
        .expect("der Versuch trägt eine Fehlerursache");
    assert_eq!(failure.kind, FailureKind::MergeConflict);
    assert!(
        failure.message.contains("readme.txt"),
        "{}",
        failure.message
    );

    assert!(agentkit_work::git::is_clean(&dir.to_string_lossy()).unwrap());
    assert_eq!(git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert!(
        !std::fs::read_to_string(dir.join("readme.txt"))
            .unwrap()
            .contains("<<<<<<<"),
        "kein Konfliktmarker im Arbeitsbaum"
    );
    assert_eq!(
        git(&dir, &["rev-parse", "work/demo/W-3"]),
        git(&dir, &["rev-parse", "work/demo/W-1"]),
        "der Item-Branch trägt nur den konfliktfreien Vorgänger, keinen halben Merge"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------- Rollback

/// Ein gescheiterter Versuch verwirft SEINE Änderungen, nicht die der
/// Vorgänger: der zweite Versuch sieht A weiterhin.
#[test]
fn rollback_eines_fehlversuchs_behaelt_die_vorgaenger() {
    let (dir, base) = git_repo("rollback_dep");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);
    add(&store, "W-2", &["W-1"]);

    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "a")]),
        (
            "W-2",
            vec![
                Box::new(|ws| {
                    std::fs::write(ws.join("halb.txt"), "halb fertig").unwrap();
                    Err("boom".to_string())
                }),
                Box::new(|ws| {
                    assert!(ws.join("a.txt").exists(), "A ist nach dem Rollback noch da");
                    assert!(
                        !ws.join("halb.txt").exists(),
                        "der Fehlversuch ist verworfen"
                    );
                    std::fs::write(ws.join("b.txt"), "b").unwrap();
                    Ok(())
                }),
            ],
        ),
    ]);

    assert_eq!(run(&store, &executor, &dir), CompletionReason::AllItemsDone);
    assert_eq!(store.snapshot().items["W-2"].attempt_count, 1);
    assert!(dir.join("a.txt").exists() && dir.join("b.txt").exists());
    assert!(!dir.join("halb.txt").exists());
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------- Workspace als Repo-Unterordner

/// Repo mit Unterordner `ws/` (der Workspace) und einer Datei daneben.
fn repo_with_subdir_workspace(name: &str) -> (PathBuf, PathBuf, String) {
    let dir = tmp_dir(name);
    agentkit_work::git::init_repo_with_commit(&dir.to_string_lossy());
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::write(dir.join("ws/in.txt"), "im Workspace\n").unwrap();
    std::fs::write(dir.join("nachbar.txt"), "daneben\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@localhost",
            "commit",
            "-q",
            "-m",
            "Unterordner",
        ],
    );
    let base = git(&dir, &["rev-parse", "HEAD"]);
    let ws = dir.join("ws");
    (dir, ws, base)
}

/// Eine uncommittete Änderung NEBEN dem Workspace wird abgelehnt, bevor ein
/// Versuch läuft — der Rollback (`git reset --hard`) hätte sie gelöscht.
#[test]
fn uncommittete_aenderung_neben_dem_workspace_wird_abgelehnt_und_bleibt_erhalten() {
    let (dir, ws, base) = repo_with_subdir_workspace("nachbar_schmutzig");
    std::fs::write(dir.join("nachbar.txt"), "fremde, uncommittete Arbeit\n").unwrap();
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);

    let executor = PerItem::new(vec![]);
    let err = run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&ws),
        &new_cancel(),
        &mut |_| {},
    )
    .err()
    .expect("der Lauf muss abgelehnt werden")
    .to_string();

    assert!(err.contains("AUSSERHALB des Workspace"), "{err}");
    assert!(executor.calls().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.join("nachbar.txt")).unwrap(),
        "fremde, uncommittete Arbeit\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Die Usability-Seite: ein Workspace als Unterordner bleibt erlaubt, solange
/// daneben nichts Uncommittetes liegt — auch ein Fehlversuch samt Rollback.
#[test]
fn workspace_als_unterordner_laeuft_wenn_das_repo_daneben_sauber_ist() {
    let (dir, ws, base) = repo_with_subdir_workspace("nachbar_sauber");
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);

    let executor = PerItem::new(vec![(
        "W-1",
        vec![
            Box::new(|ws| {
                std::fs::write(ws.join("halb.txt"), "halb").unwrap();
                Err("boom".to_string())
            }),
            write("neu.txt", "neu"),
        ],
    )]);

    assert_eq!(run(&store, &executor, &ws), CompletionReason::AllItemsDone);
    assert!(ws.join("neu.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("nachbar.txt")).unwrap(),
        "daneben\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------------- Losgelöster HEAD

#[test]
fn losgeloester_head_wird_vor_dem_versuch_abgelehnt() {
    let (dir, base) = git_repo("detached_run");
    git(&dir, &["checkout", "-q", "--detach"]);
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, &base);
    add(&store, "W-1", &[]);

    let executor = PerItem::new(vec![]);
    let err = run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg(&dir),
        &new_cancel(),
        &mut |_| {},
    )
    .err()
    .expect("der Lauf muss abgelehnt werden")
    .to_string();

    assert!(err.contains("losgelösten HEAD"), "{err}");
    assert!(executor.calls().is_empty());
    assert_eq!(
        git(&dir, &["branch", "--list", "work/*"]),
        "",
        "es darf kein Item-Branch entstanden sein"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_mit_git_isolation_lehnt_einen_losgeloesten_head_ab() {
    let (dir, _base) = git_repo("detached_create");
    git(&dir, &["checkout", "-q", "--detach"]);
    let ws = dir.to_string_lossy().to_string();

    let llm: Arc<dyn Llm> = Arc::new(FakeLlm::new(vec![]));
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
    let argv: Vec<String> = [
        "create",
        "--title",
        "Detached",
        "--objective",
        "Teste die Ablehnung.",
        "-w",
        &ws,
        "--git-isolation",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch_with_io(&argv, deps, &mut out, &mut err);

    assert_eq!(code, ExitCode::GeneralError);
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("losgelösten HEAD"), "{err}");
    assert!(
        !dir.join(".agentkit").exists(),
        "es darf kein Projektverzeichnis entstehen"
    );
    std::fs::remove_dir_all(&dir).ok();
}
