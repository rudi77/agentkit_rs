//! agentkit-viz — ein Beobachtungs-Werkzeug für agentkit-Läufe.
//!
//! Ein Lauf ist sonst eine Blackbox: was im Kontext eines Agenten steht, was
//! Schwarm-Mitglieder sich zugerufen haben, warum ein Work Item scheiterte,
//! sieht man nur an stderr-Zeilen, die nach dem Lauf weg sind. Dieses Crate
//! liest den NDJSON-Trace (`agentkit --trace DIR`, siehe `agentkit::trace`) und
//! zeigt ihn im Browser — live während eines Laufs und nachträglich aus der
//! Datei, über denselben Codepfad.
//!
//! Es ist ausdrücklich ein **Debug-Werkzeug, kein Produkt**: localhost, keine
//! Auth über das Loopback-Token hinaus, kein Multi-User, kein Schreibzugriff.
//!
//! ```no_run
//! use agentkit_viz::{VizConfig, VizServer};
//!
//! let server = VizServer::bind(VizConfig {
//!     trace_dir: ".agentkit/trace".into(),
//!     trace_file: None,
//!     work_root: None,
//!     port: 7878,
//! })
//! .unwrap();
//! println!("{}", server.url());
//! server.run();
//! ```
//!
//! Aufbau (hexagonal): [`model`] und [`project`] sind die Domäne — sie kennen
//! weder HTTP noch Dateisystem. [`trace`] ist der Adapter zum Dateisystem,
//! [`server`] der zum Browser, [`api`] die reinen Endpunkt-Funktionen dazwischen.

pub mod api;
pub mod model;
pub mod project;
pub mod server;
pub mod swarm;
pub mod trace;

pub use api::{ApiCtx, ApiError};
pub use model::{agent_label, AgentKind, PlanStep, TraceData, TraceEvent, TraceLine};
pub use project::{AgentView, ContextView, TimelineEntry, TraceState, CONTEXT_SNAPSHOT_KIND};
pub use server::{default_trace_dir, default_work_root, open_browser, VizConfig, VizServer};
pub use swarm::{DeadLetterView, ProposalView, SwarmMessageView, SwarmView};
pub use trace::{latest_trace, list_traces, TraceError, TraceFileInfo, TraceReader};
