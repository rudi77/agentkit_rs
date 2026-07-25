//! [`GraphAgent`] — ein normaler `agentkit::Agent` mit Recall davor und einer
//! Episode danach.
//!
//! Der Agent-Loop bleibt unangetastet: der Wrapper stellt den Graph-Ausschnitt
//! dem Auftragstext voran und ruft dann `run`/`run_on_bus` wie jeder andere
//! Aufrufer. Das ist bewusst die einfachste Naht — sie funktioniert mit und ohne
//! ctxman, in der CLI wie im Schwarm, und braucht keinen einzigen neuen
//! Erweiterungspunkt im Kern.
//!
//! Wer den Graphen lieber ausschließlich per Tool-Aufruf benutzt, registriert nur
//! [`crate::register_graph_tools`] und lässt den Wrapper weg.

use std::sync::Arc;

use agentkit::{truncate, Agent, Cancel, EventBus};

use crate::access::GraphAccess;
use crate::retrieval::{self, GraphQuery};
use crate::store::GraphStore;
use crate::write::{EpisodeDraft, GraphWriteCommand, SourceDraft};

/// Wie viel Graph vor den Auftrag gestellt wird — und ob der Zug protokolliert wird.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallPolicy {
    /// Recall vor jedem Zug. `false` = nur die Tools, kein automatischer Ausschnitt.
    pub automatic_recall: bool,
    pub max_claims: usize,
    /// Bewusst klein: der automatische Recall ist ein Anstoß, keine Wissensdumpe.
    /// Für alles Weitere hat das Modell `graph_search`.
    pub max_tokens: usize,
    /// Nach dem Zug eine Episode schreiben (nur mit Schreibziel).
    pub record_episode: bool,
}

impl Default for RecallPolicy {
    fn default() -> Self {
        RecallPolicy {
            automatic_recall: true,
            max_claims: 12,
            max_tokens: 800,
            record_episode: true,
        }
    }
}

impl RecallPolicy {
    /// Nur Tools, kein automatischer Recall und keine Episoden.
    pub fn tools_only() -> Self {
        RecallPolicy {
            automatic_recall: false,
            max_claims: 0,
            max_tokens: 0,
            record_episode: false,
        }
    }
}

pub struct GraphAgent {
    /// Der eingebettete Agent — öffentlich, damit Frontends weiterhin an
    /// `tools`, `memory` und die Slash-Befehle herankommen.
    pub agent: Agent,
    store: Arc<GraphStore>,
    access: GraphAccess,
    policy: RecallPolicy,
}

impl GraphAgent {
    pub fn new(agent: Agent, store: Arc<GraphStore>, access: GraphAccess) -> Self {
        GraphAgent {
            agent,
            store,
            access,
            policy: RecallPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: RecallPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn store(&self) -> &Arc<GraphStore> {
        &self.store
    }

    pub fn access(&self) -> &GraphAccess {
        &self.access
    }

    /// Der Graph-Ausschnitt zu einem Auftrag — `None`, wenn nichts passt oder der
    /// automatische Recall aus ist.
    pub fn recall(&self, task: &str) -> Option<String> {
        if !self.policy.automatic_recall {
            return None;
        }
        let index = self.store.snapshot();
        let query = GraphQuery::text(task).limits(self.policy.max_claims, self.policy.max_tokens);
        let sub = retrieval::search(&index, &self.access.view, &query);
        if sub.is_empty() {
            return None;
        }
        Some(retrieval::render(&index, &sub, false))
    }

    /// Auftrag mit vorangestelltem Graph-Ausschnitt.
    pub fn compose_task(&self, task: &str) -> String {
        match self.recall(task) {
            Some(context) => format!("{context}\n\n--- Aufgabe ---\n{task}"),
            None => task.to_string(),
        }
    }

    /// Wie `Agent::run`, mit Recall davor und Episode danach.
    pub fn run(&mut self, task: &str) -> String {
        let composed = self.compose_task(task);
        let answer = self.agent.run(&composed);
        self.record_episode(task, &answer);
        answer
    }

    /// Wie `Agent::run_on_bus` — der Pfad, den Worker-Threads und der Schwarm nutzen.
    pub fn run_on_bus(
        &mut self,
        task: &str,
        bus: &EventBus,
        task_id: i64,
        cancel: Option<&Cancel>,
        source: &str,
    ) -> String {
        let composed = self.compose_task(task);
        let answer = self
            .agent
            .run_on_bus(&composed, bus, task_id, cancel, source);
        self.record_episode(task, &answer);
        answer
    }

    /// Protokolliert den Zug als Episode. Fehler werden bewusst verschluckt: ein
    /// nicht schreibbarer Graph darf einen fertigen Agenten-Zug nicht entwerten.
    fn record_episode(&self, task: &str, answer: &str) {
        if !self.policy.record_episode || !self.access.can_write() {
            return;
        }
        let summary = format!(
            "{}: {} → {}",
            self.access.principal,
            truncate(task.trim(), 160),
            truncate(answer.trim(), 240)
        );
        let draft = EpisodeDraft::new(&summary, SourceDraft::new("agent_turn"));
        let _ = self
            .store
            .submit(GraphWriteCommand::RecordEpisode(draft), &self.access);
    }
}
