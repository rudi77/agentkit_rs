//! Die Schwarm-Laufzeit — Builder, Zwei-Phasen-Start, Monitor und Shutdown.
//!
//! Die Laufzeit startet/stoppt Actors, verteilt Peer-Referenzen, überwacht
//! Limits und wertet die Completion Policy aus. Sie orchestriert NICHT: kein
//! zentraler Router im Nachrichtenpfad, keine Interpretation von Antworten,
//! keine Auswahl des "nächsten" Agenten — mit wem kommuniziert wird,
//! entscheiden die Agenten selbst über ihre Tools.

use crate::actor_ref::AgentActorRef;
use crate::agent_actor::{actor_loop, SwarmToolContext};
use crate::completion::{
    completion_loop, CompletionPolicy, CompletionReason, DeadLetter, MessageBudget, SwarmResult,
};
use crate::directory::PeerDirectory;
use crate::error::SwarmError;
use crate::events::{SwarmEvent, SwarmEventBus};
use crate::mailbox::{mailbox, DEFAULT_MAILBOX_CAPACITY};
use crate::message::{
    now_ms, AgentCommand, AgentId, MessageKind, Recipient, SwarmMessage, RUNTIME_SENDER,
};
use crate::supervisor::{find_failed_actor, panic_message};
use crate::tools::register_swarm_tools;
use agentkit::{new_cancel, Agent, Cancel, EventBus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_MESSAGES: usize = 100;
pub const DEFAULT_MAX_HOPS: u32 = 8;

/// Baut einen Schwarm aus fertigen [`agentkit::Agent`]s und einer Topologie.
/// Mit `.agent(id, agent)` geht der Agent in den Besitz der Laufzeit über
/// (Move-Semantik) — ab dem Start besitzt ihn exklusiv sein Actor-Thread.
pub struct SwarmBuilder {
    name: String,
    agents: Vec<(AgentId, Agent)>,
    topology: crate::Topology,
    policy: CompletionPolicy,
    mailbox_capacity: usize,
    max_messages: usize,
    max_hops: u32,
    max_runtime: Option<Duration>,
    agent_bus: Option<EventBus>,
}

impl SwarmBuilder {
    pub fn new(name: &str) -> Self {
        SwarmBuilder {
            name: name.to_string(),
            agents: Vec::new(),
            topology: crate::Topology::new(),
            policy: CompletionPolicy::Consensus {
                required_approvals: 1,
            },
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            max_messages: DEFAULT_MAX_MESSAGES,
            max_hops: DEFAULT_MAX_HOPS,
            max_runtime: None,
            agent_bus: None,
        }
    }

    pub fn agent(mut self, id: &str, agent: Agent) -> Self {
        self.agents.push((id.to_string(), agent));
        self
    }

    pub fn connect_bidirectional(mut self, a: &str, b: &str) -> Self {
        self.topology.connect_bidirectional(a, b);
        self
    }

    pub fn completion(mut self, policy: CompletionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity.max(1);
        self
    }

    /// Globales Limit über alle Nachrichten-Zustellungen des Schwarms.
    pub fn max_messages(mut self, max: usize) -> Self {
        self.max_messages = max;
        self
    }

    /// Maximale Länge einer Konversationskette (Reply-/Send-Hops).
    pub fn max_hops(mut self, max: u32) -> Self {
        self.max_hops = max;
        self
    }

    /// Harte Laufzeitgrenze; danach endet `join()` mit `MaxRuntimeReached`.
    pub fn max_runtime(mut self, limit: Duration) -> Self {
        self.max_runtime = Some(limit);
        self
    }

    /// Gemeinsamer Bus für die [`agentkit::AgentEvent`]s aller Agenten
    /// (Events sind mit `source` = Agent-ID getaggt). Ohne Angabe wird ein
    /// frischer Bus angelegt.
    pub fn agent_bus(mut self, bus: EventBus) -> Self {
        self.agent_bus = Some(bus);
        self
    }

    pub fn build(self) -> Result<Swarm, SwarmError> {
        if self.agents.is_empty() {
            return Err(SwarmError::EmptySwarm);
        }
        let ids: Vec<AgentId> = self.agents.iter().map(|(id, _)| id.clone()).collect();
        for (i, id) in ids.iter().enumerate() {
            if ids[i + 1..].contains(id) {
                return Err(SwarmError::DuplicateAgent(id.clone()));
            }
        }
        self.topology.validate(&ids)?;
        Ok(Swarm { builder: self })
    }
}

/// Ein validierter, noch nicht gestarteter Schwarm.
pub struct Swarm {
    builder: SwarmBuilder,
}

impl Swarm {
    /// Startet den Schwarm in zwei Phasen:
    ///
    /// 1. Alle Mailboxes und [`AgentActorRef`]s anlegen, dann je Agent die
    ///    erlaubten Peer-Refs verteilen und die Schwarm-Tools in seine
    ///    Registry injizieren (die Agenten liegen noch im Builder-Vec).
    /// 2. Jeden Agenten per Move in seinen Actor-Thread geben; zuletzt den
    ///    CompletionActor starten.
    ///
    /// Weil jede Mailbox vor jedem Thread-Start existiert, kann keine
    /// Nachricht durch Start-Reihenfolge verloren gehen.
    pub fn start(self) -> Result<SwarmHandle, SwarmError> {
        let SwarmBuilder {
            name,
            mut agents,
            topology,
            policy,
            mailbox_capacity,
            max_messages,
            max_hops,
            max_runtime,
            agent_bus,
        } = self.builder;

        let swarm_bus = SwarmEventBus::new();
        // Vor jedem Spawn abonnieren — der Monitor in `join()` verpasst nichts.
        let events_rx = swarm_bus.subscribe();
        let agent_bus = agent_bus.unwrap_or_default();
        let stop = Arc::new(AtomicBool::new(false));
        let budget = Arc::new(MessageBudget::new(max_messages));
        let dead_letters: Arc<Mutex<Vec<DeadLetter>>> = Arc::new(Mutex::new(Vec::new()));
        let msg_seq = Arc::new(AtomicU64::new(0));
        // task_ids für run_on_bus ab 1 — die -1 ist in agentkit der Marker
        // des Haupt-Agenten.
        let turn_seq = Arc::new(AtomicI64::new(1));
        let (completion_tx, completion_rx) = sync_channel::<SwarmMessage>(max_messages.max(16));

        // Phase 1: Mailboxes + Refs, dann Tools injizieren.
        let mut refs: HashMap<AgentId, AgentActorRef> = HashMap::new();
        let mut receivers: Vec<Receiver<AgentCommand>> = Vec::new();
        for (id, _) in &agents {
            let (tx, rx) = mailbox(mailbox_capacity);
            refs.insert(id.clone(), AgentActorRef::new(id.clone(), tx));
            receivers.push(rx);
        }
        let mut ctxs: Vec<Arc<SwarmToolContext>> = Vec::new();
        for (id, agent) in agents.iter_mut() {
            let peers: PeerDirectory = topology
                .peers_of(id)
                .into_iter()
                .filter_map(|peer| refs.get(&peer).map(|r| (peer, r.clone())))
                .collect();
            let ctx = Arc::new(SwarmToolContext {
                self_id: id.clone(),
                swarm_id: name.clone(),
                peers,
                completion_tx: completion_tx.clone(),
                swarm_bus: swarm_bus.clone(),
                cancel: new_cancel(),
                current: Mutex::new(None),
                msg_seq: msg_seq.clone(),
                msg_budget: budget.clone(),
                dead_letters: dead_letters.clone(),
                max_hops,
            });
            register_swarm_tools(&mut agent.tools, ctx.clone());
            ctxs.push(ctx);
        }

        // Phase 2: Actors starten (Move des Agenten in seinen Thread).
        let mut handles: Vec<(AgentId, JoinHandle<()>)> = Vec::new();
        for (((id, agent), rx), ctx) in agents.drain(..).zip(receivers).zip(ctxs.iter().cloned()) {
            let actor_stop = stop.clone();
            let agent_bus = agent_bus.clone();
            let turn_seq = turn_seq.clone();
            let handle = std::thread::Builder::new()
                .name(format!("swarm-{id}"))
                .spawn(move || actor_loop(agent, rx, ctx, actor_stop, agent_bus, turn_seq))
                .map_err(|e| {
                    // Bereits gestartete Actors kontrolliert beenden lassen.
                    stop.store(true, Ordering::SeqCst);
                    SwarmError::ActorUnavailable(format!("Thread-Start für '{id}': {e}"))
                })?;
            handles.push((id, handle));
        }
        let completion_handle = {
            let stop = stop.clone();
            let swarm_bus = swarm_bus.clone();
            let dead_letters = dead_letters.clone();
            std::thread::Builder::new()
                .name("swarm-completion".to_string())
                .spawn(move || {
                    completion_loop(completion_rx, policy, stop, swarm_bus, dead_letters)
                })
                .map_err(|e| SwarmError::ActorUnavailable(format!("CompletionActor: {e}")))?
        };

        Ok(SwarmHandle {
            swarm_id: name,
            refs,
            handles,
            completion_handle,
            stop,
            cancels: ctxs.iter().map(|c| c.cancel.clone()).collect(),
            swarm_bus,
            events_rx,
            budget,
            dead_letters,
            msg_seq,
            deadline: max_runtime.map(|d| Instant::now() + d),
        })
    }
}

/// Handle auf einen laufenden Schwarm: Initialaufgaben einspeisen, Events
/// beobachten, auf das Ende warten oder extern stoppen.
pub struct SwarmHandle {
    swarm_id: String,
    refs: HashMap<AgentId, AgentActorRef>,
    handles: Vec<(AgentId, JoinHandle<()>)>,
    completion_handle: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    cancels: Vec<Cancel>,
    swarm_bus: SwarmEventBus,
    events_rx: Receiver<SwarmEvent>,
    budget: Arc<MessageBudget>,
    dead_letters: Arc<Mutex<Vec<DeadLetter>>>,
    msg_seq: Arc<AtomicU64>,
    deadline: Option<Instant>,
}

impl SwarmHandle {
    /// Neuer Subscriber auf die [`SwarmEvent`]s — vor `send_initial` abonnieren,
    /// frühere Events sind für neue Subscriber nicht sichtbar.
    pub fn events(&self) -> Receiver<SwarmEvent> {
        self.swarm_bus.subscribe()
    }

    /// Speist die Initialaufgabe bei einem Agenten ein (Absender "runtime").
    /// Einziger blockierender Sendepfad des Schwarms: beim Kickoff ist die
    /// Mailbox leer, und die Zustellgarantie der Initialaufgabe ist wichtiger
    /// als ein einheitliches `try_send`. Gibt die Nachricht-ID zurück.
    pub fn send_initial(&self, agent: &str, task: &str) -> Result<String, SwarmError> {
        let target = self
            .refs
            .get(agent)
            .ok_or_else(|| SwarmError::UnknownAgent(agent.to_string()))?;
        // Zählt wie jede Zustellung gegen das Budget; die Zustellung selbst
        // ist trotzdem garantiert (Kickoff schlägt nicht am Limit fehl).
        let _ = self.budget.try_consume();
        let message = SwarmMessage {
            id: format!("msg-{}", self.msg_seq.fetch_add(1, Ordering::SeqCst) + 1),
            swarm_id: self.swarm_id.clone(),
            from: RUNTIME_SENDER.to_string(),
            to: Recipient::Agent(agent.to_string()),
            kind: MessageKind::Task,
            content: task.to_string(),
            reply_to: None,
            correlation_id: None,
            created_at: now_ms(),
            hop_count: 0,
        };
        let id = message.id.clone();
        target
            .send_blocking(message.clone())
            .map_err(|_| SwarmError::ActorUnavailable(agent.to_string()))?;
        self.swarm_bus
            .publish(SwarmEvent::MessageQueued { message });
        Ok(id)
    }

    /// Beendet den Schwarm sofort kontrolliert und liefert garantiert
    /// `reason: Stopped` — bewusst NICHT über die Event-Queue: dort könnte
    /// bereits ein früheres Abschluss-Event (z. B. Konsens) warten und das
    /// versprochene `Stopped` überholen.
    pub fn stop(self) -> SwarmResult {
        self.shutdown(Some(CompletionReason::Stopped), None, HashMap::new())
    }

    /// Wartet auf das Ende des Schwarms (Konsens, Limit, Laufzeit, Fehler)
    /// und fährt ihn dann kontrolliert herunter. Der Monitor läuft im
    /// Aufrufer-Thread — kein zusätzlicher Supervisor-Thread nötig.
    pub fn join(self) -> SwarmResult {
        self.monitor(None)
    }

    /// Wie [`join`](Self::join), endet zusätzlich mit [`CompletionReason::Stopped`],
    /// sobald `cancel` gesetzt wird.
    ///
    /// Gedacht für Aufrufer, die den Schwarm SELBST in einem laufenden Auftrag
    /// starten (das `swarm`-Tool, siehe [`crate::dynamic`]): dort blockiert
    /// `join()` im Tool-Thread des Agenten, und der Stop-Knopf des Nutzers muss
    /// trotzdem durchschlagen. Der Monitor prüft die Flagge im vorhandenen
    /// 100-ms-Takt — kein zusätzlicher Thread, kein zweiter Abbruchpfad.
    pub fn join_with_cancel(self, cancel: &Cancel) -> SwarmResult {
        self.monitor(Some(cancel))
    }

    /// Der gemeinsame Monitor-Loop von [`join`](Self::join) und
    /// [`join_with_cancel`](Self::join_with_cancel).
    fn monitor(self, cancel: Option<&Cancel>) -> SwarmResult {
        let mut turns: HashMap<AgentId, usize> = HashMap::new();
        let mut failed: Option<AgentId> = None;
        let reason = loop {
            match self.events_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(SwarmEvent::SwarmCompleted { reason }) => break Some(reason),
                Ok(SwarmEvent::TurnCompleted { agent, .. }) => {
                    *turns.entry(agent).or_insert(0) += 1;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    // Abbruch vor Laufzeit/Fehler prüfen: ein extern gestoppter
                    // Schwarm soll `Stopped` melden, nicht zufällig das Limit.
                    if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                        break Some(CompletionReason::Stopped);
                    }
                    if self.deadline.is_some_and(|d| Instant::now() >= d) {
                        break Some(CompletionReason::MaxRuntimeReached);
                    }
                    if let Some(agent) = find_failed_actor(&self.handles) {
                        failed = Some(agent);
                        break None;
                    }
                }
                // Unerreichbar (wir halten den Bus) — defensiv beenden.
                Err(RecvTimeoutError::Disconnected) => break Some(CompletionReason::Stopped),
            }
        };
        self.shutdown(reason, failed, turns)
    }

    /// Gemeinsames Herunterfahren für `join` und `stop`. `reason = None` heißt:
    /// der Monitor brach wegen eines toten Actors ab (`failed`), die
    /// Fehlerdetails liefert erst das Joinen der Threads.
    fn shutdown(
        mut self,
        reason: Option<CompletionReason>,
        failed: Option<AgentId>,
        mut turns: HashMap<AgentId, usize>,
    ) -> SwarmResult {
        // Shutdown-Sequenz, Reihenfolge bewusst:
        // 1. Stop-Flag — kein Actor beginnt einen neuen Turn.
        self.stop.store(true, Ordering::SeqCst);
        // 2. Laufende Turns kooperativ abbrechen.
        for cancel in &self.cancels {
            cancel.store(true, Ordering::SeqCst);
        }
        // 3. Best-Effort-Weckruf; volle Mailboxes deckt der recv_timeout-Takt ab.
        for actor_ref in self.refs.values() {
            actor_ref.try_command(AgentCommand::Shutdown);
        }
        // 4. Alle Threads joinen; Panics werden hier geerntet.
        let mut panics: HashMap<AgentId, String> = HashMap::new();
        for (id, handle) in self.handles.drain(..) {
            if let Err(payload) = handle.join() {
                let error = panic_message(payload);
                self.swarm_bus.publish(SwarmEvent::ActorFailed {
                    agent: id.clone(),
                    error: error.clone(),
                });
                panics.insert(id, error);
            }
        }
        let _ = self.completion_handle.join();

        // Nachzügler zwischen Abbruch und Join noch in die Turn-Statistik nehmen.
        while let Ok(event) = self.events_rx.try_recv() {
            if let SwarmEvent::TurnCompleted { agent, .. } = event {
                *turns.entry(agent).or_insert(0) += 1;
            }
        }

        let reason = match reason {
            Some(reason) => reason,
            None => {
                let agent = failed.expect("Monitor-Abbruch ohne Grund");
                let error = panics
                    .get(&agent)
                    .cloned()
                    .unwrap_or_else(|| "Actor-Thread unerwartet beendet".to_string());
                CompletionReason::ActorFailure { agent, error }
            }
        };
        SwarmResult {
            reason,
            messages_sent: self.budget.used(),
            dead_letters: std::mem::take(&mut *self.dead_letters.lock().unwrap()),
            turns,
        }
    }
}
