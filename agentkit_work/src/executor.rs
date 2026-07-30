//! Der Ausführungspfad EINES Versuchs: das Arbeitspaket, das der Agent sieht
//! (§12), und der Executor-Port, der es einem echten agentkit-Agenten
//! übergibt.
//!
//! Zwei Implementierungen erfüllen Guidelines §2 (ein Trait braucht ≥2 reale
//! Nutzer): [`CodingAgentExecutor`] hier, der Test-Doppelgänger
//! `ScriptedExecutor` in `tests/runner.rs`.

use std::fmt::Write as _;
use std::sync::Arc;

use agentkit::{
    build_coding_agent, builtin_roles, AgentEvent, ApproveFn, Cancel, CodingAgentConfig,
    ExtraToolCtx, ExtraTools, Llm, McpHub, Strategy,
};

use crate::error::WorkError;
use crate::model::{FailureInfo, FailureKind, WorkItem, WorkItemId, WorkItemKind, WorkItemStatus};
use crate::state::WorkState;
use crate::store::WorkStore;
use crate::tools::{register_work_tools, WorkToolCtx};

/// Deutsches Label einer [`WorkItemKind`] fürs Arbeitspaket — die Variantennamen
/// selbst sind englische Identifier (Sprachkonvention: Identifier englisch,
/// Nutzertext deutsch), der Prompt braucht die deutsche Form.
fn kind_label(kind: WorkItemKind) -> &'static str {
    match kind {
        WorkItemKind::Discovery => "Erkundung",
        WorkItemKind::Analysis => "Analyse",
        WorkItemKind::Planning => "Planung",
        WorkItemKind::Implementation => "Umsetzung",
        WorkItemKind::Test => "Test",
        WorkItemKind::Review => "Review",
        WorkItemKind::Documentation => "Dokumentation",
    }
}

/// Deutsches Label einer [`FailureKind`] — für die Fehlerursache im Arbeitspaket
/// des nächsten Versuchs (§12).
fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::ModelFailure => "Modell-/API-Fehler",
        FailureKind::MaxSteps => "Schrittlimit erreicht, ohne fertig zu werden",
        FailureKind::InvalidOutput => "keine verwertbare Antwort",
        FailureKind::Interrupted => "unterbrochen (Prozess-Abbruch)",
        FailureKind::BudgetExceeded => "Budget überschritten",
    }
}

/// Das Arbeitspaket EINES Versuchs: genau das, was der Agent sehen soll (§12) —
/// nicht die ganze Projektgeschichte.
pub struct AgentWorkPackage {
    pub item: WorkItem,
    pub objective: String,
    /// Artefakte der ABGESCHLOSSENEN Vorgänger: (Item-ID, workspace-relativer
    /// Pfad, Zusammenfassung).
    pub predecessor_artifacts: Vec<(WorkItemId, String, String)>,
    /// Ursachen aller bisherigen Fehlversuche DIESES Items, älteste zuerst.
    pub previous_failures: Vec<FailureInfo>,
    pub workspace: String,
    pub max_steps: u32,
}

impl AgentWorkPackage {
    /// Baut das Arbeitspaket aus dem aktuellen Zustand. Fehlt das Item oder das
    /// Projekt, ist das ein Programmierfehler des Aufrufers (der Scheduler hat
    /// das Item gerade als `Run(id)` ausgewählt) — deshalb `WorkError::NotFound`,
    /// kein weicher Fehler.
    pub fn build(
        state: &WorkState,
        item_id: &str,
        workspace: &str,
        max_steps: u32,
    ) -> Result<Self, WorkError> {
        let item = state
            .items
            .get(item_id)
            .cloned()
            .ok_or_else(|| WorkError::NotFound(format!("WorkItem '{item_id}'")))?;
        let objective = state
            .project
            .as_ref()
            .map(|p| p.objective.clone())
            .ok_or_else(|| WorkError::NotFound("Projekt".to_string()))?;

        // Nur Artefakte ABGESCHLOSSENER Vorgänger — ein Vorgänger, der (noch)
        // nicht Completed ist, hätte das Item laut Scheduler gar nicht erst
        // laufen lassen dürfen; die Prüfung hier ist trotzdem explizit, damit
        // das Arbeitspaket unabhängig von der Scheduler-Invariante korrekt bleibt.
        let mut predecessor_artifacts = Vec::new();
        for dep in &item.dependencies {
            let is_completed = state
                .items
                .get(dep)
                .is_some_and(|d| d.status == WorkItemStatus::Completed);
            if !is_completed {
                continue;
            }
            let mut artifacts: Vec<_> = state
                .artifacts
                .values()
                .filter(|a| &a.work_item_id == dep)
                .collect();
            artifacts.sort_by_key(|a| crate::model::id_order(&a.id));
            for artifact in artifacts {
                predecessor_artifacts.push((
                    dep.clone(),
                    artifact.rel_path.clone(),
                    artifact.summary.clone(),
                ));
            }
        }

        // Fehlerursachen ALLER bisherigen Versuche dieses Items, älteste zuerst
        // (id_order der Attempt-ID, nicht die BTreeMap-Zeichenkettenordnung —
        // sonst stünde "A-10" vor "A-9").
        let mut failed_attempts: Vec<_> = state
            .attempts
            .values()
            .filter(|a| a.work_item_id == item_id && a.failure.is_some())
            .collect();
        failed_attempts.sort_by_key(|a| crate::model::id_order(&a.id));
        let previous_failures = failed_attempts
            .into_iter()
            .filter_map(|a| a.failure.clone())
            .collect();

        Ok(AgentWorkPackage {
            item,
            objective,
            predecessor_artifacts,
            previous_failures,
            workspace: workspace.to_string(),
            max_steps,
        })
    }

    /// Der Auftragstext für den Agenten — deutsch, als Markdown. Jede Zeile ist
    /// Arbeitsanweisung, kein Blabla: der Text IST der Prompt.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str("## Projektziel\n\n");
        out.push_str(self.objective.trim());
        out.push_str("\n\n");

        writeln!(out, "## Work Item {} — {}\n", self.item.id, self.item.title).unwrap();
        writeln!(out, "Art: {}\n", kind_label(self.item.kind)).unwrap();
        out.push_str(self.item.description.trim());
        out.push_str("\n\n");

        out.push_str("### Akzeptanzkriterien\n\n");
        if self.item.acceptance_criteria.is_empty() {
            out.push_str(
                "Keine Akzeptanzkriterien definiert — bewerte das Ergebnis nach eigenem \
                 fachlichen Urteil.\n\n",
            );
        } else {
            for crit in &self.item.acceptance_criteria {
                out.push_str("- ");
                out.push_str(crit);
                out.push('\n');
            }
            out.push('\n');
        }

        if !self.predecessor_artifacts.is_empty() {
            out.push_str("### Artefakte der Vorgänger\n\n");
            out.push_str("Lies diese Dateien mit 'read_file' — rate ihren Inhalt nicht.\n\n");
            for (id, path, summary) in &self.predecessor_artifacts {
                writeln!(out, "- [{id}] `{path}` — {summary}").unwrap();
            }
            out.push('\n');
        }

        if !self.previous_failures.is_empty() {
            out.push_str("### Bisherige Fehlversuche dieses Items\n\n");
            out.push_str(
                "Geh nicht denselben Weg noch einmal — er hat schon einmal nicht zum Ziel \
                 geführt.\n\n",
            );
            for (i, failure) in self.previous_failures.iter().enumerate() {
                writeln!(
                    out,
                    "{}. {} — {}",
                    i + 1,
                    failure_kind_label(failure.kind),
                    failure.message
                )
                .unwrap();
            }
            out.push('\n');
        }

        out.push_str(
            "Lege dein Ergebnis mit 'work_artifact' ab und schließe den Versuch — auch bei \
             Misserfolg — mit 'work_submit' ab.\n",
        );

        out
    }
}

/// Führt einen Versuch aus. Zwei Implementierungen: [`CodingAgentExecutor`]
/// (echter agentkit-Agent) und der Test-Doppelgänger in den Tests.
pub trait AgentExecutor {
    /// `Ok(antwort)` = der Agent ist fertig geworden (die Antwort kann trotzdem
    /// ein Sentinel sein); `Err(meldung)` = der Lauf kam gar nicht zustande
    /// (API-/Aufbaufehler). Der Callback bekommt jedes Agenten-Ereignis: der
    /// Runner zählt darüber Schritte und Tool-Aufrufe und verlängert das Lease.
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String>;
}

/// Führt einen Versuch über einen echten, frisch gebauten agentkit-Agenten aus.
pub struct CodingAgentExecutor {
    pub llm: Arc<dyn Llm>,
    pub approve: ApproveFn,
    pub extra_tools: Option<ExtraTools>,
    pub cancel: Cancel,
    pub dry_run: bool,
    pub shell_timeout: u64,
    /// Zusätzlicher System-Prompt des Aufrufers (z. B. `agentkit_app`-spezifisch).
    pub system_extra: Option<String>,
}

impl AgentExecutor for CodingAgentExecutor {
    fn execute(
        &self,
        pkg: &AgentWorkPackage,
        ctx: WorkToolCtx,
        store: Arc<WorkStore>,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<String, String> {
        // System-Prompt: Work-Tool-Hinweise, dann (falls bekannt) der
        // Rollen-Prompt aus `item.required_role`, dann der Zusatz des
        // Aufrufers. Eine unbekannte Rolle ist kein Fehler — das Modell hat
        // den Namen erzeugt (`work_add_item` nimmt keine Rollen-Validierung
        // vor), also wird sie stillschweigend ignoriert statt den Versuch
        // platzen zu lassen.
        let mut system = String::from(crate::tools::WORK_SYSTEM);
        if let Some(role_name) = &pkg.item.required_role {
            if let Some(role) = builtin_roles().into_iter().find(|r| &r.name == role_name) {
                system.push_str("\n\n");
                system.push_str(&role.system);
            }
        }
        if let Some(extra) = &self.system_extra {
            system.push_str("\n\n");
            system.push_str(extra);
        }

        // extra_tools registriert ERST die Work-Tools (der Agent muss sein
        // Item bearbeiten können), DANN die vom Aufrufer durchgereichten
        // Tools (z. B. `swarm`/Graph aus agentkit_app) — so bekommt der
        // Work-Agent beides, ohne dass dieses Crate `agentkit_app` kennt.
        let caller_extra = self.extra_tools.clone();
        let extra_tools: ExtraTools = Arc::new(move |reg, ectx: &ExtraToolCtx| {
            register_work_tools(reg, store.clone(), ctx.clone());
            if let Some(extra) = &caller_extra {
                extra(reg, ectx);
            }
        });

        let cfg = CodingAgentConfig {
            workspace: &pkg.workspace,
            strategy: Strategy::React,
            max_steps: pkg.max_steps as usize,
            // Offene Frage, kein bewusster Ausschluss: ob ein Work-Item-Versuch
            // von Skills profitieren würde, ist im MVP nicht geklärt — Folge-Issue.
            skills: None,
            // `--agents DIR` ist eine CLI-/Frontend-Option; dieses Crate kennt
            // kein Verzeichnis-Flag und damit keinen Wert, den es hier setzen könnte.
            agents: None,
            memory: None,
            subagents: true,
            system: Some(&system),
            verify: false,
            shell_timeout: self.shell_timeout,
            dry_run: self.dry_run,
            extra_tools: Some(extra_tools),
        };

        // Ein neuer Agent PRO Versuch — bewusst ein leerer Kontext, nicht der
        // fortgeführte Verlauf eines vorherigen Versuchs: das IST der Sinn
        // dieser Runtime (§12/§18). Ein gescheiterter Versuch soll seinen
        // Kontext nicht in den nächsten mitschleppen; was weitergegeben wird,
        // steht explizit im Arbeitspaket (Artefakte, Fehlerursachen), nicht im
        // Gesprächsverlauf.
        let (mut agent, _plan, _skills, _roles, _mcp_base, _coding) = build_coding_agent(
            self.llm.clone(),
            &cfg,
            self.approve.clone(),
            Arc::new(McpHub::empty()),
        );

        let cancel = self.cancel.clone();
        let task = pkg.render();
        let response = agent.run_cb(&task, Some(&cancel), |ev| on_event(&ev));
        Ok(response)
    }
}
