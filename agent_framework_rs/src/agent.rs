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

#[cfg(feature = "ctxman")]
use crate::context::ManagedContext;
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
use std::sync::{Arc, Mutex};

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

/// Geteilter Lauf-Kontext eines Agenten: der aktive [`EventBus`] und Stop-Knopf des
/// gerade laufenden Auftrags. Pendant zu Pythons `agent._bus`/`agent._cancel`.
///
/// Tools (z. B. das `task`-Tool aus `roles.rs`) halten einen Klon dieses Handles und
/// lesen zur Laufzeit den aktiven Bus aus, um Sub-Agent-Events in denselben Strom zu
/// leiten. `Arc`-geteilt, damit der Agent und seine Tools dieselbe Sicht teilen —
/// anders als die `ToolRegistry`, die beim Klonen kopiert wird.
#[derive(Clone, Default)]
pub struct RunHandle {
    inner: Arc<RunCtx>,
}

#[derive(Default)]
struct RunCtx {
    bus: Mutex<Option<EventBus>>,
    cancel: Mutex<Option<Cancel>>,
}

impl RunHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Der aktive EventBus des laufenden Auftrags (oder `None` ohne Bus-Lauf).
    pub fn bus(&self) -> Option<EventBus> {
        self.inner.bus.lock().unwrap().clone()
    }

    /// Der Stop-Knopf des laufenden Auftrags (oder `None`).
    pub fn cancel(&self) -> Option<Cancel> {
        self.inner.cancel.lock().unwrap().clone()
    }

    fn set(&self, bus: Option<EventBus>, cancel: Option<Cancel>) {
        *self.inner.bus.lock().unwrap() = bus;
        *self.inner.cancel.lock().unwrap() = cancel;
    }
}

/// content + tool_calls -> serialisierbares Assistant-Dict für die Historie.
pub fn to_assistant_dict(content: Option<&str>, tool_calls: &[Value]) -> Value {
    let mut d = json!({"role": "assistant", "content": content.unwrap_or("")});
    if !tool_calls.is_empty() {
        d["tool_calls"] = json!(tool_calls);
    }
    d
}

/// Quelle/Label eines als Tool laufenden Sub-Agenten: `"<name>:<Auftrag>"`, wobei
/// der Auftrag auf eine Zeile normalisiert und auf 24 Zeichen gekürzt wird. So
/// bleiben (auch parallel laufende) Sub-Agenten im Event-Strom unterscheidbar.
pub fn subagent_source(name: &str, task: &str) -> String {
    let label: String = task
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(24)
        .collect();
    format!("{name}:{label}")
}

pub struct Agent {
    llm: Arc<dyn Llm>,
    pub tools: ToolRegistry,
    pub strategy: Strategy,
    pub max_steps: usize,
    pub token_budget: usize,
    pub parallel_tools: bool,
    pub memory: ShortTermMemory,
    /// Basis-Wartezeit (ms) zwischen Stream-Retries; verdoppelt sich pro Versuch
    /// (exponentieller Backoff gegen Rate-Limits/transiente Netzfehler). Tests
    /// setzen 0, damit Fehlerpfade nicht künstlich langsam werden.
    pub retry_backoff_ms: u64,
    /// Optionaler ctxman-Kontext (Feature `ctxman`): ist er gesetzt, rendert ER die
    /// Provider-Messages (Watermarks/GC/Externalisierung statt naiver Compaction);
    /// `memory` läuft als Spiegel für Frontends (`/reset`, Token-Anzeige) weiter.
    #[cfg(feature = "ctxman")]
    pub context: Option<ManagedContext>,
    /// Geteilter Lauf-Kontext (aktiver Bus/Cancel) — von Tools wie `task` gelesen.
    run: RunHandle,
}

impl Agent {
    /// Schnellkonstruktor: ReAct-Agent mit Tools, ohne Extras.
    pub fn new(llm: Arc<dyn Llm>, tools: ToolRegistry) -> Self {
        AgentBuilder::new(llm).tools(tools).build()
    }

    pub fn builder(llm: Arc<dyn Llm>) -> AgentBuilder {
        AgentBuilder::new(llm)
    }

    /// Klon des geteilten Lauf-Kontexts. Tools (z. B. das `task`-Tool), die VOR dem
    /// Build in dieselbe Registry registriert werden, halten diesen Klon und lesen
    /// daraus zur Laufzeit den aktiven Bus/Stop-Knopf.
    pub fn run_handle(&self) -> RunHandle {
        self.run.clone()
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
    pub fn run_with_events<F>(&mut self, task: &str, cancel: Option<&Cancel>, on_event: F) -> String
    where
        F: FnMut(AgentEvent),
    {
        self.drive(task, cancel, None, on_event)
    }

    /// Gemeinsamer Kern. `bus` (falls vorhanden) wird neben dem Stop-Knopf in den
    /// geteilten [`RunHandle`] geschrieben, damit Tools wie `task` Sub-Agent-Events
    /// in denselben Strom leiten können.
    fn drive<F>(
        &mut self,
        task: &str,
        cancel: Option<&Cancel>,
        bus: Option<EventBus>,
        on_event: F,
    ) -> String
    where
        F: FnMut(AgentEvent),
    {
        let result = self.drive_inner(task, cancel, bus, on_event);
        // Kontext-Snapshot NACH jedem Lauf sichern (auch nach Abbruch/Fehler) —
        // damit ein Neustart genau dort weitermacht.
        #[cfg(feature = "ctxman")]
        if let Some(ctx) = &self.context {
            let _ = ctx.save();
        }
        result
    }

    fn drive_inner<F>(
        &mut self,
        task: &str,
        cancel: Option<&Cancel>,
        bus: Option<EventBus>,
        mut on_event: F,
    ) -> String
    where
        F: FnMut(AgentEvent),
    {
        let stopped = |cancel: Option<&Cancel>| cancel.is_some_and(|c| c.load(Ordering::Relaxed));

        // Aktiven Lauf-Kontext veröffentlichen (für Tools wie `task`). Wird zu Beginn
        // jedes Laufs überschrieben; ein explizites Zurücksetzen ist unnötig, da Tools
        // nur INNERHALB dieses Laufs ausgeführt werden (nie zwischen Läufen).
        self.run.set(bus, cancel.cloned());

        self.memory.add_user(task);
        #[cfg(feature = "ctxman")]
        if let Some(ctx) = &self.context {
            ctx.add_user(task);
        }
        #[cfg(feature = "ctxman")]
        let ctx_active = self.context.is_some();
        #[cfg(not(feature = "ctxman"))]
        let ctx_active = false;

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

            // Harness: Kontext klein halten. Mit ManagedContext übernimmt ctxman das
            // (Watermarks/GC beim Rendern) — die naive Compaction bleibt dann aus.
            if !ctx_active && self.memory.tokens() > self.token_budget {
                self.memory.compact(self.llm.as_ref(), 4);
            }

            on_event(AgentEvent::new(STEP, EventData::Step { step }));

            // Provider-Messages: rendert ctxman (falls aktiv), sonst die rohe Historie.
            #[cfg(feature = "ctxman")]
            let ctx_messages: Option<Vec<Value>> = match self.context.as_ref().map(|c| c.messages())
            {
                None => None,
                Some(Ok(m)) => Some(m),
                Some(Err(e)) => {
                    on_event(AgentEvent::new(
                        ERROR,
                        EventData::Error {
                            name: None,
                            error: format!("ctxman-Render fehlgeschlagen: {e}"),
                        },
                    ));
                    return "(keine Antwort)".to_string();
                }
            };
            #[cfg(not(feature = "ctxman"))]
            let ctx_messages: Option<Vec<Value>> = None;
            let request_messages: &[Value] =
                ctx_messages.as_deref().unwrap_or(&self.memory.messages);

            // 1) Modell streamen; Text-Deltas als Events; tool_calls rekonstruieren.
            let (content, tool_calls) = {
                let stream = match self.stream_with_retry(request_messages, cancel) {
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
            #[cfg(feature = "ctxman")]
            if let Some(ctx) = &self.context {
                ctx.add_assistant(content.as_deref(), &tool_calls);
            }

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

            for ((id, name, _args), (result, err)) in parsed.iter().zip(results) {
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
                #[cfg(feature = "ctxman")]
                if let Some(ctx) = &self.context {
                    ctx.add_tool_result(id, &result);
                }
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

    /// Retry bei transienten Fehlern beim Aufbau des Streams — mit exponentiellem
    /// Backoff (`retry_backoff_ms`, verdoppelt pro Versuch) gegen Rate-Limits (429)
    /// und kurze Netz-Aussetzer. Das Warten läuft in kleinen Schritten, damit der
    /// Stop-Knopf auch währenddessen greift.
    fn stream_with_retry(
        &self,
        messages: &[Value],
        cancel: Option<&Cancel>,
    ) -> Result<ChunkStream, String> {
        let tools = self.tools.schemas();
        let mut last = "stream fehlgeschlagen".to_string();
        for attempt in 0..3u32 {
            if attempt > 0 && self.retry_backoff_ms > 0 {
                let wait = self.retry_backoff_ms.saturating_mul(1u64 << (attempt - 1));
                let mut slept = 0u64;
                while slept < wait {
                    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return Err(last);
                    }
                    let step = (wait - slept).min(50);
                    std::thread::sleep(std::time::Duration::from_millis(step));
                    slept += step;
                }
            }
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
            let publish_bus = bus.clone();
            let source = source.to_string();
            self.drive(task, cancel, Some(bus.clone()), move |mut ev| {
                ev.task_id = task_id;
                ev.source = source.clone();
                publish_bus.publish(ev);
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

    /// Führt DIESEN (frisch gebauten) Agenten als **Sub-Agent** für `task` aus und
    /// gibt seine finale Antwort zurück — das gemeinsame „ein Agent als Tool"-Verhalten
    /// hinter `add_subagent` und dem `task`-Tool. Ein Sub-Agent ist kein eigener Typ,
    /// sondern ein ganz normaler [`Agent`]; nur der Aufrufweg unterscheidet sich:
    ///
    /// - ohne `bus`: schlicht [`Agent::run`] — der Aufrufer sieht nur das Ergebnis.
    /// - mit `bus`: [`Agent::run_on_bus`] mit `source = subagent_source(name, task)`,
    ///   damit die Events des Sub-Agenten live (und bei Parallelität unterscheidbar)
    ///   im selben Strom landen.
    pub fn run_as_subagent(
        &mut self,
        task: &str,
        name: &str,
        bus: Option<&EventBus>,
        cancel: Option<&Cancel>,
    ) -> String {
        match bus {
            None => self.run(task),
            Some(bus) => {
                let source = subagent_source(name, task);
                self.run_on_bus(task, bus, -1, cancel, &source)
            }
        }
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
    retry_backoff_ms: u64,
    plan: Option<Plan>,
    long_term: Option<LongTermMemory>,
    skills: Option<Skills>,
    memory: Option<ShortTermMemory>,
    run_handle: Option<RunHandle>,
    #[cfg(feature = "ctxman")]
    context: Option<ManagedContext>,
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
            retry_backoff_ms: 500,
            plan: None,
            long_term: None,
            skills: None,
            memory: None,
            run_handle: None,
            #[cfg(feature = "ctxman")]
            context: None,
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
    /// Basis-Wartezeit (ms) zwischen Stream-Retries (0 = kein Backoff, z. B. in Tests).
    pub fn retry_backoff_ms(mut self, ms: u64) -> Self {
        self.retry_backoff_ms = ms;
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

    /// Setzt einen vorab erzeugten [`RunHandle`]. Nötig, wenn ein Tool (z. B. `task`)
    /// VOR dem Build registriert wird und denselben Lauf-Kontext lesen soll wie der
    /// fertige Agent. Ohne Angabe wird ein frischer Handle erzeugt.
    pub fn run_handle(mut self, handle: RunHandle) -> Self {
        self.run_handle = Some(handle);
        self
    }

    /// Aktiviert ctxman als Context-Manager (Feature `ctxman`): registriert das
    /// `expand_context_ref`-Tool, setzt den System-Prompt als Static-Region und
    /// lässt den Loop die Provider-Messages von ctxman rendern.
    #[cfg(feature = "ctxman")]
    pub fn managed_context(mut self, ctx: ManagedContext) -> Self {
        self.context = Some(ctx);
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
        #[cfg(feature = "ctxman")]
        if let Some(ctx) = &self.context {
            ctx.register_tool(&mut self.tools);
        }

        let system_prompt = Agent::build_system(self.system.as_deref(), self.strategy);
        // ManagedContext: der System-Prompt IST die Static-Region (Epoch-Bump nur,
        // wenn er sich gegenüber einem geladenen Snapshot geändert hat).
        #[cfg(feature = "ctxman")]
        if let (Some(ctx), Some(sp)) = (&self.context, system_prompt.as_deref()) {
            let _ = ctx.set_system(sp);
        }
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
            retry_backoff_ms: self.retry_backoff_ms,
            #[cfg(feature = "ctxman")]
            context: self.context,
            memory,
            run: self.run_handle.unwrap_or_default(),
        }
    }
}
