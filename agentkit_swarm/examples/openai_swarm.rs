//! Beispiel — echter Zwei-Agenten-Schwarm gegen Azure/OpenAI (Feature `openai`).
//!
//!     cargo run --example openai_swarm --features openai
//!
//! Erwartet die üblichen agentkit-Umgebungsvariablen (AZURE_OPENAI_* bzw.
//! OPENAI_*, siehe agentkit-README). Ein Analyst und ein Kritiker diskutieren
//! eine Frage; der Analyst schlägt den Abschluss vor, der Kritiker stimmt ab.

use agentkit::{azure_from_env, Agent, Strategy};
use agentkit_swarm::{CompletionPolicy, CompletionReason, SwarmBuilder, SwarmEvent};
use std::sync::Arc;

fn main() {
    let llm = Arc::new(azure_from_env().expect("Azure/OpenAI-Umgebung fehlt"));

    let analyst = Agent::builder(llm.clone())
        .system(
            "Du bist der Analyst eines Zwei-Agenten-Schwarms. Beantworte die Aufgabe, \
             hole dir mit swarm_send eine Einschätzung des Kritikers ('kritiker') und \
             schlage danach mit swarm_propose den Abschluss samt Ergebnis vor.",
        )
        .strategy(Strategy::Plain)
        .build();
    let kritiker = Agent::builder(llm)
        .system(
            "Du bist der Kritiker eines Zwei-Agenten-Schwarms. Prüfe eingehende \
             Aussagen knapp und antworte mit swarm_reply. Erhältst du ein Proposal, \
             stimme mit swarm_vote ab (approve nur bei überzeugendem Ergebnis).",
        )
        .strategy(Strategy::Plain)
        .build();

    let handle = SwarmBuilder::new("openai-schwarm")
        .agent("analyst", analyst)
        .agent("kritiker", kritiker)
        .connect_bidirectional("analyst", "kritiker")
        .completion(CompletionPolicy::Consensus {
            required_approvals: 1,
        })
        .max_messages(20)
        .max_runtime(std::time::Duration::from_secs(120))
        .build()
        .expect("Schwarm-Aufbau")
        .start()
        .expect("Schwarm-Start");

    let events = handle.events();
    let consumer = std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                SwarmEvent::MessageQueued { message } => {
                    println!(
                        "[queue] {} -> {:?} ({})",
                        message.from, message.to, message.id
                    )
                }
                SwarmEvent::TurnCompleted { agent, success, .. } => {
                    println!("[turn ] {agent} fertig (ok={success})")
                }
                SwarmEvent::SwarmCompleted { .. } => break,
                _ => {}
            }
        }
    });

    handle
        .send_initial(
            "analyst",
            "Nenne die wichtigste Design-Eigenschaft des Actor-Modells und begründe sie kurz.",
        )
        .expect("Initialaufgabe");
    let result = handle.join();
    let _ = consumer.join();

    match result.reason {
        CompletionReason::Consensus {
            proposal,
            approvals,
        } => {
            println!(
                "\n=== Konsens ({approvals} Zustimmungen) ===\n{}",
                proposal.content
            )
        }
        other => println!("\n=== Ende ohne Konsens: {other:?} ==="),
    }
    println!(
        "Nachrichten: {} | Dead Letters: {}",
        result.messages_sent,
        result.dead_letters.len()
    );
}
