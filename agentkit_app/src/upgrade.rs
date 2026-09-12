//! Selbst-Update der installierten Executable (`agentkit --upgrade [VERSION]`).
//!
//! Die Domäne (Asset-Auswahl, Versions-Vergleich, URL-Bau, Orchestrierung) ist
//! IMMER kompiliert — nur `std` + `serde_json`, kein Feature-Gate — damit sie
//! offline und ohne das `upgrade`-Feature testbar bleibt. Nur der echte
//! HTTP-Adapter ([`UreqNetz`]) hängt hinter dem Feature `upgrade`.
//!
//! Sicherheitsanker: GitHub veröffentlicht keine Prüfsummen zu den Release-
//! Assets. Vertrauensanker sind deshalb ausschließlich TLS + die Domains
//! `github.com`/`api.github.com` — [`download_url`] und [`latest_api_url`]
//! bauen die URL selbst aus [`REPO`]/Tag/Asset zusammen; die
//! `browser_download_url` aus dem Release-JSON wird bewusst NIE übernommen,
//! sonst könnte eine manipulierte API-Antwort den Download umleiten.

use std::path::{Path, PathBuf};

/// Das GitHub-Repository, aus dem Releases geladen werden.
pub const REPO: &str = "rudi77/agentkit_rs";

/// Gemeinsamer Hinweistext für beide Fehlerzweige von [`asset_name`] — vermeidet
/// wörtliche Duplizierung des Quellcode-Hinweises.
const QUELLCODE_HINWEIS: &str = "Installiere stattdessen aus dem Quellcode: `cargo install \
     --path agentkit_app --bin agentkit --features \"tui pdf ctxman tiktoken graph work viz\"`.";

/// Wählt den unversionierten Release-Asset-Namen für Plattform/Variante.
///
/// `os`/`arch` erwarten die Strings aus `std::env::consts::OS`/`ARCH`
/// (`"linux"`/`"windows"`, `"x86_64"`). Es gibt keine macOS- und keine
/// aarch64-Assets — beides ist ein harter Fehler mit Hinweis auf den
/// Quellcode-Build.
pub fn asset_name(os: &str, arch: &str, mit_tui: bool) -> Result<&'static str, String> {
    if arch != "x86_64" {
        return Err(format!(
            "Nicht unterstützte Architektur „{arch}“ — es gibt nur x86_64-Release-Assets. \
             {QUELLCODE_HINWEIS}"
        ));
    }
    match (os, mit_tui) {
        ("linux", true) => Ok("agentkit-linux-x86_64"),
        ("linux", false) => Ok("agentkit-cli-linux-x86_64"),
        ("windows", true) => Ok("agentkit-windows-x86_64.exe"),
        ("windows", false) => Ok("agentkit-cli-windows-x86_64.exe"),
        _ => Err(format!(
            "Nicht unterstützte Plattform „{os}“ — es gibt nur Linux/Windows-Release-Assets. \
             {QUELLCODE_HINWEIS}"
        )),
    }
}

/// Normalisiert eine Versionsangabe (`1.2.3`/`v1.2.3`, optional `-suffix`) zu
/// einem Release-Tag (`v1.2.3`).
///
/// Bewusst restriktiv: erlaubt sind nur `<zahl>.<zahl>.<zahl>` plus ein
/// optionaler `-suffix` aus alphanumerischen Zeichen, `.` und `-`. Das
/// verhindert strukturell, dass Zeichen wie `/`, `..`, Leerraum, `?` oder `#`
/// in eine spätere URL gelangen (siehe [`download_url`]) — es braucht keine
/// gesonderte URL-Escaping-Logik, weil kein ungültiges Zeichen je bis dorthin
/// kommt.
pub fn normalisiere_tag(eingabe: &str) -> Result<String, String> {
    let fehler = || {
        format!(
            "Ungültige Versionsangabe „{eingabe}“ — erwartet wird <major>.<minor>.<patch> \
             mit optionalem -suffix, z. B. 1.2.3 oder v1.2.3-beta.1"
        )
    };
    let ohne_v = eingabe.strip_prefix('v').unwrap_or(eingabe);
    let mut teile = ohne_v.splitn(2, '-');
    let kern = teile.next().unwrap_or("");
    let suffix = teile.next();

    let zahlen: Vec<&str> = kern.split('.').collect();
    let zahlen_ok = zahlen.len() == 3
        && zahlen
            .iter()
            .all(|z| !z.is_empty() && z.chars().all(|c| c.is_ascii_digit()));
    let suffix_ok = suffix.map_or(true, |s| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    });

    if !zahlen_ok || !suffix_ok {
        return Err(fehler());
    }
    Ok(format!("v{ohne_v}"))
}

/// Liest `tag_name` aus dem JSON-Body der GitHub-Releases-API. `None` bei
/// kaputtem JSON, fehlendem Feld oder falschem Typ.
pub fn tag_aus_release_json(body: &str) -> Option<String> {
    let wert: serde_json::Value = serde_json::from_str(body).ok()?;
    wert.get("tag_name")?.as_str().map(str::to_string)
}

/// Das führende `v` eines Release-Tags weg — `"v1.2.3"` -> `"1.2.3"`.
pub fn version_von_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// Rein numerischer Vergleich dreier Versionskomponenten (`major.minor.patch`,
/// ein `-suffix` wird ignoriert). Nicht parsebare Eingaben liefern `false`
/// statt zu paniken — ein Vergleich ist hier nie ein harter Fehler.
///
/// Numerisch, nicht lexikographisch: `"0.10.0"` ist neuer als `"0.9.0"`,
/// obwohl `"0.10.0" < "0.9.0"` als String gälte.
pub fn ist_neuer(a: &str, b: &str) -> bool {
    fn komponenten(s: &str) -> Option<(u64, u64, u64)> {
        let ohne_v = s.strip_prefix('v').unwrap_or(s);
        let kern = ohne_v.split('-').next().unwrap_or(ohne_v);
        let mut teile = kern.split('.');
        let x = teile.next()?.parse().ok()?;
        let y = teile.next()?.parse().ok()?;
        let z = teile.next()?.parse().ok()?;
        if teile.next().is_some() {
            return None;
        }
        Some((x, y, z))
    }
    match (komponenten(a), komponenten(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Baut die Download-URL eines Assets aus `REPO`/Tag/Asset-Namen selbst —
/// niemals aus der `browser_download_url` des Release-JSON (siehe Modul-Doku).
pub fn download_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

/// Die URL des `latest`-Release-Endpunkts der GitHub-API.
pub fn latest_api_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// Der einzige I/O-Port des Moduls: HTTP-Text/-Bytes abrufen. Zwei reale
/// Implementierungen — [`UreqNetz`] (Feature `upgrade`) und `FakeNetz` in den
/// Tests dieses Moduls — rechtfertigen den Trait an dieser schmalen Stelle.
pub trait Netz {
    fn text(&self, url: &str) -> Result<String, String>;
    fn bytes(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Ergebnis von [`ziel_tag`]: entweder ist die laufende Version bereits die
/// richtige, oder es gibt ein konkretes Ziel-Tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entscheidung {
    Aktuell,
    Wechsel(String),
}

/// Entscheidet, welches Release-Tag Ziel eines Upgrades ist.
///
/// Ohne `gewuenscht` gewinnt der neueste Release — aber nur, wenn er
/// tatsächlich NEUER ist als die laufende Version ([`ist_neuer`]; ansonsten
/// [`Entscheidung::Aktuell`]). Ein lokal gebauter Vorabstand (z. B.
/// `0.24.0-dev` gegen den letzten veröffentlichten Release `v0.23.0`) darf
/// durch ein blankes `agentkit --upgrade` nicht heimlich heruntergestuft
/// werden — wer explizit eine ältere Version will, nennt sie explizit über
/// `gewuenscht`. Mit `gewuenscht` ist der Nutzerwunsch bindend (auch ein
/// Downgrade), außer er entspricht exakt der laufenden Version — dann ist
/// ein "Wechsel" auf sich selbst sinnlos und es bleibt ebenfalls bei
/// `Aktuell`.
///
/// BEIDE Quellen laufen durch [`normalisiere_tag`] — auch der von GitHub
/// gemeldete `neuester`. Das ist keine Förmlichkeit: der Tag landet später im
/// PFAD der Download-URL ([`download_url`]). Ein Tag wie
/// `x/../../../fremdes/repo/releases/download/v1` würde die Domain-Prüfung
/// passieren und trotzdem ein fremdes Asset laden — genau die Umleitung, die
/// der selbst gebaute URL-Pfad verhindern soll (siehe Modul-Doku).
///
/// `neuester` ist `None`, wenn gar nicht erst abgefragt wurde — das ist nur
/// zusammen mit `gewuenscht` sinnvoll, weil dann schon feststeht, wohin es
/// geht. Ohne beides gibt es nichts zu entscheiden.
pub fn ziel_tag(
    aktuell: &str,
    gewuenscht: Option<&str>,
    neuester: Option<&str>,
) -> Result<Entscheidung, String> {
    let tag = match (gewuenscht, neuester) {
        (Some(wunsch), _) => normalisiere_tag(wunsch)?,
        (None, Some(n)) => {
            let tag = normalisiere_tag(n).map_err(|_| {
                format!("GitHub meldet das unbrauchbare Release-Tag „{n}“ — Abbruch.")
            })?;
            if !ist_neuer(version_von_tag(&tag), aktuell) {
                return Ok(Entscheidung::Aktuell);
            }
            tag
        }
        (None, None) => {
            return Err(
                "Interner Fehler: ohne Versionswunsch braucht es den neuesten Release-Tag."
                    .to_string(),
            )
        }
    };
    if version_von_tag(&tag) == aktuell {
        Ok(Entscheidung::Aktuell)
    } else {
        Ok(Entscheidung::Wechsel(tag))
    }
}

/// Der Ablageort der verdrängten alten Binary (`<ziel>.alt`). Das Namensschema
/// lebt nur hier, damit Tausch, Aufräumen und Test nicht auseinanderlaufen.
/// Nur Windows braucht den Umweg (siehe [`ersetze_binary`]); der Test räumt ihn
/// plattformübergreifend mit auf.
#[cfg(any(windows, test))]
fn alt_pfad(ziel: &Path) -> PathBuf {
    let mut name = ziel.as_os_str().to_owned();
    name.push(".alt");
    PathBuf::from(name)
}

/// Ersetzt die laufende Binary `ziel` durch die neu heruntergeladene `tmp`
/// (beide im selben Verzeichnis, damit der abschließende `rename` atomar
/// bleibt).
///
/// Unix erlaubt das Überschreiben einer laufenden Executable direkt
/// (`rename` ist atomar, das offene Datei-Handle des laufenden Prozesses
/// bleibt gültig). Windows sperrt die Datei der laufenden Executable gegen
/// Löschen/Ersetzen, nicht aber gegen Umbenennen — daher der Umweg über
/// `ziel.alt`: `ziel` -> `ziel.alt` (macht den Namen frei), `tmp` -> `ziel`,
/// dann `ziel.alt` best-effort löschen (das Betriebssystem gibt die Sperre
/// oft erst nach Prozessende frei). Liegt `ziel.alt` danach noch da, liefert
/// die Funktion ihren Pfad zurück, damit der Aufrufer einen Hinweis ausgeben
/// kann.
///
/// Schlägt unter Windows der zweite `rename` (`tmp` -> `ziel`) fehl, würde
/// `ziel` sonst ersatzlos verschwinden (nur `ziel.alt` bliebe übrig) — die
/// Funktion versucht deshalb ein Rollback (`ziel.alt` -> `ziel`) und gibt
/// danach den URSPRÜNGLICHEN Fehler des zweiten `rename` zurück, unabhängig
/// davon, ob das Rollback selbst gelingt (mehr als der Versuch ist an dieser
/// Stelle nicht möglich).
///
/// Der Aufrufer ist dafür verantwortlich, die Ausführungsrechte von `tmp`
/// VOR diesem Aufruf zu setzen (unter Unix z. B. `chmod 0o755`) — diese
/// Funktion setzt keine Berechtigungen mehr.
pub fn ersetze_binary(ziel: &Path, tmp: &Path) -> std::io::Result<Option<PathBuf>> {
    #[cfg(unix)]
    {
        std::fs::rename(tmp, ziel)?;
        Ok(None)
    }
    #[cfg(windows)]
    {
        let alt = alt_pfad(ziel);
        // Rest eines früher abgebrochenen Upgrades wegräumen: `rename` scheitert
        // unter Windows, wenn das Ziel schon existiert. Best-effort — hält die
        // Sperre einer noch laufenden alten Instanz die Datei fest, schlägt erst
        // der `rename` darunter fehl, und zwar mit der aussagekräftigeren Meldung.
        std::fs::remove_file(&alt).ok();
        std::fs::rename(ziel, &alt)?;
        if let Err(e) = std::fs::rename(tmp, ziel) {
            // Rollback best-effort: mehr als der Versuch geht hier nicht —
            // der ursprüngliche Fehler zählt, nicht ein evtl. scheiterndes
            // Rollback.
            let _ = std::fs::rename(&alt, ziel);
            return Err(e);
        }
        match std::fs::remove_file(&alt) {
            Ok(()) => Ok(None),
            Err(_) => Ok(Some(alt)),
        }
    }
}

/// Obergrenze für einen Release-Asset-Download — ein Vielfaches der
/// tatsächlichen Binary-Größe, reine Absicherung gegen eine verunglückte
/// Antwort. [`UreqNetz`] begrenzt damit schon beim Lesen der Antwort, statt
/// erst danach zu prüfen.
const MAX_GROESSE: usize = 200 * 1024 * 1024;
/// Untergrenze — eine echte Binary ist um Größenordnungen größer; alles
/// darunter ist mit Sicherheit keine ausführbare Datei (z. B. eine
/// HTML-Fehlerseite).
const MIN_GROESSE: usize = 1024;

/// Orchestriert das gesamte Upgrade: Asset wählen, Ziel-Tag auflösen,
/// herunterladen, verifizieren, ersetzen.
///
/// Bei [`Entscheidung::Aktuell`] wird gar nicht erst heruntergeladen. Die
/// Verifikation läuft über `<tmp> --version`, BEVOR die laufende Binary
/// angetastet wird — schlägt sie fehl, bleibt die laufende Binary
/// unverändert und die temporäre Datei wird gelöscht.
pub fn fuehre_upgrade_aus(
    gewuenscht: Option<&str>,
    netz: &dyn Netz,
    eigener_pfad: &Path,
    mit_tui: bool,
    os: &str,
    arch: &str,
) -> Result<String, String> {
    let asset = asset_name(os, arch, mit_tui)?;
    let aktuell = env!("CARGO_PKG_VERSION");

    // Der neueste Release wird NUR abgefragt, wenn er die Entscheidung auch
    // trifft. Mit ausdrücklichem Wunsch steht das Ziel schon fest — und die
    // unauthentifizierte GitHub-API ist auf 60 Anfragen je Stunde und IP
    // gedeckelt; ein `agentkit --upgrade 1.2.3` soll daran nicht scheitern.
    // Nebeneffekt: eine unsinnige Versionsangabe fällt ohne jeden Netzaufruf auf.
    let neuester_tag = match gewuenscht {
        Some(_) => None,
        None => {
            eprintln!("Suche neueste Version …");
            let api_url = latest_api_url(REPO);
            // Struktureller Selbstschutz gegen eine künftige Änderung an
            // `latest_api_url`: ohne diese Domain würde `netz.text` nie aufgerufen.
            if !api_url.starts_with("https://api.github.com/") {
                return Err(format!(
                    "Interner Fehler: URL {api_url} zeigt nicht auf api.github.com."
                ));
            }
            let body = netz.text(&api_url)?;
            Some(
                tag_aus_release_json(&body)
                    .ok_or_else(|| "Antwort von GitHub enthält kein `tag_name`.".to_string())?,
            )
        }
    };

    let ziel_tag_str = match ziel_tag(aktuell, gewuenscht, neuester_tag.as_deref())? {
        Entscheidung::Aktuell => return Ok(format!("agentkit ist bereits aktuell (v{aktuell}).")),
        Entscheidung::Wechsel(tag) => tag,
    };

    eprintln!("Lade {asset} …");
    let url = download_url(REPO, &ziel_tag_str, asset);
    // Struktureller Selbstschutz gegen eine künftige Änderung an `download_url`:
    // ohne diese Domain würde `netz.bytes` nie aufgerufen.
    if !url.starts_with("https://github.com/") {
        return Err(format!(
            "Interner Fehler: URL {url} zeigt nicht auf github.com."
        ));
    }
    let bytes = netz.bytes(&url)?;
    if bytes.len() < MIN_GROESSE {
        return Err(format!(
            "Heruntergeladene Datei ist verdächtig klein ({} Bytes) — Abbruch.",
            bytes.len()
        ));
    }
    if bytes.len() > MAX_GROESSE {
        return Err(format!(
            "Heruntergeladene Datei ist zu groß ({} Bytes) — Abbruch.",
            bytes.len()
        ));
    }

    let eltern = eigener_pfad
        .parent()
        .ok_or_else(|| "Eigener Pfad hat kein Elternverzeichnis.".to_string())?;
    let tmp = eltern.join(format!(".agentkit-upgrade-{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("Temporäre Datei nicht schreibbar: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Wird für den `--version`-Aufruf unten gebraucht — ein Fehlschlag hier
        // ist kein best-effort-Fall, sondern ein harter Abbruch (die Datei wäre
        // sonst gar nicht ausführbar).
        let meta = std::fs::metadata(&tmp)
            .map_err(|e| format!("Berechtigungen von {} nicht lesbar: {e}", tmp.display()))?;
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)
            .map_err(|e| format!("Berechtigungen von {} nicht setzbar: {e}", tmp.display()))?;
    }

    eprintln!("Prüfe heruntergeladene Datei …");
    let erwartete_version = version_von_tag(&ziel_tag_str);
    let verifiziert = std::process::Command::new(&tmp)
        .arg("--version")
        .output()
        .is_ok_and(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.starts_with("agentkit ") && stdout.contains(erwartete_version)
        });
    if !verifiziert {
        std::fs::remove_file(&tmp).ok();
        return Err(format!(
            "Verifikation fehlgeschlagen (erwartet: `agentkit {erwartete_version}` in \
             `--version`). Laufende Binary unverändert."
        ));
    }

    eprintln!("Ersetze Binary …");
    let alt = ersetze_binary(eigener_pfad, &tmp).map_err(|e| {
        // Best-effort aufräumen — die temporäre Datei nützt nach einem
        // gescheiterten Tausch niemandem mehr.
        std::fs::remove_file(&tmp).ok();
        format!(
            "Ersetzen fehlgeschlagen: {e}. Die laufende Binary ist unverändert. \
             Bei fehlenden Rechten hilft eine erhöhte Rechte-Shell (`sudo` unter \
             Unix bzw. eine Administrator-Shell unter Windows)."
        )
    })?;

    let mut erfolg = format!("agentkit auf {ziel_tag_str} aktualisiert.");
    if let Some(alt_pfad) = alt {
        erfolg.push_str(&format!(
            " Die alte Datei liegt noch unter {} — kann manuell gelöscht werden.",
            alt_pfad.display()
        ));
    }
    Ok(erfolg)
}

/// Der echte HTTP-Adapter über `ureq` (synchron, wie überall in diesem Repo).
#[cfg(feature = "upgrade")]
pub struct UreqNetz;

#[cfg(feature = "upgrade")]
impl Netz for UreqNetz {
    fn text(&self, url: &str) -> Result<String, String> {
        let buf = self.begrenzt_lesen(url)?;
        String::from_utf8(buf).map_err(|e| format!("Antwort von {url} nicht lesbar: {e}"))
    }

    fn bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        self.begrenzt_lesen(url)
    }
}

#[cfg(feature = "upgrade")]
impl UreqNetz {
    /// GitHub verlangt einen `User-Agent`; die API würde die Anfrage sonst
    /// ablehnen.
    fn anfrage(&self, url: &str) -> Result<ureq::Response, String> {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .get(url)
            .set(
                "User-Agent",
                &format!("agentkit/{}", env!("CARGO_PKG_VERSION")),
            )
            .call()
            .map_err(|e| format!("Anfrage an {url} fehlgeschlagen: {e}"))
    }

    /// Liest die Antwort auf `url` als Bytes, aber begrenzt auf `MAX_GROESSE`
    /// (+1 Byte, um ein Überschreiten zu erkennen) — schützt vor einem
    /// unbegrenzten Download, statt die Größe erst NACH vollständigem
    /// Puffern zu prüfen.
    fn begrenzt_lesen(&self, url: &str) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let mut buf = Vec::new();
        self.anfrage(url)?
            .into_reader()
            .take(MAX_GROESSE as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("Antwort von {url} nicht lesbar: {e}"))?;
        if buf.len() > MAX_GROESSE {
            return Err(format!(
                "Antwort von {url} überschreitet die Größengrenze ({MAX_GROESSE} Bytes) — Abbruch."
            ));
        }
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ asset_name

    /// Alle vier unterstützten Linux/Windows-Kombinationen liefern den
    /// erwarteten, unversionierten Asset-Namen aus `release.yml`.
    #[test]
    fn asset_name_liefert_alle_vier_gueltigen_kombinationen() {
        assert_eq!(
            asset_name("linux", "x86_64", true),
            Ok("agentkit-linux-x86_64")
        );
        assert_eq!(
            asset_name("linux", "x86_64", false),
            Ok("agentkit-cli-linux-x86_64")
        );
        assert_eq!(
            asset_name("windows", "x86_64", true),
            Ok("agentkit-windows-x86_64.exe")
        );
        assert_eq!(
            asset_name("windows", "x86_64", false),
            Ok("agentkit-cli-windows-x86_64.exe")
        );
    }

    /// macOS hat kein Release-Asset — klare Absage statt eines falschen Downloads.
    #[test]
    fn asset_name_lehnt_macos_ab() {
        assert!(asset_name("macos", "x86_64", true).is_err());
    }

    /// aarch64 hat ebenfalls kein Release-Asset.
    #[test]
    fn asset_name_lehnt_aarch64_ab() {
        assert!(asset_name("linux", "aarch64", true).is_err());
    }

    // ------------------------------------------------------- normalisiere_tag

    #[test]
    fn normalisiere_tag_akzeptiert_mit_und_ohne_v() {
        assert_eq!(normalisiere_tag("1.2.3"), Ok("v1.2.3".to_string()));
        assert_eq!(normalisiere_tag("v1.2.3"), Ok("v1.2.3".to_string()));
    }

    #[test]
    fn normalisiere_tag_akzeptiert_suffix() {
        assert_eq!(
            normalisiere_tag("1.2.3-beta.1"),
            Ok("v1.2.3-beta.1".to_string())
        );
        assert_eq!(
            normalisiere_tag("v1.2.3-beta.1"),
            Ok("v1.2.3-beta.1".to_string())
        );
    }

    /// Ungültige Eingaben — insbesondere solche, die eine URL manipulieren
    /// könnten (`/`, `..`, Leerraum) — werden abgelehnt, nicht durchgelassen.
    #[test]
    fn normalisiere_tag_lehnt_ungueltiges_ab() {
        for eingabe in ["1.2", "abc", "../x", "1.2.3/../..", "1.2.3 --foo", ""] {
            assert!(
                normalisiere_tag(eingabe).is_err(),
                "sollte abgelehnt werden: {eingabe:?}"
            );
        }
    }

    // --------------------------------------------------- tag_aus_release_json

    #[test]
    fn tag_aus_release_json_liest_echtes_api_fragment() {
        let json = r#"{
            "url": "https://api.github.com/repos/rudi77/agentkit_rs/releases/123",
            "tag_name": "v0.23.0",
            "name": "v0.23.0",
            "draft": false,
            "prerelease": false,
            "assets": [{"name": "agentkit-linux-x86_64", "browser_download_url": "https://objects.githubusercontent.com/evil"}]
        }"#;
        assert_eq!(tag_aus_release_json(json), Some("v0.23.0".to_string()));
    }

    #[test]
    fn tag_aus_release_json_liefert_none_bei_kaputtem_json() {
        assert_eq!(tag_aus_release_json("{ das ist kein json"), None);
        assert_eq!(tag_aus_release_json(""), None);
        assert_eq!(tag_aus_release_json("{}"), None);
    }

    // ---------------------------------------------------------- ist_neuer

    #[test]
    fn ist_neuer_vergleicht_numerisch_nicht_lexikographisch() {
        assert!(ist_neuer("0.10.0", "0.9.0"));
        assert!(!ist_neuer("0.9.0", "0.10.0"));
        assert!(ist_neuer("1.2.3", "1.2.2"));
        assert!(!ist_neuer("1.2.3", "1.2.3"));
        assert!(!ist_neuer("abc", "1.2.3"));
    }

    // ------------------------------------------------------------- ziel_tag

    #[test]
    fn ziel_tag_ohne_wunsch_und_gleich_bleibt_aktuell() {
        assert_eq!(
            ziel_tag("1.2.3", None, Some("v1.2.3")),
            Ok(Entscheidung::Aktuell)
        );
    }

    #[test]
    fn ziel_tag_ohne_wunsch_und_neuer_wechselt() {
        assert_eq!(
            ziel_tag("1.2.3", None, Some("v1.3.0")),
            Ok(Entscheidung::Wechsel("v1.3.0".to_string()))
        );
    }

    /// Befund 3: ohne expliziten Wunsch darf ein ÄLTERER "neuester" Release
    /// (z. B. weil lokal ein Vorabstand wie `1.2.3-dev` läuft, der neuer ist
    /// als der letzte veröffentlichte Tag) nicht zu einem stillen Downgrade
    /// führen — `agentkit --upgrade` ohne Argument bleibt dann bei `Aktuell`.
    #[test]
    fn ziel_tag_ohne_wunsch_und_aelterer_neuester_bleibt_aktuell() {
        assert_eq!(
            ziel_tag("1.2.3", None, Some("v1.0.0")),
            Ok(Entscheidung::Aktuell)
        );
    }

    #[test]
    fn ziel_tag_mit_wunsch_gleich_aktueller_version_bleibt_aktuell() {
        assert_eq!(
            ziel_tag("1.2.3", Some("1.2.3"), Some("v1.9.0")),
            Ok(Entscheidung::Aktuell)
        );
        assert_eq!(
            ziel_tag("1.2.3", Some("v1.2.3"), Some("v1.9.0")),
            Ok(Entscheidung::Aktuell)
        );
    }

    /// Ein expliziter Wunsch ist bindend — auch ein Downgrade oder derselbe
    /// Stand wie der neueste Release, solange er von der LAUFENDEN Version
    /// abweicht.
    #[test]
    fn ziel_tag_mit_abweichendem_wunsch_wechselt_immer() {
        assert_eq!(
            ziel_tag("1.2.3", Some("1.0.0"), Some("v1.9.0")),
            Ok(Entscheidung::Wechsel("v1.0.0".to_string()))
        );
        assert_eq!(
            ziel_tag("1.2.3", Some("1.9.0"), Some("v1.9.0")),
            Ok(Entscheidung::Wechsel("v1.9.0".to_string()))
        );
        assert_eq!(
            ziel_tag("1.2.3", Some("9.0.0"), Some("v1.9.0")),
            Ok(Entscheidung::Wechsel("v9.0.0".to_string()))
        );
    }

    /// Der von GitHub gemeldete Tag ist KEINE vertrauenswürdige Eingabe: er
    /// landet im Pfad der Download-URL. Ein Tag mit Pfadanteilen würde die
    /// Domain-Prüfung passieren und trotzdem ein fremdes Asset laden — er muss
    /// deshalb genauso abgelehnt werden wie eine unsinnige Nutzereingabe.
    #[test]
    fn ziel_tag_lehnt_manipulierten_neuesten_tag_ab() {
        for boesartig in [
            "x/../../../fremdes/repo/releases/download/v1",
            "v1.2.3/../../..",
            "v1.2.3?token=abc",
            "latest",
        ] {
            let fehler = ziel_tag("1.2.3", None, Some(boesartig))
                .expect_err("manipulierter Tag muss abgelehnt werden");
            assert!(
                fehler.contains("unbrauchbare Release-Tag"),
                "unerwartete Meldung für {boesartig:?}: {fehler}"
            );
        }
    }

    /// Ohne Wunsch UND ohne abgefragten Tag gibt es nichts zu entscheiden —
    /// das ist ein Programmierfehler des Aufrufers, kein Nutzerfehler.
    #[test]
    fn ziel_tag_ohne_jede_quelle_ist_ein_interner_fehler() {
        let fehler = ziel_tag("1.2.3", None, None).expect_err("muss fehlschlagen");
        assert!(fehler.contains("Interner Fehler"), "Meldung: {fehler}");
    }

    // ------------------------------------------------ download_url/api_url

    #[test]
    fn download_url_und_latest_api_url_sind_https_github() {
        assert_eq!(
            download_url("rudi77/agentkit_rs", "v1.2.3", "agentkit-linux-x86_64"),
            "https://github.com/rudi77/agentkit_rs/releases/download/v1.2.3/agentkit-linux-x86_64"
        );
        assert_eq!(
            latest_api_url("rudi77/agentkit_rs"),
            "https://api.github.com/repos/rudi77/agentkit_rs/releases/latest"
        );
    }

    // ------------------------------------------------------------ ersetze_binary

    /// Eindeutiger Pfad je Aufruf (Prozess-ID + Zähler), damit parallele
    /// Testläufe sich nicht gegenseitig die Dateien wegziehen.
    fn tmp_pfad(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ZAEHLER: AtomicU32 = AtomicU32::new(0);
        let n = ZAEHLER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agentkit_upgrade_test_{}_{}_{}",
            std::process::id(),
            n,
            name
        ))
    }

    #[test]
    fn ersetze_binary_tauscht_den_inhalt_aus() {
        let ziel = tmp_pfad("ziel.bin");
        let tmp = tmp_pfad("neu.bin");
        std::fs::write(&ziel, b"alter inhalt").unwrap();
        std::fs::write(&tmp, b"neuer inhalt").unwrap();

        ersetze_binary(&ziel, &tmp).expect("Ersetzen darf nicht fehlschlagen");

        assert_eq!(std::fs::read(&ziel).unwrap(), b"neuer inhalt");
        assert!(!tmp.exists());

        std::fs::remove_file(&ziel).ok();
        // `ziel.alt` (Windows) best-effort mitentsorgen.
        std::fs::remove_file(alt_pfad(&ziel)).ok();
    }

    // --------------------------------------------------------- fuehre_upgrade_aus

    /// Testdouble für [`Netz`]: liefert feste Antworten, berührt kein Netz.
    struct FakeNetz {
        text: String,
        bytes: Vec<u8>,
    }

    impl Netz for FakeNetz {
        fn text(&self, _url: &str) -> Result<String, String> {
            Ok(self.text.clone())
        }
        fn bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(self.bytes.clone())
        }
    }

    /// Eine Antwort, die keine lauffähige Binary ist, muss die `--version`-
    /// Verifikation scheitern lassen — die laufende (hier: simulierte) Binary
    /// bleibt dabei unangetastet.
    #[test]
    fn fuehre_upgrade_aus_laesst_ziel_bei_fehlgeschlagener_verifikation_unangetastet() {
        let ziel = tmp_pfad("laufende.bin");
        std::fs::write(&ziel, b"unveraendert").unwrap();

        let netz = FakeNetz {
            text: r#"{"tag_name": "v9.9.9"}"#.to_string(),
            // Groß genug für die Mindestgröße, aber keine ausführbare Datei.
            bytes: vec![0u8; 2048],
        };

        let ergebnis = fuehre_upgrade_aus(None, &netz, &ziel, false, "linux", "x86_64");

        assert!(ergebnis.is_err());
        assert_eq!(std::fs::read(&ziel).unwrap(), b"unveraendert");

        std::fs::remove_dir_all(ziel.parent().unwrap())
            .ok()
            .or_else(|| std::fs::remove_file(&ziel).ok());
    }

    /// Ist die laufende Version bereits die neueste, wird gar nicht erst
    /// heruntergeladen — `FakeNetz::bytes` würde bei einem Aufruf sofort
    /// auffallen, weil sie hier absichtlich fehlerhaft wäre.
    #[test]
    fn fuehre_upgrade_aus_bricht_bei_aktueller_version_vor_dem_download_ab() {
        struct KeinDownloadNetz;
        impl Netz for KeinDownloadNetz {
            fn text(&self, _url: &str) -> Result<String, String> {
                Ok(format!(
                    r#"{{"tag_name": "v{}"}}"#,
                    env!("CARGO_PKG_VERSION")
                ))
            }
            fn bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
                Err("bytes() haette hier nicht aufgerufen werden duerfen".to_string())
            }
        }
        let ziel = tmp_pfad("aktuell.bin");
        std::fs::write(&ziel, b"unveraendert").unwrap();

        let ergebnis = fuehre_upgrade_aus(None, &KeinDownloadNetz, &ziel, false, "linux", "x86_64");

        assert!(ergebnis.unwrap().contains("bereits aktuell"));
        assert_eq!(std::fs::read(&ziel).unwrap(), b"unveraendert");
        std::fs::remove_file(&ziel).ok();
    }

    /// Eine unsinnige Versionsangabe muss auffallen, BEVOR das Netz befragt
    /// wird — `KeinNetz` meldet jeden Zugriff als Fehler, die erwartete
    /// Meldung ist also die der Versionsprüfung, nicht die des Netzes.
    #[test]
    fn fuehre_upgrade_aus_prueft_die_version_vor_jedem_netzaufruf() {
        struct KeinNetz;
        impl Netz for KeinNetz {
            fn text(&self, _url: &str) -> Result<String, String> {
                Err("text() haette hier nicht aufgerufen werden duerfen".to_string())
            }
            fn bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
                Err("bytes() haette hier nicht aufgerufen werden duerfen".to_string())
            }
        }
        let ziel = tmp_pfad("ungueltig.bin");
        std::fs::write(&ziel, b"unveraendert").unwrap();

        let fehler = fuehre_upgrade_aus(Some("1.2"), &KeinNetz, &ziel, false, "linux", "x86_64")
            .expect_err("ungültige Version muss abgelehnt werden");

        assert!(
            fehler.contains("Ungültige Versionsangabe"),
            "unerwartete Meldung: {fehler}"
        );
        assert_eq!(std::fs::read(&ziel).unwrap(), b"unveraendert");
        std::fs::remove_file(&ziel).ok();
    }

    /// Mit ausdrücklicher Zielversion steht das Ziel fest — die `latest`-
    /// Abfrage entfällt dann komplett (GitHubs unauthentifizierte API ist auf
    /// 60 Anfragen je Stunde gedeckelt). `text()` meldet hier jeden Aufruf als
    /// Fehler; dass stattdessen der Download-Pfad greift, beweist das.
    #[test]
    fn fuehre_upgrade_aus_fragt_bei_expliziter_version_nicht_nach_latest() {
        struct NurDownloadNetz;
        impl Netz for NurDownloadNetz {
            fn text(&self, _url: &str) -> Result<String, String> {
                Err("text() haette hier nicht aufgerufen werden duerfen".to_string())
            }
            fn bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
                // Absichtlich unter `MIN_GROESSE` — der Lauf soll genau hier
                // enden, ohne etwas zu ersetzen.
                Ok(vec![0u8; 10])
            }
        }
        let ziel = tmp_pfad("explizit.bin");
        std::fs::write(&ziel, b"unveraendert").unwrap();

        let fehler = fuehre_upgrade_aus(
            Some("9.9.9"),
            &NurDownloadNetz,
            &ziel,
            false,
            "linux",
            "x86_64",
        )
        .expect_err("zu kleine Antwort muss abgelehnt werden");

        assert!(
            fehler.contains("verdächtig klein"),
            "unerwartete Meldung: {fehler}"
        );
        assert_eq!(std::fs::read(&ziel).unwrap(), b"unveraendert");
        std::fs::remove_file(&ziel).ok();
    }
}
