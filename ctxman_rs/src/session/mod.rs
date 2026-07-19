//! In-Process-Orchestrierung (Ersatz der C#-API-Schicht ohne HTTP/EF): [`ContextSession`]
//! besitzt Session + Segmente + Frames und bietet die Operationen Append, Render, Page Fault,
//! Frames, Epoch-Bump und GC als synchrone Methoden an. [`CtxmanStore`] ist ein dünner
//! Multi-Session-Wrapper.

mod append;
mod frames;
mod gc;
mod refs;
mod render;

pub use append::{AppendContent, AppendOutcome, AppendRequest};
pub use frames::PopOutcome;
pub use gc::{MajorGcReport, MinorGcReport};
pub use refs::ExpandOutcome;
pub use render::{RenderOptions, RenderOutput};

use std::collections::HashMap;

use serde_json::Value;
use ulid::Ulid;

use crate::compaction::CompactionModel;
use crate::domain::{Frame, FrameStatus, PolicyConfig, Segment, Session};
use crate::error::CtxmanError;
use crate::events::{Event, EventSink};
use crate::promotion::PromotionSink;
use crate::storage::{BlobStore, InMemoryBlobStore};
use crate::tokenization::{HeuristicTokenCounter, TokenCounter};

/// Injizierbare Dienste einer [`ContextSession`] (Ersatz der DI-Registrierung in `Program.cs`).
/// ctxman ruft nie selbst das LLM des Agents auf (Spec Non-Goal N1): [`CompactionModel`] und
/// [`PromotionSink`] werden vom Host implementiert; ohne Konfiguration schlägt `run_major_gc`
/// mit einem typisierten Fehler fehl.
pub struct CtxmanServices {
    pub blob_store: Box<dyn BlobStore>,
    pub token_counter: Box<dyn TokenCounter>,
    pub compaction_model: Option<Box<dyn CompactionModel>>,
    pub promotion_sink: Option<Box<dyn PromotionSink>>,
    pub event_sink: Option<Box<dyn EventSink>>,
    /// Unix-Millis-Uhr; Tests injizieren `|| 0` für Determinismus.
    pub clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for CtxmanServices {
    fn default() -> Self {
        CtxmanServices {
            blob_store: Box::new(InMemoryBlobStore::new()),
            token_counter: Box::new(HeuristicTokenCounter),
            compaction_model: None,
            promotion_sink: None,
            event_sink: None,
            clock: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
        }
    }
}

/// Der Context eines Agent-Laufs samt Operationen (Spec §2.1/§4). Besitzt Session, Segmente
/// und Frames exklusiv (`&mut`-Disziplin ersetzt die optimistic concurrency der DB — die
/// `context_version` bleibt als beobachtbare Monotonie-Garantie erhalten, Spec §4.4).
pub struct ContextSession {
    pub(crate) session: Session,
    pub(crate) segments: Vec<Segment>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) next_seq: i64,
    pub(crate) next_event_seq: i64,
    pub(crate) event_log: Vec<Event>,
    pub(crate) services: CtxmanServices,
}

impl ContextSession {
    /// Neue aktive Session mit eingefrorener Policy (Spec §2.1/§5).
    pub fn new(policy: PolicyConfig, services: CtxmanServices) -> Self {
        let now = (services.clock)();
        ContextSession {
            session: Session::new(Ulid::new(), None, policy, now),
            segments: Vec::new(),
            frames: Vec::new(),
            next_seq: 0,
            next_event_seq: 0,
            event_log: Vec::new(),
            services,
        }
    }

    // ---- Read-only-Sicht ----

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Holt alle seit dem letzten Aufruf angefallenen Events ab (Spec §6; Ersatz des
    /// Outbox-Patterns; `after_seq`-Cursor ist die `seq` des letzten gesehenen Events).
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.event_log)
    }

    // ---- Pin / Unpin (Spec §4.3) ----

    /// Pinnt ein Segment (Spec §4.3). Static-Segment ⇒ Fehler (I1).
    pub fn pin(&mut self, segment_id: Ulid) -> Result<(), CtxmanError> {
        self.segment_mut(segment_id)?.pin()
    }

    /// Entfernt den Pin (Spec §4.3). Static-Segment ⇒ Fehler (I1).
    pub fn unpin(&mut self, segment_id: Ulid) -> Result<(), CtxmanError> {
        self.segment_mut(segment_id)?.unpin()
    }

    /// Archiviert die Session (Spec §4.3).
    pub fn archive(&mut self) {
        let now = (self.services.clock)();
        self.session.archive(now);
    }

    // ---- Interne Helfer ----

    pub(crate) fn segment_mut(&mut self, id: Ulid) -> Result<&mut Segment, CtxmanError> {
        self.segments
            .iter_mut()
            .find(|s| s.id() == id)
            .ok_or(CtxmanError::SegmentNotFound { id })
    }

    pub(crate) fn segment(&self, id: Ulid) -> Result<&Segment, CtxmanError> {
        self.segments
            .iter()
            .find(|s| s.id() == id)
            .ok_or(CtxmanError::SegmentNotFound { id })
    }

    /// IDs aller offenen Frames (Spec §2.5).
    pub(crate) fn open_frame_ids(&self) -> Vec<Ulid> {
        self.frames
            .iter()
            .filter(|f| f.status() == FrameStatus::Open)
            .map(|f| f.id())
            .collect()
    }

    /// Der Stack-Tip (Spec §2.5): der offene Frame, der nicht als `parent_frame_id` eines
    /// anderen offenen Frames referenziert wird. Wegen LIFO-Disziplin eindeutig; `None` =
    /// Root-Level. (Exakter Port von `FrameEndpoints.FindTipFrame`.)
    pub(crate) fn tip_frame_id(&self) -> Option<Ulid> {
        let open: Vec<&Frame> = self
            .frames
            .iter()
            .filter(|f| f.status() == FrameStatus::Open)
            .collect();
        if open.is_empty() {
            return None;
        }

        let referenced_as_parent: std::collections::HashSet<Ulid> =
            open.iter().filter_map(|f| f.parent_frame_id()).collect();

        open.iter()
            .find(|f| !referenced_as_parent.contains(&f.id()))
            .map(|f| f.id())
    }

    /// Hängt ein Event an Log und optionalen Sink an (Spec §6); `seq` pro Session monoton.
    pub(crate) fn record_event(&mut self, event_type: &'static str, payload: Value, now: i64) {
        let event = Event {
            id: Ulid::new(),
            session_id: self.session.id(),
            event_type,
            payload,
            seq: self.next_event_seq,
            created_at: now,
        };
        self.next_event_seq += 1;
        if let Some(sink) = &self.services.event_sink {
            sink.emit(&event);
        }
        self.event_log.push(event);
    }
}

/// Dünner Multi-Session-Wrapper: Sessions per ULID adressierbar (Ersatz für `POST /v1/sessions`
/// + Lookup). Die Dienste werden pro Session über eine Factory erzeugt.
pub struct CtxmanStore {
    services_factory: Box<dyn Fn() -> CtxmanServices + Send>,
    sessions: HashMap<Ulid, ContextSession>,
}

impl CtxmanStore {
    pub fn new(services_factory: impl Fn() -> CtxmanServices + Send + 'static) -> Self {
        CtxmanStore { services_factory: Box::new(services_factory), sessions: HashMap::new() }
    }

    pub fn create_session(&mut self, policy: PolicyConfig) -> Ulid {
        let session = ContextSession::new(policy, (self.services_factory)());
        let id = session.session().id();
        self.sessions.insert(id, session);
        id
    }

    pub fn session_mut(&mut self, id: Ulid) -> Option<&mut ContextSession> {
        self.sessions.get_mut(&id)
    }

    pub fn session(&self, id: Ulid) -> Option<&ContextSession> {
        self.sessions.get(&id)
    }

    pub fn remove(&mut self, id: Ulid) -> Option<ContextSession> {
        self.sessions.remove(&id)
    }

    pub fn session_ids(&self) -> Vec<Ulid> {
        let mut ids: Vec<Ulid> = self.sessions.keys().copied().collect();
        ids.sort();
        ids
    }
}
