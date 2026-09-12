//! Markdown-Ebene des OKF-Adapters: Frontmatter/Body-Trennung, Zusammenbau,
//! und die beiden Extraktionen, die der Referenz-Validator für Konsistenzchecks
//! braucht (Links, Footnote-Referenzen).

use super::yaml::{emit_document, YamlValue};

/// Trennt Frontmatter und Body — bewusst byte-gleich zur Referenzimplementierung
/// `okf_validate.py::split_frontmatter`: BOM wird entfernt, die erste Zeile muss
/// EXAKT `---` sein, der Block endet an der nächsten `---`-Zeile. Ein nicht
/// geschlossener Block gilt als „kein Frontmatter" (und ist beim Validator ein
/// Fehler, nicht hier — dieser Adapter liefert nur `None`).
///
/// Der Body beginnt unmittelbar nach der schließenden `---`-Zeile — inklusive
/// einer eventuell folgenden Leerzeile. [`compose`] fügt diese Leerzeile beim
/// Zusammenbau selbst ein; wer einen von hier stammenden Body erneut komponieren
/// will, muss die führende Leerzeile vorher entfernen (siehe Test unten).
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let first_line_end = text.find('\n').map(|i| i + 1).unwrap_or(text.len());
    let first_line = text[..first_line_end].trim_end_matches(['\r', '\n']);
    if first_line != "---" {
        return None;
    }
    let fm_start = first_line_end;
    let mut idx = fm_start;
    loop {
        if idx >= text.len() {
            return None; // nicht geschlossen
        }
        let line_end = text[idx..]
            .find('\n')
            .map(|i| idx + i + 1)
            .unwrap_or(text.len());
        let line = text[idx..line_end].trim_end_matches(['\r', '\n']);
        if line == "---" {
            let frontmatter = &text[fm_start..idx];
            let body = &text[line_end..];
            return Some((frontmatter, body));
        }
        idx = line_end;
    }
}

/// Setzt ein Dokument aus Frontmatter-Paaren und Body zusammen: `---\n` +
/// emittiertes YAML + `---\n\n` + Body. Der Body wird auf genau EINEN
/// abschließenden Zeilenumbruch normalisiert (überzählige werden entfernt,
/// ein fehlender wird ergänzt) — das ist die Gegenstelle zur führenden
/// Leerzeile, die [`split_frontmatter`] im Body belässt.
pub fn compose(pairs: &[(String, YamlValue)], body: &str) -> String {
    let yaml = emit_document(pairs);
    let trimmed_body = body.trim_end_matches('\n');
    format!("---\n{yaml}---\n\n{trimmed_body}\n")
}

/// Alle Markdown-Link-Ziele im Text: `(Titel, Ziel)`. Bilder (`![…]`) zählen
/// nicht, Inhalte von Code-Fences (``` `/` ~~~) werden übersprungen — genau wie
/// im Referenz-Validator, sonst meldet der Link-Check Treffer aus
/// Codebeispielen. Referenz-Style-Links (`[text][ref]`) werden bewusst NICHT
/// erkannt: OKF-Dokumente benutzen ausschließlich Inline-Links.
pub fn links(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if is_fence_marker(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        scan_links_in_line(line, &mut out);
    }
    out
}

/// Alle Footnote-Referenzen `[^label]` im Body, ohne die Definitionszeilen
/// (`[^label]: …`). Code-Fences werden aus demselben Grund wie bei [`links`]
/// übersprungen.
pub fn footnote_refs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if is_fence_marker(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        scan_footnote_refs_in_line(line, &mut out);
    }
    out
}

fn is_fence_marker(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn scan_links_in_line(line: &str, out: &mut Vec<(String, String)>) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '[' {
            let is_image = i > 0 && chars[i - 1] == '!';
            let mut j = i + 1;
            while j < n && chars[j] != ']' {
                j += 1;
            }
            if j >= n {
                break; // kein schließendes ']' mehr in dieser Zeile
            }
            if j + 1 < n && chars[j + 1] == '(' {
                let mut k = j + 2;
                while k < n && chars[k] != ')' {
                    k += 1;
                }
                if k < n {
                    if !is_image {
                        let title: String = chars[i + 1..j].iter().collect();
                        let target: String = chars[j + 2..k].iter().collect();
                        out.push((title, target));
                    }
                    i = k + 1;
                    continue;
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
}

fn scan_footnote_refs_in_line(line: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '[' && i + 1 < n && chars[i + 1] == '^' {
            let mut j = i + 2;
            while j < n && chars[j] != ']' {
                j += 1;
            }
            if j >= n {
                break;
            }
            let is_definition = j + 1 < n && chars[j + 1] == ':';
            if !is_definition {
                let label: String = chars[i + 2..j].iter().collect();
                if !label.is_empty() {
                    out.push(label);
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mit_bom() {
        let text = "\u{feff}---\nid: S-1\n---\n\nBody\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "id: S-1\n");
        assert_eq!(body, "\nBody\n");
    }

    #[test]
    fn split_ohne_bom() {
        let text = "---\nid: S-1\n---\n\nBody\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "id: S-1\n");
        assert_eq!(body, "\nBody\n");
    }

    #[test]
    fn split_ohne_frontmatter() {
        assert_eq!(split_frontmatter("Kein Frontmatter hier.\n"), None);
    }

    #[test]
    fn split_mit_unterminiertem_block() {
        assert_eq!(split_frontmatter("---\nid: S-1\nkein Ende hier\n"), None);
    }

    #[test]
    fn split_zerschneidet_body_mit_dreistrich_nicht() {
        let text = "---\nid: S-1\n---\n\nEin Abschnitt.\n\n---\n\nEin zweiter Abschnitt.\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "id: S-1\n");
        assert_eq!(body, "\nEin Abschnitt.\n\n---\n\nEin zweiter Abschnitt.\n");
    }

    #[test]
    fn compose_und_split_sind_zueinander_invers() {
        let pairs = vec![("id".to_string(), YamlValue::str("S-1"))];
        let body = "Ein Abschnitt.\nZweite Zeile.\n";
        let composed = compose(&pairs, body);

        let (fm_text, body_with_blank) = split_frontmatter(&composed).unwrap();
        let parsed = super::super::yaml::parse_document(fm_text).unwrap();
        assert_eq!(parsed, pairs);

        // compose fügt die Trennzeile selbst ein — split_frontmatter liefert sie
        // als Teil des Body zurück. Wer den Body erneut komponieren will, muss
        // genau diese eine führende Leerzeile abziehen; dann ist compose(split(x))
        // byte-identisch zu x.
        let body_stripped = body_with_blank.strip_prefix('\n').unwrap();
        assert_eq!(body_stripped, body);
        let recomposed = compose(&pairs, body_stripped);
        assert_eq!(recomposed, composed);
    }

    #[test]
    fn links_ignoriert_bilder_und_code_fences() {
        let body = "Text mit [einem Link](https://example.com/a) und ![Bild](https://example.com/b.png).\n\n```\n[Kein Link](in-code)\n```\n\n[Zweiter Link](./relativ.md)\n";
        let found = links(body);
        assert_eq!(
            found,
            vec![
                (
                    "einem Link".to_string(),
                    "https://example.com/a".to_string()
                ),
                ("Zweiter Link".to_string(), "./relativ.md".to_string()),
            ]
        );
    }

    #[test]
    fn footnote_refs_ohne_definitionszeilen() {
        let body = "Ein Satz mit Fußnote[^eins] und noch einer[^zwei].\n\n[^eins]: Erklärung eins.\n[^zwei]: Erklärung zwei.\n";
        let found = footnote_refs(body);
        assert_eq!(found, vec!["eins".to_string(), "zwei".to_string()]);
    }
}
