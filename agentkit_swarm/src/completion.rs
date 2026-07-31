//! Abschluss des Schwarms — Policy, Ergebnis und der CompletionActor.
//!
//! Der CompletionActor interpretiert keine Inhalte, er verwaltet nur formale
//! Zustände (Proposals und Votes) und wertet eine deterministische Termination
//! Policy aus. Das ist keine Orchestrierung. Er ist bewusst ein einfacher
//! Thread mit `Receiver<SwarmMessage>` und kein `Agent` — es gibt kein LLM zu
//! befragen, und ein eigener `AgentCommand`-Wrapper wäre reine Symmetrie ohne
//! zweiten Nutzer.

use crate::events::{SwarmEvent, SwarmEventBus};
use crate::message::{AgentId, DeliveryResult, MessageKind, SwarmMessage};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wann gilt der Schwarm als fertig?
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionPolicy {
    /// Ein Proposal ist angenommen, sobald `required_approvals` zustimmende
    /// Votes eingegangen sind (der Vorschlagende zählt nicht automatisch mit).
    Consensus { required_approvals: usize },
}

/// Warum der Schwarm beendet wurde.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    /// Konsens erreicht: das angenommene Proposal samt Zustimmungszahl.
    Consensus {
        proposal: SwarmMessage,
        approvals: usize,
    },
    MessageLimitReached,
    MaxRuntimeReached,
    /// Der Schwarm hat eine Weile nichts mehr getan: nichts in Arbeit, keine
    /// Events. Das ist der HÄNGE-SCHUTZ an Stelle einer Gesamtlaufzeit.
    ///
    /// Verstummen die Mitglieder, ohne vorzuschlagen, greift sonst kein
    /// Abschluss — kein Konsens, kein erschöpftes Budget — und `join()` wartet
    /// ewig. Bewusst LEERLAUF und nicht Gesamtzeit: ein Schwarm, der stundenlang
    /// produktiv arbeitet, soll weiterlaufen dürfen; abgebrochen wird nur, wer
    /// nichts mehr tut.
    Idle,
    /// Ein Actor-Thread ist gestorben (Panic) — MVP-Policy: der ganze Schwarm
    /// wird kontrolliert gestoppt.
    ActorFailure {
        agent: AgentId,
        error: String,
    },
    /// Extern über [`crate::SwarmHandle::stop`] beendet.
    Stopped,
}

/// Unzustellbare oder verworfene Nachricht (abgelehnte Sends, Mailbox-Drain
/// beim Shutdown, Votes auf unbekannte Proposals).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DeadLetter {
    pub message: SwarmMessage,
    pub reason: DeliveryResult,
}

/// Ein eingereichter Abschluss-Vorschlag und wie über ihn abgestimmt wurde.
///
/// Ohne diese Aufstellung ist ein Lauf nicht nachvollziehbar: in einem echten
/// Lauf reichten ZWEI Mitglieder konkurrierende Vorschläge ein, ein drittes
/// arbeitete am meisten und stimmte nie ab — im Ergebnis stand davon nichts,
/// nur der Text des Gewinners.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ProposalOutcome {
    pub id: String,
    pub from: AgentId,
    /// Wer zugestimmt hat (ohne den Vorschlagenden — der zählt nie mit).
    pub approvals: Vec<AgentId>,
    /// Hat dieser Vorschlag den Schwarm beendet?
    pub accepted: bool,
}

/// Endergebnis eines Schwarm-Laufs.
///
/// `Serialize`, damit der Lauf am Ende als `swarm_result`-Datensatz in den
/// Trace geht. `dynamic::render_result` bleibt daneben bestehen: das ist der
/// deutsche Text FÜRS MODELL, dies die vollständige Struktur für den Betrachter.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SwarmResult {
    pub reason: CompletionReason,
    /// Verbrauchte Nachrichten-Zustellungen (gegen `max_messages` gezählt).
    pub messages_sent: usize,
    pub dead_letters: Vec<DeadLetter>,
    /// Verarbeitete Turns je Agent.
    pub turns: HashMap<AgentId, usize>,
    /// ALLE eingereichten Vorschläge samt Zustimmungen — auch die abgelehnten
    /// und die, über die nie abgestimmt wurde.
    pub proposals: Vec<ProposalOutcome>,
    /// Wie viele Zustimmungen ein Vorschlag brauchte (die Completion Policy).
    pub required_approvals: usize,
}

/// Globales Nachrichtenbudget (`max_messages`) — geteilt über alle Agenten,
/// damit kein zentraler Router im Nachrichtenpfad nötig ist.
pub(crate) struct MessageBudget {
    count: AtomicUsize,
    max: usize,
    exhausted: AtomicBool,
}

impl MessageBudget {
    pub fn new(max: usize) -> Self {
        MessageBudget {
            count: AtomicUsize::new(0),
            max,
            exhausted: AtomicBool::new(false),
        }
    }

    /// Eine Zustellung verbrauchen; `false` = Limit erschöpft. CAS-Schleife
    /// statt `fetch_add`, damit ein abgewiesener Versuch keinen Zählerstand
    /// hinterlässt — der Zähler zählt akzeptierte Zustellungen, nicht Versuche.
    pub fn try_consume(&self) -> bool {
        let mut current = self.count.load(Ordering::SeqCst);
        loop {
            if current >= self.max {
                return false;
            }
            match self.count.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Verbrauch einer FEHLgeschlagenen Zustellung zurückgeben: das Budget
    /// zählt erfolgreiche Zustellungen — ein retrybarer `postfach_voll`-Send
    /// darf `max_messages` nicht aufzehren. Nur nach erfolgreichem
    /// [`try_consume`] aufrufen.
    pub fn refund(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }

    /// Vermerkt, dass eine Zustellung am Limit gescheitert ist.
    pub fn mark_exhausted(&self) {
        self.exhausted.store(true, Ordering::SeqCst);
    }

    /// Ist mindestens eine Zustellung am Limit gescheitert?
    ///
    /// Das Limit ist eine BREMSE, kein Not-Aus: der Monitor beendet den Schwarm
    /// daraufhin nicht sofort, sondern erst, wenn die bereits zugestellte Arbeit
    /// abgearbeitet ist (siehe [`crate::SwarmHandle::join`]). `swarm_propose` und
    /// `swarm_vote` sind budgetfrei — der Schwarm kann also auch am erschöpften
    /// Limit noch per Konsens abschließen, statt seine Arbeit wegzuwerfen.
    pub fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::SeqCst)
    }

    /// Erfolgreich verbrauchte Zustellungen.
    pub fn used(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// Der CompletionActor-Loop: sammelt Proposals und Votes, publiziert bei
/// erreichtem Quorum `SwarmCompleted` und beendet sich. `recv_timeout` statt
/// `recv`, damit das Stop-Flag auch ohne weitere Nachrichten greift.
/// Was der CompletionActor von der Laufzeit braucht — als Struct, weil clippy
/// bei sieben Parametern die Grenze zieht (dasselbe Muster wie
/// `SwarmToolConfig`/`TaskToolConfig`).
pub(crate) struct CompletionSetup {
    pub policy: CompletionPolicy,
    pub stop: Arc<AtomicBool>,
    pub swarm_bus: SwarmEventBus,
    pub dead_letters: Arc<Mutex<Vec<DeadLetter>>>,
    pub protokoll: Arc<Mutex<Vec<ProposalOutcome>>>,
    /// Abstimmungsfrist nach Erreichen des Quorums.
    pub vote_window: Duration,
    /// Mitgliederzahl — Grundlage für „alle außer dem Vorschlagenden haben
    /// geantwortet", das die Frist vorzeitig beendet.
    pub member_count: usize,
}

pub(crate) fn completion_loop(rx: Receiver<SwarmMessage>, setup: CompletionSetup) {
    let CompletionSetup {
        policy,
        stop,
        swarm_bus,
        dead_letters,
        protokoll,
        vote_window,
        member_count,
    } = setup;
    let CompletionPolicy::Consensus { required_approvals } = policy;
    // Proposal-ID -> (Proposal, Anzahl Zustimmungen). Doppelte Votes desselben
    // Agenten pro Proposal zählen nicht doppelt.
    let mut proposals: HashMap<String, (SwarmMessage, Vec<AgentId>)> = HashMap::new();
    // Läuft, sobald ein Vorschlag das Quorum erreicht hat.
    let mut frist: Option<Instant> = None;
    // Wer überhaupt schon abgestimmt hat (über alle Vorschläge hinweg).
    let mut waehler: HashSet<AgentId> = HashSet::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Empfangen und Entscheiden sind getrennt: die Frist muss auch dann
        // reifen, wenn KEINE Nachricht mehr kommt.
        let eingang = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(msg) = eingang {
            match msg.kind {
                MessageKind::Proposal => {
                    // Quorum 0 ist ohne Votes erfüllt — das Proposal schließt den
                    // Schwarm sofort ab (sonst würde `join()` endlos warten).
                    if required_approvals == 0 {
                        protokoll.lock().unwrap().push(ProposalOutcome {
                            id: msg.id.clone(),
                            from: msg.from.clone(),
                            approvals: Vec::new(),
                            accepted: true,
                        });
                        swarm_bus.publish(SwarmEvent::SwarmCompleted {
                            reason: CompletionReason::Consensus {
                                proposal: msg,
                                approvals: 0,
                            },
                        });
                        return;
                    }
                    protokoll.lock().unwrap().push(ProposalOutcome {
                        id: msg.id.clone(),
                        from: msg.from.clone(),
                        approvals: Vec::new(),
                        accepted: false,
                    });
                    proposals.insert(msg.id.clone(), (msg, Vec::new()));
                }
                MessageKind::Vote => {
                    let Some(entry) = msg
                        .correlation_id
                        .as_ref()
                        .and_then(|pid| proposals.get_mut(pid))
                    else {
                        // Vote auf unbekanntes Proposal -> Dead Letter + Event.
                        swarm_bus.publish(SwarmEvent::MessageRejected {
                            message: msg.clone(),
                            result: DeliveryResult::NotAllowed,
                        });
                        dead_letters.lock().unwrap().push(DeadLetter {
                            message: msg,
                            reason: DeliveryResult::NotAllowed,
                        });
                        continue;
                    };
                    let approve = serde_json::from_str::<serde_json::Value>(&msg.content)
                        .ok()
                        .and_then(|v| v["zustimmung"].as_bool())
                        .unwrap_or(false);
                    if approve && !entry.1.contains(&msg.from) {
                        entry.1.push(msg.from.clone());
                        if let Some(p) = protokoll
                            .lock()
                            .unwrap()
                            .iter_mut()
                            .find(|p| Some(&p.id) == msg.correlation_id.as_ref())
                        {
                            p.approvals.push(msg.from.clone());
                        }
                    }
                    waehler.insert(msg.from.clone());
                    // Quorum erreicht: NICHT sofort abschließen, sondern die
                    // Abstimmungsfrist starten (siehe `vote_window`).
                    if entry.1.len() >= required_approvals && frist.is_none() {
                        frist = Some(Instant::now() + vote_window);
                    }
                }
                // Andere Arten landen hier nicht (Tools senden nur Proposal/Vote
                // an den CompletionActor) — defensiv ignorieren.
                _ => {}
            }
        }

        // Entscheiden, sobald die Frist abgelaufen ist ODER alle außer dem
        // Vorschlagenden geantwortet haben — dann gibt es nichts mehr zu warten.
        let alle_da = waehler.len() + 1 >= member_count;
        if frist.is_some_and(|f| alle_da || Instant::now() >= f) {
            let Some((_, sieger)) = proposals
                .iter()
                // Der bestunterstützte Vorschlag gewinnt, nicht der schnellste.
                // Bei mehreren Rivalen ist das der Unterschied zwischen "wer
                // zuerst eine Stimme bekam" und "worauf sich der Schwarm einigt".
                .max_by_key(|(_, (_, zustimmungen))| zustimmungen.len())
            else {
                continue;
            };
            if let Some(p) = protokoll
                .lock()
                .unwrap()
                .iter_mut()
                .find(|p| p.id == sieger.0.id)
            {
                p.accepted = true;
            }
            swarm_bus.publish(SwarmEvent::SwarmCompleted {
                reason: CompletionReason::Consensus {
                    proposal: sieger.0.clone(),
                    approvals: sieger.1.len(),
                },
            });
            return;
        }
    }
}
