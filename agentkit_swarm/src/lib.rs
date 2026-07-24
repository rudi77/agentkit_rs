//! agentkit-swarm — ein Actor-basiertes Agent-to-Agent-System auf `agentkit`.
//!
//! Kernprinzip: **Ein Schwarm-Agent ist ein normaler [`agentkit::Agent`], der
//! exklusiv von einem Actor besessen wird.** Der Actor fügt Mailbox, Identität
//! und Peer-Kommunikation hinzu, ohne den Agent-Loop zu verändern. Es gibt
//! keinen zentralen Agenten, der die Zusammenarbeit semantisch steuert —
//! Agenten kommunizieren peer-to-peer über Tools (`swarm_send`, `swarm_reply`,
//! …), und der Schwarm endet über eine deterministische Completion Policy
//! (Konsens per `swarm_propose`/`swarm_vote`) oder über Limits.
//!
//! ```no_run
//! use std::sync::Arc;
//! use agentkit::testing::FakeLlm;
//! use agentkit::llm::Chunk;
//! use agentkit::{Agent, Strategy};
//! use agentkit_swarm::{CompletionPolicy, SwarmBuilder};
//!
//! // Zwei Agenten, gescriptet ohne Netz: A schlägt den Abschluss vor, B stimmt zu.
//! let a = Agent::builder(Arc::new(FakeLlm::new(vec![
//!     vec![Chunk::tool(0, "p1", "swarm_propose", "{\"proposal\":\"fertig\"}")],
//!     vec![Chunk::text("Vorschlag eingereicht.")],
//! ])))
//! .strategy(Strategy::Plain)
//! .build();
//! let b = Agent::builder(Arc::new(FakeLlm::new(vec![
//!     vec![Chunk::tool(0, "v1", "swarm_vote", "{\"proposal_id\":\"msg-2\",\"approve\":true}")],
//!     vec![Chunk::text("Zugestimmt.")],
//! ])))
//! .strategy(Strategy::Plain)
//! .build();
//!
//! let handle = SwarmBuilder::new("demo")
//!     .agent("a", a)
//!     .agent("b", b)
//!     .connect_bidirectional("a", "b")
//!     .completion(CompletionPolicy::Consensus { required_approvals: 1 })
//!     .build()
//!     .unwrap()
//!     .start()
//!     .unwrap();
//! handle.send_initial("a", "Klärt die Frage und schließt ab.").unwrap();
//! let result = handle.join();
//! println!("{:?}", result.reason);
//! ```

pub mod actor_ref;
pub mod agent_actor;
pub mod completion;
pub mod directory;
pub mod error;
pub mod events;
pub mod mailbox;
pub mod message;
pub mod runtime;
pub mod supervisor;
pub mod tools;
pub mod topology;

pub use actor_ref::AgentActorRef;
pub use completion::{CompletionPolicy, CompletionReason, DeadLetter, SwarmResult};
pub use directory::{sorted_peer_ids, PeerDirectory};
pub use error::SwarmError;
pub use events::{SwarmEvent, SwarmEventBus};
pub use mailbox::DEFAULT_MAILBOX_CAPACITY;
pub use message::{
    AgentCommand, AgentId, DeliveryResult, MessageKind, Recipient, SwarmMessage, RUNTIME_SENDER,
};
pub use runtime::{Swarm, SwarmBuilder, SwarmHandle, DEFAULT_MAX_HOPS, DEFAULT_MAX_MESSAGES};
pub use topology::Topology;
