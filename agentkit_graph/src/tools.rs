//! Die Graph-Tools — die einzige Sicht des Modells auf den Graphen.
//!
//! Registriert werden sie in eine **vorhandene** [`ToolRegistry`]; der Agent-Kern
//! kennt das Crate nicht. Was ein Tool darf, steckt im [`GraphAccess`], den die
//! Closure einschließt: Autor, Lauf-ID, Sicht, Schreib- und Promotionsziel setzt
//! die Laufzeit, nie das Modell. Deshalb hat kein Tool ein `scope`- oder
//! `agent`-Argument — es gäbe sonst einen Weg, sich als jemand anderes auszugeben.
//!
//! Schreib-Tools erscheinen nur, wenn der Zugriff sie erlaubt: ohne Schreibziel
//! kein `graph_remember`, ohne Promotionsziel kein `graph_promote`. Das Modell
//! sieht in einer Nur-Lese-Sitzung gar nicht erst, was es nicht tun kann.

use std::sync::Arc;

use agentkit::ToolRegistry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::access::GraphAccess;
use crate::model::ClaimStatus;
use crate::retrieval::{self, GraphQuery};
use crate::store::GraphStore;
use crate::write::{ClaimDraft, GraphWriteCommand, SourceDraft};

/// Prompt-Fragment für Frontends, die die Graph-Tools einklinken (analog zu
/// `SWARM_SYSTEM` in agentkit_swarm). Wird an den agenten-spezifischen
/// Zusatz-Prompt angehängt, nicht in den Coding-Prompt eingebaut — der Kern
/// kennt den Graphen nicht.
pub const GRAPH_SYSTEM: &str = "\
## Wissensgraph

Dir steht ein dauerhafter Wissensgraph zur Verfügung. Er überdauert diese Sitzung.

- 'graph_search' — frage ihn, BEVOR du etwas mühsam neu herausfindest.
- 'graph_neighbors' — was hängt an einer bestimmten Sache?
- 'graph_evidence' — woher stammt eine Aussage? Prüfe das, bevor du dich darauf verlässt.
- 'graph_remember' — halte eine Beobachtung oder Hypothese als Tripel fest \
(Subjekt, Prädikat, Objekt). Kurze, normalisierte Prädikate ('verwendet', 'verursacht', \
'ersetzt'), keine ganzen Sätze.
- 'graph_promote' — erst wenn eine Aussage wirklich belegt ist, wird sie dauerhaftes Wissen.

Vorläufiges bleibt vorläufig: schreibe Beobachtungen als 'observation', Vermutungen als \
'hypothesis'. Kanonisch wird eine Aussage ausschließlich über 'graph_promote'.";

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    max_claims: Option<usize>,
    max_hops: Option<usize>,
}

#[derive(Deserialize)]
struct NeighborArgs {
    entity: String,
    hops: Option<usize>,
}

#[derive(Deserialize)]
struct EvidenceArgs {
    claim_id: String,
}

#[derive(Deserialize)]
struct RememberArgs {
    subject: String,
    predicate: String,
    object: String,
    status: Option<String>,
    confidence: Option<f32>,
    excerpt: Option<String>,
    /// Sofort dauerhaft machen, statt einen zweiten `graph_promote`-Aufruf zu
    /// verlangen.
    ///
    /// Der Anlass ist gemessen: Im Benchmark-Lauf 2026-08-08 schrieben zwei
    /// Arme zusammen 158 Aussagen und promoteten davon **zwei**. Ein Protokoll,
    /// dessen entscheidender Schritt genau dann fällig wird, wenn das Modell
    /// fertig werden will, verliert diesen Schritt — der erste Aufruf fühlt
    /// sich wie Erledigung an, der zweite ist der einzige, der zählt.
    durable: Option<bool>,
}

#[derive(Deserialize)]
struct PromoteArgs {
    claim_id: String,
}

/// Registriert die Graph-Tools passend zum Zugriff: Lesen immer, `graph_remember`
/// nur mit Schreibziel, `graph_promote` nur mit Promotionsziel.
pub fn register_graph_tools(tools: &mut ToolRegistry, store: Arc<GraphStore>, access: GraphAccess) {
    let access = Arc::new(access);

    // ---------------------------------------------------------- graph_search
    let (s, a) = (store.clone(), access.clone());
    tools.add_typed(
        "graph_search",
        "Durchsucht den Wissensgraphen nach Aussagen zu einer Frage und liefert den \
         relevanten Ausschnitt mit Claim-IDs, Status und Ebene.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Wonach gesucht wird (Freitext)."},
                "max_claims": {"type": "integer", "description": "Obergrenze der Aussagen (Standard 30)."},
                "max_hops": {"type": "integer", "description": "Wie weit traversiert wird (1 oder 2, Standard 2)."}
            },
            "required": ["query"]
        }),
        move |args: SearchArgs| {
            let index = s.snapshot();
            let mut query = GraphQuery::text(&args.query);
            if let Some(max) = args.max_claims {
                query.max_claims = max.clamp(1, 100);
            }
            if let Some(hops) = args.max_hops {
                query.max_hops = hops.clamp(1, 3);
            }
            let sub = retrieval::search(&index, &a.view, &query);
            if sub.is_empty() {
                format!("(nichts im Graph zu '{}' gefunden)", args.query.trim())
            } else {
                retrieval::render(&index, &sub, true)
            }
        },
    );

    // ------------------------------------------------------- graph_neighbors
    let (s, a) = (store.clone(), access.clone());
    tools.add_typed(
        "graph_neighbors",
        "Zeigt die Umgebung einer Entity im Wissensgraphen: alle Aussagen, an denen \
         sie beteiligt ist, und deren Nachbarn.",
        json!({
            "type": "object",
            "properties": {
                "entity": {"type": "string", "description": "Name oder ID (z. B. 'E-7') der Entity."},
                "hops": {"type": "integer", "description": "1 oder 2 (Standard 1)."}
            },
            "required": ["entity"]
        }),
        move |args: NeighborArgs| {
            let index = s.snapshot();
            let query = GraphQuery::seeded(&args.entity).hops(args.hops.unwrap_or(1).clamp(1, 3));
            let sub = retrieval::search(&index, &a.view, &query);
            if sub.seeds.is_empty() {
                format!("(Entity '{}' ist im Graph nicht bekannt)", args.entity.trim())
            } else if sub.is_empty() {
                format!("(zu '{}' sind keine Aussagen gespeichert)", args.entity.trim())
            } else {
                retrieval::render(&index, &sub, true)
            }
        },
    );

    // -------------------------------------------------------- graph_evidence
    let s = store.clone();
    tools.add_typed(
        "graph_evidence",
        "Liefert die Quellen einer Aussage (Provenance): woher sie stammt, wer sie \
         erzeugt hat und in welchem Lauf.",
        json!({
            "type": "object",
            "properties": {
                "claim_id": {"type": "string", "description": "ID der Aussage, z. B. 'C-21'."}
            },
            "required": ["claim_id"]
        }),
        move |args: EvidenceArgs| {
            let index = s.snapshot();
            let sources = retrieval::evidence(&index, args.claim_id.trim());
            if sources.is_empty() {
                return format!(
                    "(keine Quellen zu '{}' — unbekannte Claim-ID?)",
                    args.claim_id.trim()
                );
            }
            let list: Vec<Value> = sources
                .iter()
                .map(|src| {
                    json!({
                        "source_id": src.id,
                        "type": src.source_type,
                        "agent": src.agent_id,
                        "run": src.run_id,
                        "tool_call_id": src.tool_call_id,
                        "artifact": src.artifact_uri,
                        "excerpt": src.excerpt,
                        "revision": src.created_revision,
                    })
                })
                .collect();
            json!({"claim_id": args.claim_id.trim(), "sources": list}).to_string()
        },
    );

    // -------------------------------------------------------- graph_remember
    if access.can_write() {
        let (s, a) = (store.clone(), access.clone());
        tools.add_typed(
            "graph_remember",
            "Hält eine Beobachtung oder Hypothese als Tripel im Arbeits-Graphen fest \
             (Subjekt – Prädikat – Objekt). Sichtbar für alle, die denselben Arbeits-Scope \
             lesen. Soll die Aussage auch SPÄTERE Läufe erreichen, setze 'durable': true — \
             das erspart den separaten 'graph_promote'-Aufruf.",
            json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string", "description": "Worüber die Aussage geht."},
                    "predicate": {"type": "string", "description": "Kurze, normalisierte Beziehung, z. B. 'verursacht'."},
                    "object": {"type": "string", "description": "Womit das Subjekt in Beziehung steht."},
                    "status": {
                        "type": "string",
                        "enum": ["observation", "hypothesis"],
                        "description": "Beobachtet oder vermutet (Standard: observation)."
                    },
                    "confidence": {"type": "number", "description": "0.0 bis 1.0 (Standard 0.6)."},
                    "excerpt": {"type": "string", "description": "Belegstelle: Ausgabe, Zitat oder Fundort."},
                    "durable": {
                        "type": "boolean",
                        "description": "true = die Aussage soll DAUERHAFT werden und für spätere Läufe sichtbar sein (spart den separaten 'graph_promote'-Aufruf). Nur für Belegtes."
                    }
                },
                "required": ["subject", "predicate", "object"]
            }),
            move |args: RememberArgs| {
                let status = match args.status.as_deref().map(ClaimStatus::parse) {
                    None => ClaimStatus::Observation,
                    Some(Some(st)) => st,
                    Some(None) => {
                        return "ERROR: status muss 'observation' oder 'hypothesis' sein"
                            .to_string()
                    }
                };
                let mut source = SourceDraft::new("agent_turn");
                if let Some(text) = args.excerpt.as_deref().filter(|t| !t.trim().is_empty()) {
                    source = source.excerpt(text);
                }
                let draft = ClaimDraft::new(&args.subject, &args.predicate, &args.object, source)
                    .status(status)
                    .confidence(args.confidence.unwrap_or(0.6));
                match s.submit(GraphWriteCommand::RecordClaim(draft), &a) {
                    Ok(receipt) => {
                        // Haltbarkeit direkt hier, wenn der Aufrufer sie will:
                        // ein zweiter Aufruf am Ende des Laufs kommt in der
                        // Praxis nicht (siehe `RememberArgs::durable`). Scheitert
                        // die Promotion — etwa ohne Promotionsziel —, bleibt die
                        // Aussage vorläufig bestehen; das Festhalten ist der
                        // Zweck des Aufrufs, die Haltbarkeit die Zugabe.
                        let mut dauerhaft = false;
                        if let (true, true, Some(id)) = (
                            args.durable.unwrap_or(false),
                            a.can_promote(),
                            receipt.claim_id.clone(),
                        ) {
                            dauerhaft = s
                                .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &a)
                                .is_ok();
                        }
                        json!({
                            "claim_id": receipt.claim_id,
                            "revision": receipt.revision,
                            "entities": receipt.entity_ids,
                            "merged": receipt.deduplicated,
                            "durable": dauerhaft,
                        })
                        .to_string()
                    }
                    Err(e) => format!("ERROR: {e}"),
                }
            },
        );
    }

    // --------------------------------------------------------- graph_promote
    if access.can_promote() {
        let (s, a) = (store.clone(), access.clone());
        tools.add_typed(
            "graph_promote",
            "Übernimmt eine belegte Aussage aus dem Arbeits-Graphen ins dauerhafte Wissen. \
             Nur benutzen, wenn die Aussage wirklich bestätigt ist — widersprechende \
             dauerhafte Aussagen werden dabei als ersetzt markiert.",
            json!({
                "type": "object",
                "properties": {
                    "claim_id": {"type": "string", "description": "ID der Aussage, z. B. 'C-21'."}
                },
                "required": ["claim_id"]
            }),
            move |args: PromoteArgs| {
                let command = GraphWriteCommand::PromoteClaim {
                    claim_id: args.claim_id.trim().to_string(),
                };
                match s.submit(command, &a) {
                    Ok(receipt) => json!({
                        "claim_id": receipt.claim_id,
                        "revision": receipt.revision,
                        "superseded": receipt.superseded,
                    })
                    .to_string(),
                    Err(e) => format!("ERROR: {e}"),
                }
            },
        );
    }
}
