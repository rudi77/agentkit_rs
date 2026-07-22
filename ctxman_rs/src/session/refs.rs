//! Page Fault / Lazy Expansion (Spec §3.4; Port von `RefEndpoints.ExpandRefAsync` plus der
//! Client-SDK-Rolle aus Spec §3.4): Blob laden, `last_referenced_turn` setzen, `ref_expanded`
//! emittieren und den Inhalt als `ref_expansion`-Segment (kurze TTL) anhängen.

use serde_json::json;
use ulid::Ulid;

use super::{AppendContent, AppendRequest, ContextSession};
use crate::domain::{Role, SegmentState};
use crate::error::CtxmanError;
use crate::events::types;

/// Ergebnis eines Page Fault (Spec §3.4): der expandierte Inhalt plus die ID des neu
/// angehängten `ref_expansion`-Segments.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandOutcome {
    pub content: String,
    pub content_type: String,
    /// ID des neu angelegten `ref_expansion`-Segments (sehr kurze TTL — kann erneut
    /// eingesammelt werden, Spec §2.3).
    pub expansion_segment_id: Ulid,
}

/// Spec §3.2.2: Externalisierung schreibt Blobs als text/plain; charset=utf-8 — Default,
/// falls der BlobRef keinen Content-Type trägt.
const DEFAULT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

impl ContextSession {
    /// Expandiert ein externalisiertes Segment (Page Fault, Spec §3.4). Nur ein
    /// externalisiertes Segment mit noch vorhandenem Blob lässt sich expandieren;
    /// evicted/compacted oder gesweepter Blob ⇒ [`CtxmanError::RefGone`] mit Restinformation.
    pub fn expand_ref(&mut self, segment_id: Ulid) -> Result<ExpandOutcome, CtxmanError> {
        let segment = self.segment(segment_id)?;

        let expandable = segment.state() == SegmentState::Externalized
            && segment
                .blob_ref()
                .map(|b| self.services.blob_store.exists(&b.key).unwrap_or(false))
                .unwrap_or(false);

        if !expandable {
            // Spec §4.3 / §7.1: nicht mehr live (evicted/compacted/gesweept) ⇒ Restinformation.
            return Err(CtxmanError::RefGone {
                summary: segment.summary().map(str::to_string),
                origin: segment.origin().map(str::to_string),
            });
        }

        let blob_ref = segment.blob_ref().expect("expandable ⇒ blob_ref vorhanden").clone();
        let bytes = self.services.blob_store.get(&blob_ref.key)?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let content_type = if blob_ref.content_type.trim().is_empty() {
            DEFAULT_CONTENT_TYPE.to_string()
        } else {
            blob_ref.content_type.clone()
        };

        let now = (self.services.clock)();
        let current_turn = self.session.current_turn();

        // Spec §3.4: Page Fault setzt last_referenced_turn des Ursprungssegments := current_turn
        // (approximierte Liveness statt reiner Heuristik).
        self.segment_mut(segment_id)?.mark_referenced(current_turn)?;

        self.record_event(
            types::REF_EXPANDED,
            json!({ "segment_id": segment_id.to_string() }),
            now,
        );

        // Spec §3.4 (SDK-Rolle): Ergebnis als kind=ref_expansion-Segment anhängen — sehr kurze
        // TTL (Spec §2.3), es kann erneut eingesammelt werden.
        let expansion_segment_id = self.append_segment(AppendRequest {
            kind: "ref_expansion".to_string(),
            role: Some(Role::User),
            content: AppendContent::Inline(content.clone()),
            source: None,
            tool_call_id: None,
            pinned: false,
            refetchable: false,
            origin: None,
        })?;

        Ok(ExpandOutcome { content, content_type, expansion_segment_id })
    }
}
