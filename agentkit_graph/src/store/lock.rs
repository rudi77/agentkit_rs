//! Sperre je Commit und der Stempel, an dem ein Store fremde Commits erkennt.
//!
//! Zwei Prozesse auf demselben Bundle vergaben früher dieselben IDs: jeder
//! zählte von seinem eigenen Stand beim Öffnen weiter. Zwei Dateien lösen das:
//!
//! - [`LOCK_FILE`] wird **nur für die Dauer eines Commits** gehalten
//!   (`create_new` ist die Atomizität, die PID steht als Inhalt darin). Nicht
//!   über die Lebensdauer des Prozesses — zwei offene Stores auf einem Bundle
//!   sind der Normalfall (zwei Terminals im selben Workspace), kein Fehler.
//! - [`STAMP_FILE`] wird am ENDE jedes Commits neu geschrieben. Wer die Sperre
//!   hält und einen anderen Stempel vorfindet als den zuletzt gesehenen, lädt
//!   das Bundle neu, bevor er IDs vergibt.
//!
//! Lesen sperrt nie: weder `open_read_only` noch ein Snapshot fasst eine der
//! beiden Dateien an.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::error::GraphError;
use crate::model::GraphRevision;

pub(crate) const LOCK_FILE: &str = "graph.lock";
pub(crate) const STAMP_FILE: &str = "graph.stamp";

/// Ab diesem Alter gilt eine Sperre als verwaist, auch wenn sich ihr Halter
/// nicht als tot nachweisen lässt (kein `/proc`, oder die PID ist inzwischen
/// neu vergeben). Ein Commit dauert Millisekunden; 30 Sekunden sind weit
/// jenseits davon und trotzdem kurz genug, dass niemand nach einem Absturz
/// von Hand aufräumen muss.
const STALE_AFTER: Duration = Duration::from_secs(30);

/// Wie lange auf eine lebende Sperre gewartet wird. Länger als [`STALE_AFTER`],
/// damit ein Wartender eine verwaiste Sperre noch selbst übernehmen kann,
/// statt mit einem Fehler aufzugeben.
const WAIT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(3);

/// Macht Namen und Stempel innerhalb eines Prozesses eindeutig — zwei Stores
/// im selben Prozess teilen sich die PID.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn io_err(path: &Path, e: std::io::Error) -> GraphError {
    GraphError::Io(format!("{}: {e}", path.display()))
}

/// Die gehaltene Commit-Sperre. Gibt sie beim `Drop` frei.
pub(crate) struct CommitLock {
    path: PathBuf,
}

impl CommitLock {
    /// Erwirbt die Sperre in `root` (legt das Verzeichnis bei Bedarf an) und
    /// wartet dabei auf einen lebenden Halter.
    ///
    /// Wurde eine verwaiste Sperre übernommen, ist ihr Halter MITTEN in einem
    /// Commit gestorben: auf der Platte können Dokumente liegen, die kein
    /// Stempel angekündigt hat. Deshalb wird dann sofort ein neuer Stempel
    /// geschrieben — jeder Store, dieser eingeschlossen, lädt daraufhin neu.
    pub(crate) fn acquire(root: &Path) -> Result<CommitLock, GraphError> {
        std::fs::create_dir_all(root).map_err(|e| io_err(root, e))?;
        let path = root.join(LOCK_FILE);
        let deadline = Instant::now() + WAIT;
        let mut taken_over = false;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    // Best-effort: die PID dient der Verwaist-Erkennung; fehlt
                    // sie, greift die Altersgrenze.
                    let _ = write!(file, "{}", std::process::id());
                    let _ = file.flush();
                    let lock = CommitLock { path };
                    if taken_over {
                        write_stamp(root, 0)?;
                    }
                    return Ok(lock);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_stale(&path) {
                        taken_over |= take_over(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        return Err(GraphError::Io(format!(
                            "{}: das Bundle ist seit über {} s gesperrt (PID {}) — \
                             schreibt dort noch ein anderer Prozess?",
                            path.display(),
                            WAIT.as_secs(),
                            if holder.is_empty() {
                                "unbekannt"
                            } else {
                                &holder
                            }
                        )));
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(io_err(&path, e)),
            }
        }
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Verwaist ist eine Sperre, deren Halter nachweislich tot ist, oder die älter
/// ist als [`STALE_AFTER`]. Eine gerade verschwundene Datei ist NICHT verwaist
/// — der Aufrufer versucht dann einfach erneut, sie anzulegen.
fn is_stale(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let pid = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        // Ohne libc gibt es nur unter Linux einen Weg, das zu fragen. Die
        // eigene PID ist nie tot (zweiter Store im selben Prozess).
        let proc_root = Path::new("/proc/self");
        if pid != std::process::id()
            && proc_root.exists()
            && !Path::new(&format!("/proc/{pid}")).exists()
        {
            return true;
        }
    }
    meta.modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age > STALE_AFTER)
}

/// Räumt eine verwaiste Sperre weg. `true`, wenn wirklich eine verwaiste
/// entfernt wurde.
///
/// Nicht einfach `remove_file`: zwischen der Prüfung und dem Löschen kann ein
/// anderer Wartender die verwaiste Sperre schon ersetzt haben, und das Löschen
/// träfe dessen frische. Das Umbenennen ist atomar — genau ein Aufrufer
/// bekommt eine bestimmte Datei — und geprüft wird danach, WAS man bekommen
/// hat. War es doch eine lebende Sperre, kommt sie zurück an ihren Platz
/// (`hard_link` scheitert, statt zu überschreiben, falls dort schon wieder
/// eine liegt). Es bleibt ein Fenster von Mikrosekunden, das nur offensteht,
/// wenn ein Absturz mitten im Commit UND zwei gleichzeitig Wartende
/// zusammentreffen.
fn take_over(path: &Path) -> bool {
    let grave = path.with_file_name(format!(
        "{LOCK_FILE}.verwaist-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::rename(path, &grave).is_err() {
        return false;
    }
    let stale = is_stale(&grave);
    if !stale {
        let _ = std::fs::hard_link(&grave, path);
    }
    let _ = std::fs::remove_file(&grave);
    stale
}

/// Der zuletzt geschriebene Stempel; leer, wenn es (noch) keinen gibt — ein
/// Bundle aus der Zeit vor der Sperre hat keinen, und „kein Stempel" ist
/// selbst ein vergleichbarer Zustand.
pub(crate) fn read_stamp(root: &Path) -> String {
    std::fs::read_to_string(root.join(STAMP_FILE)).unwrap_or_default()
}

/// Schreibt einen neuen, nie zuvor vergebenen Stempel und liefert ihn zurück.
/// Nur unter gehaltener [`CommitLock`] aufrufen.
///
/// Die Revision steht nur der Lesbarkeit wegen darin; verglichen wird der
/// ganze Text auf Gleichheit.
pub(crate) fn write_stamp(root: &Path, revision: GraphRevision) -> Result<String, GraphError> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stamp = format!(
        "{revision} {} {} {nanos}\n",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    crate::okf::bundle::write_file_atomic(&root.join(STAMP_FILE), &stamp)?;
    Ok(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graph_lock_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn die_sperre_wird_beim_drop_freigegeben() {
        let root = temp_root("drop");
        {
            let _lock = CommitLock::acquire(&root).unwrap();
            assert!(root.join(LOCK_FILE).exists());
        }
        assert!(!root.join(LOCK_FILE).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Eine Sperre mit der PID eines toten Prozesses blockiert nicht — und
    /// hinterlässt einen neuen Stempel, damit jeder Store neu lädt.
    #[test]
    fn eine_verwaiste_sperre_wird_uebernommen() {
        let root = temp_root("verwaist");
        // Ein Kindprozess, der schon beendet ist: seine PID ist sicher tot.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(root.join(LOCK_FILE), dead_pid.to_string()).unwrap();
        // Ohne /proc greift nur die Altersgrenze — dann die Datei altern lassen.
        if !Path::new("/proc/self").exists() {
            let old = SystemTime::now() - STALE_AFTER - Duration::from_secs(5);
            let file = OpenOptions::new()
                .write(true)
                .open(root.join(LOCK_FILE))
                .unwrap();
            file.set_modified(old).unwrap();
        }
        let vorher = read_stamp(&root);

        let begin = Instant::now();
        let lock = CommitLock::acquire(&root).unwrap();
        assert!(
            begin.elapsed() < Duration::from_secs(5),
            "übernommen, nicht ausgesessen"
        );
        assert_ne!(
            read_stamp(&root),
            vorher,
            "Übernahme schreibt einen Stempel"
        );
        drop(lock);
        assert!(!root.join(LOCK_FILE).exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn eine_lebende_sperre_wird_abgewartet() {
        let root = temp_root("lebend");
        let lock = CommitLock::acquire(&root).unwrap();
        let root2 = root.clone();
        let waiter = std::thread::spawn(move || {
            let begin = Instant::now();
            let _lock = CommitLock::acquire(&root2).unwrap();
            begin.elapsed()
        });
        std::thread::sleep(Duration::from_millis(80));
        drop(lock);
        let waited = waiter.join().unwrap();
        assert!(waited >= Duration::from_millis(50), "{waited:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn jeder_stempel_ist_neu() {
        let root = temp_root("stempel");
        assert_eq!(read_stamp(&root), "");
        let a = write_stamp(&root, 7).unwrap();
        let b = write_stamp(&root, 7).unwrap();
        assert_ne!(a, b);
        assert_eq!(read_stamp(&root), b);
        std::fs::remove_dir_all(&root).ok();
    }
}
