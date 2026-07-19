//! Port der C#-`MinorCollectorTests`: Vertrag der deterministischen, I/O-freien Minor
//! Collection (Spec §3.2 / §3.1 / §2.4). Der Rust-Collector mutiert nicht — die Assertions
//! prüfen den Plan (und dass die Segmente unverändert bleiben).

use ctxman::domain::{PolicyConfig, Region, Role, Segment, SegmentDraft, SegmentState};
use ctxman::gc::{collect_emergency_no_io, plan_full};
use ulid::Ulid;

#[allow(clippy::too_many_arguments)]
fn seg(
    kind: &str,
    region: Region,
    refetchable: bool,
    pinned: bool,
    origin: Option<&str>,
    tool_call_id: Option<&str>,
    tokens: u32,
    last_referenced_turn: u32,
    seq: i64,
) -> Segment {
    let mut draft = SegmentDraft::new(
        Ulid::new(),
        Ulid::new(),
        region,
        kind,
        Some(Role::Tool),
        seq,
        0,
    );
    draft.refetchable = refetchable;
    draft.pinned = pinned;
    draft.origin = origin.map(str::to_string);
    draft.tool_call_id = tool_call_id.map(str::to_string);
    draft.tokens = tokens;
    let segment = Segment::create_live(draft, "content");
    // last_referenced_turn startet bei created_turn; für abweichende Werte via mark_referenced
    // (nur auf Working erlaubt — Static-Testfälle nutzen last_referenced_turn = 0).
    let mut segment = segment;
    if last_referenced_turn > 0 && region == Region::Working {
        segment.mark_referenced(last_referenced_turn).unwrap();
    }
    segment
}

// AC2: Clean-Page-Eviction → im Plan OHNE Externalisierungs-Kandidat/Blob; origin bleibt.
#[test]
fn clean_page_plant_refetchable_ohne_externalisierung() {
    let skill = seg("skill_content", Region::Working, true, false, Some("skill://foo"), None, 5000, 0, 1);

    let plan = plan_full(&[skill.clone()], &PolicyConfig::default_policy(), 20);

    assert!(plan.externalization_candidates.is_empty());
    assert!(plan.unit_evicted.is_empty());
    assert_eq!(plan.clean_page_evicted, vec![skill.id()]);
    // Der Collector mutiert nicht: Segment bleibt live, origin bleibt im Audit-Trail (§3.2.1).
    assert_eq!(skill.state(), SegmentState::Live);
    assert_eq!(skill.origin(), Some("skill://foo"));
}

// AC4: TTL-Eviction respektiert pinned und Static — beide werden NIE geplant.
#[test]
fn ttl_eviction_verschont_pinned_und_static() {
    let pinned = seg("tool_result", Region::Working, false, true, None, None, 0, 0, 1);
    let stat = seg("tool_result", Region::Static, false, false, None, None, 0, 0, 2);

    let plan = plan_full(
        &[pinned, stat],
        &PolicyConfig::default_policy(),
        50,
    );

    assert!(plan.is_empty());
}

// AC5: Unit-Kopplung — Externalisierung zielt nur auf das tool_result, der tool_call bleibt.
#[test]
fn externalisierung_zielt_nur_auf_tool_result() {
    let call = seg("tool_call", Region::Working, false, false, None, Some("tc-1"), 5000, 10, 1);
    let result = seg("tool_result", Region::Working, false, false, None, Some("tc-1"), 5000, 10, 2);

    let plan = plan_full(
        &[call.clone(), result.clone()],
        &PolicyConfig::default_policy(),
        10,
    );

    assert_eq!(plan.externalization_candidates.len(), 1);
    assert_eq!(plan.externalization_candidates[0].segment_id, result.id());
    assert!(plan.clean_page_evicted.is_empty());
    assert!(plan.unit_evicted.is_empty());
}

// AC5: TTL-Eviction einer gekoppelten Unit entfernt BEIDE Segmente in EINER EvictedUnit.
#[test]
fn ttl_eviction_einer_unit_entfernt_beide_segmente() {
    let call = seg("tool_call", Region::Working, false, false, None, Some("tc-9"), 10, 0, 1);
    let result = seg("tool_result", Region::Working, false, false, None, Some("tc-9"), 10, 0, 2);

    let plan = plan_full(
        &[call.clone(), result.clone()],
        &PolicyConfig::default_policy(),
        20,
    );

    assert_eq!(plan.unit_evicted.len(), 1);
    let unit = &plan.unit_evicted[0];
    assert_eq!(unit.unit_id, "tc-9");
    assert!(unit.segment_ids.contains(&call.id()));
    assert!(unit.segment_ids.contains(&result.id()));
    assert!(plan.clean_page_evicted.is_empty());
}

// AC5: Eine Unit mit pinned-Segment bleibt vollständig erhalten (kein Teil-Evict).
#[test]
fn ttl_eviction_ueberspringt_unit_mit_pinned_segment() {
    let call = seg("tool_call", Region::Working, false, true, None, Some("tc-7"), 0, 0, 1);
    let result = seg("tool_result", Region::Working, false, false, None, Some("tc-7"), 0, 0, 2);

    let plan = plan_full(&[call, result], &PolicyConfig::default_policy(), 20);

    assert!(plan.is_empty());
}

// Emergency: keine Externalisierung (Phase 2 ausgelassen), nur Clean-Page + TTL (Spec §3.1).
#[test]
fn emergency_ueberspringt_externalisierung() {
    // Externalisierungs-Kandidat (nicht-refetchable, groß, Kind externalize=true), TTL NICHT
    // überschritten (frisch referenziert) — bliebe in plan_full ein reiner Phase-2-Kandidat.
    let big_result = seg("tool_result", Region::Working, false, false, None, Some("tc-2"), 5000, 50, 1);
    let big_call = seg("tool_call", Region::Working, false, false, None, Some("tc-2"), 10, 50, 2);
    // Clean-Page-Kandidat (refetchable, TTL überschritten).
    let skill = seg("skill_content", Region::Working, true, false, Some("skill://x"), None, 0, 0, 3);
    // TTL-Kandidat (nicht gekoppelt, TTL überschritten).
    let old_user = seg("user_msg", Region::Working, false, false, None, None, 0, 0, 4);

    let plan = collect_emergency_no_io(
        &[big_result, big_call, skill.clone(), old_user.clone()],
        &PolicyConfig::default_policy(),
        50,
    );

    // Phase 2 ausgelassen: das große, nicht abgelaufene tool_result ist kein Kandidat.
    assert!(plan.externalization_candidates.is_empty());
    // Clean-Page + TTL wurden geplant: skill ist Clean-Page, old_user eine Single-Segment-Unit.
    assert_eq!(plan.clean_page_evicted, vec![skill.id()]);
    assert_eq!(plan.unit_evicted.len(), 1);
    assert_eq!(plan.unit_evicted[0].segment_ids, vec![old_user.id()]);
}

// plan_full liefert im selben Szenario zusätzlich den Externalisierungs-Kandidaten.
#[test]
fn plan_full_liefert_zusaetzlich_externalisierung() {
    let big_result = seg("tool_result", Region::Working, false, false, None, Some("tc-2"), 5000, 50, 1);
    let big_call = seg("tool_call", Region::Working, false, false, None, Some("tc-2"), 10, 50, 2);

    let plan = plan_full(
        &[big_result.clone(), big_call],
        &PolicyConfig::default_policy(),
        50,
    );

    assert_eq!(plan.externalization_candidates.len(), 1);
    assert_eq!(plan.externalization_candidates[0].segment_id, big_result.id());
    // Spec §3.2.2: summary = erste 200 Zeichen + „…" (Content hier kürzer ⇒ unverändert).
    assert_eq!(plan.externalization_candidates[0].summary.as_deref(), Some("content"));
}

// Spec §3.2.2: lange Inhalte werden auf 200 Zeichen + Ellipse gekürzt.
#[test]
fn externalisierung_kuerzt_summary_auf_200_zeichen() {
    let long_content = "x".repeat(500);
    let mut draft = SegmentDraft::new(
        Ulid::new(),
        Ulid::new(),
        Region::Working,
        "tool_result",
        Some(Role::Tool),
        1,
        0,
    );
    draft.tokens = 5000;
    let segment = Segment::create_live(draft, &long_content);

    let plan = plan_full(&[segment], &PolicyConfig::default_policy(), 0);

    let summary = plan.externalization_candidates[0].summary.as_deref().unwrap();
    assert_eq!(summary.chars().count(), 201);
    assert!(summary.ends_with('…'));
}
