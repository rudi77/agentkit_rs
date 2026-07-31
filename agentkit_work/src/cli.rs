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
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentkit::{
    one_line, AgentEvent, ApproveFn, Cancel, EventData, ExitCode, ExtraTools, Llm, OutputFormat,
    TraceWriter,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::WorkError;
use crate::event::{WorkEvent, HUMAN_BY};
use crate::executor::{AgentExecutor, CodingAgentExecutor};
use crate::graph::GraphGateway;
use crate::model::{
    id_order, is_item_branch_of, item_branch_name, now_ms, slug, ArtifactKind, CompletionReason,
    ExecutorKind, ProjectStatus, RunStatus, VerificationPolicy, WorkBudget, WorkItem, WorkItemId,
    WorkItemKind, WorkItemStatus, WorkProject, WorkRun,
};
use crate::recovery;
use crate::runner::{run_to_completion, RunOutcome, RunnerConfig, WorkProgress};
use crate::scheduler::{self, Decision};
use crate::state::WorkState;
use crate::store::{WorkStore, JOURNAL_FILE};

/// Baut den tatsächlich verwendeten Executor aus dem Einzelagenten-Executor
/// (siehe [`WorkCliDeps::build_executor`]) — eigener Alias, weil der volle
/// `Box<dyn Fn(...) -> Box<dyn ...>>`-Typ sonst clippys `type_complexity`-Lint
/// auslöst.
pub type ExecutorBuilder = Box<dyn Fn(CodingAgentExecutor) -> Box<dyn AgentExecutor>>;

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
    /// Zugang zum Wissensgraphen (Feature `graph`, `--graph DIR`) — `None`
    /// ohne beides. Wandert unverändert in `RunnerConfig::graph`; ohne
    /// Gateway gibt es weder `work_claim` noch Recall, und ein Lauf verhält
    /// sich exakt wie vor Phase 4.
    pub graph: Option<Arc<dyn GraphGateway>>,
    /// Baut den tatsächlich an `run_to_completion` gereichten Executor aus
    /// dem Einzelagenten-Executor, den `cmd_run` sonst unverändert benutzt
    /// (Phase 6, §13 des Konzepts). `None` (z. B. in den Tests dieses Crates)
    /// läuft exakt wie vor Phase 6 — der Einzelagenten-Executor wird direkt
    /// verwendet. `agentkit_app` reicht hier eine Closure, die den
    /// `DispatchingExecutor` baut: er kennt sowohl `ExecutorKind::Swarm` als
    /// auch `agentkit_swarm` — beides darf dieses Crate nicht kennen
    /// (CLAUDE.md, Einbahnrichtung). Der Runner selbst ändert sich dadurch
    /// nicht: `run_to_completion` nimmt weiterhin nur `&dyn AgentExecutor`.
    pub build_executor: Option<ExecutorBuilder>,
    /// Mitschnitt des Ereignisstroms (`--trace DIR`) — `None` ohne das Flag.
    ///
    /// Ein Work-Lauf hat keinen `EventBus`, an dem der Trace sonst hängt
    /// (`agentkit::EventBus::with_trace`): der Runner reicht die `AgentEvent`s
    /// als `WorkProgress::Agent` durch einen Callback. Hier ist deshalb die
    /// Naht — sonst wäre ein Work-Lauf im Betrachter blind, und genau dafür
    /// ist der Betrachter gebaut.
    pub trace: Option<Arc<TraceWriter>>,
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
        "watch" => cmd_watch(rest, &deps, out, err),
        "pause" => cmd_pause(rest, out, err),
        "retry" => cmd_retry(rest, out, err),
        "budget" => cmd_budget(rest, out, err),
        "approve" => cmd_approve(rest, &deps, out, err),
        "reject" => cmd_reject(rest, out, err),
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
         [--max-steps N] [--items DATEI] [--verify-command \"<befehl>\"]
         [--git-isolation] [--format json]
      Legt ein neues Vorhaben an und startet Lauf 'R-1'. Gibt die Projekt-ID
      aus (Kebab-Slug des Titels; bei Kollision mit '-2', '-3', … versehen).
      --verify-command setzt die Policy 'automated_tests' mit diesem Befehl
      für jedes Item aus '--items', das keine eigene 'verification'-Angabe
      trägt.
      --git-isolation gibt jedem schreibenden Item (weder 'review' noch das
      automatisch angelegte Planungs-Item) einen eigenen Git-Branch
      ('work/<projekt-id>/<item-id>'); ein erfolgreicher Versuch committet,
      ein gescheiterter verwirft seine Änderungen. Sind alle Items terminal,
      merged ein automatisches Integrations-Item die erfolgreichen Branches
      zurück — bei einem Konflikt bricht es ab und der Lauf bleibt blockiert
      (siehe README-Abschnitt 'Git-Isolation'). Verlangt ein Git-Repository
      unter dem Workspace; sonst wird abgelehnt.

  list [-w DIR] [--dir DIR] [--format json]
      Listet alle Vorhaben unter der Work-Wurzel.

  run <projekt-id> [-w DIR] [--dir DIR] [-y] [--provider P] [--demo]
      [--max-steps N] [--steps] [--dry-run] [--force] [--format json]
      [--trace DIR]
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
      --trace DIR schreibt den kompletten Ereignisstrom des Laufs als NDJSON
      mit — die Datengrundlage für 'agentkit viz'. ACHTUNG: die Datei enthält
      Dateiinhalte, Shell-Ausgaben und Modellantworten unredigiert.

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

  watch <projekt-id> [--interval SEKUNDEN] [--tail N] [--format json]
      Live-Ansicht für ein ZWEITES Terminal neben einem laufenden 'work run'
      (deshalb sperrfrei — siehe Befund 0): Kopfzeile (Projekt/Lauf/Status),
      das Work-Item-Board (Status/Priorität/Versuche/Executor/
      Verifikationsrichtlinie), das gerade laufende Item (Agent, Versuch,
      verbleibende Lease-Zeit), Budgetverbrauch (Wandzeit/Items gegen die
      Limits, Versuche, Artefakte), die letzten Zeitleisten-Einträge und
      ausdrücklich, worauf der Lauf wartet (Freigabe, Blockade, Budget).
      --interval SEKUNDEN (Standard 2) bestimmt den Abstand zwischen zwei
      Aktualisierungen; --tail N (Standard 10) die Anzahl der Zeitleisten-
      Einträge. Beendet sich mit Ctrl-C sauber (stellt den Cursor wieder her).
      --format json gibt KEINE Endlosschleife aus, sondern GENAU EIN
      JSON-Dokument mit demselben Inhalt und kehrt zurück — der
      stdout-Kontrakt (siehe Moduldoku) gilt auch hier. Ist stdout kein
      Terminal (z. B. eine Pipe), verhält sich 'watch' ebenfalls wie ein
      einmaliges 'status': kein ANSI-Redraw in eine Pipe.

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

  approve <work-item-id> -p <projekt-id> [--reason TEXT] [-w DIR] [--dir DIR]
      Gibt ein Item frei, das in 'awaiting_verification' auf eine manuelle
      Freigabe wartet (Policy 'human_approval'): schließt es als 'completed'
      ab. Nur auf Items mit genau diesem Status/dieser Policy — sonst Exit 1.

  reject <work-item-id> -p <projekt-id> --reason TEXT [-w DIR] [--dir DIR]
      Lehnt ein wartendes Item ab: derselbe Mechanismus wie ein fachlicher
      Fehlschlag — 'attempt_count' steigt, bei verbleibenden Versuchen zurück
      auf 'pending' (der Grund erscheint im nächsten Arbeitspaket), sonst
      bleibt es 'failed'. '--reason' ist Pflicht. Nur auf Items in
      'awaiting_verification' mit Policy 'human_approval' — sonst Exit 1.

--items-Dateiformat (JSON-Liste, wird in Dateireihenfolge angelegt):
  [
    {\"title\": \"…\", \"description\": \"…\", \"kind\": \"implementation\",
     \"priority\": 5, \"depends_on\": [\"W-1\"],
     \"acceptance_criteria\": [\"…\"], \"required_role\": null,
     \"verification\": \"none\", \"executor\": \"single_agent\"}
  ]
  'depends_on' darf nur auf vorher in derselben Datei stehende Items
  verweisen (deren ID 'W-1', 'W-2', … in Anlegereihenfolge, 1-basiert nach
  Position in der Datei) — ein Verweis nach vorn wird abgelehnt.
  'verification' ist optional (Default 'none', oder '--verify-command', falls
  gesetzt): \"none\" | {\"automated_tests\": \"<befehl>\"} | \"human_approval\" |
  \"independent_agent\" (legt nach einem erfolgreichen Versuch automatisch ein
  Prüf-Item mit Rolle 'reviewer' an, siehe README-Abschnitt 'Verifikation').
  'executor' ist optional (Default \"single_agent\"): \"single_agent\" |
  {\"swarm\": \"<vorlage>\"} — welche Vorlagen ein Frontend kennt (z. B.
  'discovery', 'review'), steht in dessen eigener Doku, nicht hier; ohne
  Schwarm-Fähigkeit (Feature aus oder '--no-swarm') läuft das Item mit dem
  Einzelagenten, siehe README-Abschnitt 'Schwarm-Anbindung'.
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

/// Wie [`open_project`], aber sperrfrei (Befund 0 der Handprobe) — der Kern
/// von `status`/`items`: beide lesen nur, sie dürfen deshalb funktionieren,
/// während ein `agentkit work run` im selben Verzeichnis die Sperre hält.
/// `pause`/`retry`/`approve`/`reject`/`budget` bleiben bei [`open_project`]:
/// sie schreiben (journalen ein Ereignis) und brauchen dafür weiterhin den
/// exklusiven `WorkStore`.
fn open_project_read_only(root: &Path, project_id: &str, err: &mut dyn Write) -> Option<WorkState> {
    let dir = root.join(project_id);
    if !dir.exists() {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}' nicht gefunden unter '{}'.",
            root.display()
        );
        return None;
    }
    match WorkStore::open_read_only(&dir) {
        Ok(state) => Some(state),
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
        CompletionReason::AwaitingVerification => "awaiting_verification",
    }
}

/// Anzeige einer [`VerificationPolicy`] für `status`/`items` — knapp und
/// eindeutig, kein Roundtrip zum `--items`-Wire-Format nötig (das ist eine
/// EINGABE-Form, keine Anzeige-Form).
fn verification_policy_str(p: &VerificationPolicy) -> String {
    match p {
        VerificationPolicy::None => "none".to_string(),
        VerificationPolicy::HumanApproval => "human_approval".to_string(),
        VerificationPolicy::IndependentAgent => "independent_agent".to_string(),
        VerificationPolicy::AutomatedTests { command } => format!("automated_tests({command})"),
    }
}

/// Anzeige eines [`ExecutorKind`] für `status`/`items` (Phase 6, §13) — knapp,
/// analog zu [`verification_policy_str`].
fn executor_kind_str(e: &ExecutorKind) -> String {
    match e {
        ExecutorKind::SingleAgent => "single_agent".to_string(),
        ExecutorKind::Swarm { template } => format!("swarm:{template}"),
    }
}

/// Branch- und letzter-Commit-Anzeige für ein Item unter Git-Isolation
/// (Phase 7, §19) — `None`, wenn Isolation aus ist oder das Item nicht
/// schreibt (siehe `WorkItemKind::is_git_isolated`). Der Branchname selbst
/// ist deterministisch aus Projekt- und Item-ID ableitbar (`runner.rs`
/// benutzt dasselbe Schema), der Commit kommt aus dem letzten journalten
/// `ArtifactKind::GitCommit`-Artefakt dieses Items — `None`, wenn noch keiner
/// existiert (z. B. der Versuch hat nichts geändert).
fn git_item_display(
    snapshot: &WorkState,
    project: &WorkProject,
    item: &WorkItem,
) -> Option<(String, Option<String>)> {
    if !project.git_isolation || !item.kind.is_git_isolated() {
        return None;
    }
    let branch = item_branch_name(&project.id, &item.id);
    let commit = snapshot
        .artifacts
        .values()
        .filter(|a| a.work_item_id == item.id && a.kind == ArtifactKind::GitCommit)
        .max_by_key(|a| id_order(&a.id))
        .and_then(|a| a.commit_id.clone());
    Some((branch, commit))
}

/// Hinweis für `status`/`watch` (Befund 1 der Handprobe): das Repository
/// steht gerade auf einem Item-Branch DIESES Projekts, obwohl kein Lauf mehr
/// aktiv ist — der Fußabdruck eines abgestürzten Prozesses. `run`/`resume`
/// räumen das selbst auf, aber erst beim nächsten Start (siehe `cmd_run`);
/// zwischendurch ruft der Nutzer oft öfter `status`/`watch` auf (genau der
/// Grund für das sperrfreie Lesen aus Phase 8) und soll den Zustand nicht
/// erst beim nächsten Lauf erfahren.
///
/// `None`, wenn Git-Isolation aus ist, gerade wirklich ein Versuch läuft
/// (nicht abgelaufenes Lease — dann ist ein Item-Branch der normale, aktive
/// Zustand, kein Hinweis nötig), oder der aktuelle Branch gar kein
/// Item-Branch DIESES Projekts ist. Ein FREMDER Branch (kein
/// `work/<projekt-id>/…`) ist absichtlich KEIN Hinweis: das kann eine
/// bewusste Entscheidung des Nutzers sein (siehe `cmd_run`s Warnung, nicht
/// Korrektur, für genau diesen Fall) — `status` soll nicht bei jedem
/// manuellen Checkout aufblinken.
///
/// Bewusst NICHT `WorkRun::status != Running` als Kriterium (Befund des
/// Code-Reviews): `RunStatus` wechselt nur über `RunPaused`/`RunCompleted`/
/// `RunCanceled`, die alle in `runner::finish_run` journalt werden — auf
/// GENAU dem Pfad, den ein SIGKILL nie erreicht. Ein abgestürzter Lauf steht
/// also für immer auf `Running`, selbst wenn kein Prozess mehr lebt — dieses
/// Kriterium hätte den Hinweis exakt im Absturzfall unterdrückt, für den er
/// gedacht ist. `running_item_info`s Lease (`RunningItemInfo::
/// lease_remaining_secs`, negativ heißt abgelaufen — siehe dessen Doku) ist
/// das Kriterium, das diese Laufzeit selbst zur Unterscheidung "läuft noch
/// wirklich" von "toter Prozess" benutzt (`recovery::recover_all`).
fn git_stray_item_branch_note(
    project: &WorkProject,
    running: Option<&RunningItemInfo>,
) -> Option<String> {
    if !project.git_isolation {
        return None;
    }
    if running.is_some_and(|r| r.lease_remaining_secs >= 0) {
        return None;
    }
    let current = crate::git::current_branch(&project.workspace).ok()?;
    if !is_item_branch_of(&project.id, &current) {
        return None;
    }
    Some(format!(
        "Repository steht auf Item-Branch '{current}', obwohl kein Lauf aktiv ist — vermutlich \
         ein abgestürzter Prozess. 'agentkit work run/resume {}' wechselt beim nächsten Start \
         automatisch zurück auf den Ausgangsbranch.",
        project.id
    ))
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

/// Budgetverbrauch eines Laufs — gemeinsame Berechnung für `status` (Auftrag
/// B) und `watch` (Auftrag A): beide zeigen denselben Verbrauch, es soll
/// keine zweite, unabhängig gepflegte Kopie derselben Rechnung geben.
struct BudgetUsage {
    /// Wandzeit seit `WorkRun::started_at_ms`, in Sekunden — dieselbe
    /// Rechnung wie `scheduler::decide` für `max_wall_time_secs`, hier aber
    /// nur zur Anzeige (kein Bezug zu `budget.max_wall_time_secs` selbst).
    elapsed_wall_secs: u64,
    /// Anzahl Items im aktiven Lauf.
    item_count: usize,
    attempts_total: usize,
    artifacts_total: usize,
}

fn budget_usage(snapshot: &WorkState, run_id: Option<&str>, run: Option<&WorkRun>) -> BudgetUsage {
    let elapsed_wall_secs = run
        .map(|r| now_ms().saturating_sub(r.started_at_ms) / 1000)
        .unwrap_or(0);
    let item_count = match run_id {
        Some(rid) => snapshot
            .items
            .values()
            .filter(|it| it.run_id == rid)
            .count(),
        None => 0,
    };
    let attempts_total = match run_id {
        Some(rid) => snapshot
            .attempts
            .values()
            .filter(|a| {
                snapshot
                    .items
                    .get(&a.work_item_id)
                    .is_some_and(|it| it.run_id == rid)
            })
            .count(),
        None => 0,
    };
    BudgetUsage {
        elapsed_wall_secs,
        item_count,
        attempts_total,
        artifacts_total: snapshot.artifacts.len(),
    }
}

/// Das gerade laufende Item eines Laufs (Status `Running`) samt Agent,
/// aktuellem Versuch und verbleibender Lease-Zeit — `None`, wenn gerade
/// nichts läuft. Bei `max_parallel_agents == 1` (MVP-Fixwert) gibt es
/// höchstens eins; sortiert nach `seq`, falls dieser Wert je steigt.
///
/// `lease_remaining_secs` ist bewusst `i64`, nicht `u64`: ein abgelaufenes,
/// aber noch nicht von `recovery::recover_all` aufgeräumtes Lease (z. B.
/// während `watch` gerade zwischen zwei Aufrufen von `agentkit work
/// run`/`resume` liegt) soll als NEGATIVE verbleibende Zeit sichtbar sein,
/// nicht auf 0 zusammenklappen — der Unterschied zwischen "läuft seit 3
/// Minuten ab" und "läuft normal" ist genau die Information, die Auftrag A
/// hier verlangt.
struct RunningItemInfo {
    item_id: WorkItemId,
    title: String,
    agent_id: String,
    attempt_number: u32,
    max_attempts: u32,
    lease_remaining_secs: i64,
}

fn running_item_info(snapshot: &WorkState, run_id: &str, now_ms: u64) -> Option<RunningItemInfo> {
    let mut running: Vec<&WorkItem> = snapshot
        .items
        .values()
        .filter(|it| it.run_id == run_id && it.status == WorkItemStatus::Running)
        .collect();
    running.sort_by_key(|it| it.seq);
    let item = running.first()?;
    let lease = snapshot.leases.get(&item.id)?;
    let lease_remaining_secs = (lease.expires_at_ms as i64 - now_ms as i64) / 1000;
    Some(RunningItemInfo {
        item_id: item.id.clone(),
        title: item.title.clone(),
        agent_id: lease.agent_id.clone(),
        attempt_number: item.attempt_count + 1,
        max_attempts: item.max_attempts,
        lease_remaining_secs,
    })
}

/// Legt Lauf `R-1` an und journalt `RunStarted` — gemeinsamer Kern von
/// `cmd_create` (Erstanlage direkt nach `ProjectCreated`) und `cmd_run`
/// (Nachtrag, wenn Befund 3 zuschlägt: ein Absturz zwischen `ProjectCreated`
/// und `RunStarted` ließ das Projekt ohne Lauf zurück). `workspace` ist dabei
/// bewusst ein Parameter statt aus `store` gelesen: `cmd_create` kennt das
/// PERSISTIERTE `WorkProject` an dieser Stelle noch nicht (es wird gerade
/// erst gebaut), `cmd_run` liest es aus dem schon geladenen Snapshot.
///
/// `git_isolation` bestimmt, ob `WorkRun::base_branch` gesetzt wird (Befund 1
/// der Handprobe): ohne Git-Isolation ist `workspace` u. U. gar kein
/// Git-Repository, `current_branch` würde dort scheitern oder Unsinn liefern
/// — und ohne Item-Branches gibt es sowieso nichts wiederherzustellen. Ein
/// Fehler beim Ermitteln des Branches darf den Lauf nicht verhindern;
/// `base_branch` bleibt dann `None`, `cmd_run` lässt den Git-Zustand dann
/// unangetastet — dieselbe Ausfallsicherheit wie `base_revision`
/// (`git_head(...)` ist schon heute `Option`, kein `Result`).
///
/// Das wörtliche `"HEAD"` (ein losgelöster Arbeitsbaum, siehe
/// `git::current_branch`-Doku) zählt dabei NICHT als brauchbarer Branchname
/// (Befund des Code-Reviews): `git checkout HEAD` von einem Item-Branch aus
/// ist ein No-op, der Arbeitsbaum bliebe auf dem Item-Branch stehen, während
/// `recover_git_branch` fälschlich `Restored` melden würde — genau der
/// Zustand, den diese Korrektur beheben soll. Ein Lauf, der bei `create`
/// bereits losgelöst war, bekommt deshalb `base_branch: None` und damit
/// keinen automatischen Rückwechsel; das ist kein neuer Fehlerfall (ein
/// derart erstelltes Vorhaben hatte auch vorher keinen sinnvollen
/// Ausgangsbranch).
fn start_first_run(
    store: &WorkStore,
    project_id: &str,
    workspace: &str,
    git_isolation: bool,
) -> Result<(), WorkError> {
    let base_branch = if git_isolation {
        crate::git::current_branch(workspace)
            .ok()
            .filter(|b| b != "HEAD")
    } else {
        None
    };
    let run = WorkRun {
        id: "R-1".to_string(),
        project_id: project_id.to_string(),
        status: RunStatus::Running,
        started_at_ms: now_ms(),
        completed_at_ms: None,
        base_revision: git_head(workspace),
        base_branch,
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
            "--verify-command",
            "--format",
        ],
        &["--git-isolation"],
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

    let git_isolation = flags.has(&["--git-isolation"]);
    // Ablehnen, BEVOR überhaupt ein Projektverzeichnis entsteht (Phase 7,
    // §19): ein Vorhaben außerhalb eines Git-Repos soll eine klare deutsche
    // Meldung bekommen, nicht erst beim ersten 'run' auf einen kryptischen
    // Git-Fehler laufen ("fatal: not a git repository").
    if git_isolation && !crate::git::is_repo(&workspace) {
        let _ = writeln!(
            err,
            "[FEHLER] --git-isolation verlangt ein Git-Repository, aber '{workspace}' liegt in \
             keinem (kein 'git rev-parse --is-inside-work-tree'). Lege das Vorhaben ohne \
             --git-isolation an, oder initialisiere zuerst ein Repository ('git init')."
        );
        return ExitCode::GeneralError;
    }

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
        git_isolation,
    };
    if let Err(e) = store.submit(WorkEvent::ProjectCreated { project }) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }

    if let Err(e) = start_first_run(&store, &project_id, &workspace, git_isolation) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }

    // Default-Policy für Items OHNE eigene 'verification'-Angabe: mit
    // '--verify-command' automatisiert geprüft, sonst wie bisher `None`.
    let default_policy = match flags.value(&["--verify-command"]) {
        Some(cmd) if !cmd.trim().is_empty() => VerificationPolicy::AutomatedTests {
            command: cmd.trim().to_string(),
        },
        _ => VerificationPolicy::None,
    };
    if let Some(items_path) = flags.value(&["--items"]) {
        if let Err(msg) = load_items_file(&store, "R-1", &items_path, &default_policy) {
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
    #[serde(default)]
    verification: Option<VerificationField>,
    #[serde(default)]
    executor: Option<ExecutorField>,
}

/// Wire-Form des `verification`-Feldes der `--items`-Datei: `"none"`,
/// `{"automated_tests": "<befehl>"}` oder `"human_approval"`. Eigene,
/// handgeschriebene Form statt direkt gegen [`VerificationPolicy`]s
/// derive-Repräsentation zu deserialisieren (die verschachtelt
/// `AutomatedTests` als `{"automated_tests": {"command": "…"}}`) — die
/// Items-Datei ist eine Nutzerschnittstelle (dokumentiert im Hilfetext), das
/// Journal-Format eine interne Repräsentation; beide dürfen auseinanderlaufen.
#[derive(Deserialize)]
#[serde(untagged)]
enum VerificationField {
    Simple(String),
    AutomatedTests { automated_tests: String },
}

/// Wandelt das optionale `verification`-Feld eines Items in eine
/// [`VerificationPolicy`] — `None` (kein Feld angegeben) übernimmt
/// `fallback` (aus `--verify-command`, sonst `VerificationPolicy::None`):
/// „ohne eigene Angabe" (siehe Hilfetext) heißt wörtlich, dass das Feld ganz
/// fehlt, nicht, dass es explizit `\"none\"` trägt.
fn resolve_verification_policy(
    field: Option<VerificationField>,
    fallback: &VerificationPolicy,
) -> Result<VerificationPolicy, String> {
    match field {
        None => Ok(fallback.clone()),
        Some(VerificationField::Simple(s)) if s == "none" => Ok(VerificationPolicy::None),
        Some(VerificationField::Simple(s)) if s == "human_approval" => {
            Ok(VerificationPolicy::HumanApproval)
        }
        Some(VerificationField::Simple(s)) if s == "independent_agent" => {
            Ok(VerificationPolicy::IndependentAgent)
        }
        Some(VerificationField::Simple(s)) => Err(format!(
            "unbekannter Wert für 'verification': '{s}' — erlaubt sind: \"none\", \
             {{\"automated_tests\": \"<befehl>\"}}, \"human_approval\", \"independent_agent\""
        )),
        Some(VerificationField::AutomatedTests { automated_tests }) => {
            let command = automated_tests.trim();
            if command.is_empty() {
                return Err("'verification.automated_tests' darf nicht leer sein".to_string());
            }
            Ok(VerificationPolicy::AutomatedTests {
                command: command.to_string(),
            })
        }
    }
}

/// Wire-Form des `executor`-Feldes der `--items`-Datei (Phase 6, §13):
/// `"single_agent"` oder `{"swarm": "<vorlage>"}` — bewusst FLACH
/// (`{"swarm": "review"}`, nicht `{"swarm": {"template": "review"}}`), anders
/// als die interne Journal-Repräsentation von [`crate::model::ExecutorKind`]:
/// dieselbe Trennung „Nutzerschnittstelle vs. interne Repräsentation" wie bei
/// [`VerificationField`].
#[derive(Deserialize)]
#[serde(untagged)]
enum ExecutorField {
    Simple(String),
    Swarm { swarm: String },
}

/// Wandelt das optionale `executor`-Feld eines Items in ein [`ExecutorKind`] —
/// `None` (kein Feld angegeben) heißt `ExecutorKind::SingleAgent`, wie schon
/// vor Phase 6. Der Vorlagenname selbst wird hier NICHT geprüft (welche
/// Vorlagen es gibt, weiß nur das Frontend, siehe `ExecutorKind`-Doku) — nur
/// die Wire-Form muss stimmen.
fn resolve_executor_kind(field: Option<ExecutorField>) -> Result<ExecutorKind, String> {
    match field {
        None => Ok(ExecutorKind::SingleAgent),
        Some(ExecutorField::Simple(s)) if s == "single_agent" => Ok(ExecutorKind::SingleAgent),
        Some(ExecutorField::Simple(s)) => Err(format!(
            "unbekannter Wert für 'executor': '{s}' — erlaubt sind: \"single_agent\", \
             {{\"swarm\": \"<vorlage>\"}}"
        )),
        Some(ExecutorField::Swarm { swarm }) => {
            let template = swarm.trim();
            if template.is_empty() {
                return Err("'executor.swarm' darf nicht leer sein".to_string());
            }
            Ok(ExecutorKind::Swarm {
                template: template.to_string(),
            })
        }
    }
}

/// Legt die Items einer `--items`-Datei in Dateireihenfolge an. `depends_on`
/// darf nur auf zuvor in DIESEM Aufruf angelegte Items verweisen — das prüft
/// [`WorkState::validate_dependencies`] implizit: ein Vorwärts-Verweis zeigt
/// auf eine ID, die es im Zustand noch nicht gibt, und wird als „existiert
/// nicht" abgelehnt. Ein Zyklus ist damit strukturell unmöglich (§ siehe
/// Plan), nicht nur durch eine Konvention.
fn load_items_file(
    store: &WorkStore,
    run_id: &str,
    path: &str,
    default_policy: &VerificationPolicy,
) -> Result<(), String> {
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

    for (idx, entry) in entries.into_iter().enumerate() {
        let position = idx + 1;
        let title = entry.title.trim();
        if title.is_empty() {
            return Err(format!(
                "Item Position {position}: title darf nicht leer sein"
            ));
        }
        let title = title.to_string();
        let description = entry.description.trim();
        if description.is_empty() {
            return Err(format!(
                "Item Position {position} ('{title}'): description darf nicht leer sein"
            ));
        }
        let description = description.to_string();
        let Some(kind) = parse_item_kind(&entry.kind) else {
            return Err(format!(
                "Item Position {position} ('{title}'): unbekannte kind '{}' — erlaubt sind: {ITEM_KINDS_HELP}",
                entry.kind
            ));
        };
        // 'integration' ist der Laufzeit vorbehalten (Phase 7, automatisches
        // Merge-Item) — `parse_item_kind` deserialisiert generisch gegen jede
        // `WorkItemKind`-Variante, ITEM_KINDS_HELP listet den Wert deshalb
        // bewusst nicht, aber ohne diese zweite Prüfung könnte eine
        // `--items`-Datei ihn trotzdem angeben.
        if kind == WorkItemKind::Integration {
            return Err(format!(
                "Item Position {position} ('{title}'): kind 'integration' ist der Laufzeit \
                 vorbehalten (siehe README-Abschnitt 'Git-Isolation') und kann nicht manuell \
                 angelegt werden — erlaubt sind: {ITEM_KINDS_HELP}"
            ));
        }
        let priority = entry.priority.unwrap_or(5);
        if priority > 9 {
            return Err(format!(
                "Item Position {position} ('{title}'): priority muss zwischen 0 und 9 liegen, war {priority}"
            ));
        }
        let verification_policy =
            resolve_verification_policy(entry.verification, default_policy)
                .map_err(|e| format!("Item Position {position} ('{title}'): {e}"))?;
        let executor = resolve_executor_kind(entry.executor)
            .map_err(|e| format!("Item Position {position} ('{title}'): {e}"))?;

        let snapshot = store.snapshot();
        let id = snapshot.next_item_id();
        if let Err(e) = snapshot.validate_dependencies(&id, &entry.depends_on) {
            return Err(format!("Item Position {position} ('{title}', {id}): {e}"));
        }

        let item = WorkItem {
            id: id.clone(),
            run_id: run_id.to_string(),
            title,
            description,
            kind,
            status: WorkItemStatus::Pending,
            priority,
            seq: id_order(&id),
            required_role: entry.required_role,
            dependencies: entry.depends_on,
            acceptance_criteria: entry.acceptance_criteria,
            verification_policy,
            // Items aus einer '--items'-Datei sind nie Prüf-Items — die legt
            // ausschließlich die Laufzeit selbst an (`runner::spawn_review_item`).
            verifies: None,
            claims_promoted: false,
            executor,
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

    // Befund 0 (Handprobe): sperrfrei geöffnet — vorher scheiterte jedes
    // gerade laufende Projekt hier mit `WorkError::Locked` und fehlte in der
    // Liste (mit einem Hinweis auf stderr, "gesperrt, läuft vermutlich
    // gerade"). Ein Leser braucht keine Sperre mehr, also gibt es diesen Fall
    // nicht mehr — nur ein Verzeichnis ohne lesbares Journal (echter
    // Datenmüll) wird weiterhin stillschweigend übersprungen.
    let mut projects: Vec<WorkProject> = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(state) = WorkStore::open_read_only(entry.path()) {
                if let Some(project) = state.project {
                    projects.push(project);
                }
            }
        }
    }
    projects.sort_by(|a, b| a.id.cmp(&b.id));

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
    // Zweite, vom Lease unabhängige Recovery-Lücke (Phase 5b, §11): Items mit
    // bestandener Verifikation, deren Claims noch nicht promotet sind (Absturz
    // zwischen `WorkItemCompleted` und `ClaimsPromoted`, oder ein vorheriger
    // Promotionsversuch, der am Gateway gescheitert ist). Ohne Gateway ein No-Op.
    for msg in recovery::recover_pending_promotions(&store, deps.graph.as_ref(), now_ms()) {
        render_progress(err, WorkProgress::Note(msg), show_steps);
    }

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
            if let Err(e) = start_first_run(
                &store,
                &project_id,
                &project.workspace,
                project.git_isolation,
            ) {
                report_work_error(err, &e);
                return ExitCode::GeneralError;
            }
            "R-1".to_string()
        }
    };

    warn_workspace_mismatch(err, &locate_workspace, &project.workspace);

    // Zweite, spätere Absicherung derselben Prüfung wie in `cmd_create`
    // (Phase 7, §19): das Projekt kann angelegt worden sein, während sein
    // Workspace noch ein Git-Repo war, das inzwischen entfernt/verschoben
    // wurde — auch dann eine klare deutsche Meldung statt eines
    // Git-Fehlers mitten im ersten Versuch.
    if project.git_isolation && !crate::git::is_repo(&project.workspace) {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}' verlangt Git-Isolation, aber Workspace \
             '{}' liegt in keinem Git-Repository.",
            project.workspace
        );
        return ExitCode::GeneralError;
    }

    // Befund 1 der Handprobe (Ausgangsbranch nach hartem Prozessabbruch):
    // die eigentliche Reparatur läuft über `recovery::recover_git_branch`,
    // GENAU wie bei `recover_all`/`recover_pending_promotions` oben — hier
    // wird nur noch das Ergebnis auf stderr gerendert.
    match recovery::recover_git_branch(&store, &run_id) {
        recovery::GitBranchRecovery::Inactive | recovery::GitBranchRecovery::AlreadyOnBase => {}
        recovery::GitBranchRecovery::Restored { from, to } => {
            let _ = writeln!(
                err,
                "[work] Repository stand auf Item-Branch '{from}' (vermutlich ein abgestürzter \
                 Lauf) — zurück auf Ausgangsbranch '{to}' gewechselt."
            );
        }
        // Befund des Code-Reviews: ein SIGKILL committet nie (das übernimmt
        // erst `record_success`), uncommittete Änderungen auf dem Item-Branch
        // sind also der REGELFALL nach einem Absturz, nicht die Ausnahme.
        // Ein Checkout hätte sie — sofern sie mit dem Ausgangsbranch nicht
        // kollidieren — klaglos MIT auf den Ausgangsbranch genommen: kein
        // Datenverlust, aber eine stille Verschiebung außerhalb des
        // Item-Branches, dem die Provenance sie zuordnet. Deshalb hier ein
        // harter Abbruch statt eines automatischen Wechsels.
        recovery::GitBranchRecovery::DirtyWorkingTree { current, base } => {
            let _ = writeln!(
                err,
                "[FEHLER] Repository steht auf Item-Branch '{current}' mit uncommitteten \
                 Änderungen (vermutlich ein Absturz mitten in einem Versuch) — kein \
                 automatischer Wechsel auf '{base}', das würde die Änderungen sonst \
                 stillschweigend mitnehmen. Prüfe den Stand ('git status' auf '{current}'), \
                 committe oder verwirf ihn manuell, und wechsle danach selbst zurück ('git \
                 checkout {base}')."
            );
            return ExitCode::GeneralError;
        }
        recovery::GitBranchRecovery::ForeignBranch { current, base } => {
            let _ = writeln!(
                err,
                "[WARNUNG] Repository steht auf Branch '{current}', nicht auf dem \
                 Ausgangsbranch '{base}' dieses Laufs — der Lauf wird auf '{current}' \
                 fortgesetzt, statt automatisch zurückzuwechseln."
            );
        }
        recovery::GitBranchRecovery::BranchLookupFailed { base: _, error } => {
            let _ = writeln!(
                err,
                "[WARNUNG] aktueller Branch nicht ermittelbar ({error}) — der Lauf wird \
                 unverändert fortgesetzt."
            );
        }
        recovery::GitBranchRecovery::RestoreFailed { from, to, error } => {
            let _ = writeln!(
                err,
                "[FEHLER] Repository stand auf Item-Branch '{from}', der Zustand des \
                 Arbeitsbaums war nicht sicher feststellbar oder der Wechsel zurück auf \
                 Ausgangsbranch '{to}' ist unerwartet fehlgeschlagen: {error}. Prüfe den Stand \
                 manuell ('git status'), bevor der Lauf fortgesetzt wird."
            );
            return ExitCode::GeneralError;
        }
    }

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
    let single_agent_executor = CodingAgentExecutor {
        llm,
        approve,
        extra_tools: deps.extra_tools.clone(),
        cancel: deps.cancel.clone(),
        dry_run,
        shell_timeout: 120,
        system_extra: None,
    };
    // Ohne `build_executor` (kein Frontend mit Schwarm-Fähigkeit, z. B. die
    // Tests dieses Crates) läuft der Lauf exakt wie vor Phase 6 — der
    // Einzelagenten-Executor unverändert. Mit `build_executor` entscheidet
    // der von dort gebaute Dispatcher je Versuch anhand von
    // `pkg.item.executor`, ob ein Schwarm oder der Einzelagent läuft.
    let executor: Box<dyn AgentExecutor> = match &deps.build_executor {
        Some(build) => build(single_agent_executor),
        None => Box::new(single_agent_executor),
    };
    let runner_cfg = RunnerConfig {
        agent_id: "worker-1".to_string(),
        lease_secs: 600,
        heartbeat_secs: 30,
        workspace: project.workspace.clone(),
        graph: deps.graph.clone(),
    };

    // Welches Item gerade läuft — nur für die Beschriftung im Trace (siehe
    // `mit_item`). Der Fortschritts-Strom selbst bleibt unverändert.
    let mut laufendes_item = String::new();
    let outcome = run_to_completion(
        &store,
        &run_id,
        executor.as_ref(),
        &runner_cfg,
        &deps.cancel,
        &mut |p| {
            // Der Mitschnitt VOR der Anzeige: was auf stderr landet, hängt an
            // `--steps`, was in den Trace geht, nicht.
            if let Some(trace) = &deps.trace {
                match &p {
                    // Der Runner nennt das Item, bevor dessen Ereignisse
                    // kommen — daraus entsteht das `source`-Tag.
                    WorkProgress::ItemStarted { item, attempt, .. } => {
                        laufendes_item = format!("{item}#{attempt}");
                    }
                    WorkProgress::Agent(ev) => trace.write_event(&mit_item(ev, &laufendes_item)),
                    _ => {}
                }
            }
            render_progress(err, p, show_steps)
        },
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

/// Beschriftet ein Agent-Ereignis mit dem Work Item, in dem es entstand.
///
/// Ohne das trüge JEDES Ereignis eines Work-Laufs ein leeres `source` und der
/// Betrachter zeigte fünf Items als EINEN Agenten. Das Tag ist `W-1#2` (Item
/// und Versuch); Schwarm-Mitglieder behalten ihre eigene Kennung dahinter
/// (`W-1#2/explorer-a`), sonst wären zwei Mitglieder in zwei Items nicht
/// auseinanderzuhalten. Nur für den TRACE — der Fortschritts-Strom, den ein
/// anderer Aufrufer sieht, bleibt unverändert.
fn mit_item(ev: &AgentEvent, item: &str) -> AgentEvent {
    if item.is_empty() {
        return ev.clone();
    }
    let source = if ev.source.is_empty() {
        item.to_string()
    } else {
        format!("{item}/{}", ev.source)
    };
    AgentEvent {
        source,
        ..ev.clone()
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
        // `Structured` ist Nutzlast für Konsumenten, die `kind` kennen (Trace,
        // Betrachter) — nichts für die Konsole: die Schwarm-Ereignisse eines
        // Schwarm-Work-Items kommen daneben schon als lesbare Tool-Zeile an.
        EventData::Structured { .. }
        | EventData::TextDelta(_)
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
    // Befund 0 (Handprobe): `status` liest nur — sperrfrei geöffnet, damit es
    // funktioniert, während `agentkit work run` im selben Verzeichnis die
    // Sperre hält.
    let Some(snapshot) = open_project_read_only(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
    let Some(project) = &snapshot.project else {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}': kein Projekt im Journal."
        );
        return ExitCode::GeneralError;
    };
    let run_id = latest_run_id(&snapshot);
    let run = run_id.as_ref().and_then(|id| snapshot.runs.get(id));
    // Auftrag B: Budgetverbrauch (Wandzeit/Items gegen die Limits, Versuche,
    // Artefakte) und — falls eins läuft — die verbleibende Lease-Zeit.
    let usage = budget_usage(&snapshot, run_id.as_deref(), run);
    let running = run_id
        .as_deref()
        .and_then(|rid| running_item_info(&snapshot, rid, now_ms()));
    let git_stray_note = git_stray_item_branch_note(project, running.as_ref());

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for kind in [
        "pending",
        "running",
        "awaiting_verification",
        "completed",
        "failed",
        "canceled",
    ] {
        counts.insert(kind.to_string(), 0);
    }
    let mut blocked_items = Vec::new();
    let mut waiting_items = Vec::new();
    // Items in `AwaitingVerification` — je Item, worauf gewartet wird, damit
    // der Nutzer sieht, ob (und wie) er eingreifen muss (Policy
    // `human_approval`) oder ob eine automatisierte Prüfung nur kurz läuft.
    let mut awaiting_verification_items: Vec<Value> = Vec::new();
    // Items mit `ExecutorKind::Swarm` (Phase 6, §13) — nur gesammelt, wenn es
    // welche gibt (siehe Anzeige unten): ein Vorhaben ohne Schwarm-Items soll
    // keine leere Zeile bekommen.
    let mut swarm_items: Vec<(String, String)> = Vec::new();
    // Items unter Git-Isolation (Phase 7, §19) mit Branch/letztem Commit —
    // analog zu `swarm_items`: nur gesammelt (und angezeigt), wenn es welche
    // gibt.
    let mut git_items: Vec<(String, String, Option<String>)> = Vec::new();
    if let Some(rid) = &run_id {
        for item in snapshot.items.values().filter(|it| &it.run_id == rid) {
            *counts.entry(item.status.to_string()).or_insert(0) += 1;
            if !snapshot.blocked_by(&item.id).is_empty() {
                blocked_items.push(item.id.clone());
            } else if !snapshot.waiting_on(&item.id).is_empty() {
                waiting_items.push(item.id.clone());
            }
            if let ExecutorKind::Swarm { template } = &item.executor {
                swarm_items.push((item.id.clone(), template.clone()));
            }
            if let Some((branch, commit)) = git_item_display(&snapshot, project, item) {
                git_items.push((item.id.clone(), branch, commit));
            }
            if item.status == WorkItemStatus::AwaitingVerification {
                let hint = match &item.verification_policy {
                    VerificationPolicy::HumanApproval => {
                        format!("agentkit work approve|reject {} -p {}", item.id, project.id)
                    }
                    VerificationPolicy::AutomatedTests { command } => {
                        // `work.lock` sperrt das ganze Projektverzeichnis
                        // während eines Laufs (auch für `status`) — dieser
                        // Zustand ist hier also NIE "die Prüfung läuft gerade"
                        // (das löst synchron im selben Versuch auf), sondern
                        // immer ein Absturz mitten in der Prüfung (Befund des
                        // Code-Reviews). `work run`/`resume` räumt ihn beim
                        // nächsten Start automatisch auf (`recovery`).
                        format!(
                            "automatisierte Prüfung ('{command}') durch einen Absturz \
                             unterbrochen — 'agentkit work run/resume {}' räumt automatisch auf",
                            project.id
                        )
                    }
                    VerificationPolicy::IndependentAgent => format!(
                        "wartet auf ein automatisch angelegtes Prüf-Item (Kind 'review') — \
                         siehe 'agentkit work items {}'",
                        project.id
                    ),
                    VerificationPolicy::None => "-".to_string(),
                };
                awaiting_verification_items.push(json!({
                    "id": item.id,
                    "policy": verification_policy_str(&item.verification_policy),
                    "hint": hint,
                }));
            }
        }
    }
    swarm_items.sort_by_key(|(id, _)| id_order(id));
    git_items.sort_by_key(|(id, _, _)| id_order(id));

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
                "awaiting_verification_items": awaiting_verification_items,
                "swarm_items": swarm_items.iter().map(|(id, template)| json!({
                    "id": id,
                    "template": template,
                })).collect::<Vec<_>>(),
                "git_items": git_items.iter().map(|(id, branch, commit)| json!({
                    "id": id,
                    "branch": branch,
                    "commit": commit,
                })).collect::<Vec<_>>(),
                "git_stray_branch_note": git_stray_note,
                "attempts_total": usage.attempts_total,
                "artifacts_total": usage.artifacts_total,
                "elapsed_wall_secs": usage.elapsed_wall_secs,
                "item_count": usage.item_count,
                "running_item": running.as_ref().map(|r| json!({
                    "id": r.item_id,
                    "title": r.title,
                    "agent_id": r.agent_id,
                    "attempt": r.attempt_number,
                    "max_attempts": r.max_attempts,
                    "lease_remaining_secs": r.lease_remaining_secs,
                })),
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
            if !awaiting_verification_items.is_empty() {
                let lines: Vec<String> = awaiting_verification_items
                    .iter()
                    .map(|v| {
                        format!(
                            "{} ({})",
                            v["id"].as_str().unwrap_or("?"),
                            v["hint"].as_str().unwrap_or("?")
                        )
                    })
                    .collect();
                let _ = writeln!(out, "Wartet auf Freigabe: {}", lines.join("; "));
            }
            // Nur anzeigen, wenn es welche gibt (Phase 6, §13) — dieselbe
            // Zurückhaltung wie bei `Blockiert`/`Wartend` oben.
            if !swarm_items.is_empty() {
                let lines: Vec<String> = swarm_items
                    .iter()
                    .map(|(id, template)| format!("{id} ({template})"))
                    .collect();
                let _ = writeln!(out, "Schwarm-Items : {}", lines.join(", "));
            }
            // Nur anzeigen, wenn es welche gibt (Phase 7, §19) — dieselbe
            // Zurückhaltung wie bei `Schwarm-Items` oben.
            if !git_items.is_empty() {
                let lines: Vec<String> = git_items
                    .iter()
                    .map(|(id, branch, commit)| match commit {
                        Some(c) => format!("{id} ({branch} @ {c})"),
                        None => format!("{id} ({branch}, noch kein Commit)"),
                    })
                    .collect();
                let _ = writeln!(out, "Git-Items     : {}", lines.join(", "));
            }
            if let Some(note) = &git_stray_note {
                let _ = writeln!(out, "Hinweis       : {note}");
            }
            let wall_limit = project
                .budget
                .max_wall_time_secs
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let item_limit = project
                .budget
                .max_work_items
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(
                out,
                "Laufzeit      : {}s / {wall_limit}s",
                usage.elapsed_wall_secs
            );
            let _ = writeln!(out, "Items gesamt  : {} / {item_limit}", usage.item_count);
            let _ = writeln!(out, "Versuche      : {}", usage.attempts_total);
            let _ = writeln!(out, "Artefakte     : {}", usage.artifacts_total);
            if let Some(r) = &running {
                let _ = writeln!(
                    out,
                    "Aktuelles Item: {} '{}' — Agent {}, Versuch {}/{}, Lease {}",
                    r.item_id,
                    r.title,
                    r.agent_id,
                    r.attempt_number,
                    r.max_attempts,
                    format_lease_remaining(r.lease_remaining_secs),
                );
            }
        }
    }
    ExitCode::Success
}

/// Anzeige der verbleibenden Lease-Zeit — negativ heißt "abgelaufen, aber
/// noch nicht von `recovery::recover_all` aufgeräumt" (siehe
/// `RunningItemInfo`-Doku), nicht "0 Sekunden".
fn format_lease_remaining(secs: i64) -> String {
    if secs >= 0 {
        format!("{secs}s verbleibend")
    } else {
        format!("seit {}s abgelaufen", -secs)
    }
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
    // Befund 0 (Handprobe): `items` liest nur — sperrfrei geöffnet, wie `status`.
    let Some(snapshot) = open_project_read_only(&root, &project_id, err) else {
        return ExitCode::GeneralError;
    };
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

    // Für Branch/Commit je Item (Phase 7, §19) — `None`, wenn kein Projekt im
    // Journal steht (dann gäbe es auch keine Items, s.o.).
    let project = snapshot.project.clone();

    match format {
        OutputFormat::Json => {
            let doc: Vec<Value> = items
                .iter()
                .map(|it| {
                    let git = project
                        .as_ref()
                        .and_then(|p| git_item_display(&snapshot, p, it));
                    json!({
                        "id": it.id,
                        "status": it.status.to_string(),
                        "priority": it.priority,
                        "title": it.title,
                        "dependencies": it.dependencies,
                        "attempt_count": it.attempt_count,
                        "max_attempts": it.max_attempts,
                        "blocked": !snapshot.blocked_by(&it.id).is_empty(),
                        "verification_policy": verification_policy_str(&it.verification_policy),
                        "executor": executor_kind_str(&it.executor),
                        "branch": git.as_ref().map(|(b, _)| b.clone()),
                        "commit": git.as_ref().and_then(|(_, c)| c.clone()),
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
                } else if it.status == WorkItemStatus::AwaitingVerification {
                    " [WARTET AUF FREIGABE]"
                } else {
                    ""
                };
                // Nur anzeigen, wenn abweichend vom Regelfall (Phase 6, §13) —
                // ein Einzelagent ist keine Information wert, die in jeder
                // Zeile wiederholt wird.
                let executor = match &it.executor {
                    ExecutorKind::SingleAgent => String::new(),
                    ExecutorKind::Swarm { template } => format!(" [SCHWARM: {template}]"),
                };
                // Nur anzeigen, wenn Git-Isolation an ist UND dieses Item
                // einen eigenen Branch bekommt (Phase 7, §19) — dieselbe
                // Zurückhaltung wie beim Schwarm-Marker oben.
                let git = project
                    .as_ref()
                    .and_then(|p| git_item_display(&snapshot, p, it))
                    .map(|(branch, commit)| match commit {
                        Some(c) => format!(" [GIT: {branch} @ {c}]"),
                        None => format!(" [GIT: {branch}]"),
                    })
                    .unwrap_or_default();
                let deps = if it.dependencies.is_empty() {
                    "-".to_string()
                } else {
                    it.dependencies.join(",")
                };
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}\t{}/{}\t{}{blocked}{executor}{git}",
                    it.id,
                    it.status,
                    it.priority,
                    it.title,
                    deps,
                    it.attempt_count,
                    it.max_attempts,
                    verification_policy_str(&it.verification_policy),
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
    let tail_entries = match read_tail_events(&journal_path, tail) {
        Ok(entries) => entries,
        Err(e) => {
            let _ = writeln!(
                err,
                "[FEHLER] Journal '{}' nicht lesbar: {e}",
                journal_path.display()
            );
            return ExitCode::GeneralError;
        }
    };

    match format {
        OutputFormat::Json => {
            let _ = writeln!(out, "{}", Value::Array(tail_entries));
        }
        OutputFormat::Text => {
            if tail_entries.is_empty() {
                let _ = writeln!(out, "Keine Ereignisse.");
            }
            for entry in &tail_entries {
                let at = entry.get("at").and_then(Value::as_u64).unwrap_or(0);
                let _ = writeln!(out, "[{at}] {}", format_event_line(entry));
            }
        }
    }
    ExitCode::Success
}

/// Liest die letzten `tail` Journal-Zeilen als rohe JSON-Werte — gemeinsamer
/// Kern von `cmd_events` und dem Zeitleisten-Abschnitt von `cmd_watch` (siehe
/// Moduldoku dort: reine Anzeige, keine Zustands-Autorität, deshalb dieselbe
/// Toleranz gegenüber kaputten/abgeschnittenen Zeilen — JEDE, nicht nur die
/// letzte). Ein fehlendes Journal (frisches Projekt) ist kein Fehler, nur ein
/// echter I/O-Fehler ist einer.
fn read_tail_events(journal_path: &Path, tail: usize) -> std::io::Result<Vec<Value>> {
    let content = match fs::read_to_string(journal_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let entries: Vec<Value> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let start = entries.len().saturating_sub(tail);
    Ok(entries[start..].to_vec())
}

/// Der Ereignis-Name einer Journal-Zeile für die Anzeige — gemeinsamer Kern
/// von `cmd_events` und `cmd_watch`. Eine Snapshot-Zeile hat kein
/// "event.kind"-Feld (sie trägt den ganzen `WorkState`, kein einzelnes
/// Ereignis), aber ihr einziger Erzeuger ist `WorkStore::checkpoint` —
/// "checkpoint_created" ist hier also keine Erfindung, sondern der Name des
/// Ereignisses, das diese Zeile ausgelöst hat.
fn journal_entry_kind(entry: &Value) -> &str {
    entry
        .get("event")
        .and_then(|e| e.get("kind"))
        .and_then(Value::as_str)
        .or_else(|| entry.get("snapshot").map(|_| "checkpoint_created"))
        .unwrap_or("?")
}

/// Anzeigezeile für EIN Ereignis in `agentkit work events` (Text-Format).
/// Code-Review-Befund 1: ein `artifact_created` mit `artifact.kind ==
/// "git_commit"` (siehe `runner::record_git_commit`) bekommt hier eine
/// sprechende Zeile mit Commit und Branch, statt der generischen
/// `journal_entry_kind`-Zeile — Anzeigelogik gehört in die Anzeige, nicht in
/// ein eigenes Journal-Ereignis (das frühere `WorkEvent::GitCommitted` war
/// ein No-op in `state::apply` und duplizierte nur das Artefakt). Jedes
/// andere Ereignis bleibt bei `journal_entry_kind` unverändert — insbesondere
/// `cmd_watch`s Zeitleiste, die diese Funktion bewusst NICHT verwendet.
fn format_event_line(entry: &Value) -> String {
    let kind = journal_entry_kind(entry);
    if kind == "artifact_created" {
        if let Some(artifact) = entry.get("event").and_then(|e| e.get("artifact")) {
            if artifact.get("kind").and_then(Value::as_str) == Some("git_commit") {
                let commit = artifact
                    .get("commit_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let branch = artifact
                    .get("rel_path")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                return format!("git_commit: Commit {commit} auf Branch '{branch}'");
            }
        }
    }
    kind.to_string()
}

// -------------------------------------------------------------------- watch

/// Sortierte, komma-getrennte Anzeige einer Item-ID-Liste — gemeinsamer Kern
/// für alle `Decision`-Zweige unten, die eine Liste von IDs tragen
/// (`Blocked`/`AwaitingVerification`): `scheduler::decide` liefert sie in
/// keiner garantierten Reihenfolge, eine Anzeige soll aber stabil sein.
fn sorted_ids(ids: &[WorkItemId]) -> String {
    let mut v = ids.to_vec();
    v.sort_by_key(|id| id_order(id));
    v.join(", ")
}

/// Worauf der Lauf gerade wartet — Auftrag A verlangt das AUSDRÜCKLICH
/// (Freigabe, Blockade, Budget). Baut direkt auf `scheduler::decide` auf,
/// statt die Logik ein zweites Mal nachzubilden: derselbe deterministische
/// Kern, den auch `runner::run_to_completion` je Runde befragt.
///
/// `Decision::AtCapacity` heißt bei `max_parallel_agents == 1`: entweder
/// läuft gerade ein Versuch (dann zeigt `running` ihn), oder — falls
/// `watch` gerade läuft, während KEIN `agentkit work run`/`resume` aktiv ist
/// — alles Offene hängt an einem Item, das laut Zustand `Running` ist, aber
/// dessen Worker nicht mehr lebt (das räumt der nächste `run`/`resume` über
/// `recovery::recover_all` auf). `Decision::Run(id)` heißt: nichts läuft
/// gerade, `id` wäre das nächste, SOBALD ein Worker-Prozess läuft — reine
/// Vorschau, `watch` selbst startet nie etwas.
fn watch_waiting_str(decision: Option<&Decision>, running: Option<&RunningItemInfo>) -> String {
    match decision {
        None => "-".to_string(),
        Some(Decision::Done) => "abgeschlossen — alle Items fertig".to_string(),
        Some(Decision::BudgetExhausted(msg)) => format!("Budget erschöpft: {msg}"),
        Some(Decision::Blocked(ids)) => format!("blockiert: {}", sorted_ids(ids)),
        Some(Decision::AwaitingVerification(ids)) => {
            format!("wartet auf Freigabe: {}", sorted_ids(ids))
        }
        Some(Decision::AtCapacity) => match running {
            Some(r) => format!("läuft: {} (Agent {})", r.item_id, r.agent_id),
            None => "wartet — kein 'agentkit work run/resume' aktiv?".to_string(),
        },
        Some(Decision::Run(id)) => {
            format!("bereit: {id} würde starten, sobald 'agentkit work run' läuft")
        }
    }
}

/// Baut GENAU EIN Dokument der Watch-Ansicht und schreibt es nach `out`/`err`
/// — der Kern von `cmd_watch`: sowohl die interaktive Schleife (Neuzeichnen
/// je Intervall) als auch der Einmal-Pfad (`--format json`, oder stdout ist
/// kein Terminal) rufen dieselbe Funktion, damit es nur EINE Stelle gibt, die
/// den Inhalt zusammensetzt.
fn render_watch_once(
    dir: &Path,
    project_id: &str,
    format: OutputFormat,
    tail_n: usize,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let snapshot = match WorkStore::open_read_only(dir) {
        Ok(s) => s,
        Err(e) => {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    };
    let Some(project) = &snapshot.project else {
        let _ = writeln!(
            err,
            "[FEHLER] Projekt '{project_id}': kein Projekt im Journal."
        );
        return ExitCode::GeneralError;
    };
    let run_id = latest_run_id(&snapshot);
    let run = run_id.as_ref().and_then(|id| snapshot.runs.get(id));
    let now = now_ms();
    let usage = budget_usage(&snapshot, run_id.as_deref(), run);
    let running = run_id
        .as_deref()
        .and_then(|rid| running_item_info(&snapshot, rid, now));
    let decision = match (&run_id, run) {
        (Some(rid), Some(r)) => Some(scheduler::decide(
            &snapshot,
            rid,
            &project.budget,
            r.started_at_ms,
            now,
        )),
        _ => None,
    };
    let waiting = watch_waiting_str(decision.as_ref(), running.as_ref());
    let tail_entries = read_tail_events(&dir.join(JOURNAL_FILE), tail_n).unwrap_or_default();
    let git_stray_note = git_stray_item_branch_note(project, running.as_ref());

    let mut items: Vec<&WorkItem> = match &run_id {
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
            let doc = json!({
                "project_id": project.id,
                "title": project.title,
                "status": project_status_str(project.status),
                "run_id": run_id,
                "run_status": run.map(|r| run_status_str(r.status)),
                "items": items.iter().map(|it| json!({
                    "id": it.id,
                    "title": it.title,
                    "status": it.status.to_string(),
                    "priority": it.priority,
                    "attempt_count": it.attempt_count,
                    "max_attempts": it.max_attempts,
                    "executor": executor_kind_str(&it.executor),
                    "verification_policy": verification_policy_str(&it.verification_policy),
                })).collect::<Vec<_>>(),
                "running_item": running.as_ref().map(|r| json!({
                    "id": r.item_id,
                    "title": r.title,
                    "agent_id": r.agent_id,
                    "attempt": r.attempt_number,
                    "max_attempts": r.max_attempts,
                    "lease_remaining_secs": r.lease_remaining_secs,
                })),
                "budget_usage": {
                    "elapsed_wall_secs": usage.elapsed_wall_secs,
                    "max_wall_time_secs": project.budget.max_wall_time_secs,
                    "item_count": usage.item_count,
                    "max_work_items": project.budget.max_work_items,
                    "attempts_total": usage.attempts_total,
                    "artifacts_total": usage.artifacts_total,
                },
                "waiting": waiting,
                "events_tail": tail_entries,
                "git_stray_branch_note": git_stray_note,
            });
            let _ = writeln!(out, "{doc}");
        }
        OutputFormat::Text => {
            let _ = writeln!(out, "Projekt : {} ({})", project.title, project.id);
            let _ = writeln!(
                out,
                "Status  : {}{}",
                project_status_str(project.status),
                run.map(|r| format!(
                    " — Lauf {} {}",
                    run_id.as_deref().unwrap_or("?"),
                    run_status_str(r.status)
                ))
                .unwrap_or_default()
            );
            let _ = writeln!(out, "\nWork Items:");
            if items.is_empty() {
                let _ = writeln!(out, "  (keine)");
            }
            for it in &items {
                let _ = writeln!(
                    out,
                    "  {}\t{}\tp{}\t{}/{}\t{}\t{}",
                    it.id,
                    it.status,
                    it.priority,
                    it.attempt_count,
                    it.max_attempts,
                    executor_kind_str(&it.executor),
                    verification_policy_str(&it.verification_policy),
                );
            }
            let _ = writeln!(out, "\nAktuelles Item:");
            match &running {
                Some(r) => {
                    let _ = writeln!(
                        out,
                        "  {} '{}' — Agent {}, Versuch {}/{}, Lease {}",
                        r.item_id,
                        r.title,
                        r.agent_id,
                        r.attempt_number,
                        r.max_attempts,
                        format_lease_remaining(r.lease_remaining_secs),
                    );
                }
                None => {
                    let _ = writeln!(out, "  — nichts läuft gerade —");
                }
            }
            let wall_limit = project
                .budget
                .max_wall_time_secs
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let item_limit = project
                .budget
                .max_work_items
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(out, "\nBudget:");
            let _ = writeln!(
                out,
                "  Laufzeit : {}s / {wall_limit}s",
                usage.elapsed_wall_secs
            );
            let _ = writeln!(out, "  Items    : {} / {item_limit}", usage.item_count);
            let _ = writeln!(out, "  Versuche : {}", usage.attempts_total);
            let _ = writeln!(out, "  Artefakte: {}", usage.artifacts_total);
            let _ = writeln!(out, "\nWartet auf: {waiting}");
            if let Some(note) = &git_stray_note {
                let _ = writeln!(out, "\nHinweis: {note}");
            }
            let _ = writeln!(out, "\nZeitleiste (letzte {tail_n}):");
            if tail_entries.is_empty() {
                let _ = writeln!(out, "  (keine)");
            }
            for entry in &tail_entries {
                let at = entry.get("at").and_then(Value::as_u64).unwrap_or(0);
                let _ = writeln!(out, "  [{at}] {}", journal_entry_kind(entry));
            }
        }
    }
    ExitCode::Success
}

/// `agentkit work watch <projekt-id>` (§22 des Konzepts) — eine schlicht
/// neu gezeichnete Textansicht für ein ZWEITES Terminal neben einem
/// laufenden `agentkit work run`, deshalb sperrfrei (Befund 0 der Handprobe
/// hat genau deshalb Vorrang vor diesem Auftrag).
///
/// KEIN ratatui: das hängt am Feature `tui`, das die schlanke `cli`-
/// Release-Variante bewusst NICHT hat (siehe `.github/workflows/release.yml`)
/// — und gerade dort (Skripte, Server, CI) laufen die langen Vorhaben, die
/// `watch` beobachten soll. Eine simple, bei jedem Intervall komplett neu
/// gezeichnete ANSI-Ansicht (`\x1b[2J\x1b[H` löschen + Cursor Zeile 1) reicht
/// für dieses eine Bild und funktioniert in beiden Release-Varianten, ohne
/// eine neue Dependency einzuführen.
///
/// `--format json` gibt — wie bei jedem anderen `--format json` dieser CLI
/// (Moduldoku, stdout-Kontrakt) — GENAU EIN Dokument aus und kehrt zurück,
/// KEINE Endlosschleife. Ist stdout kein Terminal (z. B. eine Pipe), verhält
/// sich `watch` ebenfalls wie ein einmaliges `status`: ein Prozess, dessen
/// stdout gepiped oder in eine Datei umgeleitet ist, soll nicht endlos
/// ANSI-Escape-Sequenzen in diese Senke schreiben — das wäre reiner Müll für
/// jeden nachgeschalteten Konsumenten (`cat`, ein Logfile, …).
fn cmd_watch(
    args: &[String],
    deps: &WorkCliDeps<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let flags = match parse_flags_checked(
        "watch",
        args,
        &[
            "-w",
            "--workspace",
            "--dir",
            "--interval",
            "--tail",
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
            "[FEHLER] Nutzung: agentkit work watch <projekt-id> [--interval SEK] [--tail N] \
             [--format json]"
        );
        return ExitCode::GeneralError;
    };
    let workspace = workspace_of(&flags);
    let dir_override = dir_override_of(&flags);
    let format = parse_format(flags.value(&["--format"]));
    let interval_secs = match parse_opt::<u64>(&flags, "--interval") {
        Ok(v) => v.unwrap_or(2).max(1),
        Err(e) => {
            let _ = writeln!(err, "[FEHLER] {e}");
            return ExitCode::GeneralError;
        }
    };
    let tail_n = match parse_opt::<usize>(&flags, "--tail") {
        Ok(v) => v.unwrap_or(10),
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

    if format == OutputFormat::Json {
        return render_watch_once(&dir, &project_id, format, tail_n, out, err);
    }
    // Der reale Prozess-stdout entscheidet, nicht der injizierte `out`-
    // Parameter (der in Tests ein `Vec<u8>` ist): ANSI-Redraw ist nur
    // sinnvoll, wenn tatsächlich ein Terminal dahinter sitzt. In Tests ist
    // das echte stdout nie ein Terminal — dieser Zweig ist damit deterministisch
    // testbar (siehe Tests unten), ohne ein TTY vortäuschen zu müssen.
    if !std::io::stdout().is_terminal() {
        return render_watch_once(&dir, &project_id, format, tail_n, out, err);
    }

    let _ = write!(out, "\x1b[?25l"); // Cursor verstecken
    let code = loop {
        if deps.cancel.load(Ordering::SeqCst) {
            break ExitCode::Success;
        }
        let _ = write!(out, "\x1b[2J\x1b[H"); // Bildschirm löschen, Cursor Zeile 1
        let code = render_watch_once(&dir, &project_id, format, tail_n, out, err);
        let _ = out.flush();
        if code != ExitCode::Success {
            break code;
        }
        let deadline = Instant::now() + Duration::from_secs(interval_secs);
        while Instant::now() < deadline {
            if deps.cancel.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let _ = write!(out, "\x1b[?25h"); // Cursor wiederherstellen
    let _ = out.flush();
    code
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

// -------------------------------------------------------- approve/reject

/// Prüft, dass `item` wirklich auf eine manuelle Freigabe wartet, und liefert
/// den dafür zuständigen Versuch (`attempt_id`). Gemeinsamer Kern von
/// `cmd_approve`/`cmd_reject`: beide dürfen nur auf Items in
/// `AwaitingVerification` mit Policy `HumanApproval` wirken (siehe Hilfetext)
/// — ein automatisiertes Prüfkommando läuft synchron im Runner und lässt
/// diesen Zustand nie so stehen, dass ein Mensch hier eingreifen könnte.
fn require_awaiting_human_approval<'a>(
    snapshot: &'a WorkState,
    item_id: &str,
    err: &mut dyn Write,
) -> Option<&'a str> {
    let Some(item) = snapshot.items.get(item_id) else {
        let _ = writeln!(err, "[FEHLER] Work Item '{item_id}' existiert nicht.");
        return None;
    };
    if item.status != WorkItemStatus::AwaitingVerification
        || !matches!(item.verification_policy, VerificationPolicy::HumanApproval)
    {
        let _ = writeln!(
            err,
            "[FEHLER] Work Item '{item_id}' wartet nicht auf eine manuelle Freigabe (Status: \
             '{}', Policy: '{}').",
            item.status,
            verification_policy_str(&item.verification_policy)
        );
        return None;
    }
    let Some(lease) = snapshot.leases.get(item_id) else {
        // Strukturell unerreichbar: `WorkItemSubmittedForVerification` lässt
        // das Lease bewusst stehen (siehe event.rs), solange das Item
        // `AwaitingVerification` ist. Trotzdem kein `panic!` auf einer reinen
        // Nutzereingabe — eine klare Fehlermeldung ist der sicherere Fehlerfall.
        let _ = writeln!(
            err,
            "[FEHLER] Work Item '{item_id}': kein offener Versuch gefunden (interner \
             Zustandsfehler)."
        );
        return None;
    };
    Some(lease.attempt_id.as_str())
}

/// Gemeinsamer erster Schritt von `cmd_approve`/`cmd_reject`: parst die (für
/// beide identischen) Flags und liest Item- und Projekt-ID. Getrennt vom
/// zweiten Schritt ([`open_and_require_awaiting`]), weil `reject` dazwischen
/// noch `--reason` prüfen muss, BEVOR das Projekt überhaupt geöffnet wird
/// (ein fehlender Pflicht-Parameter soll nicht erst nach einem Datei-I/O
/// auffallen) — `approve` braucht diesen Zwischenschritt nicht.
fn parse_approval_flags(
    cmd: &str,
    args: &[String],
    usage: &str,
    err: &mut dyn Write,
) -> Result<(Flags, String, String), ExitCode> {
    let flags = parse_flags_checked(
        cmd,
        args,
        &["-p", "--project", "-w", "--workspace", "--dir", "--reason"],
        &[],
        err,
    )?;
    let Some(item_id) = flags.positionals.first().cloned() else {
        let _ = writeln!(err, "[FEHLER] Nutzung: {usage}");
        return Err(ExitCode::GeneralError);
    };
    let Some(project_id) = flags.value(&["-p", "--project"]) else {
        let _ = writeln!(err, "[FEHLER] -p <projekt-id> ist erforderlich.");
        return Err(ExitCode::GeneralError);
    };
    Ok((flags, item_id, project_id))
}

/// Zweiter gemeinsamer Schritt: öffnet das Projekt und validiert über
/// [`require_awaiting_human_approval`], dass das Item wirklich wartet. Gibt
/// den geöffneten Store und den zuständigen Versuch zurück.
fn open_and_require_awaiting(
    flags: &Flags,
    item_id: &str,
    project_id: &str,
    err: &mut dyn Write,
) -> Result<(WorkStore, String), ExitCode> {
    let workspace = workspace_of(flags);
    let dir_override = dir_override_of(flags);
    let root = work_root(&workspace, dir_override.as_deref());
    let Some(store) = open_project(&root, project_id, err) else {
        return Err(ExitCode::GeneralError);
    };
    let snapshot = store.snapshot();
    let Some(attempt_id) = require_awaiting_human_approval(&snapshot, item_id, err) else {
        return Err(ExitCode::GeneralError);
    };
    let attempt_id = attempt_id.to_string();
    Ok((store, attempt_id))
}

fn cmd_approve(
    args: &[String],
    deps: &WorkCliDeps<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let (flags, item_id, project_id) = match parse_approval_flags(
        "approve",
        args,
        "agentkit work approve <work-item-id> -p <projekt-id> [--reason TEXT]",
        err,
    ) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let (store, attempt_id) = match open_and_require_awaiting(&flags, &item_id, &project_id, err) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let reason = flags.value(&["--reason"]);
    let at_ms = now_ms();
    // Für die Promotion unten gebraucht — VOR dem Statuswechsel gelesen, aber
    // die Policy selbst ändert sich durch `VerificationApproved`/
    // `WorkItemCompleted` nicht.
    let policy = store
        .snapshot()
        .items
        .get(&item_id)
        .map(|it| it.verification_policy.clone())
        .unwrap_or_default();

    if let Err(e) = store.submit(WorkEvent::VerificationApproved {
        item: item_id.clone(),
        attempt: attempt_id.clone(),
        by: HUMAN_BY.to_string(),
        reason,
        at_ms,
    }) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }
    if let Err(e) = store.submit(WorkEvent::WorkItemCompleted {
        item: item_id.clone(),
        attempt: attempt_id,
        at_ms,
    }) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }
    // Verifizierte Claims promoten (§11, Phase 5b) — ein Fehlschlag bricht
    // NICHT ab (siehe `graph::promote_after_completion`), nur eine Warnung.
    if let Some(msg) = crate::graph::promote_after_completion(
        &store,
        deps.graph.as_ref(),
        &item_id,
        &policy,
        at_ms,
    ) {
        let _ = writeln!(err, "[work] Warnung: {msg}");
    }
    let _ = writeln!(out, "Work Item '{item_id}' freigegeben und abgeschlossen.");
    ExitCode::Success
}

fn cmd_reject(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let (flags, item_id, project_id) = match parse_approval_flags(
        "reject",
        args,
        "agentkit work reject <work-item-id> -p <projekt-id> --reason TEXT",
        err,
    ) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let Some(reason) = flags.value(&["--reason"]).filter(|r| !r.trim().is_empty()) else {
        let _ = writeln!(
            err,
            "[FEHLER] --reason ist erforderlich (der Grund landet im nächsten Arbeitspaket)."
        );
        return ExitCode::GeneralError;
    };
    let (store, attempt_id) = match open_and_require_awaiting(&flags, &item_id, &project_id, err) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let at_ms = now_ms();

    if let Err(e) = store.submit(WorkEvent::VerificationRejected {
        item: item_id.clone(),
        attempt: attempt_id.clone(),
        by: HUMAN_BY.to_string(),
        reason: reason.clone(),
        at_ms,
    }) {
        report_work_error(err, &e);
        return ExitCode::GeneralError;
    }
    // Derselbe Mechanismus wie ein regulärer fachlicher Fehlschlag (siehe
    // `runner::record_verification_rejected`) — reuse statt einer zweiten,
    // unabhängig gepflegten Kopie der "ist max_attempts erschöpft?"-Entscheidung.
    let released = match recovery::finish_failed_attempt(
        &store,
        &item_id,
        &attempt_id,
        |item| {
            format!(
                "Wiederholung {}/{} (manuell abgelehnt: {reason})",
                item.attempt_count + 1,
                item.max_attempts
            )
        },
        at_ms,
    ) {
        Ok(r) => r,
        Err(e) => {
            report_work_error(err, &e);
            return ExitCode::GeneralError;
        }
    };
    if released {
        let _ = writeln!(
            out,
            "Work Item '{item_id}' abgelehnt und zurückgesetzt auf 'pending'."
        );
    } else {
        let _ = writeln!(
            out,
            "Work Item '{item_id}' abgelehnt — Versuche ausgeschöpft, bleibt 'failed'."
        );
    }
    ExitCode::Success
}
