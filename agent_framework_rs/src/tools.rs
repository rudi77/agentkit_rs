//! Tool-Registry — Tools sind nur Funktionen + JSON-Schema.
//!
//! Wie in den Notebooks: Schema (fürs Modell) und Funktion (für die Ausführung)
//! liegen an EINER Stelle. Pythons `@registry.tool()`-Decorator leitet das Schema
//! aus Typ-Hints + Docstring ab — das geht in Rust mangels Laufzeit-Reflection
//! nicht generisch, daher wird das Schema hier explizit übergeben (`add`). Für
//! typsichere Argument-Deserialisierung gibt es zusätzlich [`ToolRegistry::add_typed`].

use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Eine Tool-Funktion: nimmt die geparsten Argumente (JSON-Objekt) und liefert
/// entweder ein Ergebnis als String oder einen Fehlertext.
///
/// `Arc` macht die Registry billig klonbar (wichtig für Sub-Agents, die eine
/// eigene Kopie der Registry brauchen) und `Send + Sync` erlaubt parallele
/// Tool-Ausführung über Threads.
pub type ToolFn = Arc<dyn Fn(Value) -> Result<String, String> + Send + Sync>;

/// Hält Schemas (fürs Modell) und Funktionen (für die Ausführung).
#[derive(Clone, Default)]
pub struct ToolRegistry {
    schemas: Vec<Value>,
    fns: HashMap<String, ToolFn>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry::default()
    }

    /// Tool programmatisch registrieren (z. B. aus MCP, Memory, Planning, Skills).
    pub fn add<F>(&mut self, name: &str, description: &str, parameters: Value, f: F)
    where
        F: Fn(Value) -> Result<String, String> + Send + Sync + 'static,
    {
        self.add_arc(name, description, parameters, Arc::new(f));
    }

    /// Wie [`add`], aber mit einer bereits geteilten Funktion (`Arc`).
    pub fn add_arc(&mut self, name: &str, description: &str, parameters: Value, f: ToolFn) {
        self.schemas.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": parameters,
            },
        }));
        self.fns.insert(name.to_string(), f);
    }

    /// Komfort: typsicheres Tool — die Argumente werden nach `A` deserialisiert.
    /// Kommt einem `@tool()` am nächsten, das Schema bleibt aber explizit.
    pub fn add_typed<A, R, F>(&mut self, name: &str, description: &str, parameters: Value, f: F)
    where
        A: DeserializeOwned,
        R: ToString,
        F: Fn(A) -> R + Send + Sync + 'static,
    {
        self.add(name, description, parameters, move |args: Value| {
            let parsed: A =
                serde_json::from_value(args).map_err(|e| format!("ungültige Argumente: {e}"))?;
            Ok(f(parsed).to_string())
        });
    }

    /// Tool-Schemas fürs Modell — oder `None`, wenn keine Tools da sind
    /// (entspricht Pythons `schemas()`). Borgt die Schemas, statt sie zu klonen:
    /// der Agent ruft das pro Schritt auf.
    pub fn schemas(&self) -> Option<&[Value]> {
        if self.schemas.is_empty() {
            None
        } else {
            Some(&self.schemas)
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.fns.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.fns.keys().cloned().collect()
    }

    /// Führt ein Tool aus. Unbekannte Tools werden als Fehlertext gemeldet
    /// (das Modell kann sich dann selbst korrigieren) — wie in Python ein
    /// "weicher" Fehler ohne Exception.
    pub fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match self.fns.get(name) {
            None => Ok(format!("ERROR: unbekanntes Tool '{name}'")),
            Some(f) => f(args),
        }
    }

    /// Erzeugt eine `--dry-run`-Variante der Registry: Tools, für die
    /// `is_destructive(name)` `true` liefert, werden NICHT mehr ausgeführt, sondern
    /// durch einen No-Op ersetzt, der nur einen Hinweistext zurückgibt (den der
    /// Agent-Loop als `tool_result` nach stderr loggt). Die Tool-Schemas bleiben
    /// identisch, damit das Modell denselben Werkzeugkasten "sieht" und der Loop
    /// unverändert durchläuft. Lese-/unkritische Tools bleiben aktiv.
    pub fn dry_run_blocking(&self, is_destructive: impl Fn(&str) -> bool) -> ToolRegistry {
        let mut out = ToolRegistry {
            schemas: self.schemas.clone(),
            fns: HashMap::with_capacity(self.fns.len()),
        };
        for (name, f) in &self.fns {
            if is_destructive(name) {
                let n = name.clone();
                out.fns.insert(
                    name.clone(),
                    Arc::new(move |args: Value| {
                        Ok(format!(
                            "[dry-run] '{n}' NICHT ausgeführt — zerstörerischer \
                             Schreibvorgang blockiert. Argumente: {args}"
                        ))
                    }),
                );
            } else {
                out.fns.insert(name.clone(), f.clone());
            }
        }
        out
    }
}

/// Heuristik für [`ToolRegistry::dry_run_blocking`]: schätzt anhand des Tool-Namens,
/// ob ein Tool potenziell schreibend/zerstörerisch wirkt (Datei-, Shell-, Netz- oder
/// Persistenz-Effekte). Bewusst konservativ über bekannte Verb-Marker — reine
/// Lese-/Abfrage-Tools (`read`, `list`, `get`, `recall`, …) bleiben erlaubt.
pub fn is_likely_destructive(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "write", "edit", "delete", "remove", "create", "update", "shell", "exec", "run", "save",
        "post", "patch", "put", "drop", "insert", "append", "send", "remember", "mkdir", "move",
        "rename", "upload", "commit", "push", "kill",
    ];
    let lower = name.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}
