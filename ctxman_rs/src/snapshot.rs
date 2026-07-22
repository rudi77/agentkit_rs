//! Snapshot-Persistenz (Ersatz der DB-Persistenz aus Spec §7 für die In-Process-Bibliothek):
//! vollständiger Session-Zustand als JSON-Datei. Blob-Inhalte liegen NICHT im Snapshot —
//! sie sind content-addressed im Blob Store (Spec §2.6) und werden über denselben Store
//! wieder aufgelöst.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::{Frame, Segment, Session};
use crate::error::CtxmanError;
use crate::events::Event;
use crate::session::{ContextSession, CtxmanServices};

/// Serialisierbarer Gesamtzustand einer [`ContextSession`] (ohne Dienste und ohne das
/// Event-Log — Events sind ein Abhol-Puffer, kein Zustand; `next_event_seq` bleibt erhalten,
/// damit die Event-Monotonie (Spec §6) über Snapshots hinweg gilt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session: Session,
    pub segments: Vec<Segment>,
    pub frames: Vec<Frame>,
    pub next_seq: i64,
    pub next_event_seq: i64,
}

impl ContextSession {
    /// Erstellt einen Snapshot des vollständigen Zustands.
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session: self.session.clone(),
            segments: self.segments.clone(),
            frames: self.frames.clone(),
            next_seq: self.next_seq,
            next_event_seq: self.next_event_seq,
        }
    }

    /// Stellt eine Session aus einem Snapshot wieder her. Validiert die Segment-Invariante I2
    /// (Spec §2.2) erneut, da serde die Guard-Konstruktion umgeht.
    pub fn from_snapshot(
        snapshot: SessionSnapshot,
        services: CtxmanServices,
    ) -> Result<Self, CtxmanError> {
        for segment in &snapshot.segments {
            segment.validate_invariants()?; // Spec §2.2 I2
        }
        Ok(ContextSession {
            session: snapshot.session,
            segments: snapshot.segments,
            frames: snapshot.frames,
            next_seq: snapshot.next_seq,
            next_event_seq: snapshot.next_event_seq,
            event_log: Vec::<Event>::new(),
            services,
        })
    }

    /// Schreibt den Snapshot als JSON-Datei (pretty — Diff-freundlich; die I4-Byte-Stabilität
    /// gilt für den Render-Output, nicht für Snapshots).
    pub fn save_to_file(&self, path: &Path) -> Result<(), CtxmanError> {
        let json = serde_json::to_string_pretty(&self.snapshot())
            .map_err(|e| CtxmanError::Snapshot(e.to_string()))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Lädt eine Session aus einer Snapshot-JSON-Datei.
    pub fn load_from_file(path: &Path, services: CtxmanServices) -> Result<Self, CtxmanError> {
        let json = std::fs::read_to_string(path)?;
        let snapshot: SessionSnapshot =
            serde_json::from_str(&json).map_err(|e| CtxmanError::Snapshot(e.to_string()))?;
        ContextSession::from_snapshot(snapshot, services)
    }
}
