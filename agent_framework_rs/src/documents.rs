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

/// Eine Zeile der Quelle für [`split_at_headings`]: ihr Text, ob sie eine
/// Überschrift ist (Ebene, Titel), und ihre Position — Zeilennummer oder Seite.
struct Line<'a> {
    text: &'a str,
    heading: Option<(usize, String)>,
    pos: usize,
}

/// Teilt Zeilen an ihren Überschriften. Gemeinsamer Kern für Markdown und PDF:
/// beide unterscheiden sich nur darin, WAS eine Überschrift ist und wie die
/// Fundstelle heißt (`place` bekommt die Position der ersten und der letzten
/// Zeile mit Text).
///
/// Jede Überschrift beginnt einen Abschnitt und gehört zu dessen Text. Eine
/// Überschrift, auf die direkt eine tiefere folgt, ergibt keinen eigenen
/// Abschnitt: sie steht als `parent` an ihren Kindern.
fn split_at_headings(
    lines: &[Line],
    place: impl Fn(usize, usize) -> (&'static str, String),
) -> Vec<Section> {
    struct Open<'a> {
        level: usize,
        title: String,
        parent: String,
        lines: Vec<&'a Line<'a>>,
    }
    let mut closed: Vec<Open> = Vec::new();
    let mut ancestors: Vec<(usize, &str)> = Vec::new();
    let mut open = Open {
        level: 0,
        title: PREAMBLE.to_string(),
        parent: String::new(),
        lines: Vec::new(),
    };
    for line in lines {
        if let Some((level, title)) = &line.heading {
            while ancestors.last().is_some_and(|(l, _)| l >= level) {
                ancestors.pop();
            }
            let parent = ancestors
                .iter()
                .map(|(_, t)| *t)
                .collect::<Vec<_>>()
                .join(" › ");
            ancestors.push((*level, title));
            closed.push(std::mem::replace(
                &mut open,
                Open {
                    level: *level,
                    title: title.clone(),
                    parent,
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
        let has_body = body.iter().any(|l| !l.text.trim().is_empty());
        let deeper_follows = closed.get(i + 1).is_some_and(|n| n.level > part.level);
        if !has_body && (part.level == 0 || deeper_follows) {
            continue;
        }
        let first = part.lines.iter().position(|l| !l.text.trim().is_empty());
        let last = part.lines.iter().rposition(|l| !l.text.trim().is_empty());
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        let used = &part.lines[first..=last];
        sections.push(Section {
            title: part.title.clone(),
            parent: part.parent.clone(),
            place: place(used[0].pos, used[used.len() - 1].pos),
            text: used.iter().map(|l| l.text).collect::<Vec<_>>().join("\n"),
        });
    }
    sections
}

/// Zerlegt Markdown (oder schlichten Text) an seinen ATX-Überschriften (`#`
/// bis `######`). Überschriften in Code-Blöcken zählen nicht.
pub fn split_markdown(text: &str) -> Vec<Section> {
    let mut fence: Option<&str> = None;
    let lines: Vec<Line> = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let trimmed = line.trim_start();
            let mut found = None;
            if let Some(mark) = fence {
                if trimmed.starts_with(mark) {
                    fence = None;
                }
            } else if let Some(mark) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
                fence = Some(mark);
            } else {
                found = heading(line);
            }
            Line {
                text: line,
                heading: found,
                pos: i + 1,
            }
        })
        .collect();
    split_at_headings(&lines, |from, to| ("lines", format!("{from}-{to}")))
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

/// Eine Textzeile eines PDFs mit dem, woran sich eine Überschrift erkennen
/// lässt: der Schriftgröße. Ein PDF kennt keine Gliederung, nur Zeichen an
/// Positionen.
#[derive(Debug, Clone, PartialEq)]
pub struct PageLine {
    /// 1-basiert.
    pub page: usize,
    /// Mittlere Schriftgröße der Zeile; `0.0` für eine Leerzeile.
    pub size: f64,
    pub text: String,
}

/// Ab diesem Faktor über der Schriftgröße des Fließtexts gilt eine Zeile als
/// Überschrift. Kleiner, und Hochstellungen oder leicht größere Tabellenköpfe
/// zählten mit.
const HEADING_SIZE_FACTOR: f64 = 1.15;

/// Länger ist keine Überschrift, sondern ein groß gesetzter Absatz.
const HEADING_MAX_CHARS: usize = 120;

/// Zerlegt die Zeilen eines PDFs an seinen Überschriften — die Seiten sind nur
/// noch die Fundstelle (`pages: "12-13"`), ein Abschnitt läuft über den
/// Seitenumbruch hinweg.
///
/// Als Überschrift gilt eine kurze Zeile, die **größer gesetzt** ist als der
/// Fließtext oder mit einer **Gliederungsnummer** beginnt (`§ 7`, `3.2 Titel`,
/// `Artikel 5`, siehe [`numbered_heading`]). Fett allein ist nicht erkennbar:
/// die Extraktion liefert Größe und Position, nicht den Schriftschnitt. Was
/// sich auf vielen Seiten wiederholt (Kopf- und Fußzeilen), ist keine
/// Überschrift — der Text bleibt aber stehen, es wird nichts entfernt.
///
/// Erkennt das nichts, bleibt es bei einem Abschnitt je Seite: besser eine
/// grobe Gliederung, die stimmt, als eine feine, die geraten ist.
pub fn split_pdf_lines(lines: &[PageLine]) -> Vec<Section> {
    let body = body_size(lines);
    let mut sizes: Vec<f64> = Vec::new();
    let larger = |line: &PageLine| line.size >= body * HEADING_SIZE_FACTOR;
    let candidate = |line: &PageLine| {
        let text = line.text.trim();
        (2..=HEADING_MAX_CHARS).contains(&text.chars().count())
            && text.chars().any(char::is_alphabetic)
            && !repeats_across_pages(lines, text)
    };
    for line in lines.iter().filter(|l| candidate(l) && larger(l)) {
        if !sizes.iter().any(|s| (s - line.size).abs() < 0.25) {
            sizes.push(line.size);
        }
    }
    sizes.sort_by(|a, b| b.total_cmp(a));

    let marked: Vec<Line> = lines
        .iter()
        .map(|line| {
            // Der Titel ohne die Mehrfach-Leerzeichen, die die Extraktion
            // zwischen Nummer und Text gern lässt; der Abschnittstext bleibt.
            let title = line.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let level = if !candidate(line) {
                None
            } else if larger(line) {
                sizes
                    .iter()
                    .position(|s| (s - line.size).abs() < 0.25)
                    .map(|p| p + 1)
            } else {
                numbered_heading(&title).map(|depth| sizes.len() + depth)
            };
            Line {
                text: &line.text,
                heading: level.map(|l| (l, title)),
                pos: line.page,
            }
        })
        .collect();
    if marked.iter().all(|l| l.heading.is_none()) {
        return split_by_page(lines);
    }
    split_at_headings(&marked, |from, to| {
        let pages = if from == to {
            from.to_string()
        } else {
            format!("{from}-{to}")
        };
        ("pages", pages)
    })
}

/// Die Schriftgröße des Fließtexts: die, in der die meisten Zeichen stehen
/// (auf halbe Punkt gerundet — dieselbe Schrift kommt nie bitgleich an).
fn body_size(lines: &[PageLine]) -> f64 {
    let mut weight: Vec<(i64, usize)> = Vec::new();
    for line in lines.iter().filter(|l| l.size > 0.0) {
        let key = (line.size * 2.0).round() as i64;
        let chars = line.text.chars().count();
        match weight.iter_mut().find(|(k, _)| *k == key) {
            Some((_, n)) => *n += chars,
            None => weight.push((key, chars)),
        }
    }
    weight
        .iter()
        .max_by_key(|(_, n)| *n)
        .map_or(0.0, |(k, _)| *k as f64 / 2.0)
}

/// Ist dieser Text eine Kopf- oder Fußzeile? Ja, wenn er (Ziffern außer Acht
/// gelassen, also auch „Seite 3 von 12") auf mindestens drei Seiten steht —
/// oder auf zweien, wenn das Dokument kaum mehr hat.
fn repeats_across_pages(lines: &[PageLine], text: &str) -> bool {
    let key = |t: &str| -> String { t.trim().chars().filter(|c| !c.is_ascii_digit()).collect() };
    let wanted = key(text);
    let mut pages: Vec<usize> = lines
        .iter()
        .filter(|l| key(&l.text) == wanted)
        .map(|l| l.page)
        .collect();
    pages.dedup();
    let total = lines.last().map_or(0, |l| l.page);
    pages.len() >= 3 || (pages.len() == 2 && total <= 4)
}

/// Beginnt die Zeile mit einer Gliederungsnummer, wie sie Verträge, Normen und
/// Handbücher verwenden? Liefert die Tiefe (`§ 7` → 1, `3.2.1 Titel` → 3).
///
/// Bewusst eng: eine einzelne Zahl (`1. Äpfel`) zählt nicht — das ist ebenso
/// oft ein Listenpunkt. Ein Satz ist auch keine Überschrift, also nichts, was
/// auf `.`, `,` oder `;` endet.
fn numbered_heading(text: &str) -> Option<usize> {
    static PATTERNS: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (named, decimal) = PATTERNS.get_or_init(|| {
        let build = |p: &str| regex::Regex::new(p).expect("festes Muster");
        (
            build(
                r"^(§\s?\d+[a-z]?|(Artikel|Art\.|Kapitel|Abschnitt|Teil|Anlage|Anhang|Ziffer|Article|Section|Chapter|Appendix|Annex)\s+(\d+|[IVXLC]+|[A-Z]))(\s|$)",
            ),
            build(r"^(\d+(?:\.\d+)+)\.?\s+\p{Lu}"),
        )
    });
    if text.ends_with(['.', ',', ';']) {
        return None;
    }
    if named.is_match(text) {
        return Some(1);
    }
    decimal.captures(text).map(|c| c[1].split('.').count())
}

/// Ein Abschnitt je Seite — der Rückfall für ein PDF ohne erkennbare
/// Gliederung. Der Titel nennt den Anfang der Seite, damit die Gliederung
/// zeigt, was auf ihr steht. Seiten ohne Text entfallen.
fn split_by_page(lines: &[PageLine]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut last_page = 0;
    for line in lines {
        if line.page != last_page {
            last_page = line.page;
            sections.push(Section {
                title: format!("Seite {}", line.page),
                parent: String::new(),
                place: ("pages", line.page.to_string()),
                text: String::new(),
            });
        }
        let section = sections.last_mut().expect("eben angelegt");
        section.text.push_str(&line.text);
        section.text.push('\n');
    }
    sections.retain_mut(|s| {
        s.text = s.text.trim().to_string();
        let first: String = s
            .text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        s.title = format!("{}: {}", s.title, first.trim());
        !s.text.is_empty()
    });
    sections
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

/// Eine Datei des Bundles: Name im Verzeichnis und Inhalt.
pub type BundleFile = (String, String);

/// Rendert die Abschnitte als OKF-Bundle: `index.md` mit der Gliederung und je
/// Abschnitt eine `NNN-<titel>.md`. `source` ist der Pfad der Quelle, wie ihn
/// der Agent kennt — er steht in jedem Frontmatter.
///
/// Getrennt vom Schreiben, weil [`bundle_state`] das Ergebnis mit einem schon
/// vorhandenen Bundle vergleicht: die Zerlegung ist deterministisch, also
/// heißen gleiche Dateien gleiche Quelle — ohne Zustandsdatei und ohne Hash.
pub fn render_bundle(source: &str, sections: &[Section]) -> Vec<BundleFile> {
    let total = sections.len();
    let width = total.to_string().len().max(3);
    let mut files = Vec::with_capacity(total + 1);
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
        let mut entry = format!(
            "* [{}]({name}) - {place}, {} Zeichen",
            section.title.replace('[', "\\[").replace(']', "\\]"),
            section.text.chars().count()
        );
        if !section.parent.is_empty() {
            entry.push_str(&format!(", unter „{}“", section.parent));
        }
        index.push_str(&entry);
        index.push('\n');
        files.push((name, format!("{front}---\n\n{}\n", section.text)));
    }
    files.push(("index.md".to_string(), index));
    files
}

/// Schreibt die Abschnitte als OKF-Bundle nach `dir` — [`render_bundle`] und
/// [`write_bundle_files`] in einem Schritt.
pub fn write_bundle(dir: &Path, source: &str, sections: &[Section]) -> std::io::Result<()> {
    write_bundle_files(dir, &render_bundle(source, sections))
}

/// Schreibt gerenderte Bundle-Dateien nach `dir` (legt es an).
///
/// Nur NEUE Dateien (`create_new`): liegt unter einem Bundle-Namen schon
/// etwas — auch ein Symlink, dem `create_new` nicht folgt —, ist das ein
/// Fehler statt eines Schreibens an dessen Ziel. Sonst könnte ein präpariertes
/// Repo mit `001-x.md -> ~/.bashrc` über `doc_ingest` ohne Freigabe Dateien
/// außerhalb des Workspace überschreiben. Der Aufrufer leert das Ziel vorher
/// ([`remove_bundle_files`]).
pub fn write_bundle_files(dir: &Path, files: &[BundleFile]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    for (name, content) in files {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))?
            .write_all(content.as_bytes())?;
    }
    Ok(())
}

/// Was in einem Zielverzeichnis schon liegt, verglichen mit dem, was
/// [`render_bundle`] jetzt schreiben würde.
#[derive(Debug, PartialEq, Eq)]
pub enum BundleState {
    /// Fehlt oder ist leer.
    Empty,
    /// Dieselben Bundle-Dateien mit demselben Inhalt: die Quelle ist unverändert.
    Unchanged,
    /// Ein Bundle derselben Quelle mit anderem Inhalt: die Quelle hat sich
    /// geändert — ersetzen.
    Stale,
    /// Etwas anderes: das Bundle einer anderen Quelle oder gar keines.
    Foreign,
}

/// Ordnet das Verzeichnis `dir` ein. Ein Bundle gehört zu `source`, wenn die
/// `# <quelle>`-Zeile seiner `index.md` nach [`normalize_source`] gleich ist —
/// so schreibt sie nur [`render_bundle`].
pub fn bundle_state(
    dir: &Path,
    source: &str,
    files: &[BundleFile],
) -> std::io::Result<BundleState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(BundleState::Empty);
    };
    let names = entries
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<String>>>()?;
    if names.is_empty() {
        return Ok(BundleState::Empty);
    }
    let Ok(index) = std::fs::read_to_string(dir.join("index.md")) else {
        return Ok(BundleState::Foreign);
    };
    if index_source(&index).map(normalize_source) != Some(normalize_source(source)) {
        return Ok(BundleState::Foreign);
    }
    let mut present: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| is_bundle_file(n))
        .collect();
    present.sort_unstable();
    let mut wanted: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    wanted.sort_unstable();
    // Die schon gelesene `index.md` zuerst: sie nennt Titel und Größe jedes
    // Abschnitts, verrät also fast jede Änderung, ohne eine Datei mehr zu lesen.
    let same = present == wanted
        && files
            .iter()
            .all(|(name, content)| name != "index.md" || *content == index)
        && files
            .iter()
            .filter(|(name, _)| name != "index.md")
            .all(|(name, content)| {
                std::fs::read(dir.join(name)).is_ok_and(|old| old == content.as_bytes())
            });
    Ok(if same {
        BundleState::Unchanged
    } else {
        BundleState::Stale
    })
}

/// Entfernt die Bundle-Dateien eines früheren Laufs aus `dir` — `index.md` und
/// `NNN-<titel>.md`, sonst nichts: was jemand daneben abgelegt hat, bleibt.
/// Ein Symlink unter einem dieser Namen wird als Link entfernt (sein Ziel
/// bleibt unberührt) — stehen bleiben darf er nicht, sonst schriebe das neue
/// Bundle durch ihn hindurch.
pub fn remove_bundle_files(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_bundle_file(&name) && !entry.file_type()?.is_dir() {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Der Pfad einer Quelle so, dass zwei Schreibweisen derselben Datei gleich
/// aussehen: `/` statt `\`, ohne führendes `./`.
pub fn normalize_source(source: &str) -> String {
    let mut s = source.trim().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    s
}

/// Die Quelle, die eine von [`render_bundle`] geschriebene `index.md` nennt:
/// die erste `# `-Zeile nach dem Frontmatter. Ohne Frontmatter ist es keine
/// solche `index.md` — eine eigene Datei mit passender Überschrift darf nicht
/// als Bundle gelten, sonst würden die Dateien daneben ersetzt.
fn index_source(index: &str) -> Option<&str> {
    let body = crate::skills::body_after_frontmatter(index);
    if body.len() == index.len() {
        return None;
    }
    body.lines().find_map(|l| l.strip_prefix("# "))
}

/// Gehört eine Datei zu einem Bundle? `index.md` oder `NNN-<slug>.md` — genau
/// die Namen, die [`render_bundle`] vergibt (mindestens drei Ziffern, der Slug
/// aus `[a-z0-9-]`).
fn is_bundle_file(name: &str) -> bool {
    if name == "index.md" {
        return true;
    }
    let Some((number, rest)) = name
        .strip_suffix(".md")
        .and_then(|stem| stem.split_once('-'))
    else {
        return false;
    };
    number.len() >= 3
        && number.bytes().all(|b| b.is_ascii_digit())
        && !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn place_label(place: &(&'static str, String)) -> String {
    match place.0 {
        "pages" if place.1.contains('-') => format!("Seiten {}", place.1),
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

    fn zeilen(spec: &[(usize, f64, &str)]) -> Vec<PageLine> {
        spec.iter()
            .map(|(page, size, text)| PageLine {
                page: *page,
                size: *size,
                text: text.to_string(),
            })
            .collect()
    }

    #[test]
    fn ein_pdf_wird_an_groesser_gesetzten_zeilen_geteilt_nicht_an_seiten() {
        let sections = split_pdf_lines(&zeilen(&[
            (1, 20.0, "Betriebshandbuch"),
            (1, 11.0, "Dieses Handbuch beschreibt den Betrieb."),
            (1, 15.0, "1 Sicherheit"),
            (1, 11.0, "Im Maschinenraum ist ein Helm zu tragen,"),
            (2, 11.0, "ebenso ein Gehörschutz."),
            (2, 15.0, "2 Wartung"),
            (2, 11.0, "Alle 6 Monate."),
        ]));
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Betriebshandbuch", "1 Sicherheit", "2 Wartung"]);
        // Der Abschnitt läuft über den Seitenumbruch und nennt beide Seiten.
        assert_eq!(sections[1].place, ("pages", "1-2".to_string()));
        assert!(sections[1]
            .text
            .ends_with("tragen,\nebenso ein Gehörschutz."));
        assert_eq!(sections[1].parent, "Betriebshandbuch");
        assert_eq!(sections[2].place, ("pages", "2".to_string()));
    }

    #[test]
    fn gliederungsnummern_teilen_auch_ohne_groessere_schrift() {
        let sections = split_pdf_lines(&zeilen(&[
            (1, 11.0, "Dienstleistungsvertrag"),
            (1, 11.0, "§ 1 Gegenstand"),
            (1, 11.0, "1. Der Auftragnehmer liefert."),
            (1, 11.0, "2. Der Auftraggeber zahlt."),
            (1, 11.0, "§ 2 Kündigung"),
            (1, 11.0, "Die Frist beträgt 3 Monate."),
            (1, 11.0, "2.1 Form"),
            (1, 11.0, "Schriftlich, siehe § 1 Gegenstand."),
        ]));
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Anfang", "§ 1 Gegenstand", "§ 2 Kündigung", "2.1 Form"]
        );
        // Listenpunkte und ein Verweis mitten im Satz sind keine Überschriften.
        assert!(sections[1].text.contains("2. Der Auftraggeber zahlt."));
        assert_eq!(sections[3].parent, "§ 2 Kündigung");
    }

    #[test]
    fn kopfzeilen_sind_keine_ueberschriften_bleiben_aber_im_text() {
        let mut spec = Vec::new();
        for page in 1..=3 {
            spec.push((page, 14.0, "Vertrag Muster GmbH"));
            spec.push((
                page,
                11.0,
                "Der Fließtext dieser Seite ist länger als Kopf- und Fußzeile zusammen.",
            ));
            spec.push((
                page,
                14.0,
                if page == 1 {
                    "Seite 1 von 3"
                } else if page == 2 {
                    "Seite 2 von 3"
                } else {
                    "Seite 3 von 3"
                },
            ));
        }
        spec.insert(2, (1, 14.0, "Einleitung"));
        let sections = split_pdf_lines(&zeilen(&spec));
        let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Anfang", "Einleitung"]);
        assert_eq!(sections[1].text.matches("Vertrag Muster GmbH").count(), 2);
    }

    #[test]
    fn ohne_erkennbare_gliederung_bleibt_es_bei_seiten() {
        let sections = split_pdf_lines(&zeilen(&[
            (1, 11.0, "Rechnung 2025-0042"),
            (1, 11.0, "Betrag: 100 EUR"),
            (2, 0.0, ""),
            (3, 11.0, "Anhang ohne Nummer"),
        ]));
        assert_eq!(sections.len(), 2, "die leere Seite entfällt");
        assert_eq!(sections[0].title, "Seite 1: Rechnung 2025-0042");
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
