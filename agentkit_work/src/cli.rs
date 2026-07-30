//! `agentkit work <unterkommando>` — die gesamte CLI-Logik für agentkit-work.
//!
//! Analog zu `agentkit::cli` fürs Haupt-CLI: die Logik liegt komplett hier,
//! damit `agentkit_app` (Schritt 7) nur noch Argumente durchreicht und ein
//! dünnes Wiring-Crate bleibt. Argument-Parsing ist handgeschrieben, im
//! selben Stil wie `Args::parse` in `agentkit_app/src/bin/agentkit.rs`:
//! `--flag=wert` wird in zwei Tokens aufgespalten, danach ein einfacher
//! Werte-/Schalter-/Positionsargument-Scan je Unterkommando.
//!
//! Stream-Kontrakt (bindend, siehe `agent_framework_rs/CLAUDE.md` §„The
//! executable as a Unix filter"): **stdout** trägt bei `--format json` GENAU
//! EIN JSON-Dokument, sonst die Text-Zusammenfassung — nie Fortschritt oder
//! Fehler. **stderr** trägt Fortschritt, Hinweise und Fehlermeldungen.
//!
//! Die Naht `dispatch` → `dispatch_with_io`: `dispatch` ist das, was
//! `agentkit_app` aufruft (füllt `stdout`/`stderr` echt), `dispatch_with_io`
//! nimmt `out`/`err` als `&mut dyn Write` entgegen — das macht den
//! stdout-Kontrakt (genau ein JSON-Dokument, keine Vermischung mit stderr)
//! ohne Prozess-Fork oder Capture-Hacks testbar: Tests übergeben `Vec<u8>`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentkit::{
    one_line, AgentEvent, ApproveFn, Cancel, EventData, ExitCode, ExtraTools, Llm, OutputFormat,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::executor::CodingAgentExecutor;
use crate::model::{
    id_order, now_ms, slug, CompletionReason, ProjectStatus, RunStatus, WorkBudget, WorkItem,
    WorkItemKind, WorkItemStatus, WorkProject, WorkRun,
};
use crate::recovery;
use crate::runner::{run_to_completion, RunOutcome, RunnerConfig, WorkProgress};
use crate::state::WorkState;
use crate::store::{WorkStore, JOURNAL_FILE};

/// Was das Frontend beisteuern muss, damit die Work-CLI arbeiten kann. Das
/// Crate baut selbst kein LLM und kennt keine Provider-Flags — dieselbe
/// Trennung wie beim Executor-Port in `executor.rs`.
pub struct WorkCliDeps<'a> {
    /// Baut das LLM aus Provider-Name und Demo-Schalter — im Binary ist das
    /// `build_llm` (das dort zusätzlich ein Anzeige-Label liefert, das hier
    /// nicht gebraucht wird).
    pub llm: &'a dyn Fn(&str, bool) -> Arc<dyn Llm>,
    /// Freigabe-Callback für `run_shell` (im Binary: `-y` bzw. die
    /// Rückfrage). `work run -y` überschreibt das lokal mit "immer erlauben".
    pub approve: ApproveFn,
    /// Zusätzliche Tools des Frontends (swarm/Graph) — wird unverändert an
    /// den Work-Agenten durchgereicht (`CodingAgentExecutor::extra_tools`).
    pub extra_tools: Option<ExtraTools>,
    /// Stop-Knopf des Prozesses (Ctrl-C).
    pub cancel: Cancel,
}

/// Führt `agentkit work <unterkommando> …` aus. `argv` sind die Argumente
/// NACH dem Verb `work`. Öffentlicher Einstieg für `agentkit_app` — füllt
/// `stdout`/`stderr` echt, siehe Moduldoku für die Testbarkeits-Naht.
pub fn dispatch(argv: &[String], deps: WorkCliDeps<'_>) -> ExitCode {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    dispatch_with_io(argv, deps, &mut out, &mut err)
}

/// Wie [`dispatch`], aber mit injizierten Streams — die testbare Variante
/// (siehe Moduldoku).
pub fn dispatch_with_io(
    argv: &[String],
    deps: WorkCliDeps<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let norm = normalize(argv);
    if norm.is_empty() {
        let _ = writeln!(err, "[FEHLER] Kein Unterkommando angegeben.\n{HELP_TEXT}");
        return ExitCode::GeneralError;
    }
    if norm[0] == "-h" || norm[0] == "--help" {
        let _ = write!(out, "{HELP_TEXT}");
        return ExitCode::Success;
    }
    let rest = &norm[1..];
    match norm[0].as_str() {
        "create" => cmd_create(rest, out, err),
        "list" => cmd_list(rest, out, err),
        // 'resume' ist ein Alias von 'run' — 'run' erholt sich ohnehin immer
        // zuerst über `recovery::recover_all` (siehe `cmd_run`-Doku), es gibt
        // fachlich keinen Unterschied.
        "run" | "resume" => cmd_run(rest, &deps, out, err),
        "status" => cmd_status(rest, out, err),
        "items" => cmd_items(rest, out, err),
        "events" => cmd_events(rest, out, err),
        "pause" => cmd_pause(rest, out, err),
        "retry" => cmd_retry(rest, out, err),
        "budget" => cmd_budget(rest, out, err),
        other => {
            let _ = writeln!(err, "[FEHLER] Unbekanntes Unterkommando 'work {other}'.");
            let _ = writeln!(err, "{HELP_TEXT}");
            ExitCode::GeneralError
        }
    }
}

const HELP_TEXT: &str = "\
agentkit work <unterkommando> [optionen]

Persistente Arbeits-Runtime: zerlegt ein Vorhaben in Work Items und arbeitet
sie ab. Der Zustand überlebt den Prozess (Journal unter
<workspace>/.agentkit/work/<projekt-id>/work.jsonl).

Unterkommandos:
  create --title T --objective O [-w DIR] [--dir DIR]
         [--max-wall-time SEKUNDEN] [--max-items N] [--max-attempts N]
         [--max-steps N] [--items DATEI] [--format json]
      Legt ein neues Vorhaben an und startet Lauf 'R-1'. Gibt die Projekt-ID
      aus (Kebab-Slug des Titels; bei Kollision mit '-2', '-3', … versehen).

  list [-w DIR] [--dir DIR] [--format json]
      Listet alle Vorhaben unter der Work-Wurzel.

  run <projekt-id> [-w DIR] [--dir DIR] [-y] [--provider P] [--demo]
      [--max-steps N] [--steps] [--dry-run] [--force] [--format json]
      Arbeitet den Lauf ab — erholt sich zuerst automatisch von einem
      abgebrochenen vorherigen Versuch (abgelaufene Leases). Fortschritt auf
      stderr, Ergebnis auf stdout. Exit 0 nur, wenn alle Items fertig sind.
      --dry-run blockiert JEDE schreibende Aktion des Work-Agenten (u. a.
      'write_file', 'edit_file', 'run_shell' sowie dieselben Tools in
      Sub-Agenten/Schwarm) — die Tool-Schemas bleiben sichtbar, nur die
      Ausführung wird zu einem Hinweistext (\"[dry-run] … blockiert\").
      --force übernimmt eine vorhandene Sperrdatei ('work.lock') gewaltsam —
      nur sicher, wenn wirklich kein anderer 'agentkit work'-Prozess mehr auf
      diesem Projekt läuft (z. B. nach einem harten Absturz/SIGKILL).

  resume <projekt-id> …
      Alias von 'run' — 'run' erholt sich ohnehin immer zuerst.

  status <projekt-id> [--format json]
      Projekt, Lauf, Zähler je Item-Status, Budget, blockierte/wartende Items,
      Anzahl Versuche/Artefakte.

  items <projekt-id> [--format json]
      Alle Work Items des aktiven Laufs: ID, Status, Priorität, Titel,
      Abhängigkeiten, Versuche; endgültig blockierte Items sind markiert.

  events <projekt-id> [--tail N] [--format json]
      Journal-Zeilen als Zeitleiste (Standard: die letzten 50).

  pause <projekt-id>
      Markiert einen NICHT laufenden Lauf als pausiert. Einen laufenden
      Vordergrundprozess kann das MVP NICHT prozessübergreifend stoppen (kein
      PID-Tracking) — dafür ist Ctrl-C im laufenden Prozess da.

  retry <work-item-id> -p <projekt-id>
      Setzt ein gescheitertes Item mit verbleibenden Versuchen zurück auf
      'pending'.

  budget <projekt-id> [-w DIR] [--dir DIR] [--max-wall-time SEKUNDEN]
         [--max-items N] [--max-attempts N] [--max-steps N] [--format json]
      Zeigt das aktuelle Budget an, oder überschreibt die angegebenen Felder
      (nur diese, alle übrigen bleiben stehen) und journalt 'BudgetUpdated'.
      Ohne jedes Flag wird nur angezeigt, nichts journalt. Das ist der Weg
      aus einem Lauf, der wegen 'budget_exceeded' pausiert ist: Budget hier
      erhöhen, danach 'agentkit work run <projekt-id>' erneut aufrufen.

--items-Dateiformat (JSON-Liste, wird in Dateireihenfolge angelegt):
  [
    {\"title\": \"…\", \"description\": \"…\", \"kind\": \"implementation\",
     \"priority\": 5, \"depends_on\": [\"W-1\"],
     \"acceptance_criteria\": [\"…\"], \"required_role\": null}
  ]
  'depends_on' darf nur auf vorher in derselben Datei stehende Items
  verweisen (deren ID 'W-1', 'W-2', … in Anlegereihenfolge, 1-basiert nach
  Position in der Datei) — ein Verweis nach vorn wird abgelehnt.
";

// ---------------------------------------------------------------- Parsing

/// Bereitet `argv` fürs Parsen vor: `--flag=wert` wird zu den zwei Tokens
/// `--flag`, `wert` (nur Lang-Optionen) — dasselbe Muster wie
/// `normalize_args` im Haupt-Binary.
fn normalize(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    for a in argv {
        if a.starts_with("--") && a.len() > 2 {
            if let Some((k, v)) = a.split_once('=') {
                out.push(k.to_string());
                out.push(v.to_string());
                continue;
            }
        }
        out.push(a.clone());
    }
    out
}

/// Ergebnis eines handgeschriebenen Flag-Scans: Werte-Flags (`--flag WERT`),
/// Schalter (alles andere, das mit `-` beginnt) und Positionsargumente.
///
/// Eine kleine wiederverwendbare Funktion statt acht fast identischer
/// Kopien derselben Schleife (Guidelines §3/§4) — `value_flags` sagt, welche
/// Flags das nächste Token als Wert konsumieren.
struct Flags {
    values: HashMap<String, String>,
    switches: HashSet<String>,
    positionals: Vec<String>,
}

impl Flags {
    fn value(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|n| self.values.get(*n).cloned())
    }

    fn has(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.switches.contains(*n))
    }
}

/// Parst `args` gegen die für GENAU DIESES Unterkommando erlaubten Werte-
/// und Schalter-Flags. Jedes Token, das mit `-` beginnt und weder ein
/// bekannter Werte- noch ein bekannter Schalter-Flag ist, ist ein Fehler
/// (Befund 1 des Security-Reviews): vorher wurde ein unbekanntes Flag
/// stillschweigend geschluckt (Exit 0, keine Meldung) — ein Tippfehler in
/// einem Skript sah dann wie ein erfolgreicher Lauf aus. Ein bei einem
/// ANDEREN Unterkommando gültiges Flag zählt hier genauso als unbekannt,
/// deshalb bekommt jeder `cmd_*`-Aufruf seine eigene, engere Flag-Menge statt
/// einer gemeinsamen.
///
/// `--` beendet die Options-Erkennung (GNU/POSIX): alles danach ist
/// Positionsargument, auch wenn es mit `-` beginnt — dieselbe Konvention wie
/// bei `normalize_args` im Haupt-Binary.
fn parse_flags(
    cmd: &str,
    args: &[String],
    value_flags: &[&str],
    switch_flags: &[&str],
) -> Result<Flags, String> {
    let mut values = HashMap::new();
    let mut switches = HashSet::new();
    let mut positionals = Vec::new();
    let mut it = args.iter();
    let mut literal = false;
    while let Some(a) = it.next() {
        if literal {
            positionals.push(a.clone());
            continue;
        }
        if a == "--" {
            literal = true;
            continue;
        }
        if value_flags.contains(&a.as_str()) {
            if let Some(v) = it.next() {
                values.insert(a.clone(), v.clone());
            }
        } else if switch_flags.contains(&a.as_str()) {
            switches.insert(a.clone());
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(format!(
                "unbekanntes Flag '{a}' bei 'work {cmd}' — siehe 'agentkit work --help'."
            ));
        } else {
            positionals.push(a.clone());
        }
    }
    Ok(Flags {
        values,
        switches,
        positionals,
    })
}

/// Wie [`parse_flags`], meldet einen Parse-Fehler aber sofort auf `stderr`
/// und liefert direkt den fertigen `ExitCode` — sonst hätte jede der neun
/// `cmd_*`-Funktionen dieselbe Fehlerbehandlung dupliziert (Guidelines §3/§4,
/// Rule of Three: neun Aufrufer statt zwei rechtfertigen die Abstraktion).
fn parse_flags_checked(
    cmd: &str,
    args: &[String],
    value_flags: &[&str],
    switch_flags: &[&str],
    err: &mut dyn Write,
) -> Result<Flags, ExitCode> {
    parse_flags(cmd, args, value_flags, switch_flags).map_err(|msg| {
        let _ = writeln!(err, "[FEHLER] {msg}");
        ExitCode::GeneralError
    })
}

fn parse_format(v: Option<String>) -> OutputFormat {
    match v.map(|s| s.trim().to_lowercase()) {
        Some(ref s) if s == "json" => OutputFormat::Json,
        _ => OutputFormat::Text,
    }
}

/// Parst einen optionalen numerischen Flag-Wert. `Ok(None)`, wenn das Flag
/// nicht gesetzt ist; `Err` mit einer deutschen Meldung bei einem
/// unparsbaren Wert (kein `unwrap`/`expect` auf Nutzereingaben).
fn parse_opt<T: std::str::FromStr>(flags: &Flags, name: &str) -> Result<Option<T>, String> {
    match flags.values.get(name) {
        None => Ok(None),
        Some(v) => v
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("ungültiger Wert für {name}: '{v}'")),
    }
}

fn workspace_of(flags: &Flags) -> String {
    flags
        .value(&["-w", "--workspace"])
        .unwrap_or_else(|| ".".to_string())
}

/// Kanonisiert einen Pfad, soweit auflösbar — sonst der rohe String
/// unverändert (kein harter Fehler: `canonicalize` scheitert z. B., wenn das
/// Ziel noch nicht existiert, und nicht jeder Aufrufer kennt hier ein
/// existierendes Verzeichnis). Gemeinsamer Kern von `cmd_create` (Befund 5
/// des Code-Reviews) und `warn_workspace_mismatch`.
fn canonical_or_raw(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|c| c.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

fn dir_override_of(flags: &Flags) -> Option<String> {
    flags.value(&["--dir"])
}

/// `<workspace>/.agentkit/work`, oder `--dir`, falls gesetzt — `--dir`
/// überschreibt NUR diese Wurzel, die Projekt-ID wird immer noch angehängt
/// (siehe Verzeichnis-Layout in der Moduldoku).
fn work_root(workspace: &str, dir: Option<&str>) -> PathBuf {
    match dir {
        Some(d) => PathBuf::from(d),
        None => Path::new(workspace).join(".agentkit").join("work"),
    }
}

/// Nächste freie Projekt-ID: der Slug des Titels, bei Kollision mit
/// `-2`, `-3`, … versehen (siehe Plan „Datenmodell").
fn unique_project_id(root: &Path, title: &str) -> String {
    let base = slug(title);
    if !root.join(&base).exists() {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !root.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// `git rev-parse HEAD` im Workspace — `None` bei jedem Fehler (kein Repo,
/// kein `git` installiert, …): nicht jedes Vorhaben liegt in einem
/// Git-Repository, das ist kein Fehler.
fn git_head(workspace: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rev = String::from_utf8(output.stdout).ok()?;
    let rev = rev.trim();
    if rev.is_empty() {
        None
    } else {
        Some(rev.to_string())
    }
}

fn report_work_error(err: &mut dyn Write, e: &WorkError) {
    let _ = writeln!(err, "[FEHLER] {e}");
}

/// Befund 3 des Security-Reviews: `-w`/`--dir` dienen bei `work run` nur zum
/// AUFFINDEN des Projekts, ausgeführt wird immer im PERSISTIERTEN
/// `project.workspace` (siehe Kommentar in `cmd_run`). Das ist gewollt, aber
/// bisher stumm — verschiebt oder kopiert jemand ein Projektverzeichnis (z. B.
/// auf einen anderen Rechner), arbeitet der Agent lautlos in einem anderen
/// Verzeichnis, als der Operator laut Aufruf annimmt. Kein Abbruch, keine
/// Rückfrage — nur eine deutliche Warnung MIT BEIDEN Pfaden, bevor der Lauf
/// beginnt. Kanonisiert beide Seiten, soweit auflösbar (`canonicalize`
/// scheitert z. B., wenn der aufgerufene Pfad gar nicht existiert) — dann
/// zählt der rohe String, damit ein echtes Auseinanderlaufen nicht an einem
/// bloß nicht auflösbaren Pfad vorbeirutscht.
fn warn_workspace_mismatch(
    err: &mut dyn Write,
    invoked_workspace: &str,
    persisted_workspace: &str,
) {
    let invoked = canonical_or_raw(invoked_workspace);
    let persisted = canonical_or_raw(persisted_workspace);
    if invoked != persisted {
        let _ = writeln!(
            err,
            "[WARNUNG] Dieser Aufruf impliziert Workspace '{invoked_workspace}' (aufgelöst: \
             '{invoked}'), der Lauf arbeitet aber im PERSISTIERTEN Workspace \
             '{persisted_workspace}' (aufgelöst: '{persisted}') — das Projektverzeichnis wurde \
             vermutlich verschoben oder kopiert."
        );
    }
}

/// Öffnet ein vorhandenes Projekt oder meldet den Fehler — gemeinsamer Kern
/// von `status`/`items`/`pause`/`retry` (nicht `run`, das braucht einen
/// `Arc<WorkStore>`, und nicht `events`, das das Journal ohne Store liest,
/// siehe `cmd_events`-Doku).
fn open_project(root: &Path, project_id: &str, err: &mut dyn Write) -> Option<WorkStore> {
    let dir = root.join(project_id);
    if !dir.exists() {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}' nicht gefunden unter '{}'.",
            root.display()
        );
        return None;
    }
    match WorkStore::open(&dir) {
        Ok(s) => Some(s),
        Err(e) => {
            report_work_error(err, &e);
            None
        }
    }
}

/// Der Lauf mit der höchsten `R-<n>`-Nummer — im MVP gibt es je Projekt
/// genau einen Lauf (`R-1`, von `create` gestartet), diese Wahl bleibt aber
/// korrekt, falls das je mehr werden.
fn latest_run_id(state: &WorkState) -> Option<String> {
    state.runs.keys().max_by_key(|id| id_order(id)).cloned()
}

fn project_status_str(s: ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Active => "active",
        ProjectStatus::Completed => "completed",
        ProjectStatus::Canceled => "canceled",
    }
}

fn run_status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Completed => "completed",
        RunStatus::Canceled => "canceled",
    }
}

fn completion_reason_str(r: CompletionReason) -> &'static str {
    match r {
        CompletionReason::AllItemsDone => "all_items_done",
        CompletionReason::BudgetExceeded => "budget_exceeded",
        CompletionReason::Blocked => "blocked",
        CompletionReason::Canceled => "canceled",
    }
}

fn budget_json(b: &WorkBudget) -> Value {
    json!({
        "max_wall_time_secs": b.max_wall_time_secs,
        "max_work_items": b.max_work_items,
        "max_attempts_per_item": b.max_attempts_per_item,
        "max_steps_per_attempt": b.max_steps_per_attempt,
        "max_parallel_agents": b.max_parallel_agents,
    })
}

/// Kompakte einzeilige Anzeige des Budgets — für `status` (dort ist jedes
/// Feld ohnehin nur eine von mehreren Zeilen) und als eine der beiden
/// Text-Formen von `work budget`.
fn budget_text(b: &WorkBudget) -> String {
    let opt = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string());
    format!(
        "max_wall_time_secs={} max_work_items={} max_attempts_per_item={} max_steps_per_attempt={} max_parallel_agents={}",
        opt(b.max_wall_time_secs),
        b.max_work_items.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string()),
        b.max_attempts_per_item,
        b.max_steps_per_attempt,
        b.max_parallel_agents,
    )
}

/// Legt Lauf `R-1` an und journalt `RunStarted` — gemeinsamer Kern von
/// `cmd_create` (Erstanlage direkt nach `ProjectCreated`) und `cmd_run`
/// (Nachtrag, wenn Befund 3 zuschlägt: ein Absturz zwischen `ProjectCreated`
/// und `RunStarted` ließ das Projekt ohne Lauf zurück). `workspace` ist dabei
/// bewusst ein Parameter statt aus `store` gelesen: `cmd_create` kennt das
/// PERSISTIERTE `WorkProject` an dieser Stelle noch nicht (es wird gerade
/// erst gebaut), `cmd_run` liest es aus dem schon geladenen Snapshot.
fn start_first_run(store: &WorkStore, project_id: &str, workspace: &str) -> Result<(), WorkError> {
    let run = WorkRun {
        id: "R-1".to_string(),
        project_id: project_id.to_string(),
        status: RunStatus::Running,
        started_at_ms: now_ms(),
        completed_at_ms: None,
        base_revision: git_head(workspace),
        completion_reason: None,
    };
    store.submit(WorkEvent::RunStarted { run })?;
    Ok(())
}

/// Parst die vier Budget-Flags gegen `flags` und wendet nur die GESETZTEN auf
/// `budget` an — unveränderte Felder bleiben stehen. Gemeinsamer Kern von
/// `cmd_create` (Startbudget) und `cmd_budget` (spätere Änderung), vorher an
/// beiden Stellen fast identisch ausgeschrieben (Befund 6 des Code-Reviews,
/// Guidelines §3/§4). Gibt zurück, ob sich mindestens ein Feld geändert hat —
/// `cmd_budget` braucht das, um zu entscheiden, ob überhaupt journalt wird
/// (reines Anzeigen darf `seq` nicht bewegen); `cmd_create` ignoriert es, weil
/// dort ohnehin immer journalt wird.
fn apply_budget_flags(flags: &Flags, budget: &mut WorkBudget) -> Result<bool, String> {
    let mut changed = false;
    if let Some(v) = parse_opt::<u64>(flags, "--max-wall-time")? {
        budget.max_wall_time_secs = Some(v);
        changed = true;
    }
    if let Some(v) = parse_opt::<u32>(flags, "--max-items")? {
        budget.max_work_items = Some(v);
        changed = true;
    }
    if let Some(v) = parse_opt::<u32>(flags, "--max-attempts")? {
        budget.max_attempts_per_item = v;
        changed = true;
    }
    if let Some(v) = parse_opt::<u32>(flags, "--max-steps")? {
        budget.max_steps_per_attempt = v;
        changed = true;
    }
    Ok(changed)
}

// ------------------------------------------------------------------ create

const ITEM_KINDS_HELP: &str =
    "discovery, analysis, planning, implementation, test, review, documentation";

fn parse_item_kind(s: &str) -> Option<WorkItemKind> {
    serde_json::from_value(Value::String(s.to_string())).ok()
}

fn cmd_create(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "create",
        args,
        &[
            "--title",
            "--objective",
            "-w",
            "--workspace",
            "--dir",
            "--max-wall-time",
            "--max-items",
            "--max-attempts",
            "--max-steps",
            "--items",
            "--format",
        ],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };

    let Some(title) = flags.value(&["--title"]).filter(|t| !t.trim().is_empty()) else {
        let _ = writeln!(err, "[FEHLER] --title ist erforderlich.");
        return ExitCode::GeneralError;
    };
    let Some(objective) = flags
        .value(&["--objective"])
        .filter(|o| !o.trim().is_empty())
    else {
        let _ = writeln!(err, "[FEHLER] --objective ist erforderlich.");
        return ExitCode::GeneralError;
    };
    let invoked_workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));

    let mut budget = WorkBudget::default();
    if let Err(e) = apply_budget_flags(&flags, &mut budget) {
        let _ = writeln!(err, "[FEHLER] {e}");
        return ExitCode::GeneralError;
    }

    // Befund 5 (Handprobe): `project.workspace` speichert immer den
    // KANONISIERTEN, absoluten Pfad — nie den rohen (oft relativen)
    // Aufrufwert. `-w .` speicherte früher wörtlich den String "." und löste
    // sich zur Laufzeit relativ zum JEWEILIGEN Arbeitsverzeichnis auf: wurde
    // das Projektverzeichnis verschoben oder von woanders gestartet, arbeitete
    // der Agent lautlos in einem ANDEREN Verzeichnis — und
    // `warn_workspace_mismatch` konnte nie feuern, weil beide Seiten denselben
    // relativen String identisch auflösten. `work_root` (für die Wahl des
    // Projektverzeichnisses selbst) bleibt bewusst am ROHEN `invoked_workspace`
    // hängen, nicht am kanonisierten: er beschreibt "wo suchen", nicht
    // "wo arbeiten".
    let workspace = canonical_or_raw(&invoked_workspace);

    let root = work_root(&invoked_workspace, dir_override.as_deref());
    let project_id = unique_project_id(&root, &title);
    let dir = root.join(&project_id);

    let store = match WorkStore::open(&dir) {
        Ok(s) => s,
        Err(e) => {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    };

    let project = WorkProject {
        id: project_id.clone(),
        title: title.clone(),
        objective: objective.clone(),
        workspace: workspace.clone(),
        status: ProjectStatus::Active,
        created_at_ms: now_ms(),
        budget,
    };
    if let Err(e) = store.submit(WorkEvent::ProjectCreated { project }) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }

    if let Err(e) = start_first_run(&store, &project_id, &workspace) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }

    if let Some(items_path) = flags.value(&["--items"]) {
        if let Err(msg) = load_items_file(&store, "R-1", &items_path) {
            let _ = writeln!(err, "[FEHLER] {msg}");
            return ExitCode::GeneralError;
        }
    }

    match format {
        OutputFormat::Json => {
            let doc = json!({
                "project_id": project_id,
                "run_id": "R-1",
                "dir": dir.display().to_string(),
            });
            let _ = writeln!(out, "{doc}");
        }
        OutputFormat::Text => {
            let _ = writeln!(out, "{project_id}");
        }
    }
    ExitCode::Success
}

#[derive(Deserialize)]
struct ItemFileEntry {
    title: String,
    description: String,
    kind: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    required_role: Option<String>,
}

/// Legt die Items einer `--items`-Datei in Dateireihenfolge an. `depends_on`
/// darf nur auf zuvor in DIESEM Aufruf angelegte Items verweisen — das prüft
/// [`WorkState::validate_dependencies`] implizit: ein Vorwärts-Verweis zeigt
/// auf eine ID, die es im Zustand noch nicht gibt, und wird als „existiert
/// nicht" abgelehnt. Ein Zyklus ist damit strukturell unmöglich (§ siehe
/// Plan), nicht nur durch eine Konvention.
fn load_items_file(store: &WorkStore, run_id: &str, path: &str) -> Result<(), String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("Items-Datei '{path}' nicht lesbar: {e}"))?;
    let entries: Vec<ItemFileEntry> = serde_json::from_str(&text)
        .map_err(|e| format!("Items-Datei '{path}' ist kein gültiges JSON: {e}"))?;

    let max_attempts_default = store
        .snapshot()
        .project
        .as_ref()
        .map(|p| p.budget.max_attempts_per_item)
        .unwrap_or_else(|| WorkBudget::default().max_attempts_per_item);

    for (idx, entry) in entries.iter().enumerate() {
        let position = idx + 1;
        let title = entry.title.trim();
        if title.is_empty() {
            return Err(format!(
                "Item Position {position}: title darf nicht leer sein"
            ));
        }
        let description = entry.description.trim();
        if description.is_empty() {
            return Err(format!(
                "Item Position {position} ('{title}'): description darf nicht leer sein"
            ));
        }
        let Some(kind) = parse_item_kind(&entry.kind) else {
            return Err(format!(
                "Item Position {position} ('{title}'): unbekannte kind '{}' — erlaubt sind: {ITEM_KINDS_HELP}",
                entry.kind
            ));
        };
        let priority = entry.priority.unwrap_or(5);
        if priority > 9 {
            return Err(format!(
                "Item Position {position} ('{title}'): priority muss zwischen 0 und 9 liegen, war {priority}"
            ));
        }

        let snapshot = store.snapshot();
        let id = snapshot.next_item_id();
        if let Err(e) = snapshot.validate_dependencies(&id, &entry.depends_on) {
            return Err(format!("Item Position {position} ('{title}', {id}): {e}"));
        }

        let item = WorkItem {
            id: id.clone(),
            run_id: run_id.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            kind,
            status: WorkItemStatus::Pending,
            priority,
            seq: id_order(&id),
            required_role: entry.required_role.clone(),
            dependencies: entry.depends_on.clone(),
            acceptance_criteria: entry.acceptance_criteria.clone(),
            attempt_count: 0,
            max_attempts: max_attempts_default,
            updated_at_ms: now_ms(),
        };
        store
            .submit(WorkEvent::WorkItemCreated { item })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// -------------------------------------------------------------------- list

fn cmd_list(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "list",
        args,
        &["-w", "--workspace", "--dir", "--format"],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let root = work_root(&workspace, dir_override.as_deref());

    let mut projects: Vec<WorkProject> = Vec::new();
    let mut locked: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            match WorkStore::open(entry.path()) {
                Ok(store) => {
                    if let Some(project) = store.snapshot().project.clone() {
                        projects.push(project);
                    }
                }
                // `work.lock` (Befund 1) sperrt das GANZE Verzeichnis, auch
                // für dieses lesende Kommando — ein gerade laufendes Projekt
                // fehlt der Liste dann. Anders als bei einem Verzeichnis ohne
                // lesbares Journal (echter Datenmüll, weiterhin
                // stillschweigend übersprungen) ist das kein irrelevanter
                // Eintrag: der Name landet auf stderr, damit "fehlt in der
                // Liste" nicht mit "existiert nicht" verwechselt wird.
                Err(WorkError::Locked(_)) => {
                    locked.push(entry.file_name().to_string_lossy().to_string());
                }
                Err(_) => {}
            }
        }
    }
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    if !locked.is_empty() {
        let _ = writeln!(
            err,
            "[work] Hinweis: {} Vorhaben aktuell gesperrt (läuft vermutlich gerade) und in \
             dieser Liste nicht enthalten: {}",
            locked.len(),
            locked.join(", ")
        );
    }

    match format {
        OutputFormat::Json => {
            let doc: Vec<Value> = projects
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "title": p.title,
                        "objective": p.objective,
                        "status": project_status_str(p.status),
                    })
                })
                .collect();
            let _ = writeln!(out, "{}", Value::Array(doc));
        }
        OutputFormat::Text => {
            if projects.is_empty() {
                let _ = writeln!(out, "Keine Vorhaben unter '{}'.", root.display());
            }
            for p in &projects {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}",
                    p.id,
                    project_status_str(p.status),
                    p.title
                );
            }
        }
    }
    ExitCode::Success
}

// -------------------------------------------------------------- run/resume

/// Arbeitet einen Lauf ab: Store öffnen, erst `recovery::recover_all` (und
/// dessen Report melden), dann `runner::run_to_completion` mit einem
/// `CodingAgentExecutor`. Gilt für `run` UND `resume` — beide rufen diese
/// Funktion, `resume` ist nur ein anderer Name dafür (siehe `dispatch_with_io`).
fn cmd_run(
    args: &[String],
    deps: &WorkCliDeps<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let flags = match parse_flags_checked(
        "run",
        args,
        &[
            "-w",
            "--workspace",
            "--dir",
            "--provider",
            "--max-steps",
            "--format",
        ],
        &["-y", "--yes", "--demo", "--steps", "--dry-run", "--force"],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work run <projekt-id> [optionen]"
        );
        return ExitCode::GeneralError;
    };
    // Nur zum Auffinden der Projekt-Wurzel — die tatsächliche
    // Ausführungs-Workspace ist die im Projekt PERSISTIERTE (siehe unten,
    // `project.workspace`), nicht dieser Flag-Wert: ein Vorhaben arbeitet
    // immer an dem Code-Workspace, für den es angelegt wurde, unabhängig
    // davon, von wo aus `work run` aufgerufen wird. Weichen beide voneinander
    // ab, warnt `warn_workspace_mismatch` weiter unten (Befund 3) — ehrlich,
    // ohne das Verhalten zu ändern.
    let locate_workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let provider = flags
        .value(&["--provider"])
        .unwrap_or_else(|| "auto".to_string());
    let demo = flags.has(&["--demo"]);
    let show_steps = flags.has(&["--steps"]);
    let yes = flags.has(&["-y", "--yes"]);
    // Befund 2 des Security-Reviews: `--dry-run` griff bei `work run` bisher
    // gar nicht — die Konstante `dry_run: false` unten wurde durch dieses
    // Feld ersetzt. `CodingAgentExecutor::execute` reicht den Wert an
    // `CodingAgentConfig::dry_run` durch, und `build_coding_agent` wendet die
    // Sperre (`ToolRegistry::dry_run_blocking(is_likely_destructive)`) auf
    // JEDE Registry an, auch die von `task`/`swarm` erzeugten
    // (`agent_framework_rs/CLAUDE.md` §„--dry-run"). Kein eigener Nachbau der
    // Heuristik hier nötig.
    let dry_run = flags.has(&["--dry-run"]);
    let force = flags.has(&["--force"]);
    let format = parse_format(flags.value(&["--format"]));
    let max_steps_override = match parse_opt::<u32>(&flags, "--max-steps") {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(err, "[FEHLER] {e}");
            return ExitCode::GeneralError;
        }
    };

    let root = work_root(&locate_workspace, dir_override.as_deref());
    let dir = root.join(&project_id);
    if !dir.exists() {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}' nicht gefunden unter '{}'.",
            root.display()
        );
        return ExitCode::GeneralError;
    }
    // Befund 1 des Code-Reviews: `--force` übernimmt eine zurückgebliebene
    // `work.lock` gewaltsam — der Ausweg, wenn ein früherer Prozess durch
    // `SIGKILL`/Absturz gestorben ist, ohne sie selbst zu entfernen. Kein
    // automatischer Lebendigkeits-Check (siehe `WorkStore::force_unlock`):
    // das bleibt eine bewusste, vom Bediener bestätigte Entscheidung.
    if force {
        if let Err(e) = WorkStore::force_unlock(&dir) {
            let _ = writeln!(err, "[FEHLER] Sperrdatei nicht entfernbar: {e}");
            return ExitCode::GeneralError;
        }
        let _ = writeln!(
            err,
            "[WARNUNG] --force: eine vorhandene Sperrdatei wurde entfernt — nur sicher, wenn \
             kein anderer 'agentkit work'-Prozess mehr auf diesem Projekt läuft."
        );
    }
    let store = match WorkStore::open(&dir) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    };

    // Pflicht laut Runner-Doku: der Aufrufer erholt sich, BEVOR er
    // weiterläuft, damit er den Report anzeigen kann. `Recovered` ist
    // dasselbe `WorkProgress`-Vokabular wie der Rest des Fortschritts.
    let report = match recovery::recover_all(&store, now_ms()) {
        Ok(r) => r,
        Err(e) => {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    };
    render_progress(
        err,
        WorkProgress::Recovered {
            released: report.released_items.len(),
        },
        show_steps,
    );

    let snapshot = store.snapshot();
    let Some(project) = snapshot.project.clone() else {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}': kein Projekt im Journal."
        );
        return ExitCode::GeneralError;
    };
    let run_id = match latest_run_id(&snapshot) {
        Some(id) => id,
        None => {
            // Befund 3 des Code-Reviews: `cmd_create` journalt
            // `ProjectCreated` und `RunStarted` GETRENNT — stirbt der
            // Prozess dazwischen, bleibt ein Projekt ohne Lauf zurück, und
            // `create` legt beim nächsten Aufruf wegen der Kollisionsprüfung
            // in `unique_project_id` immer ein NEUES Verzeichnis an, statt
            // dieses zu reparieren. `run`/`resume` erkennen das hier und
            // tragen 'R-1' nach demselben Muster wie `ensure_plan_item` nach
            // — mit einem Hinweis auf stderr, dass ihn nachgetragen wurde,
            // statt den Lauf für immer unbenutzbar zu lassen.
            let _ = writeln!(
                err,
                "[work] Hinweis: Projekt '{project_id}' hatte keinen Lauf (vermutlich ein \
                 Absturz zwischen 'ProjectCreated' und 'RunStarted') — Lauf 'R-1' wird nachgetragen."
            );
            if let Err(e) = start_first_run(&store, &project_id, &project.workspace) {
                report_work_error(err, &e);
                return ExitCode::GeneralError;
            }
            "R-1".to_string()
        }
    };

    warn_workspace_mismatch(err, &locate_workspace, &project.workspace);

    // `--max-steps` überschreibt das PERSISTIERTE Budget für die folgenden
    // Versuche dieses Laufs: `run_to_completion` liest `max_steps_per_attempt`
    // aus dem Store, nicht aus einem Parameter dieser Funktion. Journalt
    // trotzdem als `BudgetUpdated` statt nur im Speicher zu gelten — damit ein
    // späterer 'status' erklärt, mit welchem Budget der Lauf tatsächlich
    // gearbeitet hat. Ein zweites `ProjectCreated` (wie früher) scheidet aus:
    // `state::apply` lehnt es jetzt ab, das Vorhaben ist schon angelegt.
    if let Some(v) = max_steps_override {
        if project.budget.max_steps_per_attempt != v {
            let mut updated_budget = project.budget.clone();
            updated_budget.max_steps_per_attempt = v;
            if let Err(e) = store.submit(WorkEvent::BudgetUpdated {
                budget: updated_budget,
                at_ms: now_ms(),
            }) {
                report_work_error(err, &e);
                return ExitCode::GeneralError;
            }
        }
    }

    let llm = (deps.llm)(&provider, demo);
    let approve: ApproveFn = if yes {
        Arc::new(|_: &str| true)
    } else {
        deps.approve.clone()
    };
    let executor = CodingAgentExecutor {
        llm,
        approve,
        extra_tools: deps.extra_tools.clone(),
        cancel: deps.cancel.clone(),
        dry_run,
        shell_timeout: 120,
        system_extra: None,
    };
    let runner_cfg = RunnerConfig {
        agent_id: "worker-1".to_string(),
        lease_secs: 600,
        heartbeat_secs: 30,
        workspace: project.workspace.clone(),
    };

    let outcome = run_to_completion(
        &store,
        &run_id,
        &executor,
        &runner_cfg,
        &deps.cancel,
        &mut |p| render_progress(err, p, show_steps),
    );

    match outcome {
        Ok(res) => {
            let success = res.reason == CompletionReason::AllItemsDone;
            print_run_outcome(out, format, &res);
            if success {
                ExitCode::Success
            } else {
                ExitCode::GeneralError
            }
        }
        Err(e) => {
            report_work_error(err, &e);
            ExitCode::GeneralError
        }
    }
}

/// Fortschritt eines Laufs nach stderr — nur `ItemStarted`/`ItemDone`/
/// `Note`/`Checkpoint`/`Recovered` standardmäßig; einzelne `Agent(..)`-Events
/// nur mit `--steps` (siehe Kontrakt in der Moduldoku).
fn render_progress(err: &mut dyn Write, progress: WorkProgress, show_steps: bool) {
    match progress {
        WorkProgress::ItemStarted {
            item,
            title,
            attempt,
            max_attempts,
        } => {
            let _ = writeln!(
                err,
                "[work] {item} '{title}' — Versuch {attempt}/{max_attempts} gestartet."
            );
        }
        WorkProgress::Agent(ev) => {
            if show_steps {
                if let Some(line) = format_agent_event(&ev) {
                    let _ = writeln!(err, "  {line}");
                }
            }
        }
        WorkProgress::ItemDone { item, ok, summary } => {
            let status = if ok { "erledigt" } else { "gescheitert" };
            let _ = writeln!(err, "[work] {item} {status}: {summary}");
        }
        WorkProgress::Recovered { released } => {
            let _ = writeln!(
                err,
                "[work] Wiederaufnahme: {released} Item(s) freigegeben."
            );
        }
        WorkProgress::Checkpoint { seq } => {
            let _ = writeln!(err, "[work] Checkpoint (seq {seq}).");
        }
        WorkProgress::Note(msg) => {
            let _ = writeln!(err, "[work] Hinweis: {msg}");
        }
    }
}

fn format_agent_event(ev: &AgentEvent) -> Option<String> {
    match &ev.data {
        EventData::Step { step } => Some(format!("Schritt {step}")),
        EventData::ToolCall { name, .. } => Some(format!("Tool-Aufruf: {name}")),
        EventData::ToolResult { name, result } => {
            Some(format!("Tool-Ergebnis {name}: {}", one_line(result, 80)))
        }
        EventData::Error { name, error } => Some(format!(
            "Fehler{}: {error}",
            name.as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        )),
        EventData::TextDelta(_)
        | EventData::Plan(_)
        | EventData::Final(_)
        | EventData::Cancelled { .. }
        | EventData::Done
        | EventData::None => None,
    }
}

fn print_run_outcome(out: &mut dyn Write, format: OutputFormat, res: &RunOutcome) {
    match format {
        OutputFormat::Json => {
            let doc = json!({
                "reason": completion_reason_str(res.reason),
                "completed": res.completed,
                "failed": res.failed,
                "attempts": res.attempts,
            });
            let _ = writeln!(out, "{doc}");
        }
        OutputFormat::Text => {
            let _ = writeln!(out, "Lauf beendet: {}", completion_reason_str(res.reason));
            let _ = writeln!(out, "Versuche: {}", res.attempts);
            let _ = writeln!(out, "Abgeschlossen: {}", res.completed.join(", "));
            if !res.failed.is_empty() {
                let _ = writeln!(out, "Endgültig gescheitert: {}", res.failed.join(", "));
            }
        }
    }
}

// ------------------------------------------------------------------ status

fn cmd_status(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "status",
        args,
        &["-w", "--workspace", "--dir", "--format"],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work status <projekt-id> [--format json]"
        );
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let snapshot = store.snapshot();
    let Some(project) = &snapshot.project else {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}': kein Projekt im Journal."
        );
        return ExitCode::GeneralError;
    };
    let run_id = latest_run_id(&snapshot);
    let run = run_id.as_ref().and_then(|id| snapshot.runs.get(id));

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for kind in ["pending", "running", "completed", "failed", "canceled"] {
        counts.insert(kind.to_string(), 0);
    }
    let mut blocked_items = Vec::new();
    let mut waiting_items = Vec::new();
    let mut attempts_total = 0usize;
    if let Some(rid) = &run_id {
        for item in snapshot.items.values().filter(|it| &it.run_id == rid) {
            *counts.entry(item.status.to_string()).or_insert(0) += 1;
            if !snapshot.blocked_by(&item.id).is_empty() {
                blocked_items.push(item.id.clone());
            } else if !snapshot.waiting_on(&item.id).is_empty() {
                waiting_items.push(item.id.clone());
            }
        }
        attempts_total = snapshot
            .attempts
            .values()
            .filter(|a| {
                snapshot
                    .items
                    .get(&a.work_item_id)
                    .is_some_and(|it| &it.run_id == rid)
            })
            .count();
    }
    let artifacts_total = snapshot.artifacts.len();

    match format {
        OutputFormat::Json => {
            let doc = json!({
                "project_id": project.id,
                "title": project.title,
                "status": project_status_str(project.status),
                "run_id": run_id,
                "run_status": run.map(|r| run_status_str(r.status)),
                "completion_reason": run.and_then(|r| r.completion_reason).map(completion_reason_str),
                "budget": budget_json(&project.budget),
                "items_by_status": counts,
                "blocked_items": blocked_items,
                "waiting_items": waiting_items,
                "attempts_total": attempts_total,
                "artifacts_total": artifacts_total,
            });
            let _ = writeln!(out, "{doc}");
        }
        OutputFormat::Text => {
            let _ = writeln!(out, "Projekt       : {} ({})", project.title, project.id);
            let _ = writeln!(
                out,
                "Status        : {}",
                project_status_str(project.status)
            );
            if let (Some(rid), Some(r)) = (&run_id, run) {
                let _ = writeln!(out, "Lauf          : {rid} — {}", run_status_str(r.status));
                if let Some(reason) = r.completion_reason {
                    let _ = writeln!(out, "Abschlussgrund: {}", completion_reason_str(reason));
                }
            }
            let _ = writeln!(out, "Budget        : {}", budget_text(&project.budget));
            let counts_line: Vec<String> = counts.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let _ = writeln!(out, "Items         : {}", counts_line.join(" "));
            if !blocked_items.is_empty() {
                let _ = writeln!(out, "Blockiert     : {}", blocked_items.join(", "));
            }
            if !waiting_items.is_empty() {
                let _ = writeln!(out, "Wartend       : {}", waiting_items.join(", "));
            }
            let _ = writeln!(out, "Versuche      : {attempts_total}");
            let _ = writeln!(out, "Artefakte     : {artifacts_total}");
        }
    }
    ExitCode::Success
}

// ------------------------------------------------------------------- items

fn cmd_items(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "items",
        args,
        &["-w", "--workspace", "--dir", "--format"],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work items <projekt-id> [--format json]"
        );
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let snapshot = store.snapshot();
    // Befund 3 des Code-Reviews: ein Projekt ohne Lauf (Absturz zwischen
    // 'ProjectCreated' und 'RunStarted' in `cmd_create`, bevor 'run' ihn
    // nachträgt) ist eine leere, aber gültige Liste — kein Fehler. `status`
    // behandelt denselben Fall schon so (`run_id: Option`).
    let run_id = latest_run_id(&snapshot);
    let mut items: Vec<_> = match &run_id {
        Some(rid) => snapshot
            .items
            .values()
            .filter(|it| &it.run_id == rid)
            .collect(),
        None => Vec::new(),
    };
    items.sort_by_key(|it| it.seq);

    match format {
        OutputFormat::Json => {
            let doc: Vec<Value> = items
                .iter()
                .map(|it| {
                    json!({
                        "id": it.id,
                        "status": it.status.to_string(),
                        "priority": it.priority,
                        "title": it.title,
                        "dependencies": it.dependencies,
                        "attempt_count": it.attempt_count,
                        "max_attempts": it.max_attempts,
                        "blocked": !snapshot.blocked_by(&it.id).is_empty(),
                    })
                })
                .collect();
            let _ = writeln!(out, "{}", Value::Array(doc));
        }
        OutputFormat::Text => {
            if items.is_empty() {
                let _ = writeln!(out, "Keine Work Items im aktiven Lauf.");
            }
            for it in &items {
                let blocked = if !snapshot.blocked_by(&it.id).is_empty() {
                    " [BLOCKIERT]"
                } else {
                    ""
                };
                let deps = if it.dependencies.is_empty() {
                    "-".to_string()
                } else {
                    it.dependencies.join(",")
                };
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}/{}{blocked}",
                    it.id,
                    it.status,
                    it.priority,
                    it.title,
                    deps,
                    it.attempt_count,
                    it.max_attempts
                );
            }
        }
    }
    ExitCode::Success
}

// ------------------------------------------------------------------ events

/// Liest `work.jsonl` zeilenweise SELBST, statt über `WorkStore` zu gehen:
/// `WorkStore` materialisiert nur den aktuellen ZUSTAND, keine Zeitleiste
/// vergangener Ereignisse (die werden beim Replay konsumiert und beim
/// Checkpoint sogar überschrieben). Das Journalformat selbst ist Teil des
/// dokumentierten, stabilen Layouts (siehe Plan „Journal-Format") — ein
/// Lesezugriff hier verletzt keine Kapselung von `store/journal.rs`, das für
/// SCHREIBEN und Replay zuständig bleibt; dieser Befehl liest nur an, ohne
/// `state.rs`/`store/mod.rs` anzufassen (die gehören dem parallelen Schritt).
fn cmd_events(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "events",
        args,
        &["-w", "--workspace", "--dir", "--tail", "--format"],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work events <projekt-id> [--tail N] [--format json]"
        );
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let tail = match parse_opt::<usize>(&flags, "--tail") {
        Ok(v) => v.unwrap_or(50),
        Err(e) => {
            let _ = writeln!(err, "[FEHLER] {e}");
            return ExitCode::GeneralError;
        }
    };
    let root = work_root(&workspace, dir_override.as_deref());
    let dir = root.join(&project_id);
    if !dir.exists() {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}' nicht gefunden unter '{}'.",
            root.display()
        );
        return ExitCode::GeneralError;
    }

    let journal_path = dir.join(JOURNAL_FILE);
    let content = match fs::read_to_string(&journal_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            let _ = writeln!(
                err,
                "[FEHLER] Journal '{}' nicht lesbar: {e}",
                journal_path.display()
            );
            return ExitCode::GeneralError;
        }
    };

    // Kaputte/abgeschnittene Zeilen werden übersprungen statt den ganzen
    // Befehl scheitern zu lassen — dieselbe Toleranz wie `Journal::open`
    // für die zuletzt geschriebene Zeile, hier aber für JEDE Zeile: dies ist
    // eine reine Anzeige, keine Zustands-Autorität.
    let entries: Vec<Value> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let start = entries.len().saturating_sub(tail);
    let tail_entries = &entries[start..];

    match format {
        OutputFormat::Json => {
            let _ = writeln!(out, "{}", Value::Array(tail_entries.to_vec()));
        }
        OutputFormat::Text => {
            if tail_entries.is_empty() {
                let _ = writeln!(out, "Keine Ereignisse.");
            }
            for entry in tail_entries {
                let at = entry.get("at").and_then(Value::as_u64).unwrap_or(0);
                // Eine Snapshot-Zeile hat kein "event.kind"-Feld (sie trägt den
                // ganzen `WorkState`, kein einzelnes Ereignis) — aber ihr
                // einziger Erzeuger ist `WorkStore::checkpoint`, deshalb ist
                // "checkpoint_created" hier keine Erfindung, sondern der Name
                // des Ereignisses, das diese Zeile ausgelöst hat.
                let kind = entry
                    .get("event")
                    .and_then(|e| e.get("kind"))
                    .and_then(Value::as_str)
                    .or_else(|| entry.get("snapshot").map(|_| "checkpoint_created"))
                    .unwrap_or("?");
                let _ = writeln!(out, "[{at}] {kind}");
            }
        }
    }
    ExitCode::Success
}

// ------------------------------------------------------------------- pause

fn cmd_pause(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked("pause", args, &["-w", "--workspace", "--dir"], &[], err)
    {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(err, "[FEHLER] Nutzung: agentkit work pause <projekt-id>");
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let snapshot = store.snapshot();
    let Some(run_id) = latest_run_id(&snapshot) else {
        let _ = writeln!(err, "[FEHLER] Projekt '{project_id}' hat keinen Lauf.");
        return ExitCode::GeneralError;
    };
    let run = snapshot
        .runs
        .get(&run_id)
        .expect("run_id aus snapshot.runs");

    if run.status == RunStatus::Running {
        // Ehrlich zugeben statt vorzutäuschen (Plan §„pause"): das MVP hat
        // kein PID-/Lock-Tracking, kann also nicht unterscheiden, ob dieser
        // 'Running'-Status von einem gerade lebenden Vordergrundprozess
        // stammt oder von einem abgestürzten. Sicherer Default: ablehnen.
        let _ = writeln!(
            err,
            "[FEHLER] Lauf '{run_id}' ist als 'running' markiert — prozessübergreifendes \
             Pausieren eines laufenden Vordergrund-Laufs unterstützt das MVP nicht (kein \
             PID-Tracking). Nutze Ctrl-C im laufenden Prozess; ein abgestürzter Lauf wird beim \
             nächsten 'work run'/'work resume' automatisch fortgesetzt."
        );
        return ExitCode::GeneralError;
    }

    match store.submit(WorkEvent::RunPaused {
        run: run_id.clone(),
        reason: "manuell pausiert (work pause)".to_string(),
        at_ms: now_ms(),
    }) {
        Ok(_) => {
            let _ = writeln!(out, "Lauf '{run_id}' pausiert.");
            ExitCode::Success
        }
        Err(e) => {
            report_work_error(err, &e);
            ExitCode::GeneralError
        }
    }
}

// ------------------------------------------------------------------- retry

fn cmd_retry(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "retry",
        args,
        &["-p", "--project", "-w", "--workspace", "--dir"],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(item_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work retry <work-item-id> -p <projekt-id>"
        );
        return ExitCode::GeneralError;
    };
    let Some(project_id) = flags.value(&["-p", "--project"]) else {
        let _ = writeln!(err, "[FEHLER] -p <projekt-id> ist erforderlich.");
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let snapshot = store.snapshot();
    let Some(item) = snapshot.items.get(&item_id) else {
        let _ = writeln!(
            err,
            "[FEHLER] Work Item '{item_id}' existiert nicht in Projekt '{project_id}'."
        );
        return ExitCode::GeneralError;
    };
    if item.status != WorkItemStatus::Failed {
        let _ = writeln!(
            err,
            "[FEHLER] Work Item '{item_id}' ist nicht 'failed' (aktuell: '{}') — nur gescheiterte \
             Items können erneut versucht werden.",
            item.status
        );
        return ExitCode::GeneralError;
    }
    if item.attempt_count >= item.max_attempts {
        let _ = writeln!(
            err,
            "[FEHLER] Work Item '{item_id}' hat seine {} Versuche ausgeschöpft — ein Retry ohne \
             Erhöhung von max_attempts wäre sinnlos. Lege stattdessen ein neues Work Item an \
             (z. B. über den Agenten mit 'work_add_item').",
            item.max_attempts
        );
        return ExitCode::GeneralError;
    }

    match store.submit(WorkEvent::WorkItemReleased {
        item: item_id.clone(),
        reason: "manueller Retry (work retry)".to_string(),
        at_ms: now_ms(),
    }) {
        Ok(_) => {
            let _ = writeln!(out, "Work Item '{item_id}' zurückgesetzt auf 'pending'.");
            ExitCode::Success
        }
        Err(e) => {
            report_work_error(err, &e);
            ExitCode::GeneralError
        }
    }
}

// ------------------------------------------------------------------ budget

/// Zeigt das aktuelle Budget an oder überschreibt die angegebenen Felder.
/// Der Weg aus einem wegen `BudgetExceeded` pausierten Lauf (siehe
/// `runner::run_to_completion`, `Decision::BudgetExhausted`): ohne dieses
/// Unterkommando konnte ein Nutzer das Budget bisher nirgends ändern, obwohl
/// genau das die dokumentierte Erwartung war.
///
/// Ohne jedes der vier Werte-Flags journalt diese Funktion NICHTS — reines
/// Anzeigen darf die Sequenznummer nicht bewegen.
fn cmd_budget(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let flags = match parse_flags_checked(
        "budget",
        args,
        &[
            "-w",
            "--workspace",
            "--dir",
            "--max-wall-time",
            "--max-items",
            "--max-attempts",
            "--max-steps",
            "--format",
        ],
        &[],
        err,
    ) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let Some(project_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(
            err,
            "[FEHLER] Nutzung: agentkit work budget <projekt-id> [optionen]"
        );
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let snapshot = store.snapshot();
    let Some(project) = &snapshot.project else {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}': kein Projekt im Journal."
        );
        return ExitCode::GeneralError;
    };

    let mut budget = project.budget.clone();
    let changed = match apply_budget_flags(&flags, &mut budget) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "[FEHLER] {e}");
            return ExitCode::GeneralError;
        }
    };

    if changed {
        if let Err(e) = store.submit(WorkEvent::BudgetUpdated {
            budget: budget.clone(),
            at_ms: now_ms(),
        }) {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    }

    match format {
        OutputFormat::Json => {
            let _ = writeln!(out, "{}", budget_json(&budget));
        }
        OutputFormat::Text => {
            let _ = writeln!(out, "{}", budget_text(&budget));
        }
    }
    ExitCode::Success
}
