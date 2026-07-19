//! Ports der C#-`MajorCollectorTests` (Fensterwahl, Token-Cap, Pinned/Static-Ausschluss,
//! No-Op) plus `run_major_gc`-Orchestrierung mit Fake-CompactionModel (Promotion VOR
//! Compaction, Events, Fehlersemantik — Spec §3.3).

use std::sync::Mutex;

use ctxman::compaction::{CompactionModel, CompactionRequest, CompactionResult};
use ctxman::domain::{
    CompactionConfig, PolicyConfig, Region, Role, Segment, SegmentDraft, SegmentState,
};
use ctxman::events::types;
use ctxman::gc::plan_compaction;
use ctxman::promotion::VecPromotionSink;
use ctxman::{AppendRequest, ContextSession, CtxmanError, CtxmanServices};
use ulid::Ulid;

fn small_policy(budget_tokens: u32, max_share: f64) -> PolicyConfig {
    let mut policy = PolicyConfig::default_policy();
    policy.budget_tokens = budget_tokens;
    policy.compaction = CompactionConfig {
        model: "model".to_string(),
        prompt_template_id: "default-v1".to_string(),
        max_share,
    };
    policy
}

fn live_working(session_id: Ulid, tokens: u32, pinned: bool, seq: i64) -> Segment {
    let mut draft = SegmentDraft::new(
        Ulid::new(),
        session_id,
        Region::Working,
        "assistant_msg",
        Some(Role::Assistant),
        seq,
        0,
    );
    draft.tokens = tokens;
    draft.pinned = pinned;
    Segment::create_live(draft, "inhalt")
}

// AC8 (Spec §3.3): Fenster älteste→jüngste, akkumuliert bis max_share × budget; das Segment,
// das das Limit überschreiten würde, wird NICHT mehr aufgenommen.
#[test]
fn plan_compaction_waehlt_aelteste_bis_zum_token_cap() {
    let sid = Ulid::new();
    let policy = small_policy(1000, 0.5); // Limit = 500

    let seg1 = live_working(sid, 200, false, 1);
    let seg2 = live_working(sid, 200, false, 2);
    let seg3 = live_working(sid, 200, false, 3); // würde Cap überschreiten

    let plan = plan_compaction(&[seg1.clone(), seg2.clone(), seg3], &policy);

    assert!(!plan.is_no_op());
    assert_eq!(plan.source_segment_ids, vec![seg1.id(), seg2.id()]);
    assert_eq!(plan.tokens_before, 400);
    assert_eq!(plan.oldest_seq, 1);
}

// AC8: das erste Element wird immer aufgenommen, auch über dem Cap — bleibt dann aber allein
// und der Plan ist No-Op (< 2 Units).
#[test]
fn plan_compaction_erstes_element_immer_aufgenommen_dann_no_op() {
    let sid = Ulid::new();
    let policy = small_policy(100, 0.5); // Limit = 50

    let seg1 = live_working(sid, 200, false, 1);
    let seg2 = live_working(sid, 200, false, 2);

    let plan = plan_compaction(&[seg1, seg2], &policy);
    assert!(plan.is_no_op());
}

// AC8: oldest_seq = kleinste seq unter den ausgewählten Segmenten (Eingabe ungeordnet).
#[test]
fn plan_compaction_oldest_seq_ist_min_seq() {
    let sid = Ulid::new();
    let policy = small_policy(10_000, 0.8);

    let seg1 = live_working(sid, 100, false, 5);
    let seg2 = live_working(sid, 100, false, 9);
    let seg3 = live_working(sid, 100, false, 7);

    let plan = plan_compaction(&[seg3, seg1, seg2], &policy);

    assert!(!plan.is_no_op());
    assert_eq!(plan.oldest_seq, 5);
}

// AC13 (Spec §2.2 I1 / §3.3): gepinnte und Static-Segmente sind NIEMALS im Plan.
#[test]
fn plan_compaction_verschont_pinned_und_static() {
    let sid = Ulid::new();
    let policy = small_policy(10_000, 0.8);

    let pinned = live_working(sid, 100, true, 1);
    let mut static_draft = SegmentDraft::new(
        Ulid::new(),
        sid,
        Region::Static,
        "system_prompt",
        Some(Role::System),
        2,
        0,
    );
    static_draft.tokens = 100;
    let static_seg = Segment::create_live(static_draft, "static content");
    let normal1 = live_working(sid, 100, false, 3);
    let normal2 = live_working(sid, 100, false, 4);

    let plan = plan_compaction(
        &[pinned.clone(), static_seg.clone(), normal1.clone(), normal2.clone()],
        &policy,
    );

    assert!(!plan.is_no_op());
    assert!(!plan.source_segment_ids.contains(&pinned.id()));
    assert!(!plan.source_segment_ids.contains(&static_seg.id()));
    assert_eq!(plan.source_segment_ids, vec![normal1.id(), normal2.id()]);
}

// Spec §3.3: No-Op bei < 2 qualifizierenden Segmenten und bei 0 Segmenten.
#[test]
fn plan_compaction_no_op_bei_weniger_als_zwei() {
    let sid = Ulid::new();
    let policy = small_policy(10_000, 0.8);

    assert!(plan_compaction(&[live_working(sid, 100, false, 1)], &policy).is_no_op());
    assert!(plan_compaction(&[], &policy).is_no_op());
}

// ---- run_major_gc-Orchestrierung (Port der MajorCollection.ExecuteAsync-Semantik) ----

/// Deterministisches Fake-CompactionModel (Prinzip von agentkits FakeLlm): antwortet je
/// Template mit skriptbaren Summaries und zeichnet alle Requests auf.
struct FakeCompactionModel {
    fact_summary: String,
    compaction_summary: String,
    fail_on_fact_extraction: bool,
    requests: Mutex<Vec<CompactionRequest>>,
}

impl FakeCompactionModel {
    fn new(fact: &str, compaction: &str) -> Self {
        FakeCompactionModel {
            fact_summary: fact.to_string(),
            compaction_summary: compaction.to_string(),
            fail_on_fact_extraction: false,
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl CompactionModel for FakeCompactionModel {
    fn summarize(&self, request: &CompactionRequest) -> Result<CompactionResult, CtxmanError> {
        self.requests.lock().unwrap().push(request.clone());
        if request.prompt_template_id == "fact-extraction-v1" {
            if self.fail_on_fact_extraction {
                return Err(CtxmanError::Compaction("fact extraction down".into()));
            }
            Ok(CompactionResult { summary: self.fact_summary.clone() })
        } else {
            Ok(CompactionResult { summary: self.compaction_summary.clone() })
        }
    }
}

fn session_with_model(
    policy: PolicyConfig,
    model: FakeCompactionModel,
) -> (ContextSession, std::sync::Arc<VecPromotionSink>) {
    let sink = std::sync::Arc::new(VecPromotionSink::new());
    struct ArcSink(std::sync::Arc<VecPromotionSink>);
    impl ctxman::promotion::PromotionSink for ArcSink {
        fn write(
            &self,
            fact: &ctxman::promotion::PromotedFact,
            sink_url: &str,
        ) -> Result<(), CtxmanError> {
            self.0.write(fact, sink_url)
        }
    }
    let services = CtxmanServices {
        compaction_model: Some(Box::new(model)),
        promotion_sink: Some(Box::new(ArcSink(sink.clone()))),
        clock: Box::new(|| 0),
        ..Default::default()
    };
    (ContextSession::new(policy, services), sink)
}

#[test]
fn run_major_gc_promotet_vor_compaction_und_kompaktiert() {
    let policy = small_policy(100, 1.0);
    let (mut session, sink) =
        session_with_model(policy, FakeCompactionModel::new("FAKT: X", "ZUSAMMENFASSUNG"));

    session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "erste Nachricht"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "zweite Nachricht"),
        ])
        .unwrap();
    session.drain_events();

    let report = session.run_major_gc().unwrap();

    // Quellen kompaktiert (Soft-Delete I3), Summary-Segment an der ältesten seq (0).
    assert_eq!(report.compacted_source_ids.len(), 2);
    assert!(report.fact_promoted);
    let summary_id = report.summary_segment_id.unwrap();
    let summary = session.segments().iter().find(|s| s.id() == summary_id).unwrap();
    assert_eq!(summary.content(), Some("ZUSAMMENFASSUNG"));
    assert_eq!(summary.seq(), 0); // Spec §3.3: übernimmt die seq-Position der ältesten Quelle.
    assert_eq!(summary.kind(), "compaction_summary");
    let summary_tokens = i64::from(summary.tokens());
    for id in &report.compacted_source_ids {
        let seg = session.segments().iter().find(|s| s.id() == *id).unwrap();
        assert_eq!(seg.state(), SegmentState::Compacted);
    }

    // Promotion VOR Compaction: Sink hat den Fakt, Events in der Reihenfolge
    // fact_promoted → compaction_started → compaction_completed (Spec §3.3/§6).
    assert_eq!(sink.facts().len(), 1);
    assert_eq!(sink.facts()[0].fact, "FAKT: X");
    let events = session.drain_events();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type).collect();
    assert_eq!(
        event_types,
        vec![types::FACT_PROMOTED, types::COMPACTION_STARTED, types::COMPACTION_COMPLETED]
    );

    // Render nach Compaction: Quellen unsichtbar (I3); das Summary-Segment zählt zum Budget
    // (verhaltenstreu zum C#-Original: role = None ⇒ kein Message-Coalescing, Spec §4.6).
    let out = session
        .render(ctxman::RenderOptions {
            provider: "openai".to_string(),
            scope: ctxman::domain::RenderScope::Path,
            turn_advance: false,
        })
        .unwrap();
    assert!(!out.canonical_json.contains("erste Nachricht"));
    assert_eq!(out.tokens_total, summary_tokens);
}

#[test]
fn run_major_gc_leeres_fact_summary_promotet_nicht() {
    let policy = small_policy(100, 1.0);
    let (mut session, sink) =
        session_with_model(policy, FakeCompactionModel::new("", "ZUSAMMENFASSUNG"));

    session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "a"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "b"),
        ])
        .unwrap();
    session.drain_events();

    let report = session.run_major_gc().unwrap();

    // Spec §3.3: leeres Summary = keine dauerhaften Fakten — kein Sink-Write, kein
    // fact_promoted; Compaction läuft normal.
    assert!(!report.fact_promoted);
    assert!(sink.facts().is_empty());
    let event_types: Vec<&str> = session.drain_events().iter().map(|e| e.event_type).collect();
    assert_eq!(event_types, vec![types::COMPACTION_STARTED, types::COMPACTION_COMPLETED]);
}

#[test]
fn run_major_gc_promotion_fehler_verhindert_compaction() {
    let policy = small_policy(100, 1.0);
    let mut model = FakeCompactionModel::new("FAKT", "ZUSAMMENFASSUNG");
    model.fail_on_fact_extraction = true;
    let (mut session, sink) = session_with_model(policy, model);

    session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "a"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "b"),
        ])
        .unwrap();
    session.drain_events();

    // Spec §3.3: Promotion ist zwingend und läuft VOR der lossy Compaction — schlägt die
    // Fact-Extraction fehl, wird NICHTS kompaktiert (kein Fakt-Verlust möglich).
    let result = session.run_major_gc();
    assert!(matches!(result, Err(CtxmanError::Compaction(_))));
    assert!(sink.facts().is_empty());
    assert!(session.drain_events().is_empty());
    assert!(session
        .segments()
        .iter()
        .all(|s| s.state() == SegmentState::Live));
}

#[test]
fn run_major_gc_no_op_ohne_fenster() {
    let policy = small_policy(100, 1.0);
    let (mut session, _sink) =
        session_with_model(policy, FakeCompactionModel::new("F", "S"));

    // Nur ein Segment ⇒ < 2 Units ⇒ No-Op ohne Events und ohne Modell-Aufruf.
    session
        .append_segment(AppendRequest::inline("user_msg", Some(Role::User), "solo"))
        .unwrap();
    session.drain_events();

    let report = session.run_major_gc().unwrap();
    assert!(report.summary_segment_id.is_none());
    assert!(session.drain_events().is_empty());
}

#[test]
fn run_major_gc_ohne_model_liefert_fehler() {
    let mut session = ContextSession::new(small_policy(100, 1.0), CtxmanServices {
        clock: Box::new(|| 0),
        ..Default::default()
    });
    session
        .append_segments(vec![
            AppendRequest::inline("user_msg", Some(Role::User), "a"),
            AppendRequest::inline("assistant_msg", Some(Role::Assistant), "b"),
        ])
        .unwrap();

    assert!(matches!(session.run_major_gc(), Err(CtxmanError::Compaction(_))));
}
