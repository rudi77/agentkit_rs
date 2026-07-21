//! Static-Epoch-Bump (Spec §4.2 / I1; Port von `RenderEndpoints.ReplaceStaticSegmentsAsync`):
//! ersetzt die komplette Static-Region atomar, wendet `on_tool_removed` auf betroffene
//! Working-Units an, erhöht `static_epoch` und emittiert `static_epoch_bumped`.

use serde_json::json;
use ulid::Ulid;

use super::ContextSession;
use crate::domain::{OnToolRemoved, Region, Segment, SegmentDraft, SegmentState};
use crate::error::CtxmanError;
use crate::events::types;
use crate::rendering::static_diff::{self, StaticRegionDiffResult, StaticSegmentSpec};

/// Ergebnis eines Epoch-Bumps (Spec §4.2).
#[derive(Debug, Clone, PartialEq)]
pub struct EpochDiffOutcome {
    pub static_epoch: u32,
    pub context_version: u64,
    pub diff: StaticRegionDiffResult,
}

impl ContextSession {
    /// Ersetzt die Static-Region vollständig (Spec §4.2): jede Epoche ist eine bewusste,
    /// auditierbare Cache-Invalidierung. Auf die Units entfernter Tools wird
    /// `policy.on_tool_removed` angewandt (keep | externalize | evict). Alte Static-Segmente
    /// werden hart entfernt (wie im C#-Original), neue erhalten seq 0..n — die Render-Ordnung
    /// der Static-Region ist ohnehin kanonisch (I4), nie seq-basiert.
    pub fn bump_static_epoch(
        &mut self,
        new_static: Vec<StaticSegmentSpec>,
    ) -> Result<EpochDiffOutcome, CtxmanError> {
        // Spec §4.3: Inline-Content über 1 MiB ⇒ Fehler (vor jeder Mutation).
        for spec in &new_static {
            if spec.content.len() > 1_048_576 {
                return Err(CtxmanError::ContentTooLarge {
                    bytes: spec.content.len(),
                });
            }
        }

        let old_static: Vec<Segment> = self
            .segments
            .iter()
            .filter(|s| s.region() == Region::Static)
            .cloned()
            .collect();

        let diff = static_diff::compute(&old_static, &new_static);

        let old_static_tokens: i64 = old_static.iter().map(|s| i64::from(s.tokens())).sum();
        let new_static_tokens: i64 = new_static
            .iter()
            .map(|s| i64::from(self.services.token_counter.count(&s.content)))
            .sum();
        let tokens_delta = new_static_tokens - old_static_tokens;

        // Spec §4.2: on_tool_removed auf die Units der entfernten Tools anwenden — Port von
        // `StaticRegionDiff.ApplyOnToolRemovedAsync`, I/O (Blob-Puts) vor den Mutationen.
        self.apply_on_tool_removed(&diff)?;

        let now = (self.services.clock)();
        let old_epoch = self.session.static_epoch();
        let session_id = self.session.id();
        let current_turn = self.session.current_turn();

        // Alte Static-Region hart entfernen (C#: RemoveRange) und neue anlegen (seq 0..n).
        self.segments.retain(|s| s.region() != Region::Static);
        for (seq, spec) in new_static.into_iter().enumerate() {
            let mut draft = SegmentDraft::new(
                Ulid::new(),
                session_id,
                Region::Static,
                &spec.kind,
                spec.role,
                seq as i64,
                current_turn,
            );
            draft.source = spec.source;
            draft.tokens = self.services.token_counter.count(&spec.content);
            self.segments
                .push(Segment::create_live(draft, &spec.content));
        }

        self.session.bump_static_epoch(now);
        self.session.increment_version(now);

        // Spec §4.2 / §6: static_epoch_bumped mit Diff und Token-Delta.
        self.record_event(
            types::STATIC_EPOCH_BUMPED,
            json!({
                "old_epoch": old_epoch,
                "new_epoch": self.session.static_epoch(),
                "tokens_delta": tokens_delta,
                "diff": {
                    "added_tools": diff.added_tools,
                    "removed_tools": diff.removed_tools,
                    "added_sources": diff.added_sources,
                    "removed_sources": diff.removed_sources,
                },
            }),
            now,
        );

        Ok(EpochDiffOutcome {
            static_epoch: self.session.static_epoch(),
            context_version: self.session.context_version(),
            diff,
        })
    }

    /// Spec §4.2: keep | externalize (Default) | evict — angewandt auf die Units der
    /// entfernten Tools/Quellen.
    fn apply_on_tool_removed(&mut self, diff: &StaticRegionDiffResult) -> Result<(), CtxmanError> {
        if diff.removed_tools.is_empty() && diff.removed_sources.is_empty() {
            return Ok(());
        }

        let working: Vec<&Segment> = self
            .segments
            .iter()
            .filter(|s| s.region() == Region::Working)
            .collect();
        let affected = static_diff::affected_tool_call_ids(&working, diff);
        if affected.is_empty() {
            return Ok(());
        }

        match self.session.policy().on_tool_removed {
            OnToolRemoved::Keep => Ok(()),

            OnToolRemoved::Evict => {
                let ids: Vec<Ulid> = self
                    .segments
                    .iter()
                    .filter(|s| {
                        matches!(s.state(), SegmentState::Live | SegmentState::Externalized)
                            && matches!(s.kind(), "tool_call" | "tool_result")
                            && s.tool_call_id().is_some_and(|id| affected.contains(id))
                    })
                    .map(|s| s.id())
                    .collect();
                for id in ids {
                    self.segment_mut(id)?.evict()?;
                }
                Ok(())
            }

            // externalize (Spec §4.2 Default): nur die tool_results; I/O vor Mutation.
            OnToolRemoved::Externalize => {
                let candidates: Vec<(Ulid, String)> = self
                    .segments
                    .iter()
                    .filter(|s| {
                        s.kind() == "tool_result"
                            && s.tool_call_id().is_some_and(|id| affected.contains(id))
                            && s.state() == SegmentState::Live
                            && s.content().is_some()
                    })
                    .map(|s| (s.id(), s.content().unwrap_or_default().to_string()))
                    .collect();

                let mut writes = Vec::with_capacity(candidates.len());
                for (id, content) in candidates {
                    let blob_ref = self
                        .services
                        .blob_store
                        .put(content.as_bytes(), "text/plain")?;
                    // Spec §3.2.2-Spiegel: summary = erste 200 Zeichen + „…".
                    let summary = if content.chars().count() <= 200 {
                        content.clone()
                    } else {
                        let mut s: String = content.chars().take(200).collect();
                        s.push('…');
                        s
                    };
                    writes.push((id, blob_ref, summary));
                }
                for (id, blob_ref, summary) in writes {
                    // Token-Basis nach der Externalisierung ist die summary (Render-Ersatz).
                    let tokens = self.services.token_counter.count(&summary);
                    self.segment_mut(id)?
                        .externalize(blob_ref, Some(summary), tokens)?;
                }
                Ok(())
            }
        }
    }
}
