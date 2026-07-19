//! Segment-Append (Spec §4.3; Port der Logik aus `SegmentEndpoints.AppendSegmentsAsync`):
//! server-vergebene monotone `seq`, Token-Zählung beim Append, Tip-Frame-Bindung, ein
//! `segment_appended`-Event je Segment, genau EIN Versions-Increment pro Aufruf (auch Batch).

use serde_json::json;
use ulid::Ulid;

use super::ContextSession;
use crate::domain::{Region, Role, Segment, SegmentDraft};
use crate::error::CtxmanError;
use crate::events::types;

/// Spec §2.3: Kinds, deren typische Region Static ist. Append in die Static-Region ist nur
/// über den Epoch-Bump (§4.2) zulässig; ein direkter Append schlägt fehl (I1).
const STATIC_KINDS: [&str; 3] = ["system_prompt", "tool_def", "skill_index"];

/// Spec §4.3: Inline-Content über 1 MiB wird abgelehnt; großer Inhalt läuft über den Blob-Pfad.
const MAX_INLINE_CONTENT_BYTES: usize = 1_048_576;

/// Inhalt eines anzuhängenden Segments: inline ODER als Blob (Spec §4.3, content XOR blob_ref).
#[derive(Debug, Clone)]
pub enum AppendContent {
    /// Regulärer Inline-Content (state = live). Über 1 MiB ⇒ Fehler.
    Inline(String),
    /// Großer Inhalt: ctxman schreibt ihn content-adressiert in den Blob Store und legt das
    /// Segment von Anfang an externalisiert an (Spec §4.3/§2.6). `summary` dient als
    /// Render-Ersatz und Token-Basis (im C#-Original übernimmt `content` diese Doppelrolle).
    Blob { content: Vec<u8>, content_type: String, summary: Option<String> },
}

/// Ein anzuhängendes Working-Segment (Spec §4.3). `refetchable`/`origin` sind gegenüber dem
/// C#-Endpunkt ergänzt — Spec §2.2 führt beide als Segment-Felder (Clean-Page-Eviction §3.2.1
/// braucht sie).
#[derive(Debug, Clone)]
pub struct AppendRequest {
    pub kind: String,
    pub role: Option<Role>,
    pub content: AppendContent,
    pub source: Option<String>,
    pub tool_call_id: Option<String>,
    pub pinned: bool,
    pub refetchable: bool,
    pub origin: Option<String>,
}

impl AppendRequest {
    /// Bequemer Standardfall: Inline-Content, alles Weitere Default.
    pub fn inline(kind: &str, role: Option<Role>, content: &str) -> Self {
        AppendRequest {
            kind: kind.to_string(),
            role,
            content: AppendContent::Inline(content.to_string()),
            source: None,
            tool_call_id: None,
            pinned: false,
            refetchable: false,
            origin: None,
        }
    }
}

/// Ergebnis eines Appends (Spec §4.3): IDs in Eingabe-Reihenfolge + neue `context_version`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendOutcome {
    pub segment_ids: Vec<Ulid>,
    pub context_version: u64,
}

impl ContextSession {
    /// Hängt genau ein Segment an (Single-Form von [`ContextSession::append_segments`]).
    pub fn append_segment(&mut self, request: AppendRequest) -> Result<Ulid, CtxmanError> {
        let outcome = self.append_segments(vec![request])?;
        Ok(outcome.segment_ids[0])
    }

    /// Hängt eine Batch-Liste von Segmenten an (Spec §4.3). Validierung komplett VOR der
    /// ersten Mutation — ein Fehler lässt die Session unverändert (Ersatz der DB-Transaktion).
    pub fn append_segments(
        &mut self,
        requests: Vec<AppendRequest>,
    ) -> Result<AppendOutcome, CtxmanError> {
        // Spec §2.2 I1: Append in die Static-Region ist nur via Epoch-Bump (§4.2) zulässig.
        // Static-Signal: ein Static-Kind (§2.3). Region ist hier konstruktionsbedingt Working.
        for request in &requests {
            if STATIC_KINDS.contains(&request.kind.as_str()) {
                return Err(CtxmanError::StaticAppendForbidden);
            }
            if let AppendContent::Inline(content) = &request.content {
                // Spec §4.3: Inline-Content über 1 MiB ⇒ Fehler, Hinweis auf den Blob-Pfad.
                if content.len() > MAX_INLINE_CONTENT_BYTES {
                    return Err(CtxmanError::ContentTooLarge { bytes: content.len() });
                }
            }
        }

        // Spec §2.5: jedes Working-Segment erbt den Stack-Tip (topmost open frame) oder None.
        let tip_frame_id = self.tip_frame_id();
        let now = (self.services.clock)();
        let current_turn = self.session.current_turn();
        let session_id = self.session.id();

        // I/O (Blob-Puts) VOR der ersten Mutation — schlägt ein Put fehl, bleibt alles unverändert.
        let mut prepared: Vec<Segment> = Vec::with_capacity(requests.len());
        for request in requests {
            let mut draft = SegmentDraft::new(
                Ulid::new(),
                session_id,
                Region::Working,
                &request.kind,
                request.role,
                self.next_seq + prepared.len() as i64, // Spec §2.2: seq server-vergeben, monoton.
                current_turn,
            );
            draft.source = request.source;
            draft.tool_call_id = request.tool_call_id;
            draft.frame_id = tip_frame_id;
            draft.pinned = request.pinned;
            draft.refetchable = request.refetchable;
            draft.origin = request.origin;

            let segment = match request.content {
                AppendContent::Inline(content) => {
                    draft.tokens = self.services.token_counter.count(&content);
                    Segment::create_live(draft, &content)
                }
                AppendContent::Blob { content, content_type, summary } => {
                    // Spec §4.3 / §2.6: Blob-Pfad ⇒ von Anfang an externalisiert; summary als
                    // Render-Ersatz, Tokens aus der summary (Mirror des C#-Verhaltens, wo der
                    // Request-content zur summary wird).
                    let blob_ref = self.services.blob_store.put(&content, &content_type)?;
                    draft.tokens = summary
                        .as_deref()
                        .map(|s| self.services.token_counter.count(s))
                        .unwrap_or(0);
                    draft.summary = summary;
                    Segment::create_externalized(draft, blob_ref)
                }
            };
            prepared.push(segment);
        }

        // Ab hier nur noch unfehlbare Mutationen (Ersatz der atomaren DB-Transaktion, §4.4).
        let mut segment_ids = Vec::with_capacity(prepared.len());
        for segment in prepared {
            segment_ids.push(segment.id());
            self.next_seq = segment.seq() + 1;

            // Spec §6: jede Mutation emittiert ein Event (Outbox, append-only).
            self.record_event(
                types::SEGMENT_APPENDED,
                json!({
                    "segment_id": segment.id().to_string(),
                    "kind": segment.kind(),
                    "region": segment.region().wire_name(),
                    "seq": segment.seq(),
                    "tokens": segment.tokens(),
                }),
                now,
            );

            self.segments.push(segment);
        }

        // Spec §4.4: context_version genau EINMAL pro Aufruf erhöhen (auch bei Batch).
        self.session.increment_version(now);

        Ok(AppendOutcome { segment_ids, context_version: self.session.context_version() })
    }
}
