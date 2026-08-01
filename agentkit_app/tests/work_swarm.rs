//! Tests für den Schwarm-Executor der Work-Runtime
//! (`agentkit_app::work_swarm`, Phase 6, §13 des `agentkit-work`-Konzepts).
//! Offline, mit einem PRO AGENT skriptierten LLM — Muster
//! `agentkit_swarm/tests/dynamic.rs::PerAgentLlm`: alle Schwarm-Mitglieder
//! teilen sich EIN `Arc<dyn Llm>`, und `agentkit::testing::FakeLlm` zählt
//! seine Turns global statt je Agent, was bei nebenläufigen Actors nicht
//! deterministisch wäre.
#![cfg(feature = "work")]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use agentkit::llm::{chunk_stream, Chunk, ChunkStream, Llm, Message};
use agentkit::{new_cancel, ApproveFn};
use agentkit_app::{DispatchingExecutor, SwarmWorkExecutor};
use agentkit_work::{
    AgentExecutor, AgentWorkPackage, CodingAgentExecutor, ExecutorKind, FailureInfo,
    VerificationPolicy, WorkItem, WorkItemKind, WorkItemStatus, WorkStore, WorkSubmission,
    WorkToolCtx,
};
use serde_json::Value;

// ------------------------------------------------------------------ Helfer

/// Wie `agentkit_swarm/tests/dynamic.rs::PerAgentLlm` — ein Skript je
/// Agent-ID, erkannt am System-Prompt ("Deine Agent-ID ist '…'", dieselbe
/// Zeile, die `work_swarm::SwarmWorkExecutor` jedem Mitglied mitgibt).
struct PerAgentLlm {
    scripts: Mutex<HashMap<String, VecDeque<Vec<Chunk>>>>,
}

impl PerAgentLlm {
    fn new(scripts: Vec<(&str, Vec<Vec<Chunk>>)>) -> Arc<Self> {
        Arc::new(PerAgentLlm {
            scripts: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(id, turns)| (id.to_string(), turns.into()))
                    .collect(),
            ),
        })
    }
}

fn agent_id_from_system(system: &str) -> Option<String> {
    let rest = system.split_once("Deine Agent-ID ist '")?.1;
    Some(rest.split_once('\'')?.0.to_string())
}

impl Llm for PerAgentLlm {
    fn complete(&self, _messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("zusammengefasst".into()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        let system = messages
            .iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("");
        let Some(id) = agent_id_from_system(system) else {
            return Err("Prompt ohne Agent-ID — kein Schwarm-Mitglied?".into());
        };
        let turn = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_default();
        Ok(chunk_stream(turn))
    }
}

/// Ein Workspace je Test (die Coding-Tools der Mitglieder legen ihn an).
fn workspace(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "agentkit_app_work_swarm_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

fn swarm_item(template: &str) -> WorkItem {
    WorkItem {
        id: "W-1".to_string(),
        run_id: "R-1".to_string(),
        title: "Testitem".to_string(),
        description: "Beschreibung des Testitems.".to_string(),
        kind: WorkItemKind::Review,
        status: WorkItemStatus::Running,
        priority: 5,
        seq: 1,
        required_role: None,
        dependencies: vec![],
        acceptance_criteria: vec![],
        verification_policy: VerificationPolicy::None,
        verifies: None,
        claims_promoted: false,
        executor: ExecutorKind::Swarm {
            template: template.to_string(),
        },
        attempt_count: 0,
        max_attempts: 3,
        updated_at_ms: 0,
    }
}

fn single_agent_item() -> WorkItem {
    let mut it = swarm_item("discovery");
    it.executor = ExecutorKind::SingleAgent;
    it
}

/// `AgentWorkPackage`-Felder sind öffentlich (siehe `executor.rs`) — für
/// diese Tests reicht ein direkt gebautes Paket, ein ganzer `WorkState` wird
/// nicht gebraucht.
fn package(item: WorkItem, ws: &str) -> AgentWorkPackage {
    AgentWorkPackage {
        item,
        objective: "Testziel des Vorhabens.".to_string(),
        predecessor_artifacts: Vec::new(),
        previous_failures: Vec::<FailureInfo>::new(),
        workspace: ws.to_string(),
        max_steps: 20,
        graph_recall: None,
        remaining_wall_secs: None,
    }
}

fn tool_ctx(ws: &str) -> WorkToolCtx {
    WorkToolCtx {
        run_id: "R-1".to_string(),
        work_item_id: "W-1".to_string(),
        attempt_id: "A-1".to_string(),
        agent_id: "worker-1".to_string(),
        max_attempts: 3,
        artifacts_dir: std::path::Path::new(ws)
            .join(".agentkit")
            .join("work-artifacts"),
        submission: Arc::new(Mutex::new(None::<WorkSubmission>)),
        project_id: "demo".to_string(),
        repository_revision: None,
        gateway: None,
        verifies: None,
    }
}

fn allow_all() -> ApproveFn {
    Arc::new(|_: &str| true)
}

// ---------------------------------------------------------- SwarmWorkExecutor

/// Ein Schwarm-Versuch über die Vorlage `discovery`: das START-Mitglied
/// (`explorer-a`) schlägt einen Befund vor, `explorer-b` stimmt zu — der
/// Versuch endet über Konsens und liefert den Vorschlagstext samt
/// Zustimmungszahl zurück (der bindende Rückgabewert für den Runner).
#[test]
fn schwarm_versuch_liefert_bei_konsens_den_vorschlagstext() {
    let ws = workspace("konsens");
    let llm = PerAgentLlm::new(vec![
        (
            "explorer-a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Befund: X verursacht Y"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "explorer-b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("discovery"), &ws);

    let result = executor.execute(&pkg, tool_ctx(&ws), store, &mut |_ev| {});
    let answer = result.expect("ein Konsens-Versuch liefert Ok(...)");
    assert!(answer.contains("Befund: X verursacht Y"), "{answer}");
    assert!(answer.contains("Zustimmung"), "{answer}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Die Ereignisse der Schwarm-Mitglieder erreichen `on_event` — die früher
/// dokumentierte MVP-Grenze („Mitglieder-Events gehen verloren") ist damit
/// aufgelöst. Das ist doppelt bindend: der Runner zählt in genau diesem
/// Callback Schritte und verlängert das Lease (`runner::run_attempt`), und der
/// Trace/Betrachter sieht sonst von einem Schwarm-Work-Item gar nichts.
#[test]
fn mitglieder_ereignisse_erreichen_den_runner_callback() {
    let ws = workspace("events");
    let llm = PerAgentLlm::new(vec![
        (
            "explorer-a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Befund steht"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "explorer-b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("discovery"), &ws);

    let mut events: Vec<agentkit::AgentEvent> = Vec::new();
    executor
        .execute(&pkg, tool_ctx(&ws), store, &mut |ev| events.push(ev.clone()))
        .expect("ein Konsens-Versuch liefert Ok(...)");

    // 1. Die Turns der Mitglieder selbst, getaggt mit ihrer Agent-ID.
    assert!(
        events.iter().any(|e| e.source == "explorer-b"
            && matches!(&e.data, agentkit::EventData::ToolCall { name, .. } if name == "swarm_vote")),
        "kein tool_call von 'explorer-b' im Strom: {:?}",
        events.iter().map(|e| (&e.source, e.etype)).collect::<Vec<_>>()
    );
    // 2. Die Schwarm-Ebene selbst, strukturiert (wer schickte wem was).
    assert!(
        events.iter().any(|e| matches!(&e.data,
            agentkit::EventData::Structured { kind, .. } if kind == agentkit_swarm::SWARM_EVENT_KIND)),
        "keine strukturierten Schwarm-Ereignisse im Strom"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Die Schwarm-Mitglieder erreichen `work_artifact` und `work_submit` — ohne
/// das könnte ein Schwarm sein Ergebnis nicht abliefern (Auftrag, „ohne das
/// ist Phase 6 wertlos"). Geprüft über eine ECHTE, erfolgreiche Nutzung
/// durch das Start-Mitglied, nicht nur über Tool-Präsenz: ein nicht
/// registriertes Tool würde hier als weicher „ERROR: unbekanntes Tool"-Text
/// zurückkommen und weder ein Artefakt noch eine Submission hinterlassen.
#[test]
fn schwarm_mitglieder_erreichen_work_artifact_und_work_submit() {
    let ws = workspace("work_tools");
    let llm = PerAgentLlm::new(vec![
        (
            "reviewer-a",
            vec![
                vec![Chunk::tool(
                    0,
                    "a1",
                    "work_artifact",
                    r#"{"kind":"analysis","filename":"befund.md","content":"Inhalt","summary":"Kurzfassung"}"#,
                )],
                vec![Chunk::tool(
                    0,
                    "s1",
                    "work_submit",
                    r#"{"summary":"Review abgeschlossen","criteria":[]}"#,
                )],
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Angenommen"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "reviewer-b",
            vec![
                // `msg-1` ist die Initialaufgabe (`send_initial`); `work_artifact`
                // und `work_submit` erzeugen KEINE Schwarm-Nachrichten (sie
                // laufen nicht über `SwarmToolContext::new_message`) — der
                // Vorschlag von `reviewer-a` ist deshalb `msg-2`, genau wie im
                // Konsens-Test ohne Work-Tool-Aufrufe.
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("review"), &ws);
    let ctx = tool_ctx(&ws);
    let submission_handle = ctx.submission.clone();

    let result = executor.execute(&pkg, ctx, store.clone(), &mut |_ev| {});
    assert!(result.is_ok(), "{}", result.unwrap_err());

    let snapshot = store.snapshot();
    let artifacts = &snapshot.artifacts;
    assert_eq!(artifacts.len(), 1, "genau ein Artefakt erwartet");
    assert!(artifacts.values().any(|a| a.summary == "Kurzfassung"));

    // `work_submit` schreibt in `ctx.submission` — das Feld wird von JEDEM
    // Mitglied geteilt (`WorkToolCtx::clone()` teilt den `Arc`), der Runner
    // liest es nach dem Versuch aus. Hier direkt geprüft, ohne den ganzen
    // Runner zu benötigen.
    let submission = submission_handle.lock().unwrap();
    assert_eq!(
        submission.as_ref().map(|s| s.summary.as_str()),
        Some("Review abgeschlossen")
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// Ein unbekannter Vorlagenname liefert `Err` samt der erlaubten Namen —
/// kein Schwarm wird aufgebaut, kein LLM-Aufruf nötig.
#[test]
fn unbekannter_vorlagenname_liefert_fehler() {
    let ws = workspace("unbekannte_vorlage");
    let llm = PerAgentLlm::new(vec![]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("erfunden"), &ws);

    let err = executor
        .execute(&pkg, tool_ctx(&ws), store, &mut |_ev| {})
        .expect_err("unbekannte Vorlage muss scheitern");
    assert!(err.contains("erfunden"), "{err}");
    assert!(err.contains("discovery"), "{err}");
    assert!(err.contains("review"), "{err}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Regressionstest zu Befund 2 der Handprobe: ein Schwarm-Item kannte bisher
/// keine Laufzeitgrenze aus dem Budget des Vorhabens (`SwarmWorkExecutor`
/// baute den Schwarm mit einem von `AgentWorkPackage` unabhängigen, festen
/// Wert). Weder Mitglied ruft hier `swarm_propose`/`swarm_vote` — der Schwarm
/// kommt nie zu einem Konsens und muss über die Laufzeitgrenze beendet
/// werden. `pkg.remaining_wall_secs = Some(1)` beweist, dass der Executor
/// diesen Wert tatsächlich als Grenze durchsetzt (eine sehr kleine Zahl statt
/// echter Minuten — `SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS` wäre 900s und
/// würde diesen Test eine Viertelstunde blockieren, käme die Verdrahtung
/// nicht an). Der `CompletionReason::MaxRuntimeReached` des Schwarms muss wie
/// ein Limit behandelt werden, nicht wie ein Absturz — derselbe Sentinel
/// (`"(max_steps erreicht)"`), den `runner::record_failure` schon für den
/// Einzelagenten am Schrittlimit kennt.
#[test]
fn zeitueberschreitung_wird_als_limit_klassifiziert_nicht_als_fehler() {
    let ws = workspace("timeout");
    let llm = PerAgentLlm::new(vec![("explorer-a", vec![]), ("explorer-b", vec![])]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let mut pkg = package(swarm_item("discovery"), &ws);
    pkg.remaining_wall_secs = Some(1);

    // Befund des Code-Reviews: OHNE die Verdrahtung an `pkg.remaining_wall_secs`
    // würde der Executor auf `SWARM_ITEM_FALLBACK_MAX_RUNTIME_SECS` (900s)
    // zurückfallen und `"(max_steps erreicht)"` trotzdem irgendwann liefern —
    // nur 15 Minuten später. Die reine Werteprüfung unten könnte diesen Test
    // also "richtig" bestehen lassen, selbst wenn die Verdrahtung kaputt wäre.
    // Die Wanduhr-Grenze macht genau das sichtbar: sie schlägt fehl, statt
    // eine Viertelstunde zu hängen.
    let start = std::time::Instant::now();
    let result = executor.execute(&pkg, tool_ctx(&ws), store, &mut |_ev| {});
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "sollte binnen ~1s über 'remaining_wall_secs' abbrechen, nicht binnen {elapsed:?} — \
         Hinweis auf einen Rückfall in die 900s-Konstante"
    );
    assert_eq!(
        result.as_deref(),
        Ok("(max_steps erreicht)"),
        "eine Zeitüberschreitung muss wie ein Limit behandelt werden, nicht wie ein Absturz: \
         {result:?}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

// -------------------------------------------------------- DispatchingExecutor

fn single_agent_executor(answer: &'static str) -> CodingAgentExecutor {
    CodingAgentExecutor {
        llm: Arc::new(agentkit::testing::FakeLlm::new(vec![vec![Chunk::text(
            answer,
        )]])),
        approve: allow_all(),
        extra_tools: None,
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
        system_extra: None,
        agent_setup: None,
    }
}

/// Ein Item mit `ExecutorKind::SingleAgent` läuft über den Einzelagenten —
/// die Antwort ist die schlichte Textantwort des Fake-LLM, kein
/// Schwarm-Konsens-Text.
#[test]
fn dispatcher_waehlt_einzelagenten_bei_single_agent_executor() {
    let ws = workspace("dispatch_single");
    let dispatcher = DispatchingExecutor {
        single: single_agent_executor("Direkt erledigt."),
        swarm: Some(SwarmWorkExecutor {
            llm: PerAgentLlm::new(vec![]),
            approve: allow_all(),
            cancel: new_cancel(),
            dry_run: false,
            shell_timeout: 30,
        }),
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(single_agent_item(), &ws);

    let answer = dispatcher
        .execute(&pkg, tool_ctx(&ws), store, &mut |_ev| {})
        .expect("Einzelagent liefert eine Antwort");
    assert_eq!(answer, "Direkt erledigt.");

    std::fs::remove_dir_all(&ws).ok();
}

/// Ein Item mit `ExecutorKind::Swarm` läuft über den Schwarm-Executor, wenn
/// einer verfügbar ist — die Antwort trägt den Konsens-Vorschlag, nicht die
/// Einzelagenten-Antwort.
#[test]
fn dispatcher_waehlt_schwarm_bei_swarm_executor() {
    let ws = workspace("dispatch_swarm");
    let llm = PerAgentLlm::new(vec![
        (
            "explorer-a",
            vec![
                vec![Chunk::tool(
                    0,
                    "p1",
                    "swarm_propose",
                    r#"{"proposal":"Schwarm-Ergebnis"}"#,
                )],
                vec![Chunk::text("Vorschlag eingereicht.")],
            ],
        ),
        (
            "explorer-b",
            vec![
                vec![Chunk::tool(
                    0,
                    "v1",
                    "swarm_vote",
                    r#"{"proposal_id":"msg-2","approve":true}"#,
                )],
                vec![Chunk::text("Zugestimmt.")],
            ],
        ),
    ]);
    let dispatcher = DispatchingExecutor {
        single: single_agent_executor("sollte nicht benutzt werden"),
        swarm: Some(SwarmWorkExecutor {
            llm,
            approve: allow_all(),
            cancel: new_cancel(),
            dry_run: false,
            shell_timeout: 30,
        }),
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("discovery"), &ws);

    let answer = dispatcher
        .execute(&pkg, tool_ctx(&ws), store, &mut |_ev| {})
        .expect("Schwarm-Versuch liefert eine Antwort");
    assert!(answer.contains("Schwarm-Ergebnis"), "{answer}");
    assert!(!answer.contains("sollte nicht benutzt werden"));

    std::fs::remove_dir_all(&ws).ok();
}

/// Ohne verfügbaren Schwarm (`swarm: None`, praktisch `--no-swarm`)
/// degradiert der Dispatcher EHRLICH auf den Einzelagenten — das Item
/// scheitert nicht — und meldet die Degradation über die vorhandene
/// `on_event`-Naht (der Runner reicht jedes `AgentEvent` unverändert als
/// `WorkProgress::Agent` weiter, siehe `agentkit_work::runner::run_attempt`).
#[test]
fn ohne_schwarm_degradiert_der_dispatcher_auf_den_einzelagenten_und_meldet_es() {
    let ws = workspace("dispatch_degradiert");
    let dispatcher = DispatchingExecutor {
        single: single_agent_executor("Einzelagent hat übernommen."),
        swarm: None,
    };
    let store = Arc::new(WorkStore::in_memory());
    let pkg = package(swarm_item("review"), &ws);

    let mut events: Vec<agentkit::AgentEvent> = Vec::new();
    let answer = dispatcher
        .execute(&pkg, tool_ctx(&ws), store, &mut |ev| {
            events.push(ev.clone())
        })
        .expect("Degradation scheitert nicht");
    assert_eq!(answer, "Einzelagent hat übernommen.");

    let note = events
        .iter()
        .find_map(|ev| match &ev.data {
            agentkit::EventData::ToolResult { name, result } if name == "work_dispatch" => {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("Degradation muss über on_event gemeldet werden");
    assert!(note.contains("review"), "{note}");
    assert!(note.contains("Einzelagent"), "{note}");

    std::fs::remove_dir_all(&ws).ok();
}

/// Befund der Handprobe (Phase 3 des Viz-Plans): ein Mitglied liefert sein
/// Ergebnis mit `work_submit` ab und beendet seinen Zug — OHNE
/// `swarm_propose`. Der Schwarm läuft dann in den Leerlauf. Vorher meldete der
/// Executor dafür „(keine Antwort)", und `runner::run_attempt` prüft diesen
/// Sentinel VOR der Submission: der Versuch galt als gescheitert und wurde
/// wiederholt, obwohl das Ergebnis längst im Store lag.
#[test]
fn abgeliefertes_ergebnis_ueberlebt_einen_schwarm_ohne_konsens() {
    let ws = workspace("ohne_konsens");
    // Beide Mitglieder arbeiten, EINES liefert ab — niemand schlägt vor.
    let llm = PerAgentLlm::new(vec![
        (
            "explorer-a",
            vec![
                vec![Chunk::tool(
                    0,
                    "s1",
                    "work_submit",
                    r#"{"summary":"Befund: drei Lifecycle-Risiken","criteria":[]}"#,
                )],
                vec![Chunk::text("Abgeliefert.")],
            ],
        ),
        ("explorer-b", vec![vec![Chunk::text("Nichts zu tun.")]]),
    ]);
    let executor = SwarmWorkExecutor {
        llm,
        approve: allow_all(),
        cancel: new_cancel(),
        dry_run: false,
        shell_timeout: 30,
    };
    let store = Arc::new(WorkStore::in_memory());
    let ctx = tool_ctx(&ws);
    let submission = ctx.submission.clone();
    // Kurzes Wall-Time-Budget: der Schwarm endet an der Laufzeit statt erst
    // nach der Leerlauf-Frist — derselbe Pfad, nur schneller im Test.
    let mut pkg = package(swarm_item("discovery"), &ws);
    pkg.remaining_wall_secs = Some(1);

    let antwort = executor
        .execute(&pkg, ctx, store, &mut |_ev| {})
        .expect("kein harter Fehler");
    assert_ne!(antwort, "(keine Antwort)", "die Arbeit war getan");
    assert_ne!(antwort, "(max_steps erreicht)", "die Arbeit war getan");
    assert!(
        antwort.contains("work_submit"),
        "die Antwort soll sagen, woher das Ergebnis kommt: {antwort}"
    );
    // Und die Submission liegt weiterhin bereit — NEHMEN tut sie der Runner.
    assert!(
        submission.lock().unwrap().is_some(),
        "der Executor darf die Submission nur sehen, nicht verbrauchen"
    );

    std::fs::remove_dir_all(&ws).ok();
}
