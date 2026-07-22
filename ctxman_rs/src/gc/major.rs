use ulid::Ulid;

use super::units::group_into_units;
use crate::domain::{PolicyConfig, Region, Segment, SegmentState};

/// Der Compaction-Plan für eine Major Collection (Spec §3.3 Schritt 2). Enthält die geordneten
/// Quell-Segment-IDs (älteste→jüngste nach `seq`), die Gesamttokenanzahl vor der Komprimierung
/// und die älteste Seq-Position, an der das Zusammenfassungs-Segment eingefügt wird.
///
/// Ein leerer Plan (`is_no_op()`) signalisiert, dass keine Komprimierung stattfindet —
/// weniger als 2 auswählbare Units im Compaction-Fenster.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompactionPlan {
    pub source_segment_ids: Vec<Ulid>,
    pub tokens_before: i64,
    pub oldest_seq: i64,
}

impl CompactionPlan {
    /// Kein nutzbarer Compaction-Plan: weniger als 2 Segmente im Fenster
    /// (Spec §3.3: ein Einzel-Segment lohnt keinen LLM-Call).
    pub fn is_no_op(&self) -> bool {
        self.source_segment_ids.len() < 2
    }
}

/// Deterministische, I/O-freie Major Collection (Spec §3.3 Schritt 2; Port von
/// `MajorCollector.PlanCompaction`). Berechnet das Compaction-Fenster aus allen nicht
/// gepinnten, live Working-Segmenten — sortiert nach `seq` aufsteigend (älteste→jüngste),
/// Unit-weise akkumuliert, bis die nächste Unit `max_share × budget_tokens` überschreiten
/// würde. Kein I/O, keine Mutationen.
///
/// Spec §2.4: Compaction operiert auf Units, nie auf Einzel-Segmenten. Eine gekoppelte
/// tool_call+tool_result-Unit wird atomar behandelt — entweder ganz oder gar nicht
/// ausgewählt, damit keine verwaisten tool_use/tool_result-Blöcke im Render entstehen.
pub fn plan_compaction(segments: &[Segment], policy: &PolicyConfig) -> CompactionPlan {
    // Spec §3.3: Fenster = render-eligible Working-Segmente (live UND externalisiert);
    // Static ausgeschlossen (I1), compacted/evicted nicht berücksichtigt. Externalisierte
    // Segmente MÜSSEN mit ins Unit-Grouping: sonst würde der live tool_call einer
    // gekoppelten Unit allein kompaktiert und das externalisierte tool_result verwaiste
    // im Render (orphaned `role: tool`-Message). Ihr Fenster-Beitrag ist die summary.
    let mut candidates: Vec<&Segment> = segments
        .iter()
        .filter(|s| {
            s.region() == Region::Working
                && matches!(s.state(), SegmentState::Live | SegmentState::Externalized)
        })
        .collect();
    candidates.sort_by_key(|s| s.seq());

    // Spec §2.4: gekoppelte tool_call+tool_result-Paare bilden eine Unit — atomar behandelt.
    // Gepinnt wird auf UNIT-Ebene ausgeschlossen: ein gepinntes Segment schützt seine
    // ganze Unit (sonst entstünde derselbe Orphan über den Pin).
    let units: Vec<_> = group_into_units(&candidates)
        .into_iter()
        .filter(|u| u.segments.iter().all(|s| !s.pinned()))
        .collect();

    // Spec §3.3 / §2.4: Tokenbudget-Obergrenze = max_share × budget_tokens.
    // Akkumulieren UNIT BY UNIT — eine Unit wird ganz aufgenommen oder gar nicht.
    let token_limit = policy.compaction.max_share * f64::from(policy.budget_tokens); // Spec §3.3
    let mut selected_units = Vec::new();
    let mut accumulated: i64 = 0;

    for unit in units {
        let unit_tokens: i64 = unit.segments.iter().map(|s| i64::from(s.tokens())).sum();

        // Spec §3.3: stopp BEVOR die nächste Unit das Limit überschreiten würde.
        if !selected_units.is_empty() && (accumulated + unit_tokens) as f64 > token_limit {
            break;
        }

        accumulated += unit_tokens;
        selected_units.push(unit);
    }

    // Spec §3.3 / §2.4: weniger als 2 Units → kein sinnvoller LLM-Compaction-Call.
    if selected_units.len() < 2 {
        return CompactionPlan::default();
    }

    // Flatten der ausgewählten Units zurück in eine Segment-Liste (älteste→jüngste nach seq).
    let mut selected: Vec<&Segment> = selected_units
        .into_iter()
        .flat_map(|u| u.segments)
        .collect();
    selected.sort_by_key(|s| s.seq());

    // Spec §3.3: die älteste Seq-Position bestimmt den Einfügepunkt des Summary-Segments.
    let oldest_seq = selected[0].seq();

    CompactionPlan {
        source_segment_ids: selected.iter().map(|s| s.id()).collect(),
        tokens_before: accumulated,
        oldest_seq,
    }
}
