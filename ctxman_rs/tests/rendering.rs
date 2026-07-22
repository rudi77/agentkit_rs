//! Ports der C#-Tests `RenderGoldenTests` (Golden-File-Determinismus, Spec §4.6 / I4) und
//! `RenderPlannerTests`-Kernfälle (I3-Filter, I4-Sortierung, Scope, Coalescing, I5).

use ctxman::domain::{PolicyConfig, Region, RenderScope, Role, Segment, SegmentDraft};
use ctxman::rendering::{adapter_for, canonical_json, planner, static_prefix, RenderPlanResult};
use ulid::Ulid;

fn fixture_session_id() -> Ulid {
    Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()
}

#[allow(clippy::too_many_arguments)]
fn live(
    id: &str,
    region: Region,
    kind: &str,
    role: Option<Role>,
    content: &str,
    seq: i64,
    source: Option<&str>,
    tokens: u32,
) -> Segment {
    Segment::create_live(
        SegmentDraft {
            source: source.map(str::to_string),
            tokens,
            ..SegmentDraft::new(
                Ulid::from_string(id).unwrap(),
                fixture_session_id(),
                region,
                kind,
                role,
                seq,
                0,
            )
        },
        content,
    )
}

/// Fixture aus `RenderGoldenTests.cs`: system_prompt + 2 tool_defs (sources core/mcp:github)
/// + user/assistant-Messages mit festen ULIDs, Seqs und Tokens.
fn fixture_segments() -> Vec<Segment> {
    vec![
        live(
            "01ARZ3NDEKTSV4RRFFQ69G5FAW",
            Region::Static,
            "system_prompt",
            Some(Role::System),
            "You are a test agent.",
            0,
            Some("core"),
            5,
        ),
        live(
            "01ARZ3NDEKTSV4RRFFQ69G5FAX",
            Region::Static,
            "tool_def",
            None,
            r#"{"name":"git","description":"Git tool"}"#,
            1,
            Some("core"),
            8,
        ),
        live(
            "01ARZ3NDEKTSV4RRFFQ69G5FAY",
            Region::Static,
            "tool_def",
            None,
            r#"{"name":"search","description":"Search"}"#,
            2,
            Some("mcp:github"),
            8,
        ),
        live(
            "01ARZ3NDEKTSV4RRFFQ69G5FAZ",
            Region::Working,
            "user_msg",
            Some(Role::User),
            "Hello",
            3,
            None,
            2,
        ),
        live(
            "01ARZ3NDEKTSV4RRFFQ69G5FB0",
            Region::Working,
            "assistant_msg",
            Some(Role::Assistant),
            "Hi there",
            4,
            None,
            3,
        ),
    ]
}

fn plan(segments: &[Segment]) -> RenderPlanResult {
    planner::plan(
        segments,
        &PolicyConfig::default_policy(),
        RenderScope::Path,
        &[],
        None,
    )
}

fn golden(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(path)
        .expect("Golden-Datei lesbar")
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

#[test]
fn render_output_entspricht_golden_file() {
    // Spec §4.6 / I4 (WP3-Akzeptanz 9): byte-genauer Vergleich gegen die C#-Golden-Fixtures.
    for provider in ["anthropic", "openai"] {
        let result = plan(&fixture_segments());
        let adapter = adapter_for(provider).unwrap();
        let rendered = adapter.render(&result.model);
        let canonical = canonical_json::serialize(&rendered.request_fragment);
        assert_eq!(
            canonical,
            golden(&format!("render-{provider}.json")),
            "Provider {provider}"
        );
    }
}

#[test]
fn identischer_segment_stand_ergibt_gleichen_cache_prefix_hash() {
    // Akzeptanz 16 / I4: identischer Segment-Stand (auch reversed) ⇒ gleicher cache_prefix_hash.
    let segments = fixture_segments();
    let mut reversed = fixture_segments();
    reversed.reverse();

    let hash_a = canonical_json::compute_cache_prefix_hash(&static_prefix::build_hash_input(
        &plan(&segments).static_prefix,
    ));
    let hash_b = canonical_json::compute_cache_prefix_hash(&static_prefix::build_hash_input(
        &plan(&reversed).static_prefix,
    ));

    assert_eq!(hash_a, hash_b);
}

#[test]
fn toggle_off_on_stellt_byte_identischen_static_prefix_wieder_her() {
    // Akzeptanz 16: Toggle-Off/On einer Tool-Quelle ⇒ byte-identischer Static-Prefix.
    let all = fixture_segments();
    let without_github: Vec<Segment> = fixture_segments()
        .into_iter()
        .filter(|s| s.region() != Region::Static || s.source() != Some("mcp:github"))
        .collect();

    let hash_of = |segments: &[Segment]| {
        canonical_json::compute_cache_prefix_hash(&static_prefix::build_hash_input(
            &plan(segments).static_prefix,
        ))
    };

    let hash_original = hash_of(&all);
    let hash_without = hash_of(&without_github);
    assert_ne!(hash_original, hash_without);

    let hash_restored = hash_of(&fixture_segments());
    assert_eq!(hash_original, hash_restored);
}

// ---- Planner-Kernfälle (Port aus RenderPlannerTests) ----

#[test]
fn evicted_und_compacted_erscheinen_nie_im_output() {
    // Spec §2.2 I3.
    let mut segments = fixture_segments();
    segments[3].evict().unwrap();
    segments[4].compact(None).unwrap();

    let result = plan(&segments);
    assert!(result.model.messages.is_empty());
    // tokens_total zählt nur render-eligible Segmente.
    assert_eq!(result.tokens_total, 5 + 8 + 8);
}

#[test]
fn static_sortierung_ist_kanonisch_nie_insertion_order() {
    // Spec §4.6 / I4: (source, kind, content_hash) — nie Insertion-Order.
    let mut segments = fixture_segments();
    segments.swap(1, 2); // tool_defs vertauscht einfügen

    let result = plan(&segments);
    let sources: Vec<Option<&str>> = result
        .model
        .static_items
        .iter()
        .map(|i| i.source.as_deref())
        .collect();
    // "core" (system_prompt), "core" (git tool_def), "mcp:github" (search tool_def)
    assert_eq!(
        sources,
        vec![Some("core"), Some("core"), Some("mcp:github")]
    );
    let kinds: Vec<&str> = result
        .model
        .static_items
        .iter()
        .map(|i| i.kind.as_str())
        .collect();
    assert_eq!(kinds, vec!["system_prompt", "tool_def", "tool_def"]);
}

#[test]
fn working_sortiert_nach_seq_und_coalesced_gleiche_rollen() {
    // Spec §4.6 / I4: seq aufsteigend; benachbarte gleiche Rollen werden zu einer Message
    // mit mehreren Blocks zusammengefasst.
    let mut segments = fixture_segments();
    segments.push(live(
        "01ARZ3NDEKTSV4RRFFQ69G5FB1",
        Region::Working,
        "assistant_msg",
        Some(Role::Assistant),
        "Second thought",
        5,
        None,
        3,
    ));

    let result = plan(&segments);
    assert_eq!(result.model.messages.len(), 2); // user + (assistant coalesced)
    assert_eq!(result.model.messages[1].role, Role::Assistant);
    assert_eq!(result.model.messages[1].blocks.len(), 2);
}

#[test]
fn offene_tool_calls_werden_gemeldet_nicht_geworfen() {
    // Spec §2 I5: tool_call ohne render-eligibles tool_result ⇒ open_tool_call_ids.
    let mut segments = fixture_segments();
    segments.push(Segment::create_live(
        SegmentDraft {
            source: Some("git".to_string()),
            tool_call_id: Some("call_1".to_string()),
            ..SegmentDraft::new(
                Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FB2").unwrap(),
                fixture_session_id(),
                Region::Working,
                "tool_call",
                Some(Role::Assistant),
                6,
                0,
            )
        },
        r#"{"cmd":"status"}"#,
    ));

    let result = plan(&segments);
    assert_eq!(result.open_tool_call_ids, vec!["call_1".to_string()]);

    // Mit passendem tool_result ist die Unit vollständig.
    segments.push(Segment::create_live(
        SegmentDraft {
            tool_call_id: Some("call_1".to_string()),
            ..SegmentDraft::new(
                Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FB3").unwrap(),
                fixture_session_id(),
                Region::Working,
                "tool_result",
                Some(Role::Tool),
                7,
                0,
            )
        },
        "clean",
    ));
    let result = plan(&segments);
    assert!(result.open_tool_call_ids.is_empty());
}

#[test]
fn externalisierte_segmente_rendern_als_ref_hint() {
    // Spec §2.4: summary + Ref-Hinweis, nie der Roh-Content.
    let mut segments = fixture_segments();
    let id = segments[4].id();
    segments[4]
        .externalize(
            ctxman::domain::BlobRef {
                store: "fs".to_string(),
                key: "a".repeat(64),
                size_bytes: 8,
                content_type: "text/plain".to_string(),
            },
            Some("Hi t…".to_string()),
            1,
        )
        .unwrap();

    let result = plan(&segments);
    assert!(result.model.emit_expand_context_ref);
    let text = result.model.messages[1].blocks[0].text.as_deref().unwrap();
    assert_eq!(
        text,
        format!("Hi t…\n[content externalized — expand_context_ref(segment_id=\"{id}\")]")
    );
}

#[test]
fn scope_frame_rendert_gepinnte_root_plus_tip_frame() {
    // Spec §2.5 scope=frame: gepinnte Root-Segmente + Segmente des Tip-Frames.
    let frame_a = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FC0").unwrap();
    let frame_b = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FC1").unwrap();

    let mut segments = fixture_segments();
    // Gepinntes Root-Segment.
    let mut pinned_root = live(
        "01ARZ3NDEKTSV4RRFFQ69G5FB4",
        Region::Working,
        "task",
        Some(Role::User),
        "pinned task",
        10,
        None,
        3,
    );
    pinned_root.pin().unwrap();
    segments.push(pinned_root);
    // Segment in Frame A (nicht Tip).
    segments.push(Segment::create_live(
        SegmentDraft {
            frame_id: Some(frame_a),
            ..SegmentDraft::new(
                Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FB5").unwrap(),
                fixture_session_id(),
                Region::Working,
                "user_msg",
                Some(Role::User),
                11,
                0,
            )
        },
        "in frame a",
    ));
    // Segment in Frame B (Tip).
    segments.push(Segment::create_live(
        SegmentDraft {
            frame_id: Some(frame_b),
            ..SegmentDraft::new(
                Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FB6").unwrap(),
                fixture_session_id(),
                Region::Working,
                "user_msg",
                Some(Role::User),
                12,
                0,
            )
        },
        "in frame b",
    ));

    let open = [frame_a, frame_b];

    // scope=path: Root + alle offenen Frames.
    let path = planner::plan(
        &segments,
        &PolicyConfig::default_policy(),
        RenderScope::Path,
        &open,
        Some(frame_b),
    );
    let path_texts: Vec<&str> = path
        .model
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter().filter_map(|b| b.text.as_deref()))
        .collect();
    assert!(path_texts.contains(&"in frame a"));
    assert!(path_texts.contains(&"in frame b"));
    assert!(path_texts.contains(&"pinned task"));
    assert!(path_texts.contains(&"Hello"));

    // scope=frame: nur gepinnte Root-Segmente + Tip-Frame.
    let frame_scope = planner::plan(
        &segments,
        &PolicyConfig::default_policy(),
        RenderScope::Frame,
        &open,
        Some(frame_b),
    );
    let frame_texts: Vec<&str> = frame_scope
        .model
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter().filter_map(|b| b.text.as_deref()))
        .collect();
    assert_eq!(frame_texts, vec!["pinned task", "in frame b"]);
}

#[test]
fn geschlossene_frames_werden_im_path_scope_ausgeblendet() {
    // Spec §2.5: Segmente eines nicht (mehr) offenen Frames erscheinen nicht im Path-Scope.
    let closed_frame = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FC2").unwrap();
    let mut segments = fixture_segments();
    segments.push(Segment::create_live(
        SegmentDraft {
            frame_id: Some(closed_frame),
            ..SegmentDraft::new(
                Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FB7").unwrap(),
                fixture_session_id(),
                Region::Working,
                "user_msg",
                Some(Role::User),
                20,
                0,
            )
        },
        "hidden",
    ));

    let result = plan(&segments); // keine offenen Frames
    let texts: Vec<&str> = result
        .model
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter().filter_map(|b| b.text.as_deref()))
        .collect();
    assert!(!texts.contains(&"hidden"));
}
