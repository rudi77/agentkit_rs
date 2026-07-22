//! Events & Event-Bus — *was passiert* entkoppelt von *wie es angezeigt wird*.
//!
//! Der Agent-Loop `publish`-t neutrale, typisierte [`AgentEvent`]s. Das ist der
//! ganze Trick hinter "Streaming" und "Event-basiert": ein Producer/Consumer-Muster
//! um denselben Loop. Mehrere Consumer (UI-Renderer, Metriken, Logger) abonnieren
//! denselben Strom.
//!
//! Anders als in Python gibt es hier kein dynamisches `data: Any`, sondern eine
//! typisierte [`EventData`]-Enum — strukturell aber dasselbe Event-Modell.

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

/// Die Nutzlast eines Events. Ersetzt Pythons dynamisches `data: Any` durch eine
/// typisierte Variante — die `type`-Strings bleiben dieselben.
#[derive(Debug, Clone, PartialEq)]
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
    Done,
    None,
}

/// Ein typisiertes Agenten-Event (entspricht `AgentEvent` aus Python).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEvent {
    pub etype: &'static str,
    pub data: EventData,
    pub task_id: i64,
    /// leer = Haupt-Agent; bei Sub-Agents deren Label (z. B. "delegate:Wien").
    pub source: String,
}

impl AgentEvent {
    pub fn new(etype: &'static str, data: EventData) -> Self {
        AgentEvent {
            etype,
            data,
            task_id: -1,
            source: String::new(),
        }
    }

    pub fn with_meta(etype: &'static str, data: EventData, task_id: i64, source: String) -> Self {
        AgentEvent {
            etype,
            data,
            task_id,
            source,
        }
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
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Neuen Subscriber anlegen — gibt den Empfänger (wie eine `queue.Queue`) zurück.
    pub fn subscribe(&self) -> Receiver<AgentEvent> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Event an alle aktuellen Subscriber verteilen.
    pub fn publish(&self, event: AgentEvent) {
        let subs = self.subscribers.lock().unwrap();
        for tx in subs.iter() {
            // Abgehängte Empfänger ignorieren (wie eine geschlossene Queue).
            let _ = tx.send(event.clone());
        }
    }
}
