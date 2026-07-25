//! Statische Kommunikations-Topologie — wer darf mit wem reden.
//!
//! Die Topologie wird beim Start in Capabilities übersetzt: jeder Agent erhält
//! nur die [`crate::AgentActorRef`]s seiner Nachbarn (siehe `directory.rs`).
//! Danach wird der Graph selbst nicht mehr konsultiert.

use crate::error::SwarmError;
use crate::message::AgentId;
use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub struct Topology {
    /// Gerichtete Kanten; `connect_bidirectional` legt beide Richtungen an.
    edges: HashSet<(AgentId, AgentId)>,
}

impl Topology {
    pub fn new() -> Self {
        Topology::default()
    }

    /// Verbindet zwei Agenten in beide Richtungen.
    pub fn connect_bidirectional(&mut self, a: &str, b: &str) {
        self.edges.insert((a.to_string(), b.to_string()));
        self.edges.insert((b.to_string(), a.to_string()));
    }

    /// Die direkten Nachbarn eines Agenten (sortiert, deterministisch).
    pub fn peers_of(&self, id: &str) -> Vec<AgentId> {
        let mut peers: Vec<AgentId> = self
            .edges
            .iter()
            .filter(|(from, _)| from == id)
            .map(|(_, to)| to.clone())
            .collect();
        peers.sort();
        peers
    }

    /// Prüft, dass jede Kante nur registrierte Agenten referenziert.
    /// Isolierte Agenten sind erlaubt (sie können nur empfangen bzw. über
    /// `swarm_propose`/`swarm_vote` am Abschluss mitwirken).
    pub fn validate(&self, agents: &[AgentId]) -> Result<(), SwarmError> {
        for (from, to) in &self.edges {
            for endpoint in [from, to] {
                if !agents.contains(endpoint) {
                    return Err(SwarmError::InvalidTopology(format!(
                        "Kante verweist auf unbekannten Agenten '{endpoint}'"
                    )));
                }
            }
        }
        Ok(())
    }
}
