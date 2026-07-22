//! Planning — eine mitgeführte Todo-Liste + das `update_plan`-Tool.
//!
//! Damit der Plan *sichtbar* und *mitgeführt* wird (wie die Todo-Liste in Claude
//! Code), bekommt der Agent ein `update_plan`-Tool: das Modell schreibt seinen
//! Plan als Liste von Schritten mit Status, der Agent hält ihn fest und rendert ihn.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn status_mark(status: &str) -> &'static str {
    match status {
        "in_progress" => "[~]",
        "done" => "[x]",
        _ => "[ ]",
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub step: String,
    pub status: String,
}

type OnUpdate = Arc<dyn Fn(&[Step]) + Send + Sync>;

/// Hält die aktuelle Todo-Liste des Agenten. Über einen `Arc<Mutex<…>>`-Kern
/// klonbar, damit das registrierte Tool und das `Plan`-Handle denselben Zustand
/// sehen.
#[derive(Clone)]
pub struct Plan {
    steps: Arc<Mutex<Vec<Step>>>,
    on_update: Option<OnUpdate>,
}

impl Plan {
    pub fn new() -> Self {
        Plan {
            steps: Arc::new(Mutex::new(Vec::new())),
            on_update: None,
        }
    }

    /// Wie `Plan(on_update=...)`: Callback nach jeder Aktualisierung (z. B. für ein
    /// PLAN-Event in der UI).
    pub fn with_on_update<F>(f: F) -> Self
    where
        F: Fn(&[Step]) + Send + Sync + 'static,
    {
        Plan {
            steps: Arc::new(Mutex::new(Vec::new())),
            on_update: Some(Arc::new(f)),
        }
    }

    pub fn len(&self) -> usize {
        self.steps.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ersetzt den Plan komplett (wie TodoWrite) und gibt ihn gerendert zurück.
    pub fn update(&self, steps: Vec<Step>) -> String {
        let cleaned: Vec<Step> = steps
            .into_iter()
            .map(|s| {
                let status = match s.status.as_str() {
                    "pending" | "in_progress" | "done" => s.status,
                    _ => "pending".to_string(),
                };
                Step {
                    step: s.step.trim().to_string(),
                    status,
                }
            })
            .collect();
        {
            let mut guard = self.steps.lock().unwrap();
            *guard = cleaned;
            if let Some(cb) = &self.on_update {
                cb(&guard);
            }
        } // Lock vor render() freigeben.
        self.render()
    }

    pub fn render(&self) -> String {
        let steps = self.steps.lock().unwrap();
        if steps.is_empty() {
            return "(noch kein Plan)".to_string();
        }
        steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{} {}. {}", status_mark(&s.status), i + 1, s.step))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Bietet dem Agenten das `update_plan`-Tool an.
    pub fn register_tool(&self, registry: &mut ToolRegistry) {
        let me = self.clone();
        registry.add(
            "update_plan",
            "Legt den Arbeitsplan an oder aktualisiert ihn. Übergib die KOMPLETTE \
             Schrittliste; markiere den aktuellen Schritt als 'in_progress' und \
             erledigte als 'done'. Rufe das Tool zu Beginn und nach jedem Fortschritt auf.",
            json!({"type": "object", "properties": {
                "steps": {"type": "array", "items": {
                    "type": "object", "properties": {
                        "step": {"type": "string", "description": "Beschreibung des Schritts."},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "done"]},
                    }, "required": ["step", "status"]}}},
             "required": ["steps"]}),
            move |args: Value| {
                let steps = args
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|s| Step {
                                step: s
                                    .get("step")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                status: s
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                                    .to_string(),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(me.update(steps))
            },
        );
    }
}

impl Default for Plan {
    fn default() -> Self {
        Plan::new()
    }
}
