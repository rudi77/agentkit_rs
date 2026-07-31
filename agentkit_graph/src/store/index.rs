//! Der unveränderliche Snapshot: alle Datensätze plus die Indizes darauf.
//!
//! Ein [`GraphIndex`] wird nie an Ort und Stelle geändert. Der Schreiber baut aus
//! dem aktuellen Snapshot einen neuen ([`GraphIndex::with_ops`]) und tauscht ihn
//! ein; Leser halten so lange ihren alten `Arc` wie sie wollen. Daraus folgt
//! direkt die Snapshot-Isolation aus dem PRD: eine mehrstufige Traversierung
//! sieht garantiert einen konsistenten Stand, ohne Transaktionsklammer und ohne
//! irgendeinen Schreiber zu blockieren.
//!
//! Kosten: der Neubau kopiert Vektoren aus `Arc`s (Zeigerkopien) und die
//! Hash-Indizes (flache Kopie der Schlüssel). Das ist O(n) pro Mutation und bei
//! den hier erwarteten Größen (≤ ~10⁵ Claims) im einstelligen Millisekundenbereich
//! — die Grenze steht bewusst in der README.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{
    normalize, ClaimId, EntityId, EpisodeId, GraphClaim, GraphEntity, GraphEpisode, GraphLayer,
    GraphRevision, GraphScope, GraphSource, GraphView, SourceId,
};

/// Die kleinste Einheit einer Änderung: ein vollständiger Datensatz.
///
/// Dieselbe Repräsentation dient dem Journal als Zeile und dem Index als
/// Anwendungsschritt — deshalb ist ein Replay nichts anderes als das Anwenden
/// derselben Ops in derselben Reihenfolge, und das Neuschreiben des Journals kann
/// den aktuellen Stand einfach als Op-Folge ausgeben.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum GraphOp {
    Entity(GraphEntity),
    Claim(GraphClaim),
    Source(GraphSource),
    Episode(GraphEpisode),
}

/// Alle Datensätze eines Stores zu einer Revision.
#[derive(Debug, Clone, Default)]
pub struct GraphIndex {
    revision: GraphRevision,
    entities: Vec<Arc<GraphEntity>>,
    claims: Vec<Arc<GraphClaim>>,
    sources: Vec<Arc<GraphSource>>,
    episodes: Vec<Arc<GraphEpisode>>,
    entity_pos: HashMap<EntityId, usize>,
    claim_pos: HashMap<ClaimId, usize>,
    source_pos: HashMap<SourceId, usize>,
    episode_pos: HashMap<EpisodeId, usize>,
    /// Normalisiertes Alias -> Positionen in `entities`.
    alias_pos: HashMap<String, Vec<usize>>,
    /// Entity-ID -> Positionen in `claims` (beide Richtungen; die Richtung steht
    /// im Claim selbst). Die Traversierung ist ungerichtet — „was hängt an X?"
    /// ist die Frage, nicht „was zeigt von X weg?".
    incident: HashMap<EntityId, Vec<usize>>,
}

impl GraphIndex {
    pub fn revision(&self) -> GraphRevision {
        self.revision
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn claim_count(&self) -> usize {
        self.claims.len()
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn episode_count(&self) -> usize {
        self.episodes.len()
    }

    /// Gesamtzahl der Datensätze — Maß für den Neuschreib-Schwellwert des Journals.
    pub fn record_count(&self) -> usize {
        self.entities.len() + self.claims.len() + self.sources.len() + self.episodes.len()
    }

    pub fn entity(&self, id: &str) -> Option<&Arc<GraphEntity>> {
        self.entity_pos.get(id).map(|p| &self.entities[*p])
    }

    pub fn claim(&self, id: &str) -> Option<&Arc<GraphClaim>> {
        self.claim_pos.get(id).map(|p| &self.claims[*p])
    }

    pub fn source(&self, id: &str) -> Option<&Arc<GraphSource>> {
        self.source_pos.get(id).map(|p| &self.sources[*p])
    }

    pub fn episode(&self, id: &str) -> Option<&Arc<GraphEpisode>> {
        self.episode_pos.get(id).map(|p| &self.episodes[*p])
    }

    /// Alle Entities in Einfügereihenfolge (= aufsteigende ID).
    pub fn entities(&self) -> impl Iterator<Item = &Arc<GraphEntity>> {
        self.entities.iter()
    }

    pub fn claims(&self) -> impl Iterator<Item = &Arc<GraphClaim>> {
        self.claims.iter()
    }

    pub fn episodes(&self) -> impl Iterator<Item = &Arc<GraphEpisode>> {
        self.episodes.iter()
    }

    /// Alle Quellen in Einfügereihenfolge.
    ///
    /// Bis dahin gab es nur den Einzel-Lookup [`GraphIndex::source`] — der
    /// reicht, solange man von einem Claim ausgeht. Wer den GANZEN Graphen
    /// ausgibt (`crate::export`), braucht die Quellen aber ohne vorherige
    /// Claim-Traversierung: eine Provenance-Anzeige löst `source_ids` selbst auf.
    pub fn sources(&self) -> impl Iterator<Item = &Arc<GraphSource>> {
        self.sources.iter()
    }

    /// Alle Claims, an denen die Entity beteiligt ist (Subjekt oder Objekt).
    pub fn incident_claims(&self, entity_id: &str) -> impl Iterator<Item = &Arc<GraphClaim>> {
        self.incident
            .get(entity_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|p| &self.claims[*p])
    }

    /// Entities, deren normalisiertes Alias exakt passt — gefiltert auf die Sicht.
    pub fn by_alias(&self, alias: &str, view: &GraphView) -> Vec<&Arc<GraphEntity>> {
        let key = normalize(alias);
        let mut hits: Vec<&Arc<GraphEntity>> = self
            .alias_pos
            .get(&key)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|p| &self.entities[*p])
            .filter(|e| view.sees(e.layer, &e.scope))
            .collect();
        // Deterministische Auswahl: höchste Sicht-Priorität, dann kanonisch vor
        // vorläufig, dann die ältere (kleinere) ID.
        hits.sort_by(|a, b| {
            let pa = view.position(a.layer, &a.scope).unwrap_or(usize::MAX);
            let pb = view.position(b.layer, &b.scope).unwrap_or(usize::MAX);
            pa.cmp(&pb)
                .then_with(|| layer_rank(a.layer).cmp(&layer_rank(b.layer)))
                .then_with(|| crate::model::id_order(&a.id).cmp(&crate::model::id_order(&b.id)))
        });
        hits
    }

    pub fn sees_entity(&self, id: &str, view: &GraphView) -> bool {
        self.entity(id)
            .is_some_and(|e| view.sees(e.layer, &e.scope))
    }

    pub fn sees_claim(&self, claim: &GraphClaim, view: &GraphView) -> bool {
        view.sees(claim.layer, &claim.scope)
    }

    /// Sichtbare Claims mit gleichem Subjekt und Prädikat in einem bestimmten Ziel
    /// — die Grundlage der Widerspruchsprüfung bei der Promotion.
    pub fn contradicting(
        &self,
        claim: &GraphClaim,
        layer: GraphLayer,
        scope: &GraphScope,
    ) -> Vec<&Arc<GraphClaim>> {
        self.incident_claims(&claim.subject)
            .filter(|c| c.id != claim.id && c.layer == layer && &c.scope == scope)
            .filter(|c| c.contradicts(claim))
            .collect()
    }

    /// Wendet eine Op-Folge an und gibt den neuen Snapshot zurück (Copy-on-Write).
    pub fn with_ops(&self, ops: &[GraphOp], revision: GraphRevision) -> GraphIndex {
        let mut next = self.clone();
        for op in ops {
            next.apply(op.clone());
        }
        next.revision = revision;
        next
    }

    /// Anwenden einer einzelnen Op — Upsert über die ID. Wird beim Replay des
    /// Journals und beim Commit über denselben Pfad benutzt.
    pub(crate) fn apply(&mut self, op: GraphOp) {
        match op {
            GraphOp::Entity(entity) => {
                let aliases = entity.aliases.clone();
                let pos = match self.entity_pos.get(&entity.id) {
                    Some(p) => {
                        let p = *p;
                        self.entities[p] = Arc::new(entity);
                        p
                    }
                    None => {
                        let p = self.entities.len();
                        self.entity_pos.insert(entity.id.clone(), p);
                        self.entities.push(Arc::new(entity));
                        p
                    }
                };
                // Aliase werden nur ergänzt: ein Alias verschwindet im MVP nie,
                // und ein Eintrag zeigt weiterhin auf dieselbe Position.
                for alias in aliases {
                    let slot = self.alias_pos.entry(alias).or_default();
                    if !slot.contains(&pos) {
                        slot.push(pos);
                    }
                }
            }
            GraphOp::Claim(claim) => match self.claim_pos.get(&claim.id) {
                Some(p) => {
                    let p = *p;
                    self.claims[p] = Arc::new(claim);
                }
                None => {
                    let pos = self.claims.len();
                    self.claim_pos.insert(claim.id.clone(), pos);
                    for endpoint in [claim.subject.clone(), claim.object.clone()] {
                        let slot = self.incident.entry(endpoint).or_default();
                        if !slot.contains(&pos) {
                            slot.push(pos);
                        }
                    }
                    self.claims.push(Arc::new(claim));
                }
            },
            GraphOp::Source(source) => match self.source_pos.get(&source.id) {
                Some(p) => self.sources[*p] = Arc::new(source),
                None => {
                    self.source_pos
                        .insert(source.id.clone(), self.sources.len());
                    self.sources.push(Arc::new(source));
                }
            },
            GraphOp::Episode(episode) => match self.episode_pos.get(&episode.id) {
                Some(p) => self.episodes[*p] = Arc::new(episode),
                None => {
                    self.episode_pos
                        .insert(episode.id.clone(), self.episodes.len());
                    self.episodes.push(Arc::new(episode));
                }
            },
        }
    }

    pub(crate) fn set_revision(&mut self, revision: GraphRevision) {
        self.revision = revision;
    }

    /// Der aktuelle Stand als Op-Folge — Grundlage für das Neuschreiben des
    /// Journals. Reihenfolge: Quellen und Entities vor den Claims, die sie
    /// referenzieren (nur der Lesbarkeit wegen; der Replay ist reihenfolgefrei).
    pub(crate) fn to_ops(&self) -> Vec<GraphOp> {
        let mut ops = Vec::with_capacity(self.record_count());
        ops.extend(self.sources.iter().map(|s| GraphOp::Source((**s).clone())));
        ops.extend(self.entities.iter().map(|e| GraphOp::Entity((**e).clone())));
        ops.extend(self.claims.iter().map(|c| GraphOp::Claim((**c).clone())));
        ops.extend(
            self.episodes
                .iter()
                .map(|e| GraphOp::Episode((**e).clone())),
        );
        ops
    }
}

/// Kanonisch schlägt vorläufig, wenn beide gleich priorisiert sichtbar sind.
fn layer_rank(layer: GraphLayer) -> u8 {
    match layer {
        GraphLayer::Canonical => 0,
        GraphLayer::Working => 1,
    }
}
