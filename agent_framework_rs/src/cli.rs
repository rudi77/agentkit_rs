//! CLI als Unix-I/O-Adapter — Single Source of Truth (SSOT) für die `agentkit`-
//! Executable (Feature `cli`).
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
//! Diese Datei bündelt die SSOT-[`Config`] (clap), das [`OutputFormat`], die
//! [`ExitCode`]s und die reinen Hilfsfunktionen (stdin lesen, Task bauen,
//! JSON extrahieren) — bewusst entkoppelt von der Ausführung, damit sie testbar
//! bleiben.

use std::io::{self, IsTerminal, Read};

use clap::{Parser, ValueEnum};

use crate::Strategy;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
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

/// Single Source of Truth für CLI-Parameter, Defaults und Umgebungsvariablen.
///
/// Alles, was die `agentkit`-Executable steuert, ist hier zentral abgebildet —
/// so bleibt `--help`, interne Ausführung und Doku driftfrei.
#[derive(Parser, Debug)]
#[command(
    name = "agentkit",
    version,
    about = "Ein kleines Agent-Framework als Unix-Kommandozeilenwerkzeug.",
    long_about = "agentkit — ein LLM in einer Schleife mit Tools, gebaut als nativer \
Unix-Filter.\n\n\
stdin  = Kontext/Datenstrom (per Pipe), wird an die Query angehängt.\n\
stdout = ausschließlich das finale, bereinigte Resultat (pipe-tauglich).\n\
stderr = Status, Tool-Spur, ReAct-Gedanken, Fehler.\n\n\
Optionen stehen VOR dem Prompt, z. B.:\n  \
  cat daten.json | agentkit --format json \"Fasse den Kontext zusammen\"\n\n\
LLM-Auswahl (ohne --demo, Feature `openai`): AZURE_OPENAI_* -> Azure, sonst \
OPENAI_API_KEY [+ OPENAI_MODEL] -> OpenAI, sonst Demo-Modus (kein Netz)."
)]
pub struct Config {
    /// Das Hauptargument: die direkte Anweisung an den Agenten (mehrere Wörter ok).
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    pub prompt: Vec<String>,

    /// Erzwingt strukturierten Output.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Führt den Loop aus, blockiert aber zerstörerische Schreibvorgänge (nur stderr-Log).
    #[arg(long)]
    pub dry_run: bool,

    /// Demo-Modus erzwingen (eingebauter, netzfreier LLM).
    #[arg(long)]
    pub demo: bool,

    /// Plan-and-Execute statt ReAct.
    #[arg(long, conflicts_with = "plain")]
    pub plan: bool,

    /// Ohne Strategie-Preamble.
    #[arg(long)]
    pub plain: bool,

    /// Interaktiver Zeilen-REPL (Gedächtnis bleibt erhalten).
    #[arg(long)]
    pub repl: bool,

    /// Interaktives Terminal-UI (nur mit Feature `tui`).
    #[arg(long)]
    pub tui: bool,

    /// Maximales Kontextfenster in (geschätzten) Tokens; größer -> Exit-Code 3.
    #[arg(
        long,
        value_name = "TOKENS",
        default_value_t = 128_000,
        env = "AGENTKIT_MAX_CONTEXT"
    )]
    pub max_context: usize,

    /// Anzahl Versuche, im JSON-Modus gültiges JSON zu erzwingen, bevor Exit-Code 4.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 3,
        env = "AGENTKIT_JSON_RETRIES"
    )]
    pub json_retries: u32,
}

impl Config {
    /// Der Prompt-Text (mehrere Wörter mit Leerzeichen verbunden).
    pub fn prompt_text(&self) -> String {
        self.prompt.join(" ")
    }

    /// Strategie aus den Flags (`--plan` / `--plain`, sonst ReAct).
    pub fn strategy(&self) -> Strategy {
        if self.plan {
            Strategy::Plan
        } else if self.plain {
            Strategy::Plain
        } else {
            Strategy::React
        }
    }

    /// `true`, wenn JSON-Output erzwungen ist.
    pub fn json_mode(&self) -> bool {
        self.format == OutputFormat::Json
    }
}

/// Exit-Code für einen clap-Parse-Fehler im Sinne unseres Vertrags.
///
/// `None` = von clap selbst behandeln lassen (`--help`/`--version`: Ausgabe auf
/// stdout, Exit 0). `Some(_)` = unser Code: Nutzungsfehler (unbekanntes Flag,
/// ungültiger Wert wie `AGENTKIT_MAX_CONTEXT=abc`, …) sind Validierungsfehler und
/// enden mit [`ExitCode::ContextError`] — bewusst **nicht** mit clap's Default `2`,
/// der bei uns für API-/Netzfehler reserviert ist.
pub fn parse_error_exit(kind: clap::error::ErrorKind) -> Option<ExitCode> {
    use clap::error::ErrorKind;
    match kind {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => None,
        _ => Some(ExitCode::ContextError),
    }
}

/// Parst die CLI-Argumente (SSOT) und hält dabei den Exit-Code-Vertrag ein:
/// `--help`/`--version` laufen wie gewohnt (stdout, Exit 0), echte Nutzungsfehler
/// gehen auf stderr und enden mit Exit 3 statt clap's Default 2.
pub fn parse_config() -> Config {
    match Config::try_parse() {
        Ok(config) => config,
        Err(err) => match parse_error_exit(err.kind()) {
            None => err.exit(),
            Some(code) => {
                let _ = err.print();
                std::process::exit(code.code());
            }
        },
    }
}

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
    fn config_parses_flags_before_prompt() {
        let c = Config::try_parse_from([
            "agentkit",
            "--format",
            "json",
            "--dry-run",
            "Fasse",
            "das",
            "zusammen",
        ])
        .unwrap();
        assert!(c.json_mode());
        assert!(c.dry_run);
        assert_eq!(c.prompt_text(), "Fasse das zusammen");
        assert_eq!(c.strategy(), Strategy::React);
    }

    #[test]
    fn config_strategy_flags() {
        let c = Config::try_parse_from(["agentkit", "--plan", "x"]).unwrap();
        assert_eq!(c.strategy(), Strategy::Plan);
        let c = Config::try_parse_from(["agentkit", "--plain", "x"]).unwrap();
        assert_eq!(c.strategy(), Strategy::Plain);
        // --plan und --plain schließen sich aus.
        assert!(Config::try_parse_from(["agentkit", "--plan", "--plain", "x"]).is_err());
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
    fn parse_errors_map_off_api_exit_code() {
        use clap::error::ErrorKind;
        // Hilfe/Version: clap selbst überlassen (Exit 0 auf stdout).
        assert_eq!(parse_error_exit(ErrorKind::DisplayHelp), None);
        assert_eq!(parse_error_exit(ErrorKind::DisplayVersion), None);
        // Nutzungsfehler -> Validierung (Exit 3), niemals der API-Code 2.
        for kind in [ErrorKind::UnknownArgument, ErrorKind::ValueValidation] {
            assert_eq!(parse_error_exit(kind), Some(ExitCode::ContextError));
            assert_ne!(parse_error_exit(kind), Some(ExitCode::ApiError));
        }
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
