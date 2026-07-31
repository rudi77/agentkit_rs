//! Die JSON-Endpunkte als REINE Funktionen: Pfad + Query rein, `Value` raus.
//!
//! Kein HTTP, kein tiny_http, keine Header — das macht sie im Test ohne Server
//! prüfbar und hält den Adapter (`server.rs`) auf das reduziert, was er ist:
//! Bytes in, Bytes raus.
//!
//! Alle Endpunkte sind LESEND. Der Betrachter beobachtet; er startet, stoppt
//! und ändert nichts (siehe README, „Was bewusst fehlt").

use std::path::Path;

use serde_json::{json, Value};

use crate::project::{with_label, TraceState};
use crate::trace::list_traces;

/// Ein Fehler, der als HTTP-Status beim Aufrufer ankommt.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn not_found(message: impl Into<String>) -> ApiError {
        ApiError {
            status: 404,
            message: message.into(),
        }
    }
}

/// Was die Endpunkte zum Antworten brauchen.
pub struct ApiCtx<'a> {
    pub state: &'a TraceState,
    /// Verzeichnis mit den Trace-Dateien.
    pub trace_dir: &'a Path,
    /// Die Datei, die gerade gelesen wird (`None` = noch keine da).
    pub trace_file: Option<&'a Path>,
    /// Zeilen, die beim Lesen übersprungen wurden (kaputte Zeilen).
    pub skipped: u64,
    /// Letzter Lesefehler, falls einer ansteht — gehört in `/api/runs`, damit
    /// er in der Oberfläche steht statt nur auf stderr.
    pub fehler: Option<&'a str>,
    /// Wurzel der Work-Projekte (`<workspace>/.agentkit/work`), falls gesetzt.
    pub work_root: Option<&'a Path>,
    /// Verzeichnis des Wissensgraphen (`agentkit --graph DIR`), falls gesetzt.
    pub graph_dir: Option<&'a Path>,
}

/// Beantwortet einen API-Pfad. `path` ist der Pfad OHNE Query, `query` die
/// bereits zerlegten Parameter.
pub fn handle(ctx: &ApiCtx, path: &str, query: &Query) -> Result<Value, ApiError> {
    // Leere Segmente werden NICHT herausgefiltert: die ID des Haupt-Agenten IST
    // der leere String (`AgentEvent::source`), sein Verlauf liegt also unter
    // `/api/agents//history`. Ein Filter hier machte ausgerechnet den wichtigsten
    // Agenten unerreichbar.
    let segmente: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .map(percent_decode)
        .collect();
    let refs: Vec<&str> = segmente.iter().map(String::as_str).collect();
    match refs.as_slice() {
        ["api", "runs"] => Ok(runs(ctx)),
        ["api", "agents"] => Ok(json!({ "agents": ctx.state.agents() })),
        ["api", "agents", id, "history"] => Ok(json!({
            "agent": id,
            "events": ctx.state.history(id),
        })),
        ["api", "agents", id, "context"] => Ok(json!(ctx.state.context(id))),
        ["api", "timeline"] => Ok(json!({ "entries": ctx.state.timeline() })),
        ["api", "swarm"] => Ok(json!(ctx.state.swarm())),
        ["api", "events"] => Ok(events(ctx, query)),
        ["api", "graph"] => graph(ctx),
        ["api", "work"] => work_projects(ctx),
        ["api", "work", projekt] => work_project(ctx, projekt),
        _ => Err(ApiError::not_found(format!("kein Endpunkt: /{}", refs.join("/")))),
    }
}

/// Die Trace-Dateien des Verzeichnisses plus die, die gerade gelesen wird.
fn runs(ctx: &ApiCtx) -> Value {
    json!({
        "trace_dir": ctx.trace_dir.display().to_string(),
        "active": ctx.trace_file.map(|p| p.display().to_string()),
        "files": list_traces(ctx.trace_dir),
        "events": ctx.state.events().len(),
        "last_seq": ctx.state.last_seq(),
        "skipped_lines": ctx.skipped,
        "error": ctx.fehler,
    })
}

/// Nachladen: alle Ereignisse mit `seq > since`.
///
/// Bewusst Polling statt Server-Sent Events — Begründung im README
/// („Bewusste Abweichungen"). Der Offset ist derselbe wie beim Tailen der
/// Datei, nur eine Ebene höher.
fn events(ctx: &ApiCtx, query: &Query) -> Value {
    let since = query
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let neu: Vec<Value> = ctx.state.since(since).map(with_label).collect();
    json!({
        "last_seq": ctx.state.last_seq(),
        "events": neu,
    })
}

/// Die Work-Wurzel oder ein sprechender 404 — gemeinsam für beide Work-Endpunkte.
fn work_root<'a>(ctx: &ApiCtx<'a>) -> Result<&'a Path, ApiError> {
    ctx.work_root
        .ok_or_else(|| ApiError::not_found("kein Work-Verzeichnis gesetzt (agentkit viz --work DIR)"))
}

/// Die Work-Projekte unter der Wurzel — ein Verzeichnis je Projekt.
fn work_projects(ctx: &ApiCtx) -> Result<Value, ApiError> {
    let root = work_root(ctx)?;
    let mut projekte: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    projekte.sort();
    Ok(json!({
        "work_dir": root.display().to_string(),
        "projects": projekte,
    }))
}

/// Der komplette Zustand EINES Work-Projekts.
///
/// Über `WorkStore::open_read_only`: sperrfrei und ohne jede Schreibwirkung —
/// ein Betrachter darf die Datei eines lebenden Schreibers nicht anfassen.
#[cfg(feature = "work")]
fn work_project(ctx: &ApiCtx, projekt: &str) -> Result<Value, ApiError> {
    let root = work_root(ctx)?;
    // Nur ein Verzeichnisname, kein Pfad: sonst wäre `/api/work/..%2F..` ein
    // Leseloch in beliebige Verzeichnisse — und unter Windows ersetzte schon
    // ein `C:` beim `join` den ganzen Pfad.
    if !crate::server::ist_dateiname(projekt) {
        return Err(ApiError {
            status: 400,
            message: format!("ungültige Projekt-ID: {projekt}"),
        });
    }
    let state = agentkit_work::WorkStore::open_read_only(root.join(projekt)).map_err(|e| {
        ApiError::not_found(format!("Work-Projekt '{projekt}' nicht lesbar: {e}"))
    })?;
    serde_json::to_value(&state).map_err(|e| ApiError {
        status: 500,
        message: format!("Work-Zustand nicht serialisierbar: {e}"),
    })
}

#[cfg(not(feature = "work"))]
fn work_project(_ctx: &ApiCtx, _projekt: &str) -> Result<Value, ApiError> {
    Err(ApiError {
        status: 501,
        message: "ohne Feature `work` gebaut — kein Zugriff auf die Arbeits-Runtime".to_string(),
    })
}

/// Der vollständige Wissensgraph.
///
/// Über `GraphStore::open_read_only` plus `export`: sperrfrei und ohne jede
/// Schreibwirkung — `GraphStore::open` würde bei Bedarf kompaktieren und damit
/// die Datei eines möglicherweise lebenden Schreibers neu schreiben.
#[cfg(feature = "graph")]
fn graph(ctx: &ApiCtx) -> Result<Value, ApiError> {
    let dir = ctx.graph_dir.ok_or_else(|| {
        ApiError::not_found("kein Graph-Verzeichnis gesetzt (agentkit viz --graph DIR)")
    })?;
    let index = agentkit_graph::GraphStore::open_read_only(dir)
        .map_err(|e| ApiError::not_found(format!("Graph nicht lesbar: {e}")))?;
    serde_json::to_value(agentkit_graph::export(&index)).map_err(|e| ApiError {
        status: 500,
        message: format!("Graph nicht serialisierbar: {e}"),
    })
}

#[cfg(not(feature = "graph"))]
fn graph(_ctx: &ApiCtx) -> Result<Value, ApiError> {
    Err(ApiError {
        status: 501,
        message: "ohne Feature `graph` gebaut — kein Zugriff auf den Wissensgraphen".to_string(),
    })
}

// ------------------------------------------------------------------ Query

/// Die Query einer Anfrage (`a=1&b=zwei`, ohne führendes `?`).
///
/// Bewusst nur ein Wrapper um den Rohtext statt einer zerlegten Map: gelesen
/// werden im ganzen Crate GENAU zwei Namen (`t` und `since`), da lohnt keine
/// Datenstruktur.
#[derive(Debug, Default)]
pub struct Query<'a>(&'a str);

impl<'a> Query<'a> {
    pub fn parse(raw: &'a str) -> Query<'a> {
        Query(raw)
    }

    /// Der (prozent-dekodierte) Wert eines Parameters.
    pub fn get(&self, name: &str) -> Option<String> {
        self.0
            .split('&')
            .filter_map(|paar| paar.split_once('='))
            .find(|(k, _)| percent_decode(k) == name)
            .map(|(_, v)| percent_decode(v))
    }
}

/// Prozent-Dekodierung plus `+` als Leerzeichen. Eigene Handvoll Zeilen statt
/// einer Dependency: gebraucht werden genau diese beiden Regeln, und der
/// Betrachter soll keine Fremdcrates für Kleinigkeiten ziehen.
///
/// Ungültige Sequenzen bleiben stehen, wie sie sind — ein Betrachter, der an
/// einem `%` in einem Agent-Namen scheitert, wäre unbrauchbar.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // `get` statt `&s[..]`: die beiden Stellen hinter dem `%` müssen
            // keine Zeichengrenze sein (`%aä`), ein Slice würde dort PANIKEN —
            // und diese Funktion läuft schon vor der Token-Prüfung.
            b'%' => match s
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
