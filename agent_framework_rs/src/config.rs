//! Benutzer-Konfiguration: `~/.agentkit/config.json`.
//!
//! Die Executable soll nach der Installation *ohne* Projekt-`.env` und ohne manuell
//! gesetzte Umgebungsvariablen laufen — der Anwender trägt seine Azure-Werte einmal in
//! eine JSON-Datei im Benutzerverzeichnis ein, und zwar dort, wo `agentkit_setup.ps1`
//! sie anlegt.
//!
//! Umgesetzt ist das als **Env-Quelle mit der niedrigsten Priorität**: die Datei wird
//! auf `AZURE_OPENAI_*` / `OPENAI_*` abgebildet und setzt nur, was noch nicht gesetzt
//! ist. Damit bleibt der Rest des Codes unverändert (`azure_from_env` & Co. lesen
//! weiter aus der Umgebung) und die Rangfolge ist die erwartete:
//!
//! ```text
//! echte Umgebungsvariable  >  .env im Arbeitsverzeichnis  >  ~/.agentkit/config.json
//! ```
//!
//! Platzhalter (leerer Wert oder `<…>`) werden **nicht** gesetzt: eine frisch angelegte
//! Config mit `"api_key": "<HIER-EINTRAGEN>"` führt so zum sauberen Demo-Fallback statt
//! zu einem 401 vom Endpunkt.

use std::path::PathBuf;

use serde_json::Value;

/// Vorlage, die `agentkit config init` (und das Setup-Skript) schreibt. Der Anwender
/// muss nur noch die Azure-Werte eintragen.
pub const CONFIG_TEMPLATE: &str = r#"{
  "//": "agentkit-Konfiguration. Trage unten deine Azure-OpenAI-Werte ein.",
  "//provider": "auto | azure | openai | demo  (auto: Azure, sonst OpenAI, sonst Demo)",
  "provider": "auto",

  "azure": {
    "endpoint": "https://<DEINE-RESSOURCE>.openai.azure.com",
    "api_key": "<DEIN-AZURE-API-KEY>",
    "deployment": "<DEIN-DEPLOYMENT-NAME>",
    "api_version": "2024-10-21"
  },

  "//openai": "base_url fuer lokale OpenAI-kompatible Server (Ollama, LM Studio, vLLM), z. B. http://localhost:11434/v1 — api_key darf dann leer bleiben.",
  "openai": {
    "api_key": "",
    "model": "gpt-4o-mini",
    "base_url": ""
  },

  "//env": "Beliebige weitere Umgebungsvariablen fuer agentkit und MCP-Server.",
  "env": {}
}
"#;

/// Abbildung `config.json`-Pfad -> Umgebungsvariable. Reihenfolge = Anzeigereihenfolge
/// in `agentkit config show`.
const MAPPING: &[(&str, &str, &str)] = &[
    ("azure", "endpoint", "AZURE_OPENAI_ENDPOINT"),
    ("azure", "api_key", "AZURE_OPENAI_API_KEY"),
    ("azure", "deployment", "AZURE_OPENAI_DEPLOYMENT"),
    ("azure", "api_version", "AZURE_OPENAI_API_VERSION"),
    ("openai", "api_key", "OPENAI_API_KEY"),
    ("openai", "model", "OPENAI_MODEL"),
    ("openai", "base_url", "OPENAI_BASE_URL"),
];

/// Das Konfigurationsverzeichnis: `$AGENTKIT_HOME`, sonst `~/.agentkit`
/// (Windows: `%USERPROFILE%\.agentkit`).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENTKIT_HOME") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".agentkit"))
}

/// Pfad der Konfigurationsdatei (`<config_dir>/config.json`).
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.json"))
}

/// Ist der Wert ein unausgefüllter Platzhalter? Leere Strings zählen als "nicht
/// konfiguriert" — und alles, was noch spitze Klammern trägt. Die stehen auch *mitten*
/// im Wert (`https://<DEINE-RESSOURCE>.openai.azure.com`), deshalb reicht ein Test auf
/// Präfix/Suffix nicht: sonst würde eine frische Vorlage einen kaputten Endpoint setzen
/// und der Anwender bekäme einen Netzwerkfehler statt des Demo-Fallbacks.
fn is_placeholder(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || (v.contains('<') && v.contains('>'))
}

/// Bildet den geparsten Config-Wert auf Umgebungsvariablen ab (reine Funktion — genau
/// das, was [`load_user_config`] anschließend in die Umgebung schreibt).
///
/// Berücksichtigt `azure.*`, `openai.*`, den freien `env`-Block und `provider`
/// (-> `AGENTKIT_PROVIDER`). Platzhalter und Nicht-Strings werden übersprungen.
pub fn config_env_pairs(cfg: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (section, key, var) in MAPPING {
        if let Some(v) = cfg[section][key].as_str() {
            if !is_placeholder(v) {
                out.push((var.to_string(), v.trim().to_string()));
            }
        }
    }
    if let Some(p) = cfg["provider"].as_str() {
        if !is_placeholder(p) && p != "auto" {
            out.push(("AGENTKIT_PROVIDER".to_string(), p.trim().to_string()));
        }
    }
    if let Some(env) = cfg["env"].as_object() {
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                if !is_placeholder(v) {
                    out.push((k.clone(), v.to_string()));
                }
            }
        }
    }
    out
}

/// Lädt `~/.agentkit/config.json` und setzt daraus alle Variablen, die **noch nicht**
/// gesetzt sind (wie [`crate::load_dotenv`], nur eine Ebene tiefer in der Rangfolge).
///
/// Gibt den geladenen Pfad zurück (bzw. `None`, wenn es keine Datei gibt). Ein Syntax-
/// fehler in der Datei ist nicht fatal, wird aber auf stderr gemeldet — sonst rätselt
/// der Anwender, warum sein Key ignoriert wird.
pub fn load_user_config() -> Option<PathBuf> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(cfg) => {
            for (k, v) in config_env_pairs(&cfg) {
                if std::env::var_os(&k).is_none() {
                    std::env::set_var(&k, &v);
                }
            }
            Some(path)
        }
        Err(e) => {
            eprintln!("[WARN] {} ist kein gültiges JSON: {e}", path.display());
            None
        }
    }
}

/// Schreibt die Vorlage nach `~/.agentkit/config.json` — legt das Verzeichnis an und
/// überschreibt eine vorhandene Datei **nicht** (`Ok(false)` = war schon da).
pub fn init_user_config() -> Result<(PathBuf, bool), String> {
    let path = config_path().ok_or("kein Benutzerverzeichnis gefunden (USERPROFILE/HOME)")?;
    if path.exists() {
        return Ok((path, false));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, CONFIG_TEMPLATE).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, true))
}

/// Zeilen für `agentkit config show`: pro Variable Herkunft und (maskierter) Wert.
/// Keys werden nie im Klartext ausgegeben — die Ausgabe soll teilbar sein.
pub fn config_status() -> Vec<String> {
    let mut lines = Vec::new();
    for (_, _, var) in MAPPING {
        let shown = match std::env::var(var) {
            Ok(v) if !v.is_empty() => mask(var, &v),
            _ => "— (nicht gesetzt)".to_string(),
        };
        lines.push(format!("{var:<26} {shown}"));
    }
    lines
}

/// Maskiert Geheimnisse: von einem `*_KEY` bleiben nur die letzten vier Zeichen stehen.
fn mask(var: &str, val: &str) -> String {
    if !var.ends_with("_KEY") {
        return val.to_string();
    }
    let n = val.chars().count();
    if n <= 4 {
        return "****".to_string();
    }
    let tail: String = val.chars().skip(n - 4).collect();
    format!("{}{tail} ({n} Zeichen)", "*".repeat(8))
}
