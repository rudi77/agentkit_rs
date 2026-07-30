//! Der Store: ein `RwLock<Arc<WorkState>>` plus ein serialisierender Schreiber
//! — dasselbe Muster wie `agentkit_graph::store`, hier über einem [`WorkState`]
//! statt einem `GraphIndex`.
//!
//! ```text
//! Leser A ─┐                     ┌─ Arc<WorkState> (seq 41)
//! Leser B ─┼─ state.read().clone()┤   unveränderlich, so lange gehalten wie nötig
//! Leser C ─┘                     └─ …
//!
//! Schreiber ── writer.lock() ── Kopie des Zustands ── apply() ── Journal ── state.write() = Arc(42)
//! ```
//!
//! Der Commit ist synchron: erst die Kopie mit `apply()` prüfen (schlägt sie
//! fehl, ist nichts passiert), dann das Journal (dauerhaft), dann der Tausch
//! (sichtbar). Scheitert irgendein Schritt, bleibt der alte Snapshot stehen —
//! es gibt keinen Zustand, der im Speicher gilt, aber nicht auf der Platte steht.

mod journal;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::error::WorkError;
use crate::event::WorkEvent;
use crate::model::now_ms;
use crate::state::WorkState;
use journal::Journal;

pub use journal::JOURNAL_FILE;

/// Ab wie vielen Journal-Zeilen im Verhältnis zu den Datensätzen beim Öffnen
/// automatisch neu geschrieben wird. Faktor 2 heißt: „im Schnitt wurde jeder
/// Datensatz einmal geändert" — darüber lohnt das Zusammenfalten, darunter ist
/// es Verschwendung (Muster `agentkit_graph::store::REWRITE_FACTOR`).
const REWRITE_FACTOR: u64 = 2;
const REWRITE_MIN_LINES: u64 = 256;

/// Dateiname der Sperrdatei innerhalb des Projektverzeichnisses (Befund 1 des
/// Code-Reviews). `WorkStore` sperrt sonst nur PROZESSINTERN (`RwLock`/
/// `Mutex`) — nichts hinderte ein zweites `agentkit work run` auf demselben
/// Verzeichnis daran, unabhängig IDs aus seinem eigenen Snapshot zu berechnen
/// und dieselbe ID doppelt ans Journal anzuhängen. Der nächste `WorkStore::open`
/// lief dann beim Replay in die (korrekte) Duplikat-Ablehnung in `state::apply`
/// und lieferte `Err` — DAUERHAFT, denn jeder weitere `open`-Versuch replayt
/// dasselbe kaputte Journal erneut. Aus einer Schutzmaßnahme wurde so ein
/// Totalverlust. Die Sperrdatei verhindert das strukturell: nur ein
/// `WorkStore` pro Verzeichnis kann je gleichzeitig offen sein, im selben
/// Prozess oder über Prozessgrenzen hinweg.
pub const LOCK_FILE: &str = "work.lock";

struct WriteState {
    journal: Option<Journal>,
}

pub struct WorkStore {
    state: RwLock<Arc<WorkState>>,
    writer: Mutex<WriteState>,
    /// `Some` nur bei einem über [`WorkStore::open`] geöffneten Store —
    /// [`WorkStore::in_memory`] hat kein Verzeichnis. Trägt implizit auch,
    /// OB dieser Store eine Sperrdatei hält: sie liegt bei `dir.join(LOCK_FILE)`,
    /// ein eigenes `lock_path`-Feld wäre nur eine abgeleitete Kopie desselben
    /// Pfades (siehe [`Drop for WorkStore`](#impl-Drop-for-WorkStore) und
    /// [`WorkStore::open`], die ihn beide daraus neu bilden).
    dir: Option<PathBuf>,
    /// Wie viele Ereignis-Zeilen `Journal::open` beim Öffnen tatsächlich über
    /// `apply` eingespielt hat (siehe [`WorkStore::events_replayed_on_open`]).
    events_replayed_on_open: u64,
}

/// Legt (falls nötig) das Projektverzeichnis an und erwirbt exklusiv die
/// Sperrdatei darin — `create_new` ist dabei die Atomizität selbst: das
/// Betriebssystem lässt genau EINEN Aufrufer gewinnen, ein Exists-Check davor
/// wäre ein klassisches TOCTOU. Kein `fs2`/`fd-lock` nötig (Guidelines §4,
/// keine neue Dependency ohne Not) — eine `create_new`-Datei ist auf allen
/// drei Zielplattformen (Windows/Linux/musl) exklusiv genug für dieses MVP
/// mit genau einem Worker.
fn acquire_lock(dir: &Path) -> Result<(), WorkError> {
    std::fs::create_dir_all(dir).map_err(|e| WorkError::Io(format!("{}: {e}", dir.display())))?;
    let lock_path = dir.join(LOCK_FILE);
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let holder_pid =
                std::fs::read_to_string(&lock_path).unwrap_or_else(|_| "unbekannt".to_string());
            return Err(WorkError::Locked(format!(
                "Sperrdatei '{}' existiert bereits (angelegt von PID {holder_pid}) — vermutlich \
                 arbeitet schon ein anderer 'agentkit work'-Prozess an diesem Projekt. Beende \
                 ihn, oder entferne die Datei manuell, falls kein Prozess mehr läuft (z. B. nach \
                 einem Absturz) — 'run'/'resume' akzeptieren dafür auch '--force'.",
                lock_path.display()
            )));
        }
        Err(e) => return Err(WorkError::Io(format!("{}: {e}", lock_path.display()))),
    };
    // Best-effort: die PID ist eine Diagnose-Hilfe für die Fehlermeldung oben,
    // kein Teil der Sperr-Logik selbst — ein fehlgeschlagenes Schreiben macht
    // die Sperre nicht ungültig.
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(())
}

impl WorkStore {
    /// Flüchtiger Store ohne Journal — für Tests und Läufe, die nichts
    /// hinterlassen sollen.
    pub fn in_memory() -> Self {
        WorkStore {
            state: RwLock::new(Arc::new(WorkState::default())),
            writer: Mutex::new(WriteState { journal: None }),
            dir: None,
            events_replayed_on_open: 0,
        }
    }

    /// Öffnet das Projektverzeichnis (legt es an) und spielt `work.jsonl` ab.
    /// Ein fehlendes Journal ist ein frisches Projekt, kein Fehler.
    ///
    /// Erwirbt zuerst exklusiv [`LOCK_FILE`] (Befund 1 des Code-Reviews) —
    /// ein zweiter `open`-Aufruf auf demselben Verzeichnis scheitert mit
    /// [`WorkError::Locked`], solange dieser Store (oder ein anderer Prozess)
    /// lebt. Die Sperre wird beim `Drop` des zurückgegebenen Stores wieder
    /// entfernt; scheitert das Öffnen NACH dem Erwerb (z. B. kaputtes
    /// Journal), wird sie hier sofort wieder entfernt — sonst bliebe eine
    /// Sperre für ein Projekt zurück, das nie erfolgreich offen war, und
    /// jeder folgende Versuch schlüge mit "gesperrt" fehl statt mit dem
    /// eigentlichen Fehler.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, WorkError> {
        let dir_path = dir.as_ref().to_path_buf();
        acquire_lock(&dir_path)?;
        Self::open_locked(dir_path.clone()).map_err(|e| {
            let _ = std::fs::remove_file(dir_path.join(LOCK_FILE));
            e
        })
    }

    fn open_locked(dir_path: PathBuf) -> Result<Self, WorkError> {
        let journal_path = dir_path.join(JOURNAL_FILE);
        let (mut journal, state, events_replayed_on_open) = Journal::open(&journal_path)?;

        // Automatische Kompaktierung, wenn viele Ereignisse auf wenige
        // Datensätze treffen — sonst wächst das Journal eines langlebigen
        // Projekts unbegrenzt, obwohl der Zustand klein bleibt. Das ist die
        // EINZIGE Stelle im Crate, an der Journal-Historie bewusst
        // aufgegeben wird (siehe `Journal::rewrite`): ein `checkpoint()`
        // HÄNGT nur noch an (siehe dort), also wächst das Journal jetzt
        // schneller als vor der Korrektur — Faktor 2 („im Schnitt wurde
        // jeder Datensatz einmal geändert") bleibt trotzdem die richtige
        // Schwelle, weil ein Checkpoint je bearbeitetem Item genau EINE
        // zusätzliche Zeile beiträgt (keinen zusätzlichen Datensatz) und die
        // Ereignis-Zeilen je Item (Claim/Finish/Completed/…) das Verhältnis
        // ohnehin dominieren; `REWRITE_MIN_LINES` verhindert zusätzlich, dass
        // ein kleines, kurzlebiges Projekt überhaupt neu geschrieben wird.
        let records = state.record_count() as u64;
        if journal.lines() > REWRITE_MIN_LINES && journal.lines() > REWRITE_FACTOR * records.max(1)
        {
            journal.rewrite(state.seq, now_ms(), &state)?;
        }

        Ok(WorkStore {
            state: RwLock::new(Arc::new(state)),
            writer: Mutex::new(WriteState {
                journal: Some(journal),
            }),
            dir: Some(dir_path),
            events_replayed_on_open,
        })
    }

    /// Sperrfreier Lesepfad (Befund 0 der Handprobe): liest und spielt das
    /// Journal GENAUSO ab wie [`WorkStore::open`], nimmt dabei aber NIE
    /// [`LOCK_FILE`] und legt auch sonst nichts an — reine Lesebefehle
    /// (`status`/`items`/`events`/`list`/`watch`) sollen funktionieren,
    /// während ein `agentkit work run` im selben Verzeichnis die Sperre hält.
    /// `events` liest die Datei ohnehin schon direkt selbst und war nie
    /// betroffen; die anderen vier riefen bisher `WorkStore::open` und
    /// scheiterten deshalb mit [`WorkError::Locked`], solange ein Lauf aktiv war
    /// — für eine Laufzeit, deren erklärter Zweck stundenlange Läufe sind,
    /// genau der Moment, in dem man nachsehen will.
    ///
    /// Gibt einen [`WorkState`]-WERT zurück, keinen `WorkStore` — das ist die
    /// strukturelle Absicherung, nicht nur eine Konvention: `WorkState` hat
    /// keine `submit`/`submit_with`/`checkpoint`-Methode, also gibt es
    /// schlicht KEINEN Aufruf, über den ein Leser (versehentlich oder nicht)
    /// schreiben könnte — der Compiler weist das ab, nicht erst ein
    /// `WorkError` zur Laufzeit. Ein Wrapper-Typ mit einer internen
    /// „schreibgeschützt"-Markierung wäre schwächer: er bräuchte weiterhin
    /// eine `submit`-Methode, die dann bei jedem Aufruf erst zur Laufzeit
    /// prüfen müsste, ob sie das darf.
    ///
    /// Eine unvollständige letzte Journal-Zeile (ein Absturz — oder schlicht
    /// ein GERADE laufendes `append` eines anderen Prozesses) wird wie beim
    /// schreibenden Pfad toleriert und aus der Projektion verworfen, aber
    /// NICHT auf der Platte „repariert" (siehe `Journal::open_read_only`) —
    /// ein Leser darf die Datei nie verändern, während ein Schreiber sie
    /// vielleicht gerade hält.
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<WorkState, WorkError> {
        let journal_path = dir.as_ref().join(JOURNAL_FILE);
        Journal::open_read_only(&journal_path)
    }

    /// Entfernt eine vorhandene Sperrdatei ohne sie zu prüfen — der Ausweg für
    /// `--force` an `agentkit work run`/`resume`, wenn ein abgestürzter
    /// Prozess (`SIGKILL`, harter Absturz) sie hinterlassen hat und nie mehr
    /// selbst aufräumen kann. Kein Fehler, wenn keine Sperre existiert.
    ///
    /// Bewusst KEIN automatischer "PID lebt nicht mehr, also aufräumen"-Check:
    /// das Betriebssystem vergibt PIDs irgendwann neu, "lebt Prozess X noch"
    /// ist plattformübergreifend (Windows/Linux) nicht zuverlässig zu
    /// beantworten, und ein FALSCHES "tot" würde eine Sperre entfernen, die
    /// tatsächlich noch einen lebenden Schreiber schützt — genau der Schaden,
    /// den die Sperre verhindern soll. Das Entfernen bleibt deshalb ein
    /// bewusster, expliziter Schritt des Bedieners (`--force`), nie ein
    /// automatischer.
    pub fn force_unlock(dir: impl AsRef<Path>) -> std::io::Result<()> {
        match std::fs::remove_file(dir.as_ref().join(LOCK_FILE)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Wie viele Ereignis-Zeilen dieser Store beim Öffnen tatsächlich über
    /// `apply` eingespielt hat — NICHT die Gesamtzeilenzahl des Journals.
    /// Reine Beobachtungs-Naht für Tests (siehe `Journal::open`-Doku): belegt
    /// messbar, dass die Wiederaufnahme nur die Ereignisse NACH der letzten
    /// Snapshot-Zeile verarbeitet, nicht die gesamte Historie davor.
    pub fn events_replayed_on_open(&self) -> u64 {
        self.events_replayed_on_open
    }

    /// Verzeichnis des Projekts (`None` bei [`WorkStore::in_memory`]).
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Ein konsistenter, unveränderlicher Stand. Genau ein kurzer Read-Lock —
    /// alles Weitere läuft sperrfrei auf dem zurückgegebenen `Arc`.
    pub fn snapshot(&self) -> Arc<WorkState> {
        self.state
            .read()
            .expect("Work-State-Lock nicht poisoned")
            .clone()
    }

    /// Wendet ein Ereignis an. Reihenfolge als Invariante (siehe Moduldoku):
    /// 1. Kopie des Snapshots holen und `apply()` darauf ausführen — schlägt
    ///    das fehl (ungültiges Ereignis, verbotener Übergang), ist nichts
    ///    geschrieben und der alte Zustand bleibt exakt stehen.
    /// 2. Journal anhängen — ab hier ist das Ereignis dauerhaft.
    /// 3. Neuen Snapshot einsetzen — ab hier ist es für JEDEN Leser sichtbar.
    ///
    /// Dünner Wrapper über [`WorkStore::submit_with`] für den (häufigeren)
    /// Fall, dass das Ereignis schon fertig gebaut ist und keine ID aus dem
    /// gerade gültigen Zustand vergeben werden muss.
    pub fn submit(&self, event: WorkEvent) -> Result<u64, WorkError> {
        self.submit_with(move |_state| Ok(event))
            .map(|(seq, _)| seq)
    }

    /// Wie [`WorkStore::submit`], baut das Ereignis aber ERST INNERHALB des
    /// Schreiber-Locks aus dem dann gültigen Zustand. Nur so darf eine ID
    /// vergeben werden: `WorkState::next_*_id()` außerhalb des Locks zu lesen
    /// und das Ereignis danach zu submitten ist ein Rennen — zwei parallele
    /// Tool-Aufrufe (der Agent-Kern führt mehrere Tool-Aufrufe EINER
    /// Modellantwort parallel in `std::thread::scope` aus) berechnen dieselbe
    /// ID, und der zweite `submit` überschreibt den ersten Datensatz still.
    /// Gibt zusätzlich das tatsächlich angewandte Ereignis zurück, weil der
    /// Aufrufer die im Lock vergebene ID sonst nicht erfährt.
    ///
    /// Dieselbe Reihenfolge-Invariante wie `submit`: schlägt `build` fehl
    /// (z. B. eine zyklische Abhängigkeit), wird nichts geschrieben — weder
    /// Journal noch Snapshot ändern sich.
    pub fn submit_with<F>(&self, build: F) -> Result<(u64, WorkEvent), WorkError>
    where
        F: FnOnce(&WorkState) -> Result<WorkEvent, WorkError>,
    {
        let mut writer = self.writer.lock().expect("Work-Writer-Lock nicht poisoned");
        let current = self.snapshot();

        let event = build(&current)?;

        let mut next = (*current).clone();
        next.apply(&event)?;
        let seq = current.seq + 1;
        next.seq = seq;

        if let Some(journal) = writer.journal.as_mut() {
            journal.append(seq, now_ms(), &event)?;
        }

        *self.state.write().expect("Work-State-Lock nicht poisoned") = Arc::new(next);
        Ok((seq, event))
    }

    /// Schreibt einen Checkpoint: journalt `CheckpointCreated` auf einer Kopie
    /// des Zustands (bumpt `seq`, mutiert sonst nichts — reine
    /// Journal-Markierung, siehe `state::apply`) und HÄNGT genau diesen Stand
    /// über [`Journal::append_snapshot`] als neue Zeile an. Das Journal wird
    /// dabei NICHT gekürzt — die Historie bleibt vollständig erhalten, nur
    /// der Replay-Startpunkt für den nächsten `WorkStore::open` wandert auf
    /// diese Zeile (siehe `Journal::open`). Ein früherer Entwurf rief hier
    /// `Journal::rewrite` und ersetzte damit das GESAMTE Journal durch diese
    /// eine Zeile — nach dem ersten abgeschlossenen Work Item zeigte
    /// `agentkit work events` dann nur noch "snapshot" statt der Zeitleiste,
    /// die §14 des Konzepts als Zweck des Event-Logs nennt (Audit, Debugging,
    /// Replay von Fehlerfällen). Echte Kompaktierung passiert seitdem nur
    /// noch in `WorkStore::open`, oberhalb einer Schwelle (siehe dort).
    ///
    /// Ohne Journal (`in_memory`) ein No-Op, das den Snapshot trotzdem
    /// fortschreibt, damit der zurückgegebene `seq`-Wert stimmt.
    pub fn checkpoint(&self) -> Result<u64, WorkError> {
        let mut writer = self.writer.lock().expect("Work-Writer-Lock nicht poisoned");
        let current = self.snapshot();
        let at = now_ms();
        let seq = current.seq + 1;

        let mut next = (*current).clone();
        next.apply(&WorkEvent::CheckpointCreated { seq, at_ms: at })?;
        next.seq = seq;

        if let Some(journal) = writer.journal.as_mut() {
            journal.append_snapshot(seq, at, &next)?;
        }

        *self.state.write().expect("Work-State-Lock nicht poisoned") = Arc::new(next);
        Ok(seq)
    }

    /// Zeilen im Journal (Tests/Diagnose); `None` ohne Journal.
    pub fn journal_lines(&self) -> Option<u64> {
        let writer = self.writer.lock().expect("Work-Writer-Lock nicht poisoned");
        writer.journal.as_ref().map(Journal::lines)
    }
}

impl std::fmt::Debug for WorkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.snapshot();
        f.debug_struct("WorkStore")
            .field("seq", &snapshot.seq)
            .field("items", &snapshot.items.len())
            .field("dir", &self.dir)
            .finish()
    }
}

impl Drop for WorkStore {
    /// Gibt die Sperrdatei wieder frei, damit ein normaler (nicht
    /// abgestürzter) Programmablauf keine Altlast hinterlässt — jeder Test,
    /// der `WorkStore::open` auf demselben Verzeichnis mehrfach NACHEINANDER
    /// aufruft (Neustart-Simulation), verlässt sich darauf, dass der vorherige
    /// Store seine Sperre hier tatsächlich löst. Best-effort: ein
    /// fehlgeschlagenes `remove_file` (Datei schon weg, Berechtigung) wird
    /// stillschweigend ignoriert — ein `Drop` darf nicht paniken.
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_file(dir.join(LOCK_FILE));
        }
    }
}
