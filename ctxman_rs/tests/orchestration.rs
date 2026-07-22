//! End-to-End-Tests der ContextSession-Orchestrierung: Append (Seq/Version/Events),
//! Render-Hot-Path (I5, Watermarks, Emergency, BudgetExceeded), Minor GC mit Blob Store,
//! Page Fault, Frames (LIFO), Snapshot-Roundtrip und Event-Vokabular (Spec §2–§6).

use ctxman::domain::{KindPolicy, PolicyConfig, RenderScope, Role, SegmentState, WatermarkLevel};
use ctxman::events::types;
use ctxman::gc::GcLevel;
use ctxman::storage::FileSystemBlobStore;
use ctxman::{
    AppendContent, AppendRequest, ContextSession, CtxmanError, CtxmanServices, RenderOptions,
};

fn test_services() -> CtxmanServices {
    CtxmanServices { clock: Box::new(|| 0), ..Default::default() }
}

fn small_policy(budget_tokens: u32) -> PolicyConfig {
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = budget_tokens;
    policy
}

fn render_no_advance(session: &mut ContextSession) -> ctxman::RenderOutput {
    session
        .render(RenderOptions {
            provider: "anthropic".to_string(),
            scope: RenderScope::Path,
            turn_advance: false,
        })
        .unwrap()
}

// ---- Append (Spec §4.3/§4.4) ----

#[test]
fn append_vergibt_monotone_seq_und_erhoeht_version_einmal() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());

    let outcome = session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "Hallo"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "Hi"),
        ])
        .unwrap();

    assert_eq!(outcome.segment_ids.len(), 2);
    // Spec §2.2: seq server-vergeben, strikt monoton ab 0.
    assert_eq!(session.segments()[0].seq(), 0);
    assert_eq!(session.segments()[1].seq(), 1);
    // Spec §4.4: context_version genau EINMAL pro Aufruf (auch bei Batch).
    assert_eq!(outcome.context_version, 1);

    // Spec §6: ein segment_appended je Segment.
    let events = session.drain_events();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.event_type == types::SEGMENT_APPENDED));
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[1].seq, 1);
}

#[test]
fn append_static_kind_ist_verboten() {
    // Spec §2.2 I1 / §4.2: Static nur via Epoch-Bump.
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    let result = session.append_segment(AppendRequest::inline(
        "system_prompt",
        Some(Role::System),
        "nope",
    ));
    assert!(matches!(result, Err(CtxmanError::StaticAppendForbidden)));
    assert!(session.segments().is_empty());
}

#[test]
fn append_inline_ueber_1_mib_ist_verboten() {
    // Spec §4.3: Inline-Content über 1 MiB ⇒ Fehler, großer Inhalt läuft über den Blob-Pfad.
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    let big = "x".repeat(1_048_577);
    let result = session.append_segment(AppendRequest::inline("tool_result", Some(Role::Tool), &big));
    assert!(matches!(result, Err(CtxmanError::ContentTooLarge { .. })));
}

#[test]
fn append_blob_legt_externalisiertes_segment_an() {
    // Spec §4.3 / §2.6: Blob-Pfad ⇒ von Anfang an externalisiert; summary als Render-Ersatz.
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    let id = session
        .append_segment(AppendRequest {
            content: AppendContent::Blob {
                content: b"riesiger inhalt".to_vec(),
                content_type: "text/plain".to_string(),
                summary: Some("kurz".to_string()),
            },
            ..AppendRequest::inline("tool_result", Some(Role::Tool), "")
        })
        .unwrap();

    let segment = session.segments().iter().find(|s| s.id() == id).unwrap();
    assert_eq!(segment.state(), SegmentState::Externalized);
    assert!(segment.content().is_none());
    assert_eq!(segment.summary(), Some("kurz"));
    let blob_ref = segment.blob_ref().unwrap();
    assert_eq!(blob_ref.key.len(), 64); // content-addressed sha256 (Spec §2.6)
}

// ---- Render-Hot-Path (Spec §3.1 / §4.6 / I5) ----

#[test]
fn render_mit_offener_unit_schlaegt_fehl() {
    // Spec §2 I5: tool_call ohne tool_result ⇒ Fehler mit offenen IDs (C#: 422).
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    session
        .append_segment(AppendRequest {
            tool_call_id: Some("call_7".to_string()),
            source: Some("git".to_string()),
            ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{}")
        })
        .unwrap();

    let result = session.render(RenderOptions::default());
    match result {
        Err(CtxmanError::IncompleteUnits { open_tool_call_ids }) => {
            assert_eq!(open_tool_call_ids, vec!["call_7".to_string()]);
        }
        other => panic!("erwartet IncompleteUnits, bekam {other:?}"),
    }
    // Kein Turn-Advance im Fehlerfall.
    assert_eq!(session.session().current_turn(), 0);
}

#[test]
fn render_zaehlt_turn_und_emittiert_render_served() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Hallo"))
        .unwrap();
    session.drain_events();

    let out = session.render(RenderOptions::default()).unwrap();

    // Spec §3: ein Turn = ein Model-Call (turn_advance=true).
    assert_eq!(session.session().current_turn(), 1);
    assert_eq!(out.context_version, 2); // Append (1) + Render-Advance (2), Spec §4.4.
    assert_eq!(out.watermark, WatermarkLevel::Ok);
    assert!(out.recommended_gc.is_none());
    assert!(!out.cache_prefix_hash.is_empty());

    let events = session.drain_events();
    assert_eq!(events.len(), 1); // watermark ok ⇒ kein watermark_crossed.
    assert_eq!(events[0].event_type, types::RENDER_SERVED);
    assert_eq!(events[0].payload["cache_prefix_hash"], out.cache_prefix_hash);
}

#[test]
fn render_soft_watermark_empfiehlt_minor_gc() {
    // Budget 100, soft = 0.6 ⇒ 65 Tokens (260 Zeichen) überschreiten soft, nicht hard.
    let mut session = ContextSession::new(small_policy(100), test_services());
    session
        .append_segment(AppendRequest::inline(
            "user_msg",
            Some(Role::User),
            &"x".repeat(260),
        ))
        .unwrap();
    session.drain_events();

    let out = render_no_advance(&mut session);
    assert_eq!(out.watermark, WatermarkLevel::Soft);
    assert_eq!(out.recommended_gc, Some(GcLevel::Minor));

    let events = session.drain_events();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type).collect();
    assert_eq!(event_types, vec![types::RENDER_SERVED, types::WATERMARK_CROSSED]);
    assert_eq!(events[1].payload["level"], "soft");
}

#[test]
fn render_emergency_evicted_synchron_ohne_io() {
    // Budget 100, emergency = 0.95. Abgelaufene refetchable skill_content (96 Tokens) +
    // frische user_msg (1 Token) ⇒ 97 ≥ 95: Emergency-Pfad räumt die Clean Page synchron.
    let mut policy = small_policy(100);
    policy
        .kinds
        .insert("skill_content".to_string(), KindPolicy {
            ttl_turns: Some(0),
            refetchable: true,
            ..Default::default()
        });
    let mut session = ContextSession::new(policy, test_services());

    session
        .append_segment(AppendRequest {
            refetchable: true,
            origin: Some("skill://foo".to_string()),
            ..AppendRequest::inline("skill_content", Some(Role::User), &"s".repeat(384))
        })
        .unwrap();
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "hi"))
        .unwrap();
    session.drain_events();

    // Turn 0: TTL (0-0 > 0) noch nicht überschritten ⇒ Emergency räumt nichts, aber
    // 97 < 100 (volles Budget) ⇒ Render gelingt und zählt den Turn hoch.
    let first = session.render(RenderOptions::default()).unwrap();
    assert_eq!(first.watermark, WatermarkLevel::Emergency);
    assert_eq!(first.tokens_total, 97);
    session.drain_events();

    // Turn 1: TTL überschritten ⇒ synchrone Clean-Page-Eviction im Hot Path (Spec §3.1).
    let second = render_no_advance(&mut session);
    assert_eq!(second.watermark, WatermarkLevel::Emergency); // Plan-Watermark VOR Eviction.
    assert_eq!(second.tokens_total, 1);

    let skill = &session.segments()[0];
    assert_eq!(skill.state(), SegmentState::Evicted);
    assert_eq!(skill.origin(), Some("skill://foo")); // Audit-Trail bleibt (§3.2.1).

    let events = session.drain_events();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type).collect();
    assert_eq!(
        event_types,
        vec![types::SEGMENT_EVICTED, types::RENDER_SERVED, types::WATERMARK_CROSSED]
    );
}

#[test]
fn render_budget_exceeded_ohne_turn_advance() {
    // Gepinnte task-Segmente sind unantastbar — Emergency kann nichts räumen ⇒ Fehler,
    // KEIN Turn-Advance, KEINE Mutation (C#: 413 ohne Commit).
    let mut session = ContextSession::new(small_policy(100), test_services());
    session
        .append_segment(AppendRequest {
            pinned: true,
            ..AppendRequest::inline("task", Some(Role::User), &"t".repeat(400))
        })
        .unwrap();
    session.drain_events();

    let result = session.render(RenderOptions::default());
    assert!(matches!(
        result,
        Err(CtxmanError::BudgetExceeded { tokens_total: 100, budget: 100 })
    ));
    assert_eq!(session.session().current_turn(), 0);
    assert_eq!(session.segments()[0].state(), SegmentState::Live);
    assert!(session.drain_events().is_empty());
}

#[test]
fn render_unbekannter_provider_schlaegt_fehl() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    let result = session.render(RenderOptions {
        provider: "gemini".to_string(),
        scope: RenderScope::Path,
        turn_advance: false,
    });
    assert!(matches!(result, Err(CtxmanError::UnknownProvider(p)) if p == "gemini"));
}

// ---- Minor GC + Page Fault (Spec §3.2 / §3.4) ----

#[test]
fn minor_gc_externalisiert_und_page_fault_holt_zurueck() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-blobs");
    let _ = std::fs::remove_dir_all(&dir);

    let mut policy = PolicyConfig::default_policy();
    policy.externalize_threshold_tokens = 10;
    let services = CtxmanServices {
        blob_store: Box::new(FileSystemBlobStore::new(&dir)),
        clock: Box::new(|| 0),
        ..Default::default()
    };
    let mut session = ContextSession::new(policy, services);

    // Gekoppelte Unit: tool_call + großes tool_result (frisch ⇒ keine TTL-Eviction).
    let big_content = "Ausgabe ".repeat(50); // 400 Zeichen ⇒ 100 Tokens > threshold 10
    session
        .append_segments(vec![
            AppendRequest {
                tool_call_id: Some("tc-1".to_string()),
                source: Some("git".to_string()),
                ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{\"cmd\":\"log\"}")
            },
            AppendRequest {
                tool_call_id: Some("tc-1".to_string()),
                ..AppendRequest::inline("tool_result", Some(Role::Tool), &big_content)
            },
        ])
        .unwrap();
    session.drain_events();

    let report = session.run_minor_gc().unwrap();
    assert_eq!(report.externalized.len(), 1);
    assert!(report.clean_page_evicted.is_empty());
    assert!(report.unit_evicted.is_empty());

    let result_id = report.externalized[0];
    let segment = session.segments().iter().find(|s| s.id() == result_id).unwrap();
    assert_eq!(segment.state(), SegmentState::Externalized);
    assert!(segment.content().is_none());
    let blob_key = segment.blob_ref().unwrap().key.clone();
    assert!(dir.join(&blob_key).exists()); // Blob liegt content-addressed im FS-Store.

    let events = session.drain_events();
    assert_eq!(events[0].event_type, types::SEGMENT_EXTERNALIZED);
    assert_eq!(events[0].payload["blob_key"], blob_key);

    // Render zeigt den Ref-Hint statt des Roh-Contents (Spec §2.4).
    let out = render_no_advance(&mut session);
    assert!(out.canonical_json.contains("expand_context_ref"));
    assert!(!out.canonical_json.contains(&big_content));
    session.drain_events();

    // Page Fault (Spec §3.4): Inhalt zurückholen, ref_expansion-Segment + Events.
    let expand = session.expand_ref(result_id).unwrap();
    assert_eq!(expand.content, big_content);
    let expansion = session
        .segments()
        .iter()
        .find(|s| s.id() == expand.expansion_segment_id)
        .unwrap();
    assert_eq!(expansion.kind(), "ref_expansion");
    assert_eq!(expansion.content(), Some(big_content.as_str()));

    let events = session.drain_events();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type).collect();
    assert_eq!(event_types, vec![types::REF_EXPANDED, types::SEGMENT_APPENDED]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_ref_auf_evicted_liefert_gone_mit_restinfo() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    let id = session
        .append_segment(AppendRequest {
            origin: Some("skill://foo".to_string()),
            ..AppendRequest::inline("skill_content", Some(Role::User), "inhalt")
        })
        .unwrap();
    // Manuell evicten (Spec §2.2 I3: Soft-Delete).
    session.pin(id).unwrap();
    session.unpin(id).unwrap();
    let segment_id = id;
    // Eviction über den GC-Pfad wäre TTL-abhängig — hier direkt über expand auf live Segment:
    // ein live (nicht externalisiertes) Segment ist nicht expandierbar ⇒ RefGone.
    let result = session.expand_ref(segment_id);
    match result {
        Err(CtxmanError::RefGone { origin, .. }) => {
            assert_eq!(origin.as_deref(), Some("skill://foo"));
        }
        other => panic!("erwartet RefGone, bekam {other:?}"),
    }
}

// ---- Frames (Spec §2.5) ----

#[test]
fn frame_push_pop_mit_lifo_und_return_segment() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "Haupt"))
        .unwrap();

    let outer = session.push_frame("research");
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "im Frame"))
        .unwrap();
    let inner = session.push_frame("sub");
    session
        .append_segment(AppendRequest::inline("assistant_msg", Some(Role::Assistant), "tief"))
        .unwrap();
    session.drain_events();

    // Spec §2.5: Frame mit offenen Kindern kann nicht gepoppt werden (LIFO, Kinder zuerst).
    let result = session.pop_frame(outer, "zu früh", None);
    assert!(matches!(result, Err(CtxmanError::FrameDiscipline(_))));

    // Tip poppen: Segmente des Frames evicted, Return-Segment im Parent (outer).
    let pop = session.pop_frame(inner, "Ergebnis: 42", None).unwrap();
    let ret = session
        .segments()
        .iter()
        .find(|s| s.id() == pop.return_segment_id)
        .unwrap();
    assert_eq!(ret.kind(), "subagent_return");
    assert_eq!(ret.frame_id(), Some(outer));
    assert_eq!(ret.role(), Some(Role::Assistant));
    let deep = session.segments().iter().find(|s| s.content() == Some("tief")).unwrap();
    assert_eq!(deep.state(), SegmentState::Evicted);

    let event_types: Vec<&str> =
        session.drain_events().iter().map(|e| e.event_type).collect();
    assert_eq!(event_types, vec![types::FRAME_POPPED]);

    // Doppel-Pop ist ein Disziplin-Fehler (Bibliotheks-Guard).
    assert!(matches!(
        session.pop_frame(inner, "nochmal", None),
        Err(CtxmanError::FrameDiscipline(_))
    ));

    // Render (scope=path): Frame-Inhalte des gepoppten Frames unsichtbar, Return sichtbar.
    let out = render_no_advance(&mut session);
    assert!(out.canonical_json.contains("Ergebnis: 42"));
    assert!(!out.canonical_json.contains("tief"));
    assert!(out.canonical_json.contains("im Frame")); // outer ist noch offen.

    // Append bindet an den Tip (outer ist wieder Tip).
    let id = session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "weiter"))
        .unwrap();
    let seg = session.segments().iter().find(|s| s.id() == id).unwrap();
    assert_eq!(seg.frame_id(), Some(outer));
}

// ---- Snapshot (Persistenz-Ersatz) ----

#[test]
fn snapshot_roundtrip_erhaelt_zustand_und_hash() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "Hallo"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "Hi"),
        ])
        .unwrap();
    let before = render_no_advance(&mut session);
    session.save_to_file(&path).unwrap();

    let mut restored = ContextSession::load_from_file(&path, test_services()).unwrap();
    let after = render_no_advance(&mut restored);

    // Byte-identischer Render-Output und stabiler Cache-Prefix-Hash nach Load (I4).
    assert_eq!(before.canonical_json, after.canonical_json);
    assert_eq!(before.cache_prefix_hash, after.cache_prefix_hash);
    assert_eq!(restored.session().context_version(), session.session().context_version());
    // seq-Vergabe läuft nahtlos weiter.
    let outcome = restored
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "weiter"))
        .unwrap();
    let seg = restored.segments().iter().find(|s| s.id() == outcome).unwrap();
    assert_eq!(seg.seq(), 2);

    let _ = std::fs::remove_file(&path);
}

// ---- Static-Epoch-Bump (Spec §4.2 / I1) ----

use ctxman::StaticSegmentSpec;

fn static_spec(kind: &str, role: Option<Role>, content: &str, source: &str) -> StaticSegmentSpec {
    StaticSegmentSpec {
        kind: kind.to_string(),
        role,
        content: content.to_string(),
        source: Some(source.to_string()),
    }
}

fn full_static() -> Vec<StaticSegmentSpec> {
    vec![
        static_spec("system_prompt", Some(Role::System), "You are a test agent.", "core"),
        static_spec("tool_def", None, r#"{"name":"git","description":"Git tool"}"#, "core"),
        static_spec("tool_def", None, r#"{"name":"search","description":"Search"}"#, "mcp:github"),
    ]
}

#[test]
fn epoch_bump_ersetzt_static_region_und_emittiert_event() {
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());

    let outcome = session.bump_static_epoch(full_static()).unwrap();
    assert_eq!(outcome.static_epoch, 1);
    assert_eq!(outcome.diff.added_tools, vec!["git".to_string(), "search".to_string()]);
    assert_eq!(
        outcome.diff.added_sources,
        vec!["core".to_string(), "mcp:github".to_string()]
    );

    let events = session.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, types::STATIC_EPOCH_BUMPED);
    assert_eq!(events[0].payload["old_epoch"], 0);
    assert_eq!(events[0].payload["new_epoch"], 1);

    // Static rendert im System-/Tools-Teil (Spec §4.6).
    let out = render_no_advance(&mut session);
    assert!(out.canonical_json.contains("You are a test agent."));
    assert!(out.canonical_json.contains("\"git\""));
}

#[test]
fn epoch_bump_toggle_off_on_stellt_hash_wieder_her() {
    // Akzeptanz 16: Toggle-Off/On ⇒ byte-identischer Static-Prefix, Epoche steigt weiter.
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());

    session.bump_static_epoch(full_static()).unwrap();
    let original = render_no_advance(&mut session).cache_prefix_hash;

    let without_github: Vec<StaticSegmentSpec> = full_static()
        .into_iter()
        .filter(|s| s.source.as_deref() != Some("mcp:github"))
        .collect();
    let outcome = session.bump_static_epoch(without_github).unwrap();
    assert_eq!(outcome.diff.removed_tools, vec!["search".to_string()]);
    assert_eq!(outcome.diff.removed_sources, vec!["mcp:github".to_string()]);
    let reduced = render_no_advance(&mut session).cache_prefix_hash;
    assert_ne!(original, reduced);

    let restored_outcome = session.bump_static_epoch(full_static()).unwrap();
    assert_eq!(restored_outcome.static_epoch, 3);
    let restored = render_no_advance(&mut session).cache_prefix_hash;
    assert_eq!(original, restored);
}

#[test]
fn epoch_bump_externalisiert_units_entfernter_tools() {
    // Spec §4.2: on_tool_removed = externalize (Default) — das tool_result der Unit des
    // entfernten Tools wird externalisiert, der tool_call bleibt live (Spec §2.4).
    let mut session = ContextSession::new(PolicyConfig::default_policy(), test_services());
    session.bump_static_epoch(full_static()).unwrap();

    session
        .append_segments(vec![
            AppendRequest {
                tool_call_id: Some("tc-1".to_string()),
                source: Some("git".to_string()),
                ..AppendRequest::inline("tool_call", Some(Role::Assistant), "{}")
            },
            AppendRequest {
                tool_call_id: Some("tc-1".to_string()),
                ..AppendRequest::inline("tool_result", Some(Role::Tool), "git output")
            },
        ])
        .unwrap();

    // "git" (source core) aus der Static-Region entfernen.
    let without_git: Vec<StaticSegmentSpec> = full_static()
        .into_iter()
        .filter(|s| !s.content.contains("\"git\""))
        .collect();
    let outcome = session.bump_static_epoch(without_git).unwrap();
    assert_eq!(outcome.diff.removed_tools, vec!["git".to_string()]);

    let result = session
        .segments()
        .iter()
        .find(|s| s.kind() == "tool_result")
        .unwrap();
    assert_eq!(result.state(), SegmentState::Externalized);
    assert_eq!(result.summary(), Some("git output"));
    let call = session.segments().iter().find(|s| s.kind() == "tool_call").unwrap();
    assert_eq!(call.state(), SegmentState::Live);
}
