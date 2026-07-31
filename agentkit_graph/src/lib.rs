//! agentkit-graph — graphbasiertes Wissen für agentkit: Working Graph, Canonical
//! Graph, Provenance.
//!
//! Die Arbeitsteilung im Repo:
//!
//! | Crate | Verantwortung |
//! |---|---|
//! | agentkit | führt den LLM-/Tool-Loop aus |
//! | ctxman | verwaltet den aktuellen Kontext EINES Agenten |
//! | **agentkit-graph** | speichert Wissen — dauerhaft und über Agenten hinweg |
//! | agentkit-swarm | verwaltet Agent-Actors und Peer-Kommunikation |
//!
//! > Der Graph speichert Wissen. Was davon gerade im Modellkontext landet,
//! > entscheidet der Recall — nie der Graph selbst.
//!
//! Die Abhängigkeit läuft in eine Richtung: dieses Crate kennt agentkit, agentkit
//! kennt dieses Crate nicht. Eingeklinkt wird über die vorhandene Naht — Tools in
//! eine [`ToolRegistry`](agentkit::ToolRegistry).
//!
//! ```
//! use std::sync::Arc;
//! use agentkit_graph::{
//!     ClaimDraft, ClaimStatus, GraphAccess, GraphQuery, GraphStore, GraphWriteCommand, SourceDraft,
//! };
//!
//! let store = Arc::new(GraphStore::in_memory());
//! // Autor, Lauf und Ziel-Scope kommen aus dem Zugriff, nicht aus dem Aufruf.
//! let access = GraphAccess::session("tester", "agentkit-rs", "run-4711");
//!
//! let draft = ClaimDraft::new(
//!     "Parallele Tool-Aufrufe",
//!     "verursachen",
//!     "Session-Konkurrenz",
//!     SourceDraft::new("test_run").excerpt("cargo test mcp:: — 2 Fehlschläge"),
//! )
//! .status(ClaimStatus::Observation)
//! .confidence(0.82);
//!
//! let receipt = store.submit(GraphWriteCommand::RecordClaim(draft), &access).unwrap();
//! // Der Commit ist synchron: ab hier sieht JEDER Leser die Aussage.
//! let claim_id = receipt.claim_id.clone().unwrap();
//!
//! let index = store.snapshot();
//! let found = agentkit_graph::retrieval::search(
//!     &index,
//!     &access.view,
//!     &GraphQuery::text("Was ist über parallele Tool-Aufrufe bekannt?"),
//! );
//! assert_eq!(found.claims.len(), 1);
//! assert_eq!(found.claims[0].claim.id, claim_id);
//! ```

pub mod access;
pub mod agent;
pub mod error;
pub mod export;
pub mod model;
pub mod retrieval;
pub mod store;
pub mod tools;
pub mod write;

pub use access::GraphAccess;
pub use agent::{GraphAgent, RecallPolicy};
pub use error::GraphError;
pub use export::{export, GraphExport};
pub use model::{
    normalize, ClaimId, ClaimStatus, EntityId, EpisodeId, GraphClaim, GraphEntity, GraphEpisode,
    GraphLayer, GraphRevision, GraphScope, GraphSource, GraphTarget, GraphView, SourceId,
};
pub use retrieval::{GraphQuery, ScoredClaim, Subgraph};
pub use store::{GraphIndex, GraphStats, GraphStore, JOURNAL_FILE};
pub use tools::{register_graph_tools, GRAPH_SYSTEM};
pub use write::{ClaimDraft, EpisodeDraft, GraphReceipt, GraphWriteCommand, SourceDraft};
