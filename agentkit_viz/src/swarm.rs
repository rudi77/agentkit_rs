//! Die Schwarm-Sicht: wer hat wem was geschickt, worüber wurde abgestimmt,
//! was ging verloren.
//!
//! Alles davon steckt schon im Ereignisstrom — `agentkit_swarm` legt jedes
//! [`SwarmEvent`](https://docs.rs/agentkit-swarm) verlustfrei als
//! `structured`-Datensatz mit `kind = "swarm_event"` auf den Bus, und am Ende
//! ein `swarm_result`. Dieses Modul deutet die Nutzlast, ohne `agentkit_swarm`
//! zu kennen: es liest JSON-Felder, keine Typen (dieses Crate hängt bewusst an
//! keiner Agent-Bibliothek, siehe README).
//!
//! Die Deutung ist deshalb TOLERANT: fehlt ein Feld, bleibt es leer, statt den
//! ganzen Datensatz zu verwerfen. Ein Schwarm-Lauf, der wegen eines
//! umbenannten Feldes gar nicht mehr angezeigt wird, wäre schlechter als einer
//! mit einer leeren Spalte.

use serde::Serialize;
use serde_json::Value;

use crate::model::TraceEvent;
use crate::project::TraceState;

/// `kind` der Schwarm-Datensätze (`agentkit_swarm::SWARM_EVENT_KIND`/
/// `SWARM_RESULT_KIND`) — ein Vertrag über die Datei, kein geteilter Typ.
pub const SWARM_EVENT_KIND: &str = "swarm_event";
pub const SWARM_RESULT_KIND: &str = "swarm_result";

/// Anzeigename für einen Broadcast-Empfänger.
pub const BROADCAST: &str = "(alle)";

/// Eine Nachricht zwischen Schwarm-Mitgliedern, wie das Sequenzdiagramm sie braucht.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmMessageView {
    pub at_ms: u64,
    pub id: String,
    /// Welcher Schwarm — ein Orchestrator kann das `swarm`-Tool mehrfach
    /// aufrufen, dann stehen mehrere Läufe in EINEM Trace.
    pub swarm_id: String,
    pub from: String,
    /// Empfänger-ID oder [`BROADCAST`].
    pub to: String,
    pub kind: String,
    pub content: String,
    pub reply_to: Option<String>,
    pub correlation_id: Option<String>,
    /// false ⇔ die Zustellung wurde abgelehnt (`reason` sagt warum).
    pub delivered: bool,
    pub reason: Option<String>,
}

/// Ein Abschluss-Vorschlag samt Abstimmung.
#[derive(Debug, Clone, Serialize)]
pub struct ProposalView {
    pub id: String,
    pub from: String,
    pub content: String,
    /// Wer zugestimmt hat (der Vorschlagende zählt nie mit).
    pub approvals: Vec<String>,
    /// Wer abgelehnt hat — steht in keinem Ergebnis, ist aber genau das, was
    /// man beim Debuggen sucht: warum kam kein Konsens zustande.
    pub rejections: Vec<String>,
    /// Hat dieser Vorschlag den Schwarm beendet? (Erst mit dem Ergebnis bekannt.)
    pub accepted: bool,
}

/// Eine unzustellbare Nachricht.
#[derive(Debug, Clone, Serialize)]
pub struct DeadLetterView {
    /// Die Message-ID — auch der Schlüssel gegen Doppelungen: zwei Nachrichten
    /// mit gleichem Absender und Inhalt sind zwei Verluste, nicht einer.
    pub id: String,
    pub at_ms: u64,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub reason: String,
    pub content: String,
}

/// Alles, was der Schwarm-Reiter zeigt.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SwarmView {
    /// Die Spalten des Sequenzdiagramms, in der Reihenfolge des ersten Auftretens.
    pub members: Vec<String>,
    pub messages: Vec<SwarmMessageView>,
    pub proposals: Vec<ProposalView>,
    pub dead_letters: Vec<DeadLetterView>,
    /// Die rohen `swarm_result`-Datensätze — EINER JE LAUF. Ein Orchestrator
    /// kann das `swarm`-Tool mehrfach aufrufen; nur den letzten zu behalten
    /// hieße, die Abstimmung aller früheren Läufe als „offen" anzuzeigen.
    pub results: Vec<Value>,
}

impl TraceState {
    /// Baut die Schwarm-Sicht aus dem Ereignisstrom.
    pub fn swarm(&self) -> SwarmView {
        let mut view = SwarmView::default();
        // Stimmen, deren Vorschlag (noch) nicht bekannt ist. Das passiert, wenn
        // der Trace vor dem `proposal_created` anfaengt — dann kennt erst das
        // Ergebnis den Vorschlag, und die Stimmen waeren sonst verloren.
        let mut offene_stimmen: Vec<Stimme> = Vec::new();
        for ev in self.events() {
            if let Some(payload) = ev.structured_of(SWARM_RESULT_KIND) {
                // JEDES Ergebnis anwenden, nicht nur das letzte: ein
                // Orchestrator kann das `swarm`-Tool mehrfach aufrufen.
                uebernimm_ergebnis(payload, &mut view);
                trage_stimmen_nach(&mut offene_stimmen, &mut view);
                view.results.push(payload.clone());
                continue;
            }
            let Some(payload) = ev.structured_of(SWARM_EVENT_KIND) else {
                continue;
            };
            deute(ev, payload, &mut view, &mut offene_stimmen);
        }
        trage_stimmen_nach(&mut offene_stimmen, &mut view);
        view
    }
}

/// Eine Stimme, deren Vorschlag noch nicht bekannt ist.
struct Stimme {
    proposal: String,
    waehler: String,
    zustimmung: bool,
}

/// Ein einzelnes Schwarm-Ereignis einsortieren.
fn deute(ev: &TraceEvent, payload: &Value, view: &mut SwarmView, offen: &mut Vec<Stimme>) {
    // Externally tagged: genau ein Schlüssel benennt die Variante.
    let Some((art, inhalt)) = payload.as_object().and_then(|o| o.iter().next()) else {
        return;
    };
    match art.as_str() {
        "message_queued" => {
            if let Some(m) = nachricht(ev, &inhalt["message"], true, None) {
                merke_mitglieder(view, &m);
                view.messages.push(m);
            }
        }
        "message_rejected" => {
            let abgelehnt_weil = grund(&inhalt["result"]);
            if let Some(m) = nachricht(ev, &inhalt["message"], false, Some(abgelehnt_weil.clone()))
            {
                merke_mitglieder(view, &m);
                // `swarm_completed` ist KEIN Verlust: ein Mitglied, das seinen
                // Turn zu Ende bringt, waehrend der Konsens schon steht, hat
                // nichts falsch gemacht — die Laufzeit legt dafuer bewusst
                // keinen Dead Letter an (`agent_actor.rs`). Es als unzustellbar
                // zu zeigen liesse einen sauberen Abschluss wie eine Panne
                // aussehen. Im Diagramm bleibt die Nachricht: sie ist echter Verkehr.
                if abgelehnt_weil != "swarm_completed" {
                    view.dead_letters.push(DeadLetterView {
                        id: m.id.clone(),
                        at_ms: m.at_ms,
                        from: m.from.clone(),
                        to: m.to.clone(),
                        kind: m.kind.clone(),
                        reason: abgelehnt_weil,
                        content: m.content.clone(),
                    });
                }
                view.messages.push(m);
            }
        }
        "proposal_created" => {
            let m = &inhalt["message"];
            let from = text(&m["from"]);
            merke(view, &from);
            view.proposals.push(ProposalView {
                id: text(&m["id"]),
                from,
                content: text(&m["content"]),
                approvals: Vec::new(),
                rejections: Vec::new(),
                accepted: false,
            });
        }
        "vote_submitted" => {
            let m = &inhalt["message"];
            let waehler = text(&m["from"]);
            merke(view, &waehler);
            let ziel = m["correlation_id"].as_str().unwrap_or_default().to_string();
            // Der Inhalt ist das JSON, das `swarm_vote` baut: {"zustimmung": …}.
            let zustimmung = serde_json::from_str::<Value>(m["content"].as_str().unwrap_or(""))
                .ok()
                .and_then(|v| v["zustimmung"].as_bool())
                .unwrap_or(false);
            let stimme = Stimme {
                proposal: ziel,
                waehler,
                zustimmung,
            };
            if !trage_ein(view, &stimme) {
                offen.push(stimme);
            }
        }
        // Lifecycle: nur für die Spalten interessant — ein Mitglied, das nie
        // gesendet hat, soll trotzdem im Diagramm stehen.
        "actor_started" | "actor_stopped" | "turn_started" | "turn_completed"
        | "message_dequeued" | "actor_failed" => merke(view, &text(&inhalt["agent"])),
        _ => {}
    }
}

/// Ergebnis-Felder nachtragen: welcher Vorschlag angenommen wurde und die
/// endgültige Liste der Dead Letters (die Laufzeit kennt auch welche, die nie
/// als Ereignis auftauchten — etwa beim Leeren der Mailboxen im Shutdown).
fn uebernimm_ergebnis(result: &Value, view: &mut SwarmView) {
    for p in result["proposals"].as_array().unwrap_or(&Vec::new()) {
        let id = text(&p["id"]);
        let angenommen = p["accepted"].as_bool().unwrap_or(false);
        let zustimmungen: Vec<String> = p["approvals"]
            .as_array()
            .map(|a| a.iter().map(text).collect())
            .unwrap_or_default();
        match view.proposals.iter_mut().find(|v| v.id == id) {
            Some(vorhanden) => {
                vorhanden.accepted = angenommen;
                for z in zustimmungen {
                    if !vorhanden.approvals.contains(&z) {
                        vorhanden.approvals.push(z);
                    }
                }
            }
            // Der Vorschlag steht im Ergebnis, aber sein Ereignis fehlt im
            // Trace (etwa weil der Betrachter erst später angehängt hat).
            None => view.proposals.push(ProposalView {
                id,
                from: text(&p["from"]),
                content: String::new(),
                approvals: zustimmungen,
                rejections: Vec::new(),
                accepted: angenommen,
            }),
        }
    }
    for dl in result["dead_letters"].as_array().unwrap_or(&Vec::new()) {
        let m = &dl["message"];
        let eintrag = DeadLetterView {
            id: text(&m["id"]),
            at_ms: m["created_at"].as_u64().unwrap_or(0),
            from: text(&m["from"]),
            to: empfaenger(&m["to"]),
            kind: text(&m["kind"]),
            reason: grund(&dl["reason"]),
            content: text(&m["content"]),
        };
        // Die abgelehnten Sends stehen schon als Ereignis da — nur ergänzen,
        // was der Ereignisstrom nicht hatte.
        // Ueber die Message-ID: zwei Nachrichten mit gleichem Absender und
        // Inhalt sind zwei Verluste, nicht einer.
        if !view.dead_letters.iter().any(|d| d.id == eintrag.id) {
            view.dead_letters.push(eintrag);
        }
    }
}

fn nachricht(
    ev: &TraceEvent,
    m: &Value,
    delivered: bool,
    reason: Option<String>,
) -> Option<SwarmMessageView> {
    let from = m["from"].as_str()?.to_string();
    Some(SwarmMessageView {
        at_ms: ev.at_ms,
        id: text(&m["id"]),
        swarm_id: text(&m["swarm_id"]),
        from,
        to: empfaenger(&m["to"]),
        kind: text(&m["kind"]),
        content: text(&m["content"]),
        reply_to: m["reply_to"].as_str().map(str::to_string),
        correlation_id: m["correlation_id"].as_str().map(str::to_string),
        delivered,
        reason,
    })
}

/// `Recipient`: `{"agent": "a1"}` oder `"broadcast"`.
fn empfaenger(to: &Value) -> String {
    match to.get("agent").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => BROADCAST.to_string(),
    }
}

/// `DeliveryResult` als Text (`"mailbox_full"`, …).
fn grund(v: &Value) -> String {
    v.as_str().unwrap_or("unbekannt").to_string()
}

/// Eine Stimme bei ihrem Vorschlag eintragen. `false` = Vorschlag unbekannt.
fn trage_ein(view: &mut SwarmView, stimme: &Stimme) -> bool {
    let Some(p) = view.proposals.iter_mut().find(|p| p.id == stimme.proposal) else {
        return false;
    };
    let liste = if stimme.zustimmung {
        &mut p.approvals
    } else {
        &mut p.rejections
    };
    if !liste.contains(&stimme.waehler) {
        liste.push(stimme.waehler.clone());
    }
    true
}

/// Gepufferte Stimmen nachtragen, sobald ihr Vorschlag bekannt ist.
fn trage_stimmen_nach(offen: &mut Vec<Stimme>, view: &mut SwarmView) {
    offen.retain(|stimme| !trage_ein(view, stimme));
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn merke(view: &mut SwarmView, id: &str) {
    if !id.is_empty() && id != BROADCAST && !view.members.iter().any(|m| m == id) {
        view.members.push(id.to_string());
    }
}

fn merke_mitglieder(view: &mut SwarmView, m: &SwarmMessageView) {
    merke(view, &m.from);
    merke(view, &m.to);
}
