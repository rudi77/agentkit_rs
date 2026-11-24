//! Dokumente in Abschnitte zerlegen — die Vorstufe zum Auswerten und Vergleichen.
//!
//! Ein großes Dokument passt nicht in ein Tool-Ergebnis, und ein Agent, der es
//! am Stück liest, kann keine Fundstelle nennen. `doc_ingest` legt es deshalb
//! einmal als Verzeichnis ab: eine Markdown-Datei je Abschnitt, davor
//! YAML-Frontmatter mit der Stelle in der Quelle, dazu eine `index.md` mit der
//! Gliederung. Das Layout ist ein OKF-Bundle (Open Knowledge Format v0.2) — ein
//! Verzeichnis aus Markdown, das `read_file`, `grep` und `glob_files` ohne
//! weiteres Werkzeug lesen.
//!
//! Die Zerlegung ist **deterministisch und verlustfrei**: kein Modell fasst
//! zusammen, der Text eines Abschnitts steht wörtlich da. Nur so trägt ein
//! Zitat daraus als Beleg (siehe `agentkit_graph::evidence`).
//!
//! Dieses Modul ist rein: es zerlegt Text und schreibt in ein Verzeichnis, das
//! der Aufrufer schon geprüft hat. Die Sandbox liegt in `coding.rs`.

use std::path::Path;

/// Größter Abschnitt in Zeichen. Muss samt Frontmatter unter
/// [`crate::memory::TRUNCATE_LIMIT`] bleiben — sonst sähe das Modell einen
/// Abschnitt nie ganz, und was hinter dem Schnitt steht, wäre unzitierbar.
pub const MAX_SECTION_CHARS: usize = 12_000;

/// Titel für Text, der vor der ersten Überschrift steht.
const PREAMBLE: &str = "Anfang";

/// Ein Abschnitt eines Dokuments, mit seiner Stelle in der Quelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    /// Die Überschriften darüber (`3 Haftung › 3.2 Umfang`); leer auf oberster
    /// Ebene. Ersetzt die Abschnitte, die nur aus einer Überschrift bestünden.
    pub parent: String,
    /// Wo der Abschnitt in der Quelle steht: `("pages", "12")` oder
    /// `("lines", "120-180")`.
    pub place: (&'static str, String),
    /// Der Text, wörtlich wie in der Quelle.
    pub text: String,
}

/// Zerlegt Markdown (oder schlichten Text) an seinen Überschriften.
///
/// Jede ATX-Überschrift (`#` bis `######`) beginnt einen Abschnitt und gehört
/// zu dessen Text. Eine Überschrift, auf die direkt eine tiefere folgt, ergibt
/// keinen eigenen Abschnitt: sie steht als `parent` an ihren Kindern.
/// Überschriften in Code-Blöcken zählen nicht.
pub fn split_markdown(text: &str) -> Vec<Section> {
    struct Open<'a> {
        level: usize,
        title: String,
        parent: String,
        start: usize,
        lines: Vec<&'a str>,
    }
    let mut closed: Vec<Open> = Vec::new();
    let mut ancestors: Vec<(usize, String)> = Vec::new();
    let mut open = Open {
        level: 0,
        title: PREAMBLE.to_string(),
        parent: String::new(),
        start: 1,
        lines: Vec::new(),
    };
    let mut fence: Option<&str> = None;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(mark) = fence {
            if trimmed.starts_with(mark) {
                fence = None;
            }
        } else if let Some(mark) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
            fence = Some(mark);
        } else if let Some((level, title)) = heading(line) {
            while ancestors.last().is_some_and(|(l, _)| *l >= level) {
                ancestors.pop();
            }
            let parent = ancestors
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join(" › ");
            ancestors.push((level, title.clone()));
            closed.push(std::mem::replace(
                &mut open,
                Open {
                    level,
                    title,
                    parent,
                    start: i + 1,
                    lines: Vec::new(),
                },
            ));
        }
        open.lines.push(line);
    }
    closed.push(open);

    let mut sections = Vec::new();
    for (i, part) in closed.iter().enumerate() {
        // Die Überschrift selbst ist Zeile 1 eines Abschnitts (nicht des Anfangs).
        let body = &part.lines[usize::from(part.level > 0).min(part.lines.len())..];
        let has_body = body.iter().any(|l| !l.trim().is_empty());
        let deeper_follows = closed.get(i + 1).is_some_and(|n| n.level > part.level);
        if !has_body && (part.level == 0 || deeper_follows) {
            continue;
        }
        let used = part
            .lines
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map_or(1, |p| p + 1);
        sections.push(Section {
            title: part.title.clone(),
            parent: part.parent.clone(),
            place: ("lines", format!("{}-{}", part.start, part.start + used - 1)),
            text: part.lines[..used].join("\n"),
        });
    }
    sections
}

/// Eine ATX-Überschrift: Ebene und Titel. Bis zu drei Leerzeichen Einrückung,
/// dann 1–6 `#` und ein Leerzeichen (CommonMark) — `#hashtag` ist keine.
fn heading(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = line.trim_start_matches(' ');
    let level = rest.len() - rest.trim_start_matches('#').len();
    if indent > 3 || !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.starts_with([' ', '\t']) {
        return None;
    }
    let title = after.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then(|| (level, title.to_string()))
}

/// Ein Abschnitt je Seite — die Seite ist bei einem PDF die Fundstelle, die
/// ein Mensch nachschlagen kann. Seiten ohne Text entfallen.
pub fn split_pages(pages: &[String]) -> Vec<Section> {
    pages
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(i, text)| Section {
            title: format!("Seite {}", i + 1),
            parent: String::new(),
            place: ("pages", (i + 1).to_string()),
            text: text.trim().to_string(),
        })
        .collect()
}

/// Teilt Abschnitte über [`MAX_SECTION_CHARS`] in mehrere (`… (Teil 2/3)`),
/// bevorzugt an Absätzen. Die Stelle in der Quelle bleibt die des Ganzen.
pub fn limit_sections(sections: Vec<Section>) -> Vec<Section> {
    let mut out = Vec::new();
    for section in sections {
        let parts = pack(&section.text, &["\n\n", "\n"]);
        let total = parts.len();
        if total == 1 {
            out.push(section);
            continue;
        }
        for (i, text) in parts.into_iter().enumerate() {
            out.push(Section {
                title: format!("{} (Teil {}/{total})", section.title, i + 1),
                text,
                ..section.clone()
            });
        }
    }
    out
}

/// Zerlegt `text` in Stücke von höchstens [`MAX_SECTION_CHARS`] Zeichen.
///
/// Erst in Einheiten — Absätze; ein zu großer Absatz in Zeilen; eine zu große
/// Zeile hart nach Zeichen —, dann werden die Einheiten der Reihe nach
/// aufgefüllt. So bleibt eine Überschrift bei dem Text, der ihr folgt, auch
/// wenn dieser allein schon zu groß ist.
fn pack(text: &str, separators: &[&'static str]) -> Vec<String> {
    if text.chars().count() <= MAX_SECTION_CHARS {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for (sep, unit) in units(text, separators) {
        let joined = current.chars().count() + sep.len() + unit.chars().count();
        if !current.is_empty() && joined > MAX_SECTION_CHARS {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(sep);
        }
        current.push_str(&unit);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Die Einheiten für [`pack`], jede höchstens [`MAX_SECTION_CHARS`] groß, mit
/// dem Trenner, der sie von ihrer Vorgängerin trennte.
fn units(text: &str, separators: &[&'static str]) -> Vec<(&'static str, String)> {
    let Some((sep, finer)) = separators.split_first() else {
        let chars: Vec<char> = text.chars().collect();
        return chars
            .chunks(MAX_SECTION_CHARS)
            .map(|c| ("", c.iter().collect()))
            .collect();
    };
    let mut out = Vec::new();
    for piece in text.split(sep) {
        if piece.chars().count() <= MAX_SECTION_CHARS {
            out.push((*sep, piece.to_string()));
            continue;
        }
        // Das erste Teilstück schließt mit dem groben Trenner an, die übrigen
        // mit ihrem eigenen.
        for (i, (inner, unit)) in units(piece, finer).into_iter().enumerate() {
            out.push((if i == 0 { *sep } else { inner }, unit));
        }
    }
    out
}

/// Schreibt die Abschnitte als OKF-Bundle nach `dir`: `index.md` mit der
/// Gliederung und je Abschnitt eine `NNN-<titel>.md`. `source` ist der Pfad der
/// Quelle, wie ihn der Agent kennt — er steht in jedem Frontmatter.
pub fn write_bundle(dir: &Path, source: &str, sections: &[Section]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let total = sections.len();
    let width = total.to_string().len().max(3);
    let mut index = format!(
        "---\nokf_version: '0.2'\n---\n\n# {source}\n\n{total} Abschnitte, {} Zeichen. \
         Jeder Abschnitt enthält den Text der Quelle wörtlich.\n\n",
        sections
            .iter()
            .map(|s| s.text.chars().count())
            .sum::<usize>()
    );
    for (i, section) in sections.iter().enumerate() {
        let name = format!("{:0width$}-{}.md", i + 1, slug(&section.title));
        let place = place_label(&section.place);
        let mut front = format!(
            "---\ntype: Dokumentabschnitt\ntitle: {}\ndescription: {}\nresource: {}\nagentkit:\n  order: {}\n  {}: {}\n",
            quoted(&section.title),
            quoted(&format!(
                "Abschnitt {} von {total} aus {source} ({place})",
                i + 1
            )),
            quoted(&source.replace(char::is_whitespace, "%20")),
            i + 1,
            section.place.0,
            quoted(&section.place.1),
        );
        if !section.parent.is_empty() {
            front.push_str(&format!("  parent: {}\n", quoted(&section.parent)));
        }
        std::fs::write(dir.join(&name), format!("{front}---\n\n{}\n", section.text))?;

        let mut entry = format!(
            "* [{}]({name}) - {place}, {} Zeichen",
            section.title.replace('[', "\\[").replace(']', "\\]"),
            section.text.chars().count()
        );
        if !section.parent.is_empty() {
            entry.push_str(&format!(", unter „{}“", section.parent));
        }
        // Eine Seite hat keinen sprechenden Titel — ihr Anfang ist der Hinweis,
        // was auf ihr steht.
        if section.place.0 == "pages" {
            let first = section.text.lines().find(|l| !l.trim().is_empty());
            let preview: String = first.unwrap_or("").trim().chars().take(80).collect();
            entry.push_str(&format!(", beginnt mit „{preview}“"));
        }
        index.push_str(&entry);
        index.push('\n');
    }
    std::fs::write(dir.join("index.md"), index)
}

fn place_label(place: &(&'static str, String)) -> String {
    match place.0 {
        "pages" => format!("Seite {}", place.1),
        _ => format!("Zeilen {}", place.1),
    }
}

/// Ein YAML-String in doppelten Anführungszeichen. JSON-Strings sind gültiges
/// YAML — so braucht ein Titel mit `:` oder `#` keine eigene Escape-Logik.
fn quoted(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

/// Dateiname aus einem Titel: nur `[a-z0-9-]`, Umlaute ausgeschrieben, höchstens
/// 40 Zeichen. Ein Titel ohne ein einziges brauchbares Zeichen wird `abschnitt`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.to_lowercase().chars() {
        match c {
            'a'..='z' | '0'..='9' => out.push(c),
            'ä' => out.push_str("ae"),
            'ö' => out.push_str("oe"),
            'ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            _ if !out.ends_with('-') && !out.is_empty() => out.push('-'),
            _ => {}
        }
    }
    let cut: String = out.chars().take(40).collect();
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "abschnitt".to_string()
    } else {
        cut.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_wird_an_ueberschriften_geteilt_und_behaelt_den_wortlaut() {
        let text = "Vorwort.\n\n# 1 Zweck\n\nDer Zweck.\n\n# 2 Haftung\n\n## 2.1 Umfang\n\nBis 5.000 EUR.\n";
        let sections = split_markdown(text);
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Anfang", "1 Zweck", "2.1 Umfang"]);
        assert_eq!(sections[1].text, "# 1 Zweck\n\nDer Zweck.");
        assert_eq!(sections[1].place, ("lines", "3-5".to_string()));
        // "2 Haftung" hat keinen eigenen Text — sie steht an ihrem Kind.
        assert_eq!(sections[2].parent, "2 Haftung");
        assert_eq!(sections[2].place, ("lines", "9-11".to_string()));
    }

    #[test]
    fn eine_leere_ueberschrift_ohne_kind_bleibt_als_abschnitt() {
        let sections = split_markdown("# A\n\n# B\n\nText.\n");
        assert_eq!(sections.len(), 2, "A hat kein Kind, das sie vertritt");
        assert_eq!(sections[0].text, "# A");
    }

    #[test]
    fn ueberschriften_im_codeblock_zaehlen_nicht() {
        let sections = split_markdown("# Echt\n\n```sh\n# nur ein Kommentar\n```\n\n#hashtag\n");
        assert_eq!(sections.len(), 1);
        assert!(sections[0].text.contains("# nur ein Kommentar"));
    }

    #[test]
    fn text_ohne_ueberschrift_ist_ein_abschnitt() {
        let sections = split_markdown("Nur Text.\nZweite Zeile.\n");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Anfang");
        assert_eq!(sections[0].place.1, "1-2");
    }

    #[test]
    fn seiten_ohne_text_entfallen_die_nummer_bleibt() {
        let pages = vec!["Eins".to_string(), "  \n".to_string(), "Drei".to_string()];
        let sections = split_pages(&pages);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].title, "Seite 3");
        assert_eq!(sections[1].place, ("pages", "3".to_string()));
    }

    #[test]
    fn ein_zu_grosser_abschnitt_wird_an_absaetzen_geteilt() {
        let paragraph = "x".repeat(5_000);
        let text = [paragraph.as_str(); 3].join("\n\n");
        let parts = limit_sections(vec![Section {
            title: "Lang".to_string(),
            parent: String::new(),
            place: ("lines", "1-5".to_string()),
            text: text.clone(),
        }]);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].title, "Lang (Teil 1/2)");
        assert!(parts
            .iter()
            .all(|p| p.text.chars().count() <= MAX_SECTION_CHARS));
        // Verlustfrei: wieder zusammengesetzt ergibt es den Ausgangstext.
        assert_eq!(format!("{}\n\n{}", parts[0].text, parts[1].text), text);
    }

    #[test]
    fn die_ueberschrift_bleibt_bei_ihrem_zu_grossen_absatz() {
        let zeilen = vec!["- ein Listenpunkt mit etwas Text"; 600].join("\n");
        let text = format!("## Liste\n\n{zeilen}");
        let parts = pack(&text, &["\n\n", "\n"]);
        assert!(parts.len() > 1);
        assert!(
            parts[0].starts_with("## Liste\n\n- ein"),
            "{}",
            &parts[0][..40]
        );
        // Verlustfrei: an den Zeilenumbrüchen wieder zusammengesetzt.
        assert_eq!(parts.join("\n"), text);
    }

    #[test]
    fn ein_absatz_ohne_jeden_umbruch_wird_hart_geteilt() {
        let parts = pack(&"ä".repeat(MAX_SECTION_CHARS + 10), &["\n\n", "\n"]);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].chars().count(), 10);
    }

    #[test]
    fn slug_ist_ein_sicherer_dateiname() {
        assert_eq!(slug("§ 3.2 Haftung & Gewähr"), "3-2-haftung-gewaehr");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("§§"), "abschnitt");
        assert!(slug(&"lang ".repeat(30)).len() <= 40);
    }

    #[test]
    fn das_bundle_traegt_gliederung_und_fundstelle() {
        let dir = std::env::temp_dir().join(format!("agentkit_docs_unit_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let sections = split_markdown("# 1 Zweck: \"Test\"\n\nDer Zweck.\n");
        write_bundle(&dir, "mein vertrag.md", &sections).unwrap();

        let index = std::fs::read_to_string(dir.join("index.md")).unwrap();
        assert!(
            index.starts_with("---\nokf_version: '0.2'\n---\n"),
            "{index}"
        );
        assert!(
            index.contains("(001-1-zweck-test.md) - Zeilen 1-3"),
            "{index}"
        );

        let section = std::fs::read_to_string(dir.join("001-1-zweck-test.md")).unwrap();
        assert!(
            section.contains("title: \"1 Zweck: \\\"Test\\\"\"\n"),
            "{section}"
        );
        // `resource` muss nach OKF §5.1 ohne Leerraum sein.
        assert!(
            section.contains("resource: \"mein%20vertrag.md\"\n"),
            "{section}"
        );
        assert!(section.contains("  lines: \"1-3\"\n"), "{section}");
        assert!(
            section.ends_with("---\n\n# 1 Zweck: \"Test\"\n\nDer Zweck.\n"),
            "{section}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
