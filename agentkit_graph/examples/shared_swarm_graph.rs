//! Gemeinsames Arbeitsgedächtnis: zwei Agenten, ein Working Graph — offline.
//!
//! ```bash
//! cargo run --manifest-path agentkit_graph/Cargo.toml --example shared_swarm_graph
//! ```
//!
//! Der Tester schreibt einen Befund in den gemeinsamen Scope, der Developer liest
//! ihn — ohne dass ihn jemand als Nachricht durch dessen Kontext schicken müsste.
//! Genau das ist der Unterschied zwischen Peer-Nachricht und Shared Memory:
//! die Nachricht fordert eine Handlung an, der Graph trägt die Evidenz.

use std::sync::Arc;
use std::thread;

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Llm, ToolRegistry};
use agentkit_graph::{register_graph_tools, GraphAccess, GraphStore};

fn agent_mit_graph(
    store: &Arc<GraphStore>,
    id: &str,
    turns: Vec<Vec<Chunk>>,
) -> (Agent, GraphAccess) {
    // Gleicher Schwarm-Lauf ⇒ gleicher Schreib-Scope ⇒ geteiltes Gedächtnis.
    let access = GraphAccess::swarm(id, "agentkit-rs", "run-4711");
    let mut tools = ToolRegistry::new();
    register_graph_tools(&mut tools, store.clone(), access.clone());
    (
        Agent::new(Arc::new(FakeLlm::new(turns)) as Arc<dyn Llm>, tools),
        access,
    )
}

fn main() {
    let store = Arc::new(GraphStore::in_memory());

    let (mut tester, _) = agent_mit_graph(
        &store,
        "tester",
        vec![
            vec![Chunk::tool(
                0,
                "t1",
                "graph_remember",
                r#"{"subject":"Testlauf T-41","predicate":"zeigt",
                    "object":"Deadlock im stdin-Lock","status":"observation",
                    "confidence":0.9,"excerpt":"thread 'mcp' blockiert nach 2 s"}"#,
            )],
            vec![Chunk::text("Befund im Graph abgelegt (O-42).")],
        ],
    );

    let (mut developer, _) = agent_mit_graph(
        &store,
        "developer",
        vec![
            vec![Chunk::tool(
                0,
                "d1",
                "graph_search",
                r#"{"query":"Was zeigt Testlauf T-41?"}"#,
            )],
            vec![Chunk::text(
                "Ich sehe den Deadlock im stdin-Lock und nehme ihn mir vor.",
            )],
        ],
    );

    // Ein Actor = ein Thread = ein &mut Agent — dieselbe Aufteilung wie in
    // agentkit_swarm. Der Graph wird nur über `Arc` geteilt.
    let antwort_tester = thread::spawn(move || tester.run("Führe die MCP-Tests aus.")).join();
    println!("tester    : {}", antwort_tester.unwrap());

    let antwort_dev = thread::spawn(move || {
        developer.run("Der Tester meldet einen Befund zu T-41 — schau in den Graphen.")
    })
    .join();
    println!("developer : {}", antwort_dev.unwrap());

    println!("\nGemeinsamer Stand: {:?}", store.stats());
    let index = store.snapshot();
    for claim in index.claims() {
        println!(
            "  [{}] {} --[{}]--> {}  (von {}, {})",
            claim.id,
            index.entity(&claim.subject).unwrap().canonical_name,
            claim.predicate,
            index.entity(&claim.object).unwrap().canonical_name,
            claim.created_by,
            claim.status
        );
    }
}
