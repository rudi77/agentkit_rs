//! Die Graph-Tools — die einzige Sicht des Modells auf den Graphen.
//!
//! Registriert werden sie in eine **vorhandene** [`ToolRegistry`]; der Agent-Kern
//! kennt das Crate nicht. Was ein Tool darf, steckt im [`GraphAccess`], den die
//! Closure einschließt: Autor, Lauf-ID, Sicht, Schreib- und Promotionsziel setzt
//! die Laufzeit, nie das Modell. Deshalb hat kein Tool ein `scope`- oder
//! `agent`-Argument — es gäbe sonst einen Weg, sich als jemand anderes auszugeben.
//!
//! Schreib-Tools erscheinen nur, wenn der Zugriff sie erlaubt: ohne Schreibziel
//! kein `graph_remember`, ohne Promotionsziel kein `graph_promote`, ohne beides
//! kein `graph_retract`. Das Modell sieht in einer Nur-Lese-Sitzung gar nicht
//! erst, was es nicht tun kann.
//!
//! Dauerhaft wird nur Belegtes: eine Belegstelle zählt als fremde Quelle, wenn
//! sie wörtlich in einem Tool-Ergebnis dieses Laufs steht (siehe
//! `crate::evidence`). Alles andere bleibt die eigene Aussage des Modells —
//! festhalten darf es sie, befördern nicht.

use std::sync::Arc;

use agentkit::ToolRegistry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::access::GraphAccess;
use crate::federation::{source_json, GraphFederation, USER_PREFIX};
use crate::model::{ClaimStatus, SELF_SOURCE};
use crate::retrieval::GraphQuery;
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
- 'graph_retract' — stellt sich eine Aussage als falsch heraus, ziehe sie mit Begründung zurück.

Vorläufiges bleibt vorläufig: schreibe Beobachtungen als 'observation', Vermutungen als \
'hypothesis'. Dauerhaft wird nur, was belegt ist: gib bei 'graph_remember' als 'excerpt' ein \
WÖRTLICHES Zitat aus einem Tool-Ergebnis dieses Laufs an (Dateiinhalt, Testausgabe, \
Shell-Ausgabe). Ohne ein solches Zitat bleibt die Aussage vorläufig und lässt sich nicht befördern.

IDs mit dem Präfix 'u:' (etwa 'u:C-17') stammen aus dem projektübergreifenden Wissen des \
Benutzers. Du kannst es lesen, aber nicht ändern — dorthin hebt nur der Mensch etwas.";

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
    ///
    /// Wirkt nur für eine Beobachtung mit geprüfter Belegstelle. Eine
    /// Hypothese wird nie im selben Aufruf dauerhaft, in dem sie entsteht.
    durable: Option<bool>,
}

#[derive(Deserialize)]
struct PromoteArgs {
    claim_id: String,
}

#[derive(Deserialize)]
struct RetractArgs {
    claim_id: String,
    reason: String,
}

/// Registriert die Graph-Tools passend zum Zugriff: Lesen immer, `graph_remember`
/// nur mit Schreibziel, `graph_promote` nur mit Promotionsziel, `graph_retract`
/// mit einem von beiden.
///
/// **Zuletzt aufrufen.** Mit Schreibziel hängt sich hier außerdem ein Mitschnitt
/// an alle Tools, die schon in der Registry stehen — nur was sie liefern, kann
/// `graph_remember` später als Belegstelle anerkennen. Ein danach registriertes
/// Tool belegt nichts.
pub fn register_graph_tools(tools: &mut ToolRegistry, store: Arc<GraphStore>, access: GraphAccess) {
    register_federated_graph_tools(tools, GraphFederation::single(store, access));
}

/// Wie [`register_graph_tools`], über einem Verbund aus Projekt- und
/// Benutzer-Bundle. Gelesen wird aus beiden; IDs aus dem Benutzer-Bundle
/// tragen in Aus- und Eingabe das Präfix `u:`. Geschrieben, befördert und
/// zurückgezogen wird nur im Projekt — das Benutzer-Wissen ändert kein Tool.
pub fn register_federated_graph_tools(tools: &mut ToolRegistry, federation: GraphFederation) {
    let fed = Arc::new(federation);
    let store = fed.project.store.clone();
    let access = Arc::new(fed.project.access.clone());
    let evidence = fed.evidence.0.clone();
    if access.can_write() {
        evidence.watch(tools);
    }

    // ---------------------------------------------------------- graph_search
    let f = fed.clone();
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
            let mut query = GraphQuery::text(&args.query);
            if let Some(max) = args.max_claims {
                query.max_claims = max.clamp(1, 100);
            }
            if let Some(hops) = args.max_hops {
                query.max_hops = hops.clamp(1, 3);
            }
            let hits = f.search(&query, true);
            if hits.claims == 0 {
                format!("(nichts im Graph zu '{}' gefunden)", args.query.trim())
            } else {
                hits.text
            }
        },
    );

    // ------------------------------------------------------- graph_neighbors
    let f = fed.clone();
    tools.add_typed(
        "graph_neighbors",
        "Zeigt die Umgebung einer Entity im Wissensgraphen: alle Aussagen, an denen \
         sie beteiligt ist, und deren Nachbarn.",
        json!({
            "type": "object",
            "properties": {
                "entity": {"type": "string", "description": "Name oder ID (z. B. 'E-7', im Benutzer-Wissen 'u:E-7') der Entity."},
                "hops": {"type": "integer", "description": "1 oder 2 (Standard 1)."}
            },
            "required": ["entity"]
        }),
        move |args: NeighborArgs| {
            let query = GraphQuery::seeded(&args.entity).hops(args.hops.unwrap_or(1).clamp(1, 3));
            let hits = f.search(&query, true);
            if !hits.seeded {
                format!("(Entity '{}' ist im Graph nicht bekannt)", args.entity.trim())
            } else if hits.claims == 0 {
                format!("(zu '{}' sind keine Aussagen gespeichert)", args.entity.trim())
            } else {
                hits.text
            }
        },
    );

    // -------------------------------------------------------- graph_evidence
    let f = fed.clone();
    tools.add_typed(
        "graph_evidence",
        "Liefert die Quellen einer Aussage (Provenance): woher sie stammt, wer sie \
         erzeugt hat und in welchem Lauf.",
        json!({
            "type": "object",
            "properties": {
                "claim_id": {"type": "string", "description": "ID der Aussage, z. B. 'C-21' oder 'u:C-21'."}
            },
            "required": ["claim_id"]
        }),
        move |args: EvidenceArgs| {
            // Die Sicht gilt auch hier: Claim-IDs sind fortlaufend und damit
            // erratbar. Eine Aussage außerhalb der Sicht bekommt dieselbe
            // Antwort wie eine, die es nicht gibt — sonst wäre die Meldung
            // selbst die Auskunft, dass dort etwas liegt.
            let Some((export, prefix)) = f
                .evidence(&args.claim_id)
                .filter(|(export, _)| !export.sources.is_empty())
            else {
                return format!(
                    "(keine Quellen zu '{}' — unbekannte Claim-ID?)",
                    args.claim_id.trim()
                );
            };
            let list: Vec<Value> = export
                .sources
                .iter()
                .map(|src| source_json(src, prefix))
                .collect();
            let mut out = json!({"claim_id": args.claim_id.trim(), "sources": list});
            // Eine zurückgezogene Aussage findet keine Suche mehr — wer sie
            // über ihre ID nachschlägt, muss sehen, DASS und WARUM.
            if let Some(retraction) = &export.claim.retracted {
                out["status"] = json!(ClaimStatus::Retracted.wire_name());
                out["retracted"] = json!({"by": retraction.by, "reason": retraction.reason});
            }
            out.to_string()
        },
    );

    // -------------------------------------------------------- graph_remember
    if access.can_write() {
        let (s, a, seen) = (store.clone(), access.clone(), evidence.clone());
        tools.add_typed(
            "graph_remember",
            "Hält eine Beobachtung oder Hypothese als Tripel im Arbeits-Graphen fest \
             (Subjekt – Prädikat – Objekt). Sichtbar für alle, die denselben Arbeits-Scope \
             lesen. Soll eine belegte Beobachtung auch SPÄTERE Läufe erreichen, setze \
             'durable': true — das erspart den separaten 'graph_promote'-Aufruf.",
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
                    "excerpt": {"type": "string", "description": "Belegstelle: ein WÖRTLICHES Zitat aus einem Tool-Ergebnis dieses Laufs (Dateiinhalt, Testausgabe, Shell-Ausgabe). Nur eine so belegte Aussage kann dauerhaft werden."},
                    "durable": {
                        "type": "boolean",
                        "description": "true = die Aussage soll DAUERHAFT werden und für spätere Läufe sichtbar sein (spart den separaten 'graph_promote'-Aufruf). Wirkt nur für eine 'observation' mit geprüfter Belegstelle in 'excerpt'."
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
                // Ohne geprüfte Belegstelle ist die Quelle das Modell selbst.
                let mut source = SourceDraft::new(SELF_SOURCE);
                let mut belegt = false;
                if let Some(text) = args.excerpt.as_deref().filter(|t| !t.trim().is_empty()) {
                    match seen.find(text) {
                        Some(witness) => {
                            source = witness.source(text);
                            belegt = true;
                        }
                        None => source = source.excerpt(text),
                    }
                }
                let draft = ClaimDraft::new(&args.subject, &args.predicate, &args.object, source)
                    .status(status)
                    .confidence(args.confidence.unwrap_or(0.6));
                match s.submit(GraphWriteCommand::RecordClaim(draft), &a) {
                    Ok(receipt) => {
                        // Haltbarkeit direkt hier, wenn der Aufrufer sie will:
                        // ein zweiter Aufruf am Ende des Laufs kommt in der
                        // Praxis nicht (siehe `RememberArgs::durable`). Aber
                        // nur für eine belegte Beobachtung — eine Hypothese
                        // wird nie in dem Aufruf dauerhaft, in dem sie
                        // entsteht. Scheitert die Promotion, bleibt die
                        // Aussage vorläufig bestehen; das Festhalten ist der
                        // Zweck des Aufrufs, die Haltbarkeit die Zugabe.
                        let gewuenscht = args.durable.unwrap_or(false) && a.can_promote();
                        let mut dauerhaft = false;
                        let mut hinweis = None;
                        if gewuenscht && status != ClaimStatus::Observation {
                            hinweis = Some("eine Hypothese wird nicht im selben Aufruf dauerhaft");
                        } else if gewuenscht && !belegt {
                            hinweis = Some(
                                "nicht dauerhaft: 'excerpt' muss ein wörtliches Zitat aus \
                                 einem Tool-Ergebnis dieses Laufs sein",
                            );
                        } else if let (true, Some(id)) = (gewuenscht, receipt.claim_id.clone()) {
                            dauerhaft = s
                                .submit(GraphWriteCommand::PromoteClaim { claim_id: id }, &a)
                                .is_ok();
                        }
                        let mut out = json!({
                            "claim_id": receipt.claim_id,
                            "revision": receipt.revision,
                            "entities": receipt.entity_ids,
                            "merged": receipt.deduplicated,
                            "evidence": belegt,
                            "durable": dauerhaft,
                        });
                        if let Some(text) = hinweis {
                            out["hint"] = json!(text);
                        }
                        out.to_string()
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
                if args.claim_id.trim().starts_with(USER_PREFIX) {
                    return "ERROR: diese Aussage liegt bereits im Benutzer-Wissen".to_string();
                }
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

    // --------------------------------------------------------- graph_retract
    if access.can_write() || access.can_promote() {
        let (s, a) = (store.clone(), access.clone());
        tools.add_typed(
            "graph_retract",
            "Zieht eine Aussage zurück, die sich als falsch erwiesen hat. Sie bleibt über \
             'graph_evidence' nachlesbar, erscheint aber in keiner Suche mehr.",
            json!({
                "type": "object",
                "properties": {
                    "claim_id": {"type": "string", "description": "ID der Aussage, z. B. 'C-21'."},
                    "reason": {"type": "string", "description": "Warum die Aussage nicht (mehr) stimmt."}
                },
                "required": ["claim_id", "reason"]
            }),
            move |args: RetractArgs| {
                if args.claim_id.trim().starts_with(USER_PREFIX) {
                    return "ERROR: nicht erlaubt: das Benutzer-Wissen ändert nur der Mensch"
                        .to_string();
                }
                let command = GraphWriteCommand::RetractClaim {
                    claim_id: args.claim_id.trim().to_string(),
                    reason: args.reason,
                };
                match s.submit(command, &a) {
                    Ok(receipt) => json!({
                        "claim_id": receipt.claim_id,
                        "revision": receipt.revision,
                        "status": ClaimStatus::Retracted.wire_name(),
                    })
                    .to_string(),
                    Err(e) => format!("ERROR: {e}"),
                }
            },
        );
    }
}
