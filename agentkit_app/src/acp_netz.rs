//! `agentkit acp` über TCP: der Agent läuft auf einem anderen Rechner als der
//! Editor und arbeitet dort auf dessen Dateisystem.
//!
//! ```text
//! Editor ──stdio── agentkit acp --connect HOST:PORT   (Brücke, [`verbinde`])
//!                           │  TCP: eine Vorspann-Zeile, danach ACP unverändert
//!                  agentkit acp --listen ADDR:PORT    (Server, [`lausche`])
//! ```
//!
//! Das Protokoll selbst bleibt, wie es über stdio ist. Davor steht genau eine
//! Zeile in jede Richtung (der Vorspann): der Client weist sich mit dem Token
//! aus und nennt optional sein Verzeichnis auf dem Server. Der Vorspann liegt
//! **vor** ACP und nicht in dessen `authenticate`, damit die Brücke das
//! Protokoll nicht verstehen muss — sie kopiert danach nur noch Bytes.
//!
//! **Kein TLS.** Token und Sitzung laufen im Klartext; gedacht ist das für
//! Loopback, einen Tunnel, ein VPN oder ein vertrautes Netz. Der Server bindet
//! deshalb nur an die Adresse, die man ihm nennt.

use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentkit::acp::AgentFactory;
use agentkit::{ApproveFn, RunStrategy};
use serde_json::{json, Value};

/// Umgebungsvariable mit dem Token; hat Vorrang vor der Datei.
pub const TOKEN_ENV: &str = "AGENTKIT_ACP_TOKEN";

/// Version des Vorspanns — nicht die von ACP.
const VORSPANN_VERSION: u64 = 1;

/// Mehr liest der Server vor der Prüfung des Tokens nicht ein.
const VORSPANN_MAX: usize = 4096;

/// So lange darf der Vorspann bzw. der Verbindungsaufbau dauern.
const FRIST: Duration = Duration::from_secs(10);

/// `<config_dir>/acp-token`.
pub fn token_datei() -> Option<PathBuf> {
    agentkit::config_dir().map(|d| d.join("acp-token"))
}

/// Das Token aus [`TOKEN_ENV`], sonst aus der Datei. `None`, wenn keines da ist.
fn token_lesen() -> Option<String> {
    let aus_env = std::env::var(TOKEN_ENV).ok();
    let aus_datei = || std::fs::read_to_string(token_datei()?).ok();
    aus_env
        .or_else(aus_datei)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Das Token der Brücke. Fehlt es, sagt die Meldung, wo es hingehört.
pub fn token_laden() -> Result<String, String> {
    token_lesen().ok_or_else(|| {
        let datei = token_datei()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<config_dir>/acp-token".to_string());
        format!("kein Token: das Token des Servers in {TOKEN_ENV} setzen oder in {datei} ablegen")
    })
}

/// Das Token des Servers. Gibt es noch keines, entsteht es jetzt in der Datei.
pub fn token_laden_oder_anlegen() -> Result<String, String> {
    if let Some(token) = token_lesen() {
        return Ok(token);
    }
    let datei = token_datei().ok_or("kein Konfigurationsverzeichnis für das Token")?;
    let fehler = |e: std::io::Error| format!("{}: {e}", datei.display());
    if let Some(dir) = datei.parent() {
        std::fs::create_dir_all(dir).map_err(fehler)?;
    }
    let token = neues_token();
    let mut optionen = std::fs::OpenOptions::new();
    optionen.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut optionen, 0o600);
    optionen
        .open(&datei)
        .and_then(|mut f| writeln!(f, "{token}"))
        .map_err(fehler)?;
    Ok(token)
}

/// 256 Bit aus der Zufallsquelle des Betriebssystems: `RandomState` wird von
/// der Standardbibliothek damit geimpft (wie `agentkit_graph::trust`).
fn neues_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..4)
        .map(|_| {
            let wert = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            format!("{wert:016x}")
        })
        .collect()
}

/// Vergleich ohne frühen Ausstieg: die Dauer verrät nicht, wie viele Zeichen
/// eines geratenen Tokens stimmen.
fn gleich_konstant(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Liest eine Zeile Byte für Byte. Ein Puffer läse über das Zeilenende hinaus
/// und nähme dem Protokoll danach seine ersten Bytes weg.
fn zeile_lesen(strom: &mut impl Read) -> Result<String, String> {
    let mut zeile = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match strom.read(&mut byte) {
            Ok(0) => return Err("Verbindung vor dem Vorspann beendet".to_string()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => zeile.push(byte[0]),
            Err(e) => return Err(format!("Vorspann nicht lesbar: {e}")),
        }
        if zeile.len() > VORSPANN_MAX {
            return Err("Vorspann zu lang".to_string());
        }
    }
    String::from_utf8(zeile).map_err(|_| "Vorspann ist kein UTF-8".to_string())
}

/// Client-Seite des Vorspanns: Token (und Verzeichnis) senden, Antwort prüfen.
fn vorspann_senden(
    strom: &mut (impl Read + Write),
    token: &str,
    dir: Option<&str>,
) -> Result<(), String> {
    let mut vorspann = json!({"agentkit_acp": VORSPANN_VERSION, "token": token});
    if let Some(dir) = dir {
        vorspann["dir"] = json!(dir);
    }
    writeln!(strom, "{vorspann}")
        .and_then(|()| strom.flush())
        .map_err(|e| format!("Vorspann nicht sendbar: {e}"))?;
    let antwort: Value = serde_json::from_str(&zeile_lesen(strom)?)
        .map_err(|_| "die Gegenseite ist kein agentkit-ACP-Server".to_string())?;
    if antwort["ok"] == true {
        return Ok(());
    }
    Err(antwort["error"]
        .as_str()
        .unwrap_or("vom Server abgewiesen")
        .to_string())
}

/// Server-Seite des Vorspanns: prüft das Token und antwortet. `Ok` trägt das
/// Verzeichnis, das der Client genannt hat.
fn vorspann_pruefen(
    strom: &mut (impl Read + Write),
    token: &str,
) -> Result<Option<String>, String> {
    let ergebnis = zeile_lesen(strom).and_then(|zeile| {
        let v: Value =
            serde_json::from_str(&zeile).map_err(|_| "Vorspann ist kein JSON".to_string())?;
        if v["agentkit_acp"] != VORSPANN_VERSION {
            return Err(format!(
                "unbekannte Vorspann-Version (erwartet {VORSPANN_VERSION})"
            ));
        }
        if !gleich_konstant(v["token"].as_str().unwrap_or(""), token) {
            return Err("falsches Token".to_string());
        }
        Ok(v["dir"].as_str().map(str::to_string))
    });
    let antwort = match &ergebnis {
        Ok(_) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    let _ = writeln!(strom, "{antwort}").and_then(|()| strom.flush());
    ergebnis
}

/// Das Arbeitsverzeichnis einer Sitzung: `angabe` muss ein vorhandenes
/// Verzeichnis unter `wurzel` sein (relativ gilt ab der Wurzel). `wurzel` ist
/// schon kanonisch ([`agentkit::coding::kanonisch`]).
///
/// Angelegt wird nichts — sonst bestimmte ein Client, wo auf dem Server neue
/// Verzeichnisse entstehen.
pub fn ziel_unter_wurzel(wurzel: &Path, angabe: &str) -> Result<PathBuf, String> {
    // Der häufigste Fehlgriff ist das `cwd` eines Editors, das es nur auf
    // dessen Rechner gibt — deshalb nennt jede Absage den Ausweg.
    let ausweg = format!(
        "beim Verbinden mit --dir ein Verzeichnis unter {} nennen",
        wurzel.display()
    );
    let ziel = agentkit::coding::kanonisch(&wurzel.join(angabe))
        .filter(|p| p.is_dir())
        .ok_or_else(|| format!("{angabe} ist auf dem Server kein Verzeichnis; {ausweg}"))?;
    if !ziel.starts_with(wurzel) {
        return Err(format!(
            "{angabe} liegt nicht unter der Wurzel des Servers; {ausweg}"
        ));
    }
    Ok(ziel)
}

/// Der Server: nimmt Verbindungen an, bis der Listener endet. Jede Verbindung
/// läuft auf einem eigenen Thread und hat ihre eigenen Sitzungen; `fabrik`
/// bekommt statt des `cwd` des Editors das geprüfte Verzeichnis unter `wurzel`.
pub fn lausche(
    listener: TcpListener,
    token: String,
    wurzel: PathBuf,
    strategy: RunStrategy,
    version: &str,
    fabrik: AgentFactory,
) {
    for strom in listener.incoming() {
        let strom = match strom {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[acp] Verbindung nicht annehmbar: {e}");
                continue;
            }
        };
        let (token, wurzel, version, fabrik) = (
            token.clone(),
            wurzel.clone(),
            version.to_string(),
            fabrik.clone(),
        );
        std::thread::spawn(move || {
            let wer = strom
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "?".to_string());
            match bediene(strom, &token, wurzel, strategy, &version, fabrik) {
                Ok(()) => eprintln!("[acp] {wer}: Verbindung beendet"),
                Err(e) => eprintln!("[acp] {wer}: {e}"),
            }
        });
    }
}

/// Eine Verbindung: Vorspann, dann ACP bis zum Ende der Verbindung.
fn bediene(
    mut strom: TcpStream,
    token: &str,
    wurzel: PathBuf,
    strategy: RunStrategy,
    version: &str,
    fabrik: AgentFactory,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    // Die Frist gilt nur dem Vorspann: wer verbindet und schweigt, soll keinen
    // Thread halten. Eine Sitzung darf danach beliebig lange still sein.
    strom.set_read_timeout(Some(FRIST)).map_err(io)?;
    let dir = vorspann_pruefen(&mut strom, token)?;
    strom.set_read_timeout(None).map_err(io)?;

    let je_sitzung: AgentFactory = Arc::new(move |cwd: &str, approve: ApproveFn| {
        let ziel = ziel_unter_wurzel(&wurzel, dir.as_deref().unwrap_or(cwd))?;
        fabrik(&ziel.to_string_lossy(), approve)
    });
    let lesen = BufReader::new(strom.try_clone().map_err(io)?);
    let ende = strom.try_clone().map_err(io)?;
    agentkit::acp::serve_netz(je_sitzung, strategy, lesen, strom, version);
    // Ausdrücklich schließen: hält irgendwo noch ein Agent seinen
    // Freigabe-Callback (und damit die Leitung), bliebe die Verbindung sonst
    // offen und die Brücke wartete auf ein Ende, das nie kommt.
    let _ = ende.shutdown(Shutdown::Both);
    Ok(())
}

/// Die Brücke: verbindet `eingabe`/`ausgabe` (stdin/stdout des Editors) mit
/// dem Server. Kehrt zurück, wenn der Server die Verbindung schließt — das tut
/// er auch, sobald `eingabe` endet.
pub fn verbinde(
    addr: &str,
    token: &str,
    dir: Option<&str>,
    mut eingabe: impl Read + Send + 'static,
    mut ausgabe: impl Write,
) -> Result<(), String> {
    let mut strom = verbindung_zu(addr)?;
    let io = |e: std::io::Error| e.to_string();
    strom.set_read_timeout(Some(FRIST)).map_err(io)?;
    vorspann_senden(&mut strom, token, dir)?;
    strom.set_read_timeout(None).map_err(io)?;

    let mut hin = strom.try_clone().map_err(io)?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut eingabe, &mut hin);
        // Der Editor ist weg: dem Server das Ende melden, seine restlichen
        // Zeilen aber noch abholen.
        let _ = hin.shutdown(Shutdown::Write);
    });
    // Nach jedem Stück leeren: der Editor wartet auf die Zeile, nicht auf
    // einen vollen Puffer.
    let mut block = [0u8; 8192];
    loop {
        match strom.read(&mut block) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => ausgabe
                .write_all(&block[..n])
                .and_then(|()| ausgabe.flush())
                .map_err(io)?,
        }
    }
}

/// Verbindet zur ersten erreichbaren Adresse hinter `addr`.
fn verbindung_zu(addr: &str) -> Result<TcpStream, String> {
    let adressen = addr.to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?;
    let mut letzter = format!("{addr}: keine Adresse");
    for a in adressen {
        match TcpStream::connect_timeout(&a, FRIST) {
            Ok(s) => return Ok(s),
            Err(e) => letzter = format!("{a}: {e}"),
        }
    }
    Err(letzter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Eine Leitung im Speicher: liest die vorbereitete Eingabe, sammelt die
    /// Ausgabe.
    struct Leitung(Cursor<Vec<u8>>, Vec<u8>);
    impl Leitung {
        fn mit(eingabe: &str) -> Self {
            Leitung(Cursor::new(eingabe.as_bytes().to_vec()), Vec::new())
        }
        fn gesendet(&self) -> Value {
            serde_json::from_slice(&self.1).unwrap()
        }
    }
    impl Read for Leitung {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for Leitung {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.1.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Was die Brücke sendet, nimmt der Server an — samt Verzeichnis.
    #[test]
    fn vorspann_der_bruecke_passt_zum_server() {
        let mut client = Leitung::mit("{\"ok\":true}\n");
        vorspann_senden(&mut client, "geheim", Some("projekt")).unwrap();

        let mut server = Leitung(Cursor::new(client.1), Vec::new());
        let dir = vorspann_pruefen(&mut server, "geheim").unwrap();
        assert_eq!(dir.as_deref(), Some("projekt"));
        assert_eq!(server.gesendet()["ok"], true);
    }

    /// Ein falsches Token wird abgewiesen, und die Brücke reicht den Grund
    /// des Servers weiter.
    #[test]
    fn falsches_token_wird_abgewiesen() {
        let mut server = Leitung::mit("{\"agentkit_acp\":1,\"token\":\"geraten\"}\n");
        assert_eq!(
            vorspann_pruefen(&mut server, "geheim"),
            Err("falsches Token".to_string())
        );
        assert_eq!(server.gesendet()["ok"], false);

        let mut client = Leitung(Cursor::new([server.1, b"\n".to_vec()].concat()), Vec::new());
        assert_eq!(
            vorspann_senden(&mut client, "geraten", None),
            Err("falsches Token".to_string())
        );
    }

    /// Vor der Prüfung des Tokens liest der Server nur begrenzt viel, und nur
    /// seinen eigenen Vorspann.
    #[test]
    fn vorspann_ist_begrenzt_und_versioniert() {
        let mut lang = Leitung::mit(&"x".repeat(VORSPANN_MAX + 10));
        assert_eq!(
            vorspann_pruefen(&mut lang, "geheim"),
            Err("Vorspann zu lang".to_string())
        );
        let mut fremd = Leitung::mit("{\"jsonrpc\":\"2.0\",\"method\":\"initialize\"}\n");
        assert!(vorspann_pruefen(&mut fremd, "geheim")
            .unwrap_err()
            .contains("Vorspann-Version"));
    }

    /// Der Vorspann nimmt dem Protokoll nichts weg: was nach der Zeile kommt,
    /// liegt noch in der Leitung.
    #[test]
    fn vorspann_liest_nicht_ueber_die_zeile_hinaus() {
        let mut server = Leitung::mit("{\"agentkit_acp\":1,\"token\":\"t\"}\nREST");
        vorspann_pruefen(&mut server, "t").unwrap();
        let mut rest = String::new();
        server.0.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "REST");
    }

    #[test]
    fn token_vergleich() {
        assert!(gleich_konstant("abc", "abc"));
        assert!(!gleich_konstant("abc", "abd"));
        assert!(!gleich_konstant("abc", "abcd"));
        assert!(!gleich_konstant("", "abc"));
        assert_eq!(neues_token().len(), 64);
        assert_ne!(neues_token(), neues_token());
    }

    /// Nur vorhandene Verzeichnisse unter der Wurzel; `..` und absolute Pfade
    /// nach draußen fallen durch, und es entsteht nichts Neues.
    #[test]
    fn verzeichnis_muss_unter_der_wurzel_liegen() {
        let basis = std::env::temp_dir().join(format!("agentkit_acp_netz_{}", std::process::id()));
        std::fs::remove_dir_all(&basis).ok();
        std::fs::create_dir_all(basis.join("wurzel").join("projekt")).unwrap();
        std::fs::create_dir_all(basis.join("draussen")).unwrap();
        std::fs::write(basis.join("wurzel").join("datei.txt"), "x").unwrap();
        let wurzel = agentkit::coding::kanonisch(&basis.join("wurzel")).unwrap();
        let draussen = agentkit::coding::kanonisch(&basis.join("draussen")).unwrap();

        assert_eq!(ziel_unter_wurzel(&wurzel, "."), Ok(wurzel.clone()));
        assert_eq!(
            ziel_unter_wurzel(&wurzel, "projekt"),
            Ok(wurzel.join("projekt"))
        );
        let absolut = wurzel.join("projekt");
        assert_eq!(
            ziel_unter_wurzel(&wurzel, &absolut.to_string_lossy()),
            Ok(absolut)
        );

        for (angabe, grund) in [
            ("..", "nicht unter der Wurzel"),
            ("../draussen", "nicht unter der Wurzel"),
            (&*draussen.to_string_lossy(), "nicht unter der Wurzel"),
            ("fehlt", "kein Verzeichnis"),
            ("datei.txt", "kein Verzeichnis"),
        ] {
            let fehler = ziel_unter_wurzel(&wurzel, angabe).unwrap_err();
            assert!(fehler.contains(grund), "{angabe}: {fehler}");
        }
        assert!(!wurzel.join("fehlt").exists(), "nichts wird angelegt");
        std::fs::remove_dir_all(&basis).ok();
    }
}
