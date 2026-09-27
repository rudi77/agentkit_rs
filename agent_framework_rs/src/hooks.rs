//! Hooks — eigene Shell-Kommandos vor und nach einem Tool-Aufruf.
//!
//! Wofür: Formatter/Linter nach jeder Datei-Änderung (`cargo fmt`, `ruff`),
//! eine Firmen-Policy vor jedem Shell-Befehl, ein Audit-Log. Das Modell muss
//! dafür nichts wissen — die Hooks sitzen in der Tool-Funktion selbst.
//!
//! ```json
//! {
//!   "pre_tool":  [{"matcher": "run_shell", "command": "python3 policy.py"}],
//!   "post_tool": [{"matcher": "write_file|edit_file", "command": "cargo fmt", "timeout": 120}]
//! }
//! ```
//!
//! **Vertrag.** `matcher` ist ein Regex auf den ganzen Tool-Namen (fehlt er,
//! gilt der Hook für alle). Das Kommando läuft in der Shell von `run_shell`
//! (bash bzw. PowerShell) im Workspace und bekommt `AGENTKIT_HOOK_EVENT`
//! (`pre_tool`/`post_tool`), `AGENTKIT_TOOL`, `AGENTKIT_TOOL_ARGS` (JSON) und
//! nach dem Aufruf `AGENTKIT_TOOL_RESULT`. Der Exit-Code entscheidet:
//!
//! - `0` — in Ordnung, nichts passiert.
//! - `2` — **vor** dem Aufruf: das Tool läuft NICHT, der Agent bekommt die
//!   Ausgabe des Hooks als Begründung. **Nach** dem Aufruf: die Ausgabe wird ans
//!   Ergebnis gehängt, damit der Agent reagiert (z. B. Lint-Fehler beheben).
//! - alles andere — der Hook selbst ist kaputt; das Tool läuft trotzdem, und
//!   ein Vermerk im Ergebnis macht es sichtbar. Ein defektes Skript soll keinen
//!   Lauf blockieren, aber auch nicht still verschwinden.
//!
//! Dieselben Codes wie bei Claude Code — vorhandene Hook-Skripte passen.
//!
//! **Wo die Hooks herkommen.** `<config_dir>/hooks.json` (benutzerweit) und die
//! Datei in `AGENTKIT_HOOKS` (`--hooks FILE`). Eine Datei im Repository wird
//! bewusst NICHT automatisch geladen: ein Hook führt Code ohne Rückfrage aus,
//! und ein fremdes Repo darf das nicht einfach durch eine Datei an sich reißen.
//!
//! **Reichweite.** Die Hooks hängen an den Coding-Tools ([`crate::CodingTools`])
//! und erreichen damit jeden, der sie teilt: Haupt-Agent, Sub-Agenten,
//! Schwarm-Mitglieder und Work-Item-Agenten. MCP- und Graph-Tools laufen ohne
//! Hooks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde_json::Value;

use crate::agent::RunHandle;
use crate::coding::{run_with_timeout, shell_command, RunOutcome};
use crate::tools::ToolFn;

/// Umgebungsvariable mit dem Pfad einer zusätzlichen Hook-Datei (`--hooks`).
pub const HOOKS_ENV: &str = "AGENTKIT_HOOKS";

/// Timeout je Hook, wenn die Datei keinen nennt.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// So viel vom Tool-Ergebnis geht in `AGENTKIT_TOOL_RESULT` — Windows begrenzt
/// eine Umgebungsvariable auf 32 767 Zeichen.
const MAX_RESULT_ENV: usize = 8_000;

#[derive(Debug, Clone)]
struct Hook {
    /// `None` = gilt für jedes Tool.
    matcher: Option<Regex>,
    command: String,
    timeout: u64,
}

/// Ausgang eines Hook-Laufs.
enum Outcome {
    Ok,
    /// Exit 2 — mit der Ausgabe des Hooks.
    Block(String),
    /// Der Hook ist selbst gescheitert (anderer Exit, Timeout, nicht startbar).
    Failed(String),
}

/// Die geladenen Hooks. `Default` = keine.
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    pre: Vec<Hook>,
    post: Vec<Hook>,
}

impl Hooks {
    /// Parst eine Hook-Datei (Format siehe Moduldoku).
    pub fn from_json(text: &str) -> Result<Hooks, String> {
        let v: Value =
            serde_json::from_str(text).map_err(|e| format!("kein gültiges JSON: {e}"))?;
        Ok(Hooks {
            pre: parse_list(&v, "pre_tool")?,
            post: parse_list(&v, "post_tool")?,
        })
    }

    /// Lädt die benutzerweite `hooks.json` und die Datei aus [`HOOKS_ENV`].
    /// Eine unlesbare oder fehlerhafte Datei wird auf stderr gemeldet und
    /// übersprungen — ohne Meldung würde man sich über ausbleibende Hooks
    /// wundern, mit Abbruch wäre ein Tippfehler im Skript-Pfad fatal.
    pub fn from_env() -> Hooks {
        let mut hooks = Hooks::default();
        for path in hook_files() {
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| Hooks::from_json(&t))
            {
                Ok(h) => {
                    hooks.pre.extend(h.pre);
                    hooks.post.extend(h.post);
                }
                Err(e) => eprintln!("[WARN] Hooks aus {} ignoriert: {e}", path.display()),
            }
        }
        hooks
    }

    pub fn len(&self) -> usize {
        self.pre.len() + self.post.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Hüllt die Funktion des Tools `name` in seine Hooks. Ohne passenden Hook
    /// kommt `inner` unverändert zurück.
    ///
    /// `run` liefert den Stop-Knopf des laufenden Auftrags: ein Hook, der
    /// hängt, wird beim Abbruch beendet wie ein `run_shell`-Kindprozess.
    pub fn wrap(&self, name: &str, inner: ToolFn, workspace: &Path, run: RunHandle) -> ToolFn {
        let pre: Vec<Hook> = self
            .pre
            .iter()
            .filter(|h| h.matches(name))
            .cloned()
            .collect();
        let post: Vec<Hook> = self
            .post
            .iter()
            .filter(|h| h.matches(name))
            .cloned()
            .collect();
        if pre.is_empty() && post.is_empty() {
            return inner;
        }
        let name = name.to_string();
        let workspace = workspace.to_path_buf();
        Arc::new(move |args: Value| {
            let ctx = HookCtx {
                tool: &name,
                args: &args,
                workspace: &workspace,
                run: &run,
            };
            let mut vermerke = String::new();
            for h in &pre {
                match h.run(&ctx, "pre_tool", None) {
                    Outcome::Ok => {}
                    Outcome::Block(grund) => {
                        return Ok(format!("ABGELEHNT durch Hook »{}«: {grund}", h.command));
                    }
                    Outcome::Failed(e) => vermerke.push_str(&failed_note(h, &e)),
                }
            }
            // Ein harter Tool-Fehler geht unverändert durch: für ihn gibt es
            // kein Ergebnis, das ein Post-Hook prüfen könnte.
            let mut result = inner(args.clone())?;
            for h in &post {
                match h.run(&ctx, "post_tool", Some(&result)) {
                    Outcome::Ok => {}
                    Outcome::Block(meldung) => {
                        result.push_str(&format!("\n\n[Hook »{}« meldet]\n{meldung}", h.command));
                    }
                    Outcome::Failed(e) => vermerke.push_str(&failed_note(h, &e)),
                }
            }
            result.push_str(&vermerke);
            Ok(result)
        })
    }
}

/// Was ein Hook-Lauf über den Tool-Aufruf wissen muss.
struct HookCtx<'a> {
    tool: &'a str,
    args: &'a Value,
    workspace: &'a Path,
    run: &'a RunHandle,
}

impl Hook {
    fn matches(&self, tool: &str) -> bool {
        self.matcher.as_ref().map_or(true, |re| re.is_match(tool))
    }

    fn run(&self, ctx: &HookCtx, event: &str, result: Option<&str>) -> Outcome {
        let mut cmd = shell_command(&self.command);
        cmd.current_dir(ctx.workspace)
            .env("AGENTKIT_HOOK_EVENT", event)
            .env("AGENTKIT_TOOL", ctx.tool)
            .env("AGENTKIT_TOOL_ARGS", ctx.args.to_string());
        if let Some(r) = result {
            cmd.env(
                "AGENTKIT_TOOL_RESULT",
                r.chars().take(MAX_RESULT_ENV).collect::<String>(),
            );
        }
        match run_with_timeout(cmd, self.timeout, ctx.run.cancel().as_ref()) {
            Ok(RunOutcome::Done(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                // Die Begründung steht üblicherweise auf stderr; wer sie auf
                // stdout schreibt, soll trotzdem gehört werden.
                let text = if stderr.is_empty() { stdout } else { stderr };
                match out.status.code() {
                    Some(0) => Outcome::Ok,
                    Some(2) if text.is_empty() => Outcome::Block("(ohne Begründung)".into()),
                    Some(2) => Outcome::Block(text),
                    code => Outcome::Failed(format!(
                        "exit={} {}",
                        code.map_or("?".to_string(), |c| c.to_string()),
                        text
                    )),
                }
            }
            Ok(RunOutcome::Timeout) => Outcome::Failed(format!("Timeout nach {}s", self.timeout)),
            Ok(RunOutcome::Cancelled) => Outcome::Failed("abgebrochen".to_string()),
            Err(e) => Outcome::Failed(format!("nicht startbar: {e}")),
        }
    }
}

fn failed_note(h: &Hook, e: &str) -> String {
    format!("\n[Hook »{}« fehlgeschlagen: {}]", h.command, e.trim())
}

fn parse_list(v: &Value, key: &str) -> Result<Vec<Hook>, String> {
    let Some(list) = v.get(key) else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| format!("'{key}' muss eine Liste sein"))?;
    list.iter()
        .map(|h| {
            let command = h["command"]
                .as_str()
                .filter(|c| !c.trim().is_empty())
                .ok_or_else(|| format!("'{key}': Eintrag ohne 'command'"))?
                .to_string();
            let matcher = match h["matcher"].as_str().map(str::trim) {
                None | Some("") | Some("*") => None,
                // Verankert: `edit_file` soll nicht auch `multi_edit_file`
                // treffen, nur weil der Name darin steckt.
                Some(m) => Some(
                    Regex::new(&format!("^(?:{m})$"))
                        .map_err(|e| format!("'{key}': ungültiger matcher '{m}': {e}"))?,
                ),
            };
            let timeout = h["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS);
            Ok(Hook {
                matcher,
                command,
                timeout,
            })
        })
        .collect()
}

/// Die Hook-Dateien in Ladereihenfolge: benutzerweit, dann `--hooks`.
fn hook_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(user) = crate::config::config_dir().map(|d| d.join("hooks.json")) {
        if user.is_file() {
            files.push(user);
        }
    }
    if let Some(p) = std::env::var_os(HOOKS_ENV).filter(|p| !p.is_empty()) {
        files.push(PathBuf::from(p));
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentkit_hooks_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn echo_tool() -> ToolFn {
        Arc::new(|args: Value| Ok(format!("lief mit {args}")))
    }

    #[test]
    fn datei_wird_geparst_und_matcher_verankert() {
        let h = Hooks::from_json(
            r#"{"pre_tool": [{"matcher": "edit_file", "command": "x"}],
                "post_tool": [{"command": "y", "timeout": 5}]}"#,
        )
        .unwrap();
        assert_eq!(h.len(), 2);
        assert!(h.pre[0].matches("edit_file"));
        assert!(!h.pre[0].matches("multi_edit_file"));
        assert!(h.post[0].matches("irgendwas"));
        assert_eq!(h.post[0].timeout, 5);
        assert!(Hooks::from_json(r#"{"pre_tool": [{"matcher": "("}]}"#).is_err());
        assert!(Hooks::from_json(r#"{"pre_tool": [{"matcher": "x"}]}"#).is_err());
    }

    /// Exit 2 vor dem Aufruf blockiert — das Tool läuft nicht, die Begründung
    /// geht an den Agenten.
    #[cfg(unix)]
    #[test]
    fn pre_hook_mit_exit_2_blockiert() {
        let ws = tmp("block");
        let hooks = Hooks::from_json(
            r#"{"pre_tool": [{"matcher": "run_shell", "command": "echo \"kein $AGENTKIT_TOOL heute\" >&2; exit 2"}]}"#,
        )
        .unwrap();
        let f = hooks.wrap("run_shell", echo_tool(), &ws, RunHandle::new());
        let out = f(json!({"command": "ls"})).unwrap();
        assert!(out.starts_with("ABGELEHNT durch Hook"), "{out}");
        assert!(out.contains("kein run_shell heute"));
        // Ein nicht passender Name bleibt unberührt.
        let g = hooks.wrap("read_file", echo_tool(), &ws, RunHandle::new());
        assert!(g(json!({})).unwrap().starts_with("lief mit"));
    }

    /// Nach dem Aufruf: Exit 2 hängt die Meldung an, ein kaputter Hook hinterlässt
    /// einen Vermerk — das Ergebnis des Tools bleibt in beiden Fällen erhalten.
    #[cfg(unix)]
    #[test]
    fn post_hook_meldet_und_kaputter_hook_wird_vermerkt() {
        let ws = tmp("post");
        let hooks = Hooks::from_json(
            r#"{"post_tool": [
                {"command": "test -n \"$AGENTKIT_TOOL_RESULT\" && echo \"lint: $AGENTKIT_TOOL_ARGS\" && exit 2"},
                {"command": "exit 7"}
            ]}"#,
        )
        .unwrap();
        let f = hooks.wrap("write_file", echo_tool(), &ws, RunHandle::new());
        let out = f(json!({"path": "a.rs"})).unwrap();
        assert!(out.starts_with("lief mit"), "{out}");
        assert!(out.contains("meldet]\nlint: {\"path\":\"a.rs\"}"), "{out}");
        assert!(out.contains("fehlgeschlagen: exit=7"), "{out}");
    }

    /// Der Hook läuft im Workspace.
    #[cfg(unix)]
    #[test]
    fn hook_laeuft_im_workspace() {
        let ws = tmp("cwd");
        let hooks =
            Hooks::from_json(r#"{"post_tool": [{"command": "touch hook-war-hier"}]}"#).unwrap();
        let f = hooks.wrap("x", echo_tool(), &ws, RunHandle::new());
        f(json!({})).unwrap();
        assert!(ws.join("hook-war-hier").exists());
    }
}
