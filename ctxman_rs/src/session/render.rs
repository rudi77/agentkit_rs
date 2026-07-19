//! Render-Hot-Path (Spec §3.1/§4.3/§4.6; Port der Logik aus `RenderEndpoints.RenderAsync`):
//! Plan → I5-Prüfung → Adapter → Watermark-Aktion (soft/hard = GC-Empfehlung, emergency =
//! synchrone I/O-freie Eviction, danach ggf. `BudgetExceeded` OHNE Turn-Advance) → Events.

use serde_json::json;
use serde_json::Value;

use super::ContextSession;
use crate::domain::{RenderScope, SegmentState, WatermarkLevel};
use crate::error::CtxmanError;
use crate::events::types;
use crate::gc::{collect_emergency_no_io, GcLevel};
use crate::rendering::{adapter_for, canonical_json, planner, static_prefix, CacheBreakpoint};

/// Optionen eines Render-Aufrufs (Spec §4.3). `turn_advance = true` zählt den Turn hoch —
/// ein Turn ist genau ein Model-Call (Spec §3).
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Offener Provider-Bezeichner: "anthropic" | "openai".
    pub provider: String,
    pub scope: RenderScope,
    pub turn_advance: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            provider: "anthropic".to_string(),
            scope: RenderScope::Path,
            turn_advance: true,
        }
    }
}

/// Ergebnis eines Renders (Spec §4.3/§4.6). `request_fragment` ist das provider-spezifische
/// Fragment; `canonical_json` sind die byte-stabilen Bytes (I4), aus denen auch verglichen
/// werden kann; `recommended_gc` ersetzt das Worker-Enqueue des C#-Service — der Host ruft
/// `run_minor_gc`/`run_major_gc` explizit.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOutput {
    pub request_fragment: Value,
    pub canonical_json: String,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    pub builtin_tools: Vec<Value>,
    pub context_version: u64,
    pub static_epoch: u32,
    pub tokens_total: i64,
    pub watermark: WatermarkLevel,
    pub cache_prefix_hash: String,
    pub recommended_gc: Option<GcLevel>,
}

impl ContextSession {
    /// Rendert den aktuellen Context deterministisch (Spec §4.6, I3–I5) und führt die
    /// Watermark-Aktionen des Hot Path aus (Spec §3.1).
    pub fn render(&mut self, options: RenderOptions) -> Result<RenderOutput, CtxmanError> {
        let adapter = adapter_for(&options.provider)
            .ok_or_else(|| CtxmanError::UnknownProvider(options.provider.clone()))?;

        // Spec §2.5: offene Frames und Tip für den Scope-Filter ermitteln.
        let open_frame_ids = self.open_frame_ids();
        let tip_frame_id = self.tip_frame_id();

        let plan = planner::plan(
            &self.segments,
            self.session.policy(),
            options.scope,
            &open_frame_ids,
            tip_frame_id,
        );

        // Spec §2 I5: unvollständige Units ⇒ Fehler mit offenen tool_call-IDs (C#: 422).
        if !plan.open_tool_call_ids.is_empty() {
            return Err(CtxmanError::IncompleteUnits {
                open_tool_call_ids: plan.open_tool_call_ids,
            });
        }

        let rendered = adapter.render(&plan.model);
        let cache_prefix_hash = canonical_json::compute_cache_prefix_hash(
            &static_prefix::build_hash_input(&plan.static_prefix),
        );

        let now = (self.services.clock)();

        // Spec §3.1: soft/hard empfehlen einen asynchronen GC-Lauf (der Host führt ihn aus);
        // emergency läuft synchron I/O-frei innerhalb dieses Aufrufs.
        let recommended_gc = match plan.watermark {
            WatermarkLevel::Soft => Some(GcLevel::Minor),
            WatermarkLevel::Hard => Some(GcLevel::Major),
            _ => None,
        };

        // tokens_total wird nur im Emergency-Pfad nach der synchronen Eviction neu berechnet;
        // ok/soft/hard melden die unveränderte Zahl aus dem Plan.
        let mut reported_tokens_total = plan.tokens_total;
        let mut emergency_plan = None;

        if plan.watermark == WatermarkLevel::Emergency {
            // Spec §3.1: synchrone Notfall-Minor-Collection — nur Clean-Page + TTL-Eviction,
            // KEIN Blob-Write, KEIN LLM-Call. Erst planen, nichts mutieren.
            let ep = collect_emergency_no_io(
                &self.segments,
                self.session.policy(),
                self.session.current_turn(),
            );

            // tokens_total nach hypothetischer Eviction (render-eligible = live | externalized).
            let evicted_tokens: i64 = ep
                .clean_page_evicted
                .iter()
                .chain(ep.unit_evicted.iter().flat_map(|u| u.segment_ids.iter()))
                .filter_map(|id| self.segments.iter().find(|s| s.id() == *id))
                .filter(|s| {
                    matches!(s.state(), SegmentState::Live | SegmentState::Externalized)
                })
                .map(|s| i64::from(s.tokens()))
                .sum();
            reported_tokens_total = plan.tokens_total - evicted_tokens;

            // Spec §3.1: immer noch ≥ volles Budget B (NICHT die 0.95·B-Schwelle) ⇒ retrybarer
            // Fehler, KEIN turn_advance/Versions-Bump, KEINE Mutation (C#: 413 ohne Commit).
            if reported_tokens_total >= i64::from(self.session.policy().budget_tokens) {
                return Err(CtxmanError::BudgetExceeded {
                    tokens_total: reported_tokens_total,
                    budget: self.session.policy().budget_tokens,
                });
            }

            emergency_plan = Some(ep);
        }

        // Ab hier nur noch unfehlbare Mutationen (Ersatz der atomaren DB-Transaktion).
        if let Some(ep) = emergency_plan {
            // Spec §3.2.1 / §6: ein segment_evicted je Clean-Page-Segment.
            for id in &ep.clean_page_evicted {
                if let Ok(segment) = self.segment_mut(*id) {
                    let _ = segment.evict();
                }
                self.record_event(
                    types::SEGMENT_EVICTED,
                    json!({ "segment_id": id.to_string() }),
                    now,
                );
            }
            // Spec §3.2.3 / §6: ein unit_evicted je Unit (gekoppelte Unit ⇒ ein Event).
            for unit in &ep.unit_evicted {
                for id in &unit.segment_ids {
                    if let Ok(segment) = self.segment_mut(*id) {
                        let _ = segment.evict();
                    }
                }
                self.record_event(
                    types::UNIT_EVICTED,
                    json!({
                        "unit_id": unit.unit_id,
                        "segment_ids": unit.segment_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
                    }),
                    now,
                );
            }
        }

        if options.turn_advance {
            self.session.advance_turn(now);
            self.session.increment_version(now);
        }

        // Spec §6: render_served mit cache_prefix_hash (Determinismus-Messpunkt).
        self.record_event(
            types::RENDER_SERVED,
            json!({
                "context_version": self.session.context_version(),
                "static_epoch": self.session.static_epoch(),
                "tokens_total": reported_tokens_total,
                "cache_prefix_hash": cache_prefix_hash,
            }),
            now,
        );

        // Spec §3.1: watermark_crossed dokumentiert die überschrittene Stufe (ok ⇒ kein Event).
        if plan.watermark != WatermarkLevel::Ok {
            self.record_event(
                types::WATERMARK_CROSSED,
                json!({ "level": plan.watermark.wire_name() }),
                now,
            );
        }

        let canonical = canonical_json::serialize(&rendered.request_fragment);

        Ok(RenderOutput {
            request_fragment: rendered.request_fragment,
            canonical_json: canonical,
            cache_breakpoints: rendered.cache_breakpoints,
            builtin_tools: rendered.builtin_tools,
            context_version: self.session.context_version(),
            static_epoch: self.session.static_epoch(),
            tokens_total: reported_tokens_total,
            watermark: plan.watermark,
            cache_prefix_hash,
            recommended_gc,
        })
    }
}
