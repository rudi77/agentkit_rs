//! In-Process-Orchestrierung (Ersatz der C#-API-Schicht ohne HTTP/EF): [`ContextSession`]
//! besitzt Session + Segmente + Frames und bietet die Operationen Append, Render, Page Fault,
//! Frames, Epoch-Bump und GC als synchrone Methoden an. [`CtxmanStore`] ist ein dünner
//! Multi-Session-Wrapper.

mod append;
mod epoch;
mod frames;
mod gc;
mod refs;
mod render;

pub use append::{AppendContent, AppendOutcome, AppendRequest};
pub use epoch::EpochDiffOutcome;
pub use frames::PopOutcome;
pub use gc::{MajorGcReport, MinorGcReport};
pub use refs::ExpandOutcome;
pub use render::{RenderOptions, RenderOutput};

use std::collections::HashMap;

use serde_json::{json, Value};
use ulid::Ulid;

use crate::compaction::{
    CompactionModel, CompactionRequest, WindowItem, FACT_EXTRACTION_TEMPLATE_ID,
};
use crate::domain::{
    Frame, FrameStatus, PolicyConfig, Region, Segment, SegmentState, Session, SessionStatus,
};
use crate::error::CtxmanError;
use crate::events::{types, Event, EventSink};
use crate::promotion::{PromotedFact, PromotionSink};
use crate::rendering::canonical_json;
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

/// Ergebnis einer Archivierung (Spec §4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveOutcome {
    /// `true`, wenn die terminale Promotion einen Fakt in die Senke geschrieben hat.
    pub fact_promoted: bool,
    pub context_version: u64,
}

/// Eine bereits ausgeführte Fact-Promotion, deren Event noch nicht geschrieben ist (Port des
/// `PendingPromotionEvent` aus dem C#-Original). Modell-Aufruf und Sink-Write sind erfolgt;
/// der Aufrufer entscheidet, an welcher Stelle seiner Event-Reihenfolge das `fact_promoted`
/// steht — laut Spec §6 immer VOR dem Event der auslösenden Operation.
pub(crate) struct PendingPromotion {
    pub segment_id: Ulid,
    pub sink: String,
    pub digest: String,
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

    /// Holt alle seit dem letzten Aufruf angefallenen Events ab und **leert** den Puffer
    /// (Spec §6; Ersatz des Outbox-Patterns).
    ///
    /// Wer den Verlauf über den Puffer hinaus behalten will, hängt einen
    /// [`EventSink`](crate::events::EventSink) ein — etwa
    /// [`JsonlEventSink`](crate::events::JsonlEventSink); nur der macht die
    /// Auditierbarkeits-Zusage der Spec (G6) einlösbar. Für einen Blick ohne Entnahme
    /// gibt es [`ContextSession::events`] und [`ContextSession::events_after`].
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.event_log)
    }

    /// Sicht auf den Event-Puffer **ohne** ihn zu leeren (Spec §6).
    pub fn events(&self) -> &[Event] {
        &self.event_log
    }

    /// Events mit `seq > after_seq` (Outbox-Cursor der Spec, §4.3
    /// `GET /events?after_seq=…`), ohne den Puffer zu leeren. `seq` ist pro Session
    /// monoton, der Puffer also aufsteigend sortiert — die Grenze steht per Binärsuche.
    pub fn events_after(&self, after_seq: i64) -> &[Event] {
        let start = self.event_log.partition_point(|e| e.seq <= after_seq);
        &self.event_log[start..]
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

    /// Archiviert die Session (Spec §4.3): **terminale Promotion** über alle verbliebenen
    /// Working-Segmente, danach Status → archived und Versions-Increment.
    ///
    /// Die Promotion läuft VOR jeder Mutation. Schlägt sie fehl, wird NICHT archiviert und
    /// der Fehler propagiert — dieselbe Entscheidung wie im C#-Original, das dafür einen
    /// retrybaren `503 promotion_failed` liefert. Der Grund ist dieselbe Invariante wie bei
    /// Frame-Pop und Major GC: Promotion geht der zerstörenden Operation voraus. Das
    /// Sitzungsende ist die letzte Gelegenheit für Fakten aus Segmenten, die es nie in ein
    /// Compaction-Fenster geschafft haben — also gerade für den jüngsten Teil der Sitzung.
    /// Ein Retry ist sicher, weil noch nichts verändert wurde.
    ///
    /// Idempotent: eine bereits archivierte Session ruft das Modell nicht erneut auf (im
    /// Service verhindern das die Idempotency-Keys, §4.4).
    ///
    /// Ohne konfiguriertes [`CompactionModel`] wird die Promotion übersprungen — dieselbe
    /// Bibliotheks-Divergenz wie beim Frame-Pop.
    pub fn archive(&mut self) -> Result<ArchiveOutcome, CtxmanError> {
        if self.session.status() == SessionStatus::Archived {
            return Ok(ArchiveOutcome {
                fact_promoted: false,
                context_version: self.session.context_version(),
            });
        }

        // Spec §3.3 / §4.3: Promotion-Fenster = alle noch render-fähigen Working-Segmente,
        // älteste zuerst (Port von `SessionEndpoints.ArchiveSessionAsync`). Gepinnte sind
        // bewusst NICHT ausgenommen: `task`/`decision` sind Promotion-Kandidaten (§2.3) und
        // werden gerade deshalb nie kompaktiert — ohne diesen Lauf gingen sie verloren.
        let mut candidates: Vec<&Segment> = self
            .segments
            .iter()
            .filter(|s| {
                s.region() == Region::Working
                    && matches!(s.state(), SegmentState::Live | SegmentState::Externalized)
            })
            .collect();
        candidates.sort_by_key(|s| s.seq());
        let window_ids: Vec<Ulid> = candidates.iter().map(|s| s.id()).collect();

        let pending = self.extract_and_sink_facts(&window_ids)?;

        // Ab hier nur noch unfehlbare Mutationen (Ersatz der atomaren DB-Transaktion).
        let now = (self.services.clock)();
        let fact_promoted = pending.is_some();

        // Spec §6: fact_promoted steht VOR dem Event der auslösenden Operation.
        if let Some(pending) = pending {
            self.record_promotion_event(pending, now);
        }

        self.session.archive(now);
        // Spec §4.4: context_version genau EINMAL pro Aufruf erhöhen.
        self.session.increment_version(now);

        self.record_event(
            types::SESSION_ARCHIVED,
            json!({
                "context_version": self.session.context_version(),
                "fact_promoted": fact_promoted,
            }),
            now,
        );

        Ok(ArchiveOutcome {
            fact_promoted,
            context_version: self.session.context_version(),
        })
    }

    // ---- Fact-Promotion (Spec §3.3 Schritt 1) ----

    /// Baut das Fenster für die Modell-Aufrufe (Spec §3.3). Externalisierte Segmente
    /// steuern ihre `summary` bei — ihr Inhalt liegt im Blob Store, nicht im Segment.
    pub(crate) fn window_items(&self, ids: &[Ulid]) -> Vec<WindowItem> {
        ids.iter()
            .filter_map(|id| self.segments.iter().find(|s| s.id() == *id))
            .map(|s| WindowItem {
                content: s.content().or(s.summary()).unwrap_or_default().to_string(),
                kind: Some(s.kind().to_string()),
            })
            .collect()
    }

    /// Extrahiert dauerhafte Fakten aus dem Fenster und schreibt sie in die Senke
    /// (Spec §3.3 Schritt 1; Port von `PromotionService.ExtractAndSinkAsync`). Gemeinsame
    /// Grundlage der drei Auslöser: Frame-Pop (§2.5), Major GC (§3.3) und Archivierung
    /// (§4.3) — sie unterscheiden sich nur im Fenster und im Zeitpunkt des Events.
    ///
    /// Läuft VOR jeder Mutation des Aufrufers: schlägt der Modell- oder Sink-Aufruf fehl,
    /// propagiert der Fehler und es wurde nichts verändert — ein Retry ist sicher. Genau
    /// deshalb geht Promotion der zerstörenden Operation voraus (lossless vor lossy).
    ///
    /// `Ok(None)` heißt „nichts zu promoten": leeres Fenster, kein [`CompactionModel`]
    /// konfiguriert (dokumentierte Bibliotheks-Divergenz), oder die Extraktion fand keine
    /// dauerhaften Fakten (leeres Summary — dann läuft der Aufrufer normal weiter, §3.3).
    pub(crate) fn extract_and_sink_facts(
        &self,
        window_ids: &[Ulid],
    ) -> Result<Option<PendingPromotion>, CtxmanError> {
        if window_ids.is_empty() {
            return Ok(None);
        }
        let Some(model) = self.services.compaction_model.as_deref() else {
            return Ok(None);
        };

        let policy = self.session.policy();
        let result = model.summarize(&CompactionRequest {
            window: self.window_items(window_ids),
            prompt_template_id: FACT_EXTRACTION_TEMPLATE_ID.to_string(),
            model: policy.compaction.model.clone(),
        })?;

        if result.summary.is_empty() {
            return Ok(None);
        }

        // Spec §3.3 AC6: Source-Segmente werden durch die Promotion NICHT verändert; das
        // älteste dient nur als Herkunfts-Referenz.
        let oldest = self.segment(window_ids[0])?;
        let fact = PromotedFact {
            fact: result.summary,
            source_session: self.session.id().to_string(),
            source_turn: self.session.current_turn(),
            kind: oldest.kind().to_string(),
        };

        let sink_url = policy.promotion.sink.url.clone().unwrap_or_default();
        let sink = self.services.promotion_sink.as_deref().ok_or_else(|| {
            CtxmanError::Promotion("kein PromotionSink konfiguriert (CtxmanServices)".into())
        })?;
        sink.write(&fact, &sink_url)?;

        // Payload-Digest: SHA-256 über das snake_case-JSON der Sink-Payload in
        // Deklarationsreihenfolge (Audit ohne Inhalt; Mirror des C#-Originals).
        let payload_json = serde_json::to_string(&fact).expect("PromotedFact ist serialisierbar");
        let digest = canonical_json::content_hash(&payload_json);

        Ok(Some(PendingPromotion {
            segment_id: oldest.id(),
            sink: sink_url,
            digest,
        }))
    }

    /// Schreibt das `fact_promoted`-Event einer ausgeführten Promotion (Spec §6).
    pub(crate) fn record_promotion_event(&mut self, pending: PendingPromotion, now: i64) {
        self.record_event(
            types::FACT_PROMOTED,
            json!({
                "segment_id": pending.segment_id.to_string(),
                "sink": pending.sink,
                "payload_digest": pending.digest,
            }),
            now,
        );
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
        CtxmanStore {
            services_factory: Box::new(services_factory),
            sessions: HashMap::new(),
        }
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
