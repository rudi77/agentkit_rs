//! Retrieval: von der Frage zum Ausschnitt.
//!
//! ```text
//! Frage ──► Seeds (Alias exakt, sonst Token-Overlap)
//!        ──► 1–2 Hops ungerichtet
//!        ──► Ranking (Distanz, Konfidenz, Status, Sicht-Priorität, Aktualität)
//!        ──► Token-Budget
//!        ──► <graph-context>
//! ```
//!
//! Kein Modellaufruf, keine Embeddings, kein Volltextindex — Alias-Treffer plus
//! Token-Overlap, dieselbe Mechanik wie in agentkits `recall`. Das hält den
//! Lesepfad synchron, offline und deterministisch: gleiche Revision + gleiche
//! Sicht + gleiche Frage ⇒ gleiche Reihenfolge.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agentkit::count_tokens_text;

use crate::model::{
    id_order, normalize, tokens, ClaimStatus, EntityId, GraphClaim, GraphEntity, GraphRevision,
    GraphSource, GraphView,
};
use crate::store::GraphIndex;

/// Obergrenze für automatisch bestimmte Seeds. Mehr Startpunkte heißt fast immer
/// mehr Rauschen statt mehr Antwort.
const MAX_AUTO_SEEDS: usize = 6;

/// Länge des Wortstamms für den unscharfen Treffer. Deutsche Flexion hängt fast
/// immer hinten an ("parallele/parallelen", "Aufruf/Aufrufe"), und ein exakter
/// Token-Vergleich würde deshalb genau die Fragen verfehlen, die ein Agent
/// wirklich stellt. Fünf Zeichen sind der Kompromiss: lang genug, dass "Session"
/// und "Sequenz" auseinanderfallen, kurz genug für die üblichen Endungen.
const STEM_LEN: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct GraphQuery {
    /// Freitext (auch leer, wenn nur über `seeds` gefragt wird).
    pub text: String,
    /// Explizite Startpunkte: Entity-IDs oder Namen/Aliase.
    pub seeds: Vec<String>,
    pub max_hops: usize,
    pub max_entities: usize,
    pub max_claims: usize,
    pub max_tokens: usize,
    /// Leer = alle Status. Sonst nur die genannten.
    pub include_statuses: Vec<ClaimStatus>,
}

impl Default for GraphQuery {
    fn default() -> Self {
        GraphQuery {
            text: String::new(),
            seeds: Vec::new(),
            max_hops: 2,
            max_entities: 20,
            max_claims: 30,
            max_tokens: 4000,
            include_statuses: Vec::new(),
        }
    }
}

impl GraphQuery {
    pub fn text(text: &str) -> Self {
        GraphQuery {
            text: text.to_string(),
            ..Default::default()
        }
    }

    pub fn seeded(entity: &str) -> Self {
        GraphQuery {
            seeds: vec![entity.to_string()],
            ..Default::default()
        }
    }

    pub fn hops(mut self, hops: usize) -> Self {
        self.max_hops = hops;
        self
    }

    pub fn limits(mut self, max_claims: usize, max_tokens: usize) -> Self {
        self.max_claims = max_claims;
        self.max_tokens = max_tokens;
        self
    }

    fn allows(&self, status: ClaimStatus) -> bool {
        self.include_statuses.is_empty() || self.include_statuses.contains(&status)
    }
}

/// Ein Claim mit seinem Rang und seiner Entfernung zum nächsten Seed.
#[derive(Debug, Clone)]
pub struct ScoredClaim {
    pub claim: Arc<GraphClaim>,
    pub score: f32,
    pub hop: usize,
}

/// Das Ergebnis einer Abfrage — ein kleiner, in sich geschlossener Ausschnitt.
#[derive(Debug, Clone, Default)]
pub struct Subgraph {
    pub revision: GraphRevision,
    pub seeds: Vec<EntityId>,
    pub entities: Vec<Arc<GraphEntity>>,
    pub claims: Vec<ScoredClaim>,
    /// true ⇔ es gab mehr Treffer, als Claim- oder Token-Budget zuließen.
    pub truncated: bool,
}

impl Subgraph {
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

/// Volle Abfrage: Seeds bestimmen, traversieren, ranken, kürzen.
pub fn search(index: &GraphIndex, view: &GraphView, query: &GraphQuery) -> Subgraph {
    let seeds = find_seeds(index, view, query);
    neighborhood(index, view, &seeds, query)
}

/// Startpunkte einer Abfrage. Explizite `seeds` gewinnen; sonst entscheidet der
/// Token-Overlap mit Name, Aliasen, Typ und Beschreibung.
pub fn find_seeds(index: &GraphIndex, view: &GraphView, query: &GraphQuery) -> Vec<EntityId> {
    let mut seeds: Vec<EntityId> = Vec::new();
    for wanted in &query.seeds {
        if index.sees_entity(wanted, view) {
            push_unique(&mut seeds, wanted.clone());
        } else if let Some(hit) = index.by_alias(wanted, view).first() {
            push_unique(&mut seeds, hit.id.clone());
        }
    }
    if !seeds.is_empty() || query.text.trim().is_empty() {
        return seeds;
    }

    let needle = normalize(&query.text);
    let query_tokens = token_set(&query.text);
    let mut scored: Vec<(f32, usize, EntityId)> = Vec::new();
    for entity in index.entities() {
        if !view.sees(entity.layer, &entity.scope) {
            continue;
        }
        let mut score = 0.0f32;
        // Kommt der ganze Name in der Frage vor, ist das der stärkste Hinweis.
        for alias in entity.aliases.iter() {
            if !alias.is_empty() && needle.contains(alias.as_str()) {
                score += 3.0;
                break;
            }
        }
        let mut haystack = entity.canonical_name.clone();
        haystack.push(' ');
        haystack.push_str(&entity.entity_type);
        for alias in &entity.aliases {
            haystack.push(' ');
            haystack.push_str(alias);
        }
        if let Some(desc) = &entity.description {
            haystack.push(' ');
            haystack.push_str(desc);
        }
        score += overlap(&token_set(&haystack), &query_tokens);
        if score > 0.0 {
            let priority = view
                .position(entity.layer, &entity.scope)
                .unwrap_or(usize::MAX);
            scored.push((score, priority, entity.id.clone()));
        }
    }
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| id_order(&a.2).cmp(&id_order(&b.2)))
    });
    scored
        .into_iter()
        .take(MAX_AUTO_SEEDS.min(query.max_entities.max(1)))
        .map(|(_, _, id)| id)
        .collect()
}

/// Ungerichtete Traversierung um die Seeds herum, gerankt und budgetiert.
pub fn neighborhood(
    index: &GraphIndex,
    view: &GraphView,
    seeds: &[EntityId],
    query: &GraphQuery,
) -> Subgraph {
    let mut result = Subgraph {
        revision: index.revision(),
        seeds: seeds.to_vec(),
        ..Default::default()
    };
    if seeds.is_empty() {
        return result;
    }

    let mut hop_of: HashMap<EntityId, usize> = HashMap::new();
    for seed in seeds {
        hop_of.insert(seed.clone(), 0);
    }
    let mut frontier: Vec<EntityId> = seeds.to_vec();
    let mut found: Vec<(Arc<GraphClaim>, usize)> = Vec::new();
    let mut seen_claims: HashSet<String> = HashSet::new();

    for hop in 1..=query.max_hops.max(1) {
        let mut next: Vec<EntityId> = Vec::new();
        for entity_id in &frontier {
            for claim in index.incident_claims(entity_id) {
                if !index.sees_claim(claim, view) || !query.allows(claim.status) {
                    continue;
                }
                if seen_claims.insert(claim.id.clone()) {
                    found.push((claim.clone(), hop));
                }
                let other = if &claim.subject == entity_id {
                    &claim.object
                } else {
                    &claim.subject
                };
                if !hop_of.contains_key(other) && hop_of.len() < query.max_entities.max(1) {
                    hop_of.insert(other.clone(), hop);
                    next.push(other.clone());
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    let total = index.revision().max(1) as f32;
    let query_tokens = token_set(&query.text);
    let mut scored: Vec<ScoredClaim> = found
        .into_iter()
        .map(|(claim, hop)| {
            let score = score_claim(index, view, &claim, hop, &query_tokens, total);
            ScoredClaim { claim, score, hop }
        })
        .collect();
    // Totale Ordnung: Score, dann numerische ID — nie „zufällig gleich".
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| id_order(&a.claim.id).cmp(&id_order(&b.claim.id)))
            .then_with(|| a.claim.id.cmp(&b.claim.id))
    });

    // Budget: erst die Claim-Zahl, dann die Tokens der gerenderten Zeilen.
    let mut kept: Vec<ScoredClaim> = Vec::new();
    let mut used = 0usize;
    for item in scored {
        if kept.len() >= query.max_claims {
            result.truncated = true;
            break;
        }
        let cost = count_tokens_text(&render_claim(index, &item.claim));
        if !kept.is_empty() && used + cost > query.max_tokens {
            result.truncated = true;
            break;
        }
        used += cost;
        kept.push(item);
    }

    // Nur die Entities, die im gekürzten Ergebnis wirklich vorkommen.
    let mut entity_ids: Vec<EntityId> = Vec::new();
    for item in &kept {
        push_unique(&mut entity_ids, item.claim.subject.clone());
        push_unique(&mut entity_ids, item.claim.object.clone());
    }
    result.entities = entity_ids
        .iter()
        .filter_map(|id| index.entity(id).cloned())
        .collect();
    result.claims = kept;
    result
}

/// Die Quellen eines Claims (Provenance-Abfrage).
pub fn evidence(index: &GraphIndex, claim_id: &str) -> Vec<Arc<GraphSource>> {
    index
        .claim(claim_id)
        .map(|claim| {
            claim
                .source_ids
                .iter()
                .filter_map(|id| index.source(id).cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Exakte Tokens plus ihre Stämme. Beide Overlap-Stellen (Seeds und Ranking)
/// vergleichen dasselbe Objekt — sonst driften Fund und Rang auseinander.
struct TokenSet {
    exact: HashSet<String>,
    stems: HashSet<String>,
}

fn token_set(text: &str) -> TokenSet {
    let mut exact = HashSet::new();
    let mut stems = HashSet::new();
    for token in tokens(text) {
        stems.insert(token.chars().take(STEM_LEN).collect::<String>());
        exact.insert(token);
    }
    TokenSet { exact, stems }
}

/// Exakter Treffer zählt voll, ein Treffer nur über den Stamm halb.
fn overlap(a: &TokenSet, b: &TokenSet) -> f32 {
    let exact = a.exact.intersection(&b.exact).count();
    let stems = a.stems.intersection(&b.stems).count();
    exact as f32 + 0.5 * stems.saturating_sub(exact) as f32
}

fn score_claim(
    index: &GraphIndex,
    view: &GraphView,
    claim: &GraphClaim,
    hop: usize,
    query_tokens: &TokenSet,
    total_revision: f32,
) -> f32 {
    // Nähe zum Seed ist das stärkste Signal, dann die Belastbarkeit.
    let distance = 1.5 / hop as f32;
    let status = claim.status.rank_bonus();
    let priority = match view.position(claim.layer, &claim.scope) {
        Some(pos) => 0.5 / (pos as f32 + 1.0),
        None => 0.0,
    };
    // Aktualität über die Revision statt über die Uhr — deterministisch.
    let recency = 0.2 * (claim.updated_revision as f32 / total_revision);
    let mut text = claim.predicate.clone();
    for endpoint in [&claim.subject, &claim.object] {
        if let Some(entity) = index.entity(endpoint) {
            text.push(' ');
            text.push_str(&entity.canonical_name);
        }
    }
    let lexical = 0.25 * overlap(&token_set(&text), query_tokens);
    distance + claim.confidence + status + priority + recency + lexical
}

/// Serialisiert einen Ausschnitt für den Modellkontext.
///
/// Beschriftungen deutsch (Sprachkonvention), Werte in ihrer Wire-Schreibweise —
/// dieselben Zeichenketten, die das Modell in `graph_remember` zurückgibt.
pub fn render(index: &GraphIndex, subgraph: &Subgraph, include_sources: bool) -> String {
    if subgraph.is_empty() {
        return format!("<graph-context revision=\"{}\"/>", subgraph.revision);
    }
    let mut out = format!(
        "<graph-context revision=\"{}\" claims=\"{}\">\n",
        subgraph.revision,
        subgraph.claims.len()
    );
    for item in &subgraph.claims {
        out.push_str(&render_claim(index, &item.claim));
        if include_sources && !item.claim.source_ids.is_empty() {
            out.push_str(&format!(
                "  Quellen: {}\n",
                item.claim.source_ids.join(", ")
            ));
        }
    }
    if subgraph.truncated {
        out.push_str("  … weitere Treffer wegen Budget weggelassen\n");
    }
    out.push_str("</graph-context>");
    out
}

fn render_claim(index: &GraphIndex, claim: &GraphClaim) -> String {
    let name = |id: &str| {
        index
            .entity(id)
            .map(|e| e.canonical_name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let mut line = format!(
        "[{}] ({}) --[{}]--> ({})\n  Status: {} | Vertrauen: {:.2} | Ebene: {}\n",
        claim.id,
        name(&claim.subject),
        claim.predicate,
        name(&claim.object),
        claim.status,
        claim.confidence,
        claim.layer,
    );
    if let Some(newer) = &claim.superseded_by {
        line.push_str(&format!("  Ersetzt durch: {newer}\n"));
    }
    line
}

fn push_unique(list: &mut Vec<EntityId>, id: EntityId) {
    if !list.contains(&id) {
        list.push(id);
    }
}
