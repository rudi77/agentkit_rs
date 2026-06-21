//! CLI-Bausteine als Unix-I/O-Adapter — die pipe-tauglichen Helfer der
//! `agentkit`-Executable, bewusst entkoppelt von der Ausführung (und testbar).
//!
//! Im Sinne der hexagonalen Architektur sind die Standard-Streams die primären
//! I/O-Adapter; die Kernlogik (Agent-Loop, Tools, Memory) bleibt unberührt:
//!
//! - **`stdin`**  trägt *ausschließlich* Kontext/Datenströme (per Pipe). Wird er
//!   nicht interaktiv genutzt (`is_terminal() == false`), wird der gesamte Inhalt
//!   gelesen und an die User-Query angehängt.
//! - **`stdout`** trägt *ausschließlich* das finale, bereinigte Resultat — keine
//!   Statusmeldungen, kein TUI, kein Debug. Damit kann ein nachfolgendes Tool
//!   (`jq`, `awk`, ein zweiter Agent) sich auf Format-Treue verlassen.
//! - **`stderr`** trägt alles andere: Status, Tool-Spur, ReAct-Gedanken, Fehler.
//!
//! Diese Datei bündelt das [`OutputFormat`], die [`ExitCode`]s und die reinen
//! Hilfsfunktionen (stdin lesen, Task bauen, JSON extrahieren, Ergebnis einordnen).
//! Das Argument-Parsing selbst lebt im `agentkit`-Binary.

use std::io::{self, IsTerminal, Read};

/// Exit-Codes für verlässliches Chaining (`set -e` in Bash-Pipelines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// `0` — Aufgabe erfolgreich, Resultat auf `stdout` geflusht.
    Success = 0,
    /// `1` — unerwarteter Laufzeitfehler des CLI-Tools.
    GeneralError = 1,
    /// `2` — Modell nicht erreichbar / Rate-Limit / Netzwerkfehler.
    ApiError = 2,
    /// `3` — Kontext zu groß oder Prompt ungültig.
    ContextError = 3,
    /// `4` — erzwungenes Format (`--format`) trotz Retries nicht erzeugbar.
    FormatError = 4,
}

impl ExitCode {
    /// Der numerische Code für [`std::process::exit`].
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Erzwungenes Ausgabeformat (`--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Freitext (Standard).
    #[default]
    Text,
    /// Strukturiertes JSON (aktiviert OpenAI/Azure JSON-Mode + Validierung/Retries).
    Json,
}

/// System-Anweisung, die im JSON-Modus zusätzlich injiziert wird.
pub const JSON_SYSTEM: &str = "Gib deine endgültige Antwort AUSSCHLIESSLICH als ein \
einziges, gültiges JSON-Objekt aus. Keine Code-Fences, kein Markdown, kein \
erklärender Text davor oder danach.";

/// Liest gepipte Kontextdaten von `stdin` — aber nur, wenn `stdin` *nicht*
/// interaktiv ist (also via Pipe/Umleitung kommt). Gibt `None` zurück, wenn `stdin`
/// ein Terminal ist oder der Strom leer war.
pub fn read_stdin_context() -> io::Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    let trimmed = buf.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Verbindet Prompt und (optionalen) stdin-Kontext zur User-Query. Ohne Prompt wird
/// der Kontext selbst zur Query; ohne Kontext bleibt es der reine Prompt.
pub fn build_task(prompt: &str, context: Option<&str>) -> String {
    let prompt = prompt.trim();
    match context {
        Some(ctx) if !prompt.is_empty() => {
            format!("{prompt}\n\n--- Kontext (über stdin) ---\n{ctx}")
        }
        Some(ctx) => ctx.to_string(),
        None => prompt.to_string(),
    }
}

/// Versucht, aus einer Modellantwort ein einzelnes, gültiges JSON-Objekt/-Array zu
/// gewinnen: erst die ganze (getrimmte) Antwort, dann ein ```json-Fence, zuletzt der
/// Bereich vom ersten `{`/`[` bis zur passenden schließenden Klammer. Gibt die
/// kanonische (kompakte) Serialisierung zurück oder `None`, wenn nichts Gültiges
/// gefunden wurde.
pub fn extract_json(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // 1) Komplette Antwort ist bereits JSON.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(v.to_string());
    }

    // 2) Innerhalb eines Code-Fences (```json … ``` oder ``` … ```).
    if let Some(rest) = trimmed.split("```").nth(1) {
        let inner = rest.strip_prefix("json").unwrap_or(rest);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(inner.trim()) {
            return Some(v.to_string());
        }
    }

    // 3) Eingebettet: vom ersten Klammer-Start bis zum letzten passenden Ende.
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(start), Some(end)) = (trimmed.find(open), trimmed.rfind(close)) {
            if start < end {
                let slice = &trimmed[start..=end];
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                    return Some(v.to_string());
                }
            }
        }
    }

    None
}

/// Sentinel-Antworten des Agent-Loops auf einen Exit-Code abbilden. `None` bedeutet
/// "echtes Resultat" (Erfolg). Ein erfasster harter Fehler (Modell unerreichbar)
/// hat Vorrang und ergibt [`ExitCode::ApiError`].
pub fn classify_outcome(final_text: &str, hard_error: bool) -> Option<ExitCode> {
    if hard_error {
        return Some(ExitCode::ApiError);
    }
    match final_text {
        "(keine Antwort)" => Some(ExitCode::ApiError),
        "(max_steps erreicht)" | "(abgebrochen)" => Some(ExitCode::GeneralError),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::GeneralError.code(), 1);
        assert_eq!(ExitCode::ApiError.code(), 2);
        assert_eq!(ExitCode::ContextError.code(), 3);
        assert_eq!(ExitCode::FormatError.code(), 4);
    }

    #[test]
    fn output_format_default_is_text() {
        assert_eq!(OutputFormat::default(), OutputFormat::Text);
    }

    #[test]
    fn build_task_combines_prompt_and_context() {
        assert_eq!(build_task("frage", None), "frage");
        assert_eq!(build_task("", Some("daten")), "daten");
        let t = build_task("frage", Some("daten"));
        assert!(t.starts_with("frage"));
        assert!(t.contains("--- Kontext (über stdin) ---"));
        assert!(t.contains("daten"));
        assert_eq!(build_task("  ", None), "");
    }

    #[test]
    fn extract_json_handles_plain_fenced_and_embedded() {
        assert_eq!(extract_json(r#"{"a":1}"#), Some(r#"{"a":1}"#.to_string()));
        // Code-Fence
        assert_eq!(
            extract_json("```json\n{\"a\": 1}\n```"),
            Some(r#"{"a":1}"#.to_string())
        );
        // Eingebettet mit Geschwätz drumherum.
        assert_eq!(
            extract_json("Hier ist das Ergebnis: {\"ok\": true} — fertig."),
            Some(r#"{"ok":true}"#.to_string())
        );
        // Array
        assert_eq!(extract_json("[1, 2, 3]"), Some("[1,2,3]".to_string()));
        // Kein JSON.
        assert_eq!(extract_json("einfach nur Text"), None);
    }

    #[test]
    fn classify_outcome_maps_sentinels() {
        assert_eq!(classify_outcome("echtes Resultat", false), None);
        assert_eq!(
            classify_outcome("(keine Antwort)", false),
            Some(ExitCode::ApiError)
        );
        assert_eq!(
            classify_outcome("(max_steps erreicht)", false),
            Some(ExitCode::GeneralError)
        );
        assert_eq!(classify_outcome("egal", true), Some(ExitCode::ApiError));
    }
}
