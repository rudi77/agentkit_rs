//! Events und Observability (Spec §6): jede GC-Operation und jede Mutation emittiert ein
//! Event. Im Rust-Port ersetzt ein internes, per [`crate::session::ContextSession::drain_events`]
//! abholbares Log das Outbox-Pattern; zusätzlich kann ein [`EventSink`] synchron mithören.
//!
//! Der Puffer allein macht die Spec-Zusage G6 („Warum wusste der Agent X in Turn 30 nicht
//! mehr?") noch nicht ein: `drain_events` entnimmt, was es liefert. Wer den Verlauf behalten
//! will, hängt eine dauerhafte Senke ein — [`JsonlEventSink`] schreibt den Strom append-only
//! in eine Datei und ist damit das Bibliotheks-Äquivalent der Outbox-Tabelle des C#-Originals.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;
use ulid::Ulid;

use crate::error::CtxmanError;

/// Event-Typen aus Spec §6 als Konstanten.
pub mod types {
    pub const SEGMENT_APPENDED: &str = "segment_appended";
    pub const SEGMENT_EXTERNALIZED: &str = "segment_externalized";
    pub const SEGMENT_EVICTED: &str = "segment_evicted";
    pub const UNIT_EVICTED: &str = "unit_evicted";
    pub const COMPACTION_STARTED: &str = "compaction_started";
    pub const COMPACTION_COMPLETED: &str = "compaction_completed";
    pub const FACT_PROMOTED: &str = "fact_promoted";
    pub const FRAME_PUSHED: &str = "frame_pushed";
    pub const FRAME_POPPED: &str = "frame_popped";
    pub const REF_EXPANDED: &str = "ref_expanded";
    pub const STATIC_EPOCH_BUMPED: &str = "static_epoch_bumped";
    pub const WATERMARK_CROSSED: &str = "watermark_crossed";
    pub const RENDER_SERVED: &str = "render_served";
    /// Spec §4.3: Session archiviert (nach der terminalen Promotion).
    pub const SESSION_ARCHIVED: &str = "session_archived";
}

/// Ein Ereignis der Session (Spec §6). `seq` ist pro Session monoton (Outbox-Cursor);
/// `payload` ist snake_case-JSON wie im C#-Original.
///
/// `Serialize` (ohne `Deserialize`): `event_type` ist ein `&'static str` aus [`types`] —
/// das schreibt sich, liest sich aber nicht ohne Allokation zurück. Wer den Strom wieder
/// einliest, tut das als `serde_json::Value`; ctxman selbst liest ihn nie (die Session ist
/// die Source of Truth, der Event-Strom ist Audit-Trail).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub id: Ulid,
    pub session_id: Ulid,
    pub event_type: &'static str,
    pub payload: Value,
    pub seq: i64,
    pub created_at: i64,
}

/// Optionaler synchroner Mithörer (zusätzlich zum internen Log) — z. B. für Logging oder
/// Metriken des Hosts. Wird an den Commit-Punkten der Operationen aufgerufen.
///
/// Bewusst ohne `Result`: `emit` läuft ausschließlich in den Abschnitten, ab denen eine
/// Operation nur noch unfehlbare Mutationen ausführt (Ersatz der atomaren DB-Transaktion).
/// Eine Senke, die scheitern kann, meldet das auf ihrem eigenen Weg — sie darf eine
/// laufende Session nie abbrechen.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event);
}

/// Dauerhafte Datei-Senke (Spec §6, Ersatz der append-only Outbox-Tabelle): hängt jedes
/// Event als eine Zeile JSON an. Die Datei wird einmal im Append-Modus geöffnet und
/// gehalten — ein `Mutex` serialisiert die Schreibzugriffe, damit Zeilen nie ineinander
/// laufen.
///
/// Jede Zeile wird sofort geflusht: der Wert einer Audit-Spur liegt darin, dass sie auch
/// nach einem Absturz vollständig ist. Schreibfehler werden **verschluckt** (siehe
/// [`EventSink`]) — die Senke zählt sie in [`JsonlEventSink::write_errors`], damit ein
/// stummes Scheitern wenigstens sichtbar bleibt.
pub struct JsonlEventSink {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    write_errors: std::sync::atomic::AtomicU64,
}

impl JsonlEventSink {
    /// Öffnet (oder erzeugt) die Datei im Append-Modus. Ein vorhandener Strom wird
    /// fortgeschrieben, nie überschrieben — über einen Snapshot-Neustart hinweg bleibt die
    /// `seq`-Monotonie erhalten, weil `next_event_seq` Teil des Snapshots ist.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, CtxmanError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(JsonlEventSink {
            path,
            file: Mutex::new(file),
            write_errors: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Pfad des Event-Logs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Anzahl der Events, die nicht geschrieben werden konnten (Platte voll, Datei
    /// entfernt, …). > 0 heißt: die Audit-Spur hat Lücken.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn note_error(&self) {
        self.write_errors
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl EventSink for JsonlEventSink {
    fn emit(&self, event: &Event) {
        let Ok(mut line) = serde_json::to_string(event) else {
            self.note_error();
            return;
        };
        line.push('\n');

        // Ein vergifteter Mutex bedeutet: ein anderer Thread ist beim Schreiben gepanickt.
        // Für eine Audit-Spur ist Weiterschreiben besser als Mitpanicken.
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(poisoned) => poisoned.into_inner(),
        };
        if file
            .write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .is_err()
        {
            self.note_error();
        }
    }
}

impl std::fmt::Debug for JsonlEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlEventSink")
            .field("path", &self.path)
            .field("write_errors", &self.write_errors())
            .finish()
    }
}
