//! Adapter: `agentkit_work::GraphGateway` über `agentkit_graph`.
//!
//! `agentkit_work` kennt `agentkit_graph` nicht (CLAUDE.md, Einbahnrichtung) —
//! dieser Adapter lebt deshalb hier, im einzigen Crate, das beide Bibliotheken
//! kennt (§25 des `agentkit-work`-Konzepts). Eigenes Modul statt in `lib.rs`
//! mitgeführt: die Verdrahtung von Schwarm und Graph in `lib.rs` betrifft den
//! NORMALEN Coding-Agenten, dieser Adapter ein eigenständiges Thema (Work
//! Items) mit eigenem Provenance-Kontrakt — ein Modul, ein Thema (Guidelines
//! §3).
//!
//! # Provenance-Kodierung
//!
//! [`agentkit_work::WorkProvenance`] trägt mehr Felder, als
//! [`agentkit_graph::SourceDraft`] hat. Statt etwas zu improvisieren, werden
//! ausschließlich VORHANDENE `GraphSource`-Felder wiederverwendet:
//!
//! - `run_id` (über [`GraphAccess::with_run_id`]) trägt `"work:<project_id>/<run_id>"`
//!   — Projekt UND Lauf in einem String; das Präfix `work:` unterscheidet ihn
//!   von Session-/Schwarm-Läufen anderer Quellen im selben Feld.
//! - `tool_call_id` trägt `"<work_item_id>/<attempt_id>"` — dieselbe Rolle wie
//!   bei einer normalen Aussage (die konkrete Handlung, die den Claim erzeugt
//!   hat), hier eben ein Work-Item-Versuch statt eines Tool-Aufrufs.
//! - `artifact_uri` trägt den ERSTEN bekannten Artefaktpfad dieses Versuchs
//!   (falls vorhanden).
//! - `excerpt` trägt die Repository-Revision (falls bekannt) plus die
//!   Belegstelle des Modells, klar durch ein Präfix getrennt.
//! - `agent_id`/`created_by` kommen automatisch aus `access.principal`
//!   (gesetzt über [`GraphAccess::as_principal`] mit `prov.agent_id`) — der
//!   Autor ist der WIRKLICHE Work-Agent, nicht "work" oder der Runtime-Prozess.

use std::sync::Arc;

use agentkit_graph::{
    retrieval, ClaimDraft, GraphAccess, GraphQuery, GraphStore, GraphWriteCommand, SourceDraft,
};
use agentkit_work::{ClaimText, GraphGateway, WorkProvenance};

/// Verbindet einen offenen Wissensgraphen mit der Work-Runtime.
pub struct WorkGraphAdapter {
    pub store: Arc<GraphStore>,
    /// Zugang MIT Schreibziel (sonst schlägt `record_claims` fehl — weicher
    /// Fehler aus Sicht des Modells, siehe `GraphGateway`-Vertrag). Respektiert
    /// `--graph-readonly` unverändert: ein Nur-Lese-Zugriff hat kein
    /// Schreibziel, `record_claims` liefert dann `Err` statt zu schreiben.
    pub access: GraphAccess,
}

impl GraphGateway for WorkGraphAdapter {
    fn recall(&self, query: &str) -> Option<String> {
        let index = self.store.snapshot();
        let sub = retrieval::search(&index, &self.access.view, &GraphQuery::text(query));
        if sub.is_empty() {
            None
        } else {
            Some(retrieval::render(&index, &sub, true))
        }
    }

    fn record_claims(
        &self,
        prov: &WorkProvenance,
        claims: &[ClaimText],
    ) -> Result<Vec<String>, String> {
        // Siehe Moduldoku für die Kodierung von `run_id`/`tool_call_id`.
        let access = self
            .access
            .as_principal(&prov.agent_id)
            .with_run_id(&format!("work:{}/{}", prov.project_id, prov.run_id));

        let mut ids = Vec::with_capacity(claims.len());
        for claim in claims {
            let mut source = SourceDraft::new("work_item")
                .tool_call(&format!("{}/{}", prov.work_item_id, prov.attempt_id));
            if let Some(path) = prov.artifact_paths.first() {
                source = source.artifact(path);
            }
            if let Some(excerpt) = combined_excerpt(prov, claim) {
                source = source.excerpt(&excerpt);
            }

            let draft = ClaimDraft::new(&claim.subject, &claim.predicate, &claim.object, source)
                .confidence(claim.confidence);
            match self
                .store
                .submit(GraphWriteCommand::RecordClaim(draft), &access)
            {
                Ok(receipt) => ids.push(receipt.claim_id.unwrap_or_default()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(ids)
    }
}

/// Baut den `excerpt`-Text aus Repository-Revision (falls bekannt) und der
/// Belegstelle des Modells — beides passt in kein anderes `GraphSource`-Feld
/// (siehe Moduldoku), `excerpt` ist laut `SourceDraft`-Doku genau dafür da.
fn combined_excerpt(prov: &WorkProvenance, claim: &ClaimText) -> Option<String> {
    match (&prov.repository_revision, &claim.excerpt) {
        (Some(rev), Some(text)) => Some(format!("[rev {rev}] {text}")),
        (Some(rev), None) => Some(format!("[rev {rev}]")),
        (None, Some(text)) => Some(text.clone()),
        (None, None) => None,
    }
}
