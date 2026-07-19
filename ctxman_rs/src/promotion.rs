//! Write-only Senke für extrahierte Fakten aus der Segment-Promotion (Spec §3.3, Non-Goal N2).
//! ctxman schreibt Fakten ausschließlich; ein Rücklesen ist strukturell ausgeschlossen — das
//! Trait hat bewusst KEINE Lese- oder Abfrage-Methode.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::CtxmanError;

/// Ein dauerhafter Fakt, der durch die Major Collection (oder einen Frame-Pop) extrahiert
/// wurde und an die konfigurierte Promotion-Senke geschrieben wird (Spec §3.3).
/// Die Feld-Reihenfolge entspricht dem C#-Original — sie fließt in den `payload_digest`
/// des `fact_promoted`-Events ein (Serialisierung in Deklarationsreihenfolge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotedFact {
    pub fact: String,
    pub source_session: String,
    pub source_turn: u32,
    pub kind: String,
}

/// Synchroner Port von `IPromotionSink` (Spec §3.3; Non-Goal N2: write-only).
pub trait PromotionSink: Send + Sync {
    fn write(&self, fact: &PromotedFact, sink_url: &str) -> Result<(), CtxmanError>;
}

/// Test-/Debug-Senke: sammelt Fakten in einem Vec (Gegenstück zu `RecordingPromotionSink`).
#[derive(Default)]
pub struct VecPromotionSink {
    facts: Mutex<Vec<PromotedFact>>,
}

impl VecPromotionSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn facts(&self) -> Vec<PromotedFact> {
        self.facts.lock().expect("Promotion-Mutex nicht poisoned").clone()
    }
}

impl PromotionSink for VecPromotionSink {
    fn write(&self, fact: &PromotedFact, _sink_url: &str) -> Result<(), CtxmanError> {
        self.facts
            .lock()
            .expect("Promotion-Mutex nicht poisoned")
            .push(fact.clone());
        Ok(())
    }
}
