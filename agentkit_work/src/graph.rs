//! Der Graph-Port (§11/§25 des Konzepts, Phase 4).
//!
//! `agentkit_work` kennt `agentkit_graph` NICHT — die Abhängigkeitsrichtung im
//! Repo ist einbahnig (siehe `CLAUDE.md`): beide Crates sind Geschwister über
//! `agentkit`, nie voneinander abhängig. Die Verbindung läuft deshalb über
//! einen PORT hier plus einen ADAPTER in `agentkit_app` (das einzige Crate,
//! das beide Bibliotheken kennt). [`GraphGateway`] hat damit zwei echte
//! Implementierungen — den Adapter und `FakeGraph` in den Tests — und erfüllt
//! Guidelines §2 (kein Trait mit nur einer Implementierung „für später").
//!
//! Bewusst KEINE `promote`-Methode: Promotion nur verifizierter Claims ist
//! Phase 5 und hätte hier noch keinen Aufrufer (Guidelines §4, YAGNI).

use crate::model::{AttemptId, ProjectId, RunId, WorkItemId};

/// Wo eine Aussage in der Arbeit entstanden ist (§11 des Konzepts). Die
/// Laufzeit füllt jedes Feld aus dem laufenden Versuch — dieselbe Regel wie
/// beim übrigen [`crate::tools::WorkToolCtx`]: das Modell liefert nie seine
/// eigene Identität oder Herkunft, nur den Inhalt der Aussage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkProvenance {
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub work_item_id: WorkItemId,
    pub attempt_id: AttemptId,
    pub agent_id: String,
    /// Workspace-relative Artefaktpfade DIESES Versuchs, soweit schon bekannt
    /// (aus dem Store gelesen — siehe `tools::register_work_tools` — nicht
    /// vom Modell angegeben).
    pub artifact_paths: Vec<String>,
    pub repository_revision: Option<String>,
}

/// Eine Aussage, wie der Agent sie formuliert — bewusst OHNE Graph-Typen
/// (kein `ClaimStatus`, kein `GraphTarget`, …), damit dieses Crate
/// `agentkit_graph` nicht kennen muss (siehe Moduldoku).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimText {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
    /// Belegstelle in eigenen Worten (wandert in die Quelle des Claims).
    pub excerpt: Option<String>,
}

/// Zugang zum Wissensgraphen. Port, kein Trait auf Vorrat (Guidelines §2):
/// der Adapter in `agentkit_app` (`#[cfg(all(feature = "work", feature =
/// "graph"))]`) und `FakeGraph` in den Tests sind die zwei realen Nutzer, die
/// diese Abstraktion schon in dieser Phase rechtfertigen.
pub trait GraphGateway: Send + Sync {
    /// Wissens-Auszug für das Arbeitspaket — bereits gerenderter Text oder
    /// `None`, wenn nichts Relevantes vorliegt.
    fn recall(&self, query: &str) -> Option<String>;

    /// Hält Aussagen MIT Work-Provenance fest und gibt ihre Claim-IDs zurück.
    /// `Err` ist ein weicher Fehler aus Sicht des Modells — `tools::work_claim`
    /// übersetzt ihn in `"ERROR: …"`, journalt dabei aber nichts.
    fn record_claims(
        &self,
        prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String>;
}
