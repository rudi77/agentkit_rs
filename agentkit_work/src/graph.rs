//! Der Graph-Port (§11/§25 des Konzepts, Phase 4/5b).
//!
//! `agentkit_work` kennt `agentkit_graph` NICHT — die Abhängigkeitsrichtung im
//! Repo ist einbahnig (siehe `CLAUDE.md`): beide Crates sind Geschwister über
//! `agentkit`, nie voneinander abhängig. Die Verbindung läuft deshalb über
//! einen PORT hier plus einen ADAPTER in `agentkit_app` (das einzige Crate,
//! das beide Bibliotheken kennt). [`GraphGateway`] hat damit zwei echte
//! Implementierungen — den Adapter und `FakeGraph` in den Tests — und erfüllt
//! Guidelines §2 (kein Trait mit nur einer Implementierung „für später").
//!
//! `promote` (Phase 5b, §11/§26 Phase 7) hat seinen Aufrufer jetzt: ein Item,
//! das eine echte Verifikation durchlaufen hat (`VerificationPolicy != None`),
//! promotet nach `Completed` die Claim-IDs all seiner Versuche — siehe
//! [`promote_after_completion`].

use std::sync::Arc;

use crate::event::WorkEvent;
use crate::model::{AttemptId, ProjectId, RunId, VerificationPolicy, WorkItemId};
use crate::store::WorkStore;

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

    /// Übernimmt vorläufige Claims aus dem Working Graph ins dauerhafte
    /// Wissen (Canonical Graph), nach bestandener Verifikation eines Work
    /// Items (§11, Phase 5b). Gibt die Anzahl tatsächlich promoteter Claims
    /// zurück; `Err` ist ein weicher Fehler — der Aufrufer meldet ihn als
    /// Warnung, das Work Item bleibt trotzdem `Completed` (siehe
    /// [`promote_after_completion`]).
    fn promote(&self, claim_ids: &[String]) -> Result<usize, String>;
}

/// Promotet die Claim-IDs ALLER Versuche eines Items über `gateway`, wenn das
/// Item wirklich eine Verifikation durchlaufen hat (`policy != None`) — bei
/// `VerificationPolicy::None` gab es nie eine Prüfung, die etwas zu
/// promotieren rechtfertigt, und ohne angebundenen Graphen (`gateway ==
/// None`) gibt es nichts zu tun. Journalt `WorkEvent::ClaimsPromoted` NUR bei
/// Erfolg — ein Fehlschlag journalt nichts, damit ein späterer Resume
/// (`recovery::recover_pending_promotions`) automatisch einen neuen Versuch
/// bekommt, statt für immer unpromotet zu bleiben.
///
/// Gibt bei einem Fehlschlag eine Meldung zurück, die der Aufrufer als
/// Warnung anzeigt (Runner: `WorkProgress::Note`; CLI: stderr) — dieselbe
/// Haltung wie `GraphAgent::record_episode` in agentkit_graph: ein nicht
/// schreibbarer oder nicht erreichbarer Graph darf bereits abgeschlossene
/// Arbeit nicht entwerten, der Lauf bricht deshalb NICHT ab.
pub(crate) fn promote_after_completion(
    store: &WorkStore,
    gateway: Option<&Arc<dyn GraphGateway>>,
    item_id: &str,
    policy: &VerificationPolicy,
    at_ms: u64,
) -> Option<String> {
    if matches!(policy, VerificationPolicy::None) {
        return None;
    }
    let gateway = gateway?;

    let claim_ids: Vec<String> = store
        .snapshot()
        .attempts
        .values()
        .filter(|a| a.work_item_id == item_id)
        .flat_map(|a| a.claim_ids.iter().cloned())
        .collect();
    if claim_ids.is_empty() {
        // Nichts festgehalten — trivial nichts zu promotieren, kein
        // Gateway-Aufruf, kein Ereignis nötig.
        return None;
    }

    match gateway.promote(&claim_ids) {
        Ok(_) => {
            if let Err(e) = store.submit(WorkEvent::ClaimsPromoted {
                item: item_id.to_string(),
                claim_ids,
                at_ms,
            }) {
                return Some(format!(
                    "Item '{item_id}': Claims promotet, aber 'ClaimsPromoted' konnte nicht \
                     journalt werden ({e}) — ein späterer Resume versucht es erneut."
                ));
            }
            None
        }
        Err(e) => Some(format!("Item '{item_id}': Claims nicht promotet — {e}")),
    }
}
