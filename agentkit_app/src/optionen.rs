//! Gemeinsame Options-Prüfung der Kommandozeilen dieses Crates.
//!
//! Drei Parser teilen sich das: der Hauptparser in `bin/agentkit.rs` (nur die
//! Meldung samt Vorschlag), das `viz`-Verb und `bin/tui.rs` (zusätzlich die
//! ganze Prüfung, weil beide ihre Optionen nur nachschlagen statt sie in einer
//! Schleife zu verbrauchen). Ohne die Prüfung liefe ein Tippfehler wie
//! `--no-swam` still als Lauf OHNE die verlangte Einstellung durch.

/// Die Meldung zu einer unbekannten Option — mit der nächstliegenden
/// bekannten, wenn der Abstand nach einem Tippfehler aussieht.
pub fn unbekannte_option(option: &str, bekannte: &[&[&str]]) -> String {
    let naechste = bekannte
        .iter()
        .flat_map(|liste| liste.iter())
        .map(|k| (edit_distanz(option, k), *k))
        .min()
        .filter(|(d, k)| k.len() > 2 && *d <= 2);
    match naechste {
        Some((_, k)) => format!("Unbekannte Option {option}. Meintest du {k}?"),
        None => format!("Unbekannte Option {option}."),
    }
}

/// Levenshtein-Abstand zweier kurzer Wörter (einfügen/löschen/ersetzen).
fn edit_distanz(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut zeile: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = zeile[0];
        zeile[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let ersetzen = diagonal + usize::from(ca != *cb);
            diagonal = zeile[j + 1];
            zeile[j + 1] = ersetzen.min(zeile[j] + 1).min(diagonal + 1);
        }
    }
    zeile[b.len()]
}

/// Prüft `argv` gegen die bekannten Optionen und liefert es normalisiert
/// zurück: `--flag=wert` als zwei Tokens, damit ein Nachschlagen nach
/// `--flag` den Wert dahinter findet.
///
/// `Err` bei einer unbekannten Option oder einem fehlenden Wert. Alles ab
/// `--` und jedes Token ohne führenden Strich (auch eine negative Zahl) geht
/// unverändert durch — was der Aufrufer damit anfängt, ist seine Sache.
pub fn pruefe_optionen(
    argv: &[String],
    mit_wert: &[&str],
    ohne_wert: &[&str],
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            out.push(arg.clone());
            out.extend(it.cloned());
            break;
        }
        let (name, wert) = match arg.split_once('=') {
            Some((k, v)) if arg.starts_with("--") => (k, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        if mit_wert.contains(&name) {
            let wert = wert
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("Option {name} braucht einen Wert."))?;
            out.push(name.to_string());
            out.push(wert);
        } else if ohne_wert.contains(&name) {
            if wert.is_some() {
                return Err(format!("Option {name} nimmt keinen Wert."));
            }
            out.push(arg.clone());
        } else if name.starts_with('-') && name.len() > 1 && name.parse::<f64>().is_err() {
            return Err(unbekannte_option(name, &[mit_wert, ohne_wert]));
        } else {
            out.push(arg.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIT: &[&str] = &["-w", "--workspace", "--port"];
    const OHNE: &[&str] = &["--open", "--no-swarm"];

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Ein Tippfehler bricht ab und nennt die gemeinte Option.
    #[test]
    fn unbekannte_option_wird_mit_vorschlag_abgelehnt() {
        let e = pruefe_optionen(&v(&["--no-swam"]), MIT, OHNE).unwrap_err();
        assert_eq!(e, "Unbekannte Option --no-swam. Meintest du --no-swarm?");
        let e = pruefe_optionen(&v(&["--voellig-anders"]), MIT, OHNE).unwrap_err();
        assert_eq!(e, "Unbekannte Option --voellig-anders.");
        // Auch in der `=`-Schreibweise zählt der Name, nicht das ganze Token.
        let e = pruefe_optionen(&v(&["--prot=1"]), MIT, OHNE).unwrap_err();
        assert!(e.contains("Meintest du --port?"), "{e}");
    }

    /// `--flag=wert` und `--flag wert` sind gleichwertig.
    #[test]
    fn gleichheitszeichen_wird_aufgespalten() {
        let erwartet = v(&["--port", "9", "--open", "-w", "/tmp"]);
        assert_eq!(
            pruefe_optionen(&v(&["--port=9", "--open", "-w", "/tmp"]), MIT, OHNE).unwrap(),
            erwartet
        );
        assert_eq!(
            pruefe_optionen(&erwartet, MIT, OHNE).unwrap(),
            erwartet,
            "die getrennte Schreibweise bleibt, wie sie ist"
        );
    }

    /// Ein Wert darf mit `-` beginnen — er ist Wert, keine Option.
    #[test]
    fn wert_wird_nicht_als_option_geprueft() {
        assert!(pruefe_optionen(&v(&["-w", "-komisch"]), MIT, OHNE).is_ok());
        let e = pruefe_optionen(&v(&["--open", "--port"]), MIT, OHNE).unwrap_err();
        assert_eq!(e, "Option --port braucht einen Wert.");
    }

    /// Hinter `--`, ohne Strich und als negative Zahl ist ein Token keine Option.
    #[test]
    fn positionale_tokens_gehen_durch() {
        let argv = v(&["text", "-5", "-", "--", "--unbekannt"]);
        assert_eq!(pruefe_optionen(&argv, MIT, OHNE).unwrap(), argv);
    }
}
