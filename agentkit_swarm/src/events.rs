//! Schwarm-Events — die Beobachtungsebene der Laufzeit, getrennt von den
//! [`agentkit::AgentEvent`]s (einzelne LLM-Schritte, Tools, Text-Deltas).
//!
//! Zwei Ebenen, zwei Busse: der agentkit-`EventBus` transportiert weiterhin die
//! Turn-Interna aller Agenten (getaggt mit `source` = Agent-ID), der
//! [`SwarmEventBus`] den Actor-Lifecycle und die Kommunikation. So bleibt der
//! Agent-Kern unverändert — bewusst KEINE Generalisierung des agentkit-Busses,
//! sondern eine typisierte Strukturkopie (~30 Zeilen).

use crate::completion::CompletionReason;
use crate::message::{AgentId, DeliveryResult, SwarmMessage};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Ein Ereignis auf Schwarm-Ebene.
///
/// `Serialize`, damit `dynamic::forward_swarm_events` es VERLUSTFREI auf den
/// Agent-Bus legen kann — die Textzeile daneben plättet Absender, Art und
/// Message-ID zu einem Satz (siehe `agentkit::trace`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmEvent {
    ActorStarted {
        agent: AgentId,
    },
    ActorStopped {
        agent: AgentId,
    },
    /// Nachricht erfolgreich in die Mailbox des Empfängers gelegt.
    MessageQueued {
        message: SwarmMessage,
    },
    /// Actor hat eine Nachricht aus seiner Mailbox genommen.
    MessageDequeued {
        agent: AgentId,
        message_id: String,
    },
    /// Zustellung abgelehnt (Mailbox voll, Limit, unbekannter Empfänger, …) —
    /// die Nachricht landet zusätzlich in den Dead Letters.
    MessageRejected {
        message: SwarmMessage,
        result: DeliveryResult,
    },
    TurnStarted {
        agent: AgentId,
        message_id: String,
    },
    /// Turn beendet; `success = false` bei den agentkit-Fehler-Sentinels
    /// (Abbruch, kein Stream, max_steps) — der Actor lebt trotzdem weiter.
    TurnCompleted {
        agent: AgentId,
        message_id: String,
        success: bool,
    },
    ProposalCreated {
        message: SwarmMessage,
    },
    VoteSubmitted {
        message: SwarmMessage,
    },
    ActorFailed {
        agent: AgentId,
        error: String,
    },
    /// Der Schwarm ist fertig — das Signal, auf das `join()` wartet.
    SwarmCompleted {
        reason: CompletionReason,
    },
}

/// Minimaler Pub/Sub für [`SwarmEvent`]s — strukturgleich zu
/// [`agentkit::EventBus`], nur typisiert auf Schwarm-Events.
#[derive(Clone, Default)]
pub struct SwarmEventBus {
    subscribers: Arc<Mutex<Vec<Sender<SwarmEvent>>>>,
}

impl SwarmEventBus {
    pub fn new() -> Self {
        SwarmEventBus::default()
    }

    /// Neuen Subscriber anlegen — Events VOR dem Abonnieren sind nicht sichtbar,
    /// also vor `send_initial` abonnieren.
    pub fn subscribe(&self) -> Receiver<SwarmEvent> {
        let (tx, rx) = channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Event an alle aktuellen Subscriber verteilen.
    pub fn publish(&self, event: SwarmEvent) {
        let subs = self.subscribers.lock().unwrap();
        for tx in subs.iter() {
            // Abgehängte Empfänger ignorieren (wie eine geschlossene Queue).
            let _ = tx.send(event.clone());
        }
    }
}
