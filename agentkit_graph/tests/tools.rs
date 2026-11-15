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
    assert!(!lesend.has("graph_retract"));

    let (_, arbeitend, _) = registry(GraphAccess::working(
        "tester",
        GraphView::default(),
        GraphTarget::new(GraphLayer::Working, GraphScope::session("run-1")),
    ));
    assert!(arbeitend.has("graph_remember"));
    assert!(!arbeitend.has("graph_promote"));
    // Wer schreiben darf, darf das Eigene auch zurückziehen.
    assert!(arbeitend.has("graph_retract"));

    let (_, voll, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    assert!(voll.has("graph_remember"));
    assert!(voll.has("graph_promote"));
    assert!(voll.has("graph_retract"));
    assert_eq!(voll.names().len(), 6);
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
    assert_eq!(claim.created_by.as_str(), "agent:tester");
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
    assert_eq!(evidenz["sources"][0]["agent"], "agent:tester");
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

/// Eine Registry mit einem Tool, das VOR den Graph-Tools registriert ist —
/// so wie die Coding-Tools im echten Agenten. Nur dessen Ausgabe kann eine
/// Belegstelle stützen.
fn registry_mit_testlauf(access: GraphAccess) -> (Arc<GraphStore>, ToolRegistry) {
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    tools.add("run_shell", "Testlauf", json!({"type": "object"}), |_| {
        Ok("running 3 tests\ntest result: FAILED. 2 passed; 1 failed".to_string())
    });
    register_graph_tools(&mut tools, store.clone(), access);
    (store, tools)
}

fn claim_id(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw).unwrap()["claim_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn promote_ueber_das_tool_bestaetigt_die_aussage() {
    let (store, tools) = registry_mit_testlauf(GraphAccess::session("tester", "ws", "run-1"));
    tools.call("run_shell", json!({})).unwrap();
    let raw = tools
        .call(
            "graph_remember",
            json!({
                "subject": "A", "predicate": "nutzt", "object": "B",
                "excerpt": "test result: FAILED. 2 passed; 1 failed"
            }),
        )
        .unwrap();
    let claim_id = claim_id(&raw);

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

/// Das Gate: was nur das Modell gesagt hat, wird nicht dauerhaft — egal, ob
/// es eine Belegstelle behauptet, die kein Tool je geliefert hat.
#[test]
fn ohne_gepruefte_belegstelle_laesst_sich_nichts_befoerdern() {
    let (store, tools) = registry_mit_testlauf(GraphAccess::session("tester", "ws", "run-1"));
    tools.call("run_shell", json!({})).unwrap();

    for excerpt in [None, Some("alle 3 Tests sind grün durchgelaufen")] {
        let mut args = json!({"subject": "A", "predicate": "nutzt", "object": "B"});
        if let Some(text) = excerpt {
            args["excerpt"] = json!(text);
        }
        let raw = tools.call("graph_remember", args).unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(receipt["evidence"], false, "{raw}");

        let abgelehnt = tools
            .call("graph_promote", json!({"claim_id": claim_id(&raw)}))
            .unwrap();
        assert!(abgelehnt.starts_with("ERROR:"), "{abgelehnt}");
        assert!(abgelehnt.contains("fremde Quelle"), "{abgelehnt}");
        assert_eq!(
            store.snapshot().claim(&claim_id(&raw)).unwrap().layer,
            GraphLayer::Working
        );
    }
}

#[test]
fn durable_befoerdert_nur_belegte_beobachtungen() {
    let (store, tools) = registry_mit_testlauf(GraphAccess::session("tester", "ws", "run-1"));
    tools.call("run_shell", json!({})).unwrap();
    let beleg = "test result: FAILED. 2 passed; 1 failed";
    let remember = |object: &str, status: &str, excerpt: Option<&str>| {
        let mut args = json!({
            "subject": "Testlauf", "predicate": "zeigt", "object": object,
            "status": status, "durable": true
        });
        if let Some(text) = excerpt {
            args["excerpt"] = json!(text);
        }
        let raw = tools.call("graph_remember", args).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()
    };

    // Eine Hypothese wird nie in dem Aufruf dauerhaft, in dem sie entsteht —
    // auch nicht mit Beleg.
    let hypothese = remember("Wettlauf", "hypothesis", Some(beleg));
    assert_eq!(hypothese["durable"], false, "{hypothese}");
    assert!(hypothese["hint"].as_str().is_some(), "{hypothese}");

    // Eine Beobachtung ohne Beleg ebenso wenig.
    let unbelegt = remember("Flakiness", "observation", None);
    assert_eq!(unbelegt["durable"], false, "{unbelegt}");
    assert!(unbelegt["hint"].as_str().is_some(), "{unbelegt}");

    // Belegt und beobachtet: dauerhaft im selben Aufruf.
    let belegt = remember("Fehlschlag", "observation", Some(beleg));
    assert_eq!(belegt["durable"], true, "{belegt}");
    assert!(belegt.get("hint").is_none(), "{belegt}");

    let index = store.snapshot();
    let layer = |receipt: &serde_json::Value| {
        index
            .claim(receipt["claim_id"].as_str().unwrap())
            .unwrap()
            .layer
    };
    assert_eq!(layer(&hypothese), GraphLayer::Working);
    assert_eq!(layer(&unbelegt), GraphLayer::Working);
    assert_eq!(layer(&belegt), GraphLayer::Canonical);
}

/// Die Quelle einer belegten Aussage nennt den Aufruf, aus dem das Zitat
/// stammt — gesetzt von der Laufzeit, nicht vom Modell.
#[test]
fn eine_gepruefte_belegstelle_wird_zur_tool_quelle() {
    let (_, tools) = registry_mit_testlauf(GraphAccess::session("tester", "ws", "run-1"));
    tools.call("run_shell", json!({})).unwrap();
    let raw = tools
        .call(
            "graph_remember",
            json!({
                "subject": "A", "predicate": "nutzt", "object": "B",
                "excerpt": "2 passed;\n   1 failed"
            }),
        )
        .unwrap();
    let evidenz: serde_json::Value = serde_json::from_str(
        &tools
            .call("graph_evidence", json!({"claim_id": claim_id(&raw)}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(evidenz["sources"][0]["type"], "tool_result");
    assert_eq!(evidenz["sources"][0]["tool_call_id"], "run_shell#1");
}

#[test]
fn zurueckgezogenes_verschwindet_aus_der_suche_und_bleibt_als_evidenz() {
    let (store, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    let raw = tools
        .call(
            "graph_remember",
            json!({"subject": "MCP Client", "predicate": "nutzt", "object": "HTTP"}),
        )
        .unwrap();
    let id = claim_id(&raw);
    assert!(tools
        .call("graph_search", json!({"query": "MCP Client"}))
        .unwrap()
        .contains(&id));

    let ohne_grund = tools
        .call("graph_retract", json!({"claim_id": id, "reason": "  "}))
        .unwrap();
    assert!(ohne_grund.starts_with("ERROR:"), "{ohne_grund}");

    let raw = tools
        .call(
            "graph_retract",
            json!({"claim_id": id, "reason": "falsch gelesen, es ist stdio"}),
        )
        .unwrap();
    assert!(raw.contains("retracted"), "{raw}");
    assert_eq!(
        store.snapshot().claim(&id).unwrap().status,
        ClaimStatus::Retracted
    );

    for (tool, args) in [
        ("graph_search", json!({"query": "MCP Client"})),
        ("graph_neighbors", json!({"entity": "MCP Client"})),
    ] {
        let out = tools.call(tool, args).unwrap();
        assert!(!out.contains(&id), "{tool}: {out}");
    }

    let evidenz: serde_json::Value = serde_json::from_str(
        &tools
            .call("graph_evidence", json!({"claim_id": id}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(evidenz["status"], "retracted");
    assert_eq!(
        evidenz["retracted"]["reason"],
        "falsch gelesen, es ist stdio"
    );
    assert_eq!(evidenz["retracted"]["by"], "agent:tester");
    assert_eq!(evidenz["sources"].as_array().unwrap().len(), 1);

    // Ein zweites Mal geht nicht, und befördern lässt sie sich auch nicht mehr.
    for (tool, args) in [
        (
            "graph_retract",
            json!({"claim_id": id, "reason": "nochmal"}),
        ),
        ("graph_promote", json!({"claim_id": id})),
    ] {
        let out = tools.call(tool, args).unwrap();
        assert!(out.starts_with("ERROR:"), "{tool}: {out}");
    }
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
