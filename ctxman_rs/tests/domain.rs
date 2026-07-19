//! Ports der C#-Tests `SegmentInvariantTests`, `SessionVersionTests`, `RetentionConfigTests`:
//! Invarianten I1–I3 (Spec §2.2), Monotonie von Version/Turn/Epoche (Spec §4.4/§4.2),
//! Dauer-Parsing (Spec §7.1).

use std::time::Duration;

use ctxman::domain::{
    parse_duration, BlobRef, PolicyConfig, Region, Role, Segment, SegmentDraft, SegmentState,
    Session, SessionStatus,
};
use ctxman::CtxmanError;
use ulid::Ulid;

fn static_segment() -> Segment {
    Segment::create_live(
        SegmentDraft {
            source: Some("core".to_string()),
            ..SegmentDraft::new(
                Ulid::new(),
                Ulid::new(),
                Region::Static,
                "system_prompt",
                Some(Role::System),
                1,
                0,
            )
        },
        "you are an agent",
    )
}

fn working_segment(content: &str) -> Segment {
    Segment::create_live(
        SegmentDraft::new(
            Ulid::new(),
            Ulid::new(),
            Region::Working,
            "tool_result",
            Some(Role::Tool),
            2,
            1,
        ),
        content,
    )
}

fn sample_blob() -> BlobRef {
    BlobRef {
        store: "local".to_string(),
        key: "sha256:abc".to_string(),
        size_bytes: 1234,
        content_type: "text/plain".to_string(),
    }
}

// ---- I1: Static-Segmente sind immutable (Spec §2.2 / §4.2) ----

#[test]
fn mutationen_auf_static_segment_schlagen_fehl() {
    // Spec §2.2 I1: externalize/evict/compact/mark_referenced/pin/unpin sind auf Static verboten.
    let mut seg = static_segment();
    assert!(matches!(
        seg.externalize(sample_blob(), Some("summary".to_string())),
        Err(CtxmanError::StaticRegionImmutable { .. })
    ));
    assert!(matches!(seg.evict(), Err(CtxmanError::StaticRegionImmutable { .. })));
    assert!(matches!(
        seg.compact(Some("short".to_string())),
        Err(CtxmanError::StaticRegionImmutable { .. })
    ));
    assert!(matches!(seg.mark_referenced(5), Err(CtxmanError::StaticRegionImmutable { .. })));
    assert!(matches!(seg.pin(), Err(CtxmanError::StaticRegionImmutable { .. })));
    assert!(matches!(seg.unpin(), Err(CtxmanError::StaticRegionImmutable { .. })));
}

#[test]
fn static_segment_bleibt_nach_abgelehnten_mutationen_unveraendert() {
    // Spec §2.2 I1: keine der Mutationen verändert ein Static-Segment.
    let mut seg = static_segment();
    let _ = seg.evict();
    assert_eq!(seg.state(), SegmentState::Live);
    assert_eq!(seg.content(), Some("you are an agent"));
    assert!(seg.blob_ref().is_none());
}

// ---- I2: live | externalized nie content==None && blob_ref==None (Spec §2.2) ----

#[test]
fn konstruktion_live_ohne_content_und_blob_ref_schlaegt_fehl() {
    let draft = SegmentDraft::new(
        Ulid::new(),
        Ulid::new(),
        Region::Working,
        "user_msg",
        Some(Role::User),
        1,
        0,
    );
    let result = Segment::new(draft, None, None, 0, SegmentState::Live);
    assert!(matches!(
        result,
        Err(CtxmanError::SegmentContentInvariant { state: "live" })
    ));
}

#[test]
fn konstruktion_externalized_ohne_content_und_blob_ref_schlaegt_fehl() {
    let draft = SegmentDraft {
        summary: Some("summary".to_string()),
        ..SegmentDraft::new(
            Ulid::new(),
            Ulid::new(),
            Region::Working,
            "tool_result",
            Some(Role::Tool),
            1,
            0,
        )
    };
    let result = Segment::new(draft, None, None, 0, SegmentState::Externalized);
    assert!(matches!(
        result,
        Err(CtxmanError::SegmentContentInvariant { state: "externalized" })
    ));
}

#[test]
fn konstruktion_live_mit_content_gelingt() {
    let seg = working_segment("hello");
    assert_eq!(seg.content(), Some("hello"));
    assert!(seg.blob_ref().is_none());
    assert_eq!(seg.state(), SegmentState::Live);
}

#[test]
fn konstruktion_externalized_mit_blob_ref_gelingt() {
    let draft = SegmentDraft {
        summary: Some("summary".to_string()),
        ..SegmentDraft::new(
            Ulid::new(),
            Ulid::new(),
            Region::Working,
            "tool_result",
            Some(Role::Tool),
            1,
            0,
        )
    };
    let seg = Segment::create_externalized(draft, sample_blob());
    assert!(seg.content().is_none());
    assert!(seg.blob_ref().is_some());
    assert_eq!(seg.state(), SegmentState::Externalized);
}

#[test]
fn konstruktion_evicted_ohne_content_und_blob_ref_gelingt() {
    // Spec §2.2 I2: evicted/compacted dürfen beide None sein (Soft-Delete erhält Metadaten).
    let draft = SegmentDraft::new(
        Ulid::new(),
        Ulid::new(),
        Region::Working,
        "tool_result",
        Some(Role::Tool),
        1,
        0,
    );
    let seg = Segment::new(draft, None, None, 0, SegmentState::Evicted);
    assert!(seg.is_ok());
}

// ---- I1/I2-Folgeverhalten der Guard-Methoden ----

#[test]
fn externalize_setzt_blob_ref_und_loescht_content() {
    // Spec §3.2 Schritt 2: content := None, blob_ref := ref, state := externalized; I2 gewahrt.
    let mut seg = working_segment("big tool output");
    seg.externalize(sample_blob(), Some("short".to_string())).unwrap();
    assert_eq!(seg.state(), SegmentState::Externalized);
    assert!(seg.content().is_none());
    assert!(seg.blob_ref().is_some());
    assert_eq!(seg.summary(), Some("short"));
}

#[test]
fn evict_ist_soft_delete_und_erhaelt_daten() {
    // Spec §2.2 I3: Eviction ist reiner Zustandsübergang — Audit-Daten bleiben.
    let mut seg = working_segment("tool output");
    seg.evict().unwrap();
    assert_eq!(seg.state(), SegmentState::Evicted);
    assert_eq!(seg.content(), Some("tool output"));
}

#[test]
fn compact_aktualisiert_summary_optional() {
    let mut seg = working_segment("long history");
    seg.compact(Some("kurz".to_string())).unwrap();
    assert_eq!(seg.state(), SegmentState::Compacted);
    assert_eq!(seg.summary(), Some("kurz"));

    let mut seg2 = working_segment("long history");
    seg2.compact(None).unwrap();
    assert_eq!(seg2.state(), SegmentState::Compacted);
    assert_eq!(seg2.summary(), None);
}

// ---- Session: Monotonie von Version, Turn, Epoche (Port von SessionVersionTests) ----

#[test]
fn session_version_turn_epoche_sind_monoton() {
    let mut session = Session::new(Ulid::new(), None, PolicyConfig::default_policy(), 0);
    assert_eq!(session.context_version(), 0);
    assert_eq!(session.current_turn(), 0);
    assert_eq!(session.static_epoch(), 0);

    session.increment_version(10);
    session.increment_version(20);
    assert_eq!(session.context_version(), 2); // Spec §4.4

    session.advance_turn(30);
    assert_eq!(session.current_turn(), 1); // Spec §4.4

    session.bump_static_epoch(40);
    assert_eq!(session.static_epoch(), 1); // Spec §4.2
    // Epoch-Bump erhöht bewusst NICHT die context_version.
    assert_eq!(session.context_version(), 2);
    assert_eq!(session.updated_at(), 40);
}

#[test]
fn session_archive_setzt_status() {
    let mut session = Session::new(Ulid::new(), None, PolicyConfig::default_policy(), 0);
    assert_eq!(session.status(), SessionStatus::Active);
    session.archive(50);
    assert_eq!(session.status(), SessionStatus::Archived); // Spec §4.3
}

// ---- RetentionConfig / parse_duration (Port von RetentionConfigTests, Spec §7.1) ----

#[test]
fn parse_duration_mit_suffix() {
    assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(24 * 3600));
    assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(90 * 60));
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(2 * 86_400));
    assert_eq!(parse_duration("1.5h").unwrap(), Duration::from_secs(5400));
}

#[test]
fn parse_duration_ohne_suffix_ist_stunden() {
    assert_eq!(parse_duration("24").unwrap(), Duration::from_secs(24 * 3600));
    assert_eq!(parse_duration("0.5").unwrap(), Duration::from_secs(1800));
}

#[test]
fn parse_duration_ungueltig_schlaegt_fehl() {
    assert!(matches!(parse_duration(""), Err(CtxmanError::InvalidDuration(_))));
    assert!(matches!(parse_duration("abc"), Err(CtxmanError::InvalidDuration(_))));
    assert!(matches!(parse_duration("24x"), Err(CtxmanError::InvalidDuration(_))));
    assert!(matches!(parse_duration("-1h"), Err(CtxmanError::InvalidDuration(_))));
}

#[test]
fn retention_config_liefert_spans() {
    let policy = PolicyConfig::default_policy();
    assert_eq!(policy.retention.blob_grace(), Duration::from_secs(72 * 3600));
    assert_eq!(
        policy.retention.sweep_interval_span().unwrap(),
        Duration::from_secs(24 * 3600)
    );
}
