//! Beispiel — Diskussions-Schwarm mit drei Agenten (FakeLlm, kein Netz).
//!
//!     cargo run --example discussion_swarm
//!
//! Moderator, Optimist und Skeptiker diskutieren peer-to-peer: der Moderator
//! verteilt die Frage per Broadcast, sammelt die Einschätzungen und schlägt
//! den Abschluss vor; Optimist und Skeptiker stimmen zu (Konsens-Quorum 2).
//! Ein Consumer-Thread zeigt die Schwarm-Events live — die Nachricht-IDs in
//! den Skripten sind deterministisch, weil der Ablauf eine Kette ist.

use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, Strategy};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use std::sync::Arc;

/// Ein gescripteter Agent — eigene FakeLlm-Instanz pro Agent (der Turn-Cursor
/// des FakeLlm ist global, geteilt wäre der Ablauf nichtdeterministisch).
fn scripted(turns: Vec<Vec<Chunk>>) -> Agent {
    Agent::builder(Arc::new(FakeLlm::new(turns)))
        .strategy(Strategy::Plain)
        .retry_backoff_ms(0)
        .build()
}

fn main() {
    // Moderator: Frage verteilen (Run 1), erste Antwort notieren (Run 2),
    // nach der zweiten Antwort den Abschluss vorschlagen (Run 3, msg-5).
    let moderator = scripted(vec![
        vec![Chunk::tool(
            0,
            "b1",
            "swarm_broadcast",
            r#"{"content":"Diskussionsfrage: Lohnt ein Rewrite des Parsers?","kind":"request"}"#,
        )],
        vec![Chunk::text("Frage an beide verteilt.")],
        vec![Chunk::text("Erste Einschätzung notiert.")],
        vec![Chunk::tool(
            0,
            "p1",
            "swarm_propose",
            r#"{"proposal":"Konsens: Kein Rewrite — gezieltes Refactoring der Fehlerpfade genügt."}"#,
        )],
        vec![Chunk::text("Abschluss vorgeschlagen.")],
    ]);
    // Optimist und Skeptiker: auf die Frage antworten (Run 1), dem
    // Proposal msg-5 zustimmen (Run 2).
    let vote = r#"{"proposal_id":"msg-5","approve":true,"comment":"einverstanden"}"#;
    let optimist = scripted(vec![
        vec![Chunk::tool(
            0,
            "r1",
            "swarm_reply",
            r#"{"content":"Chance: Ein Rewrite würde die Grammatik-Altlasten loswerden."}"#,
        )],
        vec![Chunk::text("Einschätzung abgegeben.")],
        vec![Chunk::tool(0, "v1", "swarm_vote", vote)],
        vec![Chunk::text("Zugestimmt.")],
    ]);
    let skeptiker = scripted(vec![
        vec![Chunk::tool(
            0,
            "r2",
            "swarm_reply",
            r#"{"content":"Risiko: Rewrites verlieren erfahrungsgemäß stille Invarianten."}"#,
        )],
        vec![Chunk::text("Einschätzung abgegeben.")],
        vec![Chunk::tool(0, "v2", "swarm_vote", vote)],
        vec![Chunk::text("Zugestimmt.")],
    ]);

    let handle = SwarmBuilder::new("diskussion")
        .agent("moderator", moderator)
        .agent("optimist", optimist)
        .agent("skeptiker", skeptiker)
        .connect_bidirectional("moderator", "optimist")
        .connect_bidirectional("moderator", "skeptiker")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 2,
        })
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    // Consumer-Thread: zeigt die verschränkten Schwarm-Events.
    let events = handle.events();
    let consumer = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                SwarmEvent::MessageQueued { message } => println!(
                    "[queue] {} -> {:?}: {} ({})",
                    message.from,
                    message.to,
                    message.kind.as_str(),
                    message.id
                ),
                SwarmEvent::TurnCompleted { agent, success, .. } => {
                    println!("[turn ] {agent} fertig (ok={success})")
                }
                SwarmEvent::ProposalCreated { message } => {
                    println!("[prop ] {}: {}", message.from, message.content)
                }
                SwarmEvent::VoteSubmitted { message } => println!("[vote ] {}", message.from),
                SwarmEvent::SwarmCompleted { .. } => break,
                _ => {}
            }
        }
    });

    handle
        .send_initial(
            "moderator",
            "Moderiere die Diskussion und führe einen Konsens herbei.",
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
    println!(
        "\nNachrichten: {} | Turns: {:?} | Dead Letters: {}",
        result.messages_sent,
        result.turns,
        result.dead_letters.len()
    );
}
