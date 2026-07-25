//! Peer-Verzeichnis eines Agenten — die zugeteilten [`AgentActorRef`]s.
//!
//! Topologie wird als Capability umgesetzt: die Laufzeit legt hier NUR die
//! erlaubten Peers hinein; ein nicht verbundener Agent ist für das Modell
//! schlicht unerreichbar (kein Ref = keine Kommunikationsmöglichkeit). Das ist
//! stärker als eine Laufzeitprüfung gegen einen Graphen.

use crate::actor_ref::AgentActorRef;
use crate::message::AgentId;
use std::collections::HashMap;

pub type PeerDirectory = HashMap<AgentId, AgentActorRef>;

/// Sortierte Peer-IDs — deterministische Reihenfolge für `swarm_peers`,
/// Broadcast-Zustellung und Tests.
pub fn sorted_peer_ids(peers: &PeerDirectory) -> Vec<AgentId> {
    let mut ids: Vec<AgentId> = peers.keys().cloned().collect();
    ids.sort();
    ids
}
