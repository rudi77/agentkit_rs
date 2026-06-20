//! Coding-Tools — was ein Coding-Agent braucht: Dateien lesen/schreiben/ändern,
//! Verzeichnisse listen und Shell-Befehle ausführen. Mit zwei Sicherheitsnetzen:
//!
//! 1. **Sandbox**: Alle Pfade werden in einen Workspace-Ordner eingesperrt.
//! 2. **Approval**: Vor jeder Shell-Ausführung wird (per Callback) um Erlaubnis gefragt.
//!
//! `run_shell` ist plattformübergreifend: PowerShell auf Windows, sonst bash.

use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

pub const CODING_SYSTEM: &str =
    "Du bist ein Coding-Agent und arbeitest ausschließlich in der Sandbox. \
Plane deine Arbeit mit update_plan. Schreibe Code mit write_file/edit_file, \
führe ihn mit run_shell aus und teste mit pytest. Schlägt ein Test fehl, lies \
die Fehlermeldung, korrigiere den Code und versuche es erneut. Erkläre am Ende \
kurz, was du gebaut hast.";

type ApproveFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

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

    pub fn read_file(&self, path: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        std::fs::read_to_string(&p).map_err(|e| e.to_string())
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
                Ok(full.chars().take(4000).collect())
            }
            Ok(None) => Ok(format!(
                "ERROR: Timeout nach {}s.",
                self.inner.shell_timeout
            )),
            Err(e) => Err(e),
        }
    }

    pub fn register(&self, registry: &mut ToolRegistry) {
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
    CodingTools::new(workspace, approval).register(&mut registry);
    registry
}
