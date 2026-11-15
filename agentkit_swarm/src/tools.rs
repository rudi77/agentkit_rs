//! Die Schwarm-Tools — die einzige Sicht des Modells auf den Schwarm.
//!
//! Die [`crate::AgentActorRef`]s sind in Tool-Closures gekapselt; der Agent
//! sieht nur `swarm_peers`, `swarm_send`, `swarm_reply`, `swarm_broadcast`,
//! `swarm_propose` und `swarm_vote`. Alle Sendepfade sind fire-and-forget:
//! das Tool liefert sofort ein Status-JSON (weiche Fehler als Werte), die
//! Antwort des Empfängers kommt später als neue Nachricht in der eigenen
//! Mailbox an. `from` setzt immer die Closure — das Modell kann sich nicht
//! als anderer Agent ausgeben.

use crate::agent_actor::SwarmToolContext;
use crate::directory::sorted_peer_ids;
use crate::message::{DeliveryResult, MessageKind, Recipient};
use agentkit::ToolRegistry;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[derive(Deserialize)]
struct SendArgs {
    to: String,
    content: String,
    kind: Option<MessageKind>,
    correlation_id: Option<String>,
}

#[derive(Deserialize)]
struct ReplyArgs {
    content: String,
    kind: Option<MessageKind>,
}

#[derive(Deserialize)]
struct BroadcastArgs {
    content: String,
    kind: Option<MessageKind>,
}

#[derive(Deserialize)]
struct ProposeArgs {
    proposal: String,
}

#[derive(Deserialize)]
struct VoteArgs {
    proposal_id: String,
    approve: bool,
    comment: Option<String>,
}

/// Was ein Mitglied wissen muss, wenn seine Zustellung am Nachrichtenlimit
/// scheitert: der Schwarm ist damit NICHT tot — nur weitere Sends sind es.
/// Abschließen geht weiterhin, `swarm_propose`/`swarm_vote` sind budgetfrei.
const LIMIT_HINWEIS: &str = "Das Nachrichtenlimit ist erschöpft — weitere Sends kommen nicht \
mehr an. Schließe jetzt mit swarm_propose ab (Vorschläge und Stimmen zählen nicht gegen das \
Limit).";

/// Obergrenze der Abschluss-Vorschläge JE MITGLIED.
///
/// Vorschläge und Ablehnungen sind budgetfrei (der Abschluss darf nicht am
/// Nachrichtenlimit scheitern) und setzen den Hop-Zähler zurück. Ohne eigene
/// Grenze konnten zwei Mitglieder deshalb beliebig lange "Vorschlag —
/// Ablehnung — neuer Vorschlag" spielen: kein Budget, keine Kettenlänge und
/// (ohne `max_runtime`) keine Laufzeit hielt das auf, und der Leerlauf-Schutz
/// greift nicht, solange Events kommen. Bewusst weit über allem, was ein
/// arbeitender Schwarm braucht — beobachtet sind eine bis drei Runden.
pub const MAX_PROPOSALS_PER_AGENT: usize = 20;

/// Das Status-JSON eines Sendepfads. Am Limit kommt der Handlungshinweis dazu —
/// ein blankes `limit_erreicht` würde das Modell sonst zum Aufgeben verleiten.
fn zustell_status(result: DeliveryResult, nachricht_id: &str, empfaenger: &str) -> String {
    let mut out = json!({
        "status": result.status_str(),
        "nachricht_id": nachricht_id,
        "empfaenger": empfaenger,
        "retryable": result.retryable(),
    });
    if result == DeliveryResult::LimitReached {
        out["hinweis"] = json!(LIMIT_HINWEIS);
    }
    out.to_string()
}

/// Registriert alle sechs Schwarm-Tools in der (öffentlich mutierbaren)
/// Registry eines bereits gebauten Agenten. Läuft in Phase 1 des Starts,
/// bevor der Agent in seinen Actor-Thread wandert.
pub(crate) fn register_swarm_tools(tools: &mut ToolRegistry, ctx: Arc<SwarmToolContext>) {
    let kind_schema = json!({
        "type": "string",
        "enum": ["task", "request", "information", "observation", "critique"],
        "description": "Art der Nachricht (Standard: information)."
    });

    // ------------------------------------------------------------ swarm_peers
    let c = ctx.clone();
    tools.add_typed(
        "swarm_peers",
        "Listet die Schwarm-Agenten auf, mit denen du direkt kommunizieren kannst.",
        json!({"type": "object", "properties": {}}),
        move |_: serde_json::Value| json!({ "peers": sorted_peer_ids(&c.peers) }).to_string(),
    );

    // ------------------------------------------------------------- swarm_send
    let c = ctx.clone();
    tools.add_typed(
        "swarm_send",
        "Sendet eine Nachricht an einen benachbarten Schwarm-Agenten (fire-and-forget). \
         Die Antwort des Empfängers trifft später als neue Nachricht bei dir ein.",
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "ID des Empfänger-Agenten (siehe swarm_peers)."},
                "content": {"type": "string", "description": "Inhalt der Nachricht."},
                "kind": kind_schema,
                "correlation_id": {"type": "string", "description": "Optionaler fachlicher Faden."}
            },
            "required": ["to", "content"]
        }),
        move |args: SendArgs| {
            let hop = c.next_hop_count();
            if hop > c.max_hops {
                return json!({"status": "max_hops_erreicht", "empfaenger": args.to}).to_string();
            }
            let Some(target) = c.peers.get(&args.to) else {
                return json!({
                    "status": DeliveryResult::NotAllowed.status_str(),
                    "empfaenger": args.to,
                    "erreichbar": sorted_peer_ids(&c.peers),
                })
                .to_string();
            };
            let message = c.new_message(
                Recipient::Agent(args.to.clone()),
                args.kind.unwrap_or(MessageKind::Information),
                args.content,
                None,
                args.correlation_id,
                hop,
            );
            let id = message.id.clone();
            let result = c.deliver(target, message);
            zustell_status(result, &id, &args.to)
        },
    );

    // ------------------------------------------------------------ swarm_reply
    let c = ctx.clone();
    tools.add_typed(
        "swarm_reply",
        "Antwortet dem Absender der Nachricht, die du gerade bearbeitest (fire-and-forget).",
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Inhalt der Antwort."},
                "kind": {"type": "string", "enum": ["reply", "information", "observation", "critique"],
                          "description": "Art der Antwort (Standard: reply)."}
            },
            "required": ["content"]
        }),
        move |args: ReplyArgs| {
            let Some(incoming) = c.current.lock().unwrap().clone() else {
                return json!({"status": "keine_aktuelle_nachricht"}).to_string();
            };
            let hop = incoming.hop_count + 1;
            if hop > c.max_hops {
                return json!({"status": "max_hops_erreicht", "empfaenger": incoming.from})
                    .to_string();
            }
            // Die Initialaufgabe kommt von der Laufzeit, nicht von einem Agenten —
            // es gibt buchstäblich niemanden, dem man antworten könnte. Ein blankes
            // `nicht_erlaubt` wäre hier irreführend: das Mitglied hat nichts falsch
            // gemacht, es braucht nur den anderen Weg (Nachbarn bzw. Abschluss).
            if incoming.from == crate::RUNTIME_SENDER {
                return json!({
                    "status": "initialaufgabe_ohne_absender",
                    "hinweis": "Diese Nachricht ist die Initialaufgabe des Schwarms und kommt \
                                von der Laufzeit — es gibt keinen Absender, dem du antworten \
                                könntest. Arbeite sie ab, verteile Teilaufgaben mit swarm_send \
                                bzw. swarm_broadcast an deine Nachbarn und schließe am Ende mit \
                                swarm_propose ab.",
                    "erreichbar": sorted_peer_ids(&c.peers),
                })
                .to_string();
            }
            let Some(target) = c.peers.get(&incoming.from) else {
                return json!({
                    "status": DeliveryResult::NotAllowed.status_str(),
                    "empfaenger": incoming.from,
                    "erreichbar": sorted_peer_ids(&c.peers),
                })
                .to_string();
            };
            let message = c.new_message(
                Recipient::Agent(incoming.from.clone()),
                args.kind.unwrap_or(MessageKind::Reply),
                args.content,
                Some(incoming.id.clone()),
                incoming.correlation_id.clone().or(Some(incoming.id.clone())),
                hop,
            );
            let id = message.id.clone();
            let result = c.deliver(target, message);
            zustell_status(result, &id, &incoming.from)
        },
    );

    // -------------------------------------------------------- swarm_broadcast
    let c = ctx.clone();
    tools.add_typed(
        "swarm_broadcast",
        "Sendet eine Nachricht an ALLE direkt erreichbaren Schwarm-Agenten (fire-and-forget). \
         Jede Zustellung zählt gegen das Nachrichtenlimit.",
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "Inhalt der Nachricht."},
                "kind": kind_schema,
            },
            "required": ["content"]
        }),
        move |args: BroadcastArgs| {
            let hop = c.next_hop_count();
            if hop > c.max_hops {
                return json!({"status": "max_hops_erreicht"}).to_string();
            }
            let kind = args.kind.unwrap_or(MessageKind::Information);
            let message = c.new_message(Recipient::Broadcast, kind, args.content, None, None, hop);
            let mut statuses = serde_json::Map::new();
            let mut am_limit = false;
            for peer_id in sorted_peer_ids(&c.peers) {
                let result = c.deliver(&c.peers[&peer_id], message.clone());
                am_limit |= result == DeliveryResult::LimitReached;
                statuses.insert(peer_id, json!(result.status_str()));
            }
            let mut out = json!({"nachricht_id": message.id, "zustellungen": statuses});
            if am_limit {
                out["hinweis"] = json!(LIMIT_HINWEIS);
            }
            out.to_string()
        },
    );

    // ---------------------------------------------------------- swarm_propose
    let c = ctx.clone();
    tools.add_typed(
        "swarm_propose",
        "Schlägt vor, den Schwarm-Auftrag als erledigt abzuschließen. Der Vorschlag geht an \
         alle erreichbaren Agenten; sie stimmen mit swarm_vote ab. Merke dir die vorschlag_id.",
        json!({
            "type": "object",
            "properties": {
                "proposal": {"type": "string", "description": "Der Abschluss-Vorschlag (z. B. das Ergebnis)."}
            },
            "required": ["proposal"]
        }),
        move |args: ProposeArgs| {
            if c.proposals_made.fetch_add(1, Ordering::SeqCst) >= MAX_PROPOSALS_PER_AGENT {
                return json!({
                    "status": "vorschlagslimit_erreicht",
                    "hinweis": format!(
                        "Du hast bereits {MAX_PROPOSALS_PER_AGENT} Abschluss-Vorschläge \
                         eingereicht — weitere werden nicht mehr angenommen. Prüfe die \
                         vorliegenden Vorschläge der anderen Mitglieder und stimme mit \
                         swarm_vote über einen davon ab."
                    ),
                })
                .to_string();
            }
            let message = c.new_message(
                Recipient::Broadcast,
                MessageKind::Proposal,
                args.proposal,
                None,
                None,
                0,
            );
            // Der CompletionActor bekommt das Original (budgetfrei — der
            // Abschluss darf nicht am erschöpften Limit scheitern) …
            if c.completion_tx.try_send(message.clone()).is_err() {
                return json!({"status": "completion_nicht_erreichbar"}).to_string();
            }
            c.swarm_bus.publish(crate::SwarmEvent::ProposalCreated {
                message: message.clone(),
            });
            // Den eigenen Vorschlag merken — daran erkennt `swarm_vote` eine
            // Stimme auf den eigenen Vorschlag.
            c.gesehene_vorschlaege
                .lock()
                .unwrap()
                .insert(message.id.clone(), c.self_id.clone());
            // … und die Peers ihre Kopien ebenfalls budgetfrei: nur wer einen
            // Vorschlag sieht, kann über ihn abstimmen. Würde das Nachrichten-
            // limit die Kopien schlucken, wäre der Abschluss genau am Limit
            // unmöglich — also dort, wo er am dringendsten gebraucht wird.
            let mut statuses = serde_json::Map::new();
            for peer_id in sorted_peer_ids(&c.peers) {
                let result = c.deliver_free(&c.peers[&peer_id], message.clone());
                statuses.insert(peer_id, json!(result.status_str()));
            }
            json!({
                "status": "eingereicht",
                "vorschlag_id": message.id,
                "zustellungen": statuses,
            })
            .to_string()
        },
    );

    // ------------------------------------------------------------- swarm_vote
    let c = ctx;
    tools.add_typed(
        "swarm_vote",
        "Stimmt über den Abschluss-Vorschlag eines ANDEREN Mitglieds ab (vorschlag_id aus der \
         Proposal-Nachricht). Über den eigenen Vorschlag kannst du nicht abstimmen.",
        json!({
            "type": "object",
            "properties": {
                "proposal_id": {"type": "string", "description": "ID des Vorschlags (msg-…)."},
                "approve": {"type": "boolean", "description": "true = Zustimmung, false = Ablehnung."},
                "comment": {"type": "string", "description": "Optionale Begründung."}
            },
            "required": ["proposal_id", "approve"]
        }),
        move |args: VoteArgs| {
            // Über den eigenen Vorschlag stimmt man nicht ab: der Vorschlagende
            // öffnete sonst das Quorum selbst, ohne dass irgendjemand geprüft
            // hat. Weicher Fehler MIT dem Weg, der stattdessen funktioniert —
            // ein blankes "nicht erlaubt" ließe das Modell ratlos zurück.
            let eigener = c
                .gesehene_vorschlaege
                .lock()
                .unwrap()
                .get(&args.proposal_id)
                .is_some_and(|urheber| urheber == &c.self_id);
            if eigener {
                return json!({
                    "status": "eigener_vorschlag",
                    "vorschlag_id": args.proposal_id,
                    "hinweis": "Das ist dein eigener Vorschlag — über ihn stimmen nur die \
                                anderen Mitglieder ab, deine Stimme zählt hier nicht. Liegt ein \
                                Vorschlag eines anderen Mitglieds vor, prüfe ihn und stimme mit \
                                swarm_vote über DESSEN vorschlag_id ab; sonst beende deinen Zug \
                                und warte auf die Stimmen der anderen.",
                })
                .to_string();
            }
            let content = json!({
                "zustimmung": args.approve,
                "kommentar": args.comment,
            })
            .to_string();
            let message = c.new_message(
                Recipient::Broadcast,
                MessageKind::Vote,
                content,
                None,
                Some(args.proposal_id.clone()),
                0,
            );
            // Votes gehen NUR an den CompletionActor und sind budgetfrei —
            // der Schwarm muss auch am Nachrichtenlimit noch abschließen können.
            if c.completion_tx.try_send(message.clone()).is_err() {
                return json!({"status": "completion_nicht_erreichbar"}).to_string();
            }
            c.swarm_bus
                .publish(crate::SwarmEvent::VoteSubmitted {
                    message: message.clone(),
                });

            // Eine ABLEHNUNG geht zusätzlich an den Urheber — sonst erfährt er
            // nie, dass und warum sein Vorschlag durchgefallen ist, und kann
            // nicht nachbessern. Der Vote selbst landet nur beim
            // CompletionActor; ohne diesen Rückkanal wäre die Protokollregel
            // "arbeite die Einwände ein" eine Anweisung zu etwas Unmöglichem.
            //
            // Nur Ablehnungen: eine Zustimmung erzeugt keine Arbeit, sie als
            // Nachricht zuzustellen wäre bloß Rauschen in fremden Kontexten.
            // Budgetfrei wie der Vote selbst — der Abschlusspfad muss auch am
            // erschöpften Limit funktionieren.
            // Die Status sind unterscheidbar, nicht bloß "hat nicht geklappt":
            // bei einer Zustimmung gibt es nichts zuzustellen, und ein Urheber
            // außer Reichweite ist etwas anderes als ein unbekannter. In einem
            // echten Lauf ging eine von zwei Ablehnungen so verloren — sichtbar
            // war das nur, weil der Status es benennt.
            let mut zurueck = json!("entfaellt");
            if !args.approve {
                let urheber = c
                    .gesehene_vorschlaege
                    .lock()
                    .unwrap()
                    .get(&args.proposal_id)
                    .cloned();
                zurueck = json!("urheber_unbekannt");
                if let Some(urheber) = urheber {
                    zurueck = json!("kein_nachbar");
                    if let Some(target) = c.peers.get(&urheber) {
                        let einwand = c.new_message(
                            Recipient::Agent(urheber.clone()),
                            MessageKind::Critique,
                            format!(
                                "Dein Vorschlag {} wurde von mir ABGELEHNT.\n\
                                 Begründung: {}\n\n\
                                 Arbeite die Einwände ein und reiche mit swarm_propose einen \
                                 überarbeiteten Vorschlag ein.",
                                args.proposal_id,
                                args.comment.as_deref().unwrap_or("(ohne Begründung)")
                            ),
                            None,
                            Some(args.proposal_id.clone()),
                            0,
                        );
                        zurueck = json!(c.deliver_free(target, einwand).status_str());
                    }
                }
            }
            json!({
                "status": "abgegeben",
                "vorschlag_id": args.proposal_id,
                "einwand_an_urheber": zurueck,
            })
            .to_string()
        },
    );
}
