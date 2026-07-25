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
    assert_eq!(claim.created_by, "tester");
    assert_eq!(claim.source_ids.len(), 1);

    let source = index.source(&claim.source_ids[0]).unwrap();
    assert_eq!(source.agent_id.as_deref(), Some("tester"));
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

#[test]
fn journal_neuschreiben_faltet_aenderungen_zusammen() {
    let dir = tmp_dir("compact");
    let store = GraphStore::open(&dir).unwrap();
    let a = access();
    // Ein Claim, zehnmal belegt: 1 Quelle + 1 Claim je Mutation, aber am Ende
    // stehen nur wenige Datensätze im Graphen.
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
    }
    let vorher = store.journal_lines().unwrap();
    store.compact_journal().unwrap();
    let nachher = store.journal_lines().unwrap();
    assert!(nachher < vorher, "{nachher} < {vorher}");

    let stats = store.stats();
    assert_eq!(
        nachher as usize,
        stats.entities + stats.claims + stats.sources
    );

    // Und der neu geschriebene Stand lädt identisch.
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
