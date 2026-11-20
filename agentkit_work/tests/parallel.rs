//! Mehrere Worker als Spezifikation: was gleichzeitig läuft, was trotz freiem
//! Worker wartet, und was ein Abbruch, ein Fehler oder ein erschöpftes Budget
//! mit den Versuchen macht, die gerade laufen.
//!
//! Die Tests beweisen Gleichzeitigkeit über einen Treffpunkt ([`Meet`]): ein
//! Schritt wartet dort, bis die erwartete Zahl von Versuchen angekommen ist.
//! Kommen sie nie zusammen, läuft das Warten in ein Zeitlimit und der Test
//! scheitert — er hängt nicht. Nur wo ein Test zeigt, dass ein weiterer
//! Versuch NICHT dazukommt, wartet er ein kurzes, festes Fenster ab.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agentkit::{new_cancel, AgentEvent, Cancel};
use agentkit_work::{
    run_to_completion, AgentExecutor, AgentWorkPackage, CompletionReason, ExecutorKind,
    ProjectStatus, RunOutcome, RunStatus, RunnerConfig, VerificationPolicy, WorkBudget, WorkError,
    WorkEvent, WorkItem, WorkItemKind, WorkItemStatus, WorkProgress, WorkProject, WorkRun,
    WorkStore, WorkSubmission, WorkToolCtx,
};

// ------------------------------------------------------------------ Helfer

/// Obergrenze für „die Versuche treffen sich" — großzügig, damit eine
/// ausgelastete CI-Maschine keinen Fehlalarm erzeugt. Im Erfolgsfall wartet
/// niemand so lange.
const MEET_TIMEOUT: Duration = Duration::from_secs(20);
/// Wartezeit auf einen Versuch, der NICHT kommen darf.
const ABSENCE_WINDOW: Duration = Duration::from_millis(150);

/// Treffpunkt: zählt, wie viele Versuche gerade hier sind, und merkt sich
/// den Höchststand.
#[derive(Default)]
struct Meet {
    /// (gerade anwesend, bisheriger Höchststand)
    counts: Mutex<(usize, usize)>,
    changed: Condvar,
}

impl Meet {
    /// Kommt an und wartet, bis einmal `want` Versuche gleichzeitig hier
    /// waren (oder `timeout` abläuft), und geht dann wieder. Gibt den
    /// Höchststand zurück. Der Höchststand statt der aktuellen Zahl: wer
    /// zuletzt ankommt, sieht die volle Zahl sofort und geht, bevor die
    /// Wartenden aufwachen.
    fn stay(&self, want: usize, timeout: Duration) -> usize {
        let mut counts = self.counts.lock().unwrap();
        counts.0 += 1;
        counts.1 = counts.1.max(counts.0);
        self.changed.notify_all();
        let deadline = std::time::Instant::now() + timeout;
        while counts.1 < want {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            counts = self.changed.wait_timeout(counts, left).unwrap().0;
        }
        counts.0 -= 1;
        counts.1
    }
}

type Step = Box<dyn Fn(&AgentWorkPackage, &Arc<WorkStore>) -> Result<String, String> + Send + Sync>;

/// Test-Doppelgänger: je Item eine Folge von Schritten, einer je Versuch —
/// nach Item-ID adressiert, weil die Reihenfolge paralleler Versuche nicht
/// feststeht. Ein Schritt, der `Ok` liefert, gilt als `work_submit`.
struct PerItem {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
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
        }
    }
}

impl AgentExecutor for PerItem {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        // Die Sperre nur fürs Herausnehmen halten — sonst liefen die Schritte
        // selbst nacheinander.
        let step = self
            .steps
            .lock()
            .unwrap()
            .get_mut(&pkg.item.id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("kein Schritt für {}", pkg.item.id));
        on_event(&AgentEvent::new(
            agentkit::STEP,
            agentkit::EventData::Step { step: 1 },
        ));
        let answer = step(pkg, &store)?;
        if answer != "(abgebrochen)" {
            *ctx.submission.lock().unwrap() = Some(WorkSubmission {
                summary: answer.clone(),
                criteria: vec![],
            });
        }
        Ok(answer)
    }
}

fn ok() -> Step {
    Box::new(|pkg, _| Ok(format!("{} fertig.", pkg.item.id)))
}

/// Wartet am Treffpunkt auf `want` gleichzeitige Versuche und scheitert,
/// wenn sie nicht zusammenkommen.
fn meet(meet: &Arc<Meet>, want: usize) -> Step {
    let meet = meet.clone();
    Box::new(move |pkg, _| {
        let seen = meet.stay(want, MEET_TIMEOUT);
        if seen < want {
            return Err(format!(
                "{}: nur {seen} von {want} Versuchen gleichzeitig",
                pkg.item.id
            ));
        }
        Ok(format!("{} fertig.", pkg.item.id))
    })
}

/// Bleibt kurz am Treffpunkt stehen und meldet, wie viele Versuche
/// gleichzeitig da waren — für Tests, die prüfen, dass KEIN weiterer kommt.
fn linger(meet: &Arc<Meet>, most: &Arc<Mutex<usize>>) -> Step {
    let (meet, most) = (meet.clone(), most.clone());
    Box::new(move |pkg, _| {
        let seen = meet.stay(usize::MAX, ABSENCE_WINDOW);
        let mut most = most.lock().unwrap();
        *most = (*most).max(seen);
        Ok(format!("{} fertig.", pkg.item.id))
    })
}

fn budget(parallel: u32) -> WorkBudget {
    WorkBudget {
        max_parallel_agents: parallel,
        ..WorkBudget::default()
    }
}

fn setup(store: &WorkStore, budget: WorkBudget, git: Option<&str>) {
    store
        .submit(WorkEvent::ProjectCreated {
            project: WorkProject {
                id: "demo".into(),
                title: "Demo".into(),
                objective: "Teste die parallelen Worker.".into(),
                workspace: ".".into(),
                status: ProjectStatus::Active,
                created_at_ms: 0,
                budget,
                git_isolation: git.is_some(),
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
                base_revision: git.map(String::from),
                base_branch: None,
                completion_reason: None,
            },
        })
        .unwrap();
}

fn item(id: &str, kind: WorkItemKind, deps: &[&str]) -> WorkItem {
    WorkItem {
        id: id.into(),
        run_id: "R-1".into(),
        title: format!("Item {id}"),
        description: "Beschreibung".into(),
        kind,
        status: WorkItemStatus::Pending,
        priority: 5,
        seq: agentkit_work::id_order(id),
        required_role: None,
        dependencies: deps.iter().map(|d| d.to_string()).collect(),
        acceptance_criteria: vec![],
        verification_policy: VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        executor: ExecutorKind::SingleAgent,
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn add(store: &WorkStore, item: WorkItem) {
    store.submit(WorkEvent::WorkItemCreated { item }).unwrap();
}

/// Ein lesendes Item: darf auch ohne Git-Isolation neben anderen laufen.
fn reader(id: &str) -> WorkItem {
    item(id, WorkItemKind::Review, &[])
}

/// Ein schreibendes Item.
fn writer(id: &str) -> WorkItem {
    item(id, WorkItemKind::Implementation, &[])
}

fn cfg(workspace: &str) -> RunnerConfig {
    RunnerConfig {
        workspace: workspace.to_string(),
        ..RunnerConfig::default()
    }
}

fn run_with(
    store: &Arc<WorkStore>,
    executor: &PerItem,
    workspace: &str,
    cancel: &Cancel,
) -> Result<RunOutcome, WorkError> {
    run_to_completion(store, "R-1", executor, &cfg(workspace), cancel, &mut |_| {})
}

fn run(store: &Arc<WorkStore>, executor: &PerItem) -> RunOutcome {
    run_with(store, executor, ".", &new_cancel()).unwrap()
}

fn status(store: &WorkStore, id: &str) -> WorkItemStatus {
    store.snapshot().items[id].status
}

fn tmp_repo(name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "agentkit_work_parallel_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let base = agentkit_work::git::init_repo_with_commit(&dir.to_string_lossy());
    (dir, base)
}

fn write(file: &'static str, content: &'static str, meet_at: &Arc<Meet>) -> Step {
    let meet_at = meet_at.clone();
    Box::new(move |pkg, _| {
        std::fs::write(std::path::Path::new(&pkg.workspace).join(file), content).unwrap();
        if meet_at.stay(2, MEET_TIMEOUT) < 2 {
            return Err("die beiden Versuche liefen nicht gleichzeitig".to_string());
        }
        Ok(format!("{} fertig.", pkg.item.id))
    })
}

// ------------------------------------------------------- Gleichzeitigkeit

/// Der Kern: zwei unabhängige Items laufen mit zwei Workern WIRKLICH
/// gleichzeitig — beide Versuche stehen zur selben Zeit am Treffpunkt.
#[test]
fn zwei_unabhaengige_items_laufen_gleichzeitig() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, reader("W-1"));
    add(&store, reader("W-2"));

    let together = Arc::new(Meet::default());
    let executor = PerItem::new(vec![
        ("W-1", vec![meet(&together, 2)]),
        ("W-2", vec![meet(&together, 2)]),
    ]);

    let outcome = run(&store, &executor);
    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(outcome.completed.len(), 2);
    assert_eq!(outcome.attempts, 2);
}

/// Mit einem Worker (Default) läuft alles nacheinander wie bisher.
#[test]
fn mit_einem_worker_laeuft_nie_mehr_als_ein_versuch() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, WorkBudget::default(), None);
    add(&store, reader("W-1"));
    add(&store, reader("W-2"));

    let (together, most) = (Arc::new(Meet::default()), Arc::new(Mutex::new(0)));
    let executor = PerItem::new(vec![
        ("W-1", vec![linger(&together, &most)]),
        ("W-2", vec![linger(&together, &most)]),
    ]);

    assert_eq!(
        run(&store, &executor).reason,
        CompletionReason::AllItemsDone
    );
    assert_eq!(*most.lock().unwrap(), 1);
}

/// `max_parallel_agents` ist eine Obergrenze: mit zwei Workern und drei
/// bereiten Items laufen zwei zugleich, nie drei.
#[test]
fn nie_mehr_gleichzeitige_versuche_als_worker() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    for id in ["W-1", "W-2", "W-3"] {
        add(&store, reader(id));
    }

    let (together, most) = (Arc::new(Meet::default()), Arc::new(Mutex::new(0)));
    let executor = PerItem::new(vec![
        ("W-1", vec![linger(&together, &most)]),
        ("W-2", vec![linger(&together, &most)]),
        ("W-3", vec![linger(&together, &most)]),
    ]);

    assert_eq!(
        run(&store, &executor).reason,
        CompletionReason::AllItemsDone
    );
    assert_eq!(*most.lock().unwrap(), 2);
}

/// Ein freier Worker ändert nichts an den Abhängigkeiten: der Nachfolger
/// startet erst, wenn sein Vorgänger abgeschlossen im Journal steht.
#[test]
fn abhaengiges_item_wartet_trotz_freiem_worker_auf_seinen_vorgaenger() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, reader("W-1"));
    add(&store, item("W-2", WorkItemKind::Review, &["W-1"]));

    let successor: Step = Box::new(|pkg, store| {
        let predecessor = store.snapshot().items["W-1"].status;
        if predecessor != WorkItemStatus::Completed {
            return Err(format!("W-1 war beim Start von W-2 {predecessor:?}"));
        }
        Ok(format!("{} fertig.", pkg.item.id))
    });
    let executor = PerItem::new(vec![("W-1", vec![ok()]), ("W-2", vec![successor])]);

    let outcome = run(&store, &executor);
    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(
        outcome.completed,
        vec!["W-1".to_string(), "W-2".to_string()]
    );
}

/// Jeder Worker trägt seine eigene Kennung ins Journal, und jedes
/// Agenten-Ereignis nennt das Item, zu dem es gehört.
#[test]
fn versuche_tragen_die_kennung_ihres_workers_und_ereignisse_ihr_item() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, reader("W-1"));
    add(&store, reader("W-2"));

    let together = Arc::new(Meet::default());
    let executor = PerItem::new(vec![
        ("W-1", vec![meet(&together, 2)]),
        ("W-2", vec![meet(&together, 2)]),
    ]);
    let mut event_items = Vec::new();
    run_to_completion(
        &store,
        "R-1",
        &executor,
        &cfg("."),
        &new_cancel(),
        &mut |p| {
            if let WorkProgress::Agent { item, attempt, .. } = p {
                assert_eq!(attempt, 1);
                event_items.push(item);
            }
        },
    )
    .unwrap();

    let mut workers: Vec<String> = store
        .snapshot()
        .attempts
        .values()
        .map(|a| a.agent_id.clone())
        .collect();
    workers.sort();
    assert_eq!(
        workers,
        vec!["worker-1".to_string(), "worker-2".to_string()]
    );
    event_items.sort();
    assert_eq!(event_items, vec!["W-1".to_string(), "W-2".to_string()]);
}

// ------------------------------------------------------- Schreiber-Regel

/// Ohne Git-Isolation teilen sich alle Items einen Arbeitsbaum: zwei
/// schreibende Items laufen nacheinander, auch mit zwei Workern.
#[test]
fn ohne_git_isolation_laufen_zwei_schreibende_items_nacheinander() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, writer("W-1"));
    add(&store, writer("W-2"));

    let (together, most) = (Arc::new(Meet::default()), Arc::new(Mutex::new(0)));
    let executor = PerItem::new(vec![
        ("W-1", vec![linger(&together, &most)]),
        ("W-2", vec![linger(&together, &most)]),
    ]);

    assert_eq!(
        run(&store, &executor).reason,
        CompletionReason::AllItemsDone
    );
    assert_eq!(*most.lock().unwrap(), 1);
}

/// Ein lesendes Item läuft neben dem einen Schreiber.
#[test]
fn ohne_git_isolation_laeuft_ein_lesendes_item_neben_dem_schreiber() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, writer("W-1"));
    add(&store, reader("W-2"));

    let together = Arc::new(Meet::default());
    let executor = PerItem::new(vec![
        ("W-1", vec![meet(&together, 2)]),
        ("W-2", vec![meet(&together, 2)]),
    ]);

    assert_eq!(
        run(&store, &executor).reason,
        CompletionReason::AllItemsDone
    );
}

// ------------------------------------------------------------------- Git

/// Unter Git-Isolation schreiben zwei Items GLEICHZEITIG, jedes in seinem
/// eigenen Worktree; die Integration bringt beide in den Ausgangsbranch.
#[test]
fn zwei_schreibende_items_arbeiten_gleichzeitig_in_eigenen_worktrees() {
    let (dir, base) = tmp_repo("zwei_dateien");
    let ws = dir.to_string_lossy().to_string();
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), Some(&base));
    add(&store, writer("W-1"));
    add(&store, writer("W-2"));

    let together = Arc::new(Meet::default());
    let executor = PerItem::new(vec![
        ("W-1", vec![write("a.txt", "von W-1", &together)]),
        ("W-2", vec![write("b.txt", "von W-2", &together)]),
    ]);

    let outcome = run_with(&store, &executor, &ws, &new_cancel()).unwrap();
    assert_eq!(outcome.reason, CompletionReason::AllItemsDone);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "von W-1"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "von W-2"
    );
    assert_eq!(agentkit_work::git::current_branch(&ws).unwrap(), "main");
    std::fs::remove_dir_all(&dir).ok();
}

/// Ändern zwei parallele Items dieselbe Zeile, zeigt sich das erst bei der
/// Integration: der Lauf endet blockiert, der Ausgangsbranch bleibt sauber.
#[test]
fn zwei_parallele_items_mit_konflikt_blockieren_die_integration() {
    let (dir, base) = tmp_repo("konflikt");
    let ws = dir.to_string_lossy().to_string();
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), Some(&base));
    add(&store, writer("W-1"));
    add(&store, writer("W-2"));

    let together = Arc::new(Meet::default());
    let executor = PerItem::new(vec![
        (
            "W-1",
            vec![write("readme.txt", "W-1 war hier\n", &together)],
        ),
        (
            "W-2",
            vec![write("readme.txt", "W-2 war hier\n", &together)],
        ),
    ]);

    let outcome = run_with(&store, &executor, &ws, &new_cancel()).unwrap();
    assert_eq!(outcome.reason, CompletionReason::Blocked);
    let state = store.snapshot();
    let integration = state
        .items
        .values()
        .find(|it| it.kind == WorkItemKind::Integration)
        .expect("Integrations-Item wurde angelegt");
    assert_eq!(integration.status, WorkItemStatus::Failed);
    assert!(agentkit_work::git::is_clean(&ws).unwrap());
    assert_eq!(agentkit_work::git::current_branch(&ws).unwrap(), "main");
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------ Abbruch, Fehler, Budget

/// Ein Abbruch trifft alle laufenden Versuche: beide werden als unterbrochen
/// journalt, keiner verbraucht einen Versuch, und ein Resume setzt beide fort.
#[test]
fn abbruch_unterbricht_alle_laufenden_versuche_und_resume_setzt_sie_fort() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    add(&store, reader("W-1"));
    add(&store, reader("W-2"));

    let cancel = new_cancel();
    let together = Arc::new(Meet::default());
    let interrupted = |cancel: &Cancel, together: &Arc<Meet>| -> Step {
        let (cancel, together) = (cancel.clone(), together.clone());
        Box::new(move |_, _| {
            if together.stay(2, MEET_TIMEOUT) < 2 {
                return Err("die beiden Versuche liefen nicht gleichzeitig".to_string());
            }
            // Der Stop-Knopf, wie ihn ein echter Agent beobachtet.
            cancel.store(true, Ordering::SeqCst);
            Ok("(abgebrochen)".to_string())
        })
    };
    let executor = PerItem::new(vec![
        ("W-1", vec![interrupted(&cancel, &together), ok()]),
        ("W-2", vec![interrupted(&cancel, &together), ok()]),
    ]);

    let outcome = run_with(&store, &executor, ".", &cancel).unwrap();
    assert_eq!(outcome.reason, CompletionReason::Canceled);
    for id in ["W-1", "W-2"] {
        let state = store.snapshot();
        assert_eq!(state.items[id].status, WorkItemStatus::Pending, "{id}");
        assert_eq!(state.items[id].attempt_count, 0, "{id}");
    }
    assert!(store.snapshot().leases.is_empty());

    let resumed = run(&store, &executor);
    assert_eq!(resumed.reason, CompletionReason::AllItemsDone);
    assert_eq!(resumed.completed.len(), 2);
}

/// Ein harter Fehler bremst, er bricht nicht ab: der zweite Versuch läuft zu
/// Ende und steht im Journal, erst danach kommt der Fehler zurück.
#[test]
fn harter_fehler_laesst_den_anderen_versuch_zu_ende_laufen() {
    let store = Arc::new(WorkStore::in_memory());
    setup(&store, budget(2), None);
    // Ein Prüfkommando des Bedieners, das nicht startbar ist — ein
    // Konfigurationsfehler, kein fachlicher Fehlschlag.
    let mut broken = reader("W-1");
    broken.verification_policy = VerificationPolicy::AutomatedTests {
        command: "agentkit-work-dieses-kommando-gibt-es-nicht".to_string(),
    };
    add(&store, broken);
    add(&store, reader("W-2"));

    let together = Arc::new(Meet::default());
    // W-2 bleibt in Arbeit, bis W-1s Versuch im Journal beendet ist (das
    // Prüfkommando scheitert direkt danach), und noch einen Moment länger —
    // der Fehler trifft also ein, während W-2 läuft.
    let outlasts: Step = {
        let together = together.clone();
        Box::new(move |pkg, store| {
            together.stay(2, MEET_TIMEOUT);
            let deadline = std::time::Instant::now() + MEET_TIMEOUT;
            let first_finished = |store: &WorkStore| {
                store
                    .snapshot()
                    .attempts
                    .values()
                    .any(|a| a.work_item_id == "W-1" && a.finished_at_ms.is_some())
            };
            while !first_finished(store) && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            std::thread::sleep(ABSENCE_WINDOW);
            Ok(format!("{} fertig.", pkg.item.id))
        })
    };
    let executor = PerItem::new(vec![
        ("W-1", vec![meet(&together, 2)]),
        ("W-2", vec![outlasts]),
    ]);

    let result = run_with(&store, &executor, ".", &new_cancel());

    let err = result.err().expect("der harte Fehler kommt zurück");
    assert!(err.to_string().contains("nicht startbar"), "{err}");
    assert_eq!(
        status(&store, "W-2"),
        WorkItemStatus::Completed,
        "der zweite Versuch wurde trotz des Fehlers zu Ende geführt und journalt"
    );
}

/// Ein erschöpftes Budget startet nichts Neues, wirft aber keine laufende
/// Arbeit weg: der Lauf pausiert erst, wenn beide Versuche beendet sind.
#[test]
fn erschoepftes_budget_pausiert_erst_nach_dem_ende_laufender_versuche() {
    let store = Arc::new(WorkStore::in_memory());
    setup(
        &store,
        WorkBudget {
            max_work_items: Some(3),
            ..budget(2)
        },
        None,
    );
    add(&store, reader("W-1"));
    add(&store, reader("W-2"));

    let together = Arc::new(Meet::default());
    // W-1 legt mitten im Versuch ein drittes Item an — damit ist
    // `max_work_items` erreicht, während noch zwei Versuche laufen.
    let fills_budget: Step = {
        let together = together.clone();
        Box::new(move |pkg, store| {
            together.stay(2, MEET_TIMEOUT);
            add(store, reader("W-3"));
            Ok(format!("{} fertig.", pkg.item.id))
        })
    };
    let executor = PerItem::new(vec![
        ("W-1", vec![fills_budget]),
        ("W-2", vec![meet(&together, 2)]),
    ]);

    let outcome = run(&store, &executor);
    assert_eq!(outcome.reason, CompletionReason::BudgetExceeded);
    assert_eq!(outcome.completed.len(), 2);
    assert_eq!(status(&store, "W-3"), WorkItemStatus::Pending);
}
