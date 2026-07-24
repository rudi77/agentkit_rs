//! Mailbox eines Actors — ein begrenzter `sync_channel`.
//!
//! Begrenzt (bounded), damit Agenten einander nicht schneller Nachrichten
//! schicken können, als sie verarbeitet werden: eine volle Mailbox erzeugt
//! Backpressure als weichen Fehler (`postfach_voll`) statt unbegrenztem
//! Speicherwachstum. Gesendet wird nirgends blockierend (`try_send`) — außer
//! bei der garantierten Initialaufgabe.

use crate::message::AgentCommand;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

/// Voreinstellung für die Mailbox-Kapazität (per Builder überschreibbar).
pub const DEFAULT_MAILBOX_CAPACITY: usize = 16;

/// Erzeugt eine Mailbox: Sender (wird zu [`crate::AgentActorRef`]s geklont)
/// und Empfänger (gehört exklusiv dem Actor-Thread).
pub fn mailbox(capacity: usize) -> (SyncSender<AgentCommand>, Receiver<AgentCommand>) {
    sync_channel(capacity)
}
