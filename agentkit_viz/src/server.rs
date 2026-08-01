//! Der HTTP-Adapter: tiny_http außen, [`crate::api`] innen.
//!
//! **Sicherheit.** Ein Trace enthält alles, was der Agent gelesen und
//! geschrieben hat — Dateiinhalte, Shell-Ausgaben, Modellantworten. Dieser
//! Server liefert das aus. Deshalb:
//!
//! - Er bindet AUSSCHLIESSLICH an `127.0.0.1`. Kein `0.0.0.0`, keine Option
//!   dafür — ein Tippfehler soll das Leseloch nicht ins Netz öffnen.
//! - Jede Anfrage braucht das beim Start erzeugte Zufalls-Token (`?t=…`). Das
//!   schützt nicht gegen den Nutzer selbst, aber gegen jedes andere Programm
//!   auf demselben Rechner, das blind auf `localhost:<port>` zugreift — auch
//!   gegen eine beliebige Webseite im Browser des Nutzers.
//! - Er schreibt nichts. Es gibt keinen schreibenden Endpunkt.
//!
//! **Einfädig.** Jede Anfrage ist ein JSON aus dem Speicher; ein Threadpool
//! wäre Maschinerie ohne Nutzen. Die einzige potenziell langsame Stelle ist das
//! Nachlesen der Trace-Datei, und das passiert je Anfrage genau einmal
//! (Offset-basiert, siehe [`crate::trace`]).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use crate::api::{self, ApiCtx, Query};
use crate::project::TraceState;
use crate::trace::{list_traces, TraceFileInfo, TraceReader};

/// Die eine HTML-Seite; Stil und Skript werden beim Ausliefern hineingelegt
/// (siehe [`page`]).
const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

/// Womit der Betrachter gestartet wird.
pub struct VizConfig {
    /// Verzeichnis mit den Trace-Dateien (`--trace DIR` des Laufs).
    pub trace_dir: PathBuf,
    /// Eine BESTIMMTE Trace-Datei statt der jüngsten (`--trace-file`).
    pub trace_file: Option<PathBuf>,
    /// Wurzel der Work-Projekte, falls der Work-Reiter gefüllt werden soll.
    pub work_root: Option<PathBuf>,
    /// Verzeichnis des Wissensgraphen, falls der Graph-Reiter gefüllt werden soll.
    pub graph_dir: Option<PathBuf>,
    /// 0 = freien Port vom Betriebssystem wählen lassen.
    pub port: u16,
}

/// Der laufende Betrachter.
pub struct VizServer {
    server: Server,
    token: String,
    cfg: VizConfig,
    /// Die gerade gelesene Datei plus ihr Zustand. `None`, solange es keine
    /// Trace-Datei gibt — der Betrachter darf VOR dem ersten Lauf starten.
    sitzung: Option<Sitzung>,
    /// Letzter Lesefehler, damit er in der Oberfläche steht statt nur auf stderr.
    fehler: Option<String>,
    /// Die zuletzt erwanderte Sitzungsliste, mit Startzeitpunkt und Dauer des
    /// Durchlaufs (siehe [`VizServer::liste_auffrischen`]).
    dateien: Vec<TraceFileInfo>,
    dateien_stand: Option<(Instant, u128)>,
    /// Der zur aktiven Sitzung gehörende Wissensgraph, falls es einen gibt
    /// (siehe [`VizServer::graph_der_sitzung`]).
    graph_sitzung: Option<PathBuf>,
    /// Dasselbe für die Work-Projekte der Sitzung.
    work_sitzung: Option<PathBuf>,
}

/// Ab welcher Dauer ein Sitzungs-Durchlauf als teuer gilt.
const TEUER_AB_MS: u128 = 20;

/// Wie lange ein TEURER Durchlauf wiederverwendet wird.
const LISTE_GUELTIG_MS: u128 = 2_000;

struct Sitzung {
    reader: TraceReader,
    state: TraceState,
    /// Der Sitzungsname — der Pfad relativ zur Trace-Wurzel. Das ist die
    /// Kennung, die `/api/runs` ausweist und die als `run=` zurückkommt.
    name: String,
}

impl VizServer {
    /// Bindet an `127.0.0.1:<port>`. Ein belegter Port ist ein harter Fehler —
    /// still auf einen anderen auszuweichen hieße, die URL zu ändern, die der
    /// Nutzer gerade aufgeschrieben bekommt.
    pub fn bind(cfg: VizConfig) -> Result<VizServer, String> {
        let server = Server::http(("127.0.0.1", cfg.port))
            .map_err(|e| format!("Port {} nicht bindbar: {e}", cfg.port))?;
        Ok(VizServer {
            server,
            token: token(),
            cfg,
            sitzung: None,
            fehler: None,
            dateien: Vec::new(),
            dateien_stand: None,
            graph_sitzung: None,
            work_sitzung: None,
        })
    }

    /// Der Port, auf dem tatsächlich gelauscht wird (bei `port = 0` der vom
    /// Betriebssystem gewählte).
    pub fn port(&self) -> u16 {
        self.server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .unwrap_or(0)
    }

    /// Bindet der Server wirklich nur auf das Loopback-Interface?
    pub fn is_loopback(&self) -> bool {
        self.server
            .server_addr()
            .to_ip()
            .is_some_and(|a| a.ip().is_loopback())
    }

    /// Die aufzurufende Adresse — inklusive Token.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?t={}", self.port(), self.token)
    }

    /// Bedient Anfragen, bis der Prozess endet.
    pub fn run(mut self) {
        loop {
            if !self.handle_one() {
                return;
            }
        }
    }

    /// Genau EINE Anfrage bedienen. `false` = der Server ist zu.
    pub fn handle_one(&mut self) -> bool {
        let Ok(request) = self.server.recv() else {
            return false;
        };
        let url = request.url().to_string();
        let (pfad, roh_query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let query = Query::parse(roh_query);
        // Das Token ZUERST: ein Aufrufer ohne Zugang soll den Server nicht
        // dazu bringen können, das Dateisystem anzufassen.
        let antwort = if query.get("t").as_deref() != Some(self.token.as_str()) {
            text(
                403,
                "Zugriff nur mit dem Token aus der Start-URL. Der Trace enthält \
                 unredigierte Datei- und Shell-Inhalte.",
            )
        } else if pfad == "/" || pfad == "/index.html" {
            // Die Seite liest keinen Zustand — auch dafür kein Dateizugriff.
            html(page())
        } else {
            self.refresh(query.get("run").as_deref());
            self.route(pfad, &query)
        };
        let _ = request.respond(antwort);
        true
    }

    /// Neue Zeilen nachlesen. Läuft je Anfrage — deshalb braucht der Betrachter
    /// keinen Hintergrund-Thread und keinen zweiten Codepfad für „live".
    ///
    /// Welche Datei gelesen wird, entscheidet sich in dieser Reihenfolge:
    /// `--trace-file` (fest verdrahtet) > `run=<name>` aus der Anfrage > die
    /// gerade gelesene > die jüngste im Verzeichnis.
    ///
    /// Entscheidend ist der dritte Punkt: eine EINMAL gewählte Datei bleibt
    /// gewählt. Die jüngste je Anfrage neu zu bestimmen sah einfacher aus, war
    /// aber falsch — laufen zwei Agenten im selben Verzeichnis, wechselte der
    /// Betrachter im Sekundentakt zwischen ihren Dateien hin und her und las
    /// jedes Mal alles neu ein. Auf einen anderen Lauf umschalten kann jetzt
    /// nur der Nutzer (`run=`), und `/api/runs` listet die Auswahl.
    fn refresh(&mut self, gewuenscht: Option<&str>) {
        self.liste_auffrischen();
        let ziel = match (&self.cfg.trace_file, gewuenscht) {
            (Some(p), _) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some((p.clone(), name))
            }
            // Ein relativer Pfad UNTERHALB der Wurzel, nie ein absoluter und
            // nie einer mit `..` — sonst wäre `run=` ein Leseloch.
            (None, Some(name)) if ist_sitzungspfad(name) => {
                Some((self.cfg.trace_dir.join(name), name.to_string()))
            }
            (None, _) => match &self.sitzung {
                Some(s) => Some((s.reader.path().to_path_buf(), s.name.clone())),
                None => self
                    .dateien
                    .first()
                    .map(|f| (self.cfg.trace_dir.join(&f.name), f.name.clone())),
            },
        };
        let Some((ziel, name)) = ziel else {
            // Noch gar keine Trace-Datei: kein Zustand, aber auch kein Fehler.
            self.fehler = None;
            return;
        };
        // Bei jeder Anfrage neu geprüft, weil ein laufender Task sein
        // Graph-Verzeichnis erst später anlegen kann.
        self.graph_sitzung = nachbar_verzeichnis(&ziel, "graph");
        // Dasselbe für Work: es wäre irreführend, neben dem Graphen der
        // gewählten Sitzung die Work-Projekte eines ganz anderen Ortes zu
        // zeigen (der Default zeigt auf das Verzeichnis, in dem der Betrachter
        // GESTARTET wurde — bei einem Benchmark-Baum also ins Leere).
        self.work_sitzung = nachbar_verzeichnis(&ziel, "work");
        if !self
            .sitzung
            .as_ref()
            .is_some_and(|s| s.reader.path() == ziel)
        {
            self.sitzung = Some(Sitzung {
                reader: TraceReader::open(ziel),
                state: TraceState::new(),
                name,
            });
        }
        let sitzung = self.sitzung.as_mut().expect("gerade gesetzt");
        match sitzung.reader.read_new() {
            Ok(nachschub) => {
                // Der Leser hat wieder bei null angefangen (Datei gekürzt oder
                // ersetzt) — dann sind seine Ereignisse der GANZE Inhalt, und
                // der alte Zustand muss weg, sonst steht alles doppelt da.
                if nachschub.neu_begonnen {
                    sitzung.state = TraceState::new();
                }
                sitzung.state.extend(nachschub.events);
                self.fehler = None;
            }
            Err(e) => self.fehler = Some(e.to_string()),
        }
    }

    /// Die Sitzungsliste neu erwandern — zwischengespeichert wird nur, was
    /// wirklich wehtut.
    ///
    /// Der Unterschied ist groß genug, um ihn zu machen: ein gewöhnliches
    /// `.agentkit/trace` ist in Mikrosekunden erwandert, ein
    /// Benchmark-Ergebnisbaum mit 2176 Verzeichnissen gemessen in 175–380 ms —
    /// und der Betrachter stellt mehrere Anfragen pro Sekunde. Pauschal zu
    /// cachen hieße, im Normalfall grundlos zu verzögern (eine neu angelegte
    /// Sitzung erschiene erst Sekunden später); gar nicht zu cachen hieße, im
    /// Benchmark-Fall den halben Kern mit Verzeichnis-Durchläufen zu belegen.
    ///
    /// Das NACHLESEN der aktiven Datei hängt nicht daran — das passiert
    /// weiterhin bei jeder Anfrage, „live" bleibt also live.
    fn liste_auffrischen(&mut self) {
        let frisch = self.dateien_stand.is_some_and(|(seit, dauer)| {
            dauer >= TEUER_AB_MS && seit.elapsed().as_millis() < LISTE_GUELTIG_MS
        });
        if frisch {
            return;
        }
        let start = Instant::now();
        self.dateien = list_traces(&self.cfg.trace_dir);
        self.dateien_stand = Some((start, start.elapsed().as_millis()));
    }

    /// Der Wissensgraph, der zur GERADE GEWÄHLTEN Sitzung gehört — falls es
    /// einen gibt.
    ///
    /// Ein einzelnes `--graph DIR` reicht nicht mehr, seit eine Sitzung ein
    /// Benchmark-Task sein kann: dann hat jeder Task seinen eigenen Graphen,
    /// und ein fest verdrahtetes Verzeichnis zeigte für jede Sitzung denselben
    /// falschen. Die Regel folgt der Ablage, die Trace und Graph ohnehin haben
    /// (`<irgendwo>/trace/trace-*.jsonl` neben `<irgendwo>/graph`) — und trifft
    /// damit auch den gewöhnlichen Fall, wo `.agentkit/trace` neben
    /// `.agentkit/graph` liegt. Gibt es das Verzeichnis nicht, bleibt es beim
    /// `--graph` der Kommandozeile.
    /// Wird in [`VizServer::refresh`] bei jeder Anfrage neu bestimmt — der
    /// Graph eines gerade laufenden Tasks entsteht möglicherweise erst, nachdem
    /// die Sitzung schon offen ist.
    fn graph_der_sitzung(&self) -> Option<&Path> {
        self.graph_sitzung.as_deref()
    }

    /// Die Work-Wurzel dieser Sitzung — abgeleitet, sonst die der
    /// Kommandozeile.
    fn work_der_sitzung(&self) -> Option<&Path> {
        self.work_sitzung
            .as_deref()
            .or(self.cfg.work_root.as_deref())
    }

    fn route(&self, pfad: &str, query: &Query) -> Response<Cursor<Vec<u8>>> {
        // Ein Lesefehler macht die Datenendpunkte unbrauchbar — `/api/runs`
        // aber NICHT: das ist der Endpunkt, der erklärt, was los ist, und über
        // den der Nutzer eine andere Datei wählen kann. Ihn mit abzuwürgen
        // hieße, den Betrachter genau dann verstummen zu lassen, wenn er etwas
        // zu sagen hätte.
        if let Some(fehler) = &self.fehler {
            if pfad != "/api/runs" {
                return json_response(500, &json!({ "error": fehler }));
            }
        }
        let leer = TraceState::new();
        let ctx = ApiCtx {
            state: self.sitzung.as_ref().map(|s| &s.state).unwrap_or(&leer),
            trace_dir: &self.cfg.trace_dir,
            trace_file: self.sitzung.as_ref().map(|s| s.reader.path()),
            trace_name: self.sitzung.as_ref().map(|s| s.name.as_str()),
            dateien: &self.dateien,
            skipped: self
                .sitzung
                .as_ref()
                .map(|s| s.reader.skipped())
                .unwrap_or(0),
            fehler: self.fehler.as_deref(),
            work_root: self.work_der_sitzung(),
            // `--graph` hat KEINEN Default (anders als `--work`): ist es
            // gesetzt, hat der Nutzer es ausdrücklich angegeben und meint es
            // auch so. Hier wird deshalb nicht gefiltert.
            graph_dir: self.graph_der_sitzung().or(self.cfg.graph_dir.as_deref()),
        };
        match api::handle(&ctx, pfad, query) {
            Ok(value) => json_response(200, &value),
            Err(e) => json_response(e.status, &json!({ "error": e.message })),
        }
    }
}

/// Die fertige Seite: eine Datei, Stil und Skript inline. Kein zweiter Request,
/// und damit auch kein zweiter Weg, an dem das Token vorbeiginge (ein relatives
/// `<script src>` trüge die Query nicht mit).
fn page() -> String {
    INDEX_HTML
        .replace("/*{{STYLE}}*/", STYLE_CSS)
        .replace("/*{{SCRIPT}}*/", APP_JS)
}

/// Nichts von hier darf zwischengespeichert werden.
///
/// Für die API ist das offensichtlich — ein gecachter Ereignisstrom wäre kein
/// Live-Betrieb. Für die SEITE ist es der Fehler, der lange unbemerkt blieb:
/// Stil und Skript stecken per `include_str!` im Binary, die Adresse bleibt
/// aber `http://127.0.0.1:<port>/`. Ein neu gebauter Betrachter lieferte
/// deshalb eine neue Seite, die der Browser gar nicht erst holte — der Nutzer
/// sah eine alte Oberfläche und hatte keinen Hinweis darauf, dass sie alt ist.
/// Ein Werkzeug, das sich beim Weiterentwickeln selbst versteckt, ist kaputt.
const NO_STORE: (&str, &str) = ("Cache-Control", "no-store, must-revalidate");

fn json_response(status: u16, value: &Value) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|e| format!("{{\"error\":\"nicht serialisierbar: {e}\"}}").into_bytes());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header(NO_STORE.0, NO_STORE.1))
}

fn html(body: String) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(200)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .with_header(header(NO_STORE.0, NO_STORE.1))
}

fn text(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.as_bytes().to_vec())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("feste Header sind gültig")
}

/// Ein Zufalls-Token für die URL.
///
/// Aus `RandomState` statt aus einer Krypto-Crate: dessen Seed kommt vom
/// Betriebssystem (er ist die Absicherung gegen HashDoS in Rusts `HashMap`),
/// und der Zweck hier ist derselbe — ein anderes Programm auf demselben Rechner
/// soll die URL nicht raten können. Für ein localhost-Debug-Werkzeug ist das
/// die richtige Größenordnung; für ein Produkt wäre es keine.
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let wert = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        out.push_str(&format!("{wert:016x}"));
    }
    out
}

/// Öffnet die URL im Standard-Browser. Fehlschläge sind kein Grund abzubrechen —
/// die URL steht ohnehin auf der Konsole.
pub fn open_browser(url: &str) {
    let (programm, args): (&str, Vec<&str>) = if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", ""])
    } else if cfg!(target_os = "macos") {
        ("open", vec![])
    } else {
        ("xdg-open", vec![])
    };
    let ergebnis = std::process::Command::new(programm)
        .args(args)
        .arg(url)
        .spawn();
    if let Err(e) = ergebnis {
        eprintln!("[WARN] Browser konnte nicht geöffnet werden: {e}");
    }
}

/// Ist `name` ein sicherer Sitzungsname, also ein RELATIVER Pfad unterhalb der
/// Trace-Wurzel?
///
/// Seit der Betrachter einen ganzen Ergebnisbaum liest, ist der Name einer
/// Sitzung mehr als ein Dateiname (`polyglot/<job>/<trial>/agent/trace/…`).
/// Die Prüfung bleibt trotzdem positiv formuliert: sie zerlegt an `/` und lässt
/// jedes Segment durch [`ist_dateiname`] laufen. Damit kommt weder ein `..`
/// noch ein Laufwerksbuchstabe noch ein Backslash durch — ein absoluter Pfad
/// scheitert schon am leeren ersten Segment.
pub fn ist_sitzungspfad(name: &str) -> bool {
    !name.is_empty() && name.split('/').all(ist_dateiname)
}

/// Ist `name` ein reiner Dateiname (kein Pfad, kein `..`, kein Laufwerk)?
/// Die Prüfung ist POSITIV formuliert — eine Liste verbotener Zeichen übersieht
/// erfahrungsgemäß immer eines (unter Windows etwa `C:`, das ein `join` den
/// ganzen Pfad ersetzen lässt).
pub fn ist_dateiname(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Der Standard-Ort der Trace-Dateien eines Workspace.
pub fn default_trace_dir(workspace: &str) -> PathBuf {
    Path::new(workspace).join(".agentkit").join("trace")
}

/// Der Standard-Ort der Work-Projekte eines Workspace (`agentkit_work::cli`).
pub fn default_work_root(workspace: &str) -> PathBuf {
    Path::new(workspace).join(".agentkit").join("work")
}

/// Wie weit über der Trace-Datei nach einem Geschwister-Verzeichnis gesucht
/// wird.
///
/// Fünf Ebenen decken die drei Ablagen ab, die es gibt:
///
/// | Ablage | Abstand zur Trace-Datei |
/// |---|---|
/// | ein Graph JE TASK (`<task>/graph`) | 2 |
/// | geteilt je Lauf (`<lauf>/graph`) | 3 |
/// | geteilt über ALLE Läufe (`<ergebniswurzel>/graph`) | 5 |
///
/// Die letzte ist der Normalfall im Benchmark-Betrieb (siehe
/// `agentkit_bench.config.bench_graph_dir`): ein Gedächtnis, das bei jedem
/// Lauf neu anfängt, ist keins. Mehr als fünf wäre Raten — irgendwann findet
/// man den Graphen eines völlig fremden Baums.
const NACHBAR_TIEFE: usize = 5;

/// Das nächstgelegene Verzeichnis namens `name` auf oder über dem
/// Trace-Verzeichnis der Sitzung.
///
/// „Nächstgelegen" ist die inhaltliche Aussage, nicht bloß eine Suchreihenfolge:
/// hat ein Task seinen eigenen Graphen, ist das der, den er auch gesehen hat —
/// der geteilte Graph des ganzen Laufs steht weiter oben und gilt nur, wenn es
/// keinen eigenen gibt.
fn nachbar_verzeichnis(trace_datei: &Path, name: &str) -> Option<PathBuf> {
    let mut ordner = trace_datei.parent()?;
    for _ in 0..NACHBAR_TIEFE {
        ordner = ordner.parent()?;
        let kandidat = ordner.join(name);
        if kandidat.is_dir() {
            return Some(kandidat);
        }
    }
    None
}

/// Passt der Default von `--work` überhaupt zu dem, was betrachtet wird?
///
/// Nur, wenn auch das Trace-Verzeichnis das voreingestellte ist. Sonst zeigt
/// der Betrachter auf einen fremden Baum — etwa Benchmark-Ergebnisse — und der
/// Default lieferte einer Sitzung die Work-Projekte des Verzeichnisses, in dem
/// der BETRACHTER zufällig gestartet wurde. Das sah aus wie Daten dieser
/// Sitzung und war keine; ein leerer Reiter ist die ehrlichere Antwort.
pub fn work_default_passt(trace_dir: &Path, workspace: &str) -> bool {
    trace_dir == default_trace_dir(workspace)
}
