//! Der Actor um einen `agentkit::Agent` — das Kernprinzip des Crates:
//!
//! > Ein Schwarm-Agent ist ein normaler `agentkit::Agent`, der exklusiv von
//! > einem Actor besessen wird. Der Actor fügt Mailbox, Identität und
//! > Peer-Kommunikation hinzu, ohne den Agent-Loop zu verändern.
//!
//! Ein Actor = ein OS-Thread = ein `&mut Agent`: der Actor verarbeitet immer
//! genau eine Nachricht (einen Turn) gleichzeitig. Das ist normales
//! Actor-Verhalten und zugleich der Schutz der Agent-Memory vor
//! konkurrierendem Zugriff — kein Mutex, keine Reentranz, deterministischer
//! Verlauf pro Agent. Die Memory bleibt über Nachrichten hinweg erhalten
//! (`run_on_bus` appended nur).

use crate::completion::{DeadLetter, MessageBudget};
use crate::directory::PeerDirectory;
use crate::events::{SwarmEvent, SwarmEventBus};
use crate::message::{
    now_ms, AgentCommand, AgentId, DeliveryResult, MessageKind, Recipient, SwarmMessage,
};
use agentkit::{Agent, Cancel, EventBus};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Geteilter Kontext eines Actors — das Schwarm-Pendant zu agentkits
/// `RunHandle`: die Tool-Closures (`swarm_send` & Co.) halten einen
/// `Arc`-Klon und erreichen darüber Peers, CompletionActor, Budget und die
/// gerade verarbeitete Nachricht.
pub(crate) struct SwarmToolContext {
    pub self_id: AgentId,
    pub swarm_id: String,
    /// Nur die von der Topologie erlaubten Peers (Capability-Prinzip).
    pub peers: PeerDirectory,
    /// Direktkanal zum CompletionActor — steht in keiner PeerDirectory,
    /// Agenten erreichen ihn ausschließlich über `swarm_propose`/`swarm_vote`.
    pub completion_tx: SyncSender<SwarmMessage>,
    pub swarm_bus: SwarmEventBus,
    /// Stop-Knopf des laufenden Turns; die Laufzeit setzt ihn beim Shutdown.
    pub cancel: Cancel,
    /// Die eingehende Nachricht des laufenden Turns (für `swarm_reply`).
    pub current: Mutex<Option<SwarmMessage>>,
    /// Schwarmweite Sequenz für Nachricht-IDs ("msg-N").
    pub msg_seq: Arc<AtomicU64>,
    pub msg_budget: Arc<MessageBudget>,
    pub dead_letters: Arc<Mutex<Vec<DeadLetter>>>,
    pub max_hops: u32,
    /// Gesetzt, sobald der Schwarm abgeschlossen ist (Konsens, Limit, Abbruch).
    /// Ab da werden fachliche Nachrichten nicht mehr zugestellt — siehe
    /// [`DeliveryResult::SwarmCompleted`].
    pub completed: Arc<AtomicBool>,
}

impl SwarmToolContext {
    pub fn next_message_id(&self) -> String {
        format!("msg-{}", self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Baut eine neue ausgehende Nachricht. `from` kommt IMMER aus dem Kontext
    /// — das Modell kann sich nicht als anderer Agent ausgeben.
    pub fn new_message(
        &self,
        to: Recipient,
        kind: MessageKind,
        content: String,
        reply_to: Option<String>,
        correlation_id: Option<String>,
        hop_count: u32,
    ) -> SwarmMessage {
        SwarmMessage {
            id: self.next_message_id(),
            swarm_id: self.swarm_id.clone(),
            from: self.self_id.clone(),
            to,
            kind,
            content,
            reply_to,
            correlation_id,
            created_at: now_ms(),
            hop_count,
        }
    }

    /// Hop-Zähler der nächsten ausgehenden Nachricht: Kettenlänge der gerade
    /// verarbeiteten Nachricht + 1 (ohne laufenden Turn: 0). Zusammen mit
    /// `max_hops` die Bremse gegen endlose Konversationsketten.
    pub fn next_hop_count(&self) -> u32 {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|m| m.hop_count + 1)
            .unwrap_or(0)
    }

    /// Der EINE Zustellpfad für send/reply/broadcast/propose: Budget-Gate,
    /// nicht blockierende Zustellung, Events und Dead Letters an einer Stelle.
    pub fn deliver(&self, target: &crate::AgentActorRef, message: SwarmMessage) -> DeliveryResult {
        self.deliver_inner(target, message, true)
    }

    /// Zustellung OHNE Budget-Gate — ausschließlich für die Peer-Kopien eines
    /// `swarm_propose`. Der Weg zum CompletionActor ist schon budgetfrei; wären
    /// es die Kopien nicht, sähe am erschöpften Limit niemand den Vorschlag und
    /// könnte über ihn abstimmen. Der Abschluss wäre dann genau dort unmöglich,
    /// wo er am dringendsten gebraucht wird. Missbrauch ist begrenzt: jede Kopie
    /// muss weiterhin in eine (endliche) Mailbox passen, und `max_runtime` bleibt
    /// die äußere Schranke.
    pub fn deliver_free(
        &self,
        target: &crate::AgentActorRef,
        message: SwarmMessage,
    ) -> DeliveryResult {
        self.deliver_inner(target, message, false)
    }

    fn deliver_inner(
        &self,
        target: &crate::AgentActorRef,
        message: SwarmMessage,
        budgeted: bool,
    ) -> DeliveryResult {
        // Nach dem Abschluss nimmt niemand mehr fachliche Nachrichten an. Bewusst
        // VOR dem Budget-Gate und ohne Dead Letter: ein Mitglied, das seinen Turn
        // zu Ende bringt, während der Konsens schon steht, sendet erwartbar ins
        // Leere. Das als Fehlzustellung zu protokollieren, ließ einen sauberen
        // Abschluss wie eine Panne aussehen.
        if self.completed.load(Ordering::SeqCst) {
            self.swarm_bus.publish(SwarmEvent::MessageRejected {
                message,
                result: DeliveryResult::SwarmCompleted,
            });
            return DeliveryResult::SwarmCompleted;
        }
        if budgeted && !self.msg_budget.try_consume() {
            // Nur die Bremse ziehen, nicht den Schwarm beenden: sonst würde die
            // erste Zustellung über Budget alle laufenden Turns abbrechen und
            // sämtliche Mailboxen als Dead Letters verwerfen — inklusive der
            // Arbeit, die das Budget gerade bezahlt hat. Der Monitor wartet
            // stattdessen, bis das Zugestellte abgearbeitet ist; bis dahin kann
            // der Schwarm über das budgetfreie `swarm_propose`/`swarm_vote`
            // noch regulär per Konsens abschließen.
            self.msg_budget.mark_exhausted();
            self.reject(message, DeliveryResult::LimitReached);
            return DeliveryResult::LimitReached;
        }
        match target.try_deliver(message.clone()) {
            DeliveryResult::Delivered => {
                self.swarm_bus
                    .publish(SwarmEvent::MessageQueued { message });
                DeliveryResult::Delivered
            }
            other => {
                // Fehlzustellung gibt das Budget zurück: `max_messages` zählt
                // erfolgreiche Zustellungen — sonst würden retrybare
                // postfach_voll-Sends das Limit aufzehren, ohne dass je eine
                // Nachricht ankommt.
                if budgeted {
                    self.msg_budget.refund();
                }
                self.reject(message, other);
                other
            }
        }
    }

    /// Abgelehnte Nachricht: Event publizieren und als Dead Letter ablegen.
    pub fn reject(&self, message: SwarmMessage, reason: DeliveryResult) {
        self.swarm_bus.publish(SwarmEvent::MessageRejected {
            message: message.clone(),
            result: reason,
        });
        self.dead_letters
            .lock()
            .unwrap()
            .push(DeadLetter { message, reason });
    }
}

/// Fehler-Sentinels von `Agent::run_on_bus` — `run_on_bus` liefert einen
/// `String` (kein `Result`); diese drei Rückgaben sind agentkits stabile
/// Verhaltenskontrakte für Abbruch/Fehler. Exact-Match ist bewusst: das
/// theoretische Restrisiko, dass ein Modell wortwörtlich so antwortet, ist
/// akzeptiert (README, "Bewusste Design-Entscheidungen").
fn is_failure_sentinel(answer: &str) -> bool {
    matches!(
        answer,
        "(abgebrochen)" | "(keine Antwort)" | "(max_steps erreicht)"
    )
}

/// Verbleibende Mailbox-Einträge beim Shutdown in Dead Letters überführen —
/// nichts geht stillschweigend verloren.
fn drain_to_dead_letters(rx: &Receiver<AgentCommand>, ctx: &SwarmToolContext) {
    while let Ok(command) = rx.try_recv() {
        if let AgentCommand::Deliver(message) = command {
            ctx.reject(message, DeliveryResult::RecipientUnavailable);
        }
    }
}

/// Der Actor-Loop — läuft in einem eigenen Thread und besitzt den Agenten
/// exklusiv. Bewusst eine freie Funktion statt eines Actor-Structs mit
/// Methoden: der Loop ist der ganze Actor.
///
/// `recv_timeout` statt `recv`: der Shutdown muss auch einen Actor erreichen,
/// dessen Mailbox VOLL ist (das Shutdown-Kommando ist dann nicht enqueubar)
/// oder der leer blockiert — und Sender-Drop als Weckruf ist unmöglich, weil
/// Peer-Refs in den Tool-Closures der anderen Agenten leben. Der 100-ms-Takt
/// ist die langweilige, robuste Antwort; `Shutdown` bleibt der schnelle Pfad.
pub(crate) fn actor_loop(
    mut agent: Agent,
    rx: Receiver<AgentCommand>,
    ctx: Arc<SwarmToolContext>,
    stop: Arc<AtomicBool>,
    agent_bus: EventBus,
    turn_seq: Arc<AtomicI64>,
) {
    ctx.swarm_bus.publish(SwarmEvent::ActorStarted {
        agent: ctx.self_id.clone(),
    });
    loop {
        if stop.load(Ordering::SeqCst) {
            drain_to_dead_letters(&rx, &ctx);
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(AgentCommand::Deliver(message)) => {
                if stop.load(Ordering::SeqCst) {
                    ctx.reject(message, DeliveryResult::RecipientUnavailable);
                    drain_to_dead_letters(&rx, &ctx);
                    break;
                }
                ctx.swarm_bus.publish(SwarmEvent::MessageDequeued {
                    agent: ctx.self_id.clone(),
                    message_id: message.id.clone(),
                });
                *ctx.current.lock().unwrap() = Some(message.clone());
                // Stop-Knopf für den neuen Turn zurücksetzen — und die Race
                // mit dem Shutdown schließen: die Laufzeit setzt erst `stop`,
                // dann `cancel`; sähe der Re-Check hier `stop` nicht, läge
                // das `cancel=true` der Laufzeit (SeqCst-Totalordnung) sicher
                // NACH unserem Reset und bleibt erhalten.
                ctx.cancel.store(false, Ordering::SeqCst);
                if stop.load(Ordering::SeqCst) {
                    ctx.cancel.store(true, Ordering::SeqCst);
                }
                ctx.swarm_bus.publish(SwarmEvent::TurnStarted {
                    agent: ctx.self_id.clone(),
                    message_id: message.id.clone(),
                });
                let seq = turn_seq.fetch_add(1, Ordering::SeqCst);
                let answer = agent.run_on_bus(
                    &message.render_prompt(),
                    &agent_bus,
                    seq,
                    Some(&ctx.cancel),
                    &ctx.self_id,
                );
                ctx.swarm_bus.publish(SwarmEvent::TurnCompleted {
                    agent: ctx.self_id.clone(),
                    message_id: message.id,
                    success: !is_failure_sentinel(&answer),
                });
            }
            Ok(AgentCommand::CancelCurrentTurn) => {
                ctx.cancel.store(true, Ordering::SeqCst);
            }
            Ok(AgentCommand::Shutdown) => {
                drain_to_dead_letters(&rx, &ctx);
                break;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    ctx.swarm_bus.publish(SwarmEvent::ActorStopped {
        agent: ctx.self_id.clone(),
    });
}
