//! Der Schreibpfad als Spezifikation: was committet wird, was abgelehnt wird und
//! was ein Neustart wiederherstellt.

use std::sync::Arc;

use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    ClaimDraft, ClaimStatus, GraphAccess, GraphError, GraphLayer, GraphScope, GraphTarget,
    GraphView, GraphWriteCommand, SourceDraft,
};

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agentkit_graph_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn access() -> GraphAccess {
    GraphAccess::session("tester", "ws", "run-1")
}

fn claim(subject: &str, predicate: &str, object: &str) -> GraphWriteCommand {
    GraphWriteCommand::RecordClaim(ClaimDraft::new(
        subject,
        predicate,
        object,
        SourceDraft::new("test_run"),
    ))
}

// --------------------------------------------------- Legacy-Journal (Import)
//
// Hand-geschriebene Zeilen im alten Format: `schema_version: "1"`, blanke
// Actor-Principals ohne Praefix (`"tester"` statt `"agent:tester"`), kein
// `verified`, kein `extra` -- der Stand, den ein Verzeichnis aus der Zeit vor
// der OKF-Umstellung hinterlassen haben kann. `Actor`s `Deserialize` normalisiert
// einen blanken Principal automatisch zu `agent:<slug>` (siehe `model.rs`),
// unabhaengig davon, ob die Zeile aus einem Journal oder einem Bundle kommt --
// die Migration selbst muss dafuer nichts Eigenes tun.

fn legacy_line(revision: u64, op_json: &str) -> String {
    format!(r#"{{"schema_version":"1","revision":{revision},"at":1,"op":{op_json}}}"#)
}

fn legacy_source(id: &str, rev: u64, agent: &str) -> String {
    format!(
        r#"{{"record":"source","id":"{id}","source_type":"tool_result","agent_id":"{agent}","content_hash":"h-{id}","created_revision":{rev},"created_at":1}}"#
    )
}

fn legacy_entity(id: &str, name: &str, rev: u64) -> String {
    format!(
        r#"{{"record":"entity","id":"{id}","canonical_name":"{name}","entity_type":"thing","layer":"working","scope":{{"kind":"session","id":"run-1"}},"created_revision":{rev},"updated_revision":{rev},"created_at":1}}"#
    )
}

fn legacy_claim(
    id: &str,
    subject: &str,
    object: &str,
    rev: u64,
    source_id: &str,
    created_by: &str,
) -> String {
    format!(
        r#"{{"record":"claim","id":"{id}","subject":"{subject}","predicate":"nutzt","object":"{object}","layer":"working","scope":{{"kind":"session","id":"run-1"}},"status":"observation","confidence":0.5,"source_ids":["{source_id}"],"created_by":"{created_by}","created_revision":{rev},"updated_revision":{rev},"created_at":1}}"#
    )
}

/// Zaehlt Dokumente im Bundle (`*.md`, ohne `index.md`/`log.md`) -- dieselbe
/// Ausschlussregel wie `okf::bundle::collect_md_files`.
fn zaehle_md_dokumente(root: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, count: &mut usize) {
        for entry in std::fs::read_dir(dir).expect("Verzeichnis muss lesbar sein") {
            let entry = entry.expect("Eintrag muss lesbar sein");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name != "index.md" && name != "log.md" {
                    *count += 1;
                }
            }
        }
    }
    let mut count = 0;
    walk(root, &mut count);
    count
}

/// Pfad (relativ) -> Byte-Inhalt aller Dateien unter `root` -- fuer den
/// Nachweis, dass ein Lesepfad am Bundle wortwoertlich nichts aendert.
fn snapshot_dateien(root: &std::path::Path) -> std::collections::HashMap<String, Vec<u8>> {
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut std::collections::HashMap<String, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(dir).expect("Verzeichnis muss lesbar sein") {
            let entry = entry.expect("Eintrag muss lesbar sein");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn commit_ist_sofort_sichtbar_und_erhoeht_die_revision() {
    let store = GraphStore::in_memory();
    assert_eq!(store.revision(), 0);

    let receipt = store.submit(claim("A", "nutzt", "B"), &access()).unwrap();
    assert_eq!(receipt.revision, 1);

    // Read-your-writes ohne Wartemechanik: der Snapshot NACH dem Receipt hat sie.
    let index = store.snapshot();
    assert_eq!(index.revision(), 1);
    let id = receipt.claim_id.unwrap();
    assert_eq!(index.claim(&id).unwrap().predicate, "nutzt");
    assert_eq!(index.entity_count(), 2);
}

#[test]
fn ein_claim_traegt_immer_autor_lauf_und_quelle() {
    let store = GraphStore::in_memory();
    let access = access();
    let receipt = store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "Test T-41",
                "belegt",
                "Hypothese H-12",
                SourceDraft::new("test_run").excerpt("cargo test -- mcp"),
            )),
            &access,
        )
        .unwrap();

    let index = store.snapshot();
    let claim = index.claim(receipt.claim_id.as_ref().unwrap()).unwrap();
    assert_eq!(claim.created_by.as_str(), "agent:tester");
    assert_eq!(claim.source_ids.len(), 1);

    let source = index.source(&claim.source_ids[0]).unwrap();
    assert_eq!(
        source.agent_id.as_ref().map(|a| a.as_str()),
        Some("agent:tester")
    );
    assert_eq!(source.run_id.as_deref(), Some("run-1"));
    assert_eq!(source.excerpt.as_deref(), Some("cargo test -- mcp"));
}

#[test]
fn gleicher_name_loest_auf_dieselbe_entity_auf() {
    let store = GraphStore::in_memory();
    let a = access();
    store
        .submit(claim("MCP-Client", "nutzt", "stdio"), &a)
        .unwrap();
    store
        .submit(claim("mcp client", "blockiert", "Tool-Aufruf"), &a)
        .unwrap();

    let index = store.snapshot();
    // "MCP-Client" und "mcp client" normalisieren gleich -> eine Entity, drei insgesamt.
    assert_eq!(index.entity_count(), 3);
    let claims: Vec<_> = index.claims().collect();
    assert_eq!(claims[0].subject, claims[1].subject);
}

#[test]
fn identische_aussage_wird_zusammengefuehrt_statt_verdoppelt() {
    let store = GraphStore::in_memory();
    let a = access();
    let first = store.submit(claim("A", "nutzt", "B"), &a).unwrap();
    let second = store
        .submit(
            GraphWriteCommand::RecordClaim(
                ClaimDraft::new("A", "nutzt", "B", SourceDraft::new("document")).confidence(0.9),
            ),
            &a,
        )
        .unwrap();

    assert_eq!(first.claim_id, second.claim_id);
    assert!(second.deduplicated);

    let index = store.snapshot();
    assert_eq!(index.claim_count(), 1);
    let claim = index.claim(&second.claim_id.unwrap()).unwrap();
    // Zwei Belege, und die höhere Konfidenz gewinnt.
    assert_eq!(claim.source_ids.len(), 2);
    assert!((claim.confidence - 0.9).abs() < f32::EPSILON);
}

#[test]
fn nur_lesender_zugriff_kann_nicht_schreiben() {
    let store = GraphStore::in_memory();
    let view = GraphView::new(vec![GraphTarget::canonical(GraphScope::workspace("ws"))]);
    let ro = GraphAccess::read_only("leser", view);

    let err = store.submit(claim("A", "nutzt", "B"), &ro).unwrap_err();
    assert!(matches!(err, GraphError::Denied(_)));
    assert_eq!(store.revision(), 0);
}

#[test]
fn confirmed_kann_nicht_direkt_geschrieben_werden() {
    let store = GraphStore::in_memory();
    let draft = ClaimDraft::new("A", "nutzt", "B", SourceDraft::new("test_run"))
        .status(ClaimStatus::Confirmed);
    let err = store
        .submit(GraphWriteCommand::RecordClaim(draft), &access())
        .unwrap_err();
    assert!(matches!(err, GraphError::Invalid(_)));
}

#[test]
fn schreibziel_ausserhalb_der_eigenen_sicht_wird_abgelehnt() {
    let store = GraphStore::in_memory();
    let mut a = access();
    // Sicht kaputtmachen: Schreibziel entfernen -> der Agent sähe seine eigene
    // Beobachtung nicht wieder.
    a.view = GraphView::new(vec![GraphTarget::canonical(GraphScope::workspace("ws"))]);
    a.write = Some(GraphTarget::new(
        GraphLayer::Working,
        GraphScope::session("fremd"),
    ));

    let err = store.submit(claim("A", "nutzt", "B"), &a).unwrap_err();
    assert!(matches!(err, GraphError::Denied(_)));
}

#[test]
fn leere_felder_werden_abgelehnt() {
    let store = GraphStore::in_memory();
    let err = store.submit(claim("A", "  ", "B"), &access()).unwrap_err();
    assert!(matches!(err, GraphError::Invalid(_)));
}

#[test]
fn journal_ueberlebt_einen_neustart() {
    let dir = tmp_dir("restart");
    let a = access();
    let claim_id = {
        let store = GraphStore::open(&dir).unwrap();
        let receipt = store
            .submit(claim("Deadlock", "entsteht durch", "Mutex"), &a)
            .unwrap();
        store
            .submit(
                GraphWriteCommand::RecordEpisode(agentkit_graph::EpisodeDraft::new(
                    "tester hat T-41 ausgeführt",
                    SourceDraft::new("test_run"),
                )),
                &a,
            )
            .unwrap();
        receipt.claim_id.unwrap()
    };

    let wieder = GraphStore::open(&dir).unwrap();
    let index = wieder.snapshot();
    assert_eq!(index.revision(), 2);
    assert_eq!(index.claim(&claim_id).unwrap().predicate, "entsteht durch");
    assert_eq!(index.episode_count(), 1);

    // IDs laufen nach dem Neustart weiter, statt vergebene zu überschreiben.
    let receipt = wieder.submit(claim("A", "nutzt", "B"), &a).unwrap();
    assert_eq!(receipt.claim_id.as_deref(), Some("C-2"));
    assert_eq!(receipt.revision, 3);

    std::fs::remove_dir_all(&dir).ok();
}

/// Ersetzt `journal_neuschreiben_faltet_aenderungen_zusammen`: ein Bundle
/// kennt keine Journal-Kompaktierung mehr, weil es nichts zu falten gibt --
/// ein Datensatz hat schon nach jedem einzelnen Commit genau ein Dokument,
/// unabhängig davon, wie oft dieselbe Aussage belegt wurde.
#[test]
fn ein_datensatz_hat_immer_genau_ein_dokument() {
    let dir = tmp_dir("bundle_docs");
    let store = GraphStore::open(&dir).unwrap();
    let a = access();
    for i in 0..10 {
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    "A",
                    "nutzt",
                    "B",
                    SourceDraft::new("document").excerpt(&format!("Fundstelle {i}")),
                )),
                &a,
            )
            .unwrap();
        // Schon nach JEDEM einzelnen Submit: kein Anwachsen, das erst später
        // zusammengefaltet werden müsste.
        assert_eq!(zaehle_md_dokumente(&dir), store.stats().entities);
    }

    let stats = store.stats();
    assert_eq!(
        stats.claims, 1,
        "identische Aussagen werden zusammengefuehrt"
    );
    assert_eq!(stats.sources, 10);

    // Ein expliziter Rebuild ändert daran nichts.
    store.rebuild_bundle().unwrap();
    assert_eq!(zaehle_md_dokumente(&dir), stats.entities);

    let wieder = GraphStore::open(&dir).unwrap();
    assert_eq!(wieder.stats().claims, 1);
    assert_eq!(wieder.stats().sources, 10);
    assert_eq!(wieder.revision(), store.revision());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn kaputte_journal_zeile_ist_ein_harter_fehler() {
    let dir = tmp_dir("broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(agentkit_graph::JOURNAL_FILE),
        "{\"schema_version\":\"99\",\"revision\":1,\"at\":0,\"op\":{\"record\":\"source\"}}\n",
    )
    .unwrap();

    let err = GraphStore::open(&dir).unwrap_err();
    assert!(matches!(err, GraphError::Journal(_)), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn store_ist_ueber_threads_teilbar() {
    // Der Typ MUSS Send+Sync sein — sonst kann ihn kein Schwarm-Actor halten.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<GraphStore>>();
}

// ------------------------------------------------------------- Migration

/// Ein Verzeichnis aus der Zeit vor der OKF-Umstellung: nur `graph.jsonl`, im
/// alten Format. `GraphStore::open` migriert es beim ersten Öffnen zu einem
/// Bundle -- derselbe Datenbestand, blanke Principals normalisiert.
#[test]
fn legacy_journal_wird_beim_oeffnen_zu_einem_bundle_migriert() {
    let dir = tmp_dir("migration");
    std::fs::create_dir_all(&dir).unwrap();
    let inhalt = format!(
        "{}\n{}\n{}\n{}\n",
        legacy_line(1, &legacy_source("S-1", 1, "tester")),
        legacy_line(2, &legacy_entity("E-1", "MCP-Client", 2)),
        legacy_line(3, &legacy_entity("E-2", "stdio-Session", 3)),
        legacy_line(4, &legacy_claim("C-1", "E-1", "E-2", 4, "S-1", "tester")),
    );
    std::fs::write(dir.join(agentkit_graph::JOURNAL_FILE), inhalt).unwrap();

    let store = GraphStore::open(&dir).unwrap();
    let index = store.snapshot();
    assert_eq!(index.revision(), 4);
    assert_eq!(index.entity_count(), 2);
    assert_eq!(index.claim_count(), 1);
    assert_eq!(index.source_count(), 1);

    let claim = index.claim("C-1").unwrap();
    assert_eq!(
        claim.created_by.as_str(),
        "agent:tester",
        "der blanke Legacy-Principal wird beim Einlesen normalisiert"
    );
    let quelle = index.source("S-1").unwrap();
    assert_eq!(
        quelle.agent_id.as_ref().map(|a| a.as_str()),
        Some("agent:tester")
    );

    assert!(
        dir.join(agentkit_graph::BUNDLE_INDEX).exists(),
        "Bundle-Index muss stehen"
    );
    assert!(
        !dir.join(agentkit_graph::JOURNAL_FILE).exists(),
        "das alte Journal ist fort"
    );
    assert!(
        dir.join(agentkit_graph::MIGRATED_JOURNAL).exists(),
        "es liegt umbenannt daneben"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Ein zweites Öffnen nach der Migration nimmt den Bundle-Zweig: keine
/// erneute Migration, die umbenannte Datei bleibt byte-identisch liegen.
#[test]
fn zweites_oeffnen_nach_der_migration_migriert_nicht_erneut() {
    let dir = tmp_dir("migration_zweimal");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(agentkit_graph::JOURNAL_FILE),
        format!(
            "{}\n{}\n",
            legacy_line(1, &legacy_source("S-1", 1, "tester")),
            legacy_line(2, &legacy_entity("E-1", "MCP-Client", 2)),
        ),
    )
    .unwrap();

    let erster = GraphStore::open(&dir).unwrap();
    assert_eq!(erster.revision(), 2);
    let migriert_vorher = std::fs::read(dir.join(agentkit_graph::MIGRATED_JOURNAL)).unwrap();

    let zweiter = GraphStore::open(&dir).unwrap();
    assert_eq!(zweiter.revision(), 2);
    assert_eq!(zweiter.stats().entities, 1);

    let migriert_nachher = std::fs::read(dir.join(agentkit_graph::MIGRATED_JOURNAL)).unwrap();
    assert_eq!(
        migriert_vorher, migriert_nachher,
        "keine zweite Migration ruehrt die umbenannte Datei an"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// -------------------------------------------------------------- Neuanlage

/// `--graph` auf ein Notiz- oder Doku-Verzeichnis gerichtet — ein Tippfehler,
/// der sonst unwiederbringlich Daten kostet: die Neuanlage schreibt ein leeres
/// Bundle, und dessen Aufräumen entfernt JEDE fremde `.md`-Datei darunter.
/// Deshalb muss das Anlegen hier scheitern, bevor irgendetwas geschrieben wird.
#[test]
fn eine_neuanlage_in_einem_fremden_markdown_verzeichnis_wird_verweigert() {
    let dir = tmp_dir("fremdes-verzeichnis");
    std::fs::create_dir_all(dir.join("unterordner")).unwrap();
    std::fs::write(dir.join("NOTIZEN.md"), "# Wichtig\n").unwrap();
    std::fs::write(dir.join("unterordner/mehr.md"), "# Auch wichtig\n").unwrap();

    let fehler = GraphStore::open(&dir).expect_err("darf kein Bundle anlegen");
    let text = fehler.to_string();
    assert!(
        text.contains("kein Graph-Bundle"),
        "Meldung muss die Ursache nennen: {text}"
    );

    // Und vor allem: nichts wurde angefasst.
    assert_eq!(
        std::fs::read_to_string(dir.join("NOTIZEN.md")).unwrap(),
        "# Wichtig\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("unterordner/mehr.md")).unwrap(),
        "# Auch wichtig\n"
    );
    assert!(
        !dir.join(agentkit_graph::BUNDLE_INDEX).exists(),
        "es darf kein index.md entstanden sein"
    );
}

/// Ein leeres Verzeichnis wird zu einem frischen Bundle: `index.md` entsteht
/// sofort, ein Submit plus erneutes Öffnen liefert denselben Stand.
#[test]
fn neuanlage_legt_ein_leeres_bundle_an_und_uebersteht_einen_neustart() {
    let dir = tmp_dir("neuanlage");
    let store = GraphStore::open(&dir).unwrap();
    assert!(
        dir.join(agentkit_graph::BUNDLE_INDEX).exists(),
        "index.md entsteht schon beim ersten Oeffnen"
    );
    assert_eq!(store.revision(), 0);

    store.submit(claim("A", "nutzt", "B"), &access()).unwrap();
    store
        .submit(
            GraphWriteCommand::RecordEpisode(agentkit_graph::EpisodeDraft::new(
                "tester hat etwas getan",
                SourceDraft::new("test_run"),
            )),
            &access(),
        )
        .unwrap();

    let wieder = GraphStore::open(&dir).unwrap();
    assert_eq!(wieder.revision(), store.revision());
    assert_eq!(wieder.stats(), store.stats());

    std::fs::remove_dir_all(&dir).ok();
}

// ----------------------------------------------------- Sperrfreier Lesepfad

/// `open_read_only` liefert denselben Stand wie `open` — aber OHNE jede
/// Schreibwirkung: kein angelegtes Verzeichnis, kein verändertes Bundle. Ein
/// Leser darf die Dateien eines lebenden Schreibers nicht anfassen.
#[test]
fn open_read_only_liest_denselben_stand_ohne_zu_schreiben() {
    let dir = tmp_dir("readonly");
    {
        let store = GraphStore::open(&dir).unwrap();
        store.submit(claim("A", "nutzt", "B"), &access()).unwrap();
        store.submit(claim("B", "braucht", "C"), &access()).unwrap();
    }
    let vorher = snapshot_dateien(&dir);

    let index = GraphStore::open_read_only(&dir).unwrap();
    assert_eq!(index.claim_count(), 2);
    assert_eq!(index.entity_count(), 3);
    assert_eq!(index.revision(), 2);
    assert_eq!(
        snapshot_dateien(&dir),
        vorher,
        "der Lesepfad darf am Bundle nichts aendern"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein noch gar nicht angelegter Graph ist für den Leser leer, kein Fehler —
/// und er legt das Verzeichnis NICHT an (anders als `open`).
#[test]
fn open_read_only_legt_kein_verzeichnis_an() {
    let dir = tmp_dir("readonly_leer");
    let index = GraphStore::open_read_only(&dir).unwrap();
    assert_eq!(index.claim_count(), 0);
    assert!(!dir.exists(), "der Lesepfad legt nichts an");
}

/// `open_read_only` fasst ein noch nicht migriertes Legacy-Verzeichnis nicht
/// an: es liest, migriert aber nicht -- `graph.jsonl` bleibt liegen, kein
/// Bundle entsteht.
#[test]
fn open_read_only_migriert_kein_legacy_verzeichnis() {
    let dir = tmp_dir("readonly_legacy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(agentkit_graph::JOURNAL_FILE),
        format!(
            "{}\n",
            legacy_line(1, &legacy_entity("E-1", "MCP-Client", 1))
        ),
    )
    .unwrap();

    let index = GraphStore::open_read_only(&dir).unwrap();
    assert_eq!(index.entity_count(), 1);
    assert!(
        dir.join(agentkit_graph::JOURNAL_FILE).exists(),
        "der Lesepfad migriert nicht"
    );
    assert!(
        !dir.join(agentkit_graph::BUNDLE_INDEX).exists(),
        "kein Bundle entsteht"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Der Export ist die vollständige, serialisierbare Projektion — inklusive der
/// Quellen, für die es bis dahin nur den Einzel-Lookup gab.
#[test]
fn export_liefert_den_ganzen_graphen_inklusive_quellen() {
    let store = GraphStore::in_memory();
    store.submit(claim("A", "nutzt", "B"), &access()).unwrap();

    let export = agentkit_graph::export(&store.snapshot());
    assert_eq!(export.revision, 1);
    assert_eq!(export.entities.len(), 2);
    assert_eq!(export.claims.len(), 1);
    assert_eq!(export.sources.len(), 1, "die Quelle des Claims");
    // Die Provenance ist über die Quellen-IDs auflösbar — genau das braucht
    // eine Anzeige, die auf eine Kante klickt.
    let quelle = &export.claims[0].source_ids[0];
    assert!(export.sources.iter().any(|s| &s.id == quelle));
    // Und es ist wirklich serialisierbar (kein `Arc`, keine Lebensdauer).
    let json = serde_json::to_value(&export).unwrap();
    assert_eq!(json["claims"][0]["predicate"], "nutzt");
}

/// Ein Leser trifft ein Legacy-Verzeichnis mitten in einem alten Append: die
/// letzte Zeile ist halb da. Der LESEPFAD (`open_read_only`) toleriert das,
/// sonst bräche eine Anzeige, die im Sekundentakt liest, sporadisch ab. Der
/// migrierende Pfad (`open`) bleibt streng — dort wäre ein verschlucktes
/// Fragment ein echtes Datenproblem, das niemand sonst gerade repariert.
#[test]
fn open_read_only_toleriert_eine_halbe_letzte_zeile_im_legacy_journal() {
    let dir = tmp_dir("legacy_halbe_zeile");
    std::fs::create_dir_all(&dir).unwrap();
    let vollstaendig = legacy_line(1, &legacy_entity("E-1", "MCP-Client", 1));
    let angefangen = legacy_line(2, &legacy_entity("E-2", "stdio-Session", 2));
    let halb = &angefangen[..angefangen.len() - 10];
    std::fs::write(
        dir.join(agentkit_graph::JOURNAL_FILE),
        format!("{vollstaendig}\n{halb}"),
    )
    .unwrap();

    let index = GraphStore::open_read_only(&dir).unwrap();
    assert_eq!(
        index.entity_count(),
        1,
        "nur die vollstaendige Zeile darf ankommen"
    );

    assert!(
        GraphStore::open(&dir).is_err(),
        "der migrierende Pfad meldet die kaputte Zeile"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------ Abbruch beim Schreiben

/// Scheitert der Bundle-Commit, bleibt der alte Snapshot stehen und keine ID
/// ist verbraucht. Simuliert wird das portabel (keine Dateisystemrechte
/// nötig): am Zielverzeichnis des ersten Claims (`working/session-run-1`)
/// liegt bereits eine gewöhnliche DATEI -- `create_dir_all` scheitert daran
/// auf jedem Betriebssystem gleich, ein brauchbarer Stellvertreter für einen
/// Absturz mitten im Schreiben.
#[test]
fn commit_scheitert_laesst_den_snapshot_unveraendert() {
    let dir = tmp_dir("commit_scheitert");
    std::fs::create_dir_all(dir.join("working")).unwrap();
    std::fs::write(dir.join("working").join("session-run-1"), b"blockiert").unwrap();

    let store = GraphStore::open(&dir).unwrap();
    assert_eq!(store.revision(), 0);

    let err = store
        .submit(claim("A", "nutzt", "B"), &access())
        .unwrap_err();
    assert!(matches!(err, GraphError::Io(_)), "{err}");
    assert_eq!(
        store.revision(),
        0,
        "kein Snapshot getauscht, keine ID verbraucht"
    );

    std::fs::remove_dir_all(&dir).ok();
}
