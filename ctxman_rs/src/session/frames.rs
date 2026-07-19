//! Frame-Stack (Spec §2.5; Port von `FrameEndpoints`): Push bindet an den Stack-Tip, Pop
//! erzwingt LIFO (offene Kind-Frames zuerst), promotet Frame-Fakten VOR der Eviction und legt
//! das Return-Segment im Parent-Frame an.

use serde_json::json;
use ulid::Ulid;

use super::ContextSession;
use crate::compaction::{CompactionRequest, WindowItem, FACT_EXTRACTION_TEMPLATE_ID};
use crate::domain::{Frame, FrameStatus, Region, Role, Segment, SegmentDraft, SegmentState};
use crate::error::CtxmanError;
use crate::events::types;
use crate::promotion::PromotedFact;
use crate::rendering::canonical_json;

/// Ergebnis eines Frame-Pops (Spec §2.5): ID des `subagent_return`-Segments im Parent-Frame
/// plus die neue `context_version`.
#[derive(Debug, Clone, PartialEq)]
pub struct PopOutcome {
    pub return_segment_id: Ulid,
    pub context_version: u64,
}

impl ContextSession {
    /// Öffnet einen neuen Frame (Spec §2.5 push): `parent_frame_id` = Stack-Tip (oberster
    /// offener Frame) oder `None` (= Root).
    pub fn push_frame(&mut self, label: &str) -> Ulid {
        let parent_frame_id = self.tip_frame_id();
        let now = (self.services.clock)();

        let frame = Frame::new(
            Ulid::new(),
            self.session.id(),
            parent_frame_id,
            label.to_string(),
            self.session.current_turn(), // Spec §2.5
        );
        let frame_id = frame.id();

        // Spec §6: jede Mutation emittiert ein Event.
        self.record_event(
            types::FRAME_PUSHED,
            json!({
                "frame_id": frame_id.to_string(),
                "parent_frame_id": parent_frame_id.map(|id| id.to_string()),
                "label": label,
                "opened_turn": frame.opened_turn(),
            }),
            now,
        );

        self.frames.push(frame);

        // Spec §4.4: context_version genau EINMAL pro Aufruf erhöhen.
        self.session.increment_version(now);

        frame_id
    }

    /// Poppt einen Frame (Spec §2.5 pop): Promotion der Frame-Segmente VOR der Eviction
    /// (Frame-lokale Entscheidungen dürfen nicht verloren gehen, §3.3), dann alle Segmente
    /// des Frames evicten und den Return-Content als `subagent_return`-Segment (bzw.
    /// `return_kind`) im Parent-Frame anlegen. Offene Kind-Frames ⇒ Fehler (strikte LIFO-
    /// Disziplin, Kinder zuerst).
    ///
    /// Schlägt die Promotion fehl, wird NICHT gepoppt (definierter, retrybarer Fehler —
    /// Verhalten des C#-Originals). Ohne konfiguriertes CompactionModel wird die Promotion
    /// übersprungen (dokumentierte Bibliotheks-Divergenz).
    pub fn pop_frame(
        &mut self,
        frame_id: Ulid,
        return_content: &str,
        return_kind: Option<&str>,
    ) -> Result<PopOutcome, CtxmanError> {
        let frame = self
            .frames
            .iter()
            .find(|f| f.id() == frame_id)
            .ok_or(CtxmanError::FrameNotFound { id: frame_id })?;

        // Bibliotheks-Guard (ohne Idempotency-Keys): ein bereits gepoppter Frame kann nicht
        // erneut gepoppt werden.
        if frame.status() == FrameStatus::Popped {
            return Err(CtxmanError::FrameDiscipline(format!(
                "Frame {frame_id} ist bereits gepoppt"
            )));
        }
        let parent_frame_id = frame.parent_frame_id();

        // Spec §2.5 LIFO: wenn irgendein anderer OFFENER Frame diesen als Parent hat, gibt es
        // offene Kinder — Pop verboten.
        let has_open_children = self
            .frames
            .iter()
            .any(|f| f.status() == FrameStatus::Open && f.parent_frame_id() == Some(frame_id));
        if has_open_children {
            return Err(CtxmanError::FrameDiscipline(format!(
                "Frame {frame_id} hat offene Kind-Frames; Pop nur am Stack-Tip"
            )));
        }

        // Spec §3.3: Segmente des Frames für die Promotion (live/externalized Working).
        let mut frame_segment_ids: Vec<Ulid> = self
            .segments
            .iter()
            .filter(|s| s.frame_id() == Some(frame_id))
            .map(|s| s.id())
            .collect();
        frame_segment_ids.sort_by_key(|id| {
            self.segments
                .iter()
                .find(|s| s.id() == *id)
                .map(|s| s.seq())
                .unwrap_or(i64::MAX)
        });

        let promotion_candidates: Vec<&Segment> = frame_segment_ids
            .iter()
            .filter_map(|id| self.segments.iter().find(|s| s.id() == *id))
            .filter(|s| {
                s.region() == Region::Working
                    && matches!(s.state(), SegmentState::Live | SegmentState::Externalized)
            })
            .collect();

        // Spec §3.3 Schritt 1 (Port von `PromotionService.ExtractAndSinkAsync`): LLM-Call VOR
        // jeder Mutation — bei Fehler KEIN Pop, Retry ist sicher.
        let mut promotion_event: Option<(Ulid, String, String)> = None;
        if !promotion_candidates.is_empty() {
            if let Some(model) = self.services.compaction_model.as_deref() {
                let window: Vec<WindowItem> = promotion_candidates
                    .iter()
                    .map(|s| WindowItem {
                        content: s.content().or(s.summary()).unwrap_or_default().to_string(),
                        kind: Some(s.kind().to_string()),
                    })
                    .collect();

                let policy = self.session.policy();
                let result = model.summarize(&CompactionRequest {
                    window,
                    prompt_template_id: FACT_EXTRACTION_TEMPLATE_ID.to_string(),
                    model: policy.compaction.model.clone(),
                })?;

                if !result.summary.is_empty() {
                    // Spec §3.3: leeres Summary = keine dauerhaften Fakten; sonst Sink-Write.
                    let oldest = promotion_candidates[0];
                    let fact = PromotedFact {
                        fact: result.summary,
                        source_session: self.session.id().to_string(),
                        source_turn: self.session.current_turn(),
                        kind: oldest.kind().to_string(),
                    };
                    let sink_url = policy.promotion.sink.url.clone().unwrap_or_default();
                    let sink = self.services.promotion_sink.as_deref().ok_or_else(|| {
                        CtxmanError::Promotion(
                            "kein PromotionSink konfiguriert (CtxmanServices)".into(),
                        )
                    })?;
                    sink.write(&fact, &sink_url)?;

                    let payload_json =
                        serde_json::to_string(&fact).expect("PromotedFact ist serialisierbar");
                    let digest = canonical_json::content_hash(&payload_json);
                    promotion_event = Some((oldest.id(), sink_url, digest));
                }
            }
        }

        // Ab hier nur noch unfehlbare Mutationen (Ersatz der atomaren DB-Transaktion).
        let now = (self.services.clock)();

        // 1) Alle Segmente des Frames evicten (Spec §2.2 I3: Daten bleiben, nur State).
        for id in &frame_segment_ids {
            if let Ok(segment) = self.segment_mut(*id) {
                // Static-Segmente gehören nicht zu Frames — defensiv ignorieren.
                let _ = segment.evict();
            }
        }

        // 2) Return-Segment im Parent-Frame anlegen (Spec §2.5 pop).
        let return_segment_id = Ulid::new();
        let draft = SegmentDraft {
            frame_id: parent_frame_id, // None = Root-Frame (Spec §2.5)
            tokens: self.services.token_counter.count(return_content),
            ..SegmentDraft::new(
                return_segment_id,
                self.session.id(),
                Region::Working,
                return_kind.unwrap_or("subagent_return"),
                // Spec §2.5: Subagent-Return ist Assistant-Output; Rolle nötig fürs Coalescing.
                Some(Role::Assistant),
                self.next_seq,
                self.session.current_turn(),
            )
        };
        self.next_seq += 1;
        self.segments.push(Segment::create_live(draft, return_content));

        // 3) Frame als gepoppt markieren (Spec §2.5).
        if let Some(frame) = self.frames.iter_mut().find(|f| f.id() == frame_id) {
            frame.pop();
        }

        // 4) Events (Spec §6): fact_promoted ZUERST, dann frame_popped.
        if let Some((segment_id, sink, digest)) = promotion_event {
            self.record_event(
                types::FACT_PROMOTED,
                json!({
                    "segment_id": segment_id.to_string(),
                    "sink": sink,
                    "payload_digest": digest,
                }),
                now,
            );
        }
        self.record_event(
            types::FRAME_POPPED,
            json!({
                "frame_id": frame_id.to_string(),
                "return_segment_id": return_segment_id.to_string(),
            }),
            now,
        );

        // 5) context_version erhöhen (Spec §4.4: genau EINMAL pro Aufruf).
        self.session.increment_version(now);

        Ok(PopOutcome { return_segment_id, context_version: self.session.context_version() })
    }
}
