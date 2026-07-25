//! Ein einzelner Agent mit Gedächtnis über Sitzungen hinweg — offline, mit FakeLlm.
//!
//! ```bash
//! cargo run --manifest-path agentkit_graph/Cargo.toml --example graph_agent
//! ```
//!
//! Sitzung 1 beobachtet etwas und macht daraus dauerhaftes Wissen.
//! Sitzung 2 startet mit leerem Kontext — und weiß es trotzdem.

use std::sync::Arc;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Llm, ToolRegistry};
use agentkit_graph::{
    register_graph_tools, GraphAccess, GraphAgent, GraphStore, GraphWriteCommand,
};

fn main() {
    // Ein Prozess, ein Store: in echt liegt er unter `--graph DIR` auf der Platte.
    let store = Arc::new(GraphStore::in_memory());

    // ---------------------------------------------------------- Sitzung 1
    let access = GraphAccess::session("coder", "agentkit-rs", "session-1");
    let mut tools = ToolRegistry::new();
    register_graph_tools(&mut tools, store.clone(), access.clone());

    let llm = Arc::new(FakeLlm::new(vec![
        vec![Chunk::tool(
            0,
            "c1",
            "graph_remember",
            r#"{"subject":"Parallele Tool-Aufrufe","predicate":"verursachen",
                "object":"Konkurrenz auf der stdio-Session","status":"observation",
                "confidence":0.85,"excerpt":"cargo test mcp:: — 2 Fehlschläge"}"#,
        )],
        vec![Chunk::text("Beobachtung festgehalten.")],
    ]));
    let mut agent = Agent::new(llm as Arc<dyn Llm>, tools);
    println!(
        "Sitzung 1: {}",
        agent.run("Untersuche die MCP-Fehlschläge.")
    );

    // Der Agent hat eine vorläufige Beobachtung geschrieben. Erst die Promotion
    // macht daraus dauerhaftes Wissen — hier durch die Laufzeit, nach Prüfung.
    let claim_id = store
        .snapshot()
        .claims()
        .next()
        .expect("eine Beobachtung")
        .id
        .clone();
    store
        .submit(GraphWriteCommand::PromoteClaim { claim_id }, &access)
        .expect("Promotion");

    println!("Graph nach Sitzung 1: {:?}\n", store.stats());

    // ---------------------------------------------------------- Sitzung 2
    // Neuer Lauf, neuer Scope, leerer Gesprächsverlauf.
    let access = GraphAccess::session("coder", "agentkit-rs", "session-2");
    let mut tools = ToolRegistry::new();
    register_graph_tools(&mut tools, store.clone(), access.clone());

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::text(
        "Laut Graph verursachen parallele Tool-Aufrufe Konkurrenz auf der stdio-Session.",
    )]]));
    let agent = Agent::new(llm.clone() as Arc<dyn Llm>, tools);
    let mut graph_agent = GraphAgent::new(agent, store.clone(), access);

    // Beachte die Flexion: "parallelen Tool-Aufrufen" gegen "Parallele Tool-Aufrufe".
    // Der Recall vergleicht auch Wortstämme, sonst verfehlte er genau die Fragen,
    // die ein Agent auf Deutsch wirklich stellt.
    let frage = "Gibt es bekannte Probleme mit parallelen Tool-Aufrufen?";
    // Genau das bekommt das Modell zu sehen — der Recall steht vor der Aufgabe.
    println!(
        "--- Prompt der zweiten Sitzung ---\n{}\n",
        graph_agent.compose_task(frage)
    );
    println!("Sitzung 2: {}", graph_agent.run(frage));
    println!("\nGraph am Ende: {:?}", store.stats());
}
