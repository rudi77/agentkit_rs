//! Planning — eine mitgeführte Todo-Liste + das `update_plan`-Tool.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Ein nicht-ausführbarer, überprüfbarer Arbeitsplan. Anders als [`Plan`] ist
/// dieses Objekt unveränderlich: Es beschreibt vor einer Ausführung den
/// kleinstmöglichen Lösungsweg und seinen Abhängigkeitsgraphen.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanningOutput {
    pub approach: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Regeln, die der spätere Executor beim Umsetzen einhalten muss.
    #[serde(default)]
    pub rules: Vec<String>,
    pub steps: Vec<PlanningStep>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanningStep {
    pub id: String,
    pub step: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl PlanningOutput {
    /// Liest und validiert einen vom Planner erzeugten DAG.
    pub fn from_json(text: &str, max_steps: usize) -> Result<Self, String> {
        let mut plan: Self = serde_json::from_str(text)
            .map_err(|e| format!("ungültiges Plan-JSON: {e}"))?;
        plan.approach = plan.approach.trim().to_string();
        if plan.approach.is_empty() {
            return Err("der Lösungsansatz fehlt".to_string());
        }
        for entries in [&mut plan.constraints, &mut plan.rules] {
            for entry in entries.iter_mut() {
                *entry = entry.trim().to_string();
            }
            entries.retain(|entry| !entry.is_empty());
        }
        if plan.steps.is_empty() || plan.steps.len() > max_steps {
            return Err(format!("der Plan braucht zwischen 1 und {max_steps} Schritten"));
        }
        let mut ids = HashSet::new();
        for item in &mut plan.steps {
            item.id = item.id.trim().to_string();
            item.step = item.step.trim().to_string();
            if item.id.is_empty() || item.step.is_empty() || !ids.insert(item.id.clone()) {
                return Err("jeder Schritt braucht eine eindeutige ID und Beschreibung".to_string());
            }
        }
        for item in &plan.steps {
            let mut seen = HashSet::new();
            for dependency in &item.depends_on {
                if dependency == &item.id || !ids.contains(dependency) || !seen.insert(dependency) {
                    return Err(format!("ungültige Abhängigkeit bei Schritt '{}'", item.id));
                }
            }
        }
        let by_id: HashMap<&str, &PlanningStep> =
            plan.steps.iter().map(|step| (step.id.as_str(), step)).collect();
        fn visit<'a>(
            id: &'a str,
            by_id: &HashMap<&'a str, &'a PlanningStep>,
            visiting: &mut HashSet<&'a str>,
            done: &mut HashSet<&'a str>,
        ) -> bool {
            if done.contains(id) {
                return true;
            }
            if !visiting.insert(id) {
                return false;
            }
            let valid = by_id[id]
                .depends_on
                .iter()
                .all(|dependency| visit(dependency, by_id, visiting, done));
            visiting.remove(id);
            if valid {
                done.insert(id);
            }
            valid
        }
        let mut done = HashSet::new();
        for id in by_id.keys() {
            if !visit(id, &by_id, &mut HashSet::new(), &mut done) {
                return Err("der Plan enthält einen Abhängigkeitszyklus".to_string());
            }
        }
        Ok(plan)
    }

    /// Liefert alle Knoten, deren Vorgänger abgeschlossen sind. Ein Executor kann
    /// diese Menge gleichzeitig starten und nach Abschluss erneut abfragen.
    pub fn ready_step_ids<'a>(&'a self, completed: &HashSet<&str>) -> Vec<&'a str> {
        self.steps
            .iter()
            .filter(|step| {
                !completed.contains(step.id.as_str())
                    && step.depends_on.iter().all(|dependency| completed.contains(dependency.as_str()))
            })
            .map(|step| step.id.as_str())
            .collect()
    }

    /// Textform für CLI, REPL und TUI. Die Kanten stehen direkt am Zielknoten,
    /// damit unabhängige Schritte für einen Scheduler erkennbar bleiben.
    pub fn render(&self) -> String {
        let mut lines = vec![format!("Ansatz: {}", self.approach)];
        if !self.constraints.is_empty() {
            lines.push("Constraints:".to_string());
            lines.extend(self.constraints.iter().map(|constraint| format!("- {constraint}")));
        }
        if !self.rules.is_empty() {
            lines.push("Regeln:".to_string());
            lines.extend(self.rules.iter().map(|rule| format!("- {rule}")));
        }
        lines.push("Plan:".to_string());
        lines.extend(self.steps.iter().map(|step| match step.depends_on.as_slice() {
            [] => format!("{}: {}", step.id, step.step),
            dependencies => format!("{} <- {}: {}", step.id, dependencies.join(", "), step.step),
        }));
        lines.join("\n")
    }
}

fn status_mark(status: &str) -> &'static str {
    match status {
        "in_progress" => "[~]",
        "done" => "[x]",
        "failed" => "[!]",
        _ => "[ ]",
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Step {
    pub step: String,
    pub status: String,
}

type OnUpdate = Arc<dyn Fn(&[Step]) + Send + Sync>;

#[derive(Clone)]
pub struct Plan {
    steps: Arc<Mutex<Vec<Step>>>,
    on_update: Option<OnUpdate>,
}

impl Plan {
    pub fn new() -> Self {
        Plan { steps: Arc::new(Mutex::new(Vec::new())), on_update: None }
    }

    pub fn with_on_update<F>(f: F) -> Self
    where F: Fn(&[Step]) + Send + Sync + 'static {
        Plan { steps: Arc::new(Mutex::new(Vec::new())), on_update: Some(Arc::new(f)) }
    }

    pub fn len(&self) -> usize { self.steps.lock().unwrap().len() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn update(&self, steps: Vec<Step>) -> String {
        let cleaned: Vec<Step> = steps.into_iter().map(|step| Step {
            step: step.step.trim().to_string(),
            status: match step.status.as_str() {
                "pending" | "in_progress" | "done" => step.status,
                _ => "pending".to_string(),
            },
        }).collect();
        let snapshot = {
            let mut guard = self.steps.lock().unwrap();
            *guard = cleaned;
            guard.clone()
        };
        if let Some(callback) = &self.on_update { callback(&snapshot); }
        self.render()
    }

    pub fn render(&self) -> String {
        let steps = self.steps.lock().unwrap();
        if steps.is_empty() { return "(noch kein Plan)".to_string(); }
        steps.iter().enumerate().map(|(i, step)| {
            format!("{} {}. {}", status_mark(&step.status), i + 1, step.step)
        }).collect::<Vec<_>>().join("\n")
    }

    pub fn register_tool(&self, registry: &mut ToolRegistry) {
        let me = self.clone();
        registry.add(
            "update_plan",
            "Legt den Arbeitsplan an oder aktualisiert ihn. Übergib die KOMPLETTE Schrittliste; markiere den aktuellen Schritt als 'in_progress' und erledigte als 'done'. Rufe das Tool zu Beginn und nach jedem Fortschritt auf.",
            json!({"type": "object", "properties": {"steps": {"type": "array", "items": {"type": "object", "properties": {"step": {"type": "string", "description": "Beschreibung des Schritts."}, "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}}, "required": ["step", "status"]}}}, "required": ["steps"]}),
            move |args: Value| {
                let steps = args.get("steps").and_then(Value::as_array).map(|items| {
                    items.iter().map(|item| Step {
                        step: item.get("step").and_then(Value::as_str).unwrap_or("").to_string(),
                        status: item.get("status").and_then(Value::as_str).unwrap_or("pending").to_string(),
                    }).collect::<Vec<_>>()
                }).unwrap_or_default();
                Ok(me.update(steps))
            },
        );
        registry.declare("update_plan", crate::tools::ToolEffect::ReadOnly);
    }
}

impl Default for Plan {
    fn default() -> Self { Plan::new() }
}
