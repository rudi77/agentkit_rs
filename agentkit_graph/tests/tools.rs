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
    assert!(lesend.has("graph_compare"));
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
    assert_eq!(voll.names().len(), 7);
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

/// Was der Nutzer gesagt hat und was ein spät registriertes Tool (MCP)
/// geliefert hat, reicht das Frontend über den Evidenz-Griff nach — beides
/// belegt dann eine Aussage.
#[test]
fn nutzer_aussagen_und_nachgereichte_tool_ergebnisse_zaehlen_als_beleg() {
    use agentkit_graph::GraphFederation;

    let store = Arc::new(GraphStore::in_memory());
    let fed = GraphFederation::single(store.clone(), GraphAccess::project("agentkit", "s-1", &[]));
    let mut tools = ToolRegistry::new();
    agentkit_graph::register_federated_graph_tools(&mut tools, fed.clone());

    fed.evidence
        .witness_user("Bei uns wird immer mit make release gebaut, nie mit cargo direkt.");
    fed.evidence.witness_tool(
        "mcp__jira__get_issue",
        "PROJ-17: Deploy braucht das VPN-Profil ops",
    );
    // Ein Tool, dessen Ergebnis selbst von einem Modell stammt, zählt auch hier nicht.
    fed.evidence
        .witness_tool("task", "Der Sub-Agent meint: alles ist in Ordnung so.");

    let remember = |object: &str, excerpt: &str| -> serde_json::Value {
        let raw = tools
            .call(
                "graph_remember",
                json!({"subject": "Projekt", "predicate": "braucht", "object": object,
                       "excerpt": excerpt, "durable": true}),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    let vom_nutzer = remember("make release", "immer mit make release gebaut");
    let vom_mcp = remember("VPN-Profil ops", "Deploy braucht das VPN-Profil ops");
    let vom_helfer = remember("nichts", "alles ist in Ordnung so");
    assert_eq!(vom_nutzer["durable"], true, "{vom_nutzer}");
    assert_eq!(vom_mcp["durable"], true, "{vom_mcp}");
    assert_eq!(vom_helfer["durable"], false, "{vom_helfer}");

    let quelle = |receipt: &serde_json::Value| {
        let index = store.snapshot();
        let claim = index.claim(receipt["claim_id"].as_str().unwrap()).unwrap();
        (**index.source(&claim.source_ids[0]).unwrap()).clone()
    };
    assert_eq!(
        quelle(&vom_nutzer).source_type,
        agentkit_graph::USER_STATEMENT
    );
    assert_eq!(quelle(&vom_nutzer).tool_call_id, None);
    assert_eq!(quelle(&vom_mcp).source_type, "tool_result");
    assert_eq!(
        quelle(&vom_mcp).tool_call_id.as_deref(),
        Some("mcp__jira__get_issue#2")
    );
}

/// Ein Auszug, der nach einem Schlüssel aussieht, wird nicht gespeichert —
/// die Aussage und ihr Beleg bleiben, der Wortlaut nicht.
#[test]
fn ein_geheimnis_in_der_belegstelle_wird_nicht_gespeichert() {
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    tools.add("read_file", ".env lesen", json!({"type": "object"}), |_| {
        Ok("DEBUG=1\nOPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz012345\n".to_string())
    });
    register_graph_tools(
        &mut tools,
        store.clone(),
        GraphAccess::session("tester", "ws", "run-1"),
    );
    tools.call("read_file", json!({})).unwrap();
    let raw = tools
        .call(
            "graph_remember",
            json!({"subject": "Projekt", "predicate": "nutzt", "object": "OpenAI",
                   "excerpt": "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz012345"}),
        )
        .unwrap();
    assert!(raw.contains("\"evidence\":true"), "{raw}");

    let alles = serde_json::to_string(&agentkit_graph::export(&store.snapshot())).unwrap();
    assert!(!alles.contains("sk-proj-"), "{alles}");
    assert!(alles.contains(agentkit_graph::secrets::REDACTED), "{alles}");
}

/// Stammt das Zitat aus einer gelesenen Datei, nennt die Quelle diese Datei —
/// die Fundstelle, die ein Mensch nachschlagen kann. Bei einem Tool, dessen
/// `path` nur ein Suchstart ist (`grep`), bleibt sie leer.
#[test]
fn die_quelle_nennt_die_gelesene_datei() {
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    tools.add("read_file", "lesen", json!({"type": "object"}), |_| {
        Ok("# 2 Kündigung\n\nDie Frist beträgt 3 Monate zum Quartalsende.\n".to_string())
    });
    tools.add("grep", "suchen", json!({"type": "object"}), |_| {
        Ok("docs/b/004.md:3: Die Haftung ist auf 5.000 EUR begrenzt.".to_string())
    });
    register_graph_tools(
        &mut tools,
        store.clone(),
        GraphAccess::session("tester", "ws", "run-1"),
    );
    let quelle = |tools: &ToolRegistry, excerpt: &str| -> serde_json::Value {
        let raw = tools
            .call(
                "graph_remember",
                json!({"subject": "Vertrag A", "predicate": excerpt.len().to_string(),
                       "object": "x", "excerpt": excerpt}),
            )
            .unwrap();
        let evidenz: serde_json::Value = serde_json::from_str(
            &tools
                .call("graph_evidence", json!({"claim_id": claim_id(&raw)}))
                .unwrap(),
        )
        .unwrap();
        evidenz["sources"][0].clone()
    };

    tools
        .call(
            "read_file",
            json!({"path": "docs/vertrag a/002-kuendigung.md"}),
        )
        .unwrap();
    let aus_datei = quelle(&tools, "Die Frist beträgt 3 Monate");
    assert_eq!(aus_datei["type"], "tool_result");
    assert_eq!(aus_datei["artifact"], "docs/vertrag%20a/002-kuendigung.md");

    tools.call("grep", json!({"path": "docs"})).unwrap();
    let aus_suche = quelle(&tools, "Die Haftung ist auf 5.000 EUR begrenzt.");
    assert_eq!(aus_suche["type"], "tool_result");
    assert_eq!(aus_suche["artifact"], serde_json::Value::Null);

    // Die Fundstelle steht als `resource` im Bundle, wie OKF sie vorsieht.
    let index = store.snapshot();
    assert!(index
        .sources()
        .any(|s| s.artifact_uri.as_deref() == Some("docs/vertrag%20a/002-kuendigung.md")));
}

/// `graph_compare` legt die Aussagen mehrerer Entities je Prädikat
/// nebeneinander: gleich, abweichend — und was bei einer fehlt, steht als
/// Lücke da, statt in der Antwort einfach zu fehlen.
#[test]
fn compare_zeigt_gleiches_abweichendes_und_luecken() {
    let (_, tools, _) = registry(GraphAccess::session("tester", "ws", "run-1"));
    let merke = |s: &str, p: &str, o: &str| {
        tools
            .call(
                "graph_remember",
                json!({"subject": s, "predicate": p, "object": o}),
            )
            .unwrap()
    };
    merke("Vertrag A", "laufzeit", "24 Monate");
    merke("Vertrag A", "kündigungsfrist", "3 Monate");
    merke("Vertrag A", "haftungsgrenze", "5.000 EUR");
    merke("Vertrag B", "Laufzeit", "24 Monate");
    merke("Vertrag B", "kündigungsfrist", "6 Monate");
    // Als OBJEKT zählt eine Entity nicht: das ist keine Aussage über sie.
    merke("Rahmenvertrag", "ersetzt", "Vertrag B");

    let out = tools
        .call(
            "graph_compare",
            json!({"entities": ["Vertrag A", "Vertrag B"]}),
        )
        .unwrap();
    assert!(
        out.starts_with(
            "<graph-compare entities=\"2\" praedikate=\"3\" abweichend=\"1\" luecken=\"1\">"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "[laufzeit] gleich\n  Vertrag A: 24 Monate [C-1]\n  Vertrag B: 24 Monate [C-4]"
        ),
        "{out}"
    );
    assert!(out.contains("[kündigungsfrist] abweichend"), "{out}");
    assert!(
        out.contains(
            "[haftungsgrenze] Lücke\n  Vertrag A: 5.000 EUR [C-3]\n  Vertrag B: (keine Aussage)"
        ),
        "{out}"
    );
    assert!(!out.contains("ersetzt"), "{out}");

    // Zurückgezogenes zählt nicht mehr — aus der Abweichung wird eine Lücke.
    tools
        .call(
            "graph_retract",
            json!({"claim_id": "C-5", "reason": "falsch gelesen"}),
        )
        .unwrap();
    let danach = tools
        .call(
            "graph_compare",
            json!({"entities": ["Vertrag A", "Vertrag B"]}),
        )
        .unwrap();
    assert!(danach.contains("[kündigungsfrist] Lücke"), "{danach}");

    let unbekannt = tools
        .call(
            "graph_compare",
            json!({"entities": ["Vertrag A", "Vertrag Z"]}),
        )
        .unwrap();
    assert_eq!(unbekannt, "(Entity 'Vertrag Z' ist im Graph nicht bekannt)");
    let allein = tools
        .call("graph_compare", json!({"entities": ["Vertrag A"]}))
        .unwrap();
    assert!(allein.starts_with("ERROR:"), "{allein}");
}

/// Der ganze Weg mit den echten Datei-Tools: zwei Dokumente zerlegen, je einen
/// Abschnitt lesen, die Aussage mit wörtlichem Zitat festhalten, vergleichen.
/// Die Fundstelle an der Aussage ist die Abschnittsdatei.
#[test]
fn zwei_dokumente_von_der_datei_bis_zur_gegenueberstellung() {
    let dir = std::env::temp_dir().join(format!("agentkit_graph_docs_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    for (name, frist) in [("a", "3 Monate"), ("b", "6 Monate")] {
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!("# 1 Laufzeit\n\n24 Monate.\n\n# 2 Kündigung\n\nDie Frist beträgt {frist} zum Quartalsende.\n"),
        )
        .unwrap();
    }
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    agentkit::CodingTools::new(dir.to_str().unwrap(), false).register(&mut tools, None);
    register_graph_tools(
        &mut tools,
        store.clone(),
        GraphAccess::session("tester", "ws", "run-1"),
    );

    for (name, frist) in [("a", "3 Monate"), ("b", "6 Monate")] {
        tools
            .call("doc_ingest", json!({"path": format!("{name}.md")}))
            .unwrap();
        let abschnitt = format!(".agentkit/docs/{name}/002-2-kuendigung.md");
        tools.call("read_file", json!({"path": abschnitt})).unwrap();
        let raw = tools
            .call(
                "graph_remember",
                json!({
                    "subject": format!("Vertrag {name}"), "predicate": "kündigungsfrist",
                    "object": frist, "durable": true,
                    "excerpt": format!("Die Frist beträgt {frist} zum Quartalsende."),
                }),
            )
            .unwrap();
        assert!(raw.contains("\"durable\":true"), "{raw}");
    }

    let vergleich = tools
        .call(
            "graph_compare",
            json!({"entities": ["Vertrag a", "Vertrag b"]}),
        )
        .unwrap();
    assert!(
        vergleich.contains("[kündigungsfrist] abweichend"),
        "{vergleich}"
    );
    // Beide Aussagen sind dauerhaft und stehen nebeneinander: verschiedene
    // Subjekte ersetzen einander nicht.
    assert!(
        vergleich.contains("3 Monate [C-1]") && vergleich.contains("6 Monate [C-2]"),
        "{vergleich}"
    );
    let evidenz = tools
        .call("graph_evidence", json!({"claim_id": "C-2"}))
        .unwrap();
    assert!(
        evidenz.contains("\"artifact\":\".agentkit/docs/b/002-2-kuendigung.md\""),
        "{evidenz}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
