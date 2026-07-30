//! agentkit-work — Arbeits-Runtime für agentkit: Work Items, die einen
//! einzelnen Agent-Lauf überleben.
//!
//! Die Arbeitsteilung im Repo:
//!
//! | Crate | Verantwortung |
//! |---|---|
//! | agentkit | führt den LLM-/Tool-Loop EINES Agenten aus |
//! | ctxman | verwaltet den aktuellen Kontext eines Agenten |
//! | agentkit-graph | speichert Wissen — dauerhaft und über Agenten hinweg |
//! | agentkit-swarm | verwaltet Agent-Actors und Peer-Kommunikation |
//! | **agentkit-work** | zerlegt ein Vorhaben in Work Items und arbeitet sie ab — **überlebt den Prozess** |
//!
//! > Der Graph speichert Wissen, agentkit-work speichert Fortschritt: welche
//! > Teilaufgabe ist offen, wer bearbeitet sie, was ist schon geprüft, wo wird
//! > nach einem Absturz weitergemacht.
//!
//! Die Abhängigkeit läuft in eine Richtung: dieses Crate kennt agentkit,
//! agentkit kennt dieses Crate nicht. Eingeklinkt wird über dieselbe Naht wie
//! agentkit-graph — Tools in eine `ToolRegistry` (`tools.rs`).
//!
//! Dieses Modul enthält den Domänenkern, den persistenten Store, Scheduler,
//! Recovery, die Work-Tools sowie den Executor/Runner (Schritte 1–5 des
//! Plans): reine Datenstrukturen, eine Ereignis-Projektion, ein Journal, das
//! einen Prozessabsturz überlebt, und die Worker-Schleife, die einen Lauf
//! bis zum Abschluss (oder Abbruch/Budget/Blockade) abarbeitet. Die CLI
//! folgt in einem späteren Schritt.

pub mod cli;
pub mod error;
pub mod event;
pub mod executor;
pub mod model;
pub mod recovery;
pub mod runner;
pub mod scheduler;
pub mod state;
pub mod store;
pub mod tools;

pub use cli::{dispatch, dispatch_with_io, WorkCliDeps};
pub use error::WorkError;
pub use event::WorkEvent;
pub use executor::{AgentExecutor, AgentWorkPackage, CodingAgentExecutor};
pub use model::{
    id_order, now_ms, slug, ArtifactId, ArtifactKind, AttemptId, AttemptStatus, CompletionReason,
    FailureInfo, FailureKind, ProjectId, ProjectStatus, RunId, RunStatus, WorkArtifact,
    WorkAttempt, WorkBudget, WorkItem, WorkItemId, WorkItemKind, WorkItemStatus, WorkLease,
    WorkProject, WorkRun,
};
pub use recovery::{recover, recover_all, RecoveryReport};
pub use runner::{ensure_plan_item, run_to_completion, RunOutcome, RunnerConfig, WorkProgress};
pub use scheduler::{decide, Decision};
pub use state::WorkState;
pub use store::{WorkStore, JOURNAL_FILE};
pub use tools::{register_work_tools, CriterionCheck, WorkSubmission, WorkToolCtx, WORK_SYSTEM};
