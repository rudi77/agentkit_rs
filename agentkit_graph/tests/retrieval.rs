//! Retrieval als Spezifikation: Seeds, Hops, Ranking, Budget, Sichtbarkeit.

use agentkit_graph::retrieval::{self, GraphQuery};
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    ClaimDraft, ClaimStatus, GraphAccess, GraphLayer, GraphScope, GraphTarget, GraphView,
    GraphWriteCommand, SourceDraft,
};

fn schreibe(
    store: &GraphStore,
    access: &GraphAccess,
    s: &str,
    p: &str,
    o: &str,
    conf: f32,
) -> String {
    let draft = ClaimDraft::new(s, p, o, SourceDraft::new("document")).confidence(conf);
    store
        .submit(GraphWriteCommand::RecordClaim(draft), access)
        .unwrap()
        .claim_id
        .unwrap()
}

/// Eine kleine Kette: MCP Client -> stdio-Session -> Deadlock -> Testlauf.
fn kette() -> (GraphStore, GraphAccess) {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    schreibe(&store, &access, "MCP Client", "nutzt", "stdio-Session", 0.9);
    schreibe(
        &store,
        &access,
        "stdio-Session",
        "verursacht",
        "Deadlock",
        0.8,
    );
    schreibe(
        &store,
        &access,
        "Deadlock",
        "gezeigt in",
        "Testlauf T-41",
        0.7,
    );
    schreibe(
        &store,
        &access,
        "Sonstiges",
        "hat mit",
        "nichts zu tun",
        0.5,
    );
    (store, access)
}

#[test]
fn seeds_kommen_aus_dem_namen_in_der_frage() {
    let (store, access) = kette();
    let index = store.snapshot();
    let seeds = retrieval::find_seeds(
        &index,
        &access.view,
        &GraphQuery::text("Was macht der MCP Client?"),
    );
    let namen: Vec<String> = seeds
        .iter()
        .map(|id| index.entity(id).unwrap().canonical_name.clone())
        .collect();
    assert_eq!(namen.first().map(String::as_str), Some("MCP Client"));
}

#[test]
fn ein_hop_bleibt_am_seed_zwei_hops_erreichen_den_deadlock() {
    let (store, access) = kette();
    let index = store.snapshot();

    let eins = retrieval::search(
        &index,
        &access.view,
        &GraphQuery::seeded("MCP Client").hops(1),
    );
    let praedikate: Vec<&str> = eins
        .claims
        .iter()
        .map(|c| c.claim.predicate.as_str())
        .collect();
    assert_eq!(praedikate, vec!["nutzt"]);

    let zwei = retrieval::search(
        &index,
        &access.view,
        &GraphQuery::seeded("MCP Client").hops(2),
    );
    let praedikate: Vec<&str> = zwei
        .claims
        .iter()
        .map(|c| c.claim.predicate.as_str())
        .collect();
    assert!(praedikate.contains(&"verursacht"), "{praedikate:?}");
    assert!(
        !praedikate.contains(&"gezeigt in"),
        "3 Hops sind zu weit: {praedikate:?}"
    );
}

#[test]
fn flektierte_frage_findet_die_entity_trotzdem() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    schreibe(
        &store,
        &access,
        "Parallele Tool-Aufrufe",
        "verursachen",
        "Session-Konkurrenz",
        0.8,
    );
    let index = store.snapshot();

    // Deutsche Endungen dürfen den Recall nicht kosten.
    let sub = retrieval::search(
        &index,
        &access.view,
        &GraphQuery::text("Gibt es Probleme mit parallelen Tool-Aufrufen?"),
    );
    assert_eq!(sub.claims.len(), 1);

    // Ähnlich anfangende, aber andere Wörter bleiben draußen.
    let daneben = retrieval::search(
        &index,
        &access.view,
        &GraphQuery::text("Wie ist die Sequenz der Schritte?"),
    );
    assert!(daneben.is_empty());
}

#[test]
fn reihenfolge_ist_deterministisch() {
    let (store, access) = kette();
    let index = store.snapshot();
    let query = GraphQuery::text("MCP Client stdio Deadlock");

    let erste: Vec<String> = retrieval::search(&index, &access.view, &query)
        .claims
        .iter()
        .map(|c| c.claim.id.clone())
        .collect();
    for _ in 0..20 {
        let wieder: Vec<String> = retrieval::search(&index, &access.view, &query)
            .claims
            .iter()
            .map(|c| c.claim.id.clone())
            .collect();
        assert_eq!(erste, wieder);
    }
}

#[test]
fn naehe_und_konfidenz_bestimmen_den_rang() {
    let (store, access) = kette();
    let index = store.snapshot();
    let sub = retrieval::search(
        &index,
        &access.view,
        &GraphQuery::seeded("MCP Client").hops(2),
    );
    // Der direkte Nachbar steht vor dem zwei Hops entfernten.
    assert_eq!(sub.claims[0].hop, 1);
    assert!(sub.claims[0].score > sub.claims[1].score);
}

#[test]
fn claim_budget_kuerzt_und_meldet_das() {
    let (store, access) = kette();
    let index = store.snapshot();
    let mut query = GraphQuery::seeded("MCP Client").hops(2);
    query.max_claims = 1;

    let sub = retrieval::search(&index, &access.view, &query);
    assert_eq!(sub.claims.len(), 1);
    assert!(sub.truncated);
    assert!(retrieval::render(&index, &sub, false).contains("Budget"));
}

#[test]
fn token_budget_kuerzt_ebenfalls() {
    let (store, access) = kette();
    let index = store.snapshot();
    // 1 Token lässt genau die erste Aussage durch (mindestens eine kommt immer).
    let query = GraphQuery::seeded("MCP Client").hops(2).limits(30, 1);
    let sub = retrieval::search(&index, &access.view, &query);
    assert_eq!(sub.claims.len(), 1);
    assert!(sub.truncated);
}

#[test]
fn was_ausserhalb_der_sicht_liegt_bleibt_unsichtbar() {
    let store = GraphStore::in_memory();
    let geheim = GraphAccess::working(
        "reviewer",
        GraphView::default(),
        GraphTarget::new(GraphLayer::Working, GraphScope::agent("reviewer", "run-1")),
    );
    schreibe(&store, &geheim, "Reviewer", "vermutet", "Fehler in X", 0.5);

    let fremd = GraphAccess::session("developer", "ws", "run-1");
    let index = store.snapshot();

    // Der Autor sieht seine private Notiz …
    let eigen = retrieval::search(&index, &geheim.view, &GraphQuery::text("Reviewer vermutet"));
    assert_eq!(eigen.claims.len(), 1);
    // … ein anderer Agent findet weder Entity noch Aussage.
    let anderer = retrieval::search(&index, &fremd.view, &GraphQuery::text("Reviewer vermutet"));
    assert!(anderer.is_empty());
    assert!(retrieval::find_seeds(&index, &fremd.view, &GraphQuery::seeded("Reviewer")).is_empty());
}

#[test]
fn statusfilter_blendet_hypothesen_aus() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    store
        .submit(
            GraphWriteCommand::RecordClaim(
                ClaimDraft::new("A", "nutzt", "B", SourceDraft::new("document"))
                    .status(ClaimStatus::Hypothesis),
            ),
            &access,
        )
        .unwrap();
    schreibe(&store, &access, "A", "kennt", "C", 0.9);

    let index = store.snapshot();
    let mut query = GraphQuery::seeded("A");
    query.include_statuses = vec![ClaimStatus::Observation];
    let sub = retrieval::search(&index, &access.view, &query);
    assert_eq!(sub.claims.len(), 1);
    assert_eq!(sub.claims[0].claim.predicate, "kennt");
}

#[test]
fn render_enthaelt_ids_status_und_quellen() {
    let (store, access) = kette();
    let index = store.snapshot();
    let sub = retrieval::search(&index, &access.view, &GraphQuery::seeded("MCP Client"));
    let text = retrieval::render(&index, &sub, true);

    assert!(text.starts_with("<graph-context revision=\"4\""));
    assert!(text.contains("(MCP Client) --[nutzt]--> (stdio-Session)"));
    assert!(text.contains("Status: observation"));
    assert!(text.contains("Ebene: working"));
    assert!(text.contains("Quellen: S-1"));
    assert!(text.ends_with("</graph-context>"));
}

#[test]
fn leerer_treffer_rendert_ein_leeres_element() {
    let (store, access) = kette();
    let index = store.snapshot();
    let sub = retrieval::search(&index, &access.view, &GraphQuery::text("völlig unbekannt"));
    assert!(sub.is_empty());
    assert_eq!(
        retrieval::render(&index, &sub, false),
        "<graph-context revision=\"4\"/>"
    );
}

#[test]
fn evidence_liefert_die_quellen_eines_claims() {
    let (store, access) = kette();
    let id = schreibe(&store, &access, "Neu", "belegt durch", "Testlauf", 0.6);
    let index = store.snapshot();

    let quellen = retrieval::evidence(&index, &id);
    assert_eq!(quellen.len(), 1);
    assert_eq!(quellen[0].source_type, "document");
    assert_eq!(quellen[0].agent_id.as_deref(), Some("tester"));
    assert!(retrieval::evidence(&index, "C-999").is_empty());
}
