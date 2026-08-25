//! Ausführungs-Strategien — Treiber UM die Loop-Primitive, keine neuen Loops.
//!
//! [`crate::Agent::drive`] bleibt der einzige Agent-Loop. Eine Strategie ist ein
//! Treiber, der diese Primitive mehrfach aufruft und die Phasen orchestriert —
//! dasselbe Muster wie in pytaskforce, wo vier Strategien eine gemeinsame
//! `_react_loop`-Primitive komponieren. `plan_execute` entspricht dort
//! `plan_and_react`: erst planen, dann je Schritt ein voller ReAct-Durchlauf.
//!
//! **Bewusste Abweichung vom Python-Original** (dort ist Strategie nur ein
//! System-Prompt-Preamble, siehe [`crate::Strategy`]): echte Phasen heben die
//! Qualität bei mehrstufigen Aufträgen, und der Preamble-Weg bleibt als
//! [`RunStrategy::Direct`] unverändert der Default. Eingetragen in der README
//! unter „Bewusste Unterschiede zu Python".
//!
//! **Event-Vertrag.** Die Frontends beenden ihre Anzeige beim ersten DONE mit
//! LEERER `source` (`run_task` im Binary, die TUI-Schleife). Deshalb laufen
//! alle Phasen-Runs mit nicht-leerer `source` („plan", „schritt 1/3", …) — sie
//! erscheinen wie Sub-Agenten-Streams — und der Treiber publiziert am Ende
//! selbst genau EIN `final` + `done` mit leerer `source`. Die
//! Failure-Sentinels `"(abgebrochen)"` / `"(keine Antwort)"` /
//! `"(max_steps erreicht)"` werden verbatim durchgereicht — agentkit-swarm und
//! agentkit-work klassifizieren daran.

use crate::agent::{Agent, Cancel, Strategy};
use crate::cli::extract_json;
use crate::events::{AgentEvent, EventBus, EventData, DONE, FINAL, PLAN};
use crate::planning::Step;

/// Parameter der `plan_execute`-Strategie. Alle Werte haben Defaults, ein
/// Profil überschreibt nur, was es nennt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanExecuteParams {
    /// Mehr Schritte liefert der Planner nicht — Überschuss wird abgeschnitten.
    pub max_plan_steps: usize,
    /// Loop-Budget der Plan-Phase (das Modell darf zum Planen kurz explorieren).
    pub plan_max_steps: usize,
    /// Loop-Budget je Plan-Schritt.
    pub step_max_steps: usize,
    /// Nach jedem Schritt kurz prüfen, ob er wirklich erledigt ist.
    pub reflect: bool,
    /// Wie oft ein als unfertig erkannter Schritt nachgearbeitet wird —
    /// begrenzt, damit Reflexion die Kosten nicht unbeschränkt vervielfacht.
    pub max_rework_per_step: usize,
}

impl Default for PlanExecuteParams {
    fn default() -> Self {
        PlanExecuteParams {
            max_plan_steps: 12,
            plan_max_steps: 4,
            step_max_steps: 8,
            reflect: true,
            max_rework_per_step: 1,
        }
    }
}

/// Wie ein Auftrag ausgeführt wird: direkt (heutiges Verhalten) oder über den
/// `plan_execute`-Treiber.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunStrategy {
    /// Ein einzelner Loop-Durchlauf; [`Strategy`] wählt nur das Preamble.
    Direct(Strategy),
    /// Plan-Phase, dann je Schritt ein ReAct-Durchlauf, optional mit Reflexion.
    PlanExecute(PlanExecuteParams),
}

impl Default for RunStrategy {
    fn default() -> Self {
        RunStrategy::Direct(Strategy::React)
    }
}

/// Konfigurationswert → Strategie. Kennt die drei bisherigen Werte
/// (`react`/`plan`/`plain` → Direct) und `plan_execute`; Unbekanntes fällt wie
/// [`crate::roles::strategy_from_str`] auf ReAct zurück, damit ein Tippfehler
/// im Profil den Lauf nicht abbricht.
pub fn run_strategy_from_str(s: &str) -> RunStrategy {
    match s {
        "plan_execute" => RunStrategy::PlanExecute(PlanExecuteParams::default()),
        "plan" => RunStrategy::Direct(Strategy::Plan),
        "plain" => RunStrategy::Direct(Strategy::Plain),
        _ => RunStrategy::Direct(Strategy::React),
    }
}

/// Loop-Budget der Reflexions- und Nacharbeits-Prüfläufe — klein und fest: die
/// Prüffrage soll ein kurzer Blick sein, kein zweiter Arbeitsschritt.
const REFLECT_MAX_STEPS: usize = 3;

/// Führt `task` gemäß `strategy` aus. Für [`RunStrategy::Direct`] exakt
/// [`Agent::run_on_bus`] mit leerer `source` — Verhalten wie bisher.
pub fn run_with_strategy(
    agent: &mut Agent,
    task: &str,
    bus: &EventBus,
    task_id: i64,
    cancel: Option<&Cancel>,
    strategy: &RunStrategy,
) -> String {
    match strategy {
        RunStrategy::Direct(_) => agent.run_on_bus(task, bus, task_id, cancel, ""),
        RunStrategy::PlanExecute(params) => {
            run_plan_execute(agent, task, bus, task_id, cancel, *params)
        }
    }
}

/// Ist der Rückgabetext eines Laufs eine der Sentinel-Antworten des Loops?
fn is_sentinel(text: &str) -> bool {
    matches!(
        text,
        "(abgebrochen)" | "(keine Antwort)" | "(max_steps erreicht)"
    )
}

/// Publiziert den aktuellen Plan-Stand als PLAN-Event mit leerer `source` —
/// dieselbe Nutzlast, die auch das `update_plan`-Tool erzeugt, damit CLI und
/// TUI ihn mit dem vorhandenen Rendering anzeigen.
fn publish_plan(bus: &EventBus, task_id: i64, steps: &[Step]) {
    bus.publish(AgentEvent::with_meta(
        PLAN,
        EventData::Plan(steps.to_vec()),
        task_id,
        String::new(),
    ));
}

/// Schließt den Lauf auf Root-Ebene ab: FINAL nur für echte Antworten (der
/// Loop selbst schickt bei Abbruch auch keins), DONE immer.
fn publish_root_done(bus: &EventBus, task_id: i64, final_text: &str) {
    if !is_sentinel(final_text) {
        bus.publish(AgentEvent::with_meta(
            FINAL,
            EventData::Final(final_text.to_string()),
            task_id,
            String::new(),
        ));
    }
    bus.publish(AgentEvent::with_meta(
        DONE,
        EventData::Done,
        task_id,
        String::new(),
    ));
}

/// Plan-Antwort des Modells → Schrittliste. Akzeptiert ein JSON-Array aus
/// Strings oder aus Objekten mit `step`-Feld (beides kommt vor, je nachdem wie
/// wörtlich das Modell das Schema nimmt). `None` heißt: kein brauchbarer Plan.
fn parse_plan(text: &str, max_steps: usize) -> Option<Vec<Step>> {
    let json = extract_json(text)?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let items = value.as_array()?;
    let steps: Vec<Step> = items
        .iter()
        .filter_map(|item| {
            let beschreibung = item
                .as_str()
                .or_else(|| item.get("step").and_then(|s| s.as_str()))?
                .trim();
            if beschreibung.is_empty() {
                return None;
            }
            Some(Step {
                step: beschreibung.to_string(),
                status: "pending".to_string(),
            })
        })
        .take(max_steps)
        .collect();
    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

/// Der `plan_execute`-Treiber. Ein Agent-Objekt über alle Phasen: der Verlauf
/// akkumuliert, spätere Schritte sehen Plan und Ergebnisse der früheren im
/// Kontext — und ein angehängter ManagedContext (Feature `ctxman`) verwaltet
/// genau diesen Verlauf, ohne dass der Treiber davon weiß.
fn run_plan_execute(
    agent: &mut Agent,
    task: &str,
    bus: &EventBus,
    task_id: i64,
    cancel: Option<&Cancel>,
    params: PlanExecuteParams,
) -> String {
    let original_max_steps = agent.max_steps;

    // ---- Plan-Phase -------------------------------------------------------
    agent.max_steps = params.plan_max_steps;
    let plan_prompt = format!(
        "Auftrag: {task}\n\n\
         Erstelle zuerst NUR einen Plan für diesen Auftrag. Erkunde dafür, wenn \
         nötig, kurz die Umgebung. Antworte am Ende ausschließlich mit einem \
         JSON-Array von höchstens {max} Schritten, jeder Schritt ein kurzer \
         Satz. Beispiel: [\"Erster Schritt\", \"Zweiter Schritt\"]. \
         Führe den Auftrag selbst noch NICHT aus.",
        max = params.max_plan_steps
    );
    let plan_answer = agent.run_on_bus(&plan_prompt, bus, task_id, cancel, "plan");
    if plan_answer == "(abgebrochen)" {
        agent.max_steps = original_max_steps;
        publish_root_done(bus, task_id, &plan_answer);
        return plan_answer;
    }

    let Some(mut steps) = parse_plan(&plan_answer, params.max_plan_steps) else {
        // Kein brauchbarer Plan — leise auf das Direct-Verhalten zurückfallen,
        // statt den Auftrag an einer Formalie scheitern zu lassen. Der Lauf
        // mit leerer `source` publiziert FINAL + DONE dann selbst.
        agent.max_steps = original_max_steps;
        return agent.run_on_bus(task, bus, task_id, cancel, "");
    };
    publish_plan(bus, task_id, &steps);

    // ---- Execute-Phase ----------------------------------------------------
    let total = steps.len();
    let mut outcome: Option<String> = None;
    for i in 0..total {
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst)) {
            outcome = Some("(abgebrochen)".to_string());
            break;
        }
        steps[i].status = "in_progress".to_string();
        publish_plan(bus, task_id, &steps);

        let source = format!("schritt {}/{}", i + 1, total);
        let step_prompt = format!(
            "Führe jetzt Schritt {}/{} des Plans aus: {}\n\
             Der Gesamtauftrag und die bisherigen Ergebnisse stehen in deinem \
             Verlauf. Bearbeite NUR diesen Schritt.",
            i + 1,
            total,
            steps[i].step
        );
        agent.max_steps = params.step_max_steps;
        let mut step_answer = agent.run_on_bus(&step_prompt, bus, task_id, cancel, &source);
        if step_answer == "(abgebrochen)" {
            outcome = Some(step_answer);
            break;
        }

        // ---- Reflexion (optional) ----------------------------------------
        if params.reflect && !is_sentinel(&step_answer) {
            let mut rework = 0;
            while rework < params.max_rework_per_step {
                agent.max_steps = REFLECT_MAX_STEPS;
                let reflect_prompt = format!(
                    "Prüfe kurz: Ist Schritt {}/{} („{}\") damit wirklich \
                     erledigt? Antworte NUR mit ERLEDIGT oder mit \
                     NACHARBEIT: <was konkret fehlt>.",
                    i + 1,
                    total,
                    steps[i].step
                );
                let verdict = agent.run_on_bus(
                    &reflect_prompt,
                    bus,
                    task_id,
                    cancel,
                    &format!("reflexion {}/{}", i + 1, total),
                );
                if verdict == "(abgebrochen)" {
                    outcome = Some(verdict);
                    break;
                }
                let Some(mangel) = verdict.trim().strip_prefix("NACHARBEIT") else {
                    break; // ERLEDIGT (oder Unklares) — nicht nachverhandeln.
                };
                rework += 1;
                agent.max_steps = params.step_max_steps;
                let rework_prompt = format!(
                    "Arbeite Schritt {}/{} nach. Es fehlt:{}",
                    i + 1,
                    total,
                    mangel.trim_start_matches([':', ' ']).trim_end(),
                );
                step_answer = agent.run_on_bus(
                    &rework_prompt,
                    bus,
                    task_id,
                    cancel,
                    &format!("nacharbeit {}/{}", i + 1, total),
                );
                if step_answer == "(abgebrochen)" {
                    outcome = Some(step_answer.clone());
                    break;
                }
            }
            if outcome.is_some() {
                break;
            }
        }

        // Budget erschöpft oder keine Antwort: Schritt als gescheitert
        // markieren und weitermachen — spätere Schritte scheitern nicht
        // automatisch mit, und der Abschluss benennt den Rest ehrlich.
        steps[i].status = if is_sentinel(&step_answer) {
            "failed".to_string()
        } else {
            "done".to_string()
        };
        publish_plan(bus, task_id, &steps);
    }

    // ---- Abschluss --------------------------------------------------------
    let final_text = match outcome {
        Some(sentinel) => sentinel,
        None => {
            agent.max_steps = params.step_max_steps;
            agent.run_on_bus(
                "Alle Planschritte sind bearbeitet. Fasse das Gesamtergebnis \
                 des ursprünglichen Auftrags zusammen — inklusive dessen, was \
                 offen geblieben oder gescheitert ist.",
                bus,
                task_id,
                cancel,
                "abschluss",
            )
        }
    };
    agent.max_steps = original_max_steps;
    publish_root_done(bus, task_id, &final_text);
    final_text
}
