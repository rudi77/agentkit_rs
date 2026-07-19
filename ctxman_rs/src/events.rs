//! Events und Observability (Spec §6): jede GC-Operation und jede Mutation emittiert ein
//! Event. Im Rust-Port ersetzt ein internes, per [`crate::session::ContextSession::drain_events`]
//! abholbares Log das Outbox-Pattern; zusätzlich kann ein [`EventSink`] synchron mithören.

use serde_json::Value;
use ulid::Ulid;

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
    pub const BLOB_SWEPT: &str = "blob_swept";
    pub const WATERMARK_CROSSED: &str = "watermark_crossed";
    pub const RENDER_SERVED: &str = "render_served";
}

/// Ein Ereignis der Session (Spec §6). `seq` ist pro Session monoton (Outbox-Cursor);
/// `payload` ist snake_case-JSON wie im C#-Original.
#[derive(Debug, Clone, PartialEq)]
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
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event);
}
