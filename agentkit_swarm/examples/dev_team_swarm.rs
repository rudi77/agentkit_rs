//! Beispiel — Entwicklungs-Team als Ketten-Topologie (FakeLlm, kein Netz).
//!
//!     cargo run --example dev_team_swarm
//!
//! planner ↔ coder ↔ reviewer: der Planner kann den Reviewer NICHT direkt
//! erreichen (Topologie als Capability — er besitzt schlicht keine Referenz).
//! Ablauf: Planner delegiert an den Coder, der Coder holt sich das Review,
//! arbeitet die Kritik ein und schlägt den Abschluss vor; Planner und
//! Reviewer stimmen zu. Der Name ist bewusst nicht "coding_swarm" — so heißt
//! bereits ein Beispiel in agent_framework_rs (task-Tool-basiert, eine Ebene
//! tief); dieses hier zeigt das Actor-Modell mit langlebigen Peers.

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use std::sync::Arc;

fn scripted(turns: Vec<Vec<Chunk>>) -> Agent {
    Agent::builder(Arc::new(FakeLlm::new(turns)))
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build()
}

fn main() {
    // Kette ohne Races -> deterministische Nachricht-IDs:
    // msg-1 Initial, msg-2 planner->coder, msg-3 coder->reviewer,
    // msg-4 reviewer->coder, msg-5 Proposal des Coders.
    let planner = scripted(vec![
        vec![Chunk::tool(
            0,
            "s1",
            "swarm_send",
            r#"{"to":"coder","content":"Implementiere das Retry-Backoff im MCP-Client.","kind":"task"}"#,
        )],
        vec![Chunk::text("Aufgabe delegiert.")],
        vec![Chunk::tool(
            0,
            "v1",
            "swarm_vote",
            r#"{"proposal_id":"msg-5","approve":true,"comment":"Plan erfüllt"}"#,
        )],
        vec![Chunk::text("Zugestimmt.")],
    ]);
    let coder = scripted(vec![
        vec![Chunk::tool(
            0,
            "s2",
            "swarm_send",
            r#"{"to":"reviewer","content":"Review bitte: Backoff mit Jitter, 3 Versuche.","kind":"request"}"#,
        )],
        vec![Chunk::text("Review angefragt.")],
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"Retry-Backoff umgesetzt, Review-Hinweis (Obergrenze) eingearbeitet."}"#,
        )],
        vec![Chunk::text("Abschluss vorgeschlagen.")],
    ]);
    let reviewer = scripted(vec![
        vec![Chunk::tool(
            0,
            "r1",
            "swarm_reply",
            r#"{"content":"Sieht gut aus — ergänze eine Obergrenze fürs Backoff.","kind":"critique"}"#,
        )],
        vec![Chunk::text("Kritik übermittelt.")],
        vec![Chunk::tool(
            0,
            "v2",
            "swarm_vote",
            r#"{"proposal_id":"msg-5","approve":true,"comment":"Hinweis umgesetzt"}"#,
        )],
        vec![Chunk::text("Zugestimmt.")],
    ]);

    let handle = SwarmBuilder::new("dev-team")
        .agent("planner", planner)
        .agent("coder", coder)
        .agent("reviewer", reviewer)
        .connect_bidirectional("planner", "coder")
        .connect_bidirectional("coder", "reviewer")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 2,
        })
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    let events = handle.events();
    let consumer = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                SwarmEvent::MessageQueued { message } => println!(
                    "[queue] {} -> {:?}: {}",
                    message.from,
                    message.to,
                    message.kind.as_str()
                ),
                SwarmEvent::TurnCompleted { agent, .. } => println!("[turn ] {agent} fertig"),
                SwarmEvent::SwarmCompleted { .. } => break,
                _ => {}
            }
        }
    });

    handle
        .send_initial(
            "planner",
            "Plane und verantworte die Umsetzung des Retry-Backoffs.",
        )
        .expect("Initialaufgabe");
    let result = handle.join();
    consumer.join().unwrap();

    println!();
    match &result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            println!(
                "=== Konsens ({approvals} Zustimmungen) ===\n{}",
                proposal.content
            )
        }
        other => println!("=== Ende ohne Konsens: {other:?} ==="),
    }
}
