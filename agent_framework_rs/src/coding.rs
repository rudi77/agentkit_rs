//! Coding-Tools — was ein Coding-Agent braucht: Dateien lesen/schreiben/ändern,
//! Verzeichnisse listen/durchsuchen und Shell-Befehle ausführen. Mit zwei
//! Sicherheitsnetzen:
//!
//! 1. **Sandbox**: Alle Pfade werden in einen Workspace-Ordner eingesperrt.
//! 2. **Approval**: Vor jeder Shell-Ausführung wird (per Callback) um Erlaubnis gefragt.
//!
//! `run_shell` ist plattformübergreifend: PowerShell auf Windows, sonst bash.
//!
//! `glob_files` und `grep` sind read-only Such-Tools (Datei-Glob bzw. Inhalts-Regex)
//! und Teil der [`READ_ONLY_TOOLS`]-Teilmenge, die read-only-Sub-Agenten-Rollen
//! bekommen (siehe `roles.rs`).

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Read-only-Teilmenge der Coding-Tools (kein write/edit/run_shell). Praktisch für
/// Sub-Agenten-Rollen, die nur erkunden oder begutachten dürfen (siehe `roles.rs`).
pub const READ_ONLY_TOOLS: &[&str] = &["list_files", "glob_files", "grep", "read_file"];

/// Ordner, die bei Suche/Glob übersprungen werden (Rauschen statt Code).
const IGNORE: &[&str] = &[
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    "node_modules",
    ".mypy_cache",
    ".pytest_cache",
    ".idea",
    ".vscode",
];

pub const CODING_SYSTEM: &str =
    "Du bist ein Coding-Agent und arbeitest im aktuellen Projektverzeichnis \
(deine Sandbox; Pfade außerhalb sind gesperrt). Verschaffe dir zuerst mit \
list_files/glob_files/grep/read_file einen Überblick über den vorhandenen Code, \
bevor du ihn änderst (glob_files findet Dateien, grep durchsucht Inhalte — beide \
read-only). Plane deine Arbeit mit update_plan. Schreibe Code mit write_file/edit_file, \
führe ihn mit run_shell aus und teste mit pytest. Schlägt ein Test fehl, lies \
die Fehlermeldung, korrigiere den Code und versuche es erneut. Erkläre am Ende \
kurz, was du gebaut hast.";

/// Approve-Callback für `run_shell`: bekommt den Befehl, gibt `true` zum Ausführen.
pub type ApproveFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct Inner {
    workspace: PathBuf,
    approval: bool,
    shell_timeout: u64,
    approve: ApproveFn,
}

/// Registriert Coding-Tools (sandboxed) in einer ToolRegistry.
#[derive(Clone)]
pub struct CodingTools {
    inner: Arc<Inner>,
}

impl CodingTools {
    pub fn new(workspace: &str, approval: bool) -> Self {
        Self::with_approve(workspace, approval, Arc::new(default_approve), 120)
    }

    pub fn with_approve(
        workspace: &str,
        approval: bool,
        approve: ApproveFn,
        shell_timeout: u64,
    ) -> Self {
        let ws = PathBuf::from(workspace);
        std::fs::create_dir_all(&ws).ok();
        let workspace = ws.canonicalize().unwrap_or(ws);
        CodingTools {
            inner: Arc::new(Inner {
                workspace,
                approval,
                shell_timeout,
                approve,
            }),
        }
    }

    /// Sperrt einen Pfad in die Sandbox ein.
    fn safe(&self, path: &str) -> Result<PathBuf, String> {
        let ws = &self.inner.workspace;
        let joined = ws.join(path);
        // Pfad normalisieren (auch wenn er noch nicht existiert).
        let resolved = normalize(&joined);
        if resolved == *ws || resolved.starts_with(ws) {
            Ok(resolved)
        } else {
            Err(format!("Pfad außerhalb der Sandbox: {path}"))
        }
    }

    pub fn list_files(&self, path: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        let mut names: Vec<String> = std::fs::read_dir(&p)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(if names.is_empty() {
            "(leer)".to_string()
        } else {
            names.join("\n")
        })
    }

    /// Findet Dateien per Glob-Muster (z. B. `**/*.py`) relativ zum Verzeichnis.
    /// Read-only; Ignore-Ordner (`.git`, `node_modules`, …) werden übersprungen.
    pub fn glob_files(&self, pattern: &str, path: &str, limit: usize) -> Result<String, String> {
        let root = self.safe(path)?;
        let ws = &self.inner.workspace;
        let mut matches: Vec<String> = Vec::new();
        for file in walk_files(&root) {
            // Glob-Muster matcht relativ zum Start-Verzeichnis (wie Pythons
            // `root.glob(pattern)`); angezeigt wird der Pfad relativ zum Workspace.
            let rel_root = rel_str(&file, &root);
            if glob_match(pattern, &rel_root) {
                matches.push(rel_str(&file, ws));
            }
        }
        matches.sort();
        if matches.is_empty() {
            return Ok("(keine Treffer)".to_string());
        }
        let extra = matches.len().saturating_sub(limit);
        matches.truncate(limit);
        let mut out = matches.join("\n");
        if extra > 0 {
            out.push_str(&format!("\n…(+{extra} weitere)"));
        }
        Ok(out)
    }

    /// Durchsucht Dateiinhalte per Regex; liefert `pfad:zeile: text` je Treffer.
    pub fn grep(&self, pattern: &str, path: &str, glob: &str, limit: usize) -> Result<String, String> {
        let rx = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(format!("ERROR: ungültiges Regex: {e}")),
        };
        let root = self.safe(path)?;
        let ws = &self.inner.workspace;
        let mut files = walk_files(&root);
        files.sort();
        let mut hits: Vec<String> = Vec::new();
        for file in &files {
            if !glob_match(glob, &rel_str(file, &root)) {
                continue;
            }
            let Ok(bytes) = std::fs::read(file) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            let rel = rel_str(file, ws);
            for (i, line) in text.lines().enumerate() {
                if rx.is_match(line) {
                    let snippet: String = line.trim().chars().take(200).collect();
                    hits.push(format!("{rel}:{}: {snippet}", i + 1));
                    if hits.len() >= limit {
                        return Ok(format!(
                            "{}\n…(abgeschnitten bei {limit} Treffern)",
                            hits.join("\n")
                        ));
                    }
                }
            }
        }
        Ok(if hits.is_empty() {
            "(keine Treffer)".to_string()
        } else {
            hits.join("\n")
        })
    }

    pub fn read_file(&self, path: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        // Wie Python (`errors="replace"`): ungültiges UTF-8 nicht als Fehler werten.
        let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, content).map_err(|e| e.to_string())?;
        Ok(format!(
            "{} Zeichen nach {path} geschrieben.",
            content.chars().count()
        ))
    }

    pub fn edit_file(&self, path: &str, old: &str, new: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
        let count = text.matches(old).count();
        let snippet: String = old.chars().take(50).collect();
        if count == 0 {
            return Ok(format!("ERROR: '{snippet}…' nicht in {path} gefunden."));
        }
        if count > 1 {
            return Ok(format!(
                "ERROR: '{snippet}…' kommt {count}× vor — bitte eindeutiger machen."
            ));
        }
        std::fs::write(&p, text.replacen(old, new, 1)).map_err(|e| e.to_string())?;
        Ok(format!("{path} geändert."))
    }

    pub fn run_shell(&self, command: &str) -> Result<String, String> {
        if self.inner.approval && !(self.inner.approve)(command) {
            return Ok("ABGELEHNT vom Benutzer.".to_string());
        }
        // Plattformübergreifend: PowerShell auf Windows, sonst bash.
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", command]);
            c
        } else {
            let mut c = Command::new("bash");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&self.inner.workspace);

        // Einfacher Timeout über einen Watcher-Thread.
        let output = run_with_timeout(cmd, self.inner.shell_timeout);
        match output {
            Ok(Some(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let code = out.status.code().unwrap_or(-1);
                let full =
                    format!("exit={code}\n--- STDOUT ---\n{stdout}\n--- STDERR ---\n{stderr}");
                // Großzügig kappen; die feinere Grenze setzt der Agent über TRUNCATE_LIMIT.
                Ok(full.chars().take(16000).collect())
            }
            Ok(None) => Ok(format!("ERROR: Timeout nach {}s.", self.inner.shell_timeout)),
            Err(e) => Err(e),
        }
    }

    /// Registriert die Coding-Tools in `registry`.
    ///
    /// `only` beschränkt auf die genannten Tool-Namen (z. B. [`READ_ONLY_TOOLS`]
    /// für eine read-only-Sub-Agenten-Rolle); `None` = alle Tools.
    pub fn register(&self, registry: &mut ToolRegistry, only: Option<&[&str]>) {
        let want = |name: &str| only.map_or(true, |o| o.contains(&name));

        if want("list_files") {
            let me = self.clone();
            registry.add(
                "list_files",
                "Listet Dateien in einem Verzeichnis der Sandbox.",
                json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": []}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                    me.list_files(path)
                },
            );
        }
        if want("glob_files") {
            let me = self.clone();
            registry.add(
                "glob_files",
                "Findet Dateien per Glob-Muster (z. B. '**/*.py'). Read-only, keine Rückfrage.",
                json!({"type": "object", "properties": {
                    "pattern": {"type": "string", "description": "Glob-Muster, z. B. '**/*.py' oder 'src/*.ts'."},
                    "path": {"type": "string", "description": "Startverzeichnis (Default '.')."}},
                 "required": ["pattern"]}),
                move |args: Value| {
                    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("**/*");
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                    me.glob_files(pattern, path, 200)
                },
            );
        }
        if want("grep") {
            let me = self.clone();
            registry.add(
                "grep",
                "Durchsucht Dateiinhalte per Regex und gibt 'pfad:zeile: text' zurück. Read-only.",
                json!({"type": "object", "properties": {
                    "pattern": {"type": "string", "description": "Regex-Suchmuster."},
                    "path": {"type": "string", "description": "Startverzeichnis (Default '.')."},
                    "glob": {"type": "string", "description": "Auf diese Dateien beschränken, z. B. '**/*.py'."}},
                 "required": ["pattern"]}),
                move |args: Value| {
                    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                    let glob = args.get("glob").and_then(Value::as_str).unwrap_or("**/*");
                    me.grep(pattern, path, glob, 200)
                },
            );
        }
        if want("read_file") {
            let me = self.clone();
            registry.add(
                "read_file",
                "Liest eine Datei aus der Sandbox.",
                json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    me.read_file(path)
                },
            );
        }
        if want("write_file") {
            let me = self.clone();
            registry.add(
                "write_file",
                "Schreibt Text in eine Datei in der Sandbox.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"}, "content": {"type": "string"}},
                 "required": ["path", "content"]}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    me.write_file(path, content)
                },
            );
        }
        if want("edit_file") {
            let me = self.clone();
            registry.add(
                "edit_file",
                "Ersetzt einen eindeutigen Textabschnitt in einer Datei.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Zu ersetzender Text (eindeutig)."},
                    "new": {"type": "string", "description": "Neuer Text."}},
                 "required": ["path", "old", "new"]}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    let old = args.get("old").and_then(Value::as_str).unwrap_or("");
                    let new = args.get("new").and_then(Value::as_str).unwrap_or("");
                    me.edit_file(path, old, new)
                },
            );
        }
        if want("run_shell") {
            let me = self.clone();
            registry.add(
                "run_shell",
                "Führt einen Shell-Befehl in der Sandbox aus (z. B. 'python ...', 'pytest').",
                json!({"type": "object", "properties": {"command": {"type": "string"}},
                       "required": ["command"]}),
                move |args: Value| {
                    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                    me.run_shell(command)
                },
            );
        }
    }
}

fn default_approve(command: &str) -> bool {
    use std::io::{self, Write};
    print!("\n⚠️  Shell ausführen?\n  {command}\n[j/N] ");
    io::stdout().flush().ok();
    let mut ans = String::new();
    if io::stdin().read_line(&mut ans).is_err() {
        return false;
    }
    matches!(ans.trim().to_lowercase().as_str(), "j" | "ja" | "y" | "yes")
}

/// Normalisiert einen Pfad (löst `.`/`..` auf), ohne dass er existieren muss.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Pfad relativ zu `base`, als String mit `/`-Trennern (wie Python `replace(os.sep, "/")`).
fn rel_str(p: &Path, base: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// Sammelt alle Dateien unter `root` rekursiv; steigt nicht in Ignore-Ordner ab.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if IGNORE.contains(&name) {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// Glob-Match (pathlib-Semantik): `**` matcht null+ Pfadsegmente, `*` beliebige
/// Zeichen innerhalb eines Segments, `?` genau ein Zeichen.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segs(&pat, &seg)
}

fn match_segs(pat: &[&str], seg: &[&str]) -> bool {
    let Some(first) = pat.first() else {
        return seg.is_empty();
    };
    if *first == "**" {
        // `**` matcht null oder mehr Segmente.
        (0..=seg.len()).any(|i| match_segs(&pat[1..], &seg[i..]))
    } else if let Some(s0) = seg.first() {
        seg_match(first, s0) && match_segs(&pat[1..], &seg[1..])
    } else {
        false
    }
}

/// Glob innerhalb EINES Pfadsegments: `*` = beliebig viele Zeichen, `?` = eines.
fn seg_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let c: Vec<char> = s.chars().collect();
    fn go(p: &[char], c: &[char]) -> bool {
        match p.first() {
            None => c.is_empty(),
            Some('*') => (0..=c.len()).any(|i| go(&p[1..], &c[i..])),
            Some('?') => !c.is_empty() && go(&p[1..], &c[1..]),
            Some(ch) => !c.is_empty() && c[0] == *ch && go(&p[1..], &c[1..]),
        }
    }
    go(&p, &c)
}

/// Führt ein Kommando mit Timeout aus. `Ok(None)` = Timeout.
fn run_with_timeout(
    mut cmd: Command,
    timeout_secs: u64,
) -> Result<Option<std::process::Output>, String> {
    use std::sync::mpsc;
    use std::time::Duration;

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd.spawn().map_err(|e| e.to_string())?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = child.wait_with_output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(out)) => Ok(Some(out)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Ok(None), // Timeout (Kindprozess läuft im Hintergrund aus)
    }
}

/// Bequemer Helfer: registriert die Coding-Tools in einer (neuen) ToolRegistry.
pub fn coding_tools(
    registry: Option<ToolRegistry>,
    workspace: &str,
    approval: bool,
) -> ToolRegistry {
    let mut registry = registry.unwrap_or_default();
    CodingTools::new(workspace, approval).register(&mut registry, None);
    registry
}
