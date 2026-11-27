//! ACP über TCP, Ende zu Ende: ein Server ([`lausche`]) auf einem vom
//! Betriebssystem gewählten Loopback-Port, davor die Brücke ([`verbinde`]) mit
//! einer Ein- und Ausgabe im Speicher — so, wie ein Editor sie über stdio
//! bedient. Offline: das Modell ist `FakeLlm`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit::acp::AgentFactory;
use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{Agent, ApproveFn, RunStrategy, ToolRegistry};
use agentkit_app::acp_netz::{lausche, verbinde};
use serde_json::{json, Value};

const TOKEN: &str = "test-token";

/// stdin des Editors: der Test schiebt Zeilen nach; fällt der Sender weg, ist
/// das Ende der Eingabe erreicht.
struct Eingabe(Receiver<Vec<u8>>, Vec<u8>);
impl Read for Eingabe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(bytes) => self.1 = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        self.1.drain(..n);
        Ok(n)
    }
}

/// stdout des Editors: jede vollständige Zeile geht als JSON an den Test.
struct Ausgabe(Sender<Value>, Vec<u8>);
impl Write for Ausgabe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.1.extend_from_slice(buf);
        while let Some(pos) = self.1.iter().position(|b| *b == b'\n') {
            let zeile: Vec<u8> = self.1.drain(..=pos).collect();
            let _ = self.0.send(serde_json::from_slice(&zeile).unwrap());
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Eine Wurzel mit dem Unterverzeichnis `projekt` und daneben `draussen`.
fn verzeichnisse(name: &str) -> (PathBuf, PathBuf) {
    let basis =
        std::env::temp_dir().join(format!("agentkit_acp_tcp_{name}_{}", std::process::id()));
    std::fs::remove_dir_all(&basis).ok();
    std::fs::create_dir_all(basis.join("wurzel").join("projekt")).unwrap();
    std::fs::create_dir_all(basis.join("draussen")).unwrap();
    let kanonisch = |p: PathBuf| agentkit::coding::kanonisch(&p).unwrap();
    (
        kanonisch(basis.join("wurzel")),
        kanonisch(basis.join("draussen")),
    )
}

/// Startet den Server und gibt seine Adresse zurück. Die Fabrik merkt sich,
/// in welchem Verzeichnis jede Sitzung gebaut wurde.
fn server(wurzel: PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
    let gebaut: Arc<Mutex<Vec<String>>> = Arc::default();
    let merker = gebaut.clone();
    let fabrik: AgentFactory = Arc::new(move |cwd: &str, _approve: ApproveFn| {
        merker.lock().unwrap().push(cwd.to_string());
        let llm = Arc::new(FakeLlm::new(vec![vec![
            Chunk::text("Hallo "),
            Chunk::text("vom Server."),
        ]]));
        Ok(Agent::new(llm, ToolRegistry::new()))
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let adresse = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        lausche(
            listener,
            TOKEN.to_string(),
            wurzel,
            RunStrategy::default(),
            "1.0",
            fabrik,
        )
    });
    (adresse, gebaut)
}

/// Eine laufende Brücke aus Sicht des Editors.
struct Editor {
    hin: Sender<Vec<u8>>,
    her: Receiver<Value>,
}

impl Editor {
    fn verbunden(adresse: &str, dir: Option<&str>) -> Editor {
        let (hin, ein_rx) = channel::<Vec<u8>>();
        let (aus_tx, her) = channel::<Value>();
        let (adresse, dir) = (adresse.to_string(), dir.map(str::to_string));
        std::thread::spawn(move || {
            verbinde(
                &adresse,
                TOKEN,
                dir.as_deref(),
                Eingabe(ein_rx, Vec::new()),
                Ausgabe(aus_tx, Vec::new()),
            )
            .unwrap()
        });
        Editor { hin, her }
    }

    fn senden(&self, v: Value) {
        self.hin.send(format!("{v}\n").into_bytes()).unwrap();
    }

    /// Die Antwort auf die Anfrage `id`, samt allem Text davor.
    fn antwort(&self, id: u64) -> (Value, String) {
        let mut text = String::new();
        loop {
            let m = self.her.recv_timeout(Duration::from_secs(10)).unwrap();
            if m["id"] == id {
                return (m, text);
            }
            let u = &m["params"]["update"];
            if u["sessionUpdate"] == "agent_message_chunk" {
                text.push_str(u["content"]["text"].as_str().unwrap());
            }
        }
    }

    fn neue_sitzung(&self, id: u64, cwd: &str) -> Value {
        self.senden(json!({"jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": cwd, "mcpServers": []}}));
        self.antwort(id).0
    }
}

/// Ein ganzer Zug durch Brücke und Server. Das Verzeichnis kommt aus `--dir`
/// (relativ zur Wurzel) — das `cwd` des Editors, ein Pfad seines eigenen
/// Rechners, zählt dann nicht.
#[test]
fn ein_zug_ueber_tcp_im_verzeichnis_der_bruecke() {
    let (wurzel, _) = verzeichnisse("zug");
    let (adresse, gebaut) = server(wurzel.clone());
    let editor = Editor::verbunden(&adresse, Some("projekt"));

    editor.senden(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1}}));
    assert_eq!(editor.antwort(1).0["result"]["protocolVersion"], 1);

    let sitzung = editor.neue_sitzung(2, "/Users/jemand/woanders");
    let sid = sitzung["result"]["sessionId"].as_str().unwrap().to_string();
    assert_eq!(
        *gebaut.lock().unwrap(),
        [wurzel.join("projekt").to_string_lossy().to_string()]
    );

    editor.senden(
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "hallo"}]}}),
    );
    let (antwort, text) = editor.antwort(3);
    assert_eq!(antwort["result"]["stopReason"], "end_turn");
    assert_eq!(text, "Hallo vom Server.");
}

/// Ohne `--dir` gilt das `cwd` des Editors — aber nur unter der Wurzel. Ein
/// Verzeichnis daneben wird abgelehnt, ohne dass ein Agent entsteht.
#[test]
fn cwd_des_editors_gilt_nur_unter_der_wurzel() {
    let (wurzel, draussen) = verzeichnisse("wurzel");
    let (adresse, gebaut) = server(wurzel.clone());
    let editor = Editor::verbunden(&adresse, None);

    let abgelehnt = editor.neue_sitzung(1, &draussen.to_string_lossy());
    assert_eq!(abgelehnt["error"]["code"], -32603);
    assert!(abgelehnt["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nicht unter der Wurzel"));
    assert!(gebaut.lock().unwrap().is_empty());

    let drin = wurzel.join("projekt");
    let angenommen = editor.neue_sitzung(2, &drin.to_string_lossy());
    assert!(angenommen["result"]["sessionId"].is_string());
    assert_eq!(
        *gebaut.lock().unwrap(),
        [drin.to_string_lossy().to_string()]
    );
}

/// Mit dem falschen Token endet die Brücke sofort und nennt den Grund.
#[test]
fn falsches_token_beendet_die_bruecke() {
    let (wurzel, _) = verzeichnisse("token");
    let (adresse, gebaut) = server(wurzel);
    let ergebnis = verbinde(&adresse, "geraten", None, std::io::empty(), std::io::sink());
    assert_eq!(ergebnis, Err("falsches Token".to_string()));
    assert!(gebaut.lock().unwrap().is_empty());
}
