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
    assert_eq!(
        sichtbar.claims[0].claim.created_by.as_str(),
        "agent:reviewer"
    );
}

/// Der Bundle-Rebuild darf die Promotions-Spur nicht verschlucken.
///
/// `promote_claim` behält bewusst die Claim-ID, statt einen neuen Claim
/// anzulegen — die Begründung dafür war, die Vorversion stehe „weiterhin im
/// Bundle". Das stimmt nur, weil `rebuild_bundle` den AKTUELLEN Index neu
/// ausschreibt und die Working-Datei danach fort ist. Was die Promotion
/// belegbar macht, muss deshalb am überlebenden Datensatz stehen.
#[test]
fn die_promotions_spur_ueberlebt_den_bundle_rebuild() {
    let dir = std::env::temp_dir().join(format!("graph_promo_kompakt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let store = GraphStore::open(&dir).unwrap();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let id = schreibe(&store, &access, "MCP Client", "nutzt", "stdio-Session");
    let vorher = store.snapshot();
    let vor_promotion = vorher.claim(&id).unwrap().clone();
    assert_eq!(vor_promotion.status, ClaimStatus::Observation);

    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &access,
        )
        .unwrap();
    store.rebuild_bundle().unwrap();

    // Neu geladen — nur noch das, was der Rebuild geschrieben hat.
    let wieder = GraphStore::open(&dir).unwrap();
    let claim = wieder.snapshot().claim(&id).unwrap().clone();
    assert_eq!(claim.status, ClaimStatus::Confirmed);
    assert_eq!(claim.layer, GraphLayer::Canonical);
    assert_eq!(
        claim.promoted_from.as_ref(),
        Some(&GraphScope::session("run-1")),
        "aus welchem Scope promotet wurde"
    );
    assert_eq!(
        claim.promoted_from_status,
        Some(ClaimStatus::Observation),
        "und was er vorher war — sonst ist nicht unterscheidbar, \
         ob hier eine Beobachtung oder eine Vermutung dauerhaft wurde"
    );

    // Die mitgewanderten Entities tragen dieselbe Spur.
    let subjekt = wieder.snapshot().entity(&claim.subject).unwrap().clone();
    assert_eq!(
        subjekt.promoted_from.as_ref(),
        Some(&GraphScope::session("run-1")),
        "auch die Entity kam aus dem Working-Scope"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Die Promotion belegt nicht nur WOHER ein Claim kam, sondern auch WER ihn
/// WANN bestätigt hat (OKF §5.2/§5.3) — und zwei unabhängige Promotionen
/// desselben Claims hängen zwei Bestätigungen an, statt die erste zu ersetzen.
#[test]
fn promotion_haengt_eine_verifikation_an_und_eine_zweite_kommt_dazu() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws-a", "run-1");
    let id = schreibe(&store, &access, "MCP Client", "nutzt", "stdio-Session");

    let vor_erster_promotion = agentkit_graph::model::now_ms();
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &access,
        )
        .unwrap();

    let nach_erster = store.snapshot().claim(&id).unwrap().clone();
    assert_eq!(nach_erster.verified.len(), 1, "{:?}", nach_erster.verified);
    assert_eq!(
        nach_erster.verified[0].by,
        agentkit_graph::model::Actor::from_principal("tester")
    );
    assert!(
        nach_erster.verified[0].at >= vor_erster_promotion,
        "die Verifikation trägt einen plausiblen Zeitstempel"
    );

    // Eine zweite, unabhängige Promotion DESSELBEN Claims — hier durch einen
    // anderen Principal in ein zweites kanonisches Ziel (ein zweiter
    // Workspace, der dasselbe Wissen ebenfalls übernimmt). Die erste
    // Bestätigung bleibt stehen, die zweite kommt dazu.
    let zweites_ziel = GraphTarget::canonical(GraphScope::workspace("ws-b"));
    let reviewer = access.as_principal("reviewer").with_promotion(zweites_ziel);
    store
        .submit(
            GraphWriteCommand::PromoteClaim {
                claim_id: id.clone(),
            },
            &reviewer,
        )
        .unwrap();

    let nach_zweiter = store.snapshot().claim(&id).unwrap().clone();
    assert_eq!(
        nach_zweiter.verified.len(),
        2,
        "{:?}",
        nach_zweiter.verified
    );
    assert_eq!(
        nach_zweiter.verified[0].by,
        agentkit_graph::model::Actor::from_principal("tester"),
        "die erste Bestätigung bleibt stehen"
    );
    assert_eq!(
        nach_zweiter.verified[1].by,
        agentkit_graph::model::Actor::from_principal("reviewer"),
        "die zweite kommt dazu, statt die erste zu ersetzen"
    );
}
