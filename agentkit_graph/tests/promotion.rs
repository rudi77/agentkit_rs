//! Promotion: der einzige Weg von „vorläufig" nach „dauerhaft".

use agentkit_graph::retrieval::{self, GraphQuery};
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    ClaimDraft, ClaimStatus, GraphAccess, GraphError, GraphLayer, GraphScope, GraphTarget,
    GraphView, GraphWriteCommand, SourceDraft,
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

#[test]
fn promotion_macht_aus_einer_beobachtung_dauerhaftes_wissen() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let id = schreibe(&store, &access, "MCP Client", "nutzt", "stdio-Session");

    let receipt = store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &access,
        )
        .unwrap();
    assert_eq!(receipt.claim_id.as_deref(), Some(id.as_str()));

    let index = store.snapshot();
    let claim = index.claim(&id).unwrap();
    assert_eq!(claim.status, ClaimStatus::Confirmed);
    assert_eq!(claim.layer, GraphLayer::Canonical);
    assert_eq!(claim.scope, GraphScope::workspace("ws"));
    // Die Herkunft bleibt sichtbar, die Quellen bleiben dieselben.
    assert_eq!(claim.promoted_from, Some(GraphScope::session("run-1")));
    assert_eq!(claim.source_ids.len(), 1);

    // Beide Endpunkte wandern mit — sonst zeigte kanonisches Wissen auf
    // Entities, die mit dem Session-Scope verschwinden.
    for endpoint in [&claim.subject, &claim.object] {
        let entity = index.entity(endpoint).unwrap();
        assert_eq!(entity.layer, GraphLayer::Canonical);
        assert_eq!(entity.scope, GraphScope::workspace("ws"));
    }
}

#[test]
fn promotiertes_wissen_ist_fuer_andere_laeufe_sichtbar() {
    let store = GraphStore::in_memory();
    let erster = GraphAccess::session("tester", "ws", "run-1");
    let id = schreibe(
        &store,
        &erster,
        "Deadlock",
        "entsteht durch",
        "Session-Mutex",
    );

    // Vor der Promotion sieht ein späterer Lauf nichts.
    let spaeter = GraphAccess::session("tester", "ws", "run-2");
    let vorher = retrieval::search(
        &store.snapshot(),
        &spaeter.view,
        &GraphQuery::text("Wodurch entsteht der Deadlock?"),
    );
    assert!(vorher.is_empty());

    store
        .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &erster)
        .unwrap();

    let nachher = retrieval::search(
        &store.snapshot(),
        &spaeter.view,
        &GraphQuery::text("Wodurch entsteht der Deadlock?"),
    );
    assert_eq!(nachher.claims.len(), 1);
    assert_eq!(nachher.claims[0].claim.status, ClaimStatus::Confirmed);
}

#[test]
fn widersprechendes_kanonisches_wissen_wird_ersetzt_nicht_geloescht() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");

    let alt = schreibe(&store, &access, "MCP Client", "nutzt", "Synchrone Session");
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: alt.clone(),
            },
            &access,
        )
        .unwrap();

    let neu = schreibe(&store, &access, "MCP Client", "nutzt", "Async Session");
    let receipt = store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: neu.clone(),
            },
            &access,
        )
        .unwrap();

    assert_eq!(receipt.superseded, vec![alt.clone()]);
    let index = store.snapshot();
    let alter = index.claim(&alt).unwrap();
    // Die alte Aussage bleibt lesbar — als Evidenz, mit Verweis auf die neue.
    assert_eq!(alter.status, ClaimStatus::Superseded);
    assert_eq!(alter.superseded_by.as_deref(), Some(neu.as_str()));
    assert_eq!(index.claim(&neu).unwrap().status, ClaimStatus::Confirmed);
}

#[test]
fn ohne_promotionsziel_geht_nichts() {
    let store = GraphStore::in_memory();
    let nur_arbeit = GraphAccess::working(
        "tester",
        GraphView::default(),
        GraphTarget::new(GraphLayer::Working, GraphScope::session("run-1")),
    );
    let id = schreibe(&store, &nur_arbeit, "A", "nutzt", "B");

    let err = store
        .submit(
            GraphWriteCommand::PromoteClaim { claim_id: id },
            &nur_arbeit,
        )
        .unwrap_err();
    assert!(matches!(err, GraphError::Denied(_)), "{err}");
}

#[test]
fn fremdes_und_unbekanntes_kann_nicht_promotet_werden() {
    let store = GraphStore::in_memory();
    let fremder = GraphAccess::session("developer", "ws", "run-fremd");
    let id = schreibe(&store, &fremder, "Privat", "gehört", "run-fremd");

    let eigener = GraphAccess::session("tester", "ws", "run-1");
    // Nicht sichtbar heißt nicht vorhanden — kein Weg, fremde Scopes anzufassen.
    let err = store
        .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &eigener)
        .unwrap_err();
    assert!(matches!(err, GraphError::NotFound(_)), "{err}");

    let err = store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: "C-999".into(),
            },
            &eigener,
        )
        .unwrap_err();
    assert!(matches!(err, GraphError::NotFound(_)), "{err}");
}

#[test]
fn zweimal_promotieren_ist_ein_fehler() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let id = schreibe(&store, &access, "A", "nutzt", "B");
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &access,
        )
        .unwrap();

    let err = store
        .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &access)
        .unwrap_err();
    assert!(matches!(err, GraphError::Invalid(_)), "{err}");
}

#[test]
fn privates_wird_erst_durch_promotion_geteilt() {
    let store = GraphStore::in_memory();
    let geteilt = GraphTarget::new(GraphLayer::Working, GraphScope::swarm("run-4711"));
    // Der Reviewer denkt privat und veröffentlicht bewusst.
    let privat = GraphAccess::working(
        "reviewer",
        GraphView::default(),
        GraphTarget::new(
            GraphLayer::Working,
            GraphScope::agent("reviewer", "run-4711"),
        ),
    )
    .with_promotion(geteilt.clone());
    let developer = GraphAccess::swarm("developer", "ws", "run-4711");

    let id = schreibe(&store, &privat, "Änderung X", "bricht", "Test T-9");
    let unsichtbar = retrieval::search(
        &store.snapshot(),
        &developer.view,
        &GraphQuery::text("Was bricht Test T-9?"),
    );
    assert!(unsichtbar.is_empty());

    store
        .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &privat)
        .unwrap();

    let sichtbar = retrieval::search(
        &store.snapshot(),
        &developer.view,
        &GraphQuery::text("Was bricht Test T-9?"),
    );
    assert_eq!(sichtbar.claims.len(), 1);
    // Der Ursprung bleibt nachvollziehbar.
    assert_eq!(
        sichtbar.claims[0].claim.promoted_from,
        Some(GraphScope::agent("reviewer", "run-4711"))
    );
    assert_eq!(sichtbar.claims[0].claim.created_by, "reviewer");
}
