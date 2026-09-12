//! Entity-Dokument: die Abbildung eines [`GraphEntity`] samt seiner
//! ausgehenden Claims und deren Quellen auf OKF-Markdown und zurück.
//!
//! Dateisystem, Verzeichnisstruktur, `index.md`/`log.md` und Slugs sind NICHT
//! Sache dieses Moduls — das kommt mit `bundle.rs`. Hier geht es rein um die
//! Abbildung Datensatz ⇄ Dokumenttext für EIN Dokument.
//!
//! **Frontmatter ist die Wahrheit, der Body ist Beiwerk.** [`parse_entity`]
//! liest ausschließlich den `agentkit`-Maschinenblock (plus die wenigen
//! OKF-Standardfelder, die keine Entsprechung dort haben: `type`, `title`,
//! `description`). Der Body — Aussagen-Liste und Fußnoten — wird beim Parsen
//! vollständig ignoriert. Das macht den Body robust gegen Handänderungen:
//! ein Mensch darf dort redigieren, ohne den Wiederaufbau zu gefährden, weil
//! er ihn schlicht nicht berührt.

use std::collections::{BTreeMap, HashMap};

use crate::error::GraphError;
use crate::model::{
    id_order, Actor, ClaimStatus, EntityId, GraphClaim, GraphEntity, GraphEpisode, GraphLayer,
    GraphScope, GraphSource, Verification,
};
use crate::okf::{instant, markdown};

use super::yaml::{self, YamlValue};

/// Ein Entity-Dokument: die Entity, alle Claims, deren SUBJEKT sie ist, und
/// alle Quellen, die genau diese Claims belegen.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityDoc {
    pub entity: GraphEntity,
    pub claims: Vec<GraphClaim>,
    pub sources: Vec<GraphSource>,
}

/// Wohin ein Claim-Objekt verlinkt wird. `bundle.rs` baut diese Karte, weil
/// nur dort die Pfade aller Dokumente bekannt sind — dieses Modul kennt kein
/// Dateisystem.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityRef {
    pub title: String,
    pub path: String,
}

/// Ein Episoden-Dokument: die Episode und die Quellen, die sie belegen.
///
/// Anders als bei [`EntityDoc`] gibt es hier nur EINE Liste von Quellen, keine
/// Claims — eine Episode ist ein Ereignis, kein Aussagen-Netz. Die volle,
/// unveränderte `summary` steht NICHT im Frontmatter (dort nur eine gekürzte
/// `description`), sondern im Body unter „# Verlauf" — das Frontmatter bliebe
/// sonst bei langen Verläufen unlesbar groß. Der Body ist für dieses eine Feld
/// also die Wahrheit, nicht bloß Beiwerk; [`parse_episode`] liest `summary`
/// entsprechend aus dem Body zurück.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeDoc {
    pub episode: GraphEpisode,
    pub sources: Vec<GraphSource>,
}

/// OKF-Standardschlüssel auf Top-Level, die eine feste Bedeutung haben (§4.1,
/// §5) und deshalb NICHT in [`GraphEntity::extra`] landen dürfen — sonst
/// würde ein zweiter Schreibdurchlauf sie doppelt sehen (einmal als eigenes
/// Feld, einmal als „unbekannt").
const KNOWN_TOP_KEYS: [&str; 9] = [
    "type",
    "title",
    "description",
    "tags",
    "status",
    "generated",
    "verified",
    "sources",
    "agentkit",
];

// ---------------------------------------------------------------------------
// Rendern
// ---------------------------------------------------------------------------

pub fn render_entity(doc: &EntityDoc, refs: &BTreeMap<EntityId, EntityRef>) -> String {
    let mut claims_sorted = doc.claims.clone();
    claims_sorted.sort_by_key(|c| id_order(&c.id));
    let mut sources_sorted = doc.sources.clone();
    sources_sorted.sort_by_key(|s| id_order(&s.id));

    // `entity_type` leer ⇒ `thing`, sowohl für `type` als auch für den ersten
    // Tag: ein leerer String als Tag wäre kein Tag, sondern Datenmüll. Das
    // ist eine bewusste Einbahnstraße — eine Entity, deren `entity_type`
    // wirklich `""` war, bekommt beim Wiedereinlesen `"thing"` zurück. Die
    // Round-Trip-Tests unten benutzen deshalb bewusst keinen leeren
    // `entity_type`.
    let effective_type = if doc.entity.entity_type.is_empty() {
        "thing"
    } else {
        doc.entity.entity_type.as_str()
    };

    let (description_text, description_generated) = match &doc.entity.description {
        Some(text) => (text.clone(), false),
        None => (
            format!(
                "„{}“ im Wissensgraphen von agentkit.",
                doc.entity.canonical_name
            ),
            true,
        ),
    };

    let mut pairs: Vec<(String, YamlValue)> = vec![
        ("type".to_string(), YamlValue::str(effective_type)),
        (
            "title".to_string(),
            YamlValue::str(doc.entity.canonical_name.clone()),
        ),
        ("description".to_string(), YamlValue::str(description_text)),
        (
            "tags".to_string(),
            YamlValue::seq(build_tags(&doc.entity, effective_type)),
        ),
        (
            "status".to_string(),
            YamlValue::str(top_level_status(&doc.entity, &claims_sorted)),
        ),
        (
            "generated".to_string(),
            YamlValue::map([
                (
                    "by".to_string(),
                    YamlValue::str(
                        Actor::producer("agentkit-graph", env!("CARGO_PKG_VERSION"))
                            .as_str()
                            .to_string(),
                    ),
                ),
                (
                    "at".to_string(),
                    YamlValue::str(instant::to_rfc3339(generated_at(
                        &doc.entity,
                        &claims_sorted,
                    ))),
                ),
            ]),
        ),
    ];

    let verified_union = union_verified(&claims_sorted);
    if !verified_union.is_empty() {
        pairs.push((
            "verified".to_string(),
            YamlValue::seq(verified_union.iter().map(verification_to_yaml)),
        ));
    }

    if !sources_sorted.is_empty() {
        pairs.push((
            "sources".to_string(),
            YamlValue::seq(sources_sorted.iter().map(build_top_source_entry)),
        ));
    }

    // Reihenfolge der Karte ist bereits die Round-Trip-Reihenfolge
    // (`BTreeMap`) — hier nur unverändert durchgereicht (§4.1: unbekannte
    // Schlüssel überleben den Round-Trip).
    for (key, value) in &doc.entity.extra {
        pairs.push((key.clone(), value.clone()));
    }

    pairs.push((
        "agentkit".to_string(),
        build_agentkit_block(
            &doc.entity,
            &claims_sorted,
            &sources_sorted,
            description_generated,
        ),
    ));

    let body = render_body(&claims_sorted, &sources_sorted, refs);
    markdown::compose(&pairs, &body)
}

/// `f32` ⇒ `f64` über die kürzeste Dezimaldarstellung. Siehe
/// [`build_claim_entry`] für die Begründung.
fn shortest_f64(value: f32) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(f64::from(value))
}

fn build_tags(entity: &GraphEntity, effective_type: &str) -> Vec<YamlValue> {
    let mut seen: Vec<String> = vec![effective_type.to_string()];
    for alias in &entity.aliases {
        if !seen.contains(alias) {
            seen.push(alias.clone());
        }
    }
    seen.into_iter().map(YamlValue::str).collect()
}

/// `deprecated` schlägt `draft`/`stable`, sobald jeder Claim ersetzt ist —
/// ein Konzept, dessen sämtliche Aussagen widerlegt sind, ist nicht mehr
/// „vorläufig" oder „konsolidiert", sondern erledigt.
fn top_level_status(entity: &GraphEntity, claims: &[GraphClaim]) -> &'static str {
    let all_superseded =
        !claims.is_empty() && claims.iter().all(|c| c.status == ClaimStatus::Superseded);
    if all_superseded {
        "deprecated"
    } else if entity.layer == GraphLayer::Working {
        "draft"
    } else {
        "stable"
    }
}

/// Muss allein aus den Daten folgen (kein `now_ms()`) — sonst ändert jedes
/// Neuschreiben `generated.at` und der Round-Trip ist nicht byte-stabil.
fn generated_at(entity: &GraphEntity, claims: &[GraphClaim]) -> u64 {
    claims
        .iter()
        .map(|c| c.created_at)
        .fold(entity.created_at, u64::max)
}

fn union_verified(claims: &[GraphClaim]) -> Vec<Verification> {
    let mut all: Vec<Verification> = claims.iter().flat_map(|c| c.verified.clone()).collect();
    all.sort_by(|a, b| {
        a.at.cmp(&b.at)
            .then_with(|| a.by.as_str().cmp(b.by.as_str()))
    });
    all.dedup();
    all
}

fn verification_to_yaml(v: &Verification) -> YamlValue {
    YamlValue::map([
        ("by".to_string(), YamlValue::str(v.by.as_str().to_string())),
        ("at".to_string(), YamlValue::str(instant::to_rfc3339(v.at))),
    ])
}

fn scope_to_yaml(scope: &GraphScope) -> YamlValue {
    YamlValue::map([
        ("kind".to_string(), YamlValue::str(scope.kind.clone())),
        ("id".to_string(), YamlValue::str(scope.id.clone())),
    ])
}

/// Ersetzt Whitespace in einem Bestandteil der synthetisierten `resource`
/// durch `-` — die Spec verlangt whitespace-freie Werte (§5.1), ein
/// `run_id`/`tool_call_id` aus Fremdsystemen ist aber beliebig.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect()
}

/// `resource` MUSS nicht-leer sein (§5.1) — ohne `artifact_uri` wird sie aus
/// Lauf/Tool-Aufruf synthetisiert, damit kein Konsument auf ein leeres Feld
/// trifft.
fn synthesize_resource(source: &GraphSource) -> String {
    let run = source.run_id.as_deref().unwrap_or("-");
    let tail = source.tool_call_id.as_deref().unwrap_or(&source.id);
    format!(
        "agentkit://{}/{}/{}",
        sanitize_component(&source.source_type),
        sanitize_component(run),
        sanitize_component(tail),
    )
}

/// Top-Level-`sources[]`-Eintrag: nur die OKF-Felder (§5.1), damit die Zeile
/// als Flow-Mapping in eine Zeile passt und ein fremder Konsument genau die
/// Spec-Felder sieht. Der agentkit-eigene Rest steht im `agentkit`-Block.
fn build_top_source_entry(source: &GraphSource) -> YamlValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_string(), YamlValue::str(source.id.clone()));
    let resource = source
        .artifact_uri
        .clone()
        .unwrap_or_else(|| synthesize_resource(source));
    map.insert("resource".to_string(), YamlValue::str(resource));
    map.insert(
        "title".to_string(),
        YamlValue::str(source.source_type.clone()),
    );
    if let Some(actor) = &source.agent_id {
        map.insert(
            "author".to_string(),
            YamlValue::str(actor.as_str().to_string()),
        );
    }
    YamlValue::Map(map)
}

/// `agentkit.sources[]`-Eintrag: der agentkit-eigene Rest, OHNE `agent_id` —
/// der steht (als `author`) bereits im Top-Level-`sources[]`-Eintrag; ihn
/// hier zu wiederholen wäre reine Redundanz.
fn build_agentkit_source_entry(source: &GraphSource) -> YamlValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_string(), YamlValue::str(source.id.clone()));
    map.insert(
        "source_type".to_string(),
        YamlValue::str(source.source_type.clone()),
    );
    if let Some(run_id) = &source.run_id {
        map.insert("run_id".to_string(), YamlValue::str(run_id.clone()));
    }
    if let Some(tool_call_id) = &source.tool_call_id {
        map.insert(
            "tool_call_id".to_string(),
            YamlValue::str(tool_call_id.clone()),
        );
    }
    if let Some(artifact_uri) = &source.artifact_uri {
        map.insert(
            "artifact_uri".to_string(),
            YamlValue::str(artifact_uri.clone()),
        );
    }
    if let Some(excerpt) = &source.excerpt {
        map.insert("excerpt".to_string(), YamlValue::str(excerpt.clone()));
    }
    map.insert(
        "content_hash".to_string(),
        YamlValue::str(source.content_hash.clone()),
    );
    map.insert(
        "created_revision".to_string(),
        YamlValue::Int(source.created_revision as i64),
    );
    map.insert(
        "created_at".to_string(),
        YamlValue::str(instant::to_rfc3339(source.created_at)),
    );
    YamlValue::Map(map)
}

/// `agentkit.claims[]`-Eintrag. Kein `subject` — das ist implizit die Entity
/// dieses Dokuments (§Kontext). Kein `layer`/`scope` — ein Claim liegt immer
/// im selben Layer/Scope wie sein Subjekt: `write::promote_claim` verschiebt
/// beide Endpunkte IMMER gemeinsam mit dem Claim, ein abweichender
/// Claim-Scope kann also gar nicht entstehen. Das hier redundant zu
/// speichern wäre nur zusätzliche Fläche für Inkonsistenzen.
/// Ein Claim-Eintrag im `agentkit`-Block.
///
/// `layer`/`scope` werden nur geschrieben, wenn sie von denen des Subjekts
/// ABWEICHEN — und das können sie: `write.rs::resolve_or_create` löst einen
/// Subjektnamen über die ganze Sicht auf und trifft dabei auch eine bereits
/// kanonische Entity. Eine neue Beobachtung über sie ist dann `working`,
/// während ihr Subjekt `canonical` ist (derselbe Fall, den der Kommentar in
/// `agentkit_viz/src/assets/app.js` beim Filtern beschreibt).
///
/// Würde man das Ziel beim Laden stattdessen vom Subjekt ableiten, käme eine
/// vorläufige Beobachtung als kanonisch zurück — ein Modell hätte sich ohne
/// `graph_promote` selbst kanonisiert, genau die Invariante, die
/// `write.rs::record_claim` mit seiner Statusprüfung schützt.
fn build_claim_entry(claim: &GraphClaim, layer: GraphLayer, scope: &GraphScope) -> YamlValue {
    let mut map = BTreeMap::new();
    if claim.layer != layer {
        map.insert("layer".to_string(), YamlValue::str(claim.layer.wire_name()));
    }
    if &claim.scope != scope {
        map.insert("scope".to_string(), scope_to_yaml(&claim.scope));
    }
    map.insert("id".to_string(), YamlValue::str(claim.id.clone()));
    map.insert(
        "predicate".to_string(),
        YamlValue::str(claim.predicate.clone()),
    );
    map.insert("object".to_string(), YamlValue::str(claim.object.clone()));
    map.insert(
        "status".to_string(),
        YamlValue::str(claim.status.wire_name()),
    );
    map.insert(
        "confidence".to_string(),
        // Über die KÜRZESTE Dezimaldarstellung des `f32`, nicht über
        // `f64::from`: die reine Verbreiterung schreibt aus 0.82f32 ein
        // `0.8199999928474426` in die Datei — ein Wert, der dem `0.82` im Body
        // widerspricht und jeden Diff mit Rauschen füllt. `to_string` liefert
        // für einen `f32` die kürzeste Darstellung, die wieder auf denselben
        // `f32` parst; der Round-Trip bleibt damit exakt.
        YamlValue::Float(shortest_f64(claim.confidence)),
    );
    map.insert(
        "sources".to_string(),
        YamlValue::seq(claim.source_ids.iter().cloned().map(YamlValue::str)),
    );
    map.insert(
        "created_by".to_string(),
        YamlValue::str(claim.created_by.as_str().to_string()),
    );
    map.insert(
        "created_revision".to_string(),
        YamlValue::Int(claim.created_revision as i64),
    );
    map.insert(
        "updated_revision".to_string(),
        YamlValue::Int(claim.updated_revision as i64),
    );
    map.insert(
        "created_at".to_string(),
        YamlValue::str(instant::to_rfc3339(claim.created_at)),
    );
    if let Some(superseded_by) = &claim.superseded_by {
        map.insert(
            "superseded_by".to_string(),
            YamlValue::str(superseded_by.clone()),
        );
    }
    if let Some(promoted_from) = &claim.promoted_from {
        map.insert("promoted_from".to_string(), scope_to_yaml(promoted_from));
    }
    if let Some(promoted_from_status) = &claim.promoted_from_status {
        map.insert(
            "promoted_from_status".to_string(),
            YamlValue::str(promoted_from_status.wire_name()),
        );
    }
    if !claim.verified.is_empty() {
        map.insert(
            "verified".to_string(),
            YamlValue::seq(claim.verified.iter().map(verification_to_yaml)),
        );
    }
    YamlValue::Map(map)
}

fn build_agentkit_block(
    entity: &GraphEntity,
    claims_sorted: &[GraphClaim],
    sources_sorted: &[GraphSource],
    description_generated: bool,
) -> YamlValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_string(), YamlValue::str(entity.id.clone()));
    map.insert(
        "layer".to_string(),
        YamlValue::str(entity.layer.wire_name()),
    );
    map.insert("scope".to_string(), scope_to_yaml(&entity.scope));
    if let Some(promoted_from) = &entity.promoted_from {
        map.insert("promoted_from".to_string(), scope_to_yaml(promoted_from));
    }
    map.insert(
        "aliases".to_string(),
        YamlValue::seq(entity.aliases.iter().cloned().map(YamlValue::str)),
    );
    map.insert(
        "created_revision".to_string(),
        YamlValue::Int(entity.created_revision as i64),
    );
    map.insert(
        "updated_revision".to_string(),
        YamlValue::Int(entity.updated_revision as i64),
    );
    map.insert(
        "created_at".to_string(),
        YamlValue::str(instant::to_rfc3339(entity.created_at)),
    );
    if description_generated {
        // Nur gesetzt, wenn `description` synthetisch ist — der Index soll
        // nach dem Round-Trip nicht behaupten, jemand hätte sie geschrieben.
        map.insert("description_generated".to_string(), YamlValue::Bool(true));
    }
    map.insert(
        "claims".to_string(),
        YamlValue::seq(
            claims_sorted
                .iter()
                .map(|c| build_claim_entry(c, entity.layer, &entity.scope)),
        ),
    );
    map.insert(
        "sources".to_string(),
        YamlValue::seq(sources_sorted.iter().map(build_agentkit_source_entry)),
    );
    YamlValue::Map(map)
}

// ---------------------------------------------------------------------------
// Episoden
// ---------------------------------------------------------------------------

pub fn render_episode(doc: &EpisodeDoc) -> String {
    let mut sources_sorted = doc.sources.clone();
    sources_sorted.sort_by_key(|s| id_order(&s.id));

    let mut pairs: Vec<(String, YamlValue)> = vec![
        ("type".to_string(), YamlValue::str("Episode")),
        (
            "title".to_string(),
            YamlValue::str(episode_title(&doc.episode.summary)),
        ),
        (
            "description".to_string(),
            YamlValue::str(single_line_truncate_to(&doc.episode.summary, 200)),
        ),
        (
            "tags".to_string(),
            YamlValue::seq([
                YamlValue::str("episode"),
                YamlValue::str(doc.episode.scope.kind.clone()),
            ]),
        ),
        ("status".to_string(), YamlValue::str("stable")),
        (
            "generated".to_string(),
            YamlValue::map([
                (
                    "by".to_string(),
                    YamlValue::str(doc.episode.actor.as_str().to_string()),
                ),
                (
                    "at".to_string(),
                    YamlValue::str(instant::to_rfc3339(doc.episode.created_at)),
                ),
            ]),
        ),
    ];

    if !sources_sorted.is_empty() {
        pairs.push((
            "sources".to_string(),
            YamlValue::seq(sources_sorted.iter().map(build_top_source_entry)),
        ));
    }

    pairs.push((
        "agentkit".to_string(),
        build_episode_agentkit_block(&doc.episode, &sources_sorted),
    ));

    let body = render_episode_body(&doc.episode, &sources_sorted);
    markdown::compose(&pairs, &body)
}

/// „Erste Zeile der summary, auf 80 Zeichen gekürzt, einzeilig" (§Teil A) —
/// anders als [`single_line_truncate_to`] auf die ganze `summary` NUR die
/// erste Zeile, damit ein mehrzeiliger Verlauf nicht in den Titel durchschlägt.
pub(crate) fn episode_title(summary: &str) -> String {
    let first_line = summary.lines().next().unwrap_or("");
    single_line_truncate_to(first_line, 80)
}

fn build_episode_agentkit_block(
    episode: &GraphEpisode,
    sources_sorted: &[GraphSource],
) -> YamlValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_string(), YamlValue::str(episode.id.clone()));
    map.insert(
        "actor".to_string(),
        YamlValue::str(episode.actor.as_str().to_string()),
    );
    map.insert("scope".to_string(), scope_to_yaml(&episode.scope));
    map.insert(
        "created_revision".to_string(),
        YamlValue::Int(episode.created_revision as i64),
    );
    map.insert(
        "created_at".to_string(),
        YamlValue::str(instant::to_rfc3339(episode.created_at)),
    );
    map.insert(
        "sources".to_string(),
        YamlValue::seq(sources_sorted.iter().map(build_agentkit_source_entry)),
    );
    YamlValue::Map(map)
}

/// `# Verlauf` + die vollständige, unveränderte `summary`, danach `# Quellen`
/// mit den Footnote-Definitionen — nur wenn es Quellen gibt. Reiht sich damit
/// exakt in [`render_body`] (Entity-Pendant) ein.
fn render_episode_body(episode: &GraphEpisode, sources_sorted: &[GraphSource]) -> String {
    let mut body = format!("# Verlauf\n\n{}", episode.summary);
    if !sources_sorted.is_empty() {
        body.push_str("\n\n# Quellen\n\n");
        let lines: Vec<String> = sources_sorted.iter().map(render_source_def_line).collect();
        body.push_str(&lines.join("\n"));
    }
    body
}

/// Liest die `summary` aus dem Body — die einzige Stelle in diesem Modul, an
/// der der Body Wahrheit statt Beiwerk ist (siehe [`EpisodeDoc`]-Doc-Comment).
/// `body_with_leading_blank` ist der Rohwert von `markdown::split_frontmatter`
/// (mit der führenden Leerzeile, die `compose` selbst einfügt).
fn extract_episode_summary(body_with_leading_blank: &str) -> Result<String, GraphError> {
    let rest = body_with_leading_blank
        .strip_prefix('\n')
        .unwrap_or(body_with_leading_blank);
    let rest = rest.strip_prefix("# Verlauf\n\n").ok_or_else(|| {
        GraphError::Okf("Body: '# Verlauf'-Abschnitt fehlt oder ist nicht der erste".to_string())
    })?;
    // Endet der Body ohne Quellen-Abschnitt, hat `compose` genau EINEN
    // Zeilenumbruch angehängt (siehe `markdown::compose`) — den ziehen wir
    // hier ab, damit ein zweiter Render-Durchlauf byte-identisch bleibt.
    match rest.find("\n\n# Quellen\n\n") {
        Some(idx) => Ok(rest[..idx].to_string()),
        None => Ok(rest.trim_end_matches('\n').to_string()),
    }
}

pub fn parse_episode(text: &str) -> Result<EpisodeDoc, GraphError> {
    let (fm_text, body) = markdown::split_frontmatter(text)
        .ok_or_else(|| GraphError::Okf("kein YAML-Frontmatter gefunden".to_string()))?;
    let pairs = yaml::parse_document(fm_text)?;
    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);

    let type_value = get("type").and_then(YamlValue::as_str).ok_or_else(|| {
        GraphError::Okf("Frontmatter: 'type' fehlt oder ist kein String".to_string())
    })?;
    if type_value != "Episode" {
        return Err(GraphError::Okf(format!(
            "Frontmatter: 'type' muss 'Episode' sein, war '{type_value}'"
        )));
    }

    let agentkit = get("agentkit").and_then(YamlValue::as_map).ok_or_else(|| {
        GraphError::Okf("Frontmatter: 'agentkit'-Block fehlt oder ist kein Mapping".to_string())
    })?;

    let episode_id = map_str(agentkit, "id", "agentkit.id")?;
    let actor_text = map_str(agentkit, "actor", "agentkit.actor")?;
    let actor = Actor::parse(&actor_text).ok_or_else(|| {
        GraphError::Okf(format!("agentkit.actor: ungültiger Actor '{actor_text}'"))
    })?;
    let scope_map = agentkit
        .get("scope")
        .and_then(YamlValue::as_map)
        .ok_or_else(|| {
            GraphError::Okf("agentkit.scope: fehlt oder ist kein Mapping".to_string())
        })?;
    let scope = parse_scope_map(scope_map, "agentkit.scope")?;
    let created_revision = map_u64(agentkit, "created_revision", "agentkit.created_revision")?;
    let created_at = map_timestamp(agentkit, "created_at", "agentkit.created_at")?;

    let summary = extract_episode_summary(body)?;

    // `agent_id` je Quelle steht NUR im Top-Level-`sources[].author` — siehe
    // dieselbe Begründung bei `parse_entity`.
    let authors = parse_source_authors(get("sources"))?;
    let sources_seq = agentkit
        .get("sources")
        .and_then(YamlValue::as_seq)
        .unwrap_or(&[]);
    let mut sources = Vec::with_capacity(sources_seq.len());
    for entry in sources_seq {
        sources.push(parse_source_entry(entry, &authors)?);
    }
    // Die Reihenfolge ist dieselbe wie beim Schreiben (nach `id_order`
    // sortiert) — siehe `render_episode`. Bei genau einer Quelle je Episode
    // (der heutige einzige Erzeuger, `write::record_episode`) ist das ohnehin
    // nie beobachtbar.
    let source_ids = sources.iter().map(|s| s.id.clone()).collect();

    let episode = GraphEpisode {
        id: episode_id,
        actor,
        summary,
        scope,
        source_ids,
        created_revision,
        created_at,
    };

    Ok(EpisodeDoc { episode, sources })
}

// ---------------------------------------------------------------------------
// Body — nur menschliche/Graph-Sicht, keine Rekonstruktionsquelle
// ---------------------------------------------------------------------------

fn render_body(
    claims_sorted: &[GraphClaim],
    sources_sorted: &[GraphSource],
    refs: &BTreeMap<EntityId, EntityRef>,
) -> String {
    let mut body = String::from("# Aussagen\n\n");
    if claims_sorted.is_empty() {
        body.push_str("(keine Aussagen)");
    } else {
        let lines: Vec<String> = claims_sorted
            .iter()
            .map(|c| render_claim_line(c, refs))
            .collect();
        body.push_str(&lines.join("\n"));
    }
    if !sources_sorted.is_empty() {
        body.push_str("\n\n# Quellen\n\n");
        let lines: Vec<String> = sources_sorted.iter().map(render_source_def_line).collect();
        body.push_str(&lines.join("\n"));
    }
    body
}

fn render_claim_line(claim: &GraphClaim, refs: &BTreeMap<EntityId, EntityRef>) -> String {
    // Zeigt NIE auf eine Datei, die es nicht gibt (§6.1 Cross-Link-Check):
    // eine unbekannte Objekt-ID wird als Code-Text geschrieben, nicht verlinkt.
    let object_repr = match refs.get(&claim.object) {
        Some(r) => format!("[{}]({})", r.title, r.path),
        None => format!("`{}`", claim.object),
    };
    let mut source_ids = claim.source_ids.clone();
    source_ids.sort_by_key(|s| id_order(s));
    let footnotes: String = source_ids.iter().map(|s| format!("[^{s}]")).collect();
    format!(
        "* **{}** → {} — {} · {:.2}{}",
        claim.predicate,
        object_repr,
        claim.status.wire_name(),
        claim.confidence,
        footnotes
    )
}

fn render_source_def_line(source: &GraphSource) -> String {
    match &source.excerpt {
        Some(text) => format!(
            "[^{}]: {} — {}",
            source.id,
            source.source_type,
            single_line_truncate(text)
        ),
        None => format!("[^{}]: {}", source.id, source.source_type),
    }
}

/// Whitespace zu einem Leerzeichen, dann auf `max_chars` mit `…` gekürzt.
/// Zeichenbasiert (nicht byteweise) — sonst zerschnitte eine Kürzung mitten
/// in einem Umlaut den String an einer ungültigen UTF-8-Grenze. Von
/// `render_source_def_line` (200 Zeichen) und den Episoden-Feldern `title`
/// (80) / `description` (200) gemeinsam genutzt.
pub(crate) fn single_line_truncate_to(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count > max_chars {
        let truncated: String = collapsed.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        collapsed
    }
}

fn single_line_truncate(text: &str) -> String {
    single_line_truncate_to(text, 200)
}

// ---------------------------------------------------------------------------
// Parsen — ausschließlich aus dem Frontmatter
// ---------------------------------------------------------------------------

pub fn parse_entity(text: &str) -> Result<EntityDoc, GraphError> {
    let (fm_text, _body) = markdown::split_frontmatter(text)
        .ok_or_else(|| GraphError::Okf("kein YAML-Frontmatter gefunden".to_string()))?;
    let pairs = yaml::parse_document(fm_text)?;
    let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);

    let type_value = get("type")
        .and_then(YamlValue::as_str)
        .ok_or_else(|| {
            GraphError::Okf("Frontmatter: 'type' fehlt oder ist kein String".to_string())
        })?
        .to_string();
    let title = get("title")
        .and_then(YamlValue::as_str)
        .ok_or_else(|| {
            GraphError::Okf("Frontmatter: 'title' fehlt oder ist kein String".to_string())
        })?
        .to_string();
    let agentkit = get("agentkit").and_then(YamlValue::as_map).ok_or_else(|| {
        GraphError::Okf("Frontmatter: 'agentkit'-Block fehlt oder ist kein Mapping".to_string())
    })?;

    let entity_id = map_str(agentkit, "id", "agentkit.id")?;
    let layer = parse_layer(&map_str(agentkit, "layer", "agentkit.layer")?)?;
    let scope_map = agentkit
        .get("scope")
        .and_then(YamlValue::as_map)
        .ok_or_else(|| {
            GraphError::Okf("agentkit.scope: fehlt oder ist kein Mapping".to_string())
        })?;
    let scope = parse_scope_map(scope_map, "agentkit.scope")?;
    let promoted_from = match agentkit.get("promoted_from") {
        Some(v) => Some(parse_scope_value(v, "agentkit.promoted_from")?),
        None => None,
    };
    let aliases = agentkit
        .get("aliases")
        .and_then(YamlValue::as_seq)
        .map(|seq| {
            seq.iter()
                .filter_map(YamlValue::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let created_revision = map_u64(agentkit, "created_revision", "agentkit.created_revision")?;
    let updated_revision = map_u64(agentkit, "updated_revision", "agentkit.updated_revision")?;
    let created_at = map_timestamp(agentkit, "created_at", "agentkit.created_at")?;
    let description_generated = agentkit
        .get("description_generated")
        .and_then(YamlValue::as_bool)
        .unwrap_or(false);

    // Die generierte Beschreibung ist Platzhaltertext, keine echte Aussage —
    // sie darf beim Wiedereinlesen nicht als „von Hand geschrieben" gelten.
    let description = if description_generated {
        None
    } else {
        get("description")
            .and_then(YamlValue::as_str)
            .map(str::to_string)
    };

    let mut extra = BTreeMap::new();
    for (key, value) in &pairs {
        if !KNOWN_TOP_KEYS.contains(&key.as_str()) {
            extra.insert(key.clone(), value.clone());
        }
    }

    let entity = GraphEntity {
        id: entity_id,
        canonical_name: title,
        entity_type: type_value,
        description,
        aliases,
        layer,
        scope: scope.clone(),
        promoted_from,
        created_revision,
        updated_revision,
        created_at,
        extra,
    };

    // `agent_id` je Quelle steht NUR im Top-Level-`sources[].author` — der
    // agentkit-Block spart ihn sich (siehe `build_agentkit_source_entry`).
    let authors = parse_source_authors(get("sources"))?;
    let sources_seq = agentkit
        .get("sources")
        .and_then(YamlValue::as_seq)
        .unwrap_or(&[]);
    let mut sources = Vec::with_capacity(sources_seq.len());
    for entry in sources_seq {
        sources.push(parse_source_entry(entry, &authors)?);
    }

    let claims_seq = agentkit
        .get("claims")
        .and_then(YamlValue::as_seq)
        .unwrap_or(&[]);
    let mut claims = Vec::with_capacity(claims_seq.len());
    for entry in claims_seq {
        claims.push(parse_claim_entry(entry, &entity.id, layer, &scope)?);
    }

    Ok(EntityDoc {
        entity,
        claims,
        sources,
    })
}

fn map_str(map: &BTreeMap<String, YamlValue>, key: &str, path: &str) -> Result<String, GraphError> {
    map.get(key)
        .and_then(YamlValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| GraphError::Okf(format!("{path}: fehlt oder ist kein String")))
}

fn map_u64(map: &BTreeMap<String, YamlValue>, key: &str, path: &str) -> Result<u64, GraphError> {
    map.get(key)
        .and_then(YamlValue::as_u64)
        .ok_or_else(|| GraphError::Okf(format!("{path}: fehlt oder ist keine ganze Zahl")))
}

fn map_timestamp(
    map: &BTreeMap<String, YamlValue>,
    key: &str,
    path: &str,
) -> Result<u64, GraphError> {
    let raw = map_str(map, key, path)?;
    instant::from_rfc3339(&raw)
        .ok_or_else(|| GraphError::Okf(format!("{path}: ungültiger Zeitstempel '{raw}'")))
}

fn parse_layer(text: &str) -> Result<GraphLayer, GraphError> {
    match text {
        "working" => Ok(GraphLayer::Working),
        "canonical" => Ok(GraphLayer::Canonical),
        other => Err(GraphError::Okf(format!(
            "agentkit.layer: unbekannter Wert '{other}'"
        ))),
    }
}

fn parse_scope_map(
    map: &BTreeMap<String, YamlValue>,
    path: &str,
) -> Result<GraphScope, GraphError> {
    let kind = map_str(map, "kind", &format!("{path}.kind"))?;
    let id = map_str(map, "id", &format!("{path}.id"))?;
    Ok(GraphScope::new(&kind, &id))
}

fn parse_scope_value(value: &YamlValue, path: &str) -> Result<GraphScope, GraphError> {
    let map = value
        .as_map()
        .ok_or_else(|| GraphError::Okf(format!("{path}: ist kein Mapping")))?;
    parse_scope_map(map, path)
}

fn parse_verification(value: &YamlValue, path: &str) -> Result<Verification, GraphError> {
    let map = value
        .as_map()
        .ok_or_else(|| GraphError::Okf(format!("{path}: ist kein Mapping")))?;
    let by_text = map_str(map, "by", &format!("{path}.by"))?;
    let by = Actor::parse(&by_text)
        .ok_or_else(|| GraphError::Okf(format!("{path}.by: ungültiger Actor '{by_text}'")))?;
    let at = map_timestamp(map, "at", &format!("{path}.at"))?;
    Ok(Verification { by, at })
}

fn parse_source_authors(value: Option<&YamlValue>) -> Result<HashMap<String, Actor>, GraphError> {
    let mut out = HashMap::new();
    let Some(seq) = value.and_then(YamlValue::as_seq) else {
        return Ok(out);
    };
    for entry in seq {
        let map = entry
            .as_map()
            .ok_or_else(|| GraphError::Okf("sources[]: Eintrag ist kein Mapping".to_string()))?;
        let id = map_str(map, "id", "sources[].id")?;
        if let Some(author_text) = map.get("author").and_then(YamlValue::as_str) {
            let actor = Actor::parse(author_text).ok_or_else(|| {
                GraphError::Okf(format!(
                    "sources[{id}].author: ungültiger Actor '{author_text}'"
                ))
            })?;
            out.insert(id, actor);
        }
    }
    Ok(out)
}

fn parse_source_entry(
    value: &YamlValue,
    authors: &HashMap<String, Actor>,
) -> Result<GraphSource, GraphError> {
    let map = value.as_map().ok_or_else(|| {
        GraphError::Okf("agentkit.sources[]: Eintrag ist kein Mapping".to_string())
    })?;
    let id = map_str(map, "id", "agentkit.sources[].id")?;
    let path = format!("agentkit.sources[{id}]");
    let source_type = map_str(map, "source_type", &format!("{path}.source_type"))?;
    let run_id = map
        .get("run_id")
        .and_then(YamlValue::as_str)
        .map(str::to_string);
    let tool_call_id = map
        .get("tool_call_id")
        .and_then(YamlValue::as_str)
        .map(str::to_string);
    let artifact_uri = map
        .get("artifact_uri")
        .and_then(YamlValue::as_str)
        .map(str::to_string);
    let excerpt = map
        .get("excerpt")
        .and_then(YamlValue::as_str)
        .map(str::to_string);
    let content_hash = map_str(map, "content_hash", &format!("{path}.content_hash"))?;
    let created_revision = map_u64(map, "created_revision", &format!("{path}.created_revision"))?;
    let created_at = map_timestamp(map, "created_at", &format!("{path}.created_at"))?;
    let agent_id = authors.get(&id).cloned();
    Ok(GraphSource {
        id,
        source_type,
        agent_id,
        run_id,
        tool_call_id,
        artifact_uri,
        excerpt,
        content_hash,
        created_revision,
        created_at,
    })
}

fn parse_claim_entry(
    value: &YamlValue,
    subject: &str,
    layer: GraphLayer,
    scope: &GraphScope,
) -> Result<GraphClaim, GraphError> {
    let map = value.as_map().ok_or_else(|| {
        GraphError::Okf("agentkit.claims[]: Eintrag ist kein Mapping".to_string())
    })?;
    let id = map_str(map, "id", "agentkit.claims[].id")?;
    let path = format!("agentkit.claims[{id}]");
    let predicate = map_str(map, "predicate", &format!("{path}.predicate"))?;
    let object = map_str(map, "object", &format!("{path}.object"))?;
    let status_text = map_str(map, "status", &format!("{path}.status"))?;
    let status = ClaimStatus::parse(&status_text).ok_or_else(|| {
        GraphError::Okf(format!("{path}.status: unbekannter Wert '{status_text}'"))
    })?;
    let confidence = map
        .get("confidence")
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
        .ok_or_else(|| GraphError::Okf(format!("{path}.confidence: fehlt oder ist keine Zahl")))?
        as f32;
    let source_ids = map
        .get("sources")
        .and_then(YamlValue::as_seq)
        .map(|seq| {
            seq.iter()
                .filter_map(YamlValue::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let created_by_text = map_str(map, "created_by", &format!("{path}.created_by"))?;
    let created_by = Actor::parse(&created_by_text).ok_or_else(|| {
        GraphError::Okf(format!(
            "{path}.created_by: ungültiger Actor '{created_by_text}'"
        ))
    })?;
    let created_revision = map_u64(map, "created_revision", &format!("{path}.created_revision"))?;
    let updated_revision = map_u64(map, "updated_revision", &format!("{path}.updated_revision"))?;
    let created_at = map_timestamp(map, "created_at", &format!("{path}.created_at"))?;
    let superseded_by = map
        .get("superseded_by")
        .and_then(YamlValue::as_str)
        .map(str::to_string);
    let promoted_from = match map.get("promoted_from") {
        Some(v) => Some(parse_scope_value(v, &format!("{path}.promoted_from"))?),
        None => None,
    };
    let promoted_from_status = match map.get("promoted_from_status") {
        Some(v) => {
            let text = v.as_str().ok_or_else(|| {
                GraphError::Okf(format!("{path}.promoted_from_status: ist kein String"))
            })?;
            Some(ClaimStatus::parse(text).ok_or_else(|| {
                GraphError::Okf(format!(
                    "{path}.promoted_from_status: unbekannter Wert '{text}'"
                ))
            })?)
        }
        None => None,
    };
    let verified = match map.get("verified").and_then(YamlValue::as_seq) {
        Some(seq) => seq
            .iter()
            .map(|v| parse_verification(v, &format!("{path}.verified[]")))
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };

    // Fehlen `layer`/`scope`, gilt das Ziel des Subjekts — der Normalfall, der
    // damit keine zwei Zeilen je Aussage kostet. Siehe `build_claim_entry` für
    // den Fall, in dem sie WIRKLICH auseinanderfallen.
    let layer = match map.get("layer").and_then(YamlValue::as_str) {
        None => layer,
        Some("working") => GraphLayer::Working,
        Some("canonical") => GraphLayer::Canonical,
        Some(other) => {
            return Err(GraphError::Okf(format!(
                "{path}.layer: unbekannter Wert '{other}'"
            )))
        }
    };
    let scope = match map.get("scope") {
        Some(value) => parse_scope_value(value, &format!("{path}.scope"))?,
        None => scope.clone(),
    };

    Ok(GraphClaim {
        id,
        subject: subject.to_string(),
        predicate,
        object,
        layer,
        scope,
        status,
        confidence,
        source_ids,
        created_by,
        superseded_by,
        promoted_from,
        promoted_from_status,
        verified,
        created_revision,
        updated_revision,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GraphScope;

    fn ts(text: &str) -> u64 {
        instant::from_rfc3339(text).expect("Testzeitstempel muss parsen")
    }

    /// Ein Dokument mit allem: mehrere Claims (einer superseded, einer
    /// promotet mit Verifikation), mehrere Quellen (mit und ohne
    /// `artifact_uri`/`excerpt`), Umlaute, `extra`-Keys und generierter
    /// Beschreibung.
    fn voll_bestuecktes_doc() -> EntityDoc {
        let scope = GraphScope::workspace("ws1");
        let entity = GraphEntity {
            id: "E-1".to_string(),
            canonical_name: "Parallele Tool-Aufrufe".to_string(),
            entity_type: "Konzept".to_string(),
            description: None,
            aliases: vec![
                "parallele tool aufrufe".to_string(),
                "Parallel Tool Calls".to_string(),
            ],
            layer: GraphLayer::Canonical,
            scope: scope.clone(),
            promoted_from: Some(GraphScope::session("s1")),
            created_revision: 3,
            updated_revision: 5,
            created_at: ts("2026-01-01T00:00:00Z"),
            extra: BTreeMap::from([
                ("domain".to_string(), YamlValue::str("infra")),
                ("owner".to_string(), YamlValue::str("team-x")),
            ]),
        };
        let claim_ok = GraphClaim {
            id: "C-1".to_string(),
            subject: "E-1".to_string(),
            predicate: "verursacht".to_string(),
            object: "E-2".to_string(),
            layer: GraphLayer::Canonical,
            scope: scope.clone(),
            status: ClaimStatus::Confirmed,
            confidence: 0.82,
            source_ids: vec!["S-1".to_string(), "S-2".to_string()],
            created_by: Actor::human("dana"),
            superseded_by: None,
            promoted_from: Some(GraphScope::session("s1")),
            promoted_from_status: Some(ClaimStatus::Observation),
            verified: vec![Verification {
                by: Actor::human("dana"),
                at: ts("2026-02-01T10:00:00Z"),
            }],
            created_revision: 2,
            updated_revision: 5,
            created_at: ts("2026-01-15T08:00:00Z"),
        };
        let claim_superseded = GraphClaim {
            id: "C-2".to_string(),
            subject: "E-1".to_string(),
            predicate: "bezieht sich auf".to_string(),
            object: "E-9".to_string(), // taucht nicht in refs auf
            layer: GraphLayer::Canonical,
            scope: scope.clone(),
            status: ClaimStatus::Superseded,
            confidence: 0.4,
            source_ids: vec!["S-1".to_string()],
            created_by: Actor::agent("tester"),
            superseded_by: Some("C-1".to_string()),
            promoted_from: None,
            promoted_from_status: None,
            verified: Vec::new(),
            created_revision: 1,
            updated_revision: 2,
            created_at: ts("2026-01-10T08:00:00Z"),
        };
        let source_mit_excerpt = GraphSource {
            id: "S-1".to_string(),
            source_type: "tool_result".to_string(),
            agent_id: Some(Actor::human("dana")),
            run_id: Some("run-4711".to_string()),
            tool_call_id: Some("call-1".to_string()),
            artifact_uri: None, // ⇒ resource wird synthetisiert
            excerpt: Some("cargo test mcp:: — 2 Fehlschläge über Größe hinüber".to_string()),
            content_hash: "abc123".to_string(),
            created_revision: 2,
            created_at: ts("2026-01-15T07:59:00Z"),
        };
        let source_ohne_extras = GraphSource {
            id: "S-2".to_string(),
            source_type: "document".to_string(),
            agent_id: None,
            run_id: None,
            tool_call_id: None,
            artifact_uri: Some("https://example.com/doc".to_string()),
            excerpt: None,
            content_hash: "def456".to_string(),
            created_revision: 1,
            created_at: ts("2026-01-14T00:00:00Z"),
        };
        EntityDoc {
            entity,
            claims: vec![claim_ok, claim_superseded],
            sources: vec![source_mit_excerpt, source_ohne_extras],
        }
    }

    fn refs() -> BTreeMap<EntityId, EntityRef> {
        BTreeMap::from([(
            "E-2".to_string(),
            EntityRef {
                title: "Session-Konkurrenz".to_string(),
                path: "/working/session-run-4711/session-konkurrenz.md".to_string(),
            },
        )])
    }

    #[test]
    fn round_trip_erhaelt_alle_daten() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let parsed = parse_entity(&text).expect("muss parsen");
        assert_eq!(parsed, doc, "Round-Trip fehlgeschlagen:\n{text}");
    }

    /// Ein vorläufiger Claim über eine bereits kanonische Entity. Das ist kein
    /// konstruierter Fall: `write.rs::resolve_or_create` löst den Subjektnamen
    /// über die ganze Sicht auf und trifft dabei die promotete Entity, während
    /// die neue Aussage im Working-Ziel landet.
    ///
    /// Würde das Ziel beim Laden vom Subjekt abgeleitet, käme die Beobachtung
    /// als `canonical` zurück — eine Selbst-Kanonisierung ohne `graph_promote`.
    #[test]
    fn ein_claim_in_einem_anderen_ziel_als_sein_subjekt_ueberlebt() {
        let mut doc = voll_bestuecktes_doc();
        doc.claims.truncate(1);
        doc.claims[0].layer = GraphLayer::Working;
        doc.claims[0].scope = GraphScope::session("s1");

        let text = render_entity(&doc, &refs());
        assert!(
            text.contains("layer: working"),
            "abweichende Ebene muss am Claim stehen:\n{text}"
        );

        let parsed = parse_entity(&text).expect("muss parsen");
        assert_eq!(parsed.entity.layer, GraphLayer::Canonical);
        assert_eq!(parsed.claims[0].layer, GraphLayer::Working);
        assert_eq!(parsed.claims[0].scope, GraphScope::session("s1"));
        assert_eq!(parsed, doc);
    }

    /// Der Normalfall kostet keine zwei Zeilen je Aussage: liegt der Claim im
    /// selben Ziel wie sein Subjekt, stehen `layer`/`scope` gar nicht erst da.
    #[test]
    fn gleiches_ziel_wird_nicht_am_claim_wiederholt() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let claims_block = text
            .split("claims:")
            .nth(1)
            .expect("claims-Block")
            .split("sources:")
            .next()
            .expect("bis sources");
        assert!(
            !claims_block.contains("layer:"),
            "redundante Ebene am Claim:\n{claims_block}"
        );
    }

    #[test]
    fn zweimaliges_rendern_ist_byte_identisch() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let parsed = parse_entity(&text).expect("muss parsen");
        let text2 = render_entity(&parsed, &refs());
        assert_eq!(text, text2);
    }

    #[test]
    fn frontmatter_ist_konform() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let (fm_text, body) = markdown::split_frontmatter(&text).expect("Frontmatter muss da sein");
        let pairs = yaml::parse_document(fm_text).expect("Frontmatter muss parsen");
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);

        assert_eq!(get("type").and_then(YamlValue::as_str), Some("Konzept"));
        assert_eq!(
            get("title").and_then(YamlValue::as_str),
            Some("Parallele Tool-Aufrufe")
        );
        assert!(get("description").and_then(YamlValue::as_str).is_some());
        assert!(get("tags")
            .and_then(YamlValue::as_seq)
            .is_some_and(|s| !s.is_empty()));
        assert_eq!(get("status").and_then(YamlValue::as_str), Some("stable"));
        let generated = get("generated")
            .and_then(YamlValue::as_map)
            .expect("generated fehlt");
        let by = generated
            .get("by")
            .and_then(YamlValue::as_str)
            .expect("generated.by fehlt");
        assert!(
            Actor::parse(by).is_some(),
            "generated.by '{by}' ist kein gültiger Actor"
        );
        let at = generated
            .get("at")
            .and_then(YamlValue::as_str)
            .expect("generated.at fehlt");
        assert!(instant::from_rfc3339(at).is_some());

        // Jeder sources[]-Eintrag hat ein nicht-leeres `resource` (§5.1).
        let sources = get("sources")
            .and_then(YamlValue::as_seq)
            .expect("sources fehlt");
        for entry in sources {
            let resource = entry
                .as_map()
                .and_then(|m| m.get("resource"))
                .and_then(YamlValue::as_str)
                .expect("resource fehlt");
            assert!(!resource.is_empty() && !resource.contains(char::is_whitespace));
        }

        // Jede Footnote-Referenz im Body trifft ein sources[].id.
        let source_ids: Vec<&str> = sources
            .iter()
            .filter_map(|e| {
                e.as_map()
                    .and_then(|m| m.get("id"))
                    .and_then(YamlValue::as_str)
            })
            .collect();
        for label in markdown::footnote_refs(body) {
            assert!(
                source_ids.contains(&label.as_str()),
                "Footnote [^{label}] trifft keine Quelle"
            );
        }

        // Jeder Body-Link zeigt auf einen Pfad aus `refs`.
        let ref_map = refs();
        let known_paths: Vec<&str> = ref_map.values().map(|r| r.path.as_str()).collect();
        for (_, target) in markdown::links(body) {
            assert!(
                known_paths.contains(&target.as_str()),
                "Link auf '{target}' zeigt auf keinen bekannten Pfad"
            );
        }
    }

    #[test]
    fn unbekannte_objekt_id_wird_in_backticks_geschrieben() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let (_, body) = markdown::split_frontmatter(&text).unwrap();
        // C-2 zeigt auf E-9, das nicht in refs() steht.
        assert!(body.contains("`E-9`"));
        assert!(
            !body.lines().any(|l| l.contains("E-9") && l.contains("](")),
            "E-9 darf nicht verlinkt sein"
        );
    }

    #[test]
    fn alle_claims_superseded_ergibt_deprecated() {
        let mut doc = voll_bestuecktes_doc();
        doc.claims[0].status = ClaimStatus::Superseded;
        let text = render_entity(&doc, &refs());
        let (fm_text, _) = markdown::split_frontmatter(&text).unwrap();
        let pairs = yaml::parse_document(fm_text).unwrap();
        let status = pairs
            .iter()
            .find(|(k, _)| k == "status")
            .and_then(|(_, v)| v.as_str());
        assert_eq!(status, Some("deprecated"));
    }

    #[test]
    fn working_layer_ohne_deprecated_ist_draft() {
        let mut doc = voll_bestuecktes_doc();
        doc.entity.layer = GraphLayer::Working;
        for claim in &mut doc.claims {
            claim.layer = GraphLayer::Working;
        }
        let text = render_entity(&doc, &refs());
        let (fm_text, _) = markdown::split_frontmatter(&text).unwrap();
        let pairs = yaml::parse_document(fm_text).unwrap();
        let status = pairs
            .iter()
            .find(|(k, _)| k == "status")
            .and_then(|(_, v)| v.as_str());
        assert_eq!(status, Some("draft"));
    }

    #[test]
    fn keine_claims_ergibt_festen_body() {
        let mut doc = voll_bestuecktes_doc();
        doc.claims.clear();
        doc.sources.clear();
        let text = render_entity(&doc, &refs());
        let (_, body) = markdown::split_frontmatter(&text).unwrap();
        // `split_frontmatter` liefert die Leerzeile nach dem schließenden
        // `---` als Teil des Body zurück (siehe `markdown::compose`) — der
        // eigentliche Body-Inhalt ist die restliche Zeichenkette.
        assert_eq!(body, "\n# Aussagen\n\n(keine Aussagen)\n");
    }

    #[test]
    fn fehlender_agentkit_block_ist_ein_fehler() {
        let text = "---\ntype: thing\ntitle: X\n---\n\nBody\n";
        let err = parse_entity(text).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("agentkit")));
    }

    #[test]
    fn fehlendes_type_ist_ein_fehler() {
        let text = "---\ntitle: X\nagentkit: { id: E-1 }\n---\n\nBody\n";
        let err = parse_entity(text).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("'type'")));
    }

    #[test]
    fn kaputter_zeitstempel_ist_ein_fehler() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let broken = text.replacen("2026-01-01T00:00:00Z", "nicht-mal-ein-datum", 1);
        let err = parse_entity(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Zeitstempel")));
    }

    #[test]
    fn unbekannter_status_ist_ein_fehler() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let broken = text.replacen("status: confirmed", "status: quatsch", 1);
        let err = parse_entity(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("unbekannter Wert")));
    }

    #[test]
    fn unbekannter_layer_ist_ein_fehler() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let broken = text.replacen("layer: canonical", "layer: quatsch", 1);
        let err = parse_entity(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("agentkit.layer")));
    }

    #[test]
    fn confidence_wird_auf_zwei_nachkommastellen_angezeigt() {
        let doc = voll_bestuecktes_doc();
        let text = render_entity(&doc, &refs());
        let (frontmatter, body) = markdown::split_frontmatter(&text).unwrap();
        assert!(body.contains("0.82"), "erwartete '0.82' im Body:\n{body}");
        // Und das Frontmatter darf dem Body nicht widersprechen: `f64::from`
        // einer `f32`-Konfidenz schrieb hier `0.8199999928474426` hin.
        assert!(
            frontmatter.contains("confidence: 0.82\n"),
            "Frontmatter widerspricht dem Body:\n{frontmatter}"
        );
    }

    #[test]
    fn lange_ausschnitte_werden_einzeilig_und_gekuerzt() {
        let mut doc = voll_bestuecktes_doc();
        let lang = "x".repeat(250);
        doc.sources[0].excerpt = Some(format!("erste Zeile\nzweite   Zeile {lang}"));
        let text = render_entity(&doc, &refs());
        let (_, body) = markdown::split_frontmatter(&text).unwrap();
        let quellen_zeile = body
            .lines()
            .find(|l| l.starts_with("[^S-1]:"))
            .expect("Quellenzeile fehlt");
        assert!(!quellen_zeile.contains('\n'));
        assert!(quellen_zeile.ends_with('…'));
        assert!(quellen_zeile.chars().count() < 250);
    }

    // -----------------------------------------------------------------------
    // Episoden
    // -----------------------------------------------------------------------

    fn voll_bestuecktes_episode_doc() -> EpisodeDoc {
        let episode = GraphEpisode {
            id: "EP-3".to_string(),
            actor: Actor::agent("okf-agent"),
            summary:
                "Erste Zeile des Verlaufs.\nZweite Zeile mit mehr Details\nüber Größe hinüber."
                    .to_string(),
            scope: GraphScope::session("run-4711"),
            source_ids: vec!["S-1".to_string(), "S-2".to_string()],
            created_revision: 4,
            created_at: ts("2026-02-01T09:00:00Z"),
        };
        let source_1 = GraphSource {
            id: "S-1".to_string(),
            source_type: "tool_result".to_string(),
            agent_id: Some(Actor::human("dana")),
            run_id: Some("run-4711".to_string()),
            tool_call_id: Some("call-1".to_string()),
            artifact_uri: None,
            excerpt: Some("okf_agent hat 3 Fundstellen redigiert".to_string()),
            content_hash: "aaa111".to_string(),
            created_revision: 3,
            created_at: ts("2026-02-01T08:59:00Z"),
        };
        let source_2 = GraphSource {
            id: "S-2".to_string(),
            source_type: "document".to_string(),
            agent_id: None,
            run_id: None,
            tool_call_id: None,
            artifact_uri: Some("https://example.com/protokoll".to_string()),
            excerpt: None,
            content_hash: "bbb222".to_string(),
            created_revision: 2,
            created_at: ts("2026-02-01T08:00:00Z"),
        };
        EpisodeDoc {
            episode,
            sources: vec![source_1, source_2],
        }
    }

    #[test]
    fn episode_round_trip_erhaelt_alle_daten() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let parsed = parse_episode(&text).expect("muss parsen");
        assert_eq!(parsed, doc, "Episoden-Round-Trip fehlgeschlagen:\n{text}");
    }

    #[test]
    fn episode_zweimaliges_rendern_ist_byte_identisch() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let parsed = parse_episode(&text).expect("muss parsen");
        let text2 = render_episode(&parsed);
        assert_eq!(text, text2);
    }

    #[test]
    fn episode_frontmatter_ist_konform() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let (fm_text, body) = markdown::split_frontmatter(&text).expect("Frontmatter muss da sein");
        let pairs = yaml::parse_document(fm_text).expect("Frontmatter muss parsen");
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);

        assert_eq!(get("type").and_then(YamlValue::as_str), Some("Episode"));
        let title = get("title")
            .and_then(YamlValue::as_str)
            .expect("title fehlt");
        assert_eq!(title, "Erste Zeile des Verlaufs.");
        assert!(!title.contains('\n'));
        let description = get("description")
            .and_then(YamlValue::as_str)
            .expect("description fehlt");
        assert!(!description.contains('\n'));
        assert!(get("tags")
            .and_then(YamlValue::as_seq)
            .is_some_and(|s| s.iter().any(|v| v.as_str() == Some("episode"))
                && s.iter().any(|v| v.as_str() == Some("session"))));
        assert_eq!(get("status").and_then(YamlValue::as_str), Some("stable"));

        let sources = get("sources")
            .and_then(YamlValue::as_seq)
            .expect("sources fehlt");
        let source_ids: Vec<&str> = sources
            .iter()
            .filter_map(|e| {
                e.as_map()
                    .and_then(|m| m.get("id"))
                    .and_then(YamlValue::as_str)
            })
            .collect();
        for label in markdown::footnote_refs(body) {
            assert!(
                source_ids.contains(&label.as_str()),
                "Footnote [^{label}] trifft keine Quelle"
            );
        }
    }

    #[test]
    fn episode_body_enthaelt_vollstaendige_summary() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let (_, body) = markdown::split_frontmatter(&text).unwrap();
        assert!(body.contains(&doc.episode.summary));
        assert!(body.starts_with("\n# Verlauf\n\n"));
    }

    #[test]
    fn episode_titel_wird_auf_80_zeichen_gekuerzt() {
        let mut doc = voll_bestuecktes_episode_doc();
        doc.episode.summary = format!("{}\nzweite Zeile", "x".repeat(120));
        let text = render_episode(&doc);
        let (fm_text, _) = markdown::split_frontmatter(&text).unwrap();
        let pairs = yaml::parse_document(fm_text).unwrap();
        let title = pairs
            .iter()
            .find(|(k, _)| k == "title")
            .and_then(|(_, v)| v.as_str())
            .unwrap();
        assert!(title.chars().count() <= 81, "Titel zu lang: {title}");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn episode_ohne_quellen_hat_keinen_quellen_abschnitt() {
        let mut doc = voll_bestuecktes_episode_doc();
        doc.sources.clear();
        doc.episode.source_ids.clear();
        let text = render_episode(&doc);
        let (_, body) = markdown::split_frontmatter(&text).unwrap();
        assert!(!body.contains("# Quellen"));
        let parsed = parse_episode(&text).expect("muss parsen");
        assert_eq!(parsed, doc);
    }

    #[test]
    fn episode_falscher_type_ist_ein_fehler() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let broken = text.replacen("type: Episode", "type: Konzept", 1);
        let err = parse_episode(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("'type'")));
    }

    #[test]
    fn episode_fehlender_agentkit_block_ist_ein_fehler() {
        let text = "---\ntype: Episode\ntitle: X\n---\n\nBody\n";
        let err = parse_episode(text).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("agentkit")));
    }

    #[test]
    fn episode_fehlender_verlauf_abschnitt_ist_ein_fehler() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let broken = text.replacen("# Verlauf", "# Anderswas", 1);
        let err = parse_episode(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Verlauf")));
    }

    #[test]
    fn episode_ungueltiger_actor_ist_ein_fehler() {
        let doc = voll_bestuecktes_episode_doc();
        let text = render_episode(&doc);
        let broken = text.replacen("actor: agent:okf-agent", "actor: 'kein actor'", 1);
        let err = parse_episode(&broken).unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("actor")));
    }
}
