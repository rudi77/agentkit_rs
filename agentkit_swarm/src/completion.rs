//! Abschluss des Schwarms — Policy, Ergebnis und der CompletionActor.
//!
//! Der CompletionActor interpretiert keine Inhalte, er verwaltet nur formale
//! Zustände (Proposals und Votes) und wertet eine deterministische Termination
//! Policy aus. Das ist keine Orchestrierung. Er ist bewusst ein einfacher
//! Thread mit `Receiver<SwarmMessage>` und kein `Agent` — es gibt kein LLM zu
//! befragen, und ein eigener `AgentCommand`-Wrapper wäre reine Symmetrie ohne
//! zweiten Nutzer.

use crate::events::{SwarmEvent, SwarmEventBus};
use crate::message::{AgentId, DeliveryResult, MessageKind, SwarmMessage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Wann gilt der Schwarm als fertig?
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionPolicy {
    /// Ein Proposal ist angenommen, sobald `required_approvals` zustimmende
    /// Votes eingegangen sind (der Vorschlagende zählt nicht automatisch mit).
    Consensus { required_approvals: usize },
}

/// Warum der Schwarm beendet wurde.
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionReason {
    /// Konsens erreicht: das angenommene Proposal samt Zustimmungszahl.
    Consensus {
        proposal: SwarmMessage,
        approvals: usize,
    },
    MessageLimitReached,
    MaxRuntimeReached,
    /// Ein Actor-Thread ist gestorben (Panic) — MVP-Policy: der ganze Schwarm
    /// wird kontrolliert gestoppt.
    ActorFailure {
        agent: AgentId,
        error: String,
    },
    /// Extern über [`crate::SwarmHandle::stop`] beendet.
    Stopped,
}

/// Unzustellbare oder verworfene Nachricht (abgelehnte Sends, Mailbox-Drain
/// beim Shutdown, Votes auf unbekannte Proposals).
#[derive(Clone, Debug, PartialEq)]
pub struct DeadLetter {
    pub message: SwarmMessage,
    pub reason: DeliveryResult,
}

/// Endergebnis eines Schwarm-Laufs.
#[derive(Clone, Debug, PartialEq)]
pub struct SwarmResult {
    pub reason: CompletionReason,
    /// Verbrauchte Nachrichten-Zustellungen (gegen `max_messages` gezählt).
    pub messages_sent: usize,
    pub dead_letters: Vec<DeadLetter>,
    /// Verarbeitete Turns je Agent.
    pub turns: HashMap<AgentId, usize>,
}

/// Globales Nachrichtenbudget (`max_messages`) — geteilt über alle Agenten,
/// damit kein zentraler Router im Nachrichtenpfad nötig ist.
pub(crate) struct MessageBudget {
    count: AtomicUsize,
    max: usize,
    exhausted: AtomicBool,
}

impl MessageBudget {
    pub fn new(max: usize) -> Self {
        MessageBudget {
            count: AtomicUsize::new(0),
            max,
            exhausted: AtomicBool::new(false),
        }
    }

    /// Eine Zustellung verbrauchen; `false` = Limit erschöpft. CAS-Schleife
    /// statt `fetch_add`, damit ein abgewiesener Versuch keinen Zählerstand
    /// hinterlässt — der Zähler zählt akzeptierte Zustellungen, nicht Versuche.
    pub fn try_consume(&self) -> bool {
        let mut current = self.count.load(Ordering::SeqCst);
        loop {
            if current >= self.max {
                return false;
            }
            match self.count.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Verbrauch einer FEHLgeschlagenen Zustellung zurückgeben: das Budget
    /// zählt erfolgreiche Zustellungen — ein retrybarer `postfach_voll`-Send
    /// darf `max_messages` nicht aufzehren. Nur nach erfolgreichem
    /// [`try_consume`] aufrufen.
    pub fn refund(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }

    /// Vermerkt, dass eine Zustellung am Limit gescheitert ist.
    pub fn mark_exhausted(&self) {
        self.exhausted.store(true, Ordering::SeqCst);
    }

    /// Ist mindestens eine Zustellung am Limit gescheitert?
    ///
    /// Das Limit ist eine BREMSE, kein Not-Aus: der Monitor beendet den Schwarm
    /// daraufhin nicht sofort, sondern erst, wenn die bereits zugestellte Arbeit
    /// abgearbeitet ist (siehe [`crate::SwarmHandle::join`]). `swarm_propose` und
    /// `swarm_vote` sind budgetfrei — der Schwarm kann also auch am erschöpften
    /// Limit noch per Konsens abschließen, statt seine Arbeit wegzuwerfen.
    pub fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::SeqCst)
    }

    /// Erfolgreich verbrauchte Zustellungen.
    pub fn used(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// Der CompletionActor-Loop: sammelt Proposals und Votes, publiziert bei
/// erreichtem Quorum `SwarmCompleted` und beendet sich. `recv_timeout` statt
/// `recv`, damit das Stop-Flag auch ohne weitere Nachrichten greift.
pub(crate) fn completion_loop(
    rx: Receiver<SwarmMessage>,
    policy: CompletionPolicy,
    stop: Arc<AtomicBool>,
    swarm_bus: SwarmEventBus,
    dead_letters: Arc<Mutex<Vec<DeadLetter>>>,
) {
    let CompletionPolicy::Consensus { required_approvals } = policy;
    // Proposal-ID -> (Proposal, Anzahl Zustimmungen). Doppelte Votes desselben
    // Agenten pro Proposal zählen nicht doppelt.
    let mut proposals: HashMap<String, (SwarmMessage, Vec<AgentId>)> = HashMap::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match msg.kind {
            MessageKind::Proposal => {
                // Quorum 0 ist ohne Votes erfüllt — das Proposal schließt den
                // Schwarm sofort ab (sonst würde `join()` endlos warten).
                if required_approvals == 0 {
                    swarm_bus.publish(SwarmEvent::SwarmCompleted {
                        reason: CompletionReason::Consensus {
                            proposal: msg,
                            approvals: 0,
                        },
                    });
                    return;
                }
                proposals.insert(msg.id.clone(), (msg, Vec::new()));
            }
            MessageKind::Vote => {
                let Some(entry) = msg
                    .correlation_id
                    .as_ref()
                    .and_then(|pid| proposals.get_mut(pid))
                else {
                    // Vote auf unbekanntes Proposal -> Dead Letter + Event.
                    swarm_bus.publish(SwarmEvent::MessageRejected {
                        message: msg.clone(),
                        result: DeliveryResult::NotAllowed,
                    });
                    dead_letters.lock().unwrap().push(DeadLetter {
                        message: msg,
                        reason: DeliveryResult::NotAllowed,
                    });
                    continue;
                };
                let approve = serde_json::from_str::<serde_json::Value>(&msg.content)
                    .ok()
                    .and_then(|v| v["zustimmung"].as_bool())
                    .unwrap_or(false);
                if approve && !entry.1.contains(&msg.from) {
                    entry.1.push(msg.from.clone());
                }
                if entry.1.len() >= required_approvals {
                    swarm_bus.publish(SwarmEvent::SwarmCompleted {
                        reason: CompletionReason::Consensus {
                            proposal: entry.0.clone(),
                            approvals: entry.1.len(),
                        },
                    });
                    return;
                }
            }
            // Andere Arten landen hier nicht (Tools senden nur Proposal/Vote
            // an den CompletionActor) — defensiv ignorieren.
            _ => {}
        }
    }
}
