//! Wem ein Bundle gehört — die Grundlage dafür, ein FREMDES Bundle nicht
//! ungefragt zu lesen.
//!
//! Ein Bundle wird dem Modell als Wissen vorgelegt, in jeder Sitzung wieder.
//! Kommt es aus fremder Hand (ein geklontes Repository mit eingechecktem
//! Graphen), ist das ein dauerhafter Kanal für Prompt-Injection. Deshalb
//! zeichnet der Anleger sein Bundle: [`OWNER_FILE`] enthält eine Zufallszahl
//! und deren Prüfwert unter einem Schlüssel, den nur dieser Benutzer hat
//! (die Executable verwahrt ihn im Konfigurationsverzeichnis). Ein Bundle ohne
//! gültiges Zeichen gilt als unbestätigt; wer es annimmt, zeichnet es selbst.
//!
//! Das Zeichen hängt nicht am Pfad — ein verschobener Checkout bleibt
//! bestätigt — und nicht am Inhalt, denn der ändert sich mit jedem Commit.
//! Daraus folgt die Grenze: wer [`OWNER_FILE`] weitergibt, gibt das Zeichen
//! weiter. Deshalb entsteht mit ihr eine `.gitignore`, die das ganze Bundle
//! aus dem Repository heraushält.
//! Der Prüfwert ist FNV, kein kryptografischer MAC; er muss nur verhindern,
//! dass ein Fremder ein Zeichen ERFINDET, nicht dass er eines kopiert.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;

use crate::error::GraphError;
use crate::model::content_hash;
use crate::okf::bundle::write_file_atomic;

pub const OWNER_FILE: &str = "graph.owner";

/// Ein gezeichnetes Bundle hält sich aus dem Repository heraus, in dessen
/// Workspace es liegt: ein Gedächtnis, das ungefragt als „untracked" in jedem
/// `git status` auftaucht und in jeden Patch gerät, schaltet man ab. Wer das
/// Projektwissen bewusst einchecken will, ersetzt die Datei — dann aber ohne
/// die Laufzeit-Dateien, allen voran [`OWNER_FILE`].
const GITIGNORE: &str = "\
# agentkit: das Gedächtnis dieses Projekts. Standardmäßig nicht im Repository.
# Zum Einchecken diese Datei ersetzen durch:
#   graph.owner
#   graph.lock*
#   graph.stamp
#   *.tmp-*
*
";

/// 64 Bit aus der Zufallsquelle des Betriebssystems: `RandomState` wird von
/// der Standardbibliothek damit geimpft. Spart eine Dependency.
fn random_hex() -> String {
    format!("{:016x}", RandomState::new().build_hasher().finish())
}

/// Ein neuer Schlüssel für einen Benutzer.
pub fn new_key() -> String {
    format!(
        "{}{}{}{}",
        random_hex(),
        random_hex(),
        random_hex(),
        random_hex()
    )
}

fn mac(key: &str, nonce: &str) -> String {
    content_hash(&format!("{key}|{nonce}|{key}"))
}

/// Zeichnet das Bundle in `dir` mit `key`.
pub fn sign(dir: &Path, key: &str) -> Result<(), GraphError> {
    let nonce = random_hex();
    write_file_atomic(
        &dir.join(OWNER_FILE),
        &format!("{nonce} {}\n", mac(key, &nonce)),
    )?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        write_file_atomic(&ignore, GITIGNORE)?;
    }
    Ok(())
}

/// Trägt das Bundle in `dir` ein gültiges Zeichen unter `key`?
pub fn is_signed(dir: &Path, key: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(OWNER_FILE)) else {
        return false;
    };
    match text.split_whitespace().collect::<Vec<_>>().as_slice() {
        [nonce, found] => mac(key, nonce) == *found,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ein_zeichen_gilt_nur_unter_dem_schluessel_der_es_gesetzt_hat() {
        let dir = std::env::temp_dir().join(format!("graph_trust_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (meiner, fremder) = (new_key(), new_key());
        assert_ne!(meiner, fremder);

        assert!(!is_signed(&dir, &meiner), "ohne Datei unbestätigt");
        sign(&dir, &meiner).unwrap();
        assert!(is_signed(&dir, &meiner));
        assert!(!is_signed(&dir, &fremder));

        // Ein erfundenes Zeichen gilt nicht.
        std::fs::write(dir.join(OWNER_FILE), "0123456789abcdef 0123456789abcdef\n").unwrap();
        assert!(!is_signed(&dir, &meiner));

        // Das Bundle hält sich aus dem Repository heraus.
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l == "*"), "{ignore}");
        assert!(ignore.contains(OWNER_FILE), "nennt, was nie hineingehört");
        std::fs::remove_dir_all(&dir).ok();
    }
}
