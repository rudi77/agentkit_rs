//! Der Vergleich: was der Graph über mehrere Dinge zu DERSELBEN Frage weiß,
//! nebeneinander.
//!
//! `graph_neighbors` zeigt die Umgebung EINER Entity. Wer zwei Verträge
//! vergleicht, müsste sie zweimal fragen und die Zeilen selbst zuordnen — und
//! was im zweiten fehlt, fehlt dann einfach in der Antwort. Hier ordnet der
//! Code zu: je Prädikat eine Zeile pro Entity, und eine Lücke steht da.
//!
//! Das setzt eine Schreibweise voraus: das Ding ist das **Subjekt**, die Frage
//! das **Prädikat** (`Vertrag A — kündigungsfrist — 3 Monate`). Die umgekehrte
//! Form (`Kündigungsfrist — beträgt — 3 Monate`) ließe beim Befördern die
//! Aussage des einen Dokuments die des anderen ersetzen, siehe
//! `GraphClaim::contradicts`.
//!
//! Gelesen wird nur; verglichen werden Namen, nicht Bedeutungen — `3 Monate`
//! und `ein Quartal` sind hier verschieden. Das Urteil darüber bleibt beim
//! Leser.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::federation::{GraphFederation, GraphMember, USER_PREFIX};
use crate::model::{normalize, ClaimStatus, GraphEntity};
use crate::store::GraphIndex;

/// Eine Aussage in einer Zelle: das Objekt und woher man es nachschlägt.
struct Cell {
    object: String,
    claim_id: String,
    hypothesis: bool,
}

/// Was der Graph über EINE Entity sagt: ihr Name und ihre Aussagen als
/// Subjekt, in der Reihenfolge des Bestands.
struct Column {
    label: String,
    claims: Vec<(String, Cell)>,
}

/// Stellt die Aussagen der genannten Entities je Prädikat nebeneinander.
///
/// Ein Name gilt beiden Bundles, eine ID genau einem (wie bei
/// `graph_neighbors`). Ersetzte und zurückgezogene Aussagen zählen nicht. Das
/// Ergebnis ist Text für den Modellkontext; eine unbekannte Entity ist eine
/// klar benannte Antwort, kein Fehler.
pub fn compare(federation: &GraphFederation, entities: &[String]) -> String {
    let wanted: Vec<&str> = entities
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();
    if wanted.len() < 2 {
        return "ERROR: ein Vergleich braucht mindestens zwei Entities".to_string();
    }
    let mut members = vec![(&federation.project, "")];
    if let Some(user) = &federation.user {
        members.push((user, USER_PREFIX));
    }
    let indexed: Vec<(Arc<GraphIndex>, &GraphMember, &str)> = members
        .into_iter()
        .map(|(member, prefix)| (member.store.snapshot(), member, prefix))
        .collect();

    let mut columns = Vec::new();
    for name in wanted {
        match column(&indexed, name) {
            Some(col) => columns.push(col),
            None => return format!("(Entity '{name}' ist im Graph nicht bekannt)"),
        }
    }
    render(&columns)
}

fn column(indexed: &[(Arc<GraphIndex>, &GraphMember, &str)], name: &str) -> Option<Column> {
    let mut label = None;
    let mut claims = Vec::new();
    for (index, member, prefix) in indexed {
        let view = &member.access.view;
        // Eine ID meint genau EIN Bundle: mit Präfix das des Benutzers, ohne
        // das Projekt. Ein Name wird in beiden gesucht.
        let by_id: Option<&Arc<GraphEntity>> = match name.strip_prefix(USER_PREFIX) {
            Some(local) if !prefix.is_empty() => index.entity(local),
            Some(_) => None,
            None if prefix.is_empty() => index.entity(name),
            None => None,
        }
        .filter(|e| view.sees(e.layer, &e.scope));
        let hits = match by_id {
            Some(entity) => vec![entity],
            None if name.starts_with(USER_PREFIX) => Vec::new(),
            None => index.by_alias(name, view),
        };
        for entity in hits {
            label.get_or_insert_with(|| entity.canonical_name.clone());
            for claim in index.incident_claims(&entity.id) {
                let counts = claim.subject == entity.id
                    && index.sees_claim(claim, view)
                    && !matches!(
                        claim.status,
                        ClaimStatus::Superseded | ClaimStatus::Retracted
                    );
                if !counts {
                    continue;
                }
                let object = index
                    .entity(&claim.object)
                    .map_or_else(|| claim.object.clone(), |e| e.canonical_name.clone());
                claims.push((
                    claim.predicate.clone(),
                    Cell {
                        object,
                        claim_id: format!("{prefix}{}", claim.id),
                        hypothesis: claim.status == ClaimStatus::Hypothesis,
                    },
                ));
            }
        }
    }
    label.map(|label| Column { label, claims })
}

fn render(columns: &[Column]) -> String {
    // Prädikate in der Reihenfolge, in der sie zuerst auftauchen — Schreibweise
    // der ersten Nennung, verglichen wird normalisiert.
    let mut predicates: Vec<(String, &str)> = Vec::new();
    for (predicate, _) in columns.iter().flat_map(|c| &c.claims) {
        let key = normalize(predicate);
        if !predicates.iter().any(|(k, _)| *k == key) {
            predicates.push((key, predicate));
        }
    }
    if predicates.is_empty() {
        return "(zu keiner dieser Entities sind Aussagen gespeichert)".to_string();
    }

    let (mut differing, mut gaps) = (0, 0);
    let mut body = String::new();
    for (key, shown) in &predicates {
        let rows: Vec<Vec<&Cell>> = columns
            .iter()
            .map(|col| {
                col.claims
                    .iter()
                    .filter(|(p, _)| normalize(p) == *key)
                    .map(|(_, cell)| cell)
                    .collect()
            })
            .collect();
        let objects = |cells: &[&Cell]| -> BTreeSet<String> {
            cells.iter().map(|c| normalize(&c.object)).collect()
        };
        let verdict = if rows.iter().any(Vec::is_empty) {
            gaps += 1;
            "Lücke"
        } else if rows.iter().any(|r| objects(r) != objects(&rows[0])) {
            differing += 1;
            "abweichend"
        } else {
            "gleich"
        };
        body.push_str(&format!("[{shown}] {verdict}\n"));
        for (col, cells) in columns.iter().zip(&rows) {
            let value = if cells.is_empty() {
                "(keine Aussage)".to_string()
            } else {
                cells
                    .iter()
                    .map(|c| {
                        let mark = if c.hypothesis { ", Hypothese" } else { "" };
                        format!("{} [{}{mark}]", c.object, c.claim_id)
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            body.push_str(&format!("  {}: {value}\n", col.label));
        }
    }
    format!(
        "<graph-compare entities=\"{}\" praedikate=\"{}\" abweichend=\"{differing}\" luecken=\"{gaps}\">\n{body}</graph-compare>",
        columns.len(),
        predicates.len(),
    )
}
