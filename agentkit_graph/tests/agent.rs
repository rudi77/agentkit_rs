//! Der [`GraphAgent`]-Wrapper — Recall davor, Episode danach, Agent-Loop unverändert.

use std::sync::Arc;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, EventBus, Llm, ToolRegistry};
use agentkit_graph::store::GraphStore;
use agentkit_graph::{
    register_graph_tools, ClaimDraft, GraphAccess, GraphAgent, GraphWriteCommand, RecallPolicy,
    SourceDraft,
};

fn store_mit_wissen(access: &GraphAccess) -> Arc<GraphStore> {
    let store = Arc::new(GraphStore::in_memory());
    let id = store
        .submit(
            GraphWriteCommand::RecordClaim(ClaimDraft::new(
                "MCP Client",
                "nutzt",
                "stdio-Session",
                SourceDraft::new("document"),
            )),
            access,
        )
        .unwrap()
        .claim_id
        .unwrap();
    store
        .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, access)
        .unwrap();
    store
}

/// Was das Modell im ersten Zug als Nachrichten gesehen hat.
fn erste_nachrichten(llm: &Arc<FakeLlm>) -> String {
    let seen = llm.seen_messages.lock().unwrap();
    serde_json::to_string(&seen[0]).unwrap()
}

#[test]
fn recall_stellt_den_graphausschnitt_vor_die_aufgabe() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&access);
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("Verstanden.")]]));
    let agent = Agent::new(llm.clone() as Arc<dyn Llm>, ToolRegistry::new());

    let mut graph_agent = GraphAgent::new(agent, store.clone(), access);
    let antwort = graph_agent.run("Was weißt du über den MCP Client?");
    assert_eq!(antwort, "Verstanden.");

    let gesehen = erste_nachrichten(&llm);
    assert!(gesehen.contains("<graph-context"), "{gesehen}");
    assert!(gesehen.contains("--[nutzt]-->"), "{gesehen}");
    assert!(gesehen.contains("--- Aufgabe ---"), "{gesehen}");
}

#[test]
fn ohne_treffer_bleibt_die_aufgabe_unveraendert() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&access);
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("ok")]]));
    let agent = Agent::new(llm.clone() as Arc<dyn Llm>, ToolRegistry::new());

    let mut graph_agent = GraphAgent::new(agent, store, access);
    assert_eq!(
        graph_agent.compose_task("Etwas völlig anderes"),
        "Etwas völlig anderes"
    );
    graph_agent.run("Etwas völlig anderes");
    assert!(!erste_nachrichten(&llm).contains("<graph-context"));
}

#[test]
fn jeder_zug_hinterlaesst_eine_episode() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&access);
    let vorher = store.stats().episodes;
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("fertig")]]));
    let agent = Agent::new(llm as Arc<dyn Llm>, ToolRegistry::new());

    let mut graph_agent = GraphAgent::new(agent, store.clone(), access);
    graph_agent.run("Prüfe den stdio-Pfad");

    let index = store.snapshot();
    assert_eq!(index.episode_count(), vorher + 1);
    let episode = index.episodes().last().unwrap();
    assert_eq!(episode.actor, "tester");
    assert!(episode.summary.contains("Prüfe den stdio-Pfad"));
    assert!(episode.summary.contains("fertig"));
}

#[test]
fn tools_only_schaltet_recall_und_episode_ab() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&access);
    let revision_vorher = store.revision();
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("ok")]]));
    let agent = Agent::new(llm.clone() as Arc<dyn Llm>, ToolRegistry::new());

    let mut graph_agent =
        GraphAgent::new(agent, store.clone(), access).with_policy(RecallPolicy::tools_only());
    graph_agent.run("Was weißt du über den MCP Client?");

    assert!(!erste_nachrichten(&llm).contains("<graph-context"));
    assert_eq!(store.revision(), revision_vorher);
}

#[test]
fn ein_nur_lesender_agent_schreibt_keine_episode() {
    let schreiber = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&schreiber);
    let revision_vorher = store.revision();
    let lesend = GraphAccess::read_only("leser", schreiber.view.clone());

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("ok")]]));
    let agent = Agent::new(llm as Arc<dyn Llm>, ToolRegistry::new());
    let mut graph_agent = GraphAgent::new(agent, store.clone(), lesend);
    graph_agent.run("Was nutzt der MCP Client?");

    assert_eq!(store.revision(), revision_vorher);
}

#[test]
fn der_agent_schreibt_ueber_das_tool_in_den_graphen() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = Arc::new(GraphStore::in_memory());
    let mut tools = ToolRegistry::new();
    register_graph_tools(&mut tools, store.clone(), access.clone());

    // Zug 1: das Modell merkt sich eine Beobachtung. Zug 2: es antwortet.
    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "graph_remember",
            r#"{"subject":"Test T-41","predicate":"belegt","object":"Hypothese H-12","confidence":0.9}"#,
        )],
        vec![Chunk::text("Habe ich mir gemerkt.")],
    ]));
    let mut agent = Agent::new(llm as Arc<dyn Llm>, tools);
    let antwort = agent.run("Merke dir das Testergebnis.");
    assert_eq!(antwort, "Habe ich mir gemerkt.");

    let index = store.snapshot();
    assert_eq!(index.claim_count(), 1);
    let claim = index.claims().next().unwrap();
    assert_eq!(claim.predicate, "belegt");
    assert_eq!(claim.created_by, "tester");
    assert!((claim.confidence - 0.9).abs() < 1e-6);
}

#[test]
fn run_on_bus_verhaelt_sich_wie_run() {
    let access = GraphAccess::session("tester", "ws", "run-1");
    let store = store_mit_wissen(&access);
    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text("fertig")]]));
    let agent = Agent::new(llm.clone() as Arc<dyn Llm>, ToolRegistry::new());

    let bus = EventBus::new();
    let events = bus.subscribe();
    let mut graph_agent = GraphAgent::new(agent, store.clone(), access);
    let antwort = graph_agent.run_on_bus("Was nutzt der MCP Client?", &bus, 7, None, "grapher");

    assert_eq!(antwort, "fertig");
    assert!(erste_nachrichten(&llm).contains("<graph-context"));
    assert_eq!(store.snapshot().episode_count(), 1);
    // Der Bus bleibt der Bus: die Events tragen weiterhin task_id und source.
    let gesehen: Vec<_> = events.try_iter().collect();
    assert!(gesehen
        .iter()
        .all(|e| e.task_id == 7 && e.source == "grapher"));
    assert!(!gesehen.is_empty());
}
