//! Verlernen: eine zurückgezogene Aussage bleibt Evidenz, ist aber kein Wissen mehr.

use agentkit_graph::retrieval::{self, GraphQuery};
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    ClaimDraft, ClaimStatus, GraphAccess, GraphError, GraphScope, GraphTarget, GraphView,
    GraphWriteCommand, SourceDraft,
};

fn schreibe(store: &GraphStore, access: &GraphAccess, s: &str, p: &str, o: &str) -> String {
    store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                s,
                p,
                o,
                SourceDraft::new("test_run").excerpt("cargo test"),
            )),
            access,
        )
        .unwrap()
        .claim_id
        .unwrap()
}

fn zieht_zurueck(
    store: &GraphStore,
    access: &GraphAccess,
    id: &str,
) -> Result<agentkit_graph::GraphReceipt, GraphError> {
    store.submit(
        GraphWriteCommand::RetractClaim {
            claim_id: id.to_string(),
            reason: "war ein Lesefehler".to_string(),
        },
        access,
    )
}

fn promote(store: &GraphStore, access: &GraphAccess, id: &str) {
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.to_string(),
            },
            access,
        )
        .unwrap();
}

#[test]
fn zurueckgezogenes_faellt_aus_dem_retrieval_und_bleibt_als_evidenz() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let falsch = schreibe(&store, &access, "MCP Client", "nutzt", "HTTP");
    let richtig = schreibe(&store, &access, "MCP Client", "spricht", "JSON-RPC");

    zieht_zurueck(&store, &access, &falsch).unwrap();

    let index = store.snapshot();
    let claim = index.claim(&falsch).unwrap();
    assert_eq!(claim.status, ClaimStatus::Retracted);
    let retraction = claim.retracted.as_ref().unwrap();
    assert_eq!(retraction.by.as_str(), "agent:tester");
    assert_eq!(retraction.reason, "war ein Lesefehler");

    let gefunden = retrieval::search(&index, &access.view, &GraphQuery::text("MCP Client"));
    let ids: Vec<&str> = gefunden
        .claims
        .iter()
        .map(|c| c.claim.id.as_str())
        .collect();
    assert_eq!(ids, vec![richtig.as_str()]);

    // Evidenz bleibt lesbar — und wer ausdrücklich danach fragt, findet sie.
    assert_eq!(retrieval::evidence(&index, &falsch).len(), 1);
    let mut ausdruecklich = GraphQuery::text("MCP Client");
    ausdruecklich.include_statuses = vec![ClaimStatus::Retracted];
    let nur_zurueckgezogen = retrieval::search(&index, &access.view, &ausdruecklich);
    assert_eq!(nur_zurueckgezogen.claims.len(), 1);
    assert_eq!(nur_zurueckgezogen.claims[0].claim.id, falsch);
}

/// Wer die zurückgezogene Aussage erneut macht, bekommt eine neue — sie wird
/// nicht still an die unsichtbare alte angehängt.
#[test]
fn eine_erneute_aussage_belebt_die_zurueckgezogene_nicht_wieder() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let alt = schreibe(&store, &access, "A", "nutzt", "B");
    zieht_zurueck(&store, &access, &alt).unwrap();

    let neu = schreibe(&store, &access, "A", "nutzt", "B");
    assert_ne!(neu, alt);
    let index = store.snapshot();
    assert_eq!(index.claim(&alt).unwrap().status, ClaimStatus::Retracted);
    assert_eq!(index.claim(&neu).unwrap().status, ClaimStatus::Observation);
}

#[test]
fn auch_dauerhaftes_wissen_laesst_sich_zurueckziehen() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let id = schreibe(&store, &access, "A", "nutzt", "B");
    promote(&store, &access, &id);

    zieht_zurueck(&store, &access, &id).unwrap();

    // Ein späterer Lauf sieht das kanonische Wissen — und diese Aussage nicht mehr.
    let spaeter = GraphAccess::session("tester", "ws", "run-2");
    let gefunden = retrieval::search(&store.snapshot(), &spaeter.view, &GraphQuery::text("A"));
    assert!(gefunden.is_empty());

    // Eine widersprechende Beförderung markiert sie nicht nachträglich als „ersetzt".
    let neu = schreibe(&store, &access, "A", "nutzt", "C");
    promote(&store, &access, &neu);
    assert_eq!(
        store.snapshot().claim(&id).unwrap().status,
        ClaimStatus::Retracted
    );
}

#[test]
fn zurueckziehen_darf_nur_wer_dort_schreiben_darf() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let vorlaeufig = schreibe(&store, &access, "A", "nutzt", "B");
    let dauerhaft = schreibe(&store, &access, "C", "nutzt", "D");
    promote(&store, &access, &dauerhaft);

    // Ein Leser sieht beides und darf nichts davon zurückziehen.
    let leser = GraphAccess::read_only("leser", access.view.clone());
    for id in [&vorlaeufig, &dauerhaft] {
        assert!(matches!(
            zieht_zurueck(&store, &leser, id),
            Err(GraphError::Denied(_))
        ));
    }

    // Wer nur vorläufig schreiben darf, kommt an Dauerhaftes nicht heran.
    let nur_arbeit = GraphAccess::working(
        "tester",
        GraphView::new(vec![GraphTarget::canonical(GraphScope::workspace("ws"))]),
        GraphTarget::working(GraphScope::session("run-1")),
    );
    assert!(matches!(
        zieht_zurueck(&store, &nur_arbeit, &dauerhaft),
        Err(GraphError::Denied(_))
    ));
    zieht_zurueck(&store, &nur_arbeit, &vorlaeufig).unwrap();

    // Außerhalb der Sicht: dieselbe Antwort wie für eine unbekannte ID.
    let fremd = GraphAccess::session("fremd", "anderer-ws", "run-9");
    assert!(matches!(
        zieht_zurueck(&store, &fremd, &dauerhaft),
        Err(GraphError::NotFound(_))
    ));
}

#[test]
fn der_grund_ueberlebt_den_neustart() {
    let dir = std::env::temp_dir().join(format!("graph_retract_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let access = GraphAccess::session("tester", "ws", "run-1");
    let id = {
        let store = GraphStore::open(&dir).unwrap();
        let id = schreibe(&store, &access, "A", "nutzt", "B");
        zieht_zurueck(&store, &access, &id).unwrap();
        id
    };

    let wieder = GraphStore::open(&dir).unwrap();
    let claim = wieder.snapshot().claim(&id).unwrap().clone();
    assert_eq!(claim.status, ClaimStatus::Retracted);
    let retraction = claim.retracted.as_ref().expect("Grund im Dokument");
    assert_eq!(retraction.reason, "war ein Lesefehler");
    assert_eq!(retraction.by.as_str(), "agent:tester");
    std::fs::remove_dir_all(&dir).ok();
}
