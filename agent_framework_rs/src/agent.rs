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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const REACT_PREAMBLE: &str =
    "Arbeite nach dem ReAct-Muster: Überlege in kurzen Schritten, was als Nächstes \
sinnvoll ist, rufe dann ein Tool auf, beobachte das Ergebnis und entscheide den \
nächsten Schritt. Wenn du genug weißt, antworte final ohne weiteren Tool-Aufruf.";

pub const PLAN_PREAMBLE: &str =
    "Arbeite nach dem Muster Plan-and-Execute: Erstelle ZUERST einen kurzen, \
nummerierten Plan (1., 2., 3.) für die Aufgabe. Arbeite den Plan danach Schritt \
für Schritt mit Tools ab und nenne am Ende das Ergebnis.";

/// Einmaliger Einwurf vor der finalen Antwort, wenn Dateien geändert, aber danach
/// kein Check mehr ausgeführt wurde (siehe [`AgentBuilder::verify_before_final`]).
pub const VERIFY_NUDGE: &str = "Halt: Du hast Dateien geändert, aber danach keinen \
Check ausgeführt. Verifiziere deine Änderungen jetzt konkret — führe die passenden \
Tests, einen Build oder ein kurzes Prüfskript via run_shell aus und behebe gefundene \
Fehler. Gib die finale Antwort erst, wenn ein tatsächlich ausgeführter Check dein \
Ergebnis bestätigt. Verbleibende Schritte sind ausreichend; gib nicht vorzeitig auf.";

/// Ab so vielen selbst gelesenen Dateien in EINEM Lauf kommt [`DELEGATE_NUDGE`].
///
/// Vier, weil der Prompt selbst von „zwei, drei Dateien" spricht — und weil genau
/// vier `read_file` in einem einzigen Schritt der beobachtete Fall waren, der den
/// Kontext aufgebläht und danach das Rate-Limit ausgelöst hat.
const DELEGATE_READ_THRESHOLD: usize = 4;

/// Einmaliger Einwurf, wenn der Orchestrator zu viel selbst liest, statt die
/// Erkundung zu delegieren.
///
/// Warum ein Einwurf und nicht nur der System-Prompt: Der Prompt sagt es bereits
/// (siehe `coding.rs::coding_system`), aber Instruktionstreue ist modellabhängig
/// — in einem Live-Lauf hat ein Modell die Delegations-Anweisung schlicht
/// ignoriert. Der Einwurf greift unabhängig davon, genau wie [`VERIFY_NUDGE`].
/// Er kommt nur, wenn es das `task`-Tool wirklich gibt (Sub-Agenten und
/// Schwarm-Mitglieder haben es nie) — sonst wäre er eine Aufforderung zu einem
/// Werkzeug, das der Agent gar nicht hat.
pub const DELEGATE_NUDGE: &str = "Halt: Du hast jetzt mehrere Dateien selbst gelesen — \
ihr voller Inhalt bleibt für den Rest des Auftrags in deinem Kontext und verdrängt \
irgendwann das Wesentliche. Lies nicht weiter auf eigene Faust: delegiere die restliche \
Erkundung mit 'task' an einen 'explorer'-Sub-Agenten und lass dir nur die relevanten \
Stellen mit Pfad und Zeile zurückgeben. Selbst liest du danach höchstens noch das, was du \
für eine konkrete Änderung wirklich brauchst.";

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

/// Wie viele Nachrichten die Kompaktierung im Original behält. Ein Wert für
/// beide Auslöser (Token-Budget im Loop und `/compact`), damit die manuelle
/// Verdichtung sich genauso verhält wie die automatische.
const COMPACT_KEEP_LAST: usize = 4;

/// Obergrenze für ein befolgtes `Retry-After`. Länger zu warten hilft einem
/// interaktiven Lauf nicht mehr — dann lieber schnell scheitern und den Fehler
/// zeigen, statt den Nutzer minutenlang vor einem stummen Terminal zu lassen.
const RETRY_AFTER_MAX_MS: u64 = 60_000;

/// Prozessweite Rate-Limit-Sperre: bis wann darf NIEMAND anfragen?
///
/// Millisekunden seit [`rate_limit_epoch`]; `0` = keine Sperre.
///
/// Warum prozessweit und nicht je Agent: Sub-Agenten (`task`) und
/// Schwarm-Mitglieder teilen sich EIN `Arc<dyn Llm>` und damit eine
/// Deployment-Quota. Wartete jeder Agent nur für sich, hämmerten die übrigen
/// während seiner Wartezeit weiter und hielten das Limit heiß — ein Thundering
/// Herd, in dem niemand durchkommt. Gemessen: drei Schwarm-Mitglieder bzw. drei
/// Sub-Agenten, jeder mit eigenem Retry, endeten alle mit "(keine Antwort)" und
/// verwarfen dabei ihre ganze bereits gelesene Arbeit.
///
/// Ein `static` statt eines Feldes auf [`Llm`]: der Trait ist die Naht zu jeder
/// Provider-Implementierung, ein Zustandsfeld dort müsste durch alle. Praktisch
/// spricht ein Prozess ohnehin mit einem Deployment.
static RATE_LIMIT_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

fn rate_limit_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Restliche Sperrzeit, oder `None` wenn frei.
fn rate_limit_remaining() -> Option<Duration> {
    let bis = RATE_LIMIT_UNTIL_MS.load(Ordering::SeqCst);
    if bis == 0 {
        return None;
    }
    let jetzt = rate_limit_epoch().elapsed().as_millis() as u64;
    (bis > jetzt).then(|| Duration::from_millis(bis - jetzt))
}

/// Sperrt alle Agenten für `ms` Millisekunden. `fetch_max`, damit eine kürzere
/// Meldung eine laufende längere Sperre nicht verkürzt.
fn rate_limit_pause(ms: u64) {
    let bis = rate_limit_epoch().elapsed().as_millis() as u64 + ms;
    RATE_LIMIT_UNTIL_MS.fetch_max(bis, Ordering::SeqCst);
}

/// Wartet `dauer` ab, prüft dabei im 50-ms-Takt den Stop-Knopf.
/// `false` = abgebrochen.
fn warte_abbrechbar(dauer: Duration, cancel: Option<&Cancel>) -> bool {
    let mut geschlafen = Duration::ZERO;
    while geschlafen < dauer {
        if stopped(cancel) {
            return false;
        }
        let schritt = (dauer - geschlafen).min(Duration::from_millis(50));
        std::thread::sleep(schritt);
        geschlafen += schritt;
    }
    true
}

/// Die vom Provider genannte Wartezeit aus einer Fehlermeldung, in Millisekunden.
///
/// Der Provider-Adapter schreibt bei 429 ein `", Retry-After: <n>s"` in den
/// Fehlertext (`llm.rs::describe_error`) — dieser String ist der Vertrag, den
/// hier gelesen wird. Absichtlich kein typisierter Fehler: `Llm::stream` liefert
/// `Result<_, String>`, ein Fehler-Enum müsste durch JEDE `Llm`-Implementierung
/// und hätte heute genau einen Nutzer. Dieselbe Konvention wie bei agentkits
/// übrigen Sentinel-Strings; bricht der Vertrag, fällt der Retry lediglich auf
/// den exponentiellen Backoff zurück (und `retry_after_wird_geparst` schlägt an).
fn retry_after_ms(error: &str) -> Option<u64> {
    let rest = error.split_once("Retry-After: ")?.1;
    let zahl: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let sekunden: f64 = zahl.parse().ok()?;
    if !sekunden.is_finite() || sekunden <= 0.0 {
        return None;
    }
    Some((sekunden * 1000.0).ceil() as u64)
}

/// Kooperativer Stop-Knopf (Pendant zu Pythons `threading.Event`).
pub type Cancel = Arc<AtomicBool>;

/// Neuen, nicht gesetzten Stop-Knopf anlegen.
pub fn new_cancel() -> Cancel {
    Arc::new(AtomicBool::new(false))
}

/// Liest den Stop-Knopf: `true`, sobald der Abbruch angefordert wurde. Zentrale
/// Stelle für alle Cancel-Checks (Loop, Tool-Ausführung, Shell-Watcher), damit
/// das Memory-Ordering nicht zwischen den Stellen auseinanderläuft.
pub(crate) fn stopped(cancel: Option<&Cancel>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::Relaxed))
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

/// Ergebnis von [`Agent::rewind_to_turn`].
#[derive(Debug, PartialEq, Eq)]
pub enum RewindOutcome {
    /// Verlauf gekürzt; so viele Nachrichten sind weggefallen.
    Done(usize),
    /// Diesen Zug gibt es nicht (Züge sind 1-basiert).
    NoSuchTurn,
    /// Mit ctxman verwaltetem Kontext (`--ctx`) nicht möglich — siehe
    /// [`Agent::rewind_to_turn`].
    ContextManaged,
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
    /// Selbstverifikation: Will das Modell nach Datei-Änderungen (write_file/edit_file)
    /// abschließen, ohne danach einen Check (run_shell) ausgeführt zu haben, wird
    /// einmalig [`VERIFY_NUDGE`] injiziert und der Loop fortgesetzt statt beendet.
    pub verify_before_final: bool,
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

    /// Übernimmt einen geladenen Verlauf (`--session`) als neuen Zustand.
    ///
    /// Nicht einfach `agent.memory = …`: mit aktivem ctxman ist `memory` nur
    /// der Spiegel, und ein frischer `--ctx`-Zustand kennt den geladenen
    /// Verlauf noch nicht — das Modell begänne bei null, obwohl das Frontend
    /// die Historie anzeigt. Hier wird beides zusammen gesetzt, damit Spiegel
    /// und Kontext nicht auseinanderlaufen. Ein aus dem Snapshot fortgesetzter
    /// Kontext hat den Verlauf bereits; dort wird nichts nachgespielt.
    pub fn adopt_history(&mut self, memory: ShortTermMemory) {
        #[cfg(feature = "ctxman")]
        if let Some(ctx) = &self.context {
            ctx.replay(&memory.messages);
        }
        self.memory = memory;
    }

    /// Kompaktiert den Kontext auf Kommando (`/compact`), statt zu warten, bis
    /// das Token-Budget bzw. die Watermark erreicht ist. `hint` lenkt die
    /// Zusammenfassung („behalte die API-Details") — mit ctxman wirkungslos,
    /// dessen Compaction hat keinen Hinweis-Eingang.
    ///
    /// Gibt zurück, ob sich etwas geändert hat. Wie beim Rewind liegt die
    /// Entscheidung hier, weil nur `Agent` weiß, wer den Kontext führt: mit
    /// ctxman dessen GC, sonst die naive Zusammenfassung über `memory`.
    pub fn compact_now(&mut self, hint: Option<&str>) -> bool {
        #[cfg(feature = "ctxman")]
        if let Some(ctx) = &self.context {
            return ctx.compact_now();
        }
        self.memory
            .compact_with_hint(self.llm.as_ref(), COMPACT_KEEP_LAST, hint)
    }

    /// Verwaltet ctxman den Kontext (`--ctx`)? Dann rendert **er** die
    /// Provider-Messages und `memory` ist nur ein Spiegel für die Frontends.
    /// Die eine Stelle, die diese Frage beantwortet — ohne `#[cfg]` beim
    /// Aufrufer (ohne Feature `ctxman` ist es konstant `false`).
    pub fn context_managed(&self) -> bool {
        #[cfg(feature = "ctxman")]
        {
            self.context.is_some()
        }
        #[cfg(not(feature = "ctxman"))]
        {
            false
        }
    }

    /// Würde ein Rewind gerade gelingen? Gleiche Prüfung wie
    /// [`Agent::rewind_to_turn`], nur ohne etwas zu ändern — damit ein Frontend
    /// vor einer Fork-Sicherung wissen kann, ob der Schnitt danach klappt.
    pub fn rewind_check(&self) -> RewindOutcome {
        if self.context_managed() {
            RewindOutcome::ContextManaged
        } else {
            RewindOutcome::Done(0)
        }
    }

    /// Schneidet den Gesprächsverlauf vor Zug `turn` ab (1-basiert) — die
    /// Agenten-Ebene von [`ShortTermMemory::rewind_to_turn`], für `/rewind`
    /// und `/fork` in jedem Frontend.
    ///
    /// Warum hier und nicht direkt auf `memory`: mit aktivem ctxman rendert
    /// **dieser** die Provider-Messages und `memory` ist nur ein Spiegel.
    /// Ein Rewind allein auf dem Spiegel kürzt nichts, was das Modell sieht —
    /// er würde nur eine mitlaufende Session-Datei von der Wahrheit
    /// abtrennen. ctxman hat keine Kürzungs-API, also wird hier abgelehnt
    /// statt halb ausgeführt. Nur `Agent` kennt beide Seiten; das Wissen
    /// gehört deshalb hierher und nicht in jedes Frontend.
    pub fn rewind_to_turn(&mut self, turn: usize) -> RewindOutcome {
        if self.context_managed() {
            return RewindOutcome::ContextManaged;
        }
        match self.memory.rewind_to_turn(turn) {
            Some(entfernt) => RewindOutcome::Done(entfernt),
            None => RewindOutcome::NoSuchTurn,
        }
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

        // Selbstverifikation (verify_before_final): Dateiänderungen seit dem letzten
        // ausgeführten Check? Der Nudge wird höchstens einmal pro Lauf injiziert.
        let mut unverified_changes = false;
        let mut verify_nudged = false;

        // Delegations-Einwurf: nur sinnvoll, wenn es auch ein `task`-Tool gibt.
        // Kein eigener Schalter — die Registry weiß es bereits, und ein Sub-Agent
        // (der `task` nie hat) soll den Einwurf nie sehen.
        let kann_delegieren = self.tools.has("task");
        let mut dateien_gelesen = 0usize;
        let mut delegate_nudged = false;

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
                self.memory.compact(self.llm.as_ref(), COMPACT_KEEP_LAST);
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
            //    Sowohl ein fehlgeschlagener Verbindungsaufbau als auch ein mitten
            //    im Stream abgerissener Strom enden hier: ERROR-Event + Abbruch des
            //    Laufs. Beides ist ein Modell-/Netzfehler, kein Ergebnis.
            let stream = self
                .stream_with_retry(request_messages, cancel)
                .and_then(|s| consume_stream(s, || stopped(cancel), &mut on_event));
            let (content, tool_calls) = match stream {
                Ok(pair) => pair,
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

            // 2) Keine Tools mehr -> fertig. Ausnahme: verify_before_final verlangt
            //    nach Datei-Änderungen erst einen ausgeführten Check — der Einwurf
            //    kommt als User-Nachricht, der Loop läuft weiter (einmal pro Lauf).
            if tool_calls.is_empty() {
                if self.verify_before_final
                    && unverified_changes
                    && !verify_nudged
                    && step < self.max_steps
                {
                    verify_nudged = true;
                    self.memory.add_user(VERIFY_NUDGE);
                    #[cfg(feature = "ctxman")]
                    if let Some(ctx) = &self.context {
                        ctx.add_user(VERIFY_NUDGE);
                    }
                    continue;
                }
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

            // Buchführung für verify_before_final: Änderung setzt die Pflicht,
            // ein anschließender Shell-Check löst sie ein.
            for (_, name, _) in &parsed {
                match name.as_str() {
                    "write_file" | "edit_file" => unverified_changes = true,
                    "run_shell" => unverified_changes = false,
                    // Nur `read_file` zählt: grep/glob liefern Treffer-Listen,
                    // `read_file` schaufelt ganze Dateien in den Kontext — das
                    // war der beobachtete Auslöser.
                    "read_file" => dateien_gelesen += 1,
                    _ => {}
                }
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
                    ctx.add_tool_result(id, name, &result);
                }
            }

            // Delegations-Einwurf NACH den Tool-Ergebnissen: die `tool`-Nachrichten
            // müssen lückenlos auf ihren Assistant-Zug folgen, erst danach darf eine
            // User-Nachricht kommen. Einmal pro Lauf — ein wiederholter Einwurf wäre
            // Nörgeln und würde selbst Kontext kosten.
            if kann_delegieren && !delegate_nudged && dateien_gelesen >= DELEGATE_READ_THRESHOLD {
                delegate_nudged = true;
                self.memory.add_user(DELEGATE_NUDGE);
                #[cfg(feature = "ctxman")]
                if let Some(ctx) = &self.context {
                    ctx.add_user(DELEGATE_NUDGE);
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
        let cancel = self.run.cancel();
        // Unbekanntes Tool -> `Ok("ERROR: …")` (weicher Fehler, kein ERROR-Event);
        // ein fehlgeschlagener Tool-Aufruf -> `Err` (löst zusätzlich ERROR aus).
        // Nach einem Abbruch werden ausstehende Tools nicht mehr gestartet — als
        // weiches Ergebnis, damit jede tool_call-id ein Resultat behält.
        let run_one = |name: &str, args: &Value| -> (String, Option<String>) {
            if stopped(cancel.as_ref()) {
                return (
                    "ERROR: abgebrochen — nicht mehr ausgeführt.".to_string(),
                    None,
                );
            }
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
    /// und kurze Netz-Aussetzer. Nennt der Provider ein `Retry-After`, gewinnt
    /// dieses (siehe [`retry_after_ms`]). Das Warten läuft in kleinen Schritten,
    /// damit der Stop-Knopf auch währenddessen greift.
    fn stream_with_retry(
        &self,
        messages: &[Value],
        cancel: Option<&Cancel>,
    ) -> Result<ChunkStream, String> {
        let tools = self.tools.schemas();
        let mut last = "stream fehlgeschlagen".to_string();
        for attempt in 0..3u32 {
            // `retry_backoff_ms == 0` heißt weiterhin "gar nicht warten" — darauf
            // beruhen die Tests, und weder ein `Retry-After` noch die prozessweite
            // Sperre dürfen das aushebeln.
            if self.retry_backoff_ms > 0 {
                let mut warten = Duration::ZERO;
                if attempt > 0 {
                    let backoff = self.retry_backoff_ms.saturating_mul(1u64 << (attempt - 1));
                    warten = match retry_after_ms(&last) {
                        // Ein Fenster jenseits der Obergrenze: weitere Versuche
                        // wären garantiert vergeblich, also sofort mit dem Fehler
                        // raus. Bewusst OHNE Sperre für alle — so lange soll kein
                        // anderer Agent blockiert werden.
                        Some(ms) if ms > RETRY_AFTER_MAX_MS => return Err(last),
                        // Ein genanntes Fenster schlägt den eigenen Backoff: bei
                        // 429 nennt Azure typischerweise 30 s, während 500 ms/1 s
                        // noch im selben Fenster landen.
                        Some(ms) => Duration::from_millis(ms.max(backoff)),
                        None => Duration::from_millis(backoff),
                    };
                }
                // Die prozessweite Sperre gilt vor JEDEM Versuch, auch dem ersten:
                // hat ein anderer Agent gerade ein 429 kassiert, wird hier gewartet
                // statt mitzuhämmern. Das MAXIMUM statt der Summe — beide Zeiten
                // beschreiben dasselbe Fenster, nacheinander gewartet wäre es
                // doppelt.
                if let Some(rest) = rate_limit_remaining() {
                    warten = warten.max(rest);
                }
                if !warten.is_zero() && !warte_abbrechbar(warten, cancel) {
                    return Err(last);
                }
            }
            match self.llm.stream(messages, tools) {
                Ok(s) => return Ok(s),
                Err(e) => {
                    // Nennt der Provider ein befolgbares Fenster, gilt es für ALLE
                    // Agenten — sonst laufen die übrigen genau jetzt hinein.
                    if let Some(ms) = retry_after_ms(&e) {
                        if ms <= RETRY_AFTER_MAX_MS {
                            rate_limit_pause(ms);
                        }
                    }
                    last = e;
                }
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
///
/// `Err` ⇔ der Stream brach mittendrin ab. Die bis dahin gesammelten Bruchstücke
/// werden bewusst verworfen: eine halbe Antwort als fertige auszugeben (mit
/// Exit 0) ist schlimmer, als den Schritt als Fehler zu melden. Ein Abbruch über
/// `should_stop` ist dagegen ein reguläres Ende.
fn consume_stream<F: FnMut(AgentEvent)>(
    stream: ChunkStream,
    mut should_stop: impl FnMut() -> bool,
    on_event: &mut F,
) -> Result<(Option<String>, Vec<Value>), String> {
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
        let Chunk { delta } = chunk?;
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
    Ok((Some(content), calls))
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
    verify_before_final: bool,
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
            verify_before_final: false,
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
    /// Selbstverifikation vor der finalen Antwort: Nach write_file/edit_file ohne
    /// anschließenden run_shell-Check wird statt des Abschlusses einmalig
    /// [`VERIFY_NUDGE`] injiziert. Default: aus (Verhalten wie bisher).
    pub fn verify_before_final(mut self, on: bool) -> Self {
        self.verify_before_final = on;
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
            verify_before_final: self.verify_before_final,
            #[cfg(feature = "ctxman")]
            context: self.context,
            memory,
            run: self.run_handle.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinnt den String-Vertrag zwischen `llm.rs::describe_error` und
    /// [`retry_after_ms`]. Ohne diesen Test würde eine Umformulierung der
    /// Fehlermeldung den Retry stillschweigend auf den (viel zu kurzen)
    /// exponentiellen Backoff zurückfallen lassen.
    #[test]
    fn retry_after_wird_geparst() {
        // Exakt das Format aus `describe_error` bei einem Azure-429.
        let azure = "HTTP 429 (Rate-Limit), Retry-After: 30s: {\"error\":{\"code\":\
                     \"rate_limit_exceeded\"}}";
        assert_eq!(retry_after_ms(azure), Some(30_000));

        assert_eq!(retry_after_ms("… Retry-After: 1s: …"), Some(1_000));
        // Gebrochene Sekunden werden aufgerundet — lieber etwas zu lang warten.
        assert_eq!(retry_after_ms("… Retry-After: 0.5s: …"), Some(500));
        assert_eq!(retry_after_ms("… Retry-After: 2.5s: …"), Some(2_500));

        // Ohne Hinweis (oder mit unbrauchbarem) bleibt es beim Backoff.
        assert_eq!(
            retry_after_ms("HTTP 500 (Server-Fehler, transient): …"),
            None
        );
        assert_eq!(retry_after_ms("Netzwerkfehler: timeout"), None);
        assert_eq!(retry_after_ms("… Retry-After: 0s: …"), None);
        assert_eq!(retry_after_ms("… Retry-After: bald: …"), None);
    }

    /// Die prozessweite Sperre darf nur wachsen: meldet ein zweiter Agent ein
    /// kürzeres Fenster, verkürzt das die laufende Sperre nicht — sonst liefe die
    /// Herde vor Ablauf des längsten gemeldeten Fensters wieder los.
    ///
    /// Bewusst mit winzigen Werten: der Zähler ist prozessweit, eine lange Sperre
    /// hier würde die übrigen Tests ausbremsen.
    #[test]
    fn rate_limit_sperre_waechst_nur() {
        rate_limit_pause(50);
        let nach_50 = RATE_LIMIT_UNTIL_MS.load(Ordering::SeqCst);
        assert!(rate_limit_remaining().is_some_and(|d| d <= Duration::from_millis(50)));

        rate_limit_pause(1);
        assert_eq!(
            RATE_LIMIT_UNTIL_MS.load(Ordering::SeqCst),
            nach_50,
            "kürzeres Fenster hat die laufende Sperre verkürzt"
        );
    }

    /// Die Obergrenze muss über dem üblichen Azure-Fenster (30 s) liegen, sonst
    /// bricht der Retry genau den Fall ab, für den er gebaut wurde.
    #[test]
    fn obergrenze_deckt_das_uebliche_azure_fenster() {
        assert!(retry_after_ms("… Retry-After: 30s: …").unwrap() <= RETRY_AFTER_MAX_MS);
        assert!(retry_after_ms("… Retry-After: 300s: …").unwrap() > RETRY_AFTER_MAX_MS);
    }
}
