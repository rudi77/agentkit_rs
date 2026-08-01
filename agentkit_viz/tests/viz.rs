//! Tests ohne Netz nach außen: der Trace-Leser und die Projektionen laufen
//! gegen eine Fixture-Datei, der Server gegen sich selbst auf `127.0.0.1`.
//!
//! Der HTTP-Client der Server-Tests ist ein `TcpStream` und zwanzig Zeilen —
//! bewusst keine Client-Dependency für ein Werkzeug, das genau einen Server
//! anspricht, den es selbst gestartet hat.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use agentkit_viz::{TraceReader, TraceState, VizConfig, VizServer};
use serde_json::{json, Value};

// ------------------------------------------------------------------ Helfer

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agentkit_viz_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn zeile(seq: u64, source: &str, etype: &str, data: Value) -> String {
    json!({
        "schema_version": "1",
        "seq": seq,
        "at_ms": 1_700_000_000_000u64 + seq,
        "task_id": -1,
        "source": source,
        "etype": etype,
        "data": data,
    })
    .to_string()
}

/// Ein kleiner Beispiel-Trace: Haupt-Agent mit einem Tool-Aufruf und
/// Kontext-Datensatz, dazu ein Sub-Agent und ein Schwarm-Mitglied.
fn beispiel_trace(pfad: &Path) {
    let zeilen = [
        zeile(1, "", "step", json!({"step": {"step": 1}})),
        zeile(
            2,
            "",
            "tool_call",
            json!({"tool_call": {"name": "read_file", "args": {"path": "a.txt"}}}),
        ),
        zeile(
            3,
            "",
            "tool_result",
            json!({"tool_result": {"name": "read_file", "result": "Inhalt"}}),
        ),
        zeile(
            4,
            "explorer:Wien",
            "tool_call",
            json!({"tool_call": {"name": "grep", "args": {}}}),
        ),
        zeile(
            5,
            "coder",
            "structured",
            json!({"structured": {"kind": "swarm_event", "payload": {"turn_completed": {"agent": "coder"}}}}),
        ),
        zeile(6, "", "final", json!({"final": "Ergebnis: 42"})),
        zeile(7, "", "done", json!("done")),
        zeile(
            8,
            "",
            "structured",
            json!({"structured": {"kind": "context_snapshot", "payload": {
                "messages_from": 0,
                "messages_total": 2,
                "messages": [
                    {"role": "system", "content": "System"},
                    {"role": "user", "content": "Frage"}
                ],
                "report": {"segments": [{"label": "System-Prompt", "tokens": 10, "count": 1, "note": null}],
                           "total": 10, "budget": 8000, "managed": false}
            }}}),
        ),
    ];
    std::fs::write(pfad, format!("{}\n", zeilen.join("\n"))).unwrap();
}

fn gelesen(dir: &Path) -> (TraceState, TraceReader) {
    let pfad = dir.join("trace-1-1.jsonl");
    beispiel_trace(&pfad);
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);
    (state, reader)
}

// ------------------------------------------------------------------ Lesen

/// Der Leser holt beim zweiten Aufruf NUR das, was seither dazugekommen ist —
/// das ist der ganze Trick am Live-Mitschauen (Offset statt Neulesen).
#[test]
fn tailen_liefert_nur_neue_zeilen() {
    let dir = tmp("tail");
    let pfad = dir.join("t.jsonl");
    std::fs::write(
        &pfad,
        format!("{}\n", zeile(1, "", "step", json!({"step": {"step": 1}}))),
    )
    .unwrap();

    let mut reader = TraceReader::open(&pfad);
    assert_eq!(reader.read_new().unwrap().events.len(), 1);
    assert!(
        reader.read_new().unwrap().events.is_empty(),
        "nichts Neues, nichts geliefert"
    );

    let mut datei = std::fs::OpenOptions::new()
        .append(true)
        .open(&pfad)
        .unwrap();
    writeln!(datei, "{}", zeile(2, "", "done", json!("done"))).unwrap();
    let neu = reader.read_new().unwrap();
    assert_eq!(neu.events.len(), 1);
    assert_eq!(neu.events[0].seq, 2);
    assert!(!neu.neu_begonnen, "die Datei ist nur gewachsen");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Schreiber kann mitten in einer Zeile stecken. Die halbe Zeile bleibt
/// liegen, bis sie vollständig ist — sie darf weder Fehler noch Datenverlust
/// erzeugen.
#[test]
fn abgeschnittene_letzte_zeile_wird_toleriert() {
    let dir = tmp("halb");
    let pfad = dir.join("t.jsonl");
    let ganz = zeile(1, "", "step", json!({"step": {"step": 1}}));
    let halb = zeile(2, "", "final", json!({"final": "unvollstän"}));
    std::fs::write(&pfad, format!("{ganz}\n{}", &halb[..halb.len() - 20])).unwrap();

    let mut reader = TraceReader::open(&pfad);
    let erst = reader.read_new().unwrap();
    assert_eq!(erst.events.len(), 1, "nur die vollständige Zeile");

    // Der Schreiber macht die Zeile fertig.
    std::fs::write(&pfad, format!("{ganz}\n{halb}\n")).unwrap();
    let dann = reader.read_new().unwrap();
    assert_eq!(dann.events.len(), 1);
    assert_eq!(
        dann.events[0].seq, 2,
        "die nachgereichte Zeile kommt vollständig an"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein neuerer agentkit darf Ereignistypen ergänzen, ohne den Betrachter
/// unbrauchbar zu machen — die Rohform bleibt erhalten.
#[test]
fn unbekannte_nutzlast_wird_nicht_verworfen() {
    let dir = tmp("unbekannt");
    let pfad = dir.join("t.jsonl");
    std::fs::write(
        &pfad,
        format!(
            "{}\n",
            zeile(1, "", "telepathie", json!({"telepathie": {"x": 1}}))
        ),
    )
    .unwrap();

    let mut reader = TraceReader::open(&pfad);
    let events = reader.read_new().unwrap().events;
    assert_eq!(events.len(), 1);
    let json = serde_json::to_value(&events[0]).unwrap();
    assert!(
        json["data"]["unbekannt"]["telepathie"]["x"] == 1,
        "Rohform fehlt: {json}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- Projektionen

/// Die Agenten-Liste entsteht allein aus den `source`-Tags — Haupt-Agent
/// zuerst, dann Sub-Agenten, dann Schwarm-Mitglieder.
#[test]
fn agenten_werden_aus_den_source_tags_gruppiert() {
    let dir = tmp("agenten");
    let (state, _) = gelesen(&dir);

    let agenten = state.agents();
    let ids: Vec<&str> = agenten.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["", "explorer:Wien", "coder"]);

    let haupt = &agenten[0];
    assert_eq!(haupt.label, "(Haupt-Agent)");
    assert_eq!(haupt.steps, 1);
    assert_eq!(haupt.tool_calls, 1);
    assert_eq!(haupt.status, "fertig", "das done-Ereignis schließt ihn ab");
    assert_eq!(agenten[1].status, "läuft", "der Sub-Agent hat kein done");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Verlauf eines Agenten enthält genau dessen Ereignisse.
#[test]
fn verlauf_enthaelt_nur_die_ereignisse_eines_agenten() {
    let dir = tmp("verlauf");
    let (state, _) = gelesen(&dir);

    let verlauf = state.history("explorer:Wien");
    assert_eq!(verlauf.len(), 1);
    assert_eq!(verlauf[0]["seq"], 4);
    // Das Label kommt vom Server, damit es der Browser nicht ein zweites Mal
    // formulieren muss (dieselben Worte wie in der Zeitleiste).
    assert_eq!(verlauf[0]["label"], "grep({})");
    assert_eq!(state.history("").len(), 6);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Kontext des Haupt-Agenten kommt aus den `context_snapshot`-Datensätzen;
/// für alle anderen wird er REKONSTRUIERT und auch so ausgewiesen.
#[test]
fn kontext_kommt_aus_den_snapshots_und_sonst_rekonstruiert() {
    let dir = tmp("kontext");
    let (state, _) = gelesen(&dir);

    let haupt = state.context("");
    assert!(!haupt.rekonstruiert);
    assert_eq!(haupt.messages.len(), 2);
    assert_eq!(haupt.messages[0]["role"], "system");
    assert_eq!(haupt.report.as_ref().unwrap()["total"], 10);

    let sub = state.context("explorer:Wien");
    assert!(
        sub.rekonstruiert,
        "ohne Snapshot bleibt nur die Rekonstruktion"
    );
    assert!(sub.report.is_none());
    assert_eq!(sub.messages.len(), 1, "der eine Tool-Aufruf");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Mehrere Schnappschüsse werden zusammengesetzt: `messages_from` sagt, ab
/// welchem Index die Nachrichten gelten — `0` ersetzt den ganzen Stand.
#[test]
fn kontext_setzt_mehrere_snapshots_zusammen() {
    let dir = tmp("kontext2");
    let pfad = dir.join("t.jsonl");
    let schnappschuss = |seq: u64, from: usize, msgs: Value| {
        zeile(
            seq,
            "",
            "structured",
            json!({"structured": {"kind": "context_snapshot", "payload": {
                "messages_from": from, "messages_total": 0, "messages": msgs, "report": null
            }}}),
        )
    };
    std::fs::write(
        &pfad,
        format!(
            "{}\n{}\n{}\n",
            schnappschuss(1, 0, json!([{"role": "system"}, {"role": "user"}])),
            schnappschuss(2, 2, json!([{"role": "assistant"}])),
            schnappschuss(3, 0, json!([{"role": "system"}])),
        ),
    )
    .unwrap();

    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);
    let k = state.context("");
    assert_eq!(
        k.messages.len(),
        1,
        "der letzte Datensatz mit from=0 ersetzt alles"
    );
    assert_eq!(k.messages[0]["role"], "system");

    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ Server

/// Eine minimale GET-Anfrage; gibt (Status, Rumpf) zurück.
fn get(port: u16, pfad: &str) -> (u16, String) {
    let mut strom = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        strom,
        "GET {pfad} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut antwort = String::new();
    strom.read_to_string(&mut antwort).unwrap();
    let status = antwort
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rumpf = antwort.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, rumpf.to_string())
}

/// Wie [`get`], liefert aber den KOPF statt des Rumpfs — für Zusicherungen
/// über Header.
fn get_mit_kopf(port: u16, pfad: &str) -> (u16, String) {
    let mut strom = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        strom,
        "GET {pfad} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut antwort = String::new();
    strom.read_to_string(&mut antwort).unwrap();
    let status = antwort
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let kopf = antwort.split_once("\r\n\r\n").map(|(k, _)| k).unwrap_or("");
    (status, kopf.to_string())
}

/// Startet einen Server auf einem freien Port, der in einem eigenen Thread
/// bedient, bis der Testprozess endet. Bewusst OHNE Anfragezähler: eine feste
/// Anzahl müsste bei jeder neuen Zusicherung mitgezählt werden, und eine zu
/// kleine Zahl ließe den Test HÄNGEN statt zu scheitern.
fn server(dir: &Path) -> (u16, String) {
    let cfg = VizConfig {
        trace_dir: dir.to_path_buf(),
        trace_file: None,
        work_root: Some(dir.join("work")),
        graph_dir: Some(dir.join("graph")),
        port: 0,
    };
    let mut server = VizServer::bind(cfg).unwrap();
    assert!(
        server.is_loopback(),
        "der Server darf nur auf Loopback binden"
    );
    let port = server.port();
    let url = server.url();
    std::thread::spawn(move || while server.handle_one() {});
    (port, url)
}

/// Das Token aus der Start-URL.
fn token(url: &str) -> String {
    url.split_once("t=").unwrap().1.to_string()
}

/// Ohne Token gibt es nichts zu sehen — der Trace enthält unredigierte
/// Datei- und Shell-Inhalte.
#[test]
fn ohne_token_wird_abgewiesen() {
    let dir = tmp("token");
    beispiel_trace(&dir.join("trace-1-1.jsonl"));
    let (port, url) = server(&dir);

    assert_eq!(get(port, "/api/agents").0, 403, "ohne Token");
    assert_eq!(
        get(port, "/api/agents?t=falsch").0,
        403,
        "mit falschem Token"
    );
    assert_eq!(
        get(port, &format!("/api/agents?t={}", token(&url))).0,
        200,
        "mit dem richtigen Token"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Jeder Endpunkt liefert gültiges JSON — und die Inhalte stimmen mit den
/// Projektionen überein.
#[test]
fn jeder_endpunkt_liefert_gueltiges_json() {
    let dir = tmp("endpunkte");
    beispiel_trace(&dir.join("trace-1-1.jsonl"));
    let (port, url) = server(&dir);
    let t = token(&url);
    let hole = |pfad: &str| -> Value {
        let (status, rumpf) = get(port, &format!("{pfad}?t={t}"));
        assert_eq!(status, 200, "{pfad}: {rumpf}");
        serde_json::from_str(&rumpf).unwrap_or_else(|e| panic!("{pfad}: kein JSON ({e}): {rumpf}"))
    };

    let runs = hole("/api/runs");
    assert_eq!(runs["events"], 8);
    assert_eq!(runs["files"].as_array().unwrap().len(), 1);
    assert_eq!(runs["last_seq"], 8);

    assert_eq!(hole("/api/agents")["agents"].as_array().unwrap().len(), 3);
    assert_eq!(
        hole("/api/agents//history")["events"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(hole("/api/agents//context")["rekonstruiert"], false);
    assert_eq!(
        hole("/api/timeline")["entries"].as_array().unwrap().len(),
        8
    );
    assert_eq!(hole("/api/events")["events"].as_array().unwrap().len(), 8);

    // Unbekannter Pfad: 404 mit JSON-Fehler statt HTML oder leerem Rumpf.
    let (status, rumpf) = get(port, &format!("/api/gibtsnicht?t={t}"));
    assert_eq!(status, 404);
    assert!(serde_json::from_str::<Value>(&rumpf).unwrap()["error"].is_string());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `since` liefert wirklich nur das Neue — die Naht, auf der die Live-Ansicht
/// steht.
#[test]
fn events_since_liefert_nur_neues() {
    let dir = tmp("since");
    beispiel_trace(&dir.join("trace-1-1.jsonl"));
    let (port, url) = server(&dir);
    let (status, rumpf) = get(port, &format!("/api/events?since=6&t={}", token(&url)));
    assert_eq!(status, 200);
    let daten: Value = serde_json::from_str(&rumpf).unwrap();
    let seqs: Vec<u64> = daten["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![7, 8]);
    assert_eq!(daten["last_seq"], 8);
    // Der Nachschub hat DIESELBE Form wie der Verlauf (inklusive `label`) — der
    // Browser hängt ihn direkt an, statt die Ansicht neu zu bauen.
    assert_eq!(daten["events"][0]["label"], "fertig");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Die Startseite ist EIN Dokument — Stil und Skript stecken darin, sonst
/// trügen die Folge-Requests das Token nicht mit.
#[test]
fn startseite_ist_ein_dokument_mit_stil_und_skript() {
    let dir = tmp("seite");
    let (port, url) = server(&dir);
    let (status, rumpf) = get(port, &format!("/?t={}", token(&url)));
    assert_eq!(status, 200);
    assert!(rumpf.contains("<title>agentkit viz</title>"));
    assert!(rumpf.contains("--akzent"), "CSS fehlt");
    assert!(rumpf.contains("const TOKEN"), "JS fehlt");
    assert!(!rumpf.contains("{{STYLE}}"), "Platzhalter nicht ersetzt");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Betrachter darf VOR dem ersten Lauf starten: kein Trace-Verzeichnis,
/// keine Datei — trotzdem gültige, leere Antworten.
#[test]
fn ohne_trace_datei_antwortet_der_server_leer() {
    let dir = tmp("leer");
    let (port, url) = server(&dir.join("gibtsnicht"));
    let t = token(&url);

    let (status, rumpf) = get(port, &format!("/api/runs?t={t}"));
    assert_eq!(status, 200);
    let runs: Value = serde_json::from_str(&rumpf).unwrap();
    assert_eq!(runs["events"], 0);
    assert!(runs["active"].is_null());

    let (status, rumpf) = get(port, &format!("/api/agents?t={t}"));
    assert_eq!(status, 200);
    assert!(serde_json::from_str::<Value>(&rumpf).unwrap()["agents"]
        .as_array()
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein Projektname ist ein Verzeichnisname, kein Pfad — sonst wäre
/// `/api/work/..%2F..` ein Leseloch in beliebige Verzeichnisse.
#[test]
#[cfg(feature = "work")]
fn work_weist_pfad_ausbrueche_ab() {
    let dir = tmp("workpfad");
    std::fs::create_dir_all(dir.join("work")).unwrap();
    let (port, url) = server(&dir);
    let t = token(&url);

    assert_eq!(get(port, &format!("/api/work/..?t={t}")).0, 400);
    assert_eq!(get(port, &format!("/api/work/..%2Fetc?t={t}")).0, 400);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Wird die Datei KÜRZER (gekürzt oder ersetzt), fängt der Leser von vorn an
/// und sagt es — sonst hängte der Aufrufer den kompletten Inhalt ein zweites
/// Mal an seinen alten Zustand.
#[test]
fn gekuerzte_datei_meldet_den_neuanfang() {
    let dir = tmp("neuanfang");
    let pfad = dir.join("t.jsonl");
    let a = zeile(1, "", "step", json!({"step": {"step": 1}}));
    let b = zeile(2, "", "done", json!("done"));
    std::fs::write(&pfad, format!("{a}\n{b}\n")).unwrap();

    let mut reader = TraceReader::open(&pfad);
    let erst = reader.read_new().unwrap();
    assert_eq!(erst.events.len(), 2);
    assert!(!erst.neu_begonnen);

    // Ein neuer, kürzerer Lauf unter demselben Namen.
    std::fs::write(&pfad, format!("{a}\n")).unwrap();
    let dann = reader.read_new().unwrap();
    assert!(dann.neu_begonnen, "der Neuanfang muss gemeldet werden");
    assert_eq!(
        dann.events.len(),
        1,
        "und der ganze Inhalt kommen, nicht ein Rest"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Nach einem Neuanfang darf der Server nichts doppelt zeigen.
#[test]
fn server_wirft_den_zustand_bei_einem_neuanfang_weg() {
    let dir = tmp("neuanfang_server");
    let pfad = dir.join("trace-1-1.jsonl");
    beispiel_trace(&pfad);
    let (port, url) = server(&dir);
    let t = token(&url);

    let vorher: Value = serde_json::from_str(&get(port, &format!("/api/runs?t={t}")).1).unwrap();
    assert_eq!(vorher["events"], 8);

    // Dieselbe Datei, aber kürzer — wie ein neuer Lauf unter gleichem Namen.
    std::fs::write(
        &pfad,
        format!("{}\n", zeile(1, "", "step", json!({"step": {"step": 1}}))),
    )
    .unwrap();
    let nachher: Value = serde_json::from_str(&get(port, &format!("/api/runs?t={t}")).1).unwrap();
    assert_eq!(
        nachher["events"], 1,
        "nicht 9: der alte Zustand muss weg sein"
    );
    assert_eq!(nachher["last_seq"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Laufen zwei Agenten im selben Verzeichnis, darf der Server die Datei NICHT
/// von sich aus wechseln — sonst sähe der Nutzer im Sekundentakt abwechselnd
/// den einen und den anderen Lauf. Gewechselt wird nur über `run=`.
#[test]
fn der_server_wechselt_die_datei_nicht_von_selbst() {
    let dir = tmp("zweilaeufe");
    let alt = dir.join("trace-1-1.jsonl");
    beispiel_trace(&alt);
    let (port, url) = server(&dir);
    let t = token(&url);

    let erst: Value = serde_json::from_str(&get(port, &format!("/api/runs?t={t}")).1).unwrap();
    assert_eq!(erst["events"], 8);

    // Ein zweiter, JÜNGERER Lauf im selben Verzeichnis.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        dir.join("trace-2-2.jsonl"),
        format!("{}\n", zeile(1, "", "step", json!({"step": {"step": 1}}))),
    )
    .unwrap();

    let bleibt: Value = serde_json::from_str(&get(port, &format!("/api/runs?t={t}")).1).unwrap();
    assert_eq!(bleibt["events"], 8, "die gewählte Datei bleibt gewählt");
    assert_eq!(
        bleibt["files"].as_array().unwrap().len(),
        2,
        "beide stehen zur Auswahl"
    );

    // Erst der ausdrückliche Wunsch schaltet um.
    let gewaehlt: Value =
        serde_json::from_str(&get(port, &format!("/api/runs?run=trace-2-2.jsonl&t={t}")).1)
            .unwrap();
    assert_eq!(gewaehlt["events"], 1);

    // Ein Pfad statt eines Dateinamens wird ignoriert (kein Leseloch).
    let (status, rumpf) = get(port, &format!("/api/runs?run=..%2Fgeheim.jsonl&t={t}"));
    assert_eq!(status, 200);
    let unveraendert: Value = serde_json::from_str(&rumpf).unwrap();
    assert_eq!(
        unveraendert["events"], 1,
        "der Ausbruchsversuch ändert nichts"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein unbekanntes Zeilenformat ist ein harter Fehler — aber `/api/runs` muss
/// trotzdem antworten: es ist der Endpunkt, der erklärt, was los ist, und über
/// den der Nutzer eine andere Datei wählen kann.
#[test]
fn ein_lesefehler_wuergt_api_runs_nicht_ab() {
    let dir = tmp("lesefehler");
    std::fs::write(
        dir.join("trace-1-1.jsonl"),
        "{\"schema_version\":\"99\",\"seq\":1,\"at_ms\":1,\"etype\":\"step\",\"data\":\"done\"}\n",
    )
    .unwrap();
    let (port, url) = server(&dir);
    let t = token(&url);

    let (status, rumpf) = get(port, &format!("/api/runs?t={t}"));
    assert_eq!(status, 200, "runs muss antworten: {rumpf}");
    let runs: Value = serde_json::from_str(&rumpf).unwrap();
    assert!(
        runs["error"].as_str().unwrap_or("").contains("99"),
        "der Grund gehört in die Antwort: {runs}"
    );

    // Die Datenendpunkte schweigen dagegen — halbe Daten wären schlimmer.
    assert_eq!(get(port, &format!("/api/agents?t={t}")).0, 500);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `percent_decode` läuft VOR der Token-Prüfung über jeden Query-Namen — eine
/// Panik dort risse den einfädigen Server mit. Nicht-ASCII hinter einem `%`
/// darf deshalb nicht in einen Slice auf Byte-Indizes laufen.
#[test]
fn percent_decode_paniert_nicht_an_zeichengrenzen() {
    use agentkit_viz::api::percent_decode;
    assert_eq!(percent_decode("%aä"), "%aä");
    assert_eq!(percent_decode("%€"), "%€");
    assert_eq!(percent_decode("%"), "%");
    assert_eq!(percent_decode("%2F"), "/");
    assert_eq!(percent_decode("a+b"), "a b");
}

/// Ein Projektname ist ein Verzeichnisname — auch ein blankes Laufwerk (`C:`)
/// ist keiner: unter Windows ersetzt es beim `join` den ganzen Pfad.
#[test]
#[cfg(feature = "work")]
fn work_weist_ein_laufwerk_als_projektnamen_ab() {
    let dir = tmp("workdrive");
    std::fs::create_dir_all(dir.join("work")).unwrap();
    let (port, url) = server(&dir);
    let t = token(&url);
    assert_eq!(get(port, &format!("/api/work/C:?t={t}")).0, 400);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Die Namen, die `agentkit::trace` vergibt, müssen als `run=`-Parameter
/// durchkommen — sonst wäre die Lauf-Auswahl im Browser wirkungslos, ohne dass
/// es jemand merkt. Und alles, was ein Pfad sein könnte, darf es nicht.
#[test]
fn trace_dateinamen_sind_gueltige_run_parameter() {
    use agentkit_viz::server::ist_dateiname;
    assert!(ist_dateiname("trace-23708-1785485762147.jsonl"));
    assert!(!ist_dateiname("a/b.jsonl"));
    assert!(!ist_dateiname(r"a\b.jsonl"));
    assert!(!ist_dateiname(".."));
    assert!(!ist_dateiname("C:"));
    assert!(!ist_dateiname(""));
}

/// Ein Graph liegt in zwei verschiedenen Ablagen: neben dem Task
/// (`<task>/graph`, ein Graph je Task) oder eine Ebene höher beim ganzen Lauf
/// (`<lauf>/graph`, der geteilte). Der Betrachter muss beide finden — und bei
/// beidem den NÄHEREN nehmen, denn das ist der, den der Task gesehen hat.
#[test]
fn der_graph_der_sitzung_wird_auch_eine_ebene_hoeher_gefunden() {
    let dir = tmp("graphablage");
    let geteilt = dir.join("lauf/graph");
    let task_a = dir.join("lauf/task-a/trace");
    let task_b = dir.join("lauf/task-b/trace");
    let eigener = dir.join("lauf/task-b/graph");
    for d in [&geteilt, &task_a, &task_b, &eigener] {
        std::fs::create_dir_all(d).unwrap();
    }
    beispiel_trace(&task_a.join("trace-1-1.jsonl"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    beispiel_trace(&task_b.join("trace-2-2.jsonl"));

    let (port, url) = server(&dir);
    let t = token(&url);
    // task-b hat einen eigenen Graphen — der gewinnt.
    let b: Value = serde_json::from_str(
        &get(
            port,
            &format!("/api/runs?run=lauf%2Ftask-b%2Ftrace%2Ftrace-2-2.jsonl&t={t}"),
        )
        .1,
    )
    .unwrap();
    assert_eq!(b["events"], 8);
    let (status_b, _) = get(
        port,
        &format!("/api/graph?run=lauf%2Ftask-b%2Ftrace%2Ftrace-2-2.jsonl&t={t}"),
    );
    assert_ne!(status_b, 404, "task-b hat einen eigenen Graphen");

    // task-a hat keinen — dann gilt der geteilte des Laufs, eine Ebene höher.
    let (status_a, rumpf_a) = get(
        port,
        &format!("/api/graph?run=lauf%2Ftask-a%2Ftrace%2Ftrace-1-1.jsonl&t={t}"),
    );
    assert_ne!(
        status_a, 404,
        "der geteilte Graph des Laufs muss gefunden werden: {rumpf_a}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Der Default von `--work` hängt am Startverzeichnis des BETRACHTERS. Zeigt
/// `--trace` auf einen fremden Baum (Benchmark-Ergebnisse), darf er NICHT
/// greifen — sonst bekommt eine Benchmark-Sitzung die Work-Projekte des
/// Verzeichnisses untergeschoben, in dem der Betrachter zufällig gestartet
/// wurde. Das sah aus wie Daten dieser Sitzung und war keine.
#[test]
fn der_work_default_greift_nur_beim_voreingestellten_trace() {
    use agentkit_viz::server::{default_trace_dir, work_default_passt};
    assert!(
        work_default_passt(&default_trace_dir("."), "."),
        "der gewöhnliche Fall: beide voreingestellt"
    );
    assert!(
        !work_default_passt(Path::new("D:/bench/results"), "."),
        "fremder Ergebnisbaum: kein Work-Default"
    );
}

/// Ein Sitzungsname darf ein relativer Pfad sein — aber ausschließlich einer,
/// der unterhalb der Wurzel bleibt. Er kommt aus der Adresszeile.
#[test]
fn sitzungspfade_lassen_keinen_ausbruch_zu() {
    use agentkit_viz::server::ist_sitzungspfad;
    assert!(ist_sitzungspfad("trace-23708-1785485762147.jsonl"));
    assert!(ist_sitzungspfad(
        "polyglot/poly-1/agent/trace/trace-1-1.jsonl"
    ));
    assert!(!ist_sitzungspfad("../geheim.jsonl"));
    assert!(!ist_sitzungspfad("a/../../geheim.jsonl"));
    assert!(
        !ist_sitzungspfad("/etc/passwd"),
        "absolut: leeres erstes Segment"
    );
    assert!(!ist_sitzungspfad("a//b.jsonl"), "leeres Segment");
    assert!(!ist_sitzungspfad(r"a\b.jsonl"));
    assert!(!ist_sitzungspfad("C:/geheim.jsonl"));
    assert!(!ist_sitzungspfad(""));
}

/// Ein Benchmark schreibt den Trace dorthin, wo die übrigen Artefakte des Tasks
/// liegen: ein Verzeichnis je Task, tief im Ergebnisbaum. Findet der Betrachter
/// die nicht, ist er im Benchmark-Betrieb blind — genau dann, wenn man ihn
/// braucht. Der relative Pfad ist dabei Name UND Kennung.
#[test]
fn sitzungen_werden_rekursiv_gefunden_und_ueber_ihren_pfad_gewaehlt() {
    let dir = tmp("ergebnisbaum");
    let a = dir.join("polyglot/poly-1/polyglot_python_beer-song__ab12/agent/trace");
    let b = dir.join("swebench/swe-1/django__django-11099/trace");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    beispiel_trace(&a.join("trace-1-1.jsonl"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        b.join("trace-2-2.jsonl"),
        format!("{}\n", zeile(1, "", "step", json!({"step": {"step": 1}}))),
    )
    .unwrap();

    // Im selben Baum liegen fremde `.jsonl` — die SWE-bench-Predictions. Sie
    // als Sitzung anzubieten hieße, eine Ansicht mit null Ereignissen
    // unterzuschieben; die Endung allein taugt deshalb nicht als Filter.
    std::fs::write(
        dir.join("swebench/swe-1/preds.jsonl"),
        "{\"instance_id\":\"x\"}\n",
    )
    .unwrap();

    let (port, url) = server(&dir);
    let t = token(&url);
    let runs: Value = serde_json::from_str(&get(port, &format!("/api/runs?t={t}")).1).unwrap();
    let mut namen: Vec<&str> = runs["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    namen.sort();
    assert_eq!(
        namen,
        vec![
            "polyglot/poly-1/polyglot_python_beer-song__ab12/agent/trace/trace-1-1.jsonl",
            "swebench/swe-1/django__django-11099/trace/trace-2-2.jsonl",
        ],
        "beide Sitzungen stehen zur Auswahl — mit `/` getrennt, auch unter Windows"
    );
    // Die jüngste ist gewählt, und ihr Name ist der relative Pfad.
    assert_eq!(
        runs["active_name"],
        "swebench/swe-1/django__django-11099/trace/trace-2-2.jsonl"
    );
    assert_eq!(runs["events"], 1);

    // Derselbe Name schaltet als `run=` auf die andere Sitzung um.
    let gewaehlt: Value = serde_json::from_str(
        &get(
            port,
            &format!("/api/runs?run=polyglot%2Fpoly-1%2Fpolyglot_python_beer-song__ab12%2Fagent%2Ftrace%2Ftrace-1-1.jsonl&t={t}"),
        )
        .1,
    )
    .unwrap();
    assert_eq!(gewaehlt["events"], 8, "die gewählte Sitzung wird gelesen");

    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------------------------- Schwarm

/// Ein kleiner Schwarm-Lauf im Trace: eine Zustellung, eine abgelehnte, ein
/// Vorschlag, eine Zustimmung, eine Ablehnung — und das Ergebnis.
fn schwarm_trace(pfad: &Path) {
    let ereignis = |seq: u64, source: &str, payload: Value| {
        zeile(
            seq,
            source,
            "structured",
            json!({"structured": {"kind": "swarm_event", "payload": payload}}),
        )
    };
    let nachricht = |id: &str, from: &str, to: Value, kind: &str, inhalt: &str, corr: Value| {
        json!({
            "id": id, "swarm_id": "s1", "from": from, "to": to, "kind": kind,
            "content": inhalt, "reply_to": null, "correlation_id": corr,
            "created_at": 1_700_000_000_000u64, "hop_count": 0
        })
    };
    let zeilen = [
        ereignis(1, "a", json!({"actor_started": {"agent": "a"}})),
        ereignis(2, "b", json!({"actor_started": {"agent": "b"}})),
        ereignis(
            3,
            "a",
            json!({"message_queued": {"message":
                nachricht("msg-1", "a", json!({"agent": "b"}), "request", "Wie steht es?", Value::Null)}}),
        ),
        ereignis(
            4,
            "b",
            json!({"message_rejected": {
                "message": nachricht("msg-2", "b", json!({"agent": "a"}), "reply", "voll", Value::Null),
                "result": "mailbox_full"}}),
        ),
        ereignis(
            5,
            "a",
            json!({"proposal_created": {"message":
                nachricht("msg-3", "a", json!("broadcast"), "proposal", "Befund X", Value::Null)}}),
        ),
        ereignis(
            6,
            "b",
            json!({"vote_submitted": {"message": nachricht(
                "msg-4", "b", json!("broadcast"), "vote",
                "{\"zustimmung\":true,\"kommentar\":null}", json!("msg-3"))}}),
        ),
        ereignis(
            7,
            "c",
            json!({"vote_submitted": {"message": nachricht(
                "msg-5", "c", json!("broadcast"), "vote",
                "{\"zustimmung\":false,\"kommentar\":\"zu früh\"}", json!("msg-3"))}}),
        ),
        zeile(
            8,
            "",
            "structured",
            json!({"structured": {"kind": "swarm_result", "payload": {
                "reason": {"consensus": {"proposal": nachricht("msg-3", "a", json!("broadcast"), "proposal", "Befund X", Value::Null), "approvals": 1}},
                "messages_sent": 3,
                "dead_letters": [],
                "turns": {"a": 2, "b": 1},
                "proposals": [{"id": "msg-3", "from": "a", "approvals": ["b"], "accepted": true}],
                "required_approvals": 1
            }}}),
        ),
    ];
    std::fs::write(pfad, format!("{}\n", zeilen.join("\n"))).unwrap();
}

/// Die Schwarm-Sicht entsteht allein aus den `swarm_event`-Datensätzen — genau
/// das, was vor Phase 1 in der platten Textzeile verloren ging.
#[test]
fn schwarm_sicht_kennt_verkehr_abstimmung_und_dead_letters() {
    let dir = tmp("schwarm");
    let pfad = dir.join("trace-1-1.jsonl");
    schwarm_trace(&pfad);
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);

    let s = state.swarm();
    // Spalten: auch ein Mitglied, das nur abgestimmt hat, gehört dazu.
    assert_eq!(s.members, vec!["a", "b", "c"]);

    // Der Verkehr, mit Absender, Empfänger und Art.
    assert_eq!(s.messages.len(), 2);
    let erste = &s.messages[0];
    assert_eq!(
        (erste.from.as_str(), erste.to.as_str(), erste.kind.as_str()),
        ("a", "b", "request")
    );
    assert!(erste.delivered);
    let zweite = &s.messages[1];
    assert!(!zweite.delivered);
    assert_eq!(zweite.reason.as_deref(), Some("mailbox_full"));

    // Die Abstimmung — inklusive der ABLEHNUNG, die in keinem Ergebnis steht.
    assert_eq!(s.proposals.len(), 1);
    let p = &s.proposals[0];
    assert_eq!(p.id, "msg-3");
    assert_eq!(p.from, "a");
    assert_eq!(p.approvals, vec!["b"]);
    assert_eq!(
        p.rejections,
        vec!["c"],
        "wer abgelehnt hat, ist der interessante Teil"
    );
    assert!(p.accepted, "das Ergebnis trägt die Annahme nach");

    // Dead Letters: die abgelehnte Zustellung.
    assert_eq!(s.dead_letters.len(), 1);
    assert_eq!(s.dead_letters[0].reason, "mailbox_full");

    assert_eq!(s.results.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ohne Schwarm im Trace ist die Sicht leer statt kaputt.
#[test]
fn ohne_schwarm_ist_die_sicht_leer() {
    let dir = tmp("kein_schwarm");
    let (state, _) = gelesen(&dir);
    let s = state.swarm();
    // Der Beispiel-Trace enthält ein `swarm_event` eines Mitglieds ohne
    // Nachrichten — Spalten ja, Verkehr nein.
    assert!(s.messages.is_empty());
    assert!(s.proposals.is_empty());
    assert!(s.results.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `swarm_completed` als Ablehnungsgrund ist KEIN Verlust: ein Mitglied, das
/// seinen Turn zu Ende bringt, während der Konsens schon steht, hat nichts
/// falsch gemacht. Als „unzustellbar" gezeigt, sähe ein sauberer Abschluss wie
/// eine Panne aus.
#[test]
fn abschluss_bedingte_ablehnung_ist_kein_dead_letter() {
    let dir = tmp("schwarm_abschluss");
    let pfad = dir.join("trace-1-1.jsonl");
    let nachricht = json!({
        "id": "msg-9", "swarm_id": "s1", "from": "b", "to": {"agent": "a"},
        "kind": "reply", "content": "zu spät", "reply_to": null,
        "correlation_id": null, "created_at": 1, "hop_count": 0
    });
    std::fs::write(
        &pfad,
        format!(
            "{}\n",
            zeile(
                1,
                "b",
                "structured",
                json!({"structured": {"kind": "swarm_event", "payload": {
                    "message_rejected": {"message": nachricht, "result": "swarm_completed"}}}}),
            )
        ),
    )
    .unwrap();
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);

    let s = state.swarm();
    assert!(
        s.dead_letters.is_empty(),
        "kein Verlust: {:?}",
        s.dead_letters
    );
    assert_eq!(s.messages.len(), 1, "echter Verkehr bleibt im Diagramm");
    assert!(!s.messages[0].delivered);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ruft ein Orchestrator das `swarm`-Tool zweimal auf, stehen ZWEI Ergebnisse
/// im Trace. Nur das letzte anzuwenden hieße, die Abstimmung des ersten Laufs
/// für immer als „offen" anzuzeigen.
#[test]
fn mehrere_schwarm_laeufe_bekommen_jeder_sein_ergebnis() {
    let dir = tmp("schwarm_zwei");
    let pfad = dir.join("trace-1-1.jsonl");
    let ergebnis = |seq: u64, pid: &str| {
        zeile(
            seq,
            "",
            "structured",
            json!({"structured": {"kind": "swarm_result", "payload": {
                "reason": "idle",
                "messages_sent": 1,
                "dead_letters": [],
                "turns": {},
                "proposals": [{"id": pid, "from": "a", "approvals": ["b"], "accepted": true}],
                "required_approvals": 1
            }}}),
        )
    };
    std::fs::write(
        &pfad,
        format!("{}\n{}\n", ergebnis(1, "msg-1"), ergebnis(2, "msg-2")),
    )
    .unwrap();
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);

    let s = state.swarm();
    assert_eq!(s.results.len(), 2);
    assert_eq!(s.proposals.len(), 2);
    assert!(
        s.proposals.iter().all(|p| p.accepted),
        "beide Läufe haben abgeschlossen: {:?}",
        s.proposals
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Fängt der Trace ERST NACH dem Vorschlag an, kennt nur das Ergebnis ihn —
/// die Stimmen aus dem Strom dürfen deswegen nicht verloren gehen.
#[test]
fn stimmen_ohne_bekannten_vorschlag_werden_nachgetragen() {
    let dir = tmp("schwarm_stimmen");
    let pfad = dir.join("trace-1-1.jsonl");
    let vote = |seq: u64, von: &str, ja: bool| {
        zeile(
            seq,
            von,
            "structured",
            json!({"structured": {"kind": "swarm_event", "payload": {"vote_submitted": {"message": {
                "id": format!("v{seq}"), "swarm_id": "s1", "from": von, "to": "broadcast",
                "kind": "vote", "content": json!({"zustimmung": ja}).to_string(),
                "reply_to": null, "correlation_id": "msg-1", "created_at": 1, "hop_count": 0
            }}}}}),
        )
    };
    let ergebnis = zeile(
        3,
        "",
        "structured",
        json!({"structured": {"kind": "swarm_result", "payload": {
            "reason": "idle", "messages_sent": 0, "dead_letters": [], "turns": {},
            "proposals": [{"id": "msg-1", "from": "a", "approvals": [], "accepted": false}],
            "required_approvals": 2
        }}}),
    );
    std::fs::write(
        &pfad,
        format!(
            "{}\n{}\n{ergebnis}\n",
            vote(1, "b", true),
            vote(2, "c", false)
        ),
    )
    .unwrap();
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);

    let s = state.swarm();
    assert_eq!(s.proposals.len(), 1);
    assert_eq!(s.proposals[0].approvals, vec!["b"]);
    assert_eq!(
        s.proposals[0].rejections,
        vec!["c"],
        "die Ablehnung steht in keinem Ergebnis — nur im Strom"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------------------------- Graph

/// Der Graph-Endpunkt liefert den vollständigen Export — Entities, Claims und
/// die Quellen, über die eine Anzeige die Provenance einer Kante auflöst.
#[test]
#[cfg(feature = "graph")]
fn graph_endpunkt_liefert_den_export() {
    use agentkit_graph::{ClaimDraft, GraphAccess, GraphStore, GraphWriteCommand, SourceDraft};

    let dir = tmp("graph");
    let graph_dir = dir.join("graph");
    {
        let store = GraphStore::open(&graph_dir).unwrap();
        let zugang = GraphAccess::session("tester", "ws", "run-1");
        store
            .submit(
                GraphWriteCommand::RecordClaim(ClaimDraft::new(
                    "Modul M",
                    "haengt_an",
                    "Modul N",
                    SourceDraft::new("tool_result").excerpt("cargo tree"),
                )),
                &zugang,
            )
            .unwrap();
    }
    let (port, url) = server(&dir);
    let (status, rumpf) = get(port, &format!("/api/graph?t={}", token(&url)));
    assert_eq!(status, 200, "{rumpf}");
    let g: Value = serde_json::from_str(&rumpf).unwrap();
    assert_eq!(g["entities"].as_array().unwrap().len(), 2);
    assert_eq!(g["claims"][0]["predicate"], "haengt_an");
    let quelle = g["claims"][0]["source_ids"][0].as_str().unwrap();
    assert!(
        g["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == quelle),
        "die Provenance muss auflösbar sein: {g}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ohne Graph-Verzeichnis antwortet der Endpunkt mit einem sprechenden 404
/// statt mit einem Absturz — und er legt nichts an.
#[test]
#[cfg(feature = "graph")]
fn graph_endpunkt_ohne_graph_ist_leer_statt_kaputt() {
    let dir = tmp("graph_leer");
    let (port, url) = server(&dir);
    let (status, rumpf) = get(port, &format!("/api/graph?t={}", token(&url)));
    assert_eq!(
        status, 200,
        "ein noch leerer Graph ist kein Fehler: {rumpf}"
    );
    let g: Value = serde_json::from_str(&rumpf).unwrap();
    assert!(g["entities"].as_array().unwrap().is_empty());
    assert!(!dir.join("graph").exists(), "der Lesepfad legt nichts an");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Ein Work-Lauf beschriftet seine Ereignisse mit `<item>#<versuch>`; der
/// Betrachter muss sie danach gruppieren, sonst erscheinen fünf Work Items als
/// EIN Agent.
#[test]
fn work_item_agenten_werden_je_item_gruppiert() {
    let dir = tmp("workagenten");
    let pfad = dir.join("trace-1-1.jsonl");
    let schritt = |seq: u64, source: &str| zeile(seq, source, "step", json!({"step": {"step": 1}}));
    std::fs::write(
        &pfad,
        format!(
            "{}\n{}\n{}\n",
            schritt(1, "W-1#1"),
            schritt(2, "W-2#1"),
            schritt(3, "W-2#1/explorer-a"),
        ),
    )
    .unwrap();
    let mut reader = TraceReader::open(&pfad);
    let mut state = TraceState::new();
    state.extend(reader.read_new().unwrap().events);

    let agenten = state.agents();
    let ids: Vec<&str> = agenten.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["W-1#1", "W-2#1", "W-2#1/explorer-a"]);
    assert!(
        agenten
            .iter()
            .all(|a| a.kind == agentkit_viz::AgentKind::WorkItem),
        "auch das Schwarm-Mitglied gehört zu seinem Item: {agenten:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- Benchmarks

/// Der Benchmark-Reiter zeigt den ganzen Ergebnisbaum, nicht die gewählte
/// Sitzung: beide Treiber-Formen werden erkannt, das Ergebnis kommt aus dem,
/// was der Treiber gemessen hat, und jeder Task trägt den Sitzungsnamen, über
/// den man in seinen Verlauf springt.
#[test]
fn benchmark_laeufe_werden_erkannt_und_zusammengefasst() {
    let dir = tmp("benchmarks");
    // Ein SWE-bench-Lauf mit lokaler Auswertung.
    let swe = dir.join("swebench/lauf-1");
    let inst = swe.join("django__django-1/trace");
    std::fs::create_dir_all(&inst).unwrap();
    std::fs::create_dir_all(swe.join("django__django-1/work")).unwrap();
    std::fs::create_dir_all(swe.join("graph")).unwrap();
    beispiel_trace(&inst.join("trace-1-1.jsonl"));
    std::fs::write(
        swe.join("metadata.json"),
        r#"{"run_id":"lauf-1","model_name":"agentkit-test"}"#,
    )
    .unwrap();
    std::fs::write(
        swe.join("eval_local.json"),
        r#"{"total":1,"resolved":1,"results":[{"instance_id":"django__django-1","status":"resolved"}]}"#,
    )
    .unwrap();
    // Ein Harbor-Job.
    let job = dir.join("polyglot/job-1");
    std::fs::create_dir_all(job.join("polyglot_python_x__ab")).unwrap();
    std::fs::write(
        job.join("result.json"),
        r#"{"n_total_trials":1,"stats":{"evals":{"agentkit__aider-polyglot":
           {"reward_stats":{"reward":{"1.0":["polyglot_python_x__ab"]}}}}}}"#,
    )
    .unwrap();

    let (port, url) = server(&dir);
    let t = token(&url);
    let daten: Value =
        serde_json::from_str(&get(port, &format!("/api/benchmarks?t={t}")).1).unwrap();
    let runs = daten["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "beide Treiber-Formen: {daten}");

    let swe_lauf = runs.iter().find(|r| r["kind"] == "swebench").unwrap();
    assert_eq!(swe_lauf["name"], "swebench/lauf-1");
    assert_eq!(swe_lauf["summary"]["ok"], 1);
    assert_eq!(swe_lauf["summary"]["source"], "eval_local");
    let task = &swe_lauf["tasks"][0];
    assert_eq!(task["id"], "django__django-1");
    assert_eq!(
        task["status"], "resolved",
        "Status aus der lokalen Auswertung"
    );
    assert!(task["work"].as_bool().unwrap(), "Work-Projekt erkannt");
    assert_eq!(
        task["session"], "swebench/lauf-1/django__django-1/trace/trace-1-1.jsonl",
        "der Sitzungsname taugt direkt als run="
    );
    // `graph`/`work` sind KEINE Tasks — sonst stünden sie als Instanzen da.
    assert_eq!(swe_lauf["tasks"].as_array().unwrap().len(), 1);

    let harbor = runs.iter().find(|r| r["kind"] == "harbor").unwrap();
    assert_eq!(harbor["summary"]["ok"], 1);
    assert_eq!(harbor["summary"]["source"], "harbor");
    assert_eq!(harbor["tasks"][0]["status"], "reward 1.0");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Seite und API dürfen NICHT zwischengespeichert werden.
///
/// Stil und Skript stecken im Binary, die Adresse bleibt aber dieselbe: ohne
/// diesen Header liefert ein neu gebauter Betrachter eine neue Seite, die der
/// Browser gar nicht erst holt. Genau so ist ein neuer Reiter unsichtbar
/// geblieben — die schlimmste Sorte Fehler, weil nichts darauf hinweist.
#[test]
fn seite_und_api_werden_nicht_zwischengespeichert() {
    let dir = tmp("nocache");
    beispiel_trace(&dir.join("trace-1-1.jsonl"));
    let (port, url) = server(&dir);
    let t = token(&url);
    for pfad in [format!("/?t={t}"), format!("/api/runs?t={t}")] {
        let (status, kopf) = get_mit_kopf(port, &pfad);
        assert_eq!(status, 200, "{pfad}");
        assert!(
            kopf.to_lowercase().contains("cache-control: no-store"),
            "kein no-store für {pfad}:\n{kopf}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
