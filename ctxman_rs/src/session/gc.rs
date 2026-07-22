//! GC-Ausführung (Spec §3.2/§3.3; Ports von `MinorGcWorker.RunMinorAsync` und
//! `MajorCollection.ExecuteAsync`, ohne Worker/Queue — der Host ruft explizit auf).

use serde_json::json;
use ulid::Ulid;

use super::ContextSession;
use crate::compaction::{
    CompactionRequest, WindowItem, DEFAULT_TEMPLATE_ID, FACT_EXTRACTION_TEMPLATE_ID,
};
use crate::domain::{Region, Segment, SegmentDraft};
use crate::error::CtxmanError;
use crate::events::types;
use crate::gc::{major, minor, EvictedUnit};
use crate::promotion::PromotedFact;
use crate::rendering::canonical_json;

/// Ergebnis eines Minor-GC-Laufs (Spec §3.2).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MinorGcReport {
    pub externalized: Vec<Ulid>,
    pub clean_page_evicted: Vec<Ulid>,
    pub unit_evicted: Vec<EvictedUnit>,
}

impl MinorGcReport {
    pub fn is_empty(&self) -> bool {
        self.externalized.is_empty()
            && self.clean_page_evicted.is_empty()
            && self.unit_evicted.is_empty()
    }
}

/// Ergebnis eines Major-GC-Laufs (Spec §3.3). `summary_segment_id` ist `None` bei No-Op
/// (weniger als 2 Units im Compaction-Fenster).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MajorGcReport {
    pub summary_segment_id: Option<Ulid>,
    pub compacted_source_ids: Vec<Ulid>,
    pub tokens_before: i64,
    pub tokens_after: u32,
    pub fact_promoted: bool,
}

impl ContextSession {
    /// Vollständige Minor Collection (Spec §3.2; Port von `MinorGcWorker.RunMinorAsync`):
    /// Phase 2 (Externalisierung) mit Blob-Writes, Phasen 1+3 als Evictions; Events je Phase.
    /// I/O (Blob-Puts) läuft VOR der ersten Mutation — ein Fehler lässt die Session unverändert.
    pub fn run_minor_gc(&mut self) -> Result<MinorGcReport, CtxmanError> {
        let plan = minor::plan_full(
            &self.segments,
            self.session.policy(),
            self.session.current_turn(),
        );

        if plan.is_empty() {
            return Ok(MinorGcReport::default()); // Nichts zu tun ⇒ keine Events.
        }

        let now = (self.services.clock)();
        let mut report = MinorGcReport::default();

        // Phase 2 (Spec §3.2.2): erst alle Blob-Writes (I/O), dann die Mutationen.
        let mut externalizations = Vec::with_capacity(plan.externalization_candidates.len());
        for candidate in &plan.externalization_candidates {
            let blob_ref = self
                .services
                .blob_store
                .put(candidate.content.as_bytes(), "text/plain; charset=utf-8")?;
            externalizations.push((candidate.segment_id, blob_ref, candidate.summary.clone()));
        }

        for (segment_id, blob_ref, summary) in externalizations {
            let key = blob_ref.key.clone();
            // Token-Basis nach der Externalisierung ist die summary (Render-Ersatz).
            let tokens = summary
                .as_deref()
                .map(|s| self.services.token_counter.count(s))
                .unwrap_or(0);
            self.segment_mut(segment_id)?
                .externalize(blob_ref, summary, tokens)?;
            report.externalized.push(segment_id);
            self.record_event(
                types::SEGMENT_EXTERNALIZED,
                json!({ "segment_id": segment_id.to_string(), "blob_key": key }),
                now,
            );
        }

        // Phase 1 (Spec §3.2.1): Clean-Page-Eviction ⇒ ein segment_evicted je Segment.
        for id in &plan.clean_page_evicted {
            self.segment_mut(*id)?.evict()?;
            report.clean_page_evicted.push(*id);
            self.record_event(
                types::SEGMENT_EVICTED,
                json!({ "segment_id": id.to_string() }),
                now,
            );
        }

        // Phase 3 (Spec §3.2.3 / §6): TTL-Eviction operiert auf UNITS ⇒ ein unit_evicted je
        // Unit; eine gekoppelte Unit ergibt EIN Event über beide Segmente (§2.4).
        for unit in &plan.unit_evicted {
            for id in &unit.segment_ids {
                self.segment_mut(*id)?.evict()?;
            }
            self.record_event(
                types::UNIT_EVICTED,
                json!({
                    "unit_id": unit.unit_id,
                    "segment_ids": unit.segment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                }),
                now,
            );
            report.unit_evicted.push(unit.clone());
        }

        Ok(report)
    }

    /// Major Collection (Spec §3.3; Port von `MajorCollection.ExecuteAsync`): Promotion VOR
    /// Compaction (Compaction ist lossy), Fenster-IDs vor dem Modell-Call eingefroren,
    /// `compaction_summary`-Segment an der ältesten Fenster-Seq, Quellen → compacted (I3).
    /// Ohne konfiguriertes [`crate::compaction::CompactionModel`] ⇒ Fehler.
    pub fn run_major_gc(&mut self) -> Result<MajorGcReport, CtxmanError> {
        let policy = self.session.policy().clone();

        // Spec §3.3: Compaction-Fenster berechnen (I/O-frei, deterministisch).
        let plan = major::plan_compaction(&self.segments, &policy);
        if plan.is_no_op() {
            return Ok(MajorGcReport::default()); // Keine Events (< 2 Units im Fenster).
        }

        let model = self.services.compaction_model.as_deref().ok_or_else(|| {
            CtxmanError::Compaction("kein CompactionModel konfiguriert (CtxmanServices)".into())
        })?;

        // Spec §3.3 Schritt 3: Fenster-IDs JETZT einfrieren — alles, was ab hier angehängt
        // wird, hat eine strikt höhere seq und ist nie Teil des Plans.
        let frozen_window_ids = plan.source_segment_ids.clone();
        let current_turn = self.session.current_turn();
        let session_id = self.session.id();

        // Spec §3.3: die Inhalte aus dem Fenster für die LLM-Aufrufe aufbereiten.
        let window_items: Vec<WindowItem> = frozen_window_ids
            .iter()
            .filter_map(|id| self.segments.iter().find(|s| s.id() == *id))
            .map(|s| WindowItem {
                content: s.content().or(s.summary()).unwrap_or_default().to_string(),
                kind: Some(s.kind().to_string()),
            })
            .collect();

        // ── Schritt 1: PROMOTION (Spec §3.3 Schritt 1) — VOR der lossy Compaction. ──
        // Schlägt die Fact-Extraction fehl, propagiert der Fehler und es wird NICHTS
        // kompaktiert (kein Fakt-Verlust möglich); ein leeres Summary heißt „keine
        // dauerhaften Fakten" und die Compaction läuft normal weiter.
        let promotion_result = model.summarize(&CompactionRequest {
            window: window_items.clone(),
            prompt_template_id: FACT_EXTRACTION_TEMPLATE_ID.to_string(),
            model: policy.compaction.model.clone(),
        })?;

        let sink_url = policy.promotion.sink.url.clone().unwrap_or_default();
        let mut promotion_event: Option<(Ulid, String, String)> = None;

        if !promotion_result.summary.is_empty() {
            // Spec §3.3 AC6: Source-Segmente werden durch Promotion NICHT verändert.
            let oldest = self
                .segments
                .iter()
                .find(|s| s.id() == frozen_window_ids[0])
                .expect("eingefrorene Fenster-IDs existieren");

            let fact = PromotedFact {
                fact: promotion_result.summary,
                source_session: session_id.to_string(),
                source_turn: current_turn,
                kind: oldest.kind().to_string(),
            };

            let sink = self.services.promotion_sink.as_deref().ok_or_else(|| {
                CtxmanError::Promotion("kein PromotionSink konfiguriert (CtxmanServices)".into())
            })?;
            sink.write(&fact, &sink_url)?;

            // Payload-Digest: SHA-256 über das snake_case-JSON der Sink-Payload in
            // Deklarationsreihenfolge (Audit ohne Inhalt; Mirror des C#-Originals).
            let payload_json =
                serde_json::to_string(&fact).expect("PromotedFact ist serialisierbar");
            let digest = canonical_json::content_hash(&payload_json);

            promotion_event = Some((oldest.id(), sink_url, digest));
        }

        // ── Schritt 2: COMPACTION (Spec §3.3 Schritt 2). ──
        let compaction_result = model.summarize(&CompactionRequest {
            window: window_items,
            prompt_template_id: if policy.compaction.prompt_template_id.is_empty() {
                DEFAULT_TEMPLATE_ID.to_string()
            } else {
                policy.compaction.prompt_template_id.clone()
            },
            model: policy.compaction.model.clone(),
        })?;

        let summary_content = compaction_result.summary;
        let summary_tokens = self.services.token_counter.count(&summary_content);

        // ── Schritt 3: Mutationen + Events (Ersatz der atomaren DB-Transaktion). ──
        let now = (self.services.clock)();
        let source_id_strings: Vec<String> =
            frozen_window_ids.iter().map(|id| id.to_string()).collect();

        // fact_promoted ZUERST (Spec §3.3 / §6).
        let fact_promoted = promotion_event.is_some();
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
            types::COMPACTION_STARTED,
            json!({ "source_ids": source_id_strings }),
            now,
        );

        // Spec §3.3: neues compaction_summary-Segment (Working) an der ältesten Source-Seq —
        // chronologische Kontinuität; die seq wird bewusst WIEDERVERWENDET (non-unique).
        // Rolle User statt None: der Planner überspringt rollenlose Working-Segmente
        // (nicht message-render-fähig) — ohne Rolle wäre die Zusammenfassung im
        // Provider-Request unsichtbar und die Compaction de facto reine Löschung.
        let summary_segment_id = Ulid::new();
        let draft = SegmentDraft {
            tokens: summary_tokens,
            ..SegmentDraft::new(
                summary_segment_id,
                session_id,
                Region::Working,
                "compaction_summary",
                Some(crate::domain::Role::User),
                plan.oldest_seq,
                current_turn,
            )
        };
        let summary_segment = Segment::create_live(draft, &summary_content);
        self.segments.push(summary_segment);

        // Spec §3.3: Source-Segmente → compacted (Soft-Delete, I3). Nur die eingefrorenen
        // Fenster-Segmente werden angefasst — keine neu angefügten.
        for id in &frozen_window_ids {
            self.segment_mut(*id)?.compact(None)?;
        }

        self.record_event(
            types::COMPACTION_COMPLETED,
            json!({
                "source_ids": source_id_strings,
                "summary_id": summary_segment_id.to_string(),
                "tokens_before": plan.tokens_before,
                "tokens_after": summary_tokens,
            }),
            now,
        );

        Ok(MajorGcReport {
            summary_segment_id: Some(summary_segment_id),
            compacted_source_ids: frozen_window_ids,
            tokens_before: plan.tokens_before,
            tokens_after: summary_tokens,
            fact_promoted,
        })
    }
}
