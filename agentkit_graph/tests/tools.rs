//! Die Tool-Schicht: was das Modell sieht, darf es auch — und nicht mehr.

use std::sync::Arc;

use agentkit::ToolRegistry;
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    register_graph_tools, ClaimStatus, GraphAccess, GraphLayer, GraphScope, GraphTarget, GraphView,
};
use serde_json::json;

fn registry(access: GraphAccess) -> (Arc<GraphStore>, ToolRegistry, GraphAccess) {
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    register_graph_tools(&mut tools, store.clone(), access.clone());
    (store, tools, access)
}

#[test]
fn schreib_tools_erscheinen_nur_wenn_der_zugriff_sie_erlaubt() {
    let (_, lesend, _) = registry(GraphAccess::read_only(
        "leser",
        GraphView::new(vec![GraphTarget::canonical(GraphScope::workspace("ws"))]),
    ));
    assert!(lesend.has("graph_search"));
    assert!(lesend.has("graph_neighbors"));
    assert!(lesend.has("graph_evidence"));
    // Ohne Schreibziel gibt es 'graph_remember' gar nicht erst zu sehen.
    assert!(!lesend.has("graph_remember"));
    assert!(!lesend.has("graph_promote"));

    let (_, arbeitend, _) = registry(GraphAccess::working(
        "tester",
        GraphView::default(),
        GraphTarget::new(GraphLayer::Working, GraphScope::session("run-1")),
    ));
    assert!(arbeitend.has("graph_remember"));
    assert!(!arbeitend.has("graph_promote"));

    let (_, voll, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    assert!(voll.has("graph_remember"));
    assert!(voll.has("graph_promote"));
    assert_eq!(voll.names().len(), 5);
}

#[test]
fn remember_schreibt_und_search_findet() {
    let (store, tools, access) = registry(GraphAccess::session("tester", "ws", "run-1"));

    let raw = tools
        .call(
            "graph_remember",
            json!({
                "subject": "Parallele Tool-Aufrufe",
                "predicate": "verursachen",
                "object": "Session-Konkurrenz",
                "status": "observation",
                "confidence": 0.82,
                "excerpt": "zwei Fehlschläge in mcp::"
            }),
        )
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let claim_id = receipt["claim_id"].as_str().unwrap().to_string();
    assert_eq!(receipt["revision"], 1);

    // Der Autor steht fest — das Tool hat gar kein Feld dafür.
    let index = store.snapshot();
    let claim = index.claim(&claim_id).unwrap();
    assert_eq!(claim.created_by, "tester");
    assert_eq!(claim.status, ClaimStatus::Observation);
    assert_eq!(claim.scope, GraphScope::session("run-1"));
    assert_eq!(access.principal, "tester");

    let gefunden = tools
        .call(
            "graph_search",
            json!({"query": "Was verursacht Session-Konkurrenz?"}),
        )
        .unwrap();
    assert!(gefunden.contains(&claim_id), "{gefunden}");
    assert!(gefunden.contains("--[verursachen]-->"));

    let evidenz: serde_json::Value = serde_json::from_str(
        &tools
            .call("graph_evidence", json!({"claim_id": claim_id}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        evidenz["sources"][0]["excerpt"],
        "zwei Fehlschläge in mcp::"
    );
    assert_eq!(evidenz["sources"][0]["agent"], "tester");
}

#[test]
fn kein_tool_hat_ein_scope_oder_autor_argument() {
    let (_, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    let schemas = serde_json::to_string(tools.schemas().unwrap()).unwrap();
    for verboten in ["scope", "agent_id", "created_by", "layer", "principal"] {
        assert!(
            !schemas.contains(&format!("\"{verboten}\"")),
            "'{verboten}' darf kein Tool-Argument sein: {schemas}"
        );
    }
}

#[test]
fn promote_ueber_das_tool_bestaetigt_die_aussage() {
    let (store, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    let raw = tools
        .call(
            "graph_remember",
            json!({"subject": "A", "predicate": "nutzt", "object": "B"}),
        )
        .unwrap();
    let claim_id = serde_json::from_str::<serde_json::Value>(&raw).unwrap()["claim_id"]
        .as_str()
        .unwrap()
        .to_string();

    let raw = tools
        .call("graph_promote", json!({"claim_id": claim_id.clone()}))
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(receipt["claim_id"], claim_id);

    let index = store.snapshot();
    assert_eq!(
        index.claim(&claim_id).unwrap().status,
        ClaimStatus::Confirmed
    );
}

#[test]
fn fehler_kommen_als_weiches_ergebnis_zurueck() {
    let (_, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));

    let falscher_status = tools
        .call(
            "graph_remember",
            json!({"subject": "A", "predicate": "nutzt", "object": "B", "status": "confirmed"}),
        )
        .unwrap();
    assert!(falscher_status.starts_with("ERROR:"), "{falscher_status}");

    let leer = tools
        .call(
            "graph_remember",
            json!({"subject": " ", "predicate": "nutzt", "object": "B"}),
        )
        .unwrap();
    assert!(leer.starts_with("ERROR:"), "{leer}");

    let unbekannt = tools
        .call("graph_promote", json!({"claim_id": "C-999"}))
        .unwrap();
    assert!(unbekannt.starts_with("ERROR:"), "{unbekannt}");
}

#[test]
fn leere_treffer_sind_klar_benannt() {
    let (_, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    assert!(tools
        .call("graph_search", json!({"query": "nichts davon"}))
        .unwrap()
        .contains("nichts im Graph"));
    assert!(tools
        .call("graph_neighbors", json!({"entity": "Unbekannt"}))
        .unwrap()
        .contains("nicht bekannt"));
    assert!(tools
        .call("graph_evidence", json!({"claim_id": "C-1"}))
        .unwrap()
        .contains("keine Quellen"));
}

#[test]
fn neighbors_laeuft_ueber_zwei_hops() {
    let (_, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    tools
        .call(
            "graph_remember",
            json!({"subject": "MCP Client", "predicate": "nutzt", "object": "stdio"}),
        )
        .unwrap();
    tools
        .call(
            "graph_remember",
            json!({"subject": "stdio", "predicate": "verursacht", "object": "Deadlock"}),
        )
        .unwrap();

    let eins = tools
        .call("graph_neighbors", json!({"entity": "MCP Client"}))
        .unwrap();
    assert!(!eins.contains("Deadlock"), "{eins}");

    let zwei = tools
        .call(
            "graph_neighbors",
            json!({"entity": "MCP Client", "hops": 2}),
        )
        .unwrap();
    assert!(zwei.contains("Deadlock"), "{zwei}");
}
