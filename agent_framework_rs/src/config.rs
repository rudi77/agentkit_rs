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
//!
//! Nach demselben Muster trägt der Top-Level-Schlüssel `allow` eine dauerhafte
//! Allowlist für `run_shell`: Programme (erstes Wort des Befehls), die künftige Läufe
//! ohne Rückfrage ausführen dürfen. Er wird auf `AGENTKIT_ALLOW` (kommagetrennt)
//! abgebildet — genau wie `provider` auf `AGENTKIT_PROVIDER` — und ist damit ebenso als
//! echte Umgebungsvariable nutzbar (nicht aus einer `.env`: die gehört zum Projekt und
//! darf keine Rückfragen abschalten, siehe [`crate::load_dotenv`]). Bei verketteten
//! Befehlen muss jeder Teilbefehl freigegeben sein ([`ohne_rueckfrage`]).
//! [`add_allow_entry`] pflegt diese
//! Liste programmatisch (z. B. aus der `[d]auerhaft`-Antwort der Shell-Rückfrage).

use std::path::PathBuf;

use serde_json::Value;

/// Vorlage, die `agentkit config init` (und das Setup-Skript) schreibt. Der Anwender
/// muss nur noch die Azure-Werte eintragen.
pub const CONFIG_TEMPLATE: &str = r#"{
  "//": "agentkit-Konfiguration. Trage unten deine Azure-OpenAI-Werte ein.",
  "//provider": "auto | azure | openai | anthropic | demo  (auto: Azure, sonst OpenAI, sonst Anthropic, sonst Demo)",
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

  "//anthropic": "Claude ueber die native Messages API (Prompt-Caching inklusive). model leer = claude-opus-5.",
  "anthropic": {
    "api_key": "",
    "model": ""
  },

  "//env": "Beliebige weitere Umgebungsvariablen fuer agentkit und MCP-Server.",
  "env": {},

  "//allow": "Programme, die run_shell ohne Rueckfrage ausfuehren darf, z. B. [\"docker\", \"git\", \"ls\"]. Geprueft wird das ERSTE WORT des Befehls.",
  "allow": []
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
    ("anthropic", "api_key", "ANTHROPIC_API_KEY"),
    ("anthropic", "model", "ANTHROPIC_MODEL"),
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

/// Pfad der REPL-History (`<config_dir>/history`). Das Verzeichnis wird dabei
/// angelegt — bei einer frischen Installation ohne `agentkit config init`
/// existiert es sonst nicht und das Speichern schlüge fehl.
pub fn history_path() -> Option<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("history"))
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
/// Berücksichtigt `azure.*`, `openai.*`, `anthropic.*`, den freien `env`-Block und `provider`
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
    if let Some(allow) = cfg["allow"].as_array() {
        let namen: Vec<&str> = allow
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !is_placeholder(s))
            .collect();
        if !namen.is_empty() {
            out.push(("AGENTKIT_ALLOW".to_string(), namen.join(",")));
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
                    let umgebung = |n: &str| std::env::var(n).ok();
                    if let Some(url) = fremder_endpunkt(&k, crate::app::aus_dotenv, umgebung) {
                        eprintln!(
                            "[WARN] {k} aus {} wird NICHT verwendet: der Endpunkt {url} \
                             kommt aus der .env des Arbeitsverzeichnisses, und dein Key \
                             ginge sonst an eine Adresse, die das Projekt bestimmt. Wenn \
                             das gewollt ist: den Key neben den Endpunkt in die .env \
                             schreiben oder {k} als Umgebungsvariable setzen.",
                            path.display()
                        );
                        continue;
                    }
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

/// Endpunkt-Variable -> Variable des Keys, der an diesen Endpunkt geschickt wird.
const ENDPUNKT_KEY: &[(&str, &str)] = &[
    ("AZURE_OPENAI_ENDPOINT", "AZURE_OPENAI_API_KEY"),
    ("OPENAI_BASE_URL", "OPENAI_API_KEY"),
    ("ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY"),
];

/// Der Endpunkt, an den `key_var` aus der Benutzer-Config NICHT gehen darf —
/// `None`, wenn nichts dagegen spricht.
///
/// Die `.env` des Arbeitsverzeichnisses gehört zum Projekt. Setzt sie nur den
/// Endpunkt, ergänzte die Config bisher den Key dazu: ein geklontes Repo mit
/// `OPENAI_BASE_URL=https://fremd.example` bekam so den Key des Anwenders
/// geschickt. Lokale Adressen bleiben ausgenommen — `.env` mit
/// `OPENAI_BASE_URL=http://localhost:11434/v1` (Ollama, LM Studio) ist der
/// übliche, harmlose Fall.
pub(crate) fn fremder_endpunkt(
    key_var: &str,
    aus_dotenv: impl Fn(&str) -> bool,
    umgebung: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let (endpunkt_var, _) = ENDPUNKT_KEY.iter().find(|(_, k)| *k == key_var)?;
    if !aus_dotenv(endpunkt_var) {
        return None;
    }
    umgebung(endpunkt_var).filter(|url| !ist_lokal(url))
}

/// Zeigt die URL auf diesen Rechner (`localhost`, `127.0.0.1`, `::1`)?
fn ist_lokal(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Hinter dem LETZTEN `@`: `http://localhost@fremd.example` meint fremd.example.
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host_port.split(':').next().unwrap_or(""),
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
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

/// Trägt `prog` dauerhaft in die `allow`-Liste von `~/.agentkit/config.json` ein.
/// Gibt `(Pfad, true)` zurück, wenn der Eintrag neu war.
///
/// Der Round-Trip über `serde_json::Value` sortiert die Schlüssel alphabetisch neu
/// (serde_json ohne `preserve_order`), der Inhalt inklusive der `//`-Kommentarschlüssel
/// bleibt dabei aber vollständig erhalten.
pub fn add_allow_entry(prog: &str) -> Result<(PathBuf, bool), String> {
    let path = config_path().ok_or("kein Benutzerverzeichnis gefunden (USERPROFILE/HOME)")?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, CONFIG_TEMPLATE).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut cfg: Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} ist kein gültiges JSON: {e}", path.display()))?;
    // Nur ein Objekt kennt einen `allow`-Schlüssel — `Value`s `IndexMut` würde bei jedem
    // anderen Wurzeltyp (Array, String, Zahl, Bool) panicken statt `Null` zu liefern.
    if !cfg.is_object() {
        return Err(format!(
            "{} hat kein JSON-Objekt als Wurzel — von Hand reparieren",
            path.display()
        ));
    }
    if !cfg["allow"].is_array() {
        cfg["allow"] = Value::Array(Vec::new());
    }
    let allow = cfg["allow"]
        .as_array_mut()
        .expect("gerade auf Array gesetzt");
    let schon_drin = allow.iter().any(|v| v.as_str() == Some(prog));
    if !schon_drin {
        allow.push(Value::String(prog.to_string()));
    }
    let neu = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    write_atomic(&path, &neu).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, !schon_drin))
}

/// Schlüssel in `config.json`: die bestätigten `allow`-Listen aus Projekt-
/// `AGENTS.md`-Dateien, je ein [`project_allow_key`].
const TRUSTED_ALLOW: &str = "trusted_project_allow";

/// Der Merker für „diese `allow`-Liste in diesem Workspace ist bestätigt".
///
/// Workspace UND Liste gehen ein: dieselbe Liste in einem anderen Verzeichnis
/// ist ein anderes Projekt, und eine geänderte Liste — auch eine, die der Agent
/// sich selbst in die `AGENTS.md` geschrieben hat — ist nicht die, der jemand
/// zugestimmt hat. Sortiert und ohne Duplikate, damit bloßes Umstellen keine
/// neue Frage auslöst.
pub fn project_allow_key(workspace: &std::path::Path, allow: &[String]) -> String {
    let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let ws = ws.to_string_lossy();
    let mut teile: Vec<&str> = allow.iter().map(String::as_str).collect();
    teile.sort_unstable();
    teile.dedup();
    teile.insert(0, &ws);
    crate::cli::cache_key(&teile)
}

/// Steht `key` unter den bestätigten Projekt-`allow`-Listen in `config`?
/// Eine fehlende oder kaputte Datei heißt „nicht bestätigt".
pub fn project_allow_trusted(config: &std::path::Path, key: &str) -> bool {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|cfg| cfg.get(TRUSTED_ALLOW)?.as_array().cloned())
        .is_some_and(|liste| liste.iter().any(|v| v.as_str() == Some(key)))
}

/// Merkt `key` in `config` als bestätigt (legt die Datei bei Bedarf aus der
/// Vorlage an, wie [`add_allow_entry`]).
pub fn trust_project_allow(config: &std::path::Path, key: &str) -> Result<(), String> {
    let fehler = |e: &dyn std::fmt::Display| format!("{}: {e}", config.display());
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(_) => {
            if let Some(dir) = config.parent() {
                std::fs::create_dir_all(dir).map_err(|e| fehler(&e))?;
            }
            CONFIG_TEMPLATE.to_string()
        }
    };
    let mut cfg: Value =
        serde_json::from_str(&text).map_err(|e| fehler(&format!("kein gültiges JSON: {e}")))?;
    let Some(wurzel) = cfg.as_object_mut() else {
        return Err(fehler(&"kein JSON-Objekt als Wurzel — von Hand reparieren"));
    };
    let liste = wurzel
        .entry(TRUSTED_ALLOW)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(liste) = liste.as_array_mut() else {
        return Err(fehler(&format!("'{TRUSTED_ALLOW}' ist keine Liste")));
    };
    if !liste.iter().any(|v| v.as_str() == Some(key)) {
        liste.push(Value::String(key.to_string()));
    }
    let neu = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    write_atomic(config, &neu).map_err(|e| fehler(&e))
}

/// Schreibt `content` so nach `path`, dass dort nie eine halbe Datei steht:
/// erst in eine Temp-Datei im selben Verzeichnis, dann `rename`.
///
/// Ein direktes `fs::write` kürzt das Ziel zuerst auf null — ein Kill oder
/// Absturz mitten im Schreiben hinterließ abgeschnittenes JSON, und die
/// Sitzung bzw. die Config (mit den Zugangsdaten) war danach nicht mehr ladbar.
/// Dasselbe Verzeichnis, weil `rename` nur innerhalb eines Dateisystems
/// atomar ist.
pub(crate) fn write_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    // Einem Symlink folgen (Config aus einem Dotfiles-Repo): `rename` ersetzte
    // sonst den Link selbst durch eine gewöhnliche Datei.
    let ziel = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut name = ziel.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = ziel.with_file_name(name);
    let schreiben = || {
        std::fs::write(&tmp, content)?;
        // Die Rechte der bestehenden Datei übernehmen — eine auf 0600 gesetzte
        // Config darf durch das Umbenennen nicht wieder für alle lesbar werden.
        if let Ok(meta) = std::fs::metadata(&ziel) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &ziel)
    };
    let ergebnis = schreiben();
    if ergebnis.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    ergebnis
}

/// Das erste Wort eines Shell-Befehls — der Schlüssel jeder Freigabe-Regel.
pub fn shell_programm(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or("")
}

/// Die ersten Wörter ALLER Teilbefehle einer Kette (`cd x && cargo build` ->
/// `cd`, `cargo`), ohne Duplikate — das, was „immer erlauben" sich merkt.
pub fn shell_programme(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for teil in crate::coding::segmente(command) {
        let prog = shell_programm(&teil).to_string();
        if !out.contains(&prog) {
            out.push(prog);
        }
    }
    out
}

/// Darf der Befehl nach der Programm-Allowlist ohne Rückfrage laufen?
///
/// Die eine Stelle für CLI, TUI und ACP. Geprüft wird das erste Wort JEDES
/// Teilbefehls: nur das allererste zu prüfen gab mit einem freigegebenen `cd`
/// oder `git` auch `cd x && rm -rf ~` frei. Mit Kommandosubstitution wird immer
/// gefragt (siehe [`crate::coding::freigabe_segmente`]).
pub fn ohne_rueckfrage(command: &str, erlaubt: impl Fn(&str) -> bool) -> bool {
    crate::coding::freigabe_segmente(command)
        .is_some_and(|teile| teile.iter().all(|t| erlaubt(shell_programm(t))))
}

/// Zerlegt einen kommagetrennten `AGENTKIT_ALLOW`-Wert in Programmnamen
/// (erstes Wort je Eintrag, Leerraum und Leereinträge fallen weg).
pub fn allow_aus_liste(wert: &str) -> std::collections::BTreeSet<String> {
    wert.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| shell_programm(s).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Die dauerhafte Allowlist aus der Umgebung (`AGENTKIT_ALLOW`), leer wenn ungesetzt.
pub fn allow_liste() -> std::collections::BTreeSet<String> {
    std::env::var("AGENTKIT_ALLOW")
        .map(|v| allow_aus_liste(&v))
        .unwrap_or_default()
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
    // Die dauerhafte Shell-Allowlist gehört hier hin, obwohl sie kein Zugangswert ist:
    // sie ist eine stehende Erlaubnis, und `agentkit config show` ist die Stelle, an der
    // man nachsieht, was die Umgebung gerade wirklich erlaubt.
    let allow = match std::env::var("AGENTKIT_ALLOW") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => "— (nicht gesetzt)".to_string(),
    };
    lines.push(format!("{:<26} {allow}", "AGENTKIT_ALLOW"));
    // Hooks führen Code ohne Rückfrage aus — auch sie gehören zu dem, was die
    // Umgebung gerade erlaubt.
    let hooks = crate::Hooks::from_env();
    let hooks = if hooks.is_empty() {
        "— (keine)".to_string()
    } else {
        format!("{} aktiv", hooks.len())
    };
    lines.push(format!("{:<26} {hooks}", "Hooks"));
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
