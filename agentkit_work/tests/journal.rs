//! Das Journal als Spezifikation: was ein Neustart wiederherstellt, was
//! toleriert wird und was ein harter Fehler bleibt.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use agentkit_work::{
    ProjectStatus, RunStatus, WorkBudget, WorkError, WorkEvent, WorkItem, WorkItemKind,
    WorkItemStatus, WorkProject, WorkRun, WorkStore, JOURNAL_FILE,
};

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agentkit_work_{name}_{}", std::process::id()));
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
        verification_policy: agentkit_work::VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn event_line(seq: u64, event: &WorkEvent) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": "1",
        "seq": seq,
        "at": 0,
        "event": event,
    }))
    .unwrap()
}

#[test]
fn replay_roundtrip_ueber_temp_ordner() {
    let dir = tmp_dir("replay");
    {
        let store = WorkStore::open(&dir).unwrap();
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
    }

    let reopened = WorkStore::open(&dir).unwrap();
    let snap = reopened.snapshot();
    assert_eq!(snap.seq, 3);
    assert_eq!(snap.runs["R-1"].status, RunStatus::Running);
    assert_eq!(snap.items["W-1"].status, WorkItemStatus::Pending);

    // IDs laufen nach dem Neustart weiter, statt vergebene zu überschreiben.
    assert_eq!(snap.next_item_id(), "W-2");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unbekannte_schema_version_ist_ein_harter_fehler() {
    let dir = tmp_dir("unbekannte_version");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(JOURNAL_FILE),
        "{\"schema_version\":\"99\",\"seq\":1,\"at\":0,\"event\":{}}\n",
    )
    .unwrap();

    let err = WorkStore::open(&dir).unwrap_err();
    assert!(matches!(err, WorkError::Journal(_)), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkpoint_liefert_identischen_zustand_und_haengt_eine_snapshot_zeile_an() {
    // Regressionstest zu Befund 1 (Handprobe): ein früherer Entwurf rief hier
    // `Journal::rewrite` und ERSETZTE damit das gesamte Journal durch die eine
    // Snapshot-Zeile — dieser Test bestand damals auch, weil er nur prüfte,
    // dass die Zeilenzahl sinkt (`nachher_zeilen < vorher_zeilen` bzw. `== 1`).
    // Das widerspricht §14 des Konzepts (vollständige Auditierbarkeit,
    // Timeline, Replay) und machte `agentkit work events` nach dem ersten
    // Checkpoint praktisch wirkungslos. Der Fix HÄNGT die Snapshot-Zeile nur
    // noch an — das Journal WÄCHST bei einem Checkpoint um genau eine Zeile,
    // es schrumpft nicht.
    let dir = tmp_dir("checkpoint");
    let store = WorkStore::open(&dir).unwrap();
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
    for i in 1..=5u64 {
        store
            .submit(WorkEvent::WorkItemCreated {
                item: item(&format!("W-{i}"), i),
            })
            .unwrap();
    }
    let vorher_zeilen = store.journal_lines().unwrap();

    store.checkpoint().unwrap();
    let nachher_zeilen = store.journal_lines().unwrap();
    assert_eq!(
        nachher_zeilen,
        vorher_zeilen + 1,
        "ein Checkpoint HÄNGT genau eine Snapshot-Zeile an, er kürzt nicht mehr"
    );

    let erwartet = store.snapshot();
    // Erzwungen durch Befund 1 (Sperrdatei): `store` bleibt sonst offen und
    // das folgende `open` scheitert an der eigenen Sperre.
    drop(store);
    let reopened = WorkStore::open(&dir).unwrap();
    let tatsaechlich = reopened.snapshot();
    assert_eq!(
        serde_json::to_value(&*erwartet).unwrap(),
        serde_json::to_value(&*tatsaechlich).unwrap(),
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zwei_checkpoints_mit_ereignissen_dazwischen_verlieren_keine_historie() {
    // Der zentrale Regressionstest zu Befund 1: nach mehreren Checkpoints MIT
    // Ereignissen dazwischen müssen ALLE Ereignisse im Journal auffindbar
    // bleiben — nicht nur der letzte Snapshot. Vor der Korrektur hätte der
    // zweite `checkpoint()`-Aufruf (per `Journal::rewrite`) die beiden
    // `work_item_created`-Ereignisse für W-1/W-2 spurlos gelöscht.
    let dir = tmp_dir("zwei_checkpoints_historie");
    let store = WorkStore::open(&dir).unwrap();
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
    store.checkpoint().unwrap();

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
    store.checkpoint().unwrap();

    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-3", 3),
        })
        .unwrap();

    let content = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
    let event_kinds: Vec<String> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            v.get("event")
                .and_then(|e| e.get("kind"))
                .and_then(|k| k.as_str())
                .map(str::to_string)
        })
        .collect();
    let created_count = event_kinds
        .iter()
        .filter(|k| k.as_str() == "work_item_created")
        .count();
    assert_eq!(
        created_count, 3,
        "alle drei 'work_item_created'-Ereignisse (W-1, W-2, W-3) müssen erhalten bleiben: {event_kinds:?}"
    );
    assert!(
        event_kinds.contains(&"run_started".to_string()),
        "auch das früheste Ereignis vor dem ersten Checkpoint muss noch da sein: {event_kinds:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn neu_geoeffneter_store_nach_mehreren_snapshots_spielt_nur_ereignisse_nach_dem_letzten_snapshot_ab(
) {
    // Regressionstest zu Befund 1, zweiter Teil: die Wiederaufnahme bleibt
    // O(1) zur Journallänge, obwohl jetzt mehrere Snapshot-Zeilen im Journal
    // stehen können (angehängt statt ersetzt). Miss das über
    // `WorkStore::events_replayed_on_open` (die dafür eingeführte
    // Beobachtungs-Naht, siehe `Journal::open`): nach dem letzten Checkpoint
    // liegt nur EIN weiteres Ereignis (W-4), also darf `open` auch nur
    // dieses eine über `apply` einspielen — nicht die zwölf davor.
    let dir = tmp_dir("replay_ab_letztem_snapshot");
    let store = WorkStore::open(&dir).unwrap();
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
    store.checkpoint().unwrap();

    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-3", 3),
        })
        .unwrap();
    store.checkpoint().unwrap();

    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-4", 4),
        })
        .unwrap();

    let erwartet = store.snapshot();
    drop(store);

    let reopened = WorkStore::open(&dir).unwrap();
    assert_eq!(
        reopened.events_replayed_on_open(),
        1,
        "nur das eine Ereignis NACH dem letzten Snapshot (W-4) darf eingespielt werden"
    );
    assert_eq!(
        serde_json::to_value(&*erwartet).unwrap(),
        serde_json::to_value(&*reopened.snapshot()).unwrap(),
        "der Zustand nach dem Neuöffnen muss identisch zum Stand vor dem Neustart sein"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn oberhalb_der_kompaktierungsschwelle_kuerzt_open_das_journal_und_zustand_bleibt_gleich() {
    // Test #3 zu Befund 1: die automatische Kompaktierung BEIM ÖFFNEN (nicht
    // mehr bei jedem Checkpoint) muss weiterhin greifen, sobald die
    // Journallänge weit genug vor die Anzahl der Datensätze gelaufen ist
    // (`REWRITE_MIN_LINES`/`REWRITE_FACTOR`, siehe `store/mod.rs`). Erzeugt
    // dafür viele Heartbeat-Zeilen (`LeaseRenewed`) auf ein und demselben
    // Item — sie sind Ereignis-Zeilen, legen aber KEINEN neuen Datensatz an,
    // genau das Muster, das die Schwelle abfangen soll.
    let dir = tmp_dir("kompaktierung_beim_oeffnen");
    let store = WorkStore::open(&dir).unwrap();
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
    store
        .submit(WorkEvent::WorkItemClaimed {
            item: "W-1".into(),
            agent: "worker-1".into(),
            attempt: "A-1".into(),
            lease_expires_ms: 999_999_999,
            at_ms: 0,
        })
        .unwrap();
    for _ in 0..260 {
        store
            .submit(WorkEvent::LeaseRenewed {
                item: "W-1".into(),
                attempt: "A-1".into(),
                lease_expires_ms: 999_999_999,
                at_ms: 0,
            })
            .unwrap();
    }

    let vorher_zeilen = store.journal_lines().unwrap();
    assert!(
        vorher_zeilen > 256,
        "Testaufbau muss die Schwelle überschreiten: {vorher_zeilen}"
    );
    let erwartet = store.snapshot();
    drop(store);

    let reopened = WorkStore::open(&dir).unwrap();
    let nachher_zeilen = reopened.journal_lines().unwrap();
    assert!(
        nachher_zeilen < vorher_zeilen,
        "open() muss oberhalb der Schwelle weiterhin kompaktieren: {nachher_zeilen} < {vorher_zeilen}"
    );
    assert_eq!(
        serde_json::to_value(&*erwartet).unwrap(),
        serde_json::to_value(&*reopened.snapshot()).unwrap(),
        "die Kompaktierung darf den Zustand nicht verändern"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn abgeschnittene_letzte_zeile_wird_toleriert_und_der_rest_bleibt_intakt() {
    let dir = tmp_dir("truncated");
    {
        let store = WorkStore::open(&dir).unwrap();
        store
            .submit(WorkEvent::ProjectCreated { project: project() })
            .unwrap();
        store
            .submit(WorkEvent::RunStarted { run: run("R-1") })
            .unwrap();
    }

    // Absturz mitten im Schreiben simulieren: eine unvollständige JSON-Zeile
    // ohne abschließende Klammer/Zeilenumbruch anhängen.
    let mut content = std::fs::read_to_string(dir.join(JOURNAL_FILE)).unwrap();
    content
        .push_str("{\"schema_version\":\"1\",\"seq\":3,\"at\":0,\"event\":{\"kind\":\"work_item_c");
    std::fs::write(dir.join(JOURNAL_FILE), &content).unwrap();

    let store = WorkStore::open(&dir).unwrap();
    let snap = store.snapshot();
    assert_eq!(snap.seq, 2, "nur die zwei vollständigen Ereignisse zählen");
    assert_eq!(snap.runs["R-1"].status, RunStatus::Running);

    // Nächstes Schreiben überschreibt die abgeschnittene Zeile.
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-1", 1),
        })
        .unwrap();
    // Erzwungen durch Befund 1 (Sperrdatei): `store` bleibt sonst offen und
    // das folgende `open` scheitert an der eigenen Sperre.
    drop(store);
    let reopened = WorkStore::open(&dir).unwrap();
    assert_eq!(reopened.snapshot().items.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn kaputte_zeile_in_der_mitte_ist_ein_harter_fehler() {
    let dir = tmp_dir("broken_middle");
    std::fs::create_dir_all(&dir).unwrap();
    let line1 = event_line(1, &WorkEvent::ProjectCreated { project: project() });
    let line3 = event_line(2, &WorkEvent::RunStarted { run: run("R-1") });
    let content = format!("{line1}\nnicht einmal JSON\n{line3}\n");
    std::fs::write(dir.join(JOURNAL_FILE), content).unwrap();

    let err = WorkStore::open(&dir).unwrap_err();
    assert!(matches!(err, WorkError::Journal(_)), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zehn_leser_und_ein_schreiber_arbeiten_gleichzeitig() {
    let store = Arc::new(WorkStore::in_memory());
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();

    let readers = 10;
    let writes = 50u64;
    let barrier = Arc::new(Barrier::new(readers + 1));
    let gelesen = Arc::new(AtomicUsize::new(0));

    thread::scope(|scope| {
        for _ in 0..readers {
            let store = store.clone();
            let barrier = barrier.clone();
            let gelesen = gelesen.clone();
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..200 {
                    let snap = store.snapshot();
                    gelesen.fetch_add(snap.items.len(), Ordering::Relaxed);
                }
            });
        }

        let store_w = store.clone();
        scope.spawn(move || {
            barrier.wait();
            for i in 1..=writes {
                store_w
                    .submit(WorkEvent::WorkItemCreated {
                        item: item(&format!("W-{i}"), i),
                    })
                    .unwrap();
            }
        });
    });

    // Keine verlorene Mutation: genau `writes` Items, seq lückenlos.
    assert_eq!(store.snapshot().items.len(), writes as usize);
    assert_eq!(store.snapshot().seq, 2 + writes);
    assert!(gelesen.load(Ordering::Relaxed) > 0);
}

#[test]
fn ein_snapshot_aendert_sich_nicht_unter_dem_leser() {
    let store = WorkStore::in_memory();
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

    let vorher = store.snapshot();
    let anzahl_vorher = vorher.items.len();

    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2),
        })
        .unwrap();

    // Der bereits gehaltene Snapshot bleibt exakt, was er war …
    assert_eq!(vorher.items.len(), anzahl_vorher);
    // … der nächste sieht die Mutation.
    assert_eq!(store.snapshot().items.len(), anzahl_vorher + 1);
}

#[test]
fn replay_von_snapshot_zeile_plus_folgeereignissen_lehnt_keine_duplikate_ab() {
    // Absicherung gegen einen Regress durch die Duplikat-Ablehnung in
    // `state::apply` (§2 im Review): eine Snapshot-Zeile enthält ihre
    // Datensätze bereits — `Journal::open` überspringt beim Replay ALLES vor
    // der letzten Snapshot-Zeile, statt es zusätzlich über `apply`
    // einzuspielen (siehe dort). Das Journal enthält seit der Korrektur von
    // Befund 1 (Checkpoint hängt an, statt zu ersetzen) das ursprüngliche
    // `work_item_created` für W-1 weiterhin PHYSISCH VOR der Snapshot-Zeile —
    // würde das Replay diese alte Zeile versehentlich zusätzlich anwenden,
    // liefe das direkt in die Duplikat-Ablehnung (W-1 existiert laut
    // Snapshot ja schon).
    let dir = tmp_dir("replay_snapshot_plus_events");
    let store = WorkStore::open(&dir).unwrap();
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

    // Hängt eine Snapshot-Zeile mit W-1 darin an (3 Ereignis-Zeilen + 1
    // Snapshot-Zeile = 4 — die früheren Ereignis-Zeilen bleiben erhalten).
    store.checkpoint().unwrap();
    assert_eq!(store.journal_lines().unwrap(), 4);

    // Danach zwei neue Ereignis-Zeilen HINTER der Snapshot-Zeile.
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-2", 2),
        })
        .unwrap();
    store
        .submit(WorkEvent::WorkItemCreated {
            item: item("W-3", 3),
        })
        .unwrap();
    drop(store);

    // Neustart: muss Snapshot-Zeile (W-1) plus die zwei Folge-Ereignisse
    // (W-2, W-3) anstandslos wiederherstellen — kein Duplikat, keine Ablehnung.
    let reopened = WorkStore::open(&dir).unwrap();
    let snap = reopened.snapshot();
    assert_eq!(snap.items.len(), 3);
    assert!(snap.items.contains_key("W-1"));
    assert!(snap.items.contains_key("W-2"));
    assert!(snap.items.contains_key("W-3"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkpoint_und_neustart_bleiben_moeglich_obwohl_apply_ein_zweites_project_created_ablehnt() {
    // Snapshot-Zeilen werden beim Öffnen DIREKT als Zustand eingesetzt, nicht
    // über `apply` appliziert (siehe `Journal::open`) — die neue Ablehnung
    // eines zweiten `ProjectCreated` in `state::apply` darf einen ganz
    // normalen Checkpoint+Neustart-Zyklus deshalb nicht verhindern, obwohl
    // das Projekt selbst Teil jeder Snapshot-Zeile ist.
    let dir = tmp_dir("checkpoint_project_created_unaffected");
    let store = WorkStore::open(&dir).unwrap();
    store
        .submit(WorkEvent::ProjectCreated { project: project() })
        .unwrap();
    store
        .submit(WorkEvent::RunStarted { run: run("R-1") })
        .unwrap();
    store.checkpoint().unwrap();
    drop(store);

    let reopened = WorkStore::open(&dir).unwrap();
    assert_eq!(reopened.snapshot().project.as_ref().unwrap().id, "demo");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn budget_updated_ueberlebt_einen_neustart() {
    let dir = tmp_dir("budget_updated_restart");
    {
        let store = WorkStore::open(&dir).unwrap();
        store
            .submit(WorkEvent::ProjectCreated { project: project() })
            .unwrap();
        let budget = WorkBudget {
            max_steps_per_attempt: 80,
            ..WorkBudget::default()
        };
        store
            .submit(WorkEvent::BudgetUpdated { budget, at_ms: 0 })
            .unwrap();
    }

    let reopened = WorkStore::open(&dir).unwrap();
    assert_eq!(
        reopened
            .snapshot()
            .project
            .as_ref()
            .unwrap()
            .budget
            .max_steps_per_attempt,
        80
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------- Befund 1: Sperrdatei

/// Befund 1 des Code-Reviews (schwerster Befund): `WorkStore` sperrte bisher
/// nur PROZESSINTERN (`RwLock`/`Mutex`) — nichts verhinderte ein zweites
/// `WorkStore::open` auf demselben Verzeichnis, weder im selben Prozess noch
/// über Prozessgrenzen hinweg. Zwei unabhängige Schreiber hätten IDs aus
/// ihrem je eigenen Snapshot berechnet und dieselbe ID doppelt ans Journal
/// angehängt — der nächste `WorkStore::open` wäre beim Replay in die
/// (korrekte) Duplikat-Ablehnung in `state::apply` gelaufen und DAUERHAFT
/// gescheitert, weil jeder weitere Öffnungsversuch dasselbe kaputte Journal
/// erneut liest. Die Sperrdatei (`work.lock`, `create_new`) verhindert das
/// strukturell: ein zweiter `open` auf demselben Verzeichnis scheitert
/// sichtbar, solange der erste Store lebt — und gelingt wieder, sobald er
/// fällt (Neustart-Tests im Rest dieser Datei verlassen sich genau darauf).
#[test]
fn zweiter_open_scheitert_solange_der_erste_lebt_und_gelingt_nach_dem_fallenlassen_wieder() {
    let dir = tmp_dir("lock_exclusive");
    let erster = WorkStore::open(&dir).unwrap();

    let zweiter = WorkStore::open(&dir);
    assert!(
        matches!(zweiter, Err(WorkError::Locked(_))),
        "zweiter Open-Versuch hätte an der Sperre scheitern müssen: {zweiter:?}"
    );

    drop(erster);
    let dritter = WorkStore::open(&dir);
    assert!(
        dritter.is_ok(),
        "nach dem Fallenlassen des ersten Stores muss 'open' wieder gelingen: {dritter:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Die Fehlermeldung muss den Pfad und (soweit lesbar) die PID des Halters
/// nennen — sonst weiß der Bediener nicht, WELCHER Prozess gemeint ist oder
/// welche Datei er im Ernstfall manuell entfernen müsste.
#[test]
fn sperrfehler_nennt_pfad_und_pid_des_halters() {
    let dir = tmp_dir("lock_message");
    let erster = WorkStore::open(&dir).unwrap();

    let err = WorkStore::open(&dir).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("work.lock"), "{msg}");
    assert!(
        msg.contains(&std::process::id().to_string()),
        "eigene PID (einziger Prozess im Test) sollte in der Meldung stehen: {msg}"
    );

    drop(erster);
    std::fs::remove_dir_all(&dir).ok();
}

/// `WorkStore::force_unlock` entfernt eine zurückgebliebene Sperrdatei ohne
/// Prüfung — der Ausweg für `--force`, wenn ein abgestürzter Prozess sie
/// hinterlassen hat. Simuliert das, indem die Sperrdatei eines gefallenen
/// Stores (Absturz-Ersatz: der Store wird NICHT sauber gedroppt, sondern
/// `std::mem::forget`-t, damit die Sperrdatei wie nach einem `SIGKILL`
/// zurückbleibt) manuell übernommen wird.
#[test]
fn force_unlock_entfernt_eine_zurueckgebliebene_sperre_und_open_gelingt_danach() {
    let dir = tmp_dir("force_unlock");
    let store = WorkStore::open(&dir).unwrap();
    // Absturz-Ersatz: `Drop` läuft absichtlich NICHT, die Sperrdatei bleibt
    // liegen wie nach einem `SIGKILL`.
    std::mem::forget(store);

    assert!(matches!(WorkStore::open(&dir), Err(WorkError::Locked(_))));

    WorkStore::force_unlock(&dir).unwrap();
    let reopened = WorkStore::open(&dir);
    assert!(reopened.is_ok(), "{reopened:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn store_ist_ueber_threads_teilbar() {
    // Der Typ MUSS Send+Sync sein — sonst kann ihn kein Worker-Thread halten.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<WorkStore>>();
}

/// Vorwärtskompatibilität (Phase 5a, Vorgabe 2): ein Journal aus der Zeit vor
/// `VerificationPolicy` hat kein `verification_policy`-Feld am Item —
/// `#[serde(default)]` muss es trotzdem laden, mit `VerificationPolicy::None`.
/// Das Item wird hier bewusst als Rohtext geschrieben (nicht über `item()`,
/// das schon das neue Feld setzt), um genau das ALTE Journal-Format nachzubilden.
#[test]
fn journal_ohne_verification_policy_feld_laedt_weiter() {
    let dir = tmp_dir("ohne_verification_feld");
    std::fs::create_dir_all(&dir).unwrap();
    let project_line = serde_json::json!({
        "schema_version": "1",
        "seq": 1,
        "at": 0,
        "event": {"kind": "project_created", "project": project()},
    });
    let run_line = serde_json::json!({
        "schema_version": "1",
        "seq": 2,
        "at": 0,
        "event": {"kind": "run_started", "run": run("R-1")},
    });
    let item_line = serde_json::json!({
        "schema_version": "1",
        "seq": 3,
        "at": 0,
        "event": {
            "kind": "work_item_created",
            "item": {
                "id": "W-1",
                "run_id": "R-1",
                "title": "Altes Item",
                "description": "Ohne verification_policy im Journal.",
                "kind": "implementation",
                "status": "pending",
                "priority": 5,
                "seq": 1,
                "required_role": null,
                "dependencies": [],
                "acceptance_criteria": [],
                "attempt_count": 0,
                "max_attempts": 3,
                "updated_at_ms": 0
            }
        },
    });
    let content = format!("{project_line}\n{run_line}\n{item_line}\n");
    std::fs::write(dir.join(JOURNAL_FILE), content).unwrap();

    let store = WorkStore::open(&dir).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(
        snapshot.items["W-1"].verification_policy,
        agentkit_work::VerificationPolicy::None
    );

    std::fs::remove_dir_all(&dir).ok();
}
