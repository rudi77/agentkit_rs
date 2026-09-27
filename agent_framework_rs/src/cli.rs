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
use std::path::Path;

use serde_json::{json, Value};

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
    /// `124` — `--timeout` abgelaufen (derselbe Code wie `timeout(1)`).
    Timeout = 124,
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

/// Hängt Dateien (`-f datei`) mit Namen als Kontextblöcke an den Auftrag an.
/// Der Name steht im Block, damit das Modell sich auf „die zweite Datei" oder
/// `src/main.rs` beziehen kann — genau das fehlt bei `cat a b |`.
pub fn attach_files(task: &str, files: &[(String, String)]) -> String {
    let mut out = task.to_string();
    for (name, content) in files {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("--- Datei: {name} ---\n{}", content.trim_end()));
    }
    out
}

/// Setzt einen Datensatz (`--each`) in den Prompt ein: jedes `{}` wird durch
/// den Datensatz ersetzt (wie bei `xargs -I{}`). Ohne `{}` wird der Datensatz
/// wie gepipter Kontext angehängt.
pub fn expand_template(prompt: &str, record: &str) -> String {
    if prompt.contains("{}") {
        prompt.replace("{}", record).trim().to_string()
    } else {
        build_task(prompt, Some(record))
    }
}

/// Das Antwort-Schema von `--check`: ein Urteil und seine Begründung.
pub fn check_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ergebnis": {"type": "boolean", "description": "true = ja, false = nein"},
            "begruendung": {"type": "string"}
        },
        "required": ["ergebnis", "begruendung"],
        "additionalProperties": false
    })
}

/// System-Anweisung für `--check` (kommt zu [`schema_system`] hinzu).
pub const CHECK_SYSTEM: &str = "Der Auftrag ist eine Ja/Nein-Prüfung. Prüfe sorgfältig \
und antworte mit `ergebnis` = true, wenn die Frage mit JA zu beantworten ist, sonst \
false. `begruendung` erklärt das Urteil in ein bis drei Sätzen.";

/// Liest das Urteil einer `--check`-Antwort: `(ja?, begründung)`.
pub fn check_verdict(answer: &Value) -> Option<(bool, String)> {
    let ok = answer.get("ergebnis")?.as_bool()?;
    let why = answer
        .get("begruendung")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((ok, why))
}

/// System-Anweisung für eine Antwort nach JSON-Schema (`--schema`, `--check`).
pub fn schema_system(schema: &Value) -> String {
    format!(
        "{JSON_SYSTEM}\n\nDas JSON MUSS diesem JSON-Schema entsprechen:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}

/// Cache-Schlüssel (`--cache`) aus allen Teilen, die das Ergebnis bestimmen.
///
/// FNV-1a mit 128 Bit statt einer Krypto-Abhängigkeit: es geht um
/// Wiedererkennung, nicht um Angriffe, und 128 Bit machen einen Zufallstreffer
/// — der eine FALSCHE Antwort liefern würde — praktisch unmöglich. Die Teile
/// gehen längen-präfixiert ein, damit `["ab", "c"]` und `["a", "bc"]` nicht
/// denselben Schlüssel ergeben.
pub fn cache_key(parts: &[&str]) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut h = OFFSET;
    for part in parts {
        let len = (part.len() as u64).to_le_bytes();
        for b in len.iter().chain(part.as_bytes()) {
            h ^= *b as u128;
            h = h.wrapping_mul(PRIME);
        }
    }
    format!("{h:032x}")
}

/// Liest ein gespeichertes Ergebnis (`None` bei Fehlschuss oder kaputter Datei).
pub fn cache_load(dir: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(format!("{key}.json"))).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("result")?.as_str().map(str::to_string)
}

/// Speichert ein Ergebnis. Erst in eine Temp-Datei, dann umbenennen: parallele
/// Läufe (`--each -j`) sehen nie eine halb geschriebene Datei.
pub fn cache_store(dir: &Path, key: &str, result: &str, model: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
    let body = json!({"result": result, "model": model});
    std::fs::write(&tmp, body.to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{key}.json")))
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
        assert_eq!(ExitCode::Timeout.code(), 124);
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

    #[test]
    fn attach_files_nennt_die_dateinamen() {
        let files = vec![
            ("a.txt".to_string(), "eins\n".to_string()),
            ("b.txt".to_string(), "zwei".to_string()),
        ];
        let t = attach_files("Vergleiche", &files);
        assert_eq!(
            t,
            "Vergleiche\n\n--- Datei: a.txt ---\neins\n\n--- Datei: b.txt ---\nzwei"
        );
        // Ohne Prompt beginnen die Blöcke direkt.
        assert!(attach_files("", &files).starts_with("--- Datei: a.txt"));
        assert_eq!(attach_files("nur", &[]), "nur");
    }

    #[test]
    fn expand_template_ersetzt_oder_haengt_an() {
        assert_eq!(
            expand_template("Fasse {} zusammen", "a.rs"),
            "Fasse a.rs zusammen"
        );
        let t = expand_template("Klassifiziere", "ticket 1");
        assert!(t.starts_with("Klassifiziere") && t.ends_with("ticket 1"));
    }

    #[test]
    fn check_verdict_liest_urteil() {
        let v = json!({"ergebnis": false, "begruendung": "enthält einen Key"});
        assert_eq!(
            check_verdict(&v),
            Some((false, "enthält einen Key".to_string()))
        );
        assert_eq!(check_verdict(&json!({"ok": true})), None);
        // Das eigene Schema ist anbieter-tauglich — sonst gäbe es keine
        // native Erzwingung für --check.
        assert!(crate::schema::native_compatible(&check_schema()));
    }

    #[test]
    fn cache_key_ist_stabil_und_trennscharf() {
        let k = cache_key(&["modell", "auftrag"]);
        assert_eq!(k.len(), 32);
        assert_eq!(k, cache_key(&["modell", "auftrag"]));
        assert_ne!(cache_key(&["ab", "c"]), cache_key(&["a", "bc"]));
        assert_ne!(k, cache_key(&["modell", "auftrag "]));
    }

    #[test]
    fn cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agentkit_cache_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(cache_load(&dir, "k"), None);
        cache_store(&dir, "k", "antwort", "demo").unwrap();
        assert_eq!(cache_load(&dir, "k").as_deref(), Some("antwort"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
