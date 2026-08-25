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

use crate::agent::{stopped, Cancel, RunHandle};
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Read-only-Teilmenge der Coding-Tools (kein write/edit/run_shell). Praktisch für
/// Sub-Agenten-Rollen, die nur erkunden oder begutachten dürfen (siehe `roles.rs`).
/// Die git-Tools gehören dazu: ein `reviewer` braucht Diffs und Historie, ohne dafür
/// Shell-Freigaben (`run_shell`) zu benötigen.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "list_files",
    "glob_files",
    "grep",
    "read_file",
    "read_pdf",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
];

/// Größte Datei, die `grep` noch durchsucht. Alles darüber ist in einem
/// Code-Workspace praktisch nie Quelltext (Dumps, Modelle, Build-Artefakte).
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// So viel wird für die Binär-Erkennung vorab gelesen.
const BINARY_SNIFF_BYTES: usize = 8000;

/// Liest eine Datei für `grep` — oder `None`, wenn sie zu groß oder binär ist.
///
/// Erst der Kopf, dann der Rest: die Binär-Erkennung braucht nur die ersten paar
/// Kilobyte, und eine 1,9-MB-PNG (Assets liegen nicht in `IGNORE`) soll nicht
/// komplett in den Speicher wandern, nur um danach verworfen zu werden. Kriterium
/// wie bei `grep -I`: ein NUL-Byte im Kopf. Ohne das lieferte `grep` Treffer aus
/// `.so`/`.png`-Dateien, die als Zeichensalat in der Historie landen.
fn read_searchable(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > GREP_MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = vec![0u8; BINARY_SNIFF_BYTES];
    let head = file.read(&mut bytes).ok()?;
    bytes.truncate(head);
    if bytes.contains(&0) {
        return None;
    }
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

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

/// Token-Budget eines Coding-Agenten — großzügig, weil moderne Modelle großen
/// Kontext haben und die naive [`crate::ShortTermMemory::compact`] verlustbehaftet
/// ist. Gilt für JEDEN Coding-Agenten: Haupt-Agent, Sub-Agent (`task`) und
/// Schwarm-Mitglied. Der Builder-Default (8000) reicht hier nicht — ein einziges
/// Tool-Ergebnis darf bis [`crate::memory::TRUNCATE_LIMIT`] Zeichen (~4000 Token)
/// groß sein, zwei davon hätten schon kompaktiert.
pub const CODING_TOKEN_BUDGET: usize = 100_000;

/// Token-Budget für HELFER — Sub-Agenten (`task`) und Schwarm-Mitglieder.
///
/// Deutlich enger als [`CODING_TOKEN_BUDGET`], weil ein Helfer nicht den roten
/// Faden über den ganzen Auftrag halten muss: er erledigt eine abgegrenzte
/// Aufgabe und gibt eine Zusammenfassung zurück. Sein Kontext ist reines
/// Arbeitsmaterial.
///
/// Der Wert entscheidet, wie groß die Requests eines Helfers werden dürfen, und
/// damit direkt über den Verbrauch der Deployment-Quota. Gemessen an einem
/// gescheiterten Lauf: drei Sub-Agenten lasen zusammen 61 Dateien; bei
/// [`crate::memory::TRUNCATE_LIMIT`] von 16000 Zeichen je Tool-Ergebnis wuchs
/// jeder von ihnen gegen 100_000 Tokens, bevor die Kompaktierung überhaupt
/// ansprang — jede einzelne Anfrage riss dann ein entsprechendes Loch ins
/// Minutenbudget, und alle drei endeten im Rate-Limit.
///
/// Der Preis: die Kompaktierung springt früher an und kostet je Auslösung einen
/// zusätzlichen (kleinen) Modell-Aufruf. Das ist der bessere Handel, solange
/// eine Anfrage sonst das Dreifache wiegt.
pub const HELPER_TOKEN_BUDGET: usize = 30_000;

/// Haltung zuerst, Anweisungen danach.
///
/// Ein Modell, das sich als „Coding-Agent" versteht, führt Werkzeuge aus. Eines,
/// das sich als erfahrener Entwickler versteht, urteilt — und darauf kommt es an
/// den Stellen an, wo die Läufe scheitern: Im SWE-bench-Lauf prompt-25 änderten
/// 11 von 25 Aufgaben die RICHTIGE Datei und lösten das Problem trotzdem nicht.
/// Nicht mangels Werkzeug oder Fundstelle, sondern weil der Fix auf einer
/// Vermutung stand statt auf einer verstandenen Ursache.
const CODING_INTRO: &str = "Du bist ein erfahrener Software-Entwickler und arbeitest im \
aktuellen Projektverzeichnis (deine Sandbox; Pfade außerhalb sind gesperrt). Du \
arbeitest wie jemand, der seit Jahren fremden Code repariert:\n\
- SACHLICH: Du urteilst nach Belegen, nicht nach Gefühl. Eine Vermutung ist ein \
Zwischenschritt, kein Ergebnis — du prüfst sie, bevor du auf ihr aufbaust. Sagen zu \
können WARUM etwas kaputt war, ist der Unterschied zwischen einer Reparatur und einem \
Treffer ins Blaue.\n\
- GEWISSENHAFT: Du kennst die Folgen deiner Änderung, bevor du sie abgibst — auch für \
den Code, der die geänderte Stelle benutzt.\n\
- EFFIZIENT: Du liest gezielt, nicht viel. Was du einmal weißt, liest du nicht \
zweimal.\n\
- HARTNÄCKIG: Ein Problem, das sich wehrt, ist normal. Du hörst nicht auf, wenn dir der \
erste Ansatz ausgeht, sondern wechselst ihn.\n\
";

/// Orientierungsregel OHNE Sub-Agenten: der Agent liest selbst.
const ORIENT_SELBST: &str = "Verschaffe dir zuerst mit list_files/glob_files/grep/read_file \
einen Überblick über den vorhandenen Code, bevor du ihn änderst (glob_files findet Dateien, \
grep durchsucht Inhalte — beide read-only).";

/// Orientierungsregel MIT `task`: die Erkundung geht an einen explorer.
///
/// Warum die Regel ERSETZT und nicht bloß ergänzt wird: der Hinweis auf
/// Sub-Agenten steht weiter unten im Prompt, [`ORIENT_SELBST`] dagegen ganz vorn
/// und nennt die Werkzeuge namentlich. In einem Live-Lauf hat ein Modell exakt
/// diese Sequenz abgearbeitet (list_files, glob_files, grep, dann vier
/// `read_file` auf einmal) und den späteren Delegations-Hinweis ignoriert — ein
/// „das gilt vorrangig" weiter hinten schlägt eine frühere, konkretere Anweisung
/// nicht. Also darf die frühere Anweisung gar nicht erst dastehen.
const ORIENT_DELEGIEREND: &str = "Verschaffe dir zuerst einen Überblick über den vorhandenen \
Code, bevor du ihn änderst — aber lies dafür NICHT selbst viele Dateien: delegiere die \
Erkundung mit dem Tool 'task' an einen 'explorer'-Sub-Agenten (Details unten) und lass dir \
die relevanten Stellen mit Pfad und Zeile nennen. Was er dabei liest, bleibt in SEINEM \
Kontext, nicht in deinem. Selbst liest du nur die Stellen, die du für eine konkrete \
Änderung wirklich brauchst; list_files/glob_files/grep (read-only) nutzt du für gezielte \
Einzelfragen.";

/// Der Kern des Vorgehens: erst den Fehler nachweisen, dann beheben.
///
/// Ein grüner Test beweist nichts, wenn er auch ohne die Änderung grün wäre.
/// Gemessen im SWE-bench-Lauf ctxfix-25: In 22 von 25 Aufgaben gab es keinen
/// einzigen Check, der erst rot und dann grün war — und in 17 von 17 nicht
/// gelösten Aufgaben endete der Agent trotzdem mit „I fixed … I verified it".
/// Er hatte die bestehenden Tests laufen lassen, die vor der Änderung ebenso
/// grün waren.
///
/// Steht bewusst VOR der Aufforderung, Code zu schreiben: Der Nachweis ist der
/// erste Schritt, nicht die Abnahme am Ende.
/// Die Arbeitsweise in vier Schritten — die Reihenfolge ist die Aussage.
///
/// Schritt 2 (Nachweis vor Änderung) ist der einzige Prompt-Eingriff dieser
/// Reihe mit belegter Wirkung: Der Anteil der Aufgaben, in denen der Agent den
/// Fehler VOR seiner Änderung reproduziert, stieg von 36 % auf 76 %
/// (SWE-bench-Läufe ctxfix-25 → prompt-25).
///
/// Der Satz zu fremden Tests ist bewusst kurz und positiv. Die frühere Fassung
/// erklärte das Verbot über vier Sätze — danach änderten 15 statt 9 von 25
/// Aufgaben Testdateien. Ein Verbot, das sein Thema breit ausmalt, lenkt die
/// Aufmerksamkeit dorthin.
const VERIFIZIEREN: &str = "\nSo arbeitest du:\n\
1. VERSTEHEN: Finde die Stelle und erkläre dir, warum das beschriebene Verhalten \
entsteht. Nicht die erste plausible Stelle nehmen — die, für die du eine Begründung \
hast.\n\
2. NACHWEISEN: Baue einen kurzen Check, der genau dieses Verhalten zeigt, und führe ihn \
aus — er MUSS jetzt fehlschlagen. Tut er das nicht, hast du die Ursache noch nicht; such \
weiter, statt zu raten. Dein Check gehört in eine eigene Datei (die Tests des Projekts \
führst du aus, änderst sie aber nicht).\n\
3. ÄNDERN: Behebe die Ursache, bis derselbe Check durchläuft. Deckt der Auftrag mehr \
Fälle ab als deinen einen Check, prüfe auch die.\n\
4. ABSICHERN: Lass die bestehenden Tests des berührten Moduls laufen und sieh nach, wer \
die geänderte Stelle sonst noch benutzt (grep auf Funktions-/Klassennamen).";

/// Durchhalten und Abschließen.
///
/// Der Agent hört zu früh auf: Median 15 Schritte von 100 erlaubten
/// (SWE-bench-Läufe ctxfix-25 und prompt-25, unverändert). Er scheitert nicht an
/// der Obergrenze, sondern hält sich für fertig — deshalb steht hier, woran er
/// „fertig" erkennt, und was ein Rückschlag stattdessen bedeutet.
///
/// „Wechsle den Ansatz" statt „versuch es erneut": Wiederholung war das
/// beobachtete Muster (dieselben Suchen, dieselben Testläufe, dieselben
/// Dateien) — nicht fehlende Ausdauer.
const CODING_OUTRO: &str = "\nPlane deine Arbeit mit update_plan, schreibe Code mit \
write_file/edit_file und führe ihn mit run_shell aus.\n\
Ein roter Check nach deiner Änderung ist ein Zwischenstand, kein Abschluss. Du hast \
reichlich Schritte — nutze sie. Führt derselbe Weg zweimal nicht weiter, wiederhole ihn \
nicht: wechsle die Ebene (wer ruft die Stelle auf?), das Werkzeug oder die Hypothese. \
Erst wenn du eine Sackgasse begründen kannst, ist sie eine.\n\
Fertig bist du, wenn dein Check den Unterschied zeigt — nicht, wenn dir die Ideen \
ausgehen. Sag am Ende kurz, was du geändert hast, WARUM das die Ursache war und WELCHER \
Check vorher fehlschlug und jetzt durchläuft.";

/// System-Prompt des Coding-Agenten.
///
/// `delegierend` = das `task`-Tool ist vorhanden. Dann tritt
/// [`ORIENT_DELEGIEREND`] an die Stelle von [`ORIENT_SELBST`]; sonst bliebe im
/// Prompt eine Anweisung stehen, die der Delegation direkt widerspricht. Früher
/// war das die Konstante `CODING_SYSTEM` — die konnte den Unterschied nicht
/// ausdrücken.
pub fn coding_system(delegierend: bool) -> String {
    let orient = if delegierend {
        ORIENT_DELEGIEREND
    } else {
        ORIENT_SELBST
    };
    format!("{CODING_INTRO}{orient}{VERIFIZIEREN}{CODING_OUTRO}")
}

/// Größte Datei, die für `/undo` noch gesichert wird. Darüber merkt sich der
/// Checkpoint nur, DASS geändert wurde — lieber ehrlich „kann ich nicht
/// zurücknehmen" als eine Sitzung, die stillschweigend Speicher frisst.
const CHECKPOINT_MAX_BYTES: usize = 1024 * 1024;

/// Obergrenzen für den Undo-Stapel. Ohne Deckel hielte ein langer Lauf jede
/// Vorversion bis zum Prozessende im Speicher — und genau der Benchmark-Lauf
/// (`agentkit -p … -y`, hunderte Schreibvorgänge im Container) zahlt dafür,
/// ohne `/undo` überhaupt erreichen zu können.
///
/// Zwei Grenzen, weil eine allein nicht reicht: die Byte-Grenze bremst wenige
/// große Dateien, die Anzahl-Grenze viele kleine (ein `Fehlte`-Eintrag wiegt
/// null Bytes und käme sonst unbegrenzt oft vor).
const CHECKPOINT_MAX_ENTRIES: usize = 50;
const CHECKPOINT_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Wirft die ÄLTESTEN Einträge weg, bis der Stapel wieder in beide Grenzen
/// passt. Der jüngste bleibt immer stehen — die letzte Änderung soll auch dann
/// rücknehmbar sein, wenn sie allein die Byte-Grenze reißt.
fn trim_checkpoints(cps: &mut Vec<Checkpoint>) {
    fn bytes(c: &Checkpoint) -> usize {
        match &c.vorher {
            Vorzustand::Inhalt(s) => s.len(),
            _ => 0,
        }
    }
    let mut total: usize = cps.iter().map(bytes).sum();
    while cps.len() > 1
        && (cps.len() > CHECKPOINT_MAX_ENTRIES || total > CHECKPOINT_MAX_TOTAL_BYTES)
    {
        total -= bytes(&cps.remove(0));
    }
}

/// Zustand einer Datei VOR einer Änderung.
#[derive(Clone)]
pub enum Vorzustand {
    /// Die Datei gab es noch nicht — Zurücknehmen heißt löschen.
    Fehlte,
    /// Der vorherige Inhalt.
    Inhalt(String),
    /// Zu groß (siehe [`CHECKPOINT_MAX_BYTES`]) oder nicht lesbar.
    NichtGesichert,
}

/// Eine rücknehmbare Datei-Änderung.
#[derive(Clone)]
pub struct Checkpoint {
    /// Pfad wie vom Modell angegeben (für die Anzeige).
    pub pfad: String,
    pub vorher: Vorzustand,
}

/// Approve-Callback für `run_shell`: bekommt den Befehl, gibt `true` zum Ausführen.
pub type ApproveFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Leitplanken für `run_shell` — aus dem Frontmatter der Projekt-Instruktionen
/// (`AGENTS.md`, siehe [`crate::load_project_instructions`]).
///
/// Der Unterschied zur Freigabe-Policy des Frontends: die steckt im
/// [`ApproveFn`] und kennt `-y` („alles erlauben"). Diese Regeln werden VOR dem
/// Callback geprüft — `deny` ist deshalb auch mit `-y` nicht aufhebbar, und
/// `allow` kommt gar nicht erst zur Frage. Das ist der ganze Grund, warum sie
/// hier und nicht im Frontend leben.
///
/// **Was das nicht ist:** eine Sicherheitsgrenze. Der Abgleich ist eine
/// Textprüfung; sie löst weder Quoting noch `$(…)`, `eval`, Variablen oder
/// Aliase auf. `git push` fängt sie, `g=push; git $g` nicht. Wie die Sandbox in
/// [`CodingTools::safe`] bremst sie ein irrendes Modell — sie hält keinen
/// Angreifer auf, der die Befehle schreiben darf.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guardrails {
    /// Muster, die HART abgelehnt werden — ohne Rückfrage, auch mit `-y`.
    pub deny: Vec<String>,
    /// Muster, die ohne Rückfrage laufen dürfen.
    pub allow: Vec<String>,
}

impl Guardrails {
    /// Gibt es überhaupt Regeln? (Sonst wird nichts in den Prompt geschrieben.)
    pub fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.allow.is_empty()
    }

    /// Das `deny`-Muster, das diesen Befehl sperrt — für die Begründung im
    /// Tool-Ergebnis.
    pub fn verbietet(&self, command: &str) -> Option<&str> {
        Self::treffer(&self.deny, command)
    }

    /// Darf dieser Befehl ohne Rückfrage laufen? `deny` schlägt `allow` — sonst
    /// hebe ein zu weites `allow: git` ein gezieltes `deny: git push` auf.
    pub fn erlaubt(&self, command: &str) -> bool {
        self.verbietet(command).is_none() && Self::treffer(&self.allow, command).is_some()
    }

    /// Erstes Muster, das auf irgendein Segment des Befehls passt.
    fn treffer<'a>(muster: &'a [String], command: &str) -> Option<&'a str> {
        let segmente: Vec<String> = segmente(command);
        muster
            .iter()
            .find(|m| segmente.iter().any(|s| beginnt_mit(s, m)))
            .map(String::as_str)
    }
}

/// Zerlegt einen Befehl an den Verkettungs-Operatoren in seine Teilbefehle.
/// Nötig, weil ein `deny: git push` sonst an `cargo test && git push`
/// vorbeiliefe — und genau so umginge ein Modell die Regel versehentlich.
/// Whitespace wird dabei normalisiert, damit `git   push` wie `git push` passt.
fn segmente(command: &str) -> Vec<String> {
    command
        .split([';', '\n', '&', '|'])
        .map(|teil| teil.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|teil| !teil.is_empty())
        .collect()
}

/// Beginnt `segment` mit `muster` — an einer Wortgrenze? `git push` passt auf
/// `git push --force`, aber `git` nicht auf `github-cli`.
fn beginnt_mit(segment: &str, muster: &str) -> bool {
    let muster: String = muster.split_whitespace().collect::<Vec<_>>().join(" ");
    if muster.is_empty() {
        return false;
    }
    segment
        .strip_prefix(&muster)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

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
    /// Lauf-Kontext des besitzenden Agenten: `run_shell`/`git` lesen daraus den
    /// aktiven Stop-Knopf und beenden den Kindprozess sofort, statt bis zum
    /// Timeout weiterzulaufen. Default = leer (kein Abbruch von außen). Liegt
    /// direkt am Struct (nicht in `Inner`), weil `RunHandle` selbst Arc-geteilt
    /// ist — `Clone` teilt es automatisch mit allen Registry-Closures.
    run: RunHandle,
    /// Rücknehmbare Datei-Änderungen, jüngste zuletzt. Arc-geteilt, damit alle
    /// Klone (Sub-Agenten, Schwarm-Mitglieder) in denselben Stapel schreiben —
    /// `/undo` nimmt sonst nur die Änderungen des Haupt-Agenten zurück.
    checkpoints: Arc<Mutex<Vec<Checkpoint>>>,
    /// Was dieser Agent (samt seiner Klone) schon gelesen hat: Pfad → (Länge,
    /// Prüfsumme). Grundlage des Hinweises in [`CodingTools::read_file_range`].
    /// Arc-geteilt wie `checkpoints`, damit ein Sub-Agent nicht als „hat noch
    /// nie gelesen" durchgeht, wenn der Orchestrator dieselbe Datei schon hatte.
    gelesen: Arc<Mutex<std::collections::HashMap<String, (usize, u64)>>>,
    /// Leitplanken aus den Projekt-Instruktionen. Liegt wie `run` neben dem
    /// `Arc<Inner>` — dadurch bleiben die Signaturen von `new`/`with_approve`
    /// unverändert, und `Clone` gibt die Regeln automatisch an jeden Sub-Agenten
    /// und jedes Schwarm-Mitglied weiter, das sich diese Instanz teilt.
    guardrails: Guardrails,
    /// Pfadmuster, die `write_file`/`edit_file` NICHT anfassen dürfen
    /// (`--protect-paths`). Liegt wie `guardrails` neben dem `Arc<Inner>`, damit
    /// jeder Klon — Sub-Agent, Schwarm-Mitglied — dieselbe Sperre erbt: Eine
    /// Regel, die nur für den Orchestrator gilt, ist keine.
    ///
    /// Der Anlass ist gemessen: Im SWE-bench-Lauf 2026-08-08 änderte der Agent
    /// in 4 von 40 Instanzen eine Testdatei, obwohl der Auftrag es verbot. Die
    /// offizielle Auswertung spielt ihren eigenen `test_patch` vorher ein, beide
    /// kollidieren, und der GANZE Patch fällt durch — auch der korrekte
    /// Quellcode darin. Drei dieser Fixes waren nachweislich richtig. Eine Regel
    /// im Prompt wurde in fast jeder zweiten Instanz übergangen; eine Regel im
    /// Werkzeug kann man nicht übergehen.
    protected: Arc<Vec<String>>,
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
            run: RunHandle::default(),
            checkpoints: Arc::new(Mutex::new(Vec::new())),
            gelesen: Arc::new(Mutex::new(std::collections::HashMap::new())),
            guardrails: Guardrails::default(),
            protected: Arc::new(Vec::new()),
        }
    }

    /// Verdrahtet den Lauf-Kontext des besitzenden Agenten (siehe Feld `run`).
    /// Muss dasselbe Handle sein, das auch `AgentBuilder::run_handle` bekommt,
    /// und VOR `register()` gesetzt werden (die Closures klonen das Struct).
    pub fn with_run_handle(mut self, run: RunHandle) -> Self {
        self.run = run;
        self
    }

    /// Setzt die Leitplanken (siehe [`Guardrails`]). Wie [`Self::with_run_handle`]
    /// VOR `register()` aufrufen — die Tool-Closures klonen das Struct.
    pub fn with_guardrails(mut self, guardrails: Guardrails) -> Self {
        self.guardrails = guardrails;
        self
    }

    /// Setzt die geschützten Pfadmuster (siehe Feld `protected`). Wie
    /// [`Self::with_guardrails`] VOR `register()` aufrufen.
    pub fn with_protected_paths(mut self, muster: Vec<String>) -> Self {
        self.protected = Arc::new(muster);
        self
    }

    /// Schreibt dieses Shell-Kommando auf einen gesperrten Pfad? Dann der
    /// getroffene Pfad.
    ///
    /// Nötig, weil die Sperre in `write_file`/`edit_file` allein nichts hält:
    /// Gemessen im Benchmark-Lauf 2026-08-09 (`v2-work`, `astropy-7746`) wies
    /// `write_file` die Testdatei ab — woraufhin derselbe Agent sie mit
    /// `python - <<'PY' … p.write_text(…)` schrieb. Eine Sperre, um die man
    /// herumläuft, ist Dekoration; schlimmer noch, die Absage versprach
    /// ausdrücklich, dass auch `run_shell` gesperrt sei.
    ///
    /// Die Erkennung ist eine HEURISTIK und soll es bleiben: Sie fängt die
    /// naheliegenden Schreibwege, nicht jeden denkbaren. Entscheidend ist, dass
    /// sie keine LESENDEN Verwendungen abweist — `pytest tests/test_x.py` muss
    /// laufen dürfen, sonst nimmt die Sperre dem Agenten die Verifikation weg.
    /// Deshalb wird nicht auf „Pfad kommt vor" geprüft, sondern darauf, dass er
    /// das ZIEL eines Schreibvorgangs ist.
    fn shell_schreibt_geschuetztes(&self, command: &str) -> Option<String> {
        if self.protected.is_empty() {
            return None;
        }
        let cmd = command.replace('\\', "/");
        // Pfad-artige Tokens einsammeln (auch aus Anführungszeichen heraus).
        let kandidaten: Vec<String> = cmd
            .split(|c: char| c.is_whitespace() || "'\"()<>|;,".contains(c))
            .filter(|t| t.contains('/') || t.contains('.'))
            .map(|t| t.trim_matches(|c| c == '.' || c == '/').to_string())
            .filter(|t| !t.is_empty())
            .collect();
        for kandidat in kandidaten {
            if self.ist_geschuetzt(&kandidat).is_none() {
                continue;
            }
            // Wie in `write_file`: nur bestehende Dateien sind gesperrt. Ein
            // frisch angelegter eigener Test ist kein Angriff auf die Messung.
            if !self.safe(&kandidat).map(|p| p.exists()).unwrap_or(false) {
                continue;
            }
            let quoted = regex_lite_escape(&kandidat);
            // 1) Umleitung/Kopie/In-Place-Edit MIT diesem Pfad als Ziel.
            let ziel_marker = [
                format!("> {quoted}"),
                format!(">{quoted}"),
                format!(">> {quoted}"),
                format!(">>{quoted}"),
                format!("tee {quoted}"),
                format!("tee -a {quoted}"),
            ];
            if ziel_marker.iter().any(|m| cmd.contains(m.as_str())) {
                return Some(kandidat);
            }
            if (cmd.contains("sed -i") || cmd.contains("cp ") || cmd.contains("mv "))
                && cmd.contains(&kandidat)
                && cmd.rfind(&kandidat) > cmd.find("sed -i").or(cmd.find("cp ")).or(cmd.find("mv "))
            {
                return Some(kandidat);
            }
            // 2) Schreiben aus einem eingebetteten Skript heraus. Nur mit einer
            //    ausdrücklichen Schreib-API — `pytest … > log.txt` fällt so
            //    nicht herein, `p.write_text(…)` schon.
            const SCHREIB_API: &[&str] = &[
                ".write_text(",
                ".writelines(",
                ".write(",
                "'w')",
                "\"w\")",
                "'w+'",
                "shutil.copy",
                "os.replace(",
                "os.rename(",
            ];
            if SCHREIB_API.iter().any(|m| cmd.contains(m)) {
                return Some(kandidat);
            }
        }
        None
    }

    /// Ist dieser Pfad gesperrt? Vergleich auf dem workspace-relativen Pfad mit
    /// `/` als Trenner, damit dieselben Muster unter Windows und Linux passen.
    ///
    /// Gematcht wird mit [`glob_match`] — derselbe Matcher, den `glob_files`
    /// benutzt. Ein Muster ohne `/` gilt zusätzlich für den bloßen Dateinamen,
    /// sonst müsste man `**/test_*.py` schreiben, wo `test_*.py` gemeint ist.
    fn ist_geschuetzt(&self, path: &str) -> Option<String> {
        if self.protected.is_empty() {
            return None;
        }
        let rel = path.replace('\\', "/");
        let rel = rel.trim_start_matches("./");
        let name = rel.rsplit('/').next().unwrap_or(rel);
        self.protected
            .iter()
            .find(|m| glob_match(m, rel) || (!m.contains('/') && glob_match(m, name)))
            .cloned()
    }

    /// Sperrt einen Pfad in die Sandbox ein — in zwei Stufen, weil eine allein
    /// nicht reicht:
    ///
    /// 1. **Lexikalisch** ([`normalize`]): fängt `../` ohne Dateisystemzugriff und
    ///    funktioniert auch für Pfade, die es noch nicht gibt (`write_file`).
    /// 2. **Real** ([`Path::canonicalize`] des tiefsten EXISTIERENDEN Vorfahren):
    ///    die Normalisierung folgt keinem Symlink. Ein Link im Workspace, der nach
    ///    außen zeigt, kam sonst durch Stufe 1 und `read_file`/`write_file` griffen
    ///    beim Öffnen auf das Linkziel zu.
    ///
    /// Kein Schutz gegen TOCTOU (Link zwischen Prüfung und Öffnen ausgetauscht) —
    /// die Sandbox bremst ein irrendes Modell, sie ist keine Sicherheitsgrenze
    /// gegen einen Angreifer mit Schreibrechten im Workspace.
    fn safe(&self, path: &str) -> Result<PathBuf, String> {
        let ws = &self.inner.workspace;
        let resolved = normalize(&ws.join(path));
        // `starts_with` ist bei Gleichheit wahr — der Workspace selbst bleibt erlaubt.
        // Ohne auflösbaren Vorfahren (`None`) zählt die lexikalische Prüfung.
        let drin = resolved.starts_with(ws)
            && real_ancestor(&resolved).map_or(true, |real| real.starts_with(ws));
        if drin {
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
    pub fn grep(
        &self,
        pattern: &str,
        path: &str,
        glob: &str,
        limit: usize,
    ) -> Result<String, String> {
        let rx = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(format!("ERROR: ungültiges Regex: {e}")),
        };
        let root = self.safe(path)?;
        let ws = &self.inner.workspace;
        let mut files = walk_files(&root);
        files.sort();
        // Zeigt `path` auf eine Datei, ist sie selbst der einzige Kandidat — dann
        // muss der Glob gegen ihren NAMEN geprüft werden, nicht gegen den Pfad
        // relativ zu sich selbst (der wäre leer, und das Standard-Glob `**/*`
        // passt auf nichts Leeres; die Datei fiele wieder heraus).
        let glob_basis = if root.is_file() {
            root.parent().unwrap_or(&root).to_path_buf()
        } else {
            root.clone()
        };
        let mut hits: Vec<String> = Vec::new();
        for file in &files {
            if !glob_match(glob, &rel_str(file, &glob_basis)) {
                continue;
            }
            let Some(bytes) = read_searchable(file) else {
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

    /// [`CodingTools::read_file`] mit optionalem Zeilenfenster und einem
    /// Gedächtnis für bereits Gelesenes.
    ///
    /// **Zeilenfenster** (`von`/`bis`, 1-basiert, `bis` inklusive), weil eine
    /// ganze Datei im Median 2.006 Tokens kostete (SWE-bench-Lauf ctxfix-25) —
    /// für fünf Zeilen, die `grep` schon gefunden hatte. Die Zeilen werden
    /// nummeriert ausgegeben, sonst weiß der Leser nicht, wo er sich befindet.
    ///
    /// **Gedächtnis**: Liest derselbe Agent dieselbe Datei erneut, ohne dass sie
    /// sich geändert hat, wird das vermerkt. Der Inhalt kommt trotzdem — das
    /// Segment könnte inzwischen aus dem Kontext geräumt sein, und ein
    /// verschwiegener Inhalt wäre dann eine Sackgasse. Der Hinweis kostet eine
    /// Zeile und gibt dem Modell die Information, die es sich sonst aus dreißig
    /// Nachrichten zusammensuchen müsste: 280 der 551 Lesevorgänge im selben
    /// Lauf waren Wiederholungen, 37 davon in Agenten, deren Kontext nachweislich
    /// nie aufgeräumt wurde — dort stand der Inhalt noch da.
    pub fn read_file_range(
        &self,
        path: &str,
        von: Option<usize>,
        bis: Option<usize>,
    ) -> Result<String, String> {
        let voll = self.read_file(path)?;
        let hinweis = self.vermerke_lesen(path, &voll);

        let Some(start) = von else {
            return Ok(format!("{voll}{hinweis}"));
        };
        let start = start.max(1);
        let ende = bis.unwrap_or(usize::MAX).max(start);
        let zeilen: Vec<&str> = voll.lines().collect();
        if start > zeilen.len() {
            return Ok(format!(
                "(Datei hat nur {} Zeilen, ab {start} kommt nichts mehr){hinweis}",
                zeilen.len()
            ));
        }
        let ende = ende.min(zeilen.len());
        let ausschnitt: String = zeilen[start - 1..ende]
            .iter()
            .enumerate()
            .map(|(i, z)| format!("{:>6}\t{z}\n", start + i))
            .collect();
        Ok(format!(
            "{ausschnitt}(Zeilen {start}–{ende} von {}){hinweis}",
            zeilen.len()
        ))
    }

    /// Merkt sich, dass `path` mit diesem Inhalt gelesen wurde, und meldet
    /// zurück, ob das eine unveränderte Wiederholung war.
    fn vermerke_lesen(&self, path: &str, inhalt: &str) -> String {
        let fingerabdruck = (
            inhalt.len(),
            inhalt
                .as_bytes()
                .iter()
                .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(*b))),
        );
        let mut gelesen = self.gelesen.lock().expect("Lese-Gedächtnis nicht poisoned");
        match gelesen.insert(path.to_string(), fingerabdruck) {
            Some(vorher) if vorher == fingerabdruck => format!(
                "\n(HINWEIS: Diese Datei hast du in diesem Lauf bereits gelesen, sie ist \
                 seither unverändert. Steht sie noch weiter oben in deinem Kontext, \
                 lies dort nach, statt sie erneut anzufordern.)"
            ),
            _ => String::new(),
        }
    }

    /// Extrahiert den reinen Text aus einer PDF in der Sandbox (Feature `pdf`).
    /// `read_file` liefert bei PDFs nur binären Müll — dieses Tool dekodiert die
    /// Text-Ebene (z. B. für den Accounts-Payable-Workflow). Ein gescanntes Bild-PDF
    /// ohne Text-Ebene ergibt entsprechend leeren Text (kein OCR).
    #[cfg(feature = "pdf")]
    pub fn read_pdf(&self, path: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        // Fehler agenten-freundlich als "ERROR: …"-Ergebnis (kein harter Tool-Fehler).
        Ok(extract_pdf_text(&p).unwrap_or_else(|e| format!("ERROR: {e}")))
    }

    pub fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        let p = self.safe(path)?;
        // Nur BESTEHENDE Dateien sind gesperrt — eine neue anzulegen bleibt
        // erlaubt. Der Unterschied ist gemessen: Im Lauf v3-basis (25
        // SWE-bench-Instanzen) wurden 38 Schreibversuche abgewiesen, und die
        // Namen zeigten, dass der Agent überwiegend seinen EIGENEN
        // Reproduktions-Test anlegen wollte (`test_qdp_case_insensitive.py`).
        // Das ist legitim und kollidiert mit nichts: Der offizielle
        // `test_patch` fasst nur existierende Dateien an. Die Sperre nahm dem
        // Agenten damit die Verifikation weg, statt die Kollision zu
        // verhindern, für die es sie gibt.
        if p.exists() {
            if let Some(muster) = self.ist_geschuetzt(path) {
                return Ok(gesperrt_hinweis(path, &muster));
            }
        }
        self.checkpoint(&p, path);
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
        if let Some(muster) = self.ist_geschuetzt(path) {
            return Ok(gesperrt_hinweis(path, &muster));
        }
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
        self.checkpoint(&p, path);
        std::fs::write(&p, text.replacen(old, new, 1)).map_err(|e| e.to_string())?;
        Ok(format!("{path} geändert."))
    }

    /// Führt ein **read-only** git-Kommando im Workspace aus. Gemeinsame Basis der
    /// `git_*`-Tools: die Argumente werden strukturiert übergeben (nie als Shell-String
    /// interpretiert), Refs/Pfade, die wie Optionen aussehen (`-…`), werden abgelehnt —
    /// damit kann das Modell keine schreibenden git-Optionen einschleusen. Kein
    /// Approval nötig, weil nur lesende Subkommandos aufgerufen werden. Fehler (z. B.
    /// kein git-Repo, unbekannter Ref) kommen als weiches `ERROR: …`-Ergebnis zurück,
    /// damit das Modell sich selbst korrigieren kann.
    fn git(&self, dir: &str, args: &[&str]) -> Result<String, String> {
        // `dir`: optionales Unterverzeichnis (Sandbox-geprüft) — das Repo liegt nicht
        // immer im Workspace-Root (z. B. /app/repo bei Workspace /app).
        let repo = if dir.trim().is_empty() {
            self.inner.workspace.clone()
        } else {
            match self.safe(dir) {
                Ok(p) => p,
                Err(e) => return Ok(format!("ERROR: {e}")),
            }
        };
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&repo)
            .args(["-c", "color.ui=false"])
            .args(args);
        let cancel = self.run.cancel();
        match run_with_timeout(cmd, 30, cancel.as_ref()) {
            Ok(RunOutcome::Done(out)) => {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let text = stdout.trim_end();
                    Ok(if text.is_empty() {
                        "(keine Ausgabe)".to_string()
                    } else {
                        text.chars().take(16000).collect()
                    })
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    Ok(format!(
                        "ERROR: {}",
                        stderr.trim().chars().take(500).collect::<String>()
                    ))
                }
            }
            Ok(RunOutcome::Timeout) => Ok("ERROR: git-Timeout nach 30s.".to_string()),
            Ok(RunOutcome::Cancelled) => Ok(CANCELLED_RESULT.to_string()),
            Err(e) => Ok(format!("ERROR: git nicht ausführbar: {e}")),
        }
    }

    /// Lehnt Refs/Pfade ab, die wie git-Optionen aussehen (Options-Injection-Schutz).
    /// Weicher Fehler (`ERROR: …`-Ergebnis), damit das Modell sich selbst korrigiert —
    /// wie beim ungültigen Regex in `grep`.
    fn git_arg(value: &str) -> Result<&str, String> {
        let v = value.trim();
        if v.starts_with('-') {
            return Err(format!("ERROR: ungültiges git-Argument: {v}"));
        }
        Ok(v)
    }

    /// `git status` (kurz, mit Branch-Zeile).
    pub fn git_status(&self, dir: &str) -> Result<String, String> {
        self.git(dir, &["status", "--short", "--branch"])
    }

    /// `git diff [range] [-- path]`, optional als `--stat`-Übersicht.
    pub fn git_diff(
        &self,
        dir: &str,
        range: &str,
        path: &str,
        stat: bool,
    ) -> Result<String, String> {
        let (range, path) = match (Self::git_arg(range), Self::git_arg(path)) {
            (Ok(r), Ok(p)) => (r, p),
            (Err(e), _) | (_, Err(e)) => return Ok(e),
        };
        let mut args: Vec<&str> = vec!["diff"];
        if stat {
            args.push("--stat");
        }
        if !range.is_empty() {
            args.push(range);
        }
        if !path.is_empty() {
            args.push("--");
            args.push(path);
        }
        self.git(dir, &args)
    }

    /// `git log --oneline -n <limit> [-- path]`.
    pub fn git_log(&self, dir: &str, limit: usize, path: &str) -> Result<String, String> {
        let path = match Self::git_arg(path) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let n = limit.clamp(1, 200).to_string();
        let mut args: Vec<&str> = vec!["log", "--oneline", "--no-decorate", "-n", &n];
        if !path.is_empty() {
            args.push("--");
            args.push(path);
        }
        self.git(dir, &args)
    }

    /// `git show <ref>` — Commit-Metadaten + Patch (Default: HEAD).
    pub fn git_show(&self, dir: &str, reference: &str) -> Result<String, String> {
        let reference = match Self::git_arg(reference) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };
        let r = if reference.is_empty() {
            "HEAD"
        } else {
            reference
        };
        self.git(dir, &["show", r])
    }

    /// Sichert den Zustand einer Datei, BEVOR sie überschrieben wird.
    ///
    /// Wird nur von den schreibenden Tools gerufen, und erst wenn feststeht,
    /// dass wirklich geschrieben wird — ein abgelehnter `edit_file` soll keinen
    /// Eintrag hinterlassen, den man „zurücknehmen" könnte.
    fn checkpoint(&self, p: &Path, anzeige: &str) {
        let vorher = match std::fs::metadata(p) {
            Err(_) => Vorzustand::Fehlte,
            Ok(m) if m.len() as usize > CHECKPOINT_MAX_BYTES => Vorzustand::NichtGesichert,
            Ok(_) => match std::fs::read_to_string(p) {
                Ok(text) => Vorzustand::Inhalt(text),
                // Binär oder nicht lesbar: ehrlich als ungesichert vermerken.
                Err(_) => Vorzustand::NichtGesichert,
            },
        };
        let mut cps = self.checkpoints.lock().unwrap();
        cps.push(Checkpoint {
            pfad: anzeige.to_string(),
            vorher,
        });
        trim_checkpoints(&mut cps);
    }

    /// Wie viele Änderungen sich zurücknehmen lassen.
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.lock().unwrap().len()
    }

    /// Die Pfade der rücknehmbaren Änderungen, jüngste zuerst.
    pub fn checkpoint_paths(&self) -> Vec<String> {
        let cps = self.checkpoints.lock().unwrap();
        cps.iter().rev().map(|c| c.pfad.clone()).collect()
    }

    /// Nimmt die jüngste Datei-Änderung zurück. Gibt eine Meldung für den
    /// Menschen zurück, `None` wenn es nichts zurückzunehmen gibt.
    ///
    /// Ein nicht gesicherter Zustand (zu groß, binär) wird NICHT geraten — der
    /// Eintrag verschwindet, aber die Datei bleibt, und das wird gesagt.
    pub fn undo_last(&self) -> Option<String> {
        let cp = self.checkpoints.lock().unwrap().pop()?;
        let Ok(p) = self.safe(&cp.pfad) else {
            return Some(format!("{}: Pfad nicht mehr gültig.", cp.pfad));
        };
        Some(match cp.vorher {
            Vorzustand::Fehlte => match std::fs::remove_file(&p) {
                Ok(()) => format!("{} gelöscht (gab es vorher nicht).", cp.pfad),
                Err(e) => format!("{}: Löschen fehlgeschlagen: {e}", cp.pfad),
            },
            Vorzustand::Inhalt(text) => match std::fs::write(&p, text) {
                Ok(()) => format!("{} wiederhergestellt.", cp.pfad),
                Err(e) => format!("{}: Wiederherstellen fehlgeschlagen: {e}", cp.pfad),
            },
            Vorzustand::NichtGesichert => format!(
                "{}: war zu groß oder binär — nicht gesichert, Datei bleibt wie sie ist.",
                cp.pfad
            ),
        })
    }

    pub fn run_shell(&self, command: &str) -> Result<String, String> {
        // Leitplanken VOR dem Freigabe-Callback: `-y` lebt im Callback, eine
        // Sperre, die er aufheben könnte, wäre keine. Weiches `Ok(...)` wie bei
        // der Ablehnung durch den Menschen — das Modell soll sich korrigieren,
        // nicht der Lauf abbrechen.
        if let Some(regel) = self.guardrails.verbietet(command) {
            return Ok(format!(
                "ABGELEHNT durch eine Leitplanke der Projekt-Instruktionen: »{regel}«. \
                 Diese Sperre gilt auch mit --yes — such einen anderen Weg."
            ));
        }
        // Geschützte Pfade gelten auch hier — sonst ist die Sperre in
        // `write_file`/`edit_file` bloß eine Umleitung (siehe
        // `shell_schreibt_geschuetztes`).
        if let Some(pfad) = self.shell_schreibt_geschuetztes(command) {
            return Ok(format!(
                "ERROR: Dieses Kommando schreibt auf {pfad}, und dieser Pfad ist in \
diesem Lauf gesperrt — auch über die Shell. Lesen und Ausführen bleibt erlaubt \
(z. B. Tests dort laufen lassen). Ändere den Quellcode, der das gewünschte \
Verhalten herstellt."
            ));
        }
        if self.inner.approval
            && !self.guardrails.erlaubt(command)
            && !(self.inner.approve)(command)
        {
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

        // Timeout + Stop-Knopf: der laufende Befehl wird bei Abbruch sofort
        // beendet, statt bis zum Timeout weiterzulaufen.
        let cancel = self.run.cancel();
        match run_with_timeout(cmd, self.inner.shell_timeout, cancel.as_ref()) {
            Ok(RunOutcome::Done(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let code = out.status.code().unwrap_or(-1);
                let mut full =
                    format!("exit={code}\n--- STDOUT ---\n{stdout}\n--- STDERR ---\n{stderr}");
                // Ein Befehl, der den Arbeitsbaum zurücksetzt, wirft die eigene
                // Arbeit weg — das muss im Ergebnis stehen, sonst merkt es niemand.
                if code == 0 && verwirft_aenderungen(command) {
                    full.push_str(VERWORFEN_HINWEIS);
                }
                // Erfolg ohne jede Ausgabe ist zweideutig, und die Zweideutigkeit
                // kostete im Benchmark-Lauf 2026-08-08 Aufgaben: `python3
                // foo_test.py` auf einer Exercism-Testdatei ohne
                // `unittest.main()` führt NICHTS aus, schweigt und liefert
                // exit=0 — der Agent las daraus „Tests grün". Bei einem
                // Testrunner-Kommando ist Schweigen also gerade kein Beweis.
                if code == 0 && stdout.trim().is_empty() && stderr.trim().is_empty() {
                    full.push_str(STUMM_HINWEIS);
                }
                // Großzügig kappen; die feinere Grenze setzt der Agent über TRUNCATE_LIMIT.
                Ok(full.chars().take(16000).collect())
            }
            Ok(RunOutcome::Timeout) => Ok(format!(
                "ERROR: Timeout nach {}s.",
                self.inner.shell_timeout
            )),
            // Weiches Ergebnis: der Loop beendet den Lauf beim nächsten
            // Cancel-Check selbst mit "(abgebrochen)".
            Ok(RunOutcome::Cancelled) => Ok(CANCELLED_RESULT.to_string()),
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
                "Liest eine Datei aus der Sandbox. Mit 'von'/'bis' nur diesen \
                 Zeilenbereich (1-basiert, 'bis' inklusive) — nutze das für große \
                 Dateien, wenn du die Stelle schon kennst (z. B. aus grep).",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "von": {"type": "integer", "description": "Erste Zeile (1-basiert). Ohne 'bis' bis zum Ende."},
                    "bis": {"type": "integer", "description": "Letzte Zeile, inklusive."}},
                 "required": ["path"]}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    let zahl = |k: &str| args.get(k).and_then(Value::as_u64).map(|v| v as usize);
                    me.read_file_range(path, zahl("von"), zahl("bis"))
                },
            );
        }
        #[cfg(feature = "pdf")]
        if want("read_pdf") {
            let me = self.clone();
            registry.add(
                "read_pdf",
                "Extrahiert den reinen Text aus einer PDF-Datei in der Sandbox (kein OCR).",
                json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    me.read_pdf(path)
                },
            );
        }
        if want("git_status") {
            let me = self.clone();
            registry.add(
                "git_status",
                "Zeigt den git-Status (Branch + geänderte Dateien). Read-only. \
                 Liegt das Repo in einem Unterordner, diesen als 'dir' angeben.",
                json!({"type": "object", "properties": {
                    "dir": {"type": "string", "description": "Repo-Unterordner relativ zum Workspace (leer = Workspace)."}},
                 "required": []}),
                move |args: Value| {
                    let dir = args.get("dir").and_then(Value::as_str).unwrap_or("");
                    me.git_status(dir)
                },
            );
        }
        if want("git_diff") {
            let me = self.clone();
            registry.add(
                "git_diff",
                "Zeigt git-Diffs im Workspace: ohne Argumente die unkommittierten Änderungen, \
                 mit 'range' z. B. 'main..HEAD' oder einen Commit. Read-only, keine Rückfrage.",
                json!({"type": "object", "properties": {
                    "range": {"type": "string", "description": "Commit/Range, z. B. 'main..HEAD' (leer = Arbeitskopie)."},
                    "path": {"type": "string", "description": "Auf diese Datei/dieses Verzeichnis beschränken."},
                    "stat": {"type": "boolean", "description": "Nur Übersicht (--stat) statt vollem Patch."},
                    "dir": {"type": "string", "description": "Repo-Unterordner relativ zum Workspace (leer = Workspace)."}},
                 "required": []}),
                move |args: Value| {
                    let range = args.get("range").and_then(Value::as_str).unwrap_or("");
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    let stat = args.get("stat").and_then(Value::as_bool).unwrap_or(false);
                    let dir = args.get("dir").and_then(Value::as_str).unwrap_or("");
                    me.git_diff(dir, range, path, stat)
                },
            );
        }
        if want("git_log") {
            let me = self.clone();
            registry.add(
                "git_log",
                "Zeigt die git-Historie (--oneline) des Workspace. Read-only.",
                json!({"type": "object", "properties": {
                    "limit": {"type": "integer", "description": "Max. Commits (Default 20)."},
                    "path": {"type": "string", "description": "Nur Commits, die diese Datei berühren."},
                    "dir": {"type": "string", "description": "Repo-Unterordner relativ zum Workspace (leer = Workspace)."}},
                 "required": []}),
                move |args: Value| {
                    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    let dir = args.get("dir").and_then(Value::as_str).unwrap_or("");
                    me.git_log(dir, limit, path)
                },
            );
        }
        if want("git_show") {
            let me = self.clone();
            registry.add(
                "git_show",
                "Zeigt einen Commit (Metadaten + Patch), Default HEAD. Read-only.",
                json!({"type": "object", "properties": {
                    "ref": {"type": "string", "description": "Commit-Hash/Ref (Default 'HEAD')."},
                    "dir": {"type": "string", "description": "Repo-Unterordner relativ zum Workspace (leer = Workspace)."}},
                 "required": []}),
                move |args: Value| {
                    let reference = args.get("ref").and_then(Value::as_str).unwrap_or("");
                    let dir = args.get("dir").and_then(Value::as_str).unwrap_or("");
                    me.git_show(dir, reference)
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

/// Extrahiert den reinen Text aus einer PDF-Datei (Feature `pdf`). Gemeinsame Basis für
/// das sandboxed `read_pdf`-Tool (Agenten) und das `agentkit read-pdf`-Utility (Pipeline).
/// Leerer Text (z. B. gescanntes Bild-PDF ohne Text-Ebene) ⇒ Hinweistext statt Fehler.
#[cfg(feature = "pdf")]
pub fn extract_pdf_text(path: &Path) -> Result<String, String> {
    match pdf_extract::extract_text(path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(
                    "(kein extrahierbarer Text — vermutlich ein gescanntes Bild-PDF \
                    ohne Text-Ebene; hier ist kein OCR verfügbar.)"
                        .to_string(),
                )
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) => Err(format!("PDF konnte nicht gelesen werden: {e}")),
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

/// Der echte (symlink-aufgelöste) Pfad des tiefsten existierenden Vorfahren von
/// `path` — inklusive `path` selbst, falls es ihn schon gibt. `None`, wenn nichts
/// auflösbar ist. Grundlage der zweiten Sandbox-Stufe in [`CodingTools::safe`]:
/// noch nicht existierende Ziele (`write_file`) werden über ihr reales
/// Elternverzeichnis geprüft.
fn real_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if let Ok(real) = current.canonicalize() {
            return Some(real);
        }
        current = current.parent()?;
    }
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
///
/// Ist `root` selbst eine Datei, ist sie das Ergebnis. Ohne diesen Fall lief
/// `read_dir` auf einer Datei in einen Fehler, die Liste blieb leer, und `grep`
/// meldete „(keine Treffer)" — für JEDE Suche in einer einzelnen Datei. In einem
/// SWE-bench-Lauf über 25 Aufgaben waren das 136 von 659 grep-Aufrufen (21 %),
/// alle mit demselben falschen Ergebnis: nicht bloß nutzlos, sondern eine falsche
/// negative Auskunft, auf die das Modell seine nächsten Schritte stützte
/// (typisch: statt fünf Trefferzeilen die ganze Datei per `read_file`).
///
/// Symlinks werden übersprungen (`entry.file_type()` statt `is_dir`/`is_file`,
/// denn nur Ersteres folgt dem Link NICHT): ein Link nach außen würde sonst fremde
/// Dateien in glob/grep spülen, und ein Zyklus (`a -> ..`) ließe den Walk endlos
/// laufen. `file_type()` kommt zudem meist ohne zusätzlichen Syscall aus — der Typ
/// steht schon im `readdir`-Eintrag.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
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
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(p);
            } else if kind.is_file() {
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

/// Segmentweiser Match, `**` matcht null oder mehr Segmente.
///
/// Iterativ mit Rücksprung zum ZULETZT gesehenen `**` statt Rekursion über alle
/// Teilungspunkte. Die rekursive Variante probierte je `**` jede Restlänge durch
/// und wurde bei mehreren Sternen kombinatorisch — ein vom Modell erfundenes
/// `**/**/**/*.rs` reichte, um `glob_files` über einem tiefen Baum festzufahren.
fn match_segs(pat: &[&str], seg: &[&str]) -> bool {
    let (mut p, mut s) = (0, 0);
    // Position des letzten `**` im Muster und wie viel es aktuell schluckt.
    let (mut star, mut eaten) = (None, 0);
    while s < seg.len() {
        if p < pat.len() && pat[p] == "**" {
            star = Some(p);
            eaten = s;
            p += 1;
        } else if p < pat.len() && seg_match(pat[p], seg[s]) {
            p += 1;
            s += 1;
        } else if let Some(sp) = star {
            // Passt nicht: das letzte `**` ein Segment mehr schlucken lassen.
            p = sp + 1;
            eaten += 1;
            s = eaten;
        } else {
            return false;
        }
    }
    // Was vom Muster übrig ist, darf nur noch aus `**` bestehen.
    pat[p..].iter().all(|x| *x == "**")
}

/// Glob innerhalb EINES Pfadsegments: `*` = beliebig viele Zeichen, `?` = eines.
/// Iterativ mit demselben Rücksprung-Verfahren wie [`match_segs`].
fn seg_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let c: Vec<char> = s.chars().collect();
    let (mut pi, mut ci) = (0, 0);
    let (mut star, mut eaten) = (None, 0);
    while ci < c.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            eaten = ci;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == c[ci]) {
            pi += 1;
            ci += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            eaten += 1;
            ci = eaten;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|ch| *ch == '*')
}

/// Ergebnis von [`run_with_timeout`]: regulär fertig, Frist abgelaufen oder
/// von außen abgebrochen (Stop-Knopf). In den letzten beiden Fällen wurde der
/// Kindprozess abgeschossen.
enum RunOutcome {
    Done(std::process::Output),
    Timeout,
    Cancelled,
}

/// Weiches Tool-Ergebnis nach einem Abbruch (ein Sentinel für alle Stellen —
/// nicht zu verwechseln mit dem Lauf-Sentinel `"(abgebrochen)"` des Loops).
const CANCELLED_RESULT: &str = "ERROR: abgebrochen.";

/// Beschreibt dieser `run_shell`-Ausgabetext einen NACHWEISLICH
/// fehlgeschlagenen Lauf?
///
/// Steht hier und nicht im Agent-Loop, weil hier auch das Format entsteht
/// (`exit={code}` bzw. `ERROR: Timeout …`): wer die eine Stelle ändert, sieht
/// die andere.
///
/// Bewusst negativ formuliert. Die Frage „war das erfolgreich?" müsste bei
/// einem fremden `run_shell` mit anderem Ausgabeformat „nein" lauten — und
/// dann gälte dessen Ausführung nie als Verifikation. Die Frage „ist das
/// nachweislich fehlgeschlagen?" antwortet dort „nein" und lässt alles so,
/// wie es war. Blockiert wird nur, was sich belegen lässt.
///
/// Gebraucht wird das von `verify_before_final`: dort galt bisher JEDER
/// ausgeführte Shell-Befehl als Verifikation, auch ein fehlgeschlagener.
/// Beobachtet an `polyglot_python_react`: `pytest -q react_test.py` mit
/// `exit=1`, im nächsten Schritt die Abschlussmeldung — nach 28 von 100
/// Schritten.
pub fn shell_fehlgeschlagen(ergebnis: &str) -> bool {
    (ergebnis.starts_with("exit=") && !ergebnis.starts_with("exit=0"))
        || ergebnis.starts_with("ERROR:")
}

/// Hinweis an einen Befehl, der mit Exit 0 und ohne jede Ausgabe endete.
pub const STUMM_HINWEIS: &str = "\n--- HINWEIS ---\nDieser Befehl endete mit Exit 0, hat \
aber NICHTS ausgegeben. Bei einem Testlauf heißt das in aller Regel: es wurde kein \
einziger Test gesammelt oder ausgeführt — kein Bestehen, sondern ein Fehlschlag der \
Ausführung. Ruf den Testrunner direkt auf (z. B. 'pytest -q <datei>' statt \
'python <datei>') und überzeuge dich an der Ausgabe, wie viele Tests wirklich liefen.";

/// Bekannte Testrunner — die Kommandos, die eine Prüfpflicht wirklich einlösen.
///
/// Vorher genügte JEDES `run_shell` mit Exit 0, um die Pflicht aus
/// [`crate::agent::VERIFY_NUDGE`] als erfüllt zu buchen: ein `ls` reichte.
/// Gemessen im Benchmark-Lauf 2026-08-08 lief `agentkit` dadurch in Fälle wie
/// `django-11019`, wo die Schlussantwort eine Änderung im Detail beschrieb, die
/// es im Arbeitsbaum nicht gab.
const TESTRUNNER: &[&str] = &[
    "pytest",
    "unittest",
    "cargo test",
    "cargo nextest",
    "go test",
    "npm test",
    "npm run test",
    "yarn test",
    "pnpm test",
    "jest",
    "vitest",
    "mocha",
    "tox",
    "make test",
    "make check",
    "ctest",
    "gradle test",
    "mvn test",
    "phpunit",
    "rspec",
    "bats",
    "runtests.py",
    "bin/test",
    "dotnet test",
];

/// Ist dieses Shell-Kommando plausibel eine Prüfung?
///
/// Bewusst eine Positivliste: Was hier fehlt, kostet höchstens einen weiteren
/// Einwurf ([`crate::agent::MAX_VERIFY_NUDGES`] begrenzt das), während eine
/// Negativliste jeden unbekannten Befehl als Beweis durchgehen ließe — und
/// genau das war der Fehler.
///
/// `python foo_test.py` zählt NICHT: eine Exercism-Testdatei ohne
/// `unittest.main()` führt so gar nichts aus und liefert trotzdem Exit 0. Im
/// Lauf 2026-08-08 war dieses Muster bei einem Arm häufiger als `pytest`.
/// Absage an einen gesperrten Schreibversuch — WEICHER Fehler (`ERROR:` im
/// Ok-Zweig), damit das Modell sich selbst korrigiert, statt den Lauf zu
/// verlieren. Nennt das Muster, damit der Agent versteht, was gilt, und sagt
/// ausdrücklich, was er stattdessen tun soll: Ohne diesen Satz schrieb er die
/// Datei im Beobachtungslauf einfach über `run_shell` weiter.
fn gesperrt_hinweis(path: &str, muster: &str) -> String {
    format!(
        "ERROR: {path} ist in diesem Lauf schreibgeschützt (Muster '{muster}'). \
Das ist beabsichtigt; der Schutz greift auch für Schreibversuche über 'run_shell'. \
Lesen und Ausführen bleiben erlaubt — du darfst die Tests dort also laufen lassen. \
Ändere stattdessen den Quellcode, der das gewünschte Verhalten herstellt; brauchst \
du zum Prüfen einen eigenen Test, lege ihn unter einem NICHT geschützten Namen an."
    )
}

/// Minimales Escaping für die Marker-Suche: Wir bauen keine Regex, sondern
/// suchen Teilzeichenketten — es gibt also nichts zu escapen. Die Funktion
/// existiert als benannte Stelle, falls das je auf echte Muster umgestellt wird.
fn regex_lite_escape(s: &str) -> String {
    s.to_string()
}

pub fn shell_ist_pruefung(kommando: &str) -> bool {
    let k = kommando.to_ascii_lowercase();
    TESTRUNNER.iter().any(|r| k.contains(r))
}

/// Der Hinweis, der einem erfolgreichen Zurücksetz-Befehl angehängt wird.
/// Gleichzeitig der Marker, an dem [`shell_hat_verworfen`] ihn wiedererkennt —
/// dieselbe Bauart wie bei [`shell_fehlgeschlagen`]: ein Sentinel im Ergebnis,
/// kein zweiter Kanal.
const VERWORFEN_HINWEIS: &str = "\n--- HINWEIS ---\nDieser Befehl hat Änderungen im \
Arbeitsbaum VERWORFEN. Was du hier zurückgesetzt hast, ist weg — auch dein eigener \
Patch, falls er in diesen Dateien lag. Prüfe mit git_diff/git_status, was jetzt \
tatsächlich noch geändert ist, und stelle die gewollten Änderungen wieder her, bevor \
du abschließt.";

/// Erkennt Shell-Befehle, die Änderungen im Arbeitsbaum zurücknehmen.
///
/// Konservativ und wörtlich: nur die vier git-Formen, die genau das tun. Ein
/// `git checkout <branch>` (ohne `--`) wechselt den Zweig und verwirft nichts,
/// `git stash list` liest nur — beide sind hier bewusst nicht dabei.
fn verwirft_aenderungen(command: &str) -> bool {
    let c = command.split_whitespace().collect::<Vec<_>>().join(" ");
    c.contains("git checkout -- ")
        || c.contains("git restore ")
        || c.contains("git reset --hard")
        || c.contains("git stash push")
        || c.contains("git stash save")
        || c.split(';').any(|t| t.trim() == "git stash")
        || c.contains("git clean -")
}

/// true ⇔ dieses `run_shell`-Ergebnis meldet ein Zurücksetzen des Arbeitsbaums.
///
/// Gebraucht von `verify_before_final`: Wer seine Änderungen wegwirft, hat danach
/// wieder etwas zu verifizieren — der Zustand auf der Platte ist ein anderer als
/// der, den er zuletzt geprüft hat.
///
/// Beobachtet in `django__django-11001` (Lauf ctxfix-25): Der Agent baute einen
/// vollständigen Patch, `git diff` zeigte ihn, danach kam
/// `git checkout -- django/db/models/sql/compiler.py` — und die Schlussmeldung
/// lautete „I fixed the duplicate ORDER BY detection … I verified the change".
/// Eingereicht wurde ein leerer Patch.
pub fn shell_hat_verworfen(ergebnis: &str) -> bool {
    ergebnis.contains(VERWORFEN_HINWEIS)
}

/// Führt ein Kommando mit Timeout und kooperativem Abbruch aus.
///
/// Deshalb bleibt das `Child` hier und wandert nicht in einen Warte-Thread: nur so
/// lässt sich `kill()` überhaupt aufrufen. Vorher lief ein abgelaufener Befehl im
/// Hintergrund weiter — über eine lange Sitzung sammelte der Agent hängende
/// Builds an, die weiter CPU und Sperren hielten. `kill()` erwischt nur den
/// direkten Kindprozess (bash/powershell), nicht dessen eigene Kinder; das ist
/// deutlich besser als nichts, aber keine Garantie.
///
/// Die Pipes werden nebenläufig leergelesen — bliebe das aus, könnte ein Befehl
/// mit viel Ausgabe an einer vollen Pipe blockieren und liefe immer in den Timeout.
fn run_with_timeout(
    mut cmd: Command,
    timeout_secs: u64,
    cancel: Option<&Cancel>,
) -> Result<RunOutcome, String> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;

    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out_reader = drain(child.stdout.take());
    let err_reader = drain(child.stderr.take());

    // Wartezeit von 1 ms auf 20 ms hochlaufen lassen: die git-Tools sind meist nach
    // wenigen Millisekunden fertig und sollen nicht auf einen festen Takt warten,
    // ein langer Build soll den Prozess nicht mit Aufwachen beschäftigen.
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None => {
                let cancelled = stopped(cancel);
                if cancelled || Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(if cancelled {
                        RunOutcome::Cancelled
                    } else {
                        RunOutcome::Timeout
                    });
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(20));
            }
        }
    };
    Ok(RunOutcome::Done(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    }))
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
