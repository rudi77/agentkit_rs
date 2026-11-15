//! Regressionstests zu den Befunden des Code-Reviews: je Test ein Fehler, der
//! vorher Daten kostete oder die Sicht umging.

use std::sync::Arc;

use agentkit::ToolRegistry;
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    register_graph_tools, ClaimDraft, ClaimStatus, GraphAccess, GraphLayer, GraphScope,
    GraphTarget, GraphView, GraphWriteCommand, SourceDraft,
};
use serde_json::json;

fn temp_bundle(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graph_review_{label}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

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

/// Ein Tab in einem Namen wurde roh ins Frontmatter geschrieben; der Commit
/// gelang, aber das nächste `open` scheiterte für den GANZEN Graphen.
#[test]
fn ein_tab_im_namen_macht_das_bundle_nicht_unlesbar() {
    let dir = temp_bundle("tab");
    {
        let store = GraphStore::open(&dir).unwrap();
        let access = GraphAccess::session("tester", "ws", "run-1");
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    "foo\tbar",
                    "steht in",
                    "Makefile",
                    SourceDraft::new("tool_result").excerpt("all:\tcargo build"),
                )),
                &access,
            )
            .unwrap();
    }
    let wieder = GraphStore::open(&dir).expect("Bundle muss sich wieder öffnen lassen");
    let index = wieder.snapshot();
    assert!(index.entities().any(|e| e.canonical_name == "foo\tbar"));
    assert!(index
        .sources()
        .any(|s| s.excerpt.as_deref() == Some("all:\tcargo build")));
    std::fs::remove_dir_all(&dir).ok();
}

/// Ein geschütztes Leerzeichen vor `#` galt beim Lesen als Kommentarbeginn,
/// beim Schreiben aber nicht als Grund zu quotieren: `"Issue\u{a0}#12"` kam
/// als `"Issue"` zurück.
#[test]
fn unicode_leerraum_vor_raute_ueberlebt_den_neustart() {
    let dir = temp_bundle("nbsp");
    {
        let store = GraphStore::open(&dir).unwrap();
        let access = GraphAccess::session("tester", "ws", "run-1");
        schreibe(&store, &access, "Issue\u{a0}#12", "betrifft", "Parser");
    }
    let wieder = GraphStore::open(&dir).unwrap();
    assert!(wieder
        .snapshot()
        .entities()
        .any(|e| e.canonical_name == "Issue\u{a0}#12"));
    std::fs::remove_dir_all(&dir).ok();
}

/// Zwei Sessions legen unabhängig voneinander eine Entity „Foo" an, beide
/// promoten. Die ältere Entity (kleinere ID) bekommt im Ziel den Slug `foo` —
/// und überschrieb dort das Dokument der jüngeren, die schon kanonisch war:
/// im Speicher vier Entities und zwei Aussagen, nach dem Neustart drei und eine.
#[test]
fn promotion_ueberschreibt_kein_gleichnamiges_kanonisches_dokument() {
    let dir = temp_bundle("gleichnamig");
    let erwartet = {
        let store = GraphStore::open(&dir).unwrap();
        let a = GraphAccess::session("alice", "ws", "sess-a");
        let b = GraphAccess::session("bob", "ws", "sess-b");
        let von_a = schreibe(&store, &a, "Foo", "nutzt", "Bar");
        let von_b = schreibe(&store, &b, "Foo", "braucht", "Baz");
        promote(&store, &b, &von_b);
        promote(&store, &a, &von_a);
        store.stats()
    };
    assert_eq!((erwartet.entities, erwartet.claims), (4, 2));

    let wieder = GraphStore::open(&dir).unwrap();
    assert_eq!(
        wieder.stats(),
        erwartet,
        "nichts darf auf der Platte fehlen"
    );
    let index = wieder.snapshot();
    for claim in index.claims() {
        assert_eq!(claim.status, ClaimStatus::Confirmed);
        assert!(index.entity(&claim.subject).is_some(), "{}", claim.id);
        assert!(index.entity(&claim.object).is_some(), "{}", claim.id);
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Die Gegenrichtung: verlässt die ältere Entity ein Verzeichnis, rückt die
/// jüngere gleichen Slugs auf den freien Dateinamen nach. Ihr Dokument muss
/// dorthin umziehen — und darf dabei nicht dem Aufräumen des alten Pfads der
/// promoteten Entity zum Opfer fallen.
#[test]
fn nachrueckende_entity_zieht_mit_ihrem_dokument_um() {
    let dir = temp_bundle("nachruecken");
    let erwartet = {
        let store = GraphStore::open(&dir).unwrap();
        let a = GraphAccess::session("alice", "ws", "sess-a");
        // `Foo` und `Fooα` sind zwei Entities (verschiedene Aliase), aber EIN Slug.
        let erste = schreibe(&store, &a, "Foo", "nutzt", "Bar");
        schreibe(&store, &a, "Fooα", "braucht", "Baz");
        promote(&store, &a, &erste);
        store.stats()
    };

    let wieder = GraphStore::open(&dir).unwrap();
    assert_eq!(wieder.stats(), erwartet);
    let arbeitsziel = dir.join("working").join("session-sess-a");
    assert!(
        arbeitsziel.join("foo.md").exists(),
        "nachgerückt auf foo.md"
    );
    let reste: Vec<String> = std::fs::read_dir(&arbeitsziel)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("foo-"))
        .collect();
    assert!(
        reste.is_empty(),
        "kein liegen gebliebenes Dokument: {reste:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `graph_evidence` muss die Sicht achten: Claim-IDs sind fortlaufend, ein
/// Agent konnte sie durchprobieren und Belegstellen aus fremden privaten
/// Scopes lesen. Die Antwort darf sich nicht von „gibt es nicht" unterscheiden.
#[test]
fn evidence_zeigt_keine_quellen_ausserhalb_der_sicht() {
    let store = Arc::new(GraphStore::in_memory());
    let privat = GraphAccess::working(
        "reviewer",
        GraphView::default(),
        GraphTarget::new(GraphLayer::Working, GraphScope::agent("reviewer", "run-1")),
    );
    let mut eigene = ToolRegistry::new();
    register_graph_tools(&mut eigene, store.clone(), privat);
    let geschrieben: serde_json::Value = serde_json::from_str(
        &eigene
            .call(
                "graph_remember",
                json!({"subject": "Geheim", "predicate": "ist", "object": "privat",
                       "excerpt": "nur für den Reviewer"}),
            )
            .unwrap(),
    )
    .unwrap();
    let claim_id = geschrieben["claim_id"].as_str().unwrap().to_string();

    // Der Urheber sieht seine Quelle weiterhin.
    let selbst = eigene
        .call("graph_evidence", json!({"claim_id": claim_id}))
        .unwrap();
    assert!(selbst.contains("nur für den Reviewer"), "{selbst}");

    let mut fremde = ToolRegistry::new();
    register_graph_tools(
        &mut fremde,
        store,
        GraphAccess::session("anderer", "ws", "run-2"),
    );
    let fremd = fremde
        .call("graph_evidence", json!({"claim_id": claim_id}))
        .unwrap();
    assert!(!fremd.contains("nur für den Reviewer"), "{fremd}");
    let unbekannt = fremde
        .call("graph_evidence", json!({"claim_id": "C-999"}))
        .unwrap();
    assert_eq!(
        fremd.replace(&claim_id, "X"),
        unbekannt.replace("C-999", "X"),
        "kein Existenz-Orakel"
    );
}

/// `normalize` verwirft `#` und `+`: „C", „C#" und „C++" lösten auf DIESELBE
/// Entity auf, Aussagen über drei Sprachen hingen an einer. Die gewollten
/// Zusammenführungen (Schreibweise, Leerraum, Bindestrich) bleiben.
#[test]
fn c_csharp_und_cpp_sind_drei_entities() {
    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let subjekt = |name: &str| {
        let receipt = store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    name,
                    "ist",
                    "Sprache",
                    SourceDraft::new("test_run"),
                )),
                &access,
            )
            .unwrap();
        receipt.entity_ids[0].clone()
    };
    let c = subjekt("C");
    let csharp = subjekt("C#");
    let cpp = subjekt("C++");
    assert_ne!(c, csharp);
    assert_ne!(c, cpp);
    assert_ne!(csharp, cpp);

    // Wiedererkennung je Name — auch in anderer Schreibweise.
    assert_eq!(subjekt("c#"), csharp);
    assert_eq!(subjekt("c++"), cpp);
    assert_eq!(subjekt("c"), c);

    // Was vorher verschmolz und verschmelzen SOLL, tut es weiterhin.
    let mcp = subjekt("MCP-Client (stdio)");
    assert_eq!(subjekt("mcp client stdio"), mcp);
    assert_eq!(subjekt("  MCP   Client  STDIO "), mcp);
    let issue = subjekt("Issue #12");
    assert_eq!(subjekt("issue 12"), issue, "freistehendes # trennt nicht");

    // Subjekt und Objekt derselben Aussage dürfen ebenfalls nicht verschmelzen.
    let receipt = store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "F",
                "ist nicht",
                "F#",
                SourceDraft::new("test_run"),
            )),
            &access,
        )
        .unwrap();
    assert_ne!(receipt.entity_ids[0], receipt.entity_ids[1]);
}

/// Ein Bundle aus der Zeit davor: EINE Entity „C" trägt das Alias `c`. Sie
/// bleibt erhalten und auffindbar; „C#" bekommt ab jetzt eine eigene Entity,
/// und die Nachbarschaftssuche findet zu jedem Namen die richtige.
#[test]
fn name_mit_zusatz_findet_seine_eigene_entity() {
    use agentkit_graph::retrieval::{find_seeds, GraphQuery};

    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let c = schreibe_subjekt(&store, &access, "C");
    let index = store.snapshot();
    // Ohne passende Entity bleibt der bisherige Treffer (Lesen ist nachsichtig).
    assert_eq!(
        find_seeds(&index, &access.view, &GraphQuery::seeded("C#")),
        vec![c.clone()]
    );

    let csharp = schreibe_subjekt(&store, &access, "C#");
    let index = store.snapshot();
    assert_eq!(
        find_seeds(&index, &access.view, &GraphQuery::seeded("C#")),
        vec![csharp]
    );
    assert_eq!(
        find_seeds(&index, &access.view, &GraphQuery::seeded("C")),
        vec![c]
    );
}

fn schreibe_subjekt(store: &GraphStore, access: &GraphAccess, name: &str) -> String {
    store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                name,
                "ist",
                "Sprache",
                SourceDraft::new("test_run"),
            )),
            access,
        )
        .unwrap()
        .entity_ids[0]
        .clone()
}

/// Ein Alias traf als bloßer Teilstring: die Entity „C" wurde Seed jeder
/// Frage, in der irgendwo ein „c" vorkam.
#[test]
fn alias_trifft_nur_an_tokengrenzen() {
    use agentkit_graph::retrieval::{find_seeds, GraphQuery};

    let store = GraphStore::in_memory();
    let access = GraphAccess::session("tester", "ws", "run-1");
    let c = schreibe_subjekt(&store, &access, "C");
    let index = store.snapshot();

    let fremd = GraphQuery::text("Warum ist der Nutzer frustriert wegen der Rechnung");
    assert!(find_seeds(&index, &access.view, &fremd).is_empty());

    let gemeint = GraphQuery::text("Welche Compiler gibt es für C unter Linux");
    assert_eq!(find_seeds(&index, &access.view, &gemeint), vec![c]);
}

/// Die `summary` einer Episode steht im Body. Enthielt sie selbst eine
/// Überschrift „# Quellen", wurde sie beim Laden dort abgeschnitten.
#[test]
fn episoden_summary_mit_quellen_ueberschrift_bleibt_ganz() {
    use agentkit_graph::EpisodeDraft;

    let dir = temp_bundle("episode");
    let summary = "Teil 1\n\n# Quellen\n\nTeil 2, der wichtige";
    {
        let store = GraphStore::open(&dir).unwrap();
        let access = GraphAccess::session("tester", "ws", "run-1");
        store
            .submit(
                GraphWriteCommand::RecordEpisode(EpisodeDraft::new(
                    summary,
                    SourceDraft::new("agent_turn").excerpt("Beleg"),
                )),
                &access,
            )
            .unwrap();
    }
    let index = GraphStore::open_read_only(&dir).unwrap();
    let geladen: Vec<String> = index.episodes().map(|e| e.summary.clone()).collect();
    assert_eq!(geladen, vec![summary.to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}
