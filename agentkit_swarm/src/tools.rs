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
            json!({
                "status": result.status_str(),
                "nachricht_id": id,
                "empfaenger": args.to,
                "retryable": result.retryable(),
            })
            .to_string()
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
            let Some(target) = c.peers.get(&incoming.from) else {
                // z. B. die Initialaufgabe der Laufzeit ("runtime") — kein Peer.
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
            json!({
                "status": result.status_str(),
                "nachricht_id": id,
                "empfaenger": incoming.from,
                "retryable": result.retryable(),
            })
            .to_string()
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
            for peer_id in sorted_peer_ids(&c.peers) {
                let result = c.deliver(&c.peers[&peer_id], message.clone());
                statuses.insert(peer_id, json!(result.status_str()));
            }
            json!({"nachricht_id": message.id, "zustellungen": statuses}).to_string()
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
            // … die Peers bekommen Kopien über den normalen (budgetierten) Pfad.
            let mut statuses = serde_json::Map::new();
            for peer_id in sorted_peer_ids(&c.peers) {
                let result = c.deliver(&c.peers[&peer_id], message.clone());
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
        "Stimmt über einen Abschluss-Vorschlag ab (vorschlag_id aus der Proposal-Nachricht).",
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
                .publish(crate::SwarmEvent::VoteSubmitted { message });
            json!({"status": "abgegeben", "vorschlag_id": args.proposal_id}).to_string()
        },
    );
}
