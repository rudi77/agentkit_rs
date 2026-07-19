use std::collections::HashSet;

use ulid::Ulid;

use super::canonical_json;
use super::model::{
    RenderBlockKind, RenderContentBlock, RenderMessage, RenderModel, RenderPlanResult,
    RenderStaticItem,
};
use crate::domain::{PolicyConfig, Region, RenderScope, Segment, SegmentState, WatermarkLevel};

/// Deterministische Render-Pipeline (Spec §4.6): erzeugt aus dem Segment-Stand einer Session
/// ein neutrales [`RenderModel`] samt Render-Metadaten. Zustandslos — alle Entscheidungen
/// folgen ausschließlich aus den Eingaben, nie aus Insertion-Order (I4).
///
/// Erzwingt:
/// I3 — `evicted | compacted` Segmente erscheinen nie im Output.
/// I4 — strikt `Static → Working`; Static kanonisch nach `(source, kind, content_hash)`,
///      Working strikt nach `seq`; gepinnte Segmente bleiben an ihrer chronologischen Position.
/// I5 — unvollständige Units (tool_call ohne render-eligibles tool_result) werden gemeldet,
///      nicht geworfen; der Aufrufer entscheidet über den Fehler.
///
/// `open_frame_ids`: IDs aller offenen Frames; leer = Root-Level (kein Frame aktiv).
/// `tip_frame_id`: der oberste offene Frame (Stack-Tip); `None` wenn kein Frame offen.
pub fn plan(
    segments: &[Segment],
    policy: &PolicyConfig,
    scope: RenderScope,
    open_frame_ids: &[Ulid],
    tip_frame_id: Option<Ulid>,
) -> RenderPlanResult {
    // Spec §2.2 I3: nur live | externalized sind render-eligible; evicted | compacted nie.
    let eligible: Vec<&Segment> = segments
        .iter()
        .filter(|s| matches!(s.state(), SegmentState::Live | SegmentState::Externalized))
        .collect();

    // Spec §4.6 / I4: Static kanonisch nach (source, kind, content_hash) — nie Insertion-Order.
    // Static-Segmente sind innerhalb der Epoche immutable und damit live ⇒ content != None (I1/I2).
    let mut static_items: Vec<RenderStaticItem> = eligible
        .iter()
        .filter(|s| s.region() == Region::Static)
        .map(|s| {
            let content = s.content().unwrap_or_default().to_string();
            let content_hash = canonical_json::content_hash(&content);
            RenderStaticItem {
                source: s.source().map(str::to_string),
                kind: s.kind().to_string(),
                content,
                content_hash,
                tokens: s.tokens(),
            }
        })
        .collect();
    static_items.sort_by(|a, b| {
        (a.source.as_deref().unwrap_or(""), &a.kind, &a.content_hash)
            .cmp(&(b.source.as_deref().unwrap_or(""), &b.kind, &b.content_hash))
    });

    // Spec §2.5: Scope-Filter auf dem Working-Set VOR dem Sort/Coalescing anwenden (I4).
    // Static wird nie gefiltert. open_frame_ids leer bedeutet Root-Level (kein Frame aktiv).
    let open_frame_ids: HashSet<Ulid> = open_frame_ids.iter().copied().collect();

    let mut working: Vec<&Segment> = eligible
        .iter()
        .filter(|s| s.region() == Region::Working)
        .filter(|s| match scope {
            // Spec §2.5 scope=path (Default): Root-Segmente (frame_id = None) ODER Segmente
            // eines offenen Frames. Geschlossene/evicted Frames werden ausgeblendet.
            RenderScope::Path => s
                .frame_id()
                .map(|id| open_frame_ids.contains(&id))
                .unwrap_or(true),
            // Spec §2.5 scope=frame: gepinnte Root-Segmente (frame_id = None && pinned) PLUS
            // Segmente des Tip-Frames. Ohne Tip degeneriert zu gepinnten Root-Segmenten.
            RenderScope::Frame => {
                (s.frame_id().is_none() && s.pinned())
                    || (tip_frame_id.is_some() && s.frame_id() == tip_frame_id)
            }
        })
        .copied()
        .collect();

    // Spec §4.6 / I4: Working strikt nach seq aufsteigend; gepinnte Segmente bleiben an ihrer
    // chronologischen Position (kein Herausziehen, kein Reordering).
    working.sort_by_key(|s| s.seq());

    // Spec §2 I5: vollständige Unit = tool_call + korrespondierendes tool_result (via
    // tool_call_id). Render-eligible tool_results (live | externalized) schließen einen Call.
    let fulfilled_tool_call_ids: HashSet<&str> = working
        .iter()
        .filter(|s| s.kind() == "tool_result")
        .filter_map(|s| s.tool_call_id())
        .collect();

    let mut open_tool_call_ids: Vec<String> = Vec::new();
    for segment in &working {
        if segment.kind() == "tool_call" {
            if let Some(id) = segment.tool_call_id() {
                if !fulfilled_tool_call_ids.contains(id)
                    && !open_tool_call_ids.iter().any(|existing| existing == id)
                {
                    open_tool_call_ids.push(id.to_string());
                }
            }
        }
    }

    // Working → Content-Blocks, dann Coalescing benachbarter gleicher Rollen (Spec §4.6).
    let messages = coalesce(&working);

    let emit_expand_context_ref = eligible
        .iter()
        .any(|s| s.state() == SegmentState::Externalized);

    // tokens_total: Summe der bereits gezählten pro-Segment-Tokens; keine Neuberechnung über
    // einen Provider-Tokenizer.
    let tokens_total: i64 = eligible.iter().map(|s| i64::from(s.tokens())).sum();

    let watermark = WatermarkLevel::derive(tokens_total, policy);

    let model = RenderModel {
        static_items: static_items.clone(),
        messages,
        emit_expand_context_ref,
        tokens_total,
    };

    RenderPlanResult {
        model,
        open_tool_call_ids,
        tokens_total,
        watermark,
        static_prefix: static_items,
    }
}

/// Fasst benachbarte Working-Segmente gleicher Rolle (nach seq-Sort) zu einer
/// [`RenderMessage`] mit mehreren Content-Blocks zusammen (Spec §4.6, Coalescing).
fn coalesce(working_sorted: &[&Segment]) -> Vec<RenderMessage> {
    let mut messages: Vec<RenderMessage> = Vec::new();

    for segment in working_sorted {
        // Working-Segmente ohne Rolle sind nicht message-render-fähig — defensiv überspringen.
        let Some(role) = segment.role() else {
            continue;
        };

        let block = to_block(segment);

        match messages.last_mut() {
            Some(last) if last.role == role => last.blocks.push(block),
            _ => messages.push(RenderMessage { role, blocks: vec![block] }),
        }
    }

    messages
}

/// Baut den Content-Block eines Working-Segments. Externalisierte Segmente werden durch
/// `summary` + Ref-Hinweis auf die Segment-ID ersetzt (Spec §2.4) — nie der Roh-Content.
fn to_block(segment: &Segment) -> RenderContentBlock {
    let kind = match segment.kind() {
        "tool_call" => RenderBlockKind::ToolCall,
        "tool_result" => RenderBlockKind::ToolResult,
        _ => RenderBlockKind::Text,
    };

    // Spec §2.4: externalisiertes Result ⇒ summary + Ref-Hinweis, der auf das Segment zeigt.
    let text = if segment.state() == SegmentState::Externalized {
        Some(build_ref_hint(segment))
    } else {
        segment.content().map(str::to_string)
    };

    let tool_name = if segment.kind() == "tool_call" {
        segment.source().map(str::to_string)
    } else {
        None
    };

    RenderContentBlock {
        kind,
        text,
        tool_call_id: segment.tool_call_id().map(str::to_string),
        tool_name,
    }
}

fn build_ref_hint(segment: &Segment) -> String {
    let summary = match segment.summary() {
        Some(s) if !s.is_empty() => s,
        _ => "(externalized)",
    };
    format!(
        "{summary}\n[content externalized — expand_context_ref(segment_id=\"{}\")]",
        segment.id()
    )
}
