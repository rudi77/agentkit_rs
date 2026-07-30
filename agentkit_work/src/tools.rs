//! Die Work-Tools — die einzige Sicht des ausführenden Agenten auf sein Work Item.
//!
//! Registriert werden sie in eine **vorhandene** [`ToolRegistry`]; der Agent-Kern
//! kennt dieses Crate nicht (Muster `agentkit_graph::tools`). Was ein Versuch
//! darf, steckt im [`WorkToolCtx`], den die Closures einschließen: Lauf, Item,
//! Versuch und Agent-Identität setzt die Laufzeit, nie das Modell. Deshalb hat
//! kein Tool ein `run_id`-, `work_item_id`-, `attempt_id`- oder `agent_id`-
//! Argument — es gäbe sonst einen Weg, sich als jemand anderes auszugeben oder
//! an einem fremden Item zu arbeiten.
//!
//! Fehlerkontrakt (siehe `agent_framework_rs/CLAUDE.md` und
//! `agentkit_graph::tools`): alles, was das Modell falsch machen kann — leeres
//! Feld, unbekannter Enum-Wert, zyklische Abhängigkeit, Pfadausbruch — ist ein
//! weicher Fehler `"ERROR: …"`, aus dem sich das Modell selbst korrigieren kann.
//! Ein echtes `Err` gibt es hier nicht; auch ein [`WorkError`] aus dem Store
//! wird über [`soft`] in einen weichen Fehler übersetzt.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentkit::ToolRegistry;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::event::WorkEvent;
use crate::model::{
    id_order, now_ms, ArtifactKind, AttemptId, RunId, WorkArtifact, WorkItem, WorkItemId,
    WorkItemKind, WorkItemStatus,
};
use crate::store::WorkStore;

/// Prompt-Fragment für Frontends, die die Work-Tools einklinken (analog zu
/// `GRAPH_SYSTEM` in agentkit_graph). Wird an den agenten-spezifischen
/// Zusatz-Prompt angehängt, nicht in den Coding-Prompt eingebaut — der
/// Agent-Kern kennt Work Items nicht.
pub const WORK_SYSTEM: &str = "\
## Work Item

Du bearbeitest genau EIN Work Item aus einem laufenden Vorhaben — nicht das \
gesamte Vorhaben und nicht die Folge-Items. Deine Antwort selbst zählt nicht \
als Ergebnis.

- 'work_artifact' — lege dein Ergebnis (Analyse, Code, Test, Dokumentation) als \
Datei ab. Der nächste Agent liest sie über 'read_file', nicht aus deiner \
Antwort — schreib dort hinein, nicht in den Chat.
- 'work_add_item' — nur, wenn während der Arbeit wirklich eine neue, \
abgegrenzte Teilaufgabe nötig wird, die vorher niemand vorhergesehen hat. Kein \
Zerlegen um des Zerlegens willen: die meisten Versuche brauchen kein einziges \
'work_add_item'.
- 'work_submit' — schließe JEDEN Versuch damit ab, auch einen gescheiterten. \
Bewerte darin jedes Akzeptanzkriterium einzeln (erfüllt oder nicht, mit \
Beleg). Ohne diesen Aufruf gilt dein Versuch als unvollständig.";

/// Erlaubte Wire-Werte von [`WorkItemKind`] — für die Fehlermeldung, wenn das
/// Modell einen unbekannten Wert schickt (die Werte selbst kommen aus dem
/// `serde(rename_all = "snake_case")` in `model.rs`, hier nur zur Anzeige).
const ITEM_KINDS: [&str; 7] = [
    "discovery",
    "analysis",
    "planning",
    "implementation",
    "test",
    "review",
    "documentation",
];

/// Erlaubte Wire-Werte von [`ArtifactKind`], analog zu [`ITEM_KINDS`].
const ARTIFACT_KINDS: [&str; 5] = ["analysis", "code", "test", "documentation", "other"];

/// Weicher Fehler ans Modell (kein `Err` — siehe Fehlerkontrakt in der
/// Moduldoku): ein String, aus dem sich das Modell selbst korrigieren kann.
/// Anders als `agentkit_swarm::dynamic::soft` liefert diese Variante direkt
/// `String` statt `Result<String, String>` — die Tools hier laufen über
/// [`ToolRegistry::add_typed`], dessen Closures `R: ToString` liefern, keinen
/// `Result`; das rohe `add` (das `dynamic.rs` nutzt) gibt es hier nicht her.
fn soft(msg: impl std::fmt::Display) -> String {
    format!("ERROR: {msg}")
}

/// Deserialisiert einen Freitext-Wert gegen ein fieldloses Enum mit
/// `#[serde(rename_all = "snake_case")]` — die JSON-Repräsentation eines
/// solchen Enums ist einfach der Variantenname als String, also reicht der
/// Umweg über `Value::String` statt einer von Hand gepflegten `match`-Tabelle.
fn parse_wire<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_string())).ok()
}

/// Pfadanzeige mit `/` als Trenner, unabhängig vom Betriebssystem — passend zu
/// `rel_path`, das laut Plan immer mit `/` im Journal steht.
fn display_unix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Prüft, dass `filename` ein einfacher Dateiname ist — kein Pfad, kein
/// Ausbruch. Lexikalische Stufe (siehe [`resolve_artifact_path`] für die
/// zweite Stufe): funktioniert auch für Dateien, die es noch nicht gibt, folgt
/// aber keinem Symlink. Vom Vorbild `CodingTools::safe` in
/// `agent_framework_rs/src/coding.rs` bewusst NICHT wiederverwendet (`safe`
/// ist privat und für beliebig tiefe, mehrteilige Pfade unter einem Workspace
/// gebaut) — hier ist die Eingabeform enger: das Tool-Schema erlaubt von
/// vornherein nur einen einzelnen, bereits geprüften Dateinamen ohne
/// Zwischenverzeichnisse, eine eigene schmale Prüfung passt also besser als
/// eine gemeinsame Abstraktion für zwei verschiedene Formen (Guidelines §2).
const RESERVED_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn validate_filename(filename: &str) -> Result<(), String> {
    if filename.trim().is_empty() {
        return Err("filename darf nicht leer sein".to_string());
    }
    // Führende/nachfolgende Leerzeichen erlaubt Windows in der Datei zwar
    // nicht (sie werden beim Schreiben stillschweigend entfernt), aber genau
    // das wäre die Falle: `rel_path` im Journal enthielte sie weiter, während
    // die tatsächlich geschriebene Datei sie nicht hat — zwei Wahrheiten, die
    // auseinanderlaufen. Ein legitimer Dateiname braucht sie nie.
    if filename != filename.trim() {
        return Err(format!(
            "filename '{filename}' darf keine führenden/nachfolgenden Leerzeichen enthalten"
        ));
    }
    if filename.contains('/') || filename.contains('\\') {
        return Err(format!(
            "filename '{filename}' darf keinen Verzeichnistrenner enthalten — nur ein \
             einfacher Dateiname ist erlaubt"
        ));
    }
    if filename.contains("..") {
        return Err(format!("filename '{filename}' darf kein '..' enthalten"));
    }
    // JEDER Doppelpunkt, nicht nur einer an Position 1: ein Laufwerksbuchstabe
    // sitzt zwar immer dort ('C:...'), aber Windows' Alternate-Data-Stream-
    // Syntax ('befund.md:versteckt') hängt den Doppelpunkt irgendwo mitten im
    // Namen an — ein Inhalt läge dann in einem Stream, den `read_file`,
    // `list_files` & Co. nie sehen, während das Journal die Datei als normal
    // vorhanden meldet. Ein legitimer Dateiname braucht nie einen Doppelpunkt.
    if filename.contains(':') {
        return Err(format!(
            "filename '{filename}' darf keinen ':' enthalten (Laufwerksbuchstabe oder \
             Alternate Data Stream)"
        ));
    }
    // Reservierte Gerätenamen (Windows, unabhängig von Groß-/Kleinschreibung
    // und einer eventuellen Endung wie 'NUL.txt'): der Schreibversuch würde
    // gegen das Gerät statt eine Datei laufen — `CON` z. B. gegen die Konsole,
    // quer durch den stdout/stderr-Vertrag der CLI. Das Journal würde ein
    // Artefakt behaupten, das nie im Dateisystem landet.
    let stem = filename.split('.').next().unwrap_or(filename);
    if RESERVED_WINDOWS_NAMES
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
    {
        return Err(format!(
            "filename '{filename}' ist unter Windows ein reservierter Gerätename"
        ));
    }
    // Kein separater `Path::is_absolute()`-Check: jede absolute Form (führendes
    // `/`, `\`, oder ein Laufwerksbuchstabe) enthält zwangsläufig eines der
    // Zeichen oben — ein zusätzlicher Check wäre nie erreichbar. Was die
    // Zeichen-Checks NICHT fangen, sind Sonderkomponenten wie `.` (aktuelles
    // Verzeichnis) — dafür bleibt die Komponentenzerlegung unten (Gürtel und
    // Hosenträger, echtes zweites Netz, kein toter Code): sie verlangt genau
    // EINEN normalen Namen und lehnt `.` und alles andere ab.
    let mut components = Path::new(filename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(format!(
            "filename '{filename}' muss ein einfacher Dateiname sein"
        )),
    }
}

/// Baut den Zielpfad für ein Artefakt und legt das Versuchs-Verzeichnis an.
/// Zweite Sicherheitsstufe nach [`validate_filename`]: kanonisiert das (jetzt
/// existierende) Elternverzeichnis und prüft, dass es wirklich unter
/// `artifacts_dir` liegt — die Zieldatei selbst existiert noch nicht, daher
/// wird nicht sie, sondern ihr Elternverzeichnis kanonisiert.
///
/// Der Zielpfad ist `artifacts_dir/work_item_id/attempt_id/filename`, nicht
/// nur `artifacts_dir/work_item_id/filename` — Regressionskorrektur: hinge
/// der Pfad nur am Item, träfe ein WIEDERHOLTER Versuch (nach einem
/// Fehlschlag desselben Items) beim erneuten Ablegen desselben Dateinamens
/// auf eine schon existierende Datei seines Vorgänger-Versuchs und würde am
/// `create_new` unten scheitern, obwohl Retry ein Kernfeature dieser Laufzeit
/// ist. Mit dem Versuch im Pfad kollidiert `create_new` nur noch, was
/// innerhalb DESSELBEN Versuchs zweimal denselben Namen benutzt — genau das
/// soll sichtbar scheitern (siehe Kommentar an der Schreibstelle). Nebeneffekt:
/// der Pfad trägt jetzt die Provenance — wer `artifacts/W-3/` ansieht, sieht
/// direkt, welcher Versuch was erzeugt hat, statt nur das letzte Überschreiben.
///
/// Restrisiko wie bei `CodingTools::safe` (`agent_framework_rs/src/coding.rs`):
/// kein Schutz gegen TOCTOU auf der Zieldatei selbst — existiert unter
/// `filename` bereits ein Symlink, der nach draußen zeigt, folgt `fs::write`
/// ihm beim Schreiben. Die Sandbox bremst ein irrendes Modell, sie ist keine
/// Sicherheitsgrenze gegen einen Angreifer mit Schreibrechten im Workspace.
fn resolve_artifact_path(
    artifacts_dir: &Path,
    work_item_id: &str,
    attempt_id: &str,
    filename: &str,
) -> Result<PathBuf, String> {
    validate_filename(filename)?;
    let attempt_dir = artifacts_dir.join(work_item_id).join(attempt_id);
    std::fs::create_dir_all(&attempt_dir).map_err(|e| {
        format!(
            "Verzeichnis '{}' nicht anlegbar: {e}",
            attempt_dir.display()
        )
    })?;

    let canon_parent = attempt_dir.canonicalize().map_err(|e| {
        format!(
            "Verzeichnis '{}' nicht auflösbar: {e}",
            attempt_dir.display()
        )
    })?;
    let canon_root = artifacts_dir.canonicalize().map_err(|e| {
        format!(
            "artifacts_dir '{}' nicht auflösbar: {e}",
            artifacts_dir.display()
        )
    })?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(format!("filename '{filename}' verlässt artifacts_dir"));
    }

    Ok(attempt_dir.join(filename))
}

/// Wie ein einzelnes Akzeptanzkriterium nach einem Versuch bewertet wurde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriterionCheck {
    pub criterion: String,
    pub met: bool,
    pub evidence: String,
}

/// Was ein Versuch als Ergebnis gemeldet hat. Der Runner liest das nach dem
/// Agentenlauf aus, weil nur er Schritte und Tool-Aufrufe zählen kann (siehe
/// `AttemptFinished` in `event.rs`, das der Runner journalt, nicht dieses Tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSubmission {
    pub summary: String,
    pub criteria: Vec<CriterionCheck>,
}

/// Der Arbeitskontext EINES Versuchs. Laufzeit-Daten, die das Modell nie
/// übergeben darf — deshalb steckt sie hier und nicht in den Tool-Argumenten
/// (siehe Moduldoku).
#[derive(Clone)]
pub struct WorkToolCtx {
    pub run_id: RunId,
    pub work_item_id: WorkItemId,
    pub attempt_id: AttemptId,
    pub agent_id: String,
    pub max_attempts: u32,
    /// `<projektverzeichnis>/artifacts`. MUSS relativ zur Workspace-Wurzel des
    /// ausführenden Agenten sein, nicht absolut: `work_artifact` meldet den
    /// Erfolg mit genau diesem Pfad (siehe dort), und `read_file`/`CodingTools::safe`
    /// verankern ihre Sandbox an der kanonisierten Workspace-Wurzel — ein
    /// absoluter Pfad läge für sie außerhalb, selbst wenn er tatsächlich
    /// innerhalb des Workspace liegt. Aufgabe des Runners (Schritt 5), der
    /// diesen Kontext baut.
    pub artifacts_dir: PathBuf,
    /// Ergebnis des Versuchs; der Runner liest es nach `execute` aus.
    pub submission: Arc<Mutex<Option<WorkSubmission>>>,
}

#[derive(Deserialize)]
struct AddItemArgs {
    title: String,
    description: String,
    kind: String,
    priority: Option<u8>,
    #[serde(default)]
    depends_on: Vec<WorkItemId>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
}

#[derive(Deserialize)]
struct ArtifactArgs {
    kind: String,
    filename: String,
    content: String,
    summary: String,
}

#[derive(Deserialize)]
struct CriterionArg {
    criterion: String,
    met: bool,
    evidence: String,
}

#[derive(Deserialize)]
struct SubmitArgs {
    summary: String,
    #[serde(default)]
    criteria: Vec<CriterionArg>,
}

/// Registriert die drei Work-Tools in `tools`. `store` wird je Tool geklont
/// (steckt schon hinter `Arc`); aus `ctx` wird je Tool nur geklont, was die
/// jeweilige Closure wirklich braucht — ein voller `ctx.clone()` hielte auch
/// die Felder am Leben, die dieses Tool nie liest (z. B. `submission` in
/// `work_add_item`).
pub fn register_work_tools(tools: &mut ToolRegistry, store: Arc<WorkStore>, ctx: WorkToolCtx) {
    // ------------------------------------------------------- work_add_item
    let s = store.clone();
    let (run_id, max_attempts) = (ctx.run_id.clone(), ctx.max_attempts);
    tools.add_typed(
        "work_add_item",
        "Legt ein neues Work Item im laufenden Vorhaben an. Nur benutzen, wenn während \
         der Arbeit wirklich eine neue, abgegrenzte Teilaufgabe nötig wird.",
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Kurzer Titel der Teilaufgabe."},
                "description": {"type": "string", "description": "Was zu tun ist."},
                "kind": {
                    "type": "string",
                    "enum": ITEM_KINDS,
                    "description": "Art der Teilaufgabe."
                },
                "priority": {
                    "type": "integer",
                    "description": "0 (niedrig) bis 9 (hoch), Standard 5."
                },
                "depends_on": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "IDs von Work Items, die vorher abgeschlossen sein müssen."
                },
                "acceptance_criteria": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Woran der Erfolg dieser Teilaufgabe geprüft wird."
                }
            },
            "required": ["title", "description", "kind"]
        }),
        move |args: AddItemArgs| -> String {
            let title = args.title.trim();
            if title.is_empty() {
                return soft("title darf nicht leer sein");
            }
            let title = title.to_string();
            let description = args.description.trim();
            if description.is_empty() {
                return soft("description darf nicht leer sein");
            }
            let description = description.to_string();
            let Some(kind) = parse_wire::<WorkItemKind>(&args.kind) else {
                return soft(format!(
                    "unbekannte kind '{}' — erlaubt sind: {}",
                    args.kind,
                    ITEM_KINDS.join(", ")
                ));
            };
            let priority = args.priority.unwrap_or(5);
            if priority > 9 {
                return soft(format!(
                    "priority muss zwischen 0 und 9 liegen, war {priority}"
                ));
            }
            let depends_on = args.depends_on;
            let acceptance_criteria = args.acceptance_criteria;
            let run_id = run_id.clone();

            // ID-Vergabe, Zyklenprüfung UND Ereignisbau liegen bewusst in
            // EINER Closure innerhalb des Schreiber-Locks (`submit_with`):
            // der Agent-Kern führt mehrere Tool-Aufrufe EINER Modellantwort
            // parallel in `std::thread::scope` aus, und das Zerlegen eines
            // Vorhabens ruft `work_add_item` typischerweise mehrfach in genau
            // so einer Antwort auf. Snapshot lesen und danach submitten (der
            // frühere Fehler hier) hätte zwei Threads dieselbe ID berechnen
            // lassen — der zweite `submit` hätte den ersten Datensatz still
            // überschrieben.
            let result = s.submit_with(move |snapshot| {
                let id = snapshot.next_item_id();
                snapshot.validate_dependencies(&id, &depends_on)?;
                Ok(WorkEvent::WorkItemCreated {
                    item: WorkItem {
                        id: id.clone(),
                        run_id,
                        title,
                        description,
                        kind,
                        status: WorkItemStatus::Pending,
                        priority,
                        // Numerischer Teil der ID: monoton, weil `item_seq`
                        // in `WorkState` nur wächst — reicht als
                        // Erzeugungsreihenfolge, ohne einen eigenen Zähler
                        // mitzuführen.
                        seq: id_order(&id),
                        required_role: None,
                        dependencies: depends_on,
                        acceptance_criteria,
                        attempt_count: 0,
                        max_attempts,
                        updated_at_ms: now_ms(),
                    },
                })
            });
            match result {
                Ok((_, WorkEvent::WorkItemCreated { item })) => {
                    format!("Work Item '{}' angelegt (Status: pending).", item.id)
                }
                Ok(_) => {
                    unreachable!("submit_with liefert das gebaute Ereignis unverändert zurück")
                }
                Err(e) => soft(e),
            }
        },
    );

    // -------------------------------------------------------- work_artifact
    let s = store.clone();
    let (artifacts_dir, work_item_id, attempt_id) = (
        ctx.artifacts_dir.clone(),
        ctx.work_item_id.clone(),
        ctx.attempt_id.clone(),
    );
    tools.add_typed(
        "work_artifact",
        "Legt das Ergebnis des Versuchs als Datei unter dem Work Item ab und gibt den \
         Pfad zurück, unter dem sie mit 'read_file' lesbar ist. Schreib das Ergebnis \
         hierher statt in deine Antwort.",
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ARTIFACT_KINDS,
                    "description": "Art des Artefakts."
                },
                "filename": {
                    "type": "string",
                    "description": "Einfacher Dateiname ohne Pfad, z. B. 'analyse.md'."
                },
                "content": {"type": "string", "description": "Der vollständige Dateiinhalt."},
                "summary": {"type": "string", "description": "Kurze Zusammenfassung des Inhalts."}
            },
            "required": ["kind", "filename", "content", "summary"]
        }),
        move |args: ArtifactArgs| -> String {
            let Some(kind) = parse_wire::<ArtifactKind>(&args.kind) else {
                return soft(format!(
                    "unbekannte kind '{}' — erlaubt sind: {}",
                    args.kind,
                    ARTIFACT_KINDS.join(", ")
                ));
            };
            let summary = args.summary.trim();
            if summary.is_empty() {
                return soft("summary darf nicht leer sein");
            }
            let summary = summary.to_string();

            let target = match resolve_artifact_path(
                &artifacts_dir,
                &work_item_id,
                &attempt_id,
                &args.filename,
            ) {
                Ok(p) => p,
                Err(e) => return soft(e),
            };

            // Exklusives Schreiben AUSSERHALB des Schreiber-Locks (Befund 4
            // des Code-Reviews): vorher lag hier ein einfaches `fs::write`,
            // das zwei parallele Aufrufe mit demselben `filename` wortlos
            // überschreiben ließ — BEIDE meldeten Erfolg, obwohl das früher
            // journalte Artefakt hinterher auf einen Inhalt zeigte, den es
            // nie erzeugt hat. `create_new` macht das Schreiben selbst
            // exklusiv (Atomizität durch das Betriebssystem, kein
            // Exists-Check als TOCTOU nötig) — UNABHÄNGIG vom Store-Lock: der
            // zweite gleichnamige Aufruf trifft auf eine schon existierende
            // Datei und scheitert SICHTBAR (weicher Fehler), statt sie zu
            // ersetzen. Deshalb muss das Schreiben nicht mehr im
            // Schreiber-Lock laufen wie bei der ID-Vergabe (`work_add_item`)
            // — ein langsamer/großer Artefaktinhalt würde sonst den Lock
            // halten, der JEDEN anderen parallelen Tool-Aufruf derselben
            // Modellantwort serialisiert (`std::thread::scope`).
            //
            // Seit der Pfadkorrektur (`resolve_artifact_path`) liegt der
            // Zielpfad je VERSUCH, nicht mehr je Item: `create_new` greift
            // deshalb nur noch innerhalb DESSELBEN Versuchs — ein
            // Wiederholungsversuch desselben Items hat ein eigenes
            // Verzeichnis und trifft nie auf die Datei seines Vorgängers.
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .and_then(|mut file| std::io::Write::write_all(&mut file, args.content.as_bytes()))
            {
                return soft(if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "Datei '{}' existiert bereits (z. B. durch einen parallelen \
                         'work_artifact'-Aufruf mit demselben Dateinamen oder eine frühere \
                         Ablage) — wähle einen anderen Dateinamen.",
                        target.display()
                    )
                } else {
                    format!("Datei '{}' nicht schreibbar: {e}", target.display())
                });
            }

            // Mit `/` als Trenner, plattformunabhängig im Journal — der
            // Versuch steckt jetzt mit im Pfad (siehe `resolve_artifact_path`).
            let rel_path = format!("artifacts/{work_item_id}/{attempt_id}/{}", args.filename);
            let workspace_path = display_unix(&target);
            let artifact_work_item_id = work_item_id.clone();
            let artifact_attempt_id = attempt_id.clone();
            // ID-Vergabe im Schreiber-Lock — dieselbe Race-Begründung wie bei
            // `work_add_item`: zwei parallele Aufrufe dürfen nie dieselbe
            // Artefakt-ID berechnen. Die Datei existiert an dieser Stelle
            // bereits (siehe oben); scheitert `submit_with` dennoch (z. B.
            // Journal-I/O), bleibt nur eine verwaiste, aber harmlose Datei
            // zurück, die kein Datensatz referenziert.
            let result = s.submit_with(move |snapshot| {
                Ok(WorkEvent::ArtifactCreated {
                    artifact: WorkArtifact {
                        id: snapshot.next_artifact_id(),
                        work_item_id: artifact_work_item_id,
                        attempt_id: artifact_attempt_id,
                        kind,
                        rel_path,
                        summary,
                        created_at_ms: now_ms(),
                    },
                })
            });
            match result {
                Ok(_) => format!(
                    "Artefakt gespeichert unter '{workspace_path}' — mit 'read_file' lesbar."
                ),
                Err(e) => soft(e),
            }
        },
    );

    // --------------------------------------------------------- work_submit
    tools.add_typed(
        "work_submit",
        "Schließt den aktuellen Versuch ab: Zusammenfassung plus Bewertung jedes \
         Akzeptanzkriteriums. Zwingend am Ende jedes Versuchs, auch bei Misserfolg. \
         Journalt nichts direkt — der Runner liest das Ergebnis nach dem Lauf aus.",
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "description": "Zusammenfassung des Ergebnisses."},
                "criteria": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "criterion": {"type": "string", "description": "Welches Akzeptanzkriterium."},
                            "met": {"type": "boolean", "description": "Erfüllt oder nicht."},
                            "evidence": {"type": "string", "description": "Beleg dafür."}
                        },
                        "required": ["criterion", "met", "evidence"]
                    },
                    "description": "Bewertung jedes Akzeptanzkriteriums aus dem Work Item."
                }
            },
            "required": ["summary"]
        }),
        move |args: SubmitArgs| -> String {
            let summary = args.summary.trim();
            if summary.is_empty() {
                return soft("summary darf nicht leer sein");
            }
            let criteria: Vec<CriterionCheck> = args
                .criteria
                .into_iter()
                .map(|crit| CriterionCheck {
                    criterion: crit.criterion,
                    met: crit.met,
                    evidence: crit.evidence,
                })
                .collect();
            let unmet = criteria.iter().filter(|c| !c.met).count();

            // Mehrfacher Aufruf gewinnt der letzte: der Runner liest das Feld
            // erst NACH dem Agentenlauf aus, ein Zwischenstand zählt nie.
            *ctx.submission
                .lock()
                .expect("Work-Submission-Lock nicht poisoned") = Some(WorkSubmission {
                summary: summary.to_string(),
                criteria,
            });

            if unmet > 0 {
                format!(
                    "Ergebnis übernommen. Achtung: {unmet} Akzeptanzkriterium/-kriterien als \
                     nicht erfüllt markiert."
                )
            } else {
                "Ergebnis übernommen.".to_string()
            }
        },
    );
}
