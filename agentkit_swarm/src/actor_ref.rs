//! Sendefähige Referenz auf einen Actor — das Capability-Prinzip des Schwarms:
//! Wer keinen `AgentActorRef` besitzt, kann nicht senden. Andere Agenten
//! bekommen nie den Actor selbst (und damit nie Zugriff auf dessen `Agent`),
//! sondern nur diese Referenz.

use crate::message::{AgentCommand, AgentId, DeliveryResult, SwarmMessage};
use std::sync::mpsc::{SyncSender, TrySendError};

#[derive(Clone)]
pub struct AgentActorRef {
    pub id: AgentId,
    tx: SyncSender<AgentCommand>,
}

impl AgentActorRef {
    pub fn new(id: impl Into<AgentId>, tx: SyncSender<AgentCommand>) -> Self {
        AgentActorRef { id: id.into(), tx }
    }

    /// Nicht blockierender Zustellversuch einer fachlichen Nachricht.
    pub fn try_deliver(&self, message: SwarmMessage) -> DeliveryResult {
        match self.tx.try_send(AgentCommand::Deliver(message)) {
            Ok(()) => DeliveryResult::Delivered,
            Err(TrySendError::Full(_)) => DeliveryResult::MailboxFull,
            Err(TrySendError::Disconnected(_)) => DeliveryResult::RecipientUnavailable,
        }
    }

    /// Best-Effort-Kommando (Shutdown/Cancel). Eine volle Mailbox ist hier ok:
    /// der Actor-Loop prüft das Stop-Flag ohnehin per `recv_timeout`-Takt.
    pub fn try_command(&self, command: AgentCommand) -> bool {
        self.tx.try_send(command).is_ok()
    }

    /// Blockierendes Senden — nur für die garantiert zuzustellende
    /// Initialaufgabe (`send_initial` auf leerer Mailbox) gedacht.
    pub(crate) fn send_blocking(&self, message: SwarmMessage) -> Result<(), ()> {
        self.tx.send(AgentCommand::Deliver(message)).map_err(|_| ())
    }
}
