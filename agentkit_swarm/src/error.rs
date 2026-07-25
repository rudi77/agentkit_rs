//! Fehler der Schwarm-Laufzeit. Ein kleines Enum mit `Display` genügt —
//! bewusst keine Fehler-Trait-Hierarchie (Guidelines §4). Weiche Fehler im
//! Nachrichtenpfad (Mailbox voll, Limit, …) sind KEINE `SwarmError`s, sondern
//! Werte ([`crate::DeliveryResult`]), die das Modell als Status-JSON sieht.

use crate::message::AgentId;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwarmError {
    /// Agent-ID ist im Schwarm nicht registriert.
    UnknownAgent(AgentId),
    /// Agent-ID wurde doppelt registriert.
    DuplicateAgent(AgentId),
    /// Schwarm ohne Agenten.
    EmptySwarm,
    /// Topologie-Kante verweist auf einen unbekannten Agenten.
    InvalidTopology(String),
    /// Actor-Thread konnte nicht gestartet oder erreicht werden.
    ActorUnavailable(String),
}

impl fmt::Display for SwarmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwarmError::UnknownAgent(id) => write!(f, "unbekannter Agent '{id}'"),
            SwarmError::DuplicateAgent(id) => write!(f, "Agent '{id}' doppelt registriert"),
            SwarmError::EmptySwarm => write!(f, "Schwarm ohne Agenten"),
            SwarmError::InvalidTopology(e) => write!(f, "ungültige Topologie: {e}"),
            SwarmError::ActorUnavailable(e) => write!(f, "Actor nicht verfügbar: {e}"),
        }
    }
}

impl std::error::Error for SwarmError {}
