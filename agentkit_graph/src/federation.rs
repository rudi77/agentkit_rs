//! Der Verbund: ein Projekt-Bundle und, optional, das Benutzer-Bundle dahinter.
//!
//! Beide sind eigenständige Graphen mit eigenen, überlappenden IDs — `C-17`
//! gibt es in jedem. Getrennt bleiben sie, damit das Projektwissen beim
//! Projekt liegt (sichtbar, kopierbar) und nicht in einem benutzerweiten Topf.
//! Der Preis ist die Mehrdeutigkeit der IDs, und die löst genau diese eine
//! Stelle auf: an der Tool-Grenze trägt alles aus dem Benutzer-Bundle das
//! Präfix [`USER_PREFIX`], eine ID ohne Präfix meint das Projekt-Bundle. In
//! den Dateien steht nie ein Präfix.
//!
//! Gelesen wird aus beiden, das Projekt zuerst. Geschrieben wird nur ins
//! Projekt. Ins Benutzer-Bundle führt ein einziger Weg,
//! [`GraphFederation::promote_to_user`], und der verlangt einen Menschen.

use std::sync::Arc;

use agentkit::count_tokens_text;

use crate::access::GraphAccess;
use crate::error::GraphError;
use crate::evidence::GraphEvidence;
use crate::model::{Actor, ClaimStatus, GraphScope, GraphSource, GraphTarget};
use crate::retrieval::{self, GraphQuery, Subgraph};
use crate::store::GraphStore;
use crate::write::{ClaimExport, GraphReceipt, GraphWriteCommand};

/// Präfix der IDs aus dem Benutzer-Bundle, z. B. `u:C-17`.
pub const USER_PREFIX: &str = "u:";

/// Ein Graph und der Zugriff, unter dem er benutzt wird.
#[derive(Clone)]
pub struct GraphMember {
    pub store: Arc<GraphStore>,
    pub access: GraphAccess,
}

#[derive(Clone)]
pub struct GraphFederation {
    pub project: GraphMember,
    pub user: Option<GraphMember>,
    /// Was in diesem Lauf als Beleg zählt (siehe [`GraphEvidence`]). Geteilt
    /// mit jedem Klon — Tools, Recall und Schwarm-Mitglieder führen EIN Buch.
    pub evidence: GraphEvidence,
}

/// Ergebnis einer Suche über den Verbund.
pub struct FederatedHits {
    /// Der gerenderte Ausschnitt (`<graph-context>`), IDs bereits qualifiziert.
    pub text: String,
    pub claims: usize,
    /// true ⇔ mindestens ein Startpunkt wurde gefunden — unterscheidet „kenne
    /// ich nicht" von „dazu ist nichts gespeichert".
    pub seeded: bool,
}

impl GraphFederation {
    /// Nur ein Projekt-Graph — verhält sich wie der Graph allein.
    pub fn single(store: Arc<GraphStore>, access: GraphAccess) -> Self {
        GraphFederation {
            project: GraphMember { store, access },
            user: None,
            evidence: GraphEvidence::default(),
        }
    }

    /// Hängt das Benutzer-Bundle lesend an.
    pub fn with_user(mut self, store: Arc<GraphStore>) -> Self {
        let access = GraphAccess::user_read_only(&self.project.access.principal);
        self.user = Some(GraphMember { store, access });
        self
    }

    /// Derselbe Verbund unter anderem Autor (Schwarm-Mitglieder).
    pub fn as_principal(&self, principal: &str) -> Self {
        let rename = |m: &GraphMember| GraphMember {
            store: m.store.clone(),
            access: m.access.as_principal(principal),
        };
        GraphFederation {
            project: rename(&self.project),
            user: self.user.as_ref().map(rename),
            evidence: self.evidence.clone(),
        }
    }

    /// Löst eine ID von der Tool-Grenze auf: das zuständige Mitglied, die ID
    /// ohne Präfix und das Präfix selbst. `None` für eine `u:`-ID ohne
    /// Benutzer-Bundle.
    pub fn locate<'a>(&'a self, id: &'a str) -> Option<(&'a GraphMember, &'a str, &'static str)> {
        let id = id.trim();
        match id.strip_prefix(USER_PREFIX) {
            Some(local) => self.user.as_ref().map(|m| (m, local, USER_PREFIX)),
            None => Some((&self.project, id, "")),
        }
    }

    /// Sucht in beiden Graphen. Die Treffer des Projekts stehen vor denen des
    /// Benutzers; innerhalb eines Graphen gilt dessen Rangfolge. Das Budget
    /// der Abfrage gilt für beide zusammen — was das Projekt verbraucht,
    /// bleibt dem Benutzer-Wissen nicht mehr.
    pub fn search(&self, query: &GraphQuery, include_sources: bool) -> FederatedHits {
        let project_index = self.project.store.snapshot();
        let project_query = GraphQuery {
            seeds: seeds_for(&query.seeds, false),
            ..query.clone()
        };
        let project_hits =
            retrieval::search(&project_index, &self.project.access.view, &project_query);

        let user = self.user.as_ref().map(|member| {
            let index = member.store.snapshot();
            let used = retrieval::render(&project_index, &project_hits, include_sources);
            let user_query = GraphQuery {
                seeds: seeds_for(&query.seeds, true),
                max_claims: query.max_claims.saturating_sub(project_hits.claims.len()),
                max_tokens: if project_hits.is_empty() {
                    query.max_tokens
                } else {
                    query.max_tokens.saturating_sub(count_tokens_text(&used))
                },
                ..query.clone()
            };
            // Explizite Startpunkte, von denen keiner dem Benutzer-Bundle gilt:
            // dann dort nicht auf die Freitextsuche zurückfallen.
            let nothing_to_ask = !query.seeds.is_empty() && user_query.seeds.is_empty();
            let hits = if user_query.max_claims == 0 || user_query.max_tokens == 0 || nothing_to_ask
            {
                Subgraph::default()
            } else {
                retrieval::search(&index, &member.access.view, &user_query)
            };
            (index, hits)
        });

        let mut parts = vec![(&*project_index, &project_hits, "")];
        if let Some((index, hits)) = &user {
            parts.push((&**index, hits, USER_PREFIX));
        }
        FederatedHits {
            text: retrieval::render_parts(&parts, include_sources),
            claims: parts.iter().map(|(_, sub, _)| sub.claims.len()).sum(),
            seeded: parts.iter().any(|(_, sub, _)| !sub.seeds.is_empty()),
        }
    }

    /// Wie viel dauerhaftes Wissen dieser Leser hat: (Projekt, Benutzer).
    /// Gezählt wird, was der Recall auch liefern würde — Bestätigtes in der
    /// Sicht.
    pub fn knowledge_counts(&self) -> (usize, usize) {
        let count = |member: &GraphMember| {
            let index = member.store.snapshot();
            index
                .claims()
                .filter(|c| {
                    c.status == ClaimStatus::Confirmed && index.sees_claim(c, &member.access.view)
                })
                .count()
        };
        (count(&self.project), self.user.as_ref().map_or(0, count))
    }

    /// Die Quellen einer Aussage samt der Aussage selbst — `None`, wenn es sie
    /// für diesen Leser nicht gibt (unbekannt ODER außerhalb der Sicht; die
    /// Antwort ist bewusst dieselbe).
    pub fn evidence(&self, claim_id: &str) -> Option<(ClaimExport, &'static str)> {
        let (member, local, prefix) = self.locate(claim_id)?;
        let index = member.store.snapshot();
        ClaimExport::read(&index, local, &member.access)
            .ok()
            .map(|export| (export, prefix))
    }

    /// Hebt eine bestätigte Aussage aus dem Projekt- ins Benutzer-Wissen.
    ///
    /// Der einzige Weg dorthin, und er gehört dem Menschen: `approved_by` muss
    /// ein `human:…` sein (geprüft im Schreibpfad), und kein Tool ruft diese
    /// Funktion auf. Sonst wanderte Wissen über ein Projekt — Kundencode — in
    /// jedes andere.
    pub fn promote_to_user(
        &self,
        claim_id: &str,
        approved_by: Actor,
    ) -> Result<GraphReceipt, GraphError> {
        let user = self.user.as_ref().ok_or_else(|| {
            GraphError::Denied("es ist kein Benutzer-Bundle angebunden".to_string())
        })?;
        if claim_id.trim().starts_with(USER_PREFIX) {
            return Err(GraphError::Invalid(format!(
                "'{}' liegt bereits im Benutzer-Wissen",
                claim_id.trim()
            )));
        }
        let export = ClaimExport::read(
            &self.project.store.snapshot(),
            claim_id.trim(),
            &self.project.access,
        )?;
        let access = user
            .access
            .clone()
            .with_promotion(GraphTarget::canonical(GraphScope::user()));
        user.store.submit(
            GraphWriteCommand::AdoptClaim {
                export: Box::new(export),
                approved_by,
            },
            &access,
        )
    }
}

/// Die Startpunkte einer Abfrage für EIN Mitglied. Namen gelten beiden;
/// eine ID gilt dem Benutzer-Bundle nur mit Präfix und dem Projekt nur ohne.
fn seeds_for(seeds: &[String], user: bool) -> Vec<String> {
    seeds
        .iter()
        .filter_map(|seed| {
            let seed = seed.trim();
            match (seed.strip_prefix(USER_PREFIX), user) {
                (Some(local), true) => Some(local.to_string()),
                (Some(_), false) => None,
                (None, true) if looks_like_entity_id(seed) => None,
                (None, _) => Some(seed.to_string()),
            }
        })
        .collect()
}

fn looks_like_entity_id(text: &str) -> bool {
    text.strip_prefix("E-")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Die Quelle einer Aussage in der Form, in der `graph_evidence` sie ausgibt.
pub(crate) fn source_json(source: &GraphSource, prefix: &str) -> serde_json::Value {
    serde_json::json!({
        "source_id": format!("{prefix}{}", source.id),
        "type": source.source_type,
        "agent": source.agent_id,
        "run": source.run_id,
        "tool_call_id": source.tool_call_id,
        "artifact": source.artifact_uri,
        "excerpt": source.excerpt,
        "revision": source.created_revision,
    })
}
