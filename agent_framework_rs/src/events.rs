//! Events & Event-Bus — *was passiert* entkoppelt von *wie es angezeigt wird*.
//!
//! Der Agent-Loop `publish`-t neutrale, typisierte [`AgentEvent`]s. Das ist der
//! ganze Trick hinter "Streaming" und "Event-basiert": ein Producer/Consumer-Muster
//! um denselben Loop. Mehrere Consumer (UI-Renderer, Metriken, Logger) abonnieren
//! denselben Strom.
//!
//! Anders als in Python gibt es hier kein dynamisches `data: Any`, sondern eine
//! typisierte [`EventData`]-Enum — strukturell aber dasselbe Event-Modell.

use serde::Serialize;
use serde_json::Value;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

// Event-Typen — als &'static str-Konstanten, damit `event.type` (wie in Python)
// vergleichbar bleibt.
pub const STEP: &str = "step"; // ein neuer Loop-Schritt beginnt
pub const TEXT_DELTA: &str = "text_delta"; // ein Stück der Antwort (Streaming-Token)
pub const TOOL_CALL: &str = "tool_call"; // Agent ruft ein Tool auf
pub const TOOL_RESULT: &str = "tool_result"; // Ergebnis eines Tools
pub const PLAN: &str = "plan"; // der Agent hat seinen Plan / seine Todo-Liste aktualisiert
pub const FINAL: &str = "final"; // finale Antwort steht
pub const ERROR: &str = "error"; // ein Tool/Call ist schiefgegangen
pub const CANCELLED: &str = "cancelled"; // Auftrag wurde mittendrin abgebrochen
pub const DONE: &str = "done"; // Auftrag komplett abgearbeitet (auch nach Abbruch)
pub const STRUCTURED: &str = "structured"; // strukturierte Nutzlast eines Frontends/Erweiterungs-Crates

/// Die Nutzlast eines Events. Ersetzt Pythons dynamisches `data: Any` durch eine
/// typisierte Variante — die `type`-Strings bleiben dieselben.
///
/// `Serialize` (kein `Deserialize`): der Kern legt seinen Ereignisstrom als
/// NDJSON ab ([`crate::trace`]), liest ihn aber nie zurück — [`AgentEvent::etype`]
/// ist ein `&'static str` und wäre gar nicht deserialisierbar. Wer den Trace
/// liest, definiert eigene Spiegeltypen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventData {
    Step {
        step: usize,
    },
    TextDelta(String),
    ToolCall {
        name: String,
        args: Value,
    },
    ToolResult {
        name: String,
        result: String,
    },
    Error {
        name: Option<String>,
        error: String,
    },
    /// Der aktualisierte Plan als strukturierte Schrittliste (nicht vorgerendert),
    /// damit Konsumenten ihn selbst darstellen oder auswerten können (Spec: „Daten =
    /// das Plan-Objekt selbst"). Rendern via [`crate::render_steps`].
    Plan(Vec<crate::planning::Step>),
    Final(String),
    Cancelled {
        where_: String,
    },
    /// Strukturierte Nutzlast eines Frontends/Erweiterungs-Crates. Der Kern kennt
    /// den Inhalt bewusst NICHT — `kind` benennt ihn, `payload` trägt ihn. So kann
    /// z. B. agentkit-swarm seine Ereignisse verlustfrei über den Bus schicken,
    /// ohne dass der Agent-Kern den Schwarm kennenlernt (Abhängigkeitsrichtung).
    /// Für Renderer und TUI ist es eine einzelne kompakte Zeile; ausgewertet wird
    /// es erst von Konsumenten, die `kind` kennen.
    Structured {
        kind: String,
        payload: Value,
    },
    Done,
    None,
}

/// Ein typisiertes Agenten-Event (entspricht `AgentEvent` aus Python).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentEvent {
    pub etype: &'static str,
    pub data: EventData,
    pub task_id: i64,
    /// leer = Haupt-Agent; bei Sub-Agents deren Label (z. B. "delegate:Wien").
    pub source: String,
    /// Die `tool_call_id` des Modells — verbindet `tool_call`, `tool_result`
    /// und ein etwaiges `error` DESSELBEN Aufrufs.
    ///
    /// Im Umschlag statt in der Nutzlast, weil sie dasselbe tut wie `task_id`
    /// und `source`: sie ordnet ein Ereignis zu, statt es zu beschreiben. Ohne
    /// sie ist die Zuordnung bei mehreren Tool-Aufrufen in EINEM Schritt
    /// Ratearbeit über die Reihenfolge — das Modell darf parallel aufrufen, und
    /// der Agent führt sie auch parallel aus.
    ///
    /// Leer bei allem, was nicht zu einem Tool-Aufruf gehört.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub call_id: String,
}

impl AgentEvent {
    pub fn new(etype: &'static str, data: EventData) -> Self {
        AgentEvent {
            etype,
            data,
            task_id: -1,
            source: String::new(),
            call_id: String::new(),
        }
    }

    pub fn with_meta(etype: &'static str, data: EventData, task_id: i64, source: String) -> Self {
        AgentEvent {
            etype,
            data,
            task_id,
            source,
            call_id: String::new(),
        }
    }

    /// Die Korrelations-ID setzen (Builder, damit die bestehenden
    /// Konstruktoren unverändert bleiben).
    pub fn with_call_id(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = call_id.into();
        self
    }

    /// Ein [`EventData::Structured`]-Event bauen — die Abkürzung für
    /// Erweiterungs-Crates und den Trace, die sonst dreimal dieselbe Zeile
    /// schreiben müssten.
    pub fn structured(kind: &str, payload: Value, task_id: i64, source: &str) -> Self {
        AgentEvent::with_meta(
            STRUCTURED,
            EventData::Structured {
                kind: kind.to_string(),
                payload,
            },
            task_id,
            source.to_string(),
        )
    }

    /// Bequemer Zugriff auf den finalen/abschließenden Text, falls vorhanden.
    pub fn text(&self) -> Option<&str> {
        match &self.data {
            EventData::Final(s) | EventData::TextDelta(s) => Some(s),
            _ => None,
        }
    }
}

/// Minimaler Pub/Sub: ein `publish`, beliebig viele Subscriber-Queues.
///
/// Entspricht Pythons `EventBus` (mehrere `queue.Queue`-Subscriber). Hier über
/// `std::sync::mpsc`-Kanäle; `subscribe()` gibt den Empfänger zurück.
#[derive(Clone, Default)]
pub struct EventBus {
    subscribers: Arc<Mutex<Vec<Sender<AgentEvent>>>>,
    /// Optionaler Mitschnitt (`--trace DIR`). Sitzt am BUS, nicht an einem
    /// einzelnen Consumer: sonst hinge der Trace daran, dass ein bestimmtes
    /// Frontend gerade zuhört — Ereignisse nach dem Abschluss-DONE (Nachzügler
    /// eines Sub-Agenten) fehlten, und jedes weitere Frontend bräuchte seine
    /// eigene Verdrahtung.
    trace: Option<Arc<crate::trace::TraceWriter>>,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus::default()
    }

    /// Ein Bus, der jedes Ereignis zusätzlich als NDJSON mitschreibt.
    pub fn with_trace(trace: Arc<crate::trace::TraceWriter>) -> Self {
        EventBus {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            trace: Some(trace),
        }
    }

    /// Neuen Subscriber anlegen — gibt den Empfänger (wie eine `queue.Queue`) zurück.
    ///
    /// **Wer blockierend liest, darf den Bus nicht selbst behalten.** Die
    /// Sender-Enden liegen IM Bus, nicht beim Empfänger: solange irgendein
    /// [`EventBus`]-Klon lebt, liefert `recv()` niemals `Disconnected`. Ein
    /// Consumer, der auf ein Abschluss-Event wartet und nebenbei einen eigenen
    /// Klon hält, hängt deshalb für immer, wenn der Producer-Thread stirbt
    /// (Panik in einem Tool) — das Event kommt nicht mehr, und der Kanal schließt
    /// nicht. Entweder den Bus per Move an den Producer geben (so macht es die
    /// CLI in `run_task`) oder nicht blockierend lesen (so macht es das TUI mit
    /// `try_recv` in seiner Render-Schleife).
    pub fn subscribe(&self) -> Receiver<AgentEvent> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Event an alle aktuellen Subscriber verteilen (und mitschreiben).
    pub fn publish(&self, event: AgentEvent) {
        if let Some(trace) = &self.trace {
            trace.write_event(&event);
        }
        let subs = self.subscribers.lock().unwrap();
        for tx in subs.iter() {
            // Abgehängte Empfänger ignorieren (wie eine geschlossene Queue).
            let _ = tx.send(event.clone());
        }
    }
}
