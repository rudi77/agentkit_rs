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

use serde_json::{json, Value};
use tiny_http::{Header, Response, Server};

use crate::api::{self, ApiCtx, Query};
use crate::project::TraceState;
use crate::trace::{latest_trace, TraceReader};

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
}

struct Sitzung {
    reader: TraceReader,
    state: TraceState,
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
        let ziel = match (&self.cfg.trace_file, gewuenscht) {
            (Some(p), _) => Some(p.clone()),
            // Nur der Dateiname, nie ein Pfad — sonst wäre `run=` ein Leseloch.
            (None, Some(name)) if ist_dateiname(name) => Some(self.cfg.trace_dir.join(name)),
            (None, _) => match &self.sitzung {
                Some(s) => Some(s.reader.path().to_path_buf()),
                None => latest_trace(&self.cfg.trace_dir),
            },
        };
        let Some(ziel) = ziel else {
            // Noch gar keine Trace-Datei: kein Zustand, aber auch kein Fehler.
            self.fehler = None;
            return;
        };
        if !self
            .sitzung
            .as_ref()
            .is_some_and(|s| s.reader.path() == ziel)
        {
            self.sitzung = Some(Sitzung {
                reader: TraceReader::open(ziel),
                state: TraceState::new(),
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
            skipped: self.sitzung.as_ref().map(|s| s.reader.skipped()).unwrap_or(0),
            fehler: self.fehler.as_deref(),
            work_root: self.cfg.work_root.as_deref(),
            graph_dir: self.cfg.graph_dir.as_deref(),
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

fn json_response(status: u16, value: &Value) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|e| {
        format!("{{\"error\":\"nicht serialisierbar: {e}\"}}").into_bytes()
    });
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
}

fn html(body: String) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.into_bytes())
        .with_status_code(200)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
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

