//! Der Schreibpfad: Kommandos, ihre Prüfungen und die daraus entstehenden Ops.
//!
//! Alles hier ist eine reine Funktion über einem Snapshot ([`apply`]) — sie
//! entscheidet, WAS geschrieben wird, aber schreibt nicht selbst. Der Store
//! kümmert sich um Reihenfolge, Journal und Sichtbarkeit. Das macht den ganzen
//! Schreibpfad ohne Datei und ohne Nebenläufigkeit testbar.
//!
//! Bewusst **synchron**: ein Commit hängt eine Handvoll Zeilen an eine Datei und
//! tauscht einen `Arc` — das ist schneller als jede Warteschlange, die man
//! davorsetzen könnte. Damit entfallen Writer-Actor, Queue-Tiefe,
//! Revisions-Timeout und der ganze `Accepted`-vs-`Committed`-Zustandsraum:
//! kommt ein Receipt zurück, ist die Mutation sichtbar (Read-your-writes).

use crate::access::GraphAccess;
use crate::error::GraphError;
use crate::model::{
    content_hash, normalize, now_ms, ClaimId, ClaimStatus, EntityId, EpisodeId, GraphClaim,
    GraphEntity, GraphEpisode, GraphRevision, GraphSource, GraphTarget, SourceId,
};
use crate::store::index::{GraphIndex, GraphOp};

/// Die Herkunft einer Mutation. Pflicht — es gibt keinen Konstruktor für einen
/// Claim ohne Quelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDraft {
    /// Freier Typ: `agent_turn`, `tool_result`, `document`, `test_run`, …
    pub source_type: String,
    pub excerpt: Option<String>,
    pub tool_call_id: Option<String>,
    pub artifact_uri: Option<String>,
}

impl SourceDraft {
    pub fn new(source_type: &str) -> Self {
        SourceDraft {
            source_type: source_type.to_string(),
            excerpt: None,
            tool_call_id: None,
            artifact_uri: None,
        }
    }

    pub fn excerpt(mut self, text: &str) -> Self {
        self.excerpt = Some(text.to_string());
        self
    }

    pub fn tool_call(mut self, id: &str) -> Self {
        self.tool_call_id = Some(id.to_string());
        self
    }

    pub fn artifact(mut self, uri: &str) -> Self {
        self.artifact_uri = Some(uri.to_string());
        self
    }
}

/// Ein zu schreibender Claim. Subjekt und Objekt sind **Namen**, keine IDs — die
/// Auflösung auf vorhandene Entities passiert im Schreibpfad (exakter Alias-Treffer,
/// sonst neue Entity).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimDraft {
    pub subject: String,
    pub subject_type: Option<String>,
    pub predicate: String,
    pub object: String,
    pub object_type: Option<String>,
    pub status: ClaimStatus,
    pub confidence: f32,
    pub source: SourceDraft,
}

impl ClaimDraft {
    pub fn new(subject: &str, predicate: &str, object: &str, source: SourceDraft) -> Self {
        ClaimDraft {
            subject: subject.to_string(),
            subject_type: None,
            predicate: predicate.to_string(),
            object: object.to_string(),
            object_type: None,
            status: ClaimStatus::Observation,
            confidence: 0.6,
            source,
        }
    }

    pub fn status(mut self, status: ClaimStatus) -> Self {
        self.status = status;
        self
    }

    pub fn confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn types(mut self, subject_type: &str, object_type: &str) -> Self {
        self.subject_type = Some(subject_type.to_string());
        self.object_type = Some(object_type.to_string());
        self
    }
}

/// Ein Ereignis („was ist passiert"), kein Wissen („was gilt").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeDraft {
    pub summary: String,
    pub source: SourceDraft,
}

impl EpisodeDraft {
    pub fn new(summary: &str, source: SourceDraft) -> Self {
        EpisodeDraft {
            summary: summary.to_string(),
            source,
        }
    }
}

/// Die drei Mutationen des MVP. Mehr Kommandos gibt es erst, wenn es Erzeuger
/// dafür gibt — `support`/`contest` brauchen eine Konsolidierung, die es noch
/// nicht gibt (siehe README, „Nicht im Umfang").
#[derive(Debug, Clone, PartialEq)]
pub enum GraphWriteCommand {
    /// Beobachtung oder Hypothese in den Working-Scope schreiben.
    RecordClaim(ClaimDraft),
    /// Ereignis protokollieren (wird nie promotet, nie traversiert).
    RecordEpisode(EpisodeDraft),
    /// Vorläufigen Claim ins Promotion-Ziel übernehmen.
    PromoteClaim { claim_id: ClaimId },
}

/// Ergebnis einer committeten Mutation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphReceipt {
    /// Revision, ab der die Mutation für jeden Leser sichtbar ist.
    pub revision: GraphRevision,
    pub claim_id: Option<ClaimId>,
    pub episode_id: Option<EpisodeId>,
    /// Beteiligte Entities (neu angelegte und wiederverwendete).
    pub entity_ids: Vec<EntityId>,
    /// Claims, die durch eine Promotion ersetzt wurden.
    pub superseded: Vec<ClaimId>,
    /// true ⇔ ein vorhandener Claim wurde um eine Quelle ergänzt, statt einen
    /// zweiten mit gleichem Inhalt anzulegen.
    pub deduplicated: bool,
}

/// Fortlaufende IDs je Datensatzart. Liegt im Store hinter dem Schreib-Mutex;
/// die Vergabe ist damit lückenlos und über einen Neustart hinweg stabil
/// (das Journal-Replay setzt die Zähler auf das Maximum).
#[derive(Debug, Clone, Default)]
pub(crate) struct IdCounters {
    pub entity: u64,
    pub claim: u64,
    pub source: u64,
    pub episode: u64,
}

impl IdCounters {
    fn next_entity(&mut self) -> EntityId {
        self.entity += 1;
        format!("E-{}", self.entity)
    }
    fn next_claim(&mut self) -> ClaimId {
        self.claim += 1;
        format!("C-{}", self.claim)
    }
    fn next_source(&mut self) -> SourceId {
        self.source += 1;
        format!("S-{}", self.source)
    }
    fn next_episode(&mut self) -> EpisodeId {
        self.episode += 1;
        format!("EP-{}", self.episode)
    }

    /// Zähler aus einer bereits vergebenen ID nachziehen (Journal-Replay).
    pub(crate) fn observe(&mut self, op: &GraphOp) {
        let (slot, id) = match op {
            GraphOp::Entity(e) => (&mut self.entity, &e.id),
            GraphOp::Claim(c) => (&mut self.claim, &c.id),
            GraphOp::Source(s) => (&mut self.source, &s.id),
            GraphOp::Episode(e) => (&mut self.episode, &e.id),
        };
        let n = crate::model::id_order(id);
        if n > *slot {
            *slot = n;
        }
    }
}

/// Prüft ein Kommando gegen Snapshot und Zugriff und liefert die zu schreibenden
/// Ops plus das Receipt. Schreibt nichts — der Aufrufer committet.
pub(crate) fn apply(
    index: &GraphIndex,
    command: GraphWriteCommand,
    access: &GraphAccess,
    revision: GraphRevision,
    ids: &mut IdCounters,
) -> Result<(Vec<GraphOp>, GraphReceipt), GraphError> {
    match command {
        GraphWriteCommand::RecordClaim(draft) => record_claim(index, draft, access, revision, ids),
        GraphWriteCommand::RecordEpisode(draft) => record_episode(draft, access, revision, ids),
        GraphWriteCommand::PromoteClaim { claim_id } => {
            promote_claim(index, &claim_id, access, revision)
        }
    }
}

fn write_target(access: &GraphAccess) -> Result<GraphTarget, GraphError> {
    access.write.clone().ok_or_else(|| {
        GraphError::Denied("dieser Zugriff darf nur lesen (kein Schreibziel gesetzt)".into())
    })
}

fn build_source(
    draft: &SourceDraft,
    access: &GraphAccess,
    revision: GraphRevision,
    ids: &mut IdCounters,
    at: u64,
) -> GraphSource {
    let hash_input = format!(
        "{}|{}|{}|{}",
        draft.source_type,
        draft.excerpt.as_deref().unwrap_or(""),
        draft.tool_call_id.as_deref().unwrap_or(""),
        draft.artifact_uri.as_deref().unwrap_or("")
    );
    GraphSource {
        id: ids.next_source(),
        source_type: draft.source_type.clone(),
        // Autor und Lauf kommen IMMER aus dem Zugriff, nie aus dem Draft.
        agent_id: Some(access.principal.clone()),
        run_id: access.run_id.clone(),
        tool_call_id: draft.tool_call_id.clone(),
        artifact_uri: draft.artifact_uri.clone(),
        excerpt: draft.excerpt.clone(),
        content_hash: content_hash(&hash_input),
        created_revision: revision,
        created_at: at,
    }
}

fn record_claim(
    index: &GraphIndex,
    draft: ClaimDraft,
    access: &GraphAccess,
    revision: GraphRevision,
    ids: &mut IdCounters,
) -> Result<(Vec<GraphOp>, GraphReceipt), GraphError> {
    let target = write_target(access)?;
    let subject = draft.subject.trim();
    let predicate = draft.predicate.trim();
    let object = draft.object.trim();
    if subject.is_empty() || predicate.is_empty() || object.is_empty() {
        return Err(GraphError::Invalid(
            "subject, predicate und object dürfen nicht leer sein".into(),
        ));
    }
    // `confirmed` entsteht ausschließlich durch Promotion, `superseded` nur als
    // deren Nebenwirkung — sonst könnte ein Modell sich selbst kanonisieren.
    if !matches!(
        draft.status,
        ClaimStatus::Observation | ClaimStatus::Hypothesis
    ) {
        return Err(GraphError::Invalid(format!(
            "status '{}' kann nicht direkt geschrieben werden (nur observation oder hypothesis)",
            draft.status
        )));
    }

    let at = now_ms();
    let mut ops = Vec::new();
    let source = build_source(&draft.source, access, revision, ids, at);
    let source_id = source.id.clone();
    ops.push(GraphOp::Source(source));

    let mut fresh: Vec<(String, EntityId)> = Vec::new();
    let subject_id = resolve_or_create(
        index,
        access,
        &target,
        subject,
        draft.subject_type.as_deref(),
        revision,
        ids,
        at,
        &mut ops,
        &mut fresh,
    );
    let object_id = resolve_or_create(
        index,
        access,
        &target,
        object,
        draft.object_type.as_deref(),
        revision,
        ids,
        at,
        &mut ops,
        &mut fresh,
    );

    // Gleiche Aussage im selben Ziel? Dann bekommt sie nur eine weitere Quelle.
    // Zwei identische Claims nebeneinander wären kein zusätzliches Wissen,
    // sondern doppeltes Gewicht beim Ranking.
    let existing = index
        .incident_claims(&subject_id)
        .find(|c| {
            c.layer == target.layer
                && c.scope == target.scope
                && c.subject == subject_id
                && c.object == object_id
                && normalize(&c.predicate) == normalize(predicate)
                && c.status != ClaimStatus::Superseded
        })
        .map(|c| (**c).clone());

    if let Some(mut claim) = existing {
        if !claim.source_ids.contains(&source_id) {
            claim.source_ids.push(source_id);
        }
        claim.confidence = claim.confidence.max(draft.confidence.clamp(0.0, 1.0));
        claim.updated_revision = revision;
        let receipt = GraphReceipt {
            revision,
            claim_id: Some(claim.id.clone()),
            entity_ids: vec![subject_id, object_id],
            deduplicated: true,
            ..Default::default()
        };
        ops.push(GraphOp::Claim(claim));
        return Ok((ops, receipt));
    }

    let claim = GraphClaim {
        id: ids.next_claim(),
        subject: subject_id.clone(),
        predicate: predicate.to_string(),
        object: object_id.clone(),
        layer: target.layer,
        scope: target.scope.clone(),
        status: draft.status,
        confidence: draft.confidence.clamp(0.0, 1.0),
        source_ids: vec![source_id],
        created_by: access.principal.clone(),
        superseded_by: None,
        promoted_from: None,
        created_revision: revision,
        updated_revision: revision,
        created_at: at,
    };
    let receipt = GraphReceipt {
        revision,
        claim_id: Some(claim.id.clone()),
        entity_ids: vec![subject_id, object_id],
        ..Default::default()
    };
    ops.push(GraphOp::Claim(claim));
    Ok((ops, receipt))
}

/// Entity-Auflösung ohne Modellaufruf: exakter Treffer auf einem normalisierten
/// Alias innerhalb der Sicht, sonst eine neue Entity im Schreibziel.
///
/// Bewusst nur exakt: eine falsche Verschmelzung ist über Multi-Hop-Abfragen
/// teuer, eine doppelte Entity dagegen billig reparierbar (Precision first).
#[allow(clippy::too_many_arguments)]
fn resolve_or_create(
    index: &GraphIndex,
    access: &GraphAccess,
    target: &GraphTarget,
    name: &str,
    entity_type: Option<&str>,
    revision: GraphRevision,
    ids: &mut IdCounters,
    at: u64,
    ops: &mut Vec<GraphOp>,
    fresh: &mut Vec<(String, EntityId)>,
) -> EntityId {
    let key = normalize(name);
    // Innerhalb derselben Mutation neu angelegte Entity (Subjekt == Objekt).
    if let Some((_, id)) = fresh.iter().find(|(k, _)| k == &key) {
        return id.clone();
    }
    // Direkte ID-Referenz (`E-7`) — praktisch für Tools, die eine ID aus einem
    // vorherigen Ergebnis zurückgeben.
    if index.sees_entity(name, &access.view) {
        return name.to_string();
    }
    if let Some(hit) = index.by_alias(name, &access.view).first() {
        return hit.id.clone();
    }
    let entity = GraphEntity {
        id: ids.next_entity(),
        canonical_name: name.to_string(),
        entity_type: entity_type.unwrap_or("thing").to_string(),
        description: None,
        aliases: vec![key.clone()],
        layer: target.layer,
        scope: target.scope.clone(),
        created_revision: revision,
        updated_revision: revision,
        created_at: at,
    };
    let id = entity.id.clone();
    fresh.push((key, id.clone()));
    ops.push(GraphOp::Entity(entity));
    id
}

fn record_episode(
    draft: EpisodeDraft,
    access: &GraphAccess,
    revision: GraphRevision,
    ids: &mut IdCounters,
) -> Result<(Vec<GraphOp>, GraphReceipt), GraphError> {
    let target = write_target(access)?;
    let summary = draft.summary.trim();
    if summary.is_empty() {
        return Err(GraphError::Invalid("summary darf nicht leer sein".into()));
    }
    let at = now_ms();
    let source = build_source(&draft.source, access, revision, ids, at);
    let source_id = source.id.clone();
    let episode = GraphEpisode {
        id: ids.next_episode(),
        actor: access.principal.clone(),
        summary: summary.to_string(),
        scope: target.scope.clone(),
        source_ids: vec![source_id],
        created_revision: revision,
        created_at: at,
    };
    let receipt = GraphReceipt {
        revision,
        episode_id: Some(episode.id.clone()),
        ..Default::default()
    };
    Ok((
        vec![GraphOp::Source(source), GraphOp::Episode(episode)],
        receipt,
    ))
}

/// Promotion: ein vorläufiger Claim wird ins Ziel verschoben und dort bestätigt.
///
/// Der Datensatz behält seine ID (die Evidenz-Verknüpfung bleibt dieselbe); die
/// Vorversion steht weiterhin im Journal — **das Journal ist der Audit-Trail**,
/// nicht eine zweite Kopie im Graphen.
fn promote_claim(
    index: &GraphIndex,
    claim_id: &str,
    access: &GraphAccess,
    revision: GraphRevision,
) -> Result<(Vec<GraphOp>, GraphReceipt), GraphError> {
    let target = access
        .promote
        .clone()
        .ok_or_else(|| GraphError::Denied("dieser Zugriff darf nicht promotieren".into()))?;
    let claim = index
        .claim(claim_id)
        .filter(|c| index.sees_claim(c, &access.view))
        .ok_or_else(|| GraphError::NotFound(format!("Claim '{claim_id}'")))?;
    if claim.source_ids.is_empty() {
        return Err(GraphError::Invalid(format!(
            "Claim '{claim_id}' hat keine Quelle und kann nicht promotet werden"
        )));
    }
    if claim.status == ClaimStatus::Superseded {
        return Err(GraphError::Invalid(format!(
            "Claim '{claim_id}' wurde bereits ersetzt"
        )));
    }
    if claim.layer == target.layer && claim.scope == target.scope {
        return Err(GraphError::Invalid(format!(
            "Claim '{claim_id}' liegt bereits in {target}"
        )));
    }

    let mut ops = Vec::new();

    // Beide Endpunkte müssen mitwandern, sonst zeigte ein kanonischer Claim auf
    // Entities, die mit dem Working-Scope verschwinden.
    for endpoint in [&claim.subject, &claim.object] {
        if let Some(entity) = index.entity(endpoint) {
            if entity.layer != target.layer || entity.scope != target.scope {
                let mut moved = (**entity).clone();
                moved.layer = target.layer;
                moved.scope = target.scope.clone();
                moved.updated_revision = revision;
                ops.push(GraphOp::Entity(moved));
            }
        }
    }

    // Widersprüche im Ziel werden nicht überschrieben, sondern als ersetzt
    // markiert — die alte Aussage bleibt als Evidenz lesbar.
    let mut superseded = Vec::new();
    for other in index.contradicting(claim, target.layer, &target.scope) {
        if other.status == ClaimStatus::Superseded {
            continue;
        }
        let mut old = (**other).clone();
        old.status = ClaimStatus::Superseded;
        old.superseded_by = Some(claim.id.clone());
        old.updated_revision = revision;
        superseded.push(old.id.clone());
        ops.push(GraphOp::Claim(old));
    }

    let mut promoted = (**claim).clone();
    promoted.promoted_from = Some(promoted.scope.clone());
    promoted.layer = target.layer;
    promoted.scope = target.scope.clone();
    promoted.status = ClaimStatus::Confirmed;
    promoted.updated_revision = revision;
    let receipt = GraphReceipt {
        revision,
        claim_id: Some(promoted.id.clone()),
        entity_ids: vec![promoted.subject.clone(), promoted.object.clone()],
        superseded,
        ..Default::default()
    };
    ops.push(GraphOp::Claim(promoted));
    Ok((ops, receipt))
}
