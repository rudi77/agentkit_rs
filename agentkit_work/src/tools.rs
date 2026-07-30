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
use crate::graph::{ClaimText, GraphGateway, WorkProvenance};
use crate::model::{
    id_order, now_ms, ArtifactKind, AttemptId, ProjectId, RunId, WorkArtifact, WorkItem,
    WorkItemId, WorkItemKind, WorkItemStatus,
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
Beleg). Ohne diesen Aufruf gilt dein Versuch als unvollständig.
- 'work_claim' — falls ein Wissensgraph angebunden ist: halte dauerhaft \
nützliche Erkenntnisse fest (Hypothesen, Beobachtungen, gescheiterte \
Ansätze). NICHT der Arbeitsfortschritt selbst — der gehört ins Artefakt und \
in 'work_submit'.";

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
    /// Projekt, zu dem dieser Versuch gehört — fehlte bisher, weil kein Tool
    /// es brauchte; `work_claim` baut daraus die `WorkProvenance` (Phase 4).
    pub project_id: ProjectId,
    /// Git-Revision des Workspace bei Laufstart (`WorkRun::base_revision`),
    /// soweit bekannt — wandert in die `WorkProvenance` jedes über
    /// `work_claim` festgehaltenen Claims.
    pub repository_revision: Option<String>,
    /// Zugang zum Wissensgraphen (Phase 4/§25) — `None` ohne `--graph DIR`
    /// oder ohne das Feature `graph`. Nur dann existiert `work_claim`
    /// überhaupt (siehe `register_work_tools`): Fähigkeit entscheidet bei der
    /// Registrierung, nicht im Tool-Körper (Muster
    /// `agentkit_graph::register_graph_tools`/`access.can_write()`).
    pub gateway: Option<Arc<dyn GraphGateway>>,
    /// Gesetzt, wenn DIESER Versuch ein Prüf-Item bearbeitet (`WorkItem::verifies`,
    /// aus `VerificationPolicy::IndependentAgent` erzeugt, Phase 5b) — trägt
    /// die ID des geprüften Items. Nur dann registriert `register_work_tools`
    /// das Tool `work_verdict` (Fähigkeit entscheidet bei der Registrierung,
    /// nicht im Tool-Körper, dasselbe Muster wie `gateway`/`work_claim`).
    pub verifies: Option<WorkItemId>,
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

#[derive(Deserialize)]
struct ClaimArg {
    subject: String,
    predicate: String,
    object: String,
    confidence: Option<f32>,
    excerpt: Option<String>,
}

#[derive(Deserialize)]
struct ClaimArgs {
    claims: Vec<ClaimArg>,
}

#[derive(Deserialize)]
struct VerdictArgs {
    approved: bool,
    reason: String,
}

/// Standard-Konfidenz eines `work_claim`-Eintrags ohne explizite Angabe.
/// Etwas höher als `agentkit_graph`s eigener Default (0.6, `ClaimDraft::new`):
/// eine Aussage aus einer abgeschlossenen Arbeitseinheit mit Artefakten und
/// Provenance ist im Schnitt besser belegt als eine beiläufige Chat-Notiz.
const DEFAULT_CLAIM_CONFIDENCE: f32 = 0.7;

/// Registriert die Work-Tools in `tools`: immer `work_add_item`/
/// `work_artifact`/`work_submit`, dazu `work_claim`, aber NUR wenn
/// `ctx.gateway` gesetzt ist (Fähigkeit entscheidet bei der Registrierung,
/// nicht im Tool-Körper — Muster `agentkit_graph::register_graph_tools`).
/// `store` wird je Tool geklont (steckt schon hinter `Arc`); aus `ctx` wird
/// je Tool nur geklont, was die jeweilige Closure wirklich braucht — ein
/// voller `ctx.clone()` hielte auch die Felder am Leben, die dieses Tool nie
/// liest (z. B. `submission` in `work_add_item`).
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
                        // Verifikation über 'work_add_item' ist nicht im
                        // Umfang von Phase 5a (nur `--items`/CLI setzen eine
                        // Policy) — ein zur Laufzeit vom Agenten erzeugtes
                        // Item bekommt den Default `None`, wie bisher.
                        verification_policy: crate::model::VerificationPolicy::None,
                        verifies: None,
                        claims_promoted: false,
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

    // ---------------------------------------------------------- work_claim
    // Nur registriert, wenn ein Gateway vorhanden ist — Fähigkeit entscheidet
    // bei der Registrierung, nicht im Tool-Körper (siehe Funktionsdoku und
    // `agentkit_graph::register_graph_tools`).
    if let Some(gateway) = ctx.gateway.clone() {
        let s = store.clone();
        let (project_id, run_id, work_item_id, attempt_id, agent_id, repository_revision) = (
            ctx.project_id.clone(),
            ctx.run_id.clone(),
            ctx.work_item_id.clone(),
            ctx.attempt_id.clone(),
            ctx.agent_id.clone(),
            ctx.repository_revision.clone(),
        );
        tools.add_typed(
            "work_claim",
            "Hält dauerhaft nützliche Erkenntnisse dieses Versuchs im Wissensgraphen fest \
             (Hypothesen, Beobachtungen, gescheiterte Ansätze) — MIT Provenance zu diesem \
             Work Item. Nicht für den Arbeitsfortschritt selbst, der gehört in 'work_artifact' \
             und 'work_submit'.",
            json!({
                "type": "object",
                "properties": {
                    "claims": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": {"type": "string", "description": "Worüber die Aussage geht."},
                                "predicate": {"type": "string", "description": "Kurze, normalisierte Beziehung, z. B. 'verursacht'."},
                                "object": {"type": "string", "description": "Womit das Subjekt in Beziehung steht."},
                                "confidence": {"type": "number", "description": "0.0 bis 1.0 (Standard 0.7)."},
                                "excerpt": {"type": "string", "description": "Belegstelle: Ausgabe, Zitat oder Fundort."}
                            },
                            "required": ["subject", "predicate", "object"]
                        },
                        "description": "Eine oder mehrere Aussagen, in einem Aufruf abgelegt."
                    }
                },
                "required": ["claims"]
            }),
            move |args: ClaimArgs| -> String {
                if args.claims.is_empty() {
                    return soft("claims darf nicht leer sein");
                }
                let mut claims: Vec<ClaimText> = Vec::with_capacity(args.claims.len());
                for c in &args.claims {
                    let subject = c.subject.trim();
                    let predicate = c.predicate.trim();
                    let object = c.object.trim();
                    if subject.is_empty() || predicate.is_empty() || object.is_empty() {
                        return soft("subject, predicate und object dürfen nicht leer sein");
                    }
                    let confidence = c.confidence.unwrap_or(DEFAULT_CLAIM_CONFIDENCE);
                    if !(0.0..=1.0).contains(&confidence) {
                        return soft(format!(
                            "confidence muss zwischen 0.0 und 1.0 liegen, war {confidence}"
                        ));
                    }
                    claims.push(ClaimText {
                        subject: subject.to_string(),
                        predicate: predicate.to_string(),
                        object: object.to_string(),
                        confidence,
                        excerpt: c
                            .excerpt
                            .as_deref()
                            .map(str::trim)
                            .filter(|e| !e.is_empty())
                            .map(str::to_string),
                    });
                }

                // Artefaktpfade DIESES Versuchs aus dem Store — das Modell
                // gibt sie nicht an, siehe `WorkProvenance`-Doku.
                let artifact_paths: Vec<String> = s
                    .snapshot()
                    .artifacts
                    .values()
                    .filter(|a| a.attempt_id == attempt_id)
                    .map(|a| a.rel_path.clone())
                    .collect();

                let prov = WorkProvenance {
                    project_id: project_id.clone(),
                    run_id: run_id.clone(),
                    work_item_id: work_item_id.clone(),
                    attempt_id: attempt_id.clone(),
                    agent_id: agent_id.clone(),
                    artifact_paths,
                    repository_revision: repository_revision.clone(),
                };

                match gateway.record_claims(&prov, &claims) {
                    Ok(claim_ids) => {
                        // Journalt am Versuch, damit Phase 5 die IDs
                        // wiederfindet (§14) — HÄNGT an (siehe event.rs): ein
                        // Versuch darf 'work_claim' mehrfach aufrufen.
                        if let Err(e) = s.submit(WorkEvent::ClaimsRecorded {
                            attempt: attempt_id.clone(),
                            claim_ids: claim_ids.clone(),
                            at_ms: now_ms(),
                        }) {
                            return soft(e);
                        }
                        format!(
                            "{} Aussage(n) festgehalten: {}",
                            claim_ids.len(),
                            claim_ids.join(", ")
                        )
                    }
                    Err(e) => soft(e),
                }
            },
        );
    }

    // -------------------------------------------------------- work_verdict
    // Nur registriert, wenn DIESER Versuch ein Prüf-Item bearbeitet
    // (`ctx.verifies` gesetzt) — Fähigkeit entscheidet bei der Registrierung,
    // nicht im Tool-Körper (dasselbe Muster wie `work_claim`/`ctx.gateway`
    // oben). Ein normales Item bekommt dieses Tool nie zu sehen.
    if let Some(reviewed_item_id) = ctx.verifies.clone() {
        let s = store.clone();
        let reviewer_agent_id = ctx.agent_id.clone();
        let gateway = ctx.gateway.clone();
        tools.add_typed(
            "work_verdict",
            "Meldet das Urteil über das geprüfte Work Item — genau EINMAL pro Prüf-Item. \
             'approved' entscheidet über Zustimmung oder Ablehnung, 'reason' ist IMMER \
             Pflicht (auch bei Zustimmung): der Text landet im Journal und, bei Ablehnung, \
             im nächsten Arbeitspaket des Autors. Prüfe die Akzeptanzkriterien — mach die \
             Arbeit nicht neu.",
            json!({
                "type": "object",
                "properties": {
                    "approved": {
                        "type": "boolean",
                        "description": "true = Versuch akzeptiert, false = abgelehnt."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Begründung des Urteils — Pflicht, auch bei Zustimmung."
                    }
                },
                "required": ["approved", "reason"]
            }),
            move |args: VerdictArgs| -> String {
                let reason = args.reason.trim();
                if reason.is_empty() {
                    return soft("reason darf nicht leer sein — auch bei Zustimmung");
                }
                let reason = reason.to_string();

                let snapshot = s.snapshot();
                let Some(reviewed) = snapshot.items.get(&reviewed_item_id) else {
                    return soft(format!(
                        "geprüftes Item '{reviewed_item_id}' existiert nicht (interner \
                         Zustandsfehler)"
                    ));
                };
                if reviewed.status != WorkItemStatus::AwaitingVerification {
                    return soft(format!(
                        "geprüftes Item '{reviewed_item_id}' wartet nicht (mehr) auf eine \
                         Prüfung (Status: '{}')",
                        reviewed.status
                    ));
                }
                let policy = reviewed.verification_policy.clone();
                let Some(lease) = snapshot.leases.get(&reviewed_item_id) else {
                    // Strukturell unerreichbar (siehe cli.rs
                    // `require_awaiting_human_approval`): `AwaitingVerification`
                    // behält sein Lease bewusst. Klare Fehlermeldung statt Panic.
                    return soft(format!(
                        "geprüftes Item '{reviewed_item_id}': kein offener Versuch gefunden \
                         (interner Zustandsfehler)"
                    ));
                };
                let attempt_id = lease.attempt_id.clone();
                let at_ms = now_ms();

                if args.approved {
                    if let Err(e) = s.submit(WorkEvent::VerificationApproved {
                        item: reviewed_item_id.clone(),
                        attempt: attempt_id.clone(),
                        by: reviewer_agent_id.clone(),
                        reason: Some(reason),
                        at_ms,
                    }) {
                        return soft(e);
                    }
                    if let Err(e) = s.submit(WorkEvent::WorkItemCompleted {
                        item: reviewed_item_id.clone(),
                        attempt: attempt_id,
                        at_ms,
                    }) {
                        return soft(e);
                    }
                    let warning = crate::graph::promote_after_completion(
                        &s,
                        gateway.as_ref(),
                        &reviewed_item_id,
                        &policy,
                        at_ms,
                    );
                    match warning {
                        None => format!(
                            "Urteil erfasst: '{reviewed_item_id}' freigegeben und abgeschlossen."
                        ),
                        Some(w) => format!(
                            "Urteil erfasst: '{reviewed_item_id}' freigegeben und abgeschlossen. \
                             Achtung: {w}"
                        ),
                    }
                } else {
                    if let Err(e) = s.submit(WorkEvent::VerificationRejected {
                        item: reviewed_item_id.clone(),
                        attempt: attempt_id.clone(),
                        by: reviewer_agent_id.clone(),
                        reason: reason.clone(),
                        at_ms,
                    }) {
                        return soft(e);
                    }
                    // Derselbe Mechanismus wie eine automatisierte/menschliche
                    // Ablehnung (siehe `runner::record_verification_rejected`,
                    // `cli::cmd_reject`) — eine einzige gepflegte Kopie der
                    // "ist max_attempts erschöpft?"-Entscheidung.
                    let released = crate::recovery::finish_failed_attempt(
                        &s,
                        &reviewed_item_id,
                        &attempt_id,
                        |item| {
                            format!(
                                "Wiederholung {}/{} (unabhängige Prüfung abgelehnt: {reason})",
                                item.attempt_count + 1,
                                item.max_attempts
                            )
                        },
                        at_ms,
                    );
                    match released {
                        Ok(true) => format!(
                            "Urteil erfasst: '{reviewed_item_id}' abgelehnt und zurückgesetzt \
                             auf 'pending'."
                        ),
                        Ok(false) => format!(
                            "Urteil erfasst: '{reviewed_item_id}' abgelehnt — Versuche \
                             ausgeschöpft, bleibt 'failed'."
                        ),
                        Err(e) => soft(e),
                    }
                }
            },
        );
    }

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
