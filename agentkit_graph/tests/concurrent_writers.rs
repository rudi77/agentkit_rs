//! Mehrere Schreiber auf EINEM Bundle: zwei Prozesse (oder zwei Stores im
//! selben Prozess) vergeben nie dieselbe ID und verlieren keinen Datensatz.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agentkit_graph::store::GraphStore;
use agentkit_graph::{ClaimDraft, GraphAccess, GraphWriteCommand, SourceDraft};

const CLAIMS_JE_SCHREIBER: usize = 25;
const KIND_DIR: &str = "AGENTKIT_GRAPH_TEST_KIND_DIR";
const KIND_NAME: &str = "AGENTKIT_GRAPH_TEST_KIND_NAME";

fn temp_bundle(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graph_writers_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Schreibt eigene, unverwechselbare Aussagen — jede legt zwei Entities, eine
/// Quelle und einen Claim an.
fn schreibe_alle(dir: &Path, wer: &str) {
    let store = GraphStore::open(dir).expect("öffnen");
    let access = GraphAccess::session(wer, "ws", wer);
    for i in 0..CLAIMS_JE_SCHREIBER {
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    &format!("{wer} Subjekt {i}"),
                    "nutzt",
                    &format!("{wer} Objekt {i}"),
                    SourceDraft::new("test_run"),
                )),
                &access,
            )
            .expect("schreiben");
    }
}

/// Kein eigener Test, sondern der Rumpf des Kindprozesses: das Test-Binary
/// ruft sich selbst mit gesetzten Umgebungsvariablen auf. Ohne sie tut die
/// Funktion nichts.
#[test]
fn kindprozess_schreibt() {
    if let (Ok(dir), Ok(name)) = (std::env::var(KIND_DIR), std::env::var(KIND_NAME)) {
        schreibe_alle(Path::new(&dir), &name);
    }
}

fn pruefe_bestand(dir: &Path, schreiber: usize) {
    let index = GraphStore::open_read_only(dir).expect("neu öffnen");
    let erwartet = schreiber * CLAIMS_JE_SCHREIBER;
    // Der Index führt je ID genau einen Datensatz: eine doppelt vergebene ID
    // zeigte sich hier als fehlender Datensatz.
    assert_eq!(index.claim_count(), erwartet, "Claims");
    assert_eq!(index.entity_count(), 2 * erwartet, "Entities");
    assert_eq!(index.source_count(), erwartet, "Quellen");
    let namen: HashSet<String> = index.entities().map(|e| e.canonical_name.clone()).collect();
    assert_eq!(
        namen.len(),
        2 * erwartet,
        "jede Entity hat ihren eigenen Namen"
    );
    let revisionen: HashSet<u64> = index.claims().map(|c| c.created_revision).collect();
    assert_eq!(
        revisionen.len(),
        erwartet,
        "jeder Commit hat seine eigene Revision"
    );
}

#[test]
fn mehrere_prozesse_schreiben_gleichzeitig_ohne_id_kollision() {
    let dir = temp_bundle("prozesse");
    let kinder: Vec<_> = (0..3)
        .map(|i| {
            Command::new(std::env::current_exe().unwrap())
                .args(["kindprozess_schreibt", "--exact", "--test-threads=1"])
                .env(KIND_DIR, &dir)
                .env(KIND_NAME, format!("kind{i}"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Kindprozess starten")
        })
        .collect();
    schreibe_alle(&dir, "eltern");
    for kind in kinder {
        let out = kind.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pruefe_bestand(&dir, 4);
    assert!(
        !dir.join("graph.lock").exists(),
        "die Sperre lebt nur während eines Commits"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Zwei Stores im selben Prozess auf demselben Verzeichnis — die Sperre hängt
/// am Commit, nicht am Öffnen, also dürfen beide offen sein und abwechselnd
/// schreiben.
#[test]
fn zwei_offene_stores_sehen_die_commits_des_anderen_beim_schreiben() {
    let dir = temp_bundle("stores");
    let a = GraphStore::open(&dir).unwrap();
    let b = GraphStore::open(&dir).unwrap();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let schreibe = |store: &GraphStore, s: &str| {
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    s,
                    "nutzt",
                    "Ziel",
                    SourceDraft::new("test_run"),
                )),
                &access,
            )
            .unwrap()
    };

    let erste = schreibe(&a, "Erstes");
    let zweite = schreibe(&b, "Zweites");
    let dritte = schreibe(&a, "Drittes");

    assert_eq!(
        [erste.revision, zweite.revision, dritte.revision],
        [1, 2, 3]
    );
    let ids: HashSet<_> = [&erste, &zweite, &dritte]
        .iter()
        .map(|r| r.claim_id.clone().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    // „Ziel" gibt es nur einmal: B hat die Entity von A nach dem Abgleich
    // wiedergefunden, statt eine zweite anzulegen.
    assert_eq!(a.snapshot().entity_count(), 4);
    assert_eq!(a.snapshot().claim_count(), 3);
    std::fs::remove_dir_all(&dir).ok();
}

/// Ein Rebuild aus einem veralteten Stand darf die Dokumente fremder Commits
/// nicht als verwaist wegräumen.
#[test]
fn rebuild_gleicht_vorher_mit_der_platte_ab() {
    let dir = temp_bundle("rebuild");
    let alt = GraphStore::open(&dir).unwrap();
    schreibe_alle(&dir, "anderer");

    alt.rebuild_bundle().unwrap();

    pruefe_bestand(&dir, 1);
    std::fs::remove_dir_all(&dir).ok();
}

/// `open_frozen` fasst das Verzeichnis nicht an — weder legt es ein Bundle an,
/// noch schreibt ein Commit dorthin.
#[test]
fn ein_eingefrorener_store_schreibt_nie_auf_die_platte() {
    let dir = temp_bundle("frozen");
    let leer = GraphStore::open_frozen(&dir).unwrap();
    assert_eq!(leer.snapshot().claim_count(), 0);
    assert!(!dir.exists(), "nichts angelegt");

    schreibe_alle(&dir, "schreiber");
    let frozen = GraphStore::open_frozen(&dir).unwrap();
    assert_eq!(frozen.snapshot().claim_count(), CLAIMS_JE_SCHREIBER);
    assert_eq!(frozen.path(), None);
    std::fs::remove_dir_all(&dir).ok();
}
