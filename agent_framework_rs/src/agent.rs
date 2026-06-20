//! Der Agent — ein LLM in einer Schleife mit Tools.
//!
//! Derselbe Loop wie im Python-Port, streamend und event-basiert:
//!
//! ```text
//! solange das Modell ein Tool aufruft:
//!     Tool ausführen -> Ergebnis anhängen -> Modell erneut fragen
//! sonst:
//!     finale Antwort
//! ```
//!
//! Statt Pythons Generator (`run_iter`) reicht der Loop hier jedes [`AgentEvent`]
//! an eine `FnMut`-Senke. Darauf bauen [`Agent::run`] (sammelt die finale Antwort)
//! und [`Agent::run_on_bus`] (für Worker-Threads + mehrere Consumer) auf.
//!
//! ReAct vs. Plan-and-Execute steuert nur der System-Prompt — `strategy`.
//! Harness: max_steps, Retries, Fehlertoleranz, Compaction, kooperatives Abbrechen.

use crate::events::*;
use crate::llm::{Chunk, ChunkStream, Llm};
use crate::memory::{truncate, ShortTermMemory, TRUNCATE_LIMIT};
use crate::planning::Plan;
use crate::skills::Skills;
use crate::tools::ToolRegistry;
use crate::LongTermMemory;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const REACT_PREAMBLE: &str =
    "Arbeite nach dem ReAct-Muster: Überlege in kurzen Schritten, was als Nächstes \
sinnvoll ist, rufe dann ein Tool auf, beobachte das Ergebnis und entscheide den \
nächsten Schritt. Wenn du genug weißt, antworte final ohne weiteren Tool-Aufruf.";

pub const PLAN_PREAMBLE: &str =
    "Arbeite nach dem Muster Plan-and-Execute: Erstelle ZUERST einen kurzen, \
nummerierten Plan (1., 2., 3.) für die Aufgabe. Arbeite den Plan danach Schritt \
für Schritt mit Tools ab und nenne am Ende das Ergebnis.";

/// Strategie = nur ein anderes System-Prompt-Preamble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    React,
    Plan,
    Plain,
}

impl Strategy {
    fn preamble(self) -> &'static str {
        match self {
            Strategy::React => REACT_PREAMBLE,
            Strategy::Plan => PLAN_PREAMBLE,
            Strategy::Plain => "",
        }
    }
}

/// Kooperativer Stop-Knopf (Pendant zu Pythons `threading.Event`).
pub type Cancel = Arc<AtomicBool>;

/// Neuen, nicht gesetzten Stop-Knopf anlegen.
pub fn new_cancel() -> Cancel {
    Arc::new(AtomicBool::new(false))
}

/// content + tool_calls -> serialisierbares Assistant-Dict für die Historie.
pub fn to_assistant_dict(content: Option<&str>, tool_calls: &[Value]) -> Value {
    let mut d = json!({"role": "assistant", "content": content.unwrap_or("")});
    if !tool_calls.is_empty() {
        d["tool_calls"] = json!(tool_calls);
    }
    d
}

pub struct Agent {
    llm: Arc<dyn Llm>,
    pub tools: ToolRegistry,
    pub strategy: Strategy,
    pub max_steps: usize,
    pub token_budget: usize,
    pub parallel_tools: bool,
    pub memory: ShortTermMemory,
}

impl Agent {
    /// Schnellkonstruktor: ReAct-Agent mit Tools, ohne Extras.
    pub fn new(llm: Arc<dyn Llm>, tools: ToolRegistry) -> Self {
        AgentBuilder::new(llm).tools(tools).build()
    }

    pub fn builder(llm: Arc<dyn Llm>) -> AgentBuilder {
        AgentBuilder::new(llm)
    }

    fn build_system(system: Option<&str>, strategy: Strategy) -> Option<String> {
        let parts: Vec<&str> = [strategy.preamble(), system.unwrap_or("")]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    // ----------------------------------------------------------------- core

    /// Arbeitet einen Auftrag ab und reicht jedes [`AgentEvent`] an `on_event`.
    /// Gibt die finale Antwort zurück. Gemeinsamer Kern aller Komfortmethoden
    /// (entspricht Pythons `run_iter` + `_drive` in einem).
    pub fn run_with_events<F>(
        &mut self,
        task: &str,
        cancel: Option<&Cancel>,
        mut on_event: F,
    ) -> String
    where
        F: FnMut(AgentEvent),
    {
        let stopped = |cancel: Option<&Cancel>| cancel.is_some_and(|c| c.load(Ordering::Relaxed));

        self.memory.add_user(task);

        for step in 1..=self.max_steps {
            if stopped(cancel) {
                on_event(AgentEvent::new(
                    CANCELLED,
                    EventData::Cancelled {
                        where_: format!("vor Schritt {step}"),
                    },
                ));
                return "(abgebrochen)".to_string();
            }

            // Harness: Kontext klein halten.
            if self.memory.tokens() > self.token_budget {
                self.memory.compact(self.llm.as_ref(), 4);
            }

            on_event(AgentEvent::new(STEP, EventData::Step { step }));

            // 1) Modell streamen; Text-Deltas als Events; tool_calls rekonstruieren.
            let (content, tool_calls) = {
                let stream = match self.stream_with_retry(&self.memory.messages) {
                    Ok(s) => s,
                    Err(e) => {
                        on_event(AgentEvent::new(
                            ERROR,
                            EventData::Error {
                                name: None,
                                error: e,
                            },
                        ));
                        return "(keine Antwort)".to_string();
                    }
                };
                consume_stream(stream, || stopped(cancel), &mut on_event)
            };
            self.memory
                .add(to_assistant_dict(content.as_deref(), &tool_calls));

            if stopped(cancel) {
                on_event(AgentEvent::new(
                    CANCELLED,
                    EventData::Cancelled {
                        where_: "mitten im Stream".to_string(),
                    },
                ));
                return "(abgebrochen)".to_string();
            }

            // 2) Keine Tools mehr -> fertig.
            if tool_calls.is_empty() {
                let text = content.unwrap_or_default();
                on_event(AgentEvent::new(FINAL, EventData::Final(text.clone())));
                return text;
            }
            if stopped(cancel) {
                on_event(AgentEvent::new(
                    CANCELLED,
                    EventData::Cancelled {
                        where_: "vor Tool-Aufruf".to_string(),
                    },
                ));
                return "(abgebrochen)".to_string();
            }

            // 3) Tools ausführen — mehrere Tool-Calls (optional) nebenläufig,
            //    Reihenfolge bleibt erhalten (tool-Nachrichten zu ihren IDs).
            //    Wir behalten nur die tool_call-id (für das Pairing), nicht den
            //    ganzen Tool-Call-Value.
            let mut parsed: Vec<(String, String, Value)> = Vec::with_capacity(tool_calls.len());
            for tc in &tool_calls {
                let id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let args: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                on_event(AgentEvent::new(
                    TOOL_CALL,
                    EventData::ToolCall {
                        name: name.clone(),
                        args: args.clone(),
                    },
                ));
                parsed.push((id, name, args));
            }

            let results = self.execute_tools(&parsed);

            for ((id, name, _args), (result, err)) in parsed.iter().zip(results.into_iter()) {
                if let Some(error) = err {
                    on_event(AgentEvent::new(
                        ERROR,
                        EventData::Error {
                            name: Some(name.clone()),
                            error,
                        },
                    ));
                }
                let result = truncate(&result, TRUNCATE_LIMIT);
                on_event(AgentEvent::new(
                    TOOL_RESULT,
                    EventData::ToolResult {
                        name: name.clone(),
                        result: result.clone(),
                    },
                ));
                self.memory
                    .add(json!({"role": "tool", "tool_call_id": id, "content": result}));
            }
        }

        let msg = "(max_steps erreicht)".to_string();
        on_event(AgentEvent::new(FINAL, EventData::Final(msg.clone())));
        msg
    }

    /// Führt die geparsten `(id, name, args)`-Tool-Calls aus -> Liste von
    /// (result, error). Bei >1 Call und `parallel_tools` nebenläufig
    /// (Reihenfolge erhalten).
    fn execute_tools(&self, parsed: &[(String, String, Value)]) -> Vec<(String, Option<String>)> {
        let tools = &self.tools;
        // Unbekanntes Tool -> `Ok("ERROR: …")` (weicher Fehler, kein ERROR-Event);
        // ein fehlgeschlagener Tool-Aufruf -> `Err` (löst zusätzlich ERROR aus).
        let run_one = |name: &str, args: &Value| -> (String, Option<String>) {
            match tools.call(name, args.clone()) {
                Ok(s) => (s, None),
                Err(e) => (format!("ERROR: {e}"), Some(e)),
            }
        };

        if self.parallel_tools && parsed.len() > 1 {
            std::thread::scope(|scope| {
                let handles: Vec<_> = parsed
                    .iter()
                    .map(|(_, name, args)| scope.spawn(|| run_one(name, args)))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            })
        } else {
            parsed
                .iter()
                .map(|(_, name, args)| run_one(name, args))
                .collect()
        }
    }

    /// Retry bei transienten Fehlern beim Aufbau des Streams.
    fn stream_with_retry(&self, messages: &[Value]) -> Result<ChunkStream, String> {
        let tools = self.tools.schemas();
        let mut last = "stream fehlgeschlagen".to_string();
        for _ in 0..3 {
            match self.llm.stream(messages, tools) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    // ------------------------------------------------------------- bequem

    /// Arbeitet den Auftrag ab und gibt die finale Antwort als String zurück.
    pub fn run(&mut self, task: &str) -> String {
        self.run_with_events(task, None, |_| {})
    }

    /// Wie [`run`], aber mit Live-Event-Callback und optionalem Stop-Knopf.
    pub fn run_cb<F: FnMut(AgentEvent)>(
        &mut self,
        task: &str,
        cancel: Option<&Cancel>,
        on_event: F,
    ) -> String {
        self.run_with_events(task, cancel, on_event)
    }

    /// Arbeitet den Auftrag ab, publiziert jedes Event (mit `source`-Tag) auf einen
    /// EventBus und schließt mit einem DONE-Event. Gibt die finale Antwort zurück.
    /// Ideal für Worker-Threads, mehrere Consumer und Sub-Agent-Forwarding.
    pub fn run_on_bus(
        &mut self,
        task: &str,
        bus: &EventBus,
        task_id: i64,
        cancel: Option<&Cancel>,
        source: &str,
    ) -> String {
        let final_answer = {
            let bus = bus.clone();
            let source = source.to_string();
            self.run_with_events(task, cancel, move |mut ev| {
                ev.task_id = task_id;
                ev.source = source.clone();
                bus.publish(ev);
            })
        };
        bus.publish(AgentEvent::with_meta(
            DONE,
            EventData::Done,
            task_id,
            source.to_string(),
        ));
        final_answer
    }
}

/// Konsumiert den Streaming-Iterator: ruft `on_event` für jedes Token (TEXT_DELTA)
/// und setzt fragmentierte tool_call-Deltas pro `index` wieder zusammen.
fn consume_stream<F: FnMut(AgentEvent)>(
    stream: ChunkStream,
    mut should_stop: impl FnMut() -> bool,
    on_event: &mut F,
) -> (Option<String>, Vec<Value>) {
    // Ein tool_call wird pro `index` aus mehreren Deltas zusammengesetzt.
    #[derive(Default)]
    struct Slot {
        id: Option<String>,
        name: Option<String>,
        args: Vec<String>,
    }
    let mut content = String::new();
    let mut tool_calls: BTreeMap<usize, Slot> = BTreeMap::new();

    for chunk in stream {
        if should_stop() {
            break;
        }
        let Chunk { delta } = chunk;
        if let Some(text) = delta.content {
            if !text.is_empty() {
                content.push_str(&text);
                on_event(AgentEvent::new(TEXT_DELTA, EventData::TextDelta(text)));
            }
        }
        for tc in delta.tool_calls {
            let slot = tool_calls.entry(tc.index).or_default();
            if tc.id.is_some() {
                slot.id = tc.id;
            }
            if tc.name.is_some() {
                slot.name = tc.name;
            }
            if let Some(args) = tc.arguments {
                slot.args.push(args);
            }
        }
    }

    let calls: Vec<Value> = tool_calls
        .into_values()
        .map(|slot| {
            let joined = slot.args.concat();
            let arguments = if joined.is_empty() {
                "{}".to_string()
            } else {
                joined
            };
            json!({
                "id": slot.id,
                "type": "function",
                "function": {"name": slot.name, "arguments": arguments},
            })
        })
        .collect();

    // `String::new().concat()` wäre "" gewesen — der Leer-Sonderfall entfällt.
    (Some(content), calls)
}

/// Builder für alle optionalen Bausteine (Plan, Memory, Skills, …).
pub struct AgentBuilder {
    llm: Arc<dyn Llm>,
    tools: ToolRegistry,
    system: Option<String>,
    strategy: Strategy,
    max_steps: usize,
    token_budget: usize,
    parallel_tools: bool,
    plan: Option<Plan>,
    long_term: Option<LongTermMemory>,
    skills: Option<Skills>,
    memory: Option<ShortTermMemory>,
}

impl AgentBuilder {
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        AgentBuilder {
            llm,
            tools: ToolRegistry::new(),
            system: None,
            strategy: Strategy::React,
            max_steps: 12,
            token_budget: 8000,
            parallel_tools: true,
            plan: None,
            long_term: None,
            skills: None,
            memory: None,
        }
    }

    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub fn system(mut self, system: &str) -> Self {
        self.system = Some(system.to_string());
        self
    }
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = n;
        self
    }
    pub fn token_budget(mut self, n: usize) -> Self {
        self.token_budget = n;
        self
    }
    pub fn parallel_tools(mut self, on: bool) -> Self {
        self.parallel_tools = on;
        self
    }
    pub fn plan(mut self, plan: Plan) -> Self {
        self.plan = Some(plan);
        self
    }
    pub fn long_term(mut self, ltm: LongTermMemory) -> Self {
        self.long_term = Some(ltm);
        self
    }
    pub fn skills(mut self, skills: Skills) -> Self {
        self.skills = Some(skills);
        self
    }
    pub fn memory(mut self, memory: ShortTermMemory) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn build(mut self) -> Agent {
        // Optionaler Plan / Langzeitgedächtnis / Skills als Tools einklinken.
        if let Some(plan) = &self.plan {
            plan.register_tool(&mut self.tools);
        }
        if let Some(ltm) = &self.long_term {
            ltm.register_tools(&mut self.tools);
        }
        if let Some(skills) = &self.skills {
            skills.register(&mut self.tools);
        }

        let system_prompt = Agent::build_system(self.system.as_deref(), self.strategy);
        let memory = match self.memory {
            None => ShortTermMemory::new(system_prompt.as_deref()),
            Some(mut mem) => {
                if let Some(sp) = system_prompt {
                    let has_system = mem
                        .messages
                        .iter()
                        .any(|m| m.get("role").and_then(Value::as_str) == Some("system"));
                    if !has_system {
                        mem.messages
                            .insert(0, json!({"role": "system", "content": sp}));
                    }
                }
                mem
            }
        };

        Agent {
            llm: self.llm,
            tools: self.tools,
            strategy: self.strategy,
            max_steps: self.max_steps,
            token_budget: self.token_budget,
            parallel_tools: self.parallel_tools,
            memory,
        }
    }
}
