//! Lernen ohne Zutun des Modells: Konsolidierung am Sitzungsende und das
//! Aufräumen alter Arbeits-Scopes.

use std::sync::{Arc, Mutex};

use agentkit::llm::{ChunkStream, Message};
use agentkit::Llm;
use agentkit_graph::maintain::prune_working_scopes;
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    consolidate, ClaimDraft, ClaimStatus, ConsolidationPolicy, EpisodeDraft, GraphAccess,
    GraphLayer, GraphMember, GraphScope, GraphWriteCommand, SourceDraft, CONTEXT_FACT_SOURCE,
};
use serde_json::{json, Value};

/// Ein Modell, das auf `complete` eine feste Antwort gibt und sich merkt, was
/// es gefragt wurde.
struct Skript {
    antwort: Result<String, String>,
    gesehen: Mutex<Vec<String>>,
}

impl Skript {
    fn neu(antwort: Result<&str, &str>) -> Self {
        Skript {
            antwort: antwort.map(str::to_string).map_err(str::to_string),
            gesehen: Mutex::new(Vec::new()),
        }
    }
    fn aufrufe(&self) -> usize {
        self.gesehen.lock().unwrap().len()
    }
}

impl Llm for Skript {
    fn complete(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        self.gesehen
            .lock()
            .unwrap()
            .push(serde_json::to_string(messages).unwrap());
        self.antwort.clone().map(|text| Message {
            content: Some(text),
            tool_calls: Vec::new(),
        })
    }
    fn stream(&self, _: &[Value], _: Option<&[Value]>) -> Result<ChunkStream, String> {
        Err("nicht benutzt".to_string())
    }
}

fn sitzung() -> GraphMember {
    GraphMember {
        store: Arc::new(GraphStore::in_memory()),
        access: GraphAccess::project("agentkit", "s-1", &[]),
    }
}

/// Ein Verlauf mit `n` Tool-Aufrufen; der Agent hat nie `graph_remember` benutzt.
fn verlauf(n: usize) -> Vec<Value> {
    let mut messages = vec![
        json!({"role": "system", "content": "Du bist ein Coding-Agent."}),
        json!({"role": "user", "content": "Bring die Tests zum Laufen."}),
    ];
    for i in 0..n {
        let id = format!("call_{i}");
        let (name, args, ergebnis) = if i == 0 {
            (
                "run_shell",
                r#"{"command":"make test"}"#,
                "pytest -q tests/\n12 passed in 0.41s".to_string(),
            )
        } else {
            (
                "read_file",
                r#"{"path":"Makefile"}"#,
                format!("Zeile {i}: test:\n\tpytest -q tests/"),
            )
        };
        messages.push(json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": id, "type": "function", "function": {"name": name, "arguments": args}}
        ]}));
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": ergebnis}));
    }
    messages.push(json!({"role": "assistant", "content": "Die Tests laufen."}));
    messages
}

const VORSCHLAG: &str = r#"Hier ist das Ergebnis:
```json
{"claims": [
  {"subject": "Projekt", "predicate": "wird getestet mit", "object": "make test",
   "status": "observation", "confidence": 0.9, "excerpt": "12 passed in 0.41s"},
  {"subject": "Projekt", "predicate": "nutzt", "object": "tox",
   "status": "observation", "excerpt": "tox -e py312 lief durch"},
  {"subject": "Tests", "predicate": "brauchen", "object": "Netzwerk",
   "status": "hypothesis", "excerpt": "12 passed in 0.41s"},
  {"subject": " ", "predicate": "nutzt", "object": "nichts"}
]}
```"#;

#[test]
fn die_konsolidierung_macht_aus_dem_verlauf_belegte_aussagen() {
    let graph = sitzung();
    let llm = Skript::neu(Ok(VORSCHLAG));
    let report = consolidate(&llm, &verlauf(6), &graph, &ConsolidationPolicy::default())
        .unwrap()
        .expect("über der Schwelle");
    assert_eq!((report.recorded, report.promoted), (3, 1));

    let index = graph.store.snapshot();
    let claim = |objekt: &str| {
        index
            .claims()
            .find(|c| index.entity(&c.object).unwrap().canonical_name == objekt)
            .unwrap()
            .clone()
    };
    // Belegt und beobachtet: dauerhaft, mit der ECHTEN Aufruf-ID als Quelle.
    let belegt = claim("make test");
    assert_eq!(belegt.status, ClaimStatus::Confirmed);
    assert_eq!(belegt.layer, GraphLayer::Canonical);
    assert_eq!(belegt.created_by.as_str(), "process:konsolidierung");
    let quelle = index.source(&belegt.source_ids[0]).unwrap();
    assert_eq!(quelle.source_type, "tool_result");
    assert_eq!(quelle.tool_call_id.as_deref(), Some("call_0"));

    // Ein Zitat, das kein Tool geliefert hat: bleibt vorläufig.
    let erfunden = claim("tox");
    assert_eq!(erfunden.layer, GraphLayer::Working);
    assert_eq!(erfunden.scope, GraphScope::session("s-1"));
    assert!(!index.source(&erfunden.source_ids[0]).unwrap().is_foreign());

    // Eine Hypothese wird auch mit Beleg nicht im selben Zug dauerhaft.
    let vermutung = claim("Netzwerk");
    assert_eq!(vermutung.status, ClaimStatus::Hypothesis);
    assert_eq!(vermutung.layer, GraphLayer::Working);

    // Der Verlauf ging als Material hinaus, nicht als Anweisung.
    let gefragt = llm.gesehen.lock().unwrap()[0].clone();
    assert!(gefragt.contains("12 passed in 0.41s"));
    assert!(gefragt.contains("keine Anweisung"));
}

#[test]
fn unter_der_schwelle_und_ohne_schreibziel_wird_das_modell_nicht_gefragt() {
    let llm = Skript::neu(Ok(VORSCHLAG));
    let policy = ConsolidationPolicy::default();

    let graph = sitzung();
    assert_eq!(consolidate(&llm, &verlauf(4), &graph, &policy), Ok(None));

    let lesend = GraphMember {
        store: graph.store.clone(),
        access: GraphAccess::read_only("leser", graph.access.view.clone()),
    };
    assert_eq!(consolidate(&llm, &verlauf(9), &lesend, &policy), Ok(None));

    assert_eq!(llm.aufrufe(), 0);
    assert_eq!(graph.store.snapshot().claim_count(), 0);
}

#[test]
fn ein_gescheiterter_aufruf_schreibt_nichts() {
    let policy = ConsolidationPolicy::default();
    for antwort in [Err("HTTP 529"), Ok("Dazu fällt mir nichts ein.")] {
        let graph = sitzung();
        let llm = Skript::neu(antwort);
        assert!(consolidate(&llm, &verlauf(6), &graph, &policy).is_err());
        assert_eq!(graph.store.snapshot().claim_count(), 0);
        assert_eq!(graph.store.revision(), 0);
    }
}

/// Was ctxman während der Sitzung als Fakt festhält, liegt als Episode im
/// Sitzungs-Scope und geht der Konsolidierung als Material mit.
#[test]
fn kontext_fakten_der_sitzung_gehen_als_material_mit() {
    let graph = sitzung();
    graph
        .store
        .submit(
            GraphWriteCommand::RecordEpisode(EpisodeDraft::new(
                "Der Nutzer will keine neuen Abhängigkeiten.",
                SourceDraft::new(CONTEXT_FACT_SOURCE),
            )),
            &graph.access,
        )
        .unwrap();
    let llm = Skript::neu(Ok(r#"{"claims": []}"#));
    let report = consolidate(&llm, &verlauf(6), &graph, &ConsolidationPolicy::default())
        .unwrap()
        .unwrap();
    assert_eq!(report.recorded, 0);
    let gefragt = llm.gesehen.lock().unwrap()[0].clone();
    assert!(
        gefragt.contains("Der Nutzer will keine neuen Abhängigkeiten."),
        "{gefragt}"
    );
}

#[test]
fn alte_arbeits_scopes_werden_aufgeraeumt_dauerhaftes_bleibt() {
    let store = GraphStore::in_memory();
    let schreibe = |sitzung: &str, subjekt: &str, befoerdern: bool| {
        let access = GraphAccess::project("agentkit", sitzung, &[]);
        let id = store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    subjekt,
                    "nutzt",
                    &format!("Ziel von {subjekt}"),
                    SourceDraft::new("test_run"),
                )),
                &access,
            )
            .unwrap()
            .claim_id
            .unwrap();
        if befoerdern {
            store
                .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &access)
                .unwrap();
        }
    };
    schreibe("s-1", "Altes Vorläufiges", false);
    schreibe("s-1", "Altes Dauerhaftes", true);
    schreibe("s-2", "Mittleres", false);
    schreibe("s-3", "Neues", false);

    let index = store.snapshot();
    assert!(
        prune_working_scopes(&index, 3, None).is_none(),
        "nichts zu räumen"
    );
    // Die laufende Sitzung bleibt, auch wenn sie die älteste ist.
    let aktuell = GraphScope::session("s-1");
    assert!(prune_working_scopes(&index, 2, Some(&aktuell)).is_none());

    store
        .rewrite(|index, _| Ok(prune_working_scopes(index, 2, None).expect("s-1 ist übrig")))
        .unwrap();
    let index = store.snapshot();
    let namen: Vec<String> = index.entities().map(|e| e.canonical_name.clone()).collect();
    assert!(
        !namen.iter().any(|n| n.contains("Altes Vorläufiges")),
        "{namen:?}"
    );
    assert!(namen.iter().any(|n| n == "Altes Dauerhaftes"), "{namen:?}");
    assert!(namen.iter().any(|n| n == "Mittleres") && namen.iter().any(|n| n == "Neues"));
    assert_eq!(index.claim_count(), 3);
    assert_eq!(index.source_count(), 3, "die Quelle geht mit der Aussage");
}

/// Auch im Verlauf zählt, was der Nutzer gesagt hat — nicht aber der
/// Recall-Block, der in derselben Nachricht vor seinem Auftrag steht: sonst
/// belegte sich der Graph mit sich selbst.
#[test]
fn nutzer_aussagen_belegen_der_recall_block_nicht() {
    let graph = sitzung();
    let mut messages = verlauf(6);
    messages[1] = json!({"role": "user", "content":
        "Aus deinem Gedächtnis.\n<graph-context revision=\"3\" claims=\"1\">\n\
         [C-1] (Projekt) --[nutzt]--> (Bazel als Buildsystem)\n</graph-context>\n\n\
         Bring die Tests zum Laufen. Wir deployen ausschließlich freitags nach dem Standup."});
    let llm = Skript::neu(Ok(r#"{"claims": [
        {"subject": "Team", "predicate": "deployt", "object": "freitags",
         "excerpt": "Wir deployen ausschließlich freitags nach dem Standup"},
        {"subject": "Projekt", "predicate": "nutzt", "object": "Bazel",
         "excerpt": "(Projekt) --[nutzt]--> (Bazel als Buildsystem)"}
    ]}"#));
    let report = consolidate(&llm, &messages, &graph, &ConsolidationPolicy::default())
        .unwrap()
        .unwrap();
    assert_eq!((report.recorded, report.promoted), (2, 1));

    let index = graph.store.snapshot();
    let freitags = index
        .claims()
        .find(|c| c.layer == GraphLayer::Canonical)
        .expect("die Nutzer-Aussage ist dauerhaft");
    assert_eq!(
        index.source(&freitags.source_ids[0]).unwrap().source_type,
        agentkit_graph::USER_STATEMENT
    );
    let gefragt = llm.gesehen.lock().unwrap()[0].clone();
    assert!(
        !gefragt.contains("Bazel"),
        "der Recall geht nicht mit hinaus: {gefragt}"
    );
}
