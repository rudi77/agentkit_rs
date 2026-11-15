//! Pflege durch den Menschen: vergessen, zusammenführen, umbenennen.
//!
//! Anders als alles in `write.rs` LÖSCHEN diese Eingriffe Datensätze. Sie
//! gehören deshalb nicht zu den Kommandos, die ein Tool absetzen kann, und
//! laufen nicht über einzelne Ops, sondern über [`crate::GraphStore::rewrite`]:
//! jede Funktion hier liefert den vollständigen neuen Bestand. Reine
//! Funktionen über einem Snapshot, ohne Datei.

use std::collections::HashSet;

use crate::error::GraphError;
use crate::model::{normalize, GraphClaim, GraphEntity, GraphLayer, GraphRevision, GraphScope};
use crate::store::{GraphIndex, GraphOp};

fn records(
    index: &GraphIndex,
) -> (
    Vec<GraphEntity>,
    Vec<GraphClaim>,
    Vec<GraphOp>,
    Vec<GraphOp>,
) {
    (
        index.entities().map(|e| (**e).clone()).collect(),
        index.claims().map(|c| (**c).clone()).collect(),
        index
            .sources()
            .map(|s| GraphOp::Source((**s).clone()))
            .collect(),
        index
            .episodes()
            .map(|e| GraphOp::Episode((**e).clone()))
            .collect(),
    )
}

/// Setzt den Bestand wieder zusammen und wirft dabei weg, was niemand mehr
/// braucht: Quellen ohne Aussage oder Episode, und Entities aus `maybe_orphaned`,
/// an denen keine Aussage mehr hängt.
fn assemble(
    entities: Vec<GraphEntity>,
    claims: Vec<GraphClaim>,
    sources: Vec<GraphOp>,
    episodes: Vec<GraphOp>,
    maybe_orphaned: &[String],
) -> Vec<GraphOp> {
    let mut used_sources: HashSet<&str> = claims
        .iter()
        .flat_map(|c| c.source_ids.iter().map(String::as_str))
        .collect();
    for op in &episodes {
        if let GraphOp::Episode(e) = op {
            used_sources.extend(e.source_ids.iter().map(String::as_str));
        }
    }
    let connected: HashSet<&str> = claims
        .iter()
        .flat_map(|c| [c.subject.as_str(), c.object.as_str()])
        .collect();

    let mut ops: Vec<GraphOp> = sources
        .into_iter()
        .filter(|op| matches!(op, GraphOp::Source(s) if used_sources.contains(s.id.as_str())))
        .collect();
    ops.extend(
        entities
            .iter()
            .filter(|e| !maybe_orphaned.contains(&e.id) || connected.contains(e.id.as_str()))
            .cloned()
            .map(GraphOp::Entity),
    );
    ops.extend(claims.iter().cloned().map(GraphOp::Claim));
    ops.extend(episodes);
    ops
}

/// Löscht eine Aussage endgültig — samt ihrer Quellen und Endpunkte, soweit
/// nichts anderes sie noch braucht. Verweise anderer Aussagen auf sie
/// (`superseded_by`) werden gelöst: die ID kann später neu vergeben werden.
pub fn forget_claim(
    index: &GraphIndex,
    claim_id: &str,
    revision: GraphRevision,
) -> Result<Vec<GraphOp>, GraphError> {
    let gone = index
        .claim(claim_id)
        .ok_or_else(|| GraphError::NotFound(format!("Claim '{claim_id}'")))?;
    let endpoints = [gone.subject.clone(), gone.object.clone()];
    let (entities, mut claims, sources, episodes) = records(index);
    claims.retain(|c| c.id != claim_id);
    for claim in &mut claims {
        if claim.superseded_by.as_deref() == Some(claim_id) {
            claim.superseded_by = None;
            claim.updated_revision = revision;
        }
    }
    Ok(assemble(entities, claims, sources, episodes, &endpoints))
}

/// Gibt einer Entity einen neuen Namen. Der alte bleibt als Alias, damit
/// Aussagen unter dem alten Namen sie weiterhin finden.
pub fn rename_entity(
    index: &GraphIndex,
    entity_id: &str,
    name: &str,
    revision: GraphRevision,
) -> Result<Vec<GraphOp>, GraphError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(GraphError::Invalid(
            "der neue Name darf nicht leer sein".into(),
        ));
    }
    if index.entity(entity_id).is_none() {
        return Err(GraphError::NotFound(format!("Entity '{entity_id}'")));
    }
    let (mut entities, claims, sources, episodes) = records(index);
    for entity in entities.iter_mut().filter(|e| e.id == entity_id) {
        entity.canonical_name = name.to_string();
        let alias = normalize(name);
        if !entity.aliases.contains(&alias) {
            entity.aliases.push(alias);
        }
        entity.updated_revision = revision;
    }
    Ok(assemble(entities, claims, sources, episodes, &[]))
}

/// Führt `from` in `into` zusammen: alle Aussagen über `from` gelten danach
/// `into`, dessen Aliase kommen dazu, `from` verschwindet. Entstehen dabei
/// gleiche Aussagen, bleibt die ältere und erbt die Quellen der anderen.
///
/// Nur innerhalb EINES Ziels: eine vorläufige Entity in eine dauerhafte zu
/// falten, höbe ihre Aussagen an der Promotion vorbei.
pub fn merge_entities(
    index: &GraphIndex,
    from: &str,
    into: &str,
    revision: GraphRevision,
) -> Result<Vec<GraphOp>, GraphError> {
    let entity = |id: &str| {
        index
            .entity(id)
            .ok_or_else(|| GraphError::NotFound(format!("Entity '{id}'")))
    };
    let (source, target) = (entity(from)?, entity(into)?);
    if from == into {
        return Err(GraphError::Invalid(
            "eine Entity lässt sich nicht mit sich selbst zusammenführen".into(),
        ));
    }
    if source.layer != target.layer || source.scope != target.scope {
        return Err(GraphError::Invalid(format!(
            "'{from}' liegt in {}@{}, '{into}' in {}@{} — zusammenführen geht nur innerhalb eines Ziels",
            source.layer, source.scope, target.layer, target.scope
        )));
    }
    let extra_aliases = source.aliases.clone();
    let (mut entities, claims, sources, episodes) = records(index);
    entities.retain(|e| e.id != from);
    for entity in entities.iter_mut().filter(|e| e.id == into) {
        for alias in &extra_aliases {
            if !entity.aliases.contains(alias) {
                entity.aliases.push(alias.clone());
            }
        }
        entity.updated_revision = revision;
    }

    let mut merged: Vec<GraphClaim> = Vec::new();
    for mut claim in claims {
        for endpoint in [&mut claim.subject, &mut claim.object] {
            if endpoint == from {
                *endpoint = into.to_string();
                claim.updated_revision = revision;
            }
        }
        let twin = merged.iter_mut().find(|c| {
            c.subject == claim.subject
                && c.object == claim.object
                && normalize(&c.predicate) == normalize(&claim.predicate)
                && c.layer == claim.layer
                && c.scope == claim.scope
                && c.status == claim.status
        });
        match twin {
            Some(kept) => {
                for id in claim.source_ids {
                    if !kept.source_ids.contains(&id) {
                        kept.source_ids.push(id);
                    }
                }
                kept.confidence = kept.confidence.max(claim.confidence);
                kept.updated_revision = revision;
            }
            None => merged.push(claim),
        }
    }
    // Ein verschwundener Zwilling kann noch als Nachfolger genannt sein.
    let alive: HashSet<String> = merged.iter().map(|c| c.id.clone()).collect();
    for claim in &mut merged {
        if claim
            .superseded_by
            .as_ref()
            .is_some_and(|id| !alive.contains(id))
        {
            claim.superseded_by = None;
            claim.updated_revision = revision;
        }
    }
    Ok(assemble(entities, merged, sources, episodes, &[]))
}

/// Räumt alte Arbeits-Scopes weg: von den vorläufigen `session`- und
/// `swarm`-Scopes bleiben die `keep` jüngsten (und immer `current`), der Rest
/// verschwindet samt Aussagen, Episoden, Quellen und verwaisten Entities.
///
/// Vorläufiges, das nie befördert wurde, ist nach ein paar Sitzungen niemandes
/// Wissen mehr — nur der nächste Lauf im selben Scope sähe es überhaupt. Ohne
/// das Aufräumen wüchse `working/` mit jeder Sitzung. `None`, wenn es nichts
/// zu räumen gibt (dann lohnt das Neuschreiben nicht).
pub fn prune_working_scopes(
    index: &GraphIndex,
    keep: usize,
    current: Option<&GraphScope>,
) -> Option<Vec<GraphOp>> {
    // Jüngste Aktivität je Scope — über die Revision, nicht über die Uhr.
    let mut latest: Vec<(GraphScope, GraphRevision)> = Vec::new();
    let mut touch = |scope: &GraphScope, revision: GraphRevision| {
        if !matches!(scope.kind.as_str(), "session" | "swarm") {
            return;
        }
        match latest.iter_mut().find(|(s, _)| s == scope) {
            Some((_, seen)) => *seen = (*seen).max(revision),
            None => latest.push((scope.clone(), revision)),
        }
    };
    for claim in index.claims().filter(|c| c.layer == GraphLayer::Working) {
        touch(&claim.scope, claim.updated_revision);
    }
    for entity in index.entities().filter(|e| e.layer == GraphLayer::Working) {
        touch(&entity.scope, entity.updated_revision);
    }
    for episode in index.episodes() {
        touch(&episode.scope, episode.created_revision);
    }
    latest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
    let stale: Vec<GraphScope> = latest
        .into_iter()
        .skip(keep)
        .map(|(scope, _)| scope)
        .filter(|scope| Some(scope) != current)
        .collect();
    if stale.is_empty() {
        return None;
    }

    let (entities, mut claims, sources, mut episodes) = records(index);
    claims.retain(|c| !(c.layer == GraphLayer::Working && stale.contains(&c.scope)));
    episodes.retain(|op| !matches!(op, GraphOp::Episode(e) if stale.contains(&e.scope)));
    // Vorläufige Entities der alten Scopes gehen mit, wenn nichts mehr an
    // ihnen hängt — eine dauerhafte Aussage kann sie noch brauchen.
    let maybe_orphaned: Vec<String> = entities
        .iter()
        .filter(|e| e.layer == GraphLayer::Working && stale.contains(&e.scope))
        .map(|e| e.id.clone())
        .collect();
    Some(assemble(
        entities,
        claims,
        sources,
        episodes,
        &maybe_orphaned,
    ))
}
