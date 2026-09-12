//! Minimaler YAML-Adapter für OKF-Frontmatter — kein voller YAML-Parser, sondern
//! genau die Teilmenge, die OKF-Dokumente tatsächlich benutzen (Block- und
//! Flow-Mappings/-Sequenzen, plain/quotierte Skalare). Unbekannte Konstrukte
//! (Block-Skalare, Anker, Aliase, Tags, mehrzeiliger Flow) sind bewusst ein
//! harter Fehler statt eines stillen Best-Effort-Ergebnisses — ein Frontmatter-
//! Feld, das falsch gelesen wird, ist schwerer zu bemerken als ein Parse-Fehler.
//!
//! `emit_document`/`parse_document` sind zueinander invers für jeden Wert, den
//! der Emitter selbst erzeugt (siehe Roundtrip-Tests unten); ein Dokument, das
//! ein Mensch von Hand schreibt, darf zusätzlich die in den Doc-Kommentaren
//! genannten Varianten benutzen (z. B. Sequenzen auf Schlüsselspalte).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::GraphError;

/// Ein YAML-Wert nach dem Lesen bzw. vor dem Schreiben eines Frontmatter-Feldes.
///
/// `Map` ist bewusst ein `BTreeMap`: deterministische Schlüsselreihenfolge ⇒
/// byte-stabile Ausgabe (zwei Emit-Durchläufe desselben Werts sind identisch).
/// Die Feldreihenfolge des *Dokuments* (Top-Level) wird davon nicht berührt —
/// die trägt [`emit_document`] separat über den `&[(String, YamlValue)]`-Vektor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum YamlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<YamlValue>),
    Map(BTreeMap<String, YamlValue>),
}

impl YamlValue {
    pub fn str(s: impl Into<String>) -> Self {
        YamlValue::Str(s.into())
    }

    pub fn seq(items: impl IntoIterator<Item = YamlValue>) -> Self {
        YamlValue::Seq(items.into_iter().collect())
    }

    pub fn map(pairs: impl IntoIterator<Item = (String, YamlValue)>) -> Self {
        YamlValue::Map(pairs.into_iter().collect())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            YamlValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            YamlValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            YamlValue::Int(i) => u64::try_from(*i).ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            YamlValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            YamlValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[YamlValue]> {
        match self {
            YamlValue::Seq(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, YamlValue>> {
        match self {
            YamlValue::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Feldzugriff — nur für `Map`-Werte, sonst `None` (kein Panic).
    pub fn get(&self, key: &str) -> Option<&YamlValue> {
        self.as_map()?.get(key)
    }

    fn is_scalar(&self) -> bool {
        !matches!(self, YamlValue::Seq(_) | YamlValue::Map(_))
    }
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Rendert ein Frontmatter-Dokument als Block-Mapping in der übergebenen
/// Reihenfolge (nicht alphabetisch — die Feldreihenfolge ist Teil des Formats,
/// z. B. `id` vor `type` vor den fachlichen Feldern).
pub fn emit_document(pairs: &[(String, YamlValue)]) -> String {
    let mut out = String::new();
    emit_mapping_body(pairs, 0, &mut out);
    out
}

fn emit_mapping_body(pairs: &[(String, YamlValue)], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for (key, value) in pairs {
        out.push_str(&pad);
        out.push_str(&render_key(key));
        out.push(':');
        emit_value_after_key(value, indent, out);
    }
}

/// Rendert alles, was nach `key:` kommt — entweder auf derselben Zeile
/// (Skalar oder Flow-Container) oder als eingerückter Block ab der Folgezeile.
fn emit_value_after_key(value: &YamlValue, indent: usize, out: &mut String) {
    match value {
        YamlValue::Seq(items) => {
            if items.is_empty() {
                out.push_str(" []\n");
            } else if items.iter().all(YamlValue::is_scalar) {
                out.push(' ');
                emit_flow_seq(items, out);
                out.push('\n');
            } else {
                out.push('\n');
                emit_block_seq(items, indent + 2, out);
            }
        }
        YamlValue::Map(map) => {
            if map.is_empty() {
                out.push_str(" {}\n");
            } else if map.values().all(YamlValue::is_scalar) {
                out.push(' ');
                emit_flow_map(map, out);
                out.push('\n');
            } else {
                out.push('\n');
                let child_pairs: Vec<(String, YamlValue)> =
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                emit_mapping_body(&child_pairs, indent + 2, out);
            }
        }
        scalar => {
            out.push(' ');
            out.push_str(&render_scalar(scalar));
            out.push('\n');
        }
    }
}

/// Block-Sequenz: jedes Element beginnt mit `- ` auf `indent` Spalten. Ist das
/// Element selbst ein nicht-triviales Mapping, steht dessen erster Schlüssel
/// noch hinter dem `- `, alle weiteren Schlüssel werden um 2 Spalten
/// eingerückt (auf die Spalte des ersten Schlüssels) — das ist die einzige
/// Stelle, an der ein Block-Konstrukt nicht bei einem Vielfachen von 2 beginnt.
fn emit_block_seq(items: &[YamlValue], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for item in items {
        out.push_str(&pad);
        out.push_str("- ");
        match item {
            YamlValue::Map(map) if !map.is_empty() && !map.values().all(YamlValue::is_scalar) => {
                let child_pairs: Vec<(String, YamlValue)> =
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let (first_key, first_value) = &child_pairs[0];
                out.push_str(&render_key(first_key));
                out.push(':');
                emit_value_after_key(first_value, indent + 2, out);
                emit_mapping_body(&child_pairs[1..], indent + 2, out);
            }
            YamlValue::Map(map) => {
                if map.is_empty() {
                    out.push_str("{}\n");
                } else {
                    emit_flow_map(map, out);
                    out.push('\n');
                }
            }
            YamlValue::Seq(seq) => {
                if seq.is_empty() {
                    out.push_str("[]\n");
                } else if seq.iter().all(YamlValue::is_scalar) {
                    emit_flow_seq(seq, out);
                    out.push('\n');
                } else {
                    out.push('\n');
                    emit_block_seq(seq, indent + 2, out);
                }
            }
            scalar => {
                out.push_str(&render_scalar(scalar));
                out.push('\n');
            }
        }
    }
}

fn emit_flow_seq(items: &[YamlValue], out: &mut String) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&render_scalar_in(item, Context::Flow));
    }
    out.push(']');
}

fn emit_flow_map(map: &BTreeMap<String, YamlValue>, out: &mut String) {
    out.push_str("{ ");
    for (i, (key, value)) in map.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&render_key(key));
        out.push_str(": ");
        out.push_str(&render_scalar_in(value, Context::Flow));
    }
    out.push_str(" }");
}

/// Ein Skalar im Block-Kontext (`key: wert`, `- wert`).
fn render_scalar(value: &YamlValue) -> String {
    render_scalar_in(value, Context::Block)
}

/// Wo ein Skalar steht, entscheidet mit, was plain sein darf: innerhalb von
/// `[…]`/`{…}` beenden `,`, `]` und `}` den Wert. Ohne diese Unterscheidung
/// würde ein Titel wie `Fehler in a, b` als ZWEI Listenelemente zurückgelesen —
/// ein stiller Datenverlust, den erst der nächste Leser bemerkt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Block,
    Flow,
}

fn render_scalar_in(value: &YamlValue, context: Context) -> String {
    match value {
        YamlValue::Null => "null".to_string(),
        YamlValue::Bool(b) => b.to_string(),
        YamlValue::Int(i) => i.to_string(),
        YamlValue::Float(f) => render_float(*f),
        YamlValue::Str(s) => render_str(s, context),
        YamlValue::Seq(_) | YamlValue::Map(_) => {
            unreachable!("render_scalar wird nur auf Skalare angewendet")
        }
    }
}

/// Ein Mapping-Schlüssel. Quotiert nach denselben Regeln wie ein Flow-Skalar,
/// zusätzlich bei jedem `:` — ein Schlüssel `a:b` würde unquotiert an der
/// falschen Stelle getrennt.
///
/// Betrifft in der Praxis nur Schlüssel aus `GraphEntity::extra`: die stammen
/// aus handgeschriebenem Frontmatter und sind damit beliebig. Die eigenen
/// Schlüssel sind alle plain — sie laufen unverändert durch.
fn render_key(key: &str) -> String {
    if key.contains(':') {
        return render_single_quoted(key);
    }
    render_str(key, Context::Flow)
}

/// Rust zeigt `3.0_f64` als `"3"` an (kein `.0`) — das würde ein Leser als
/// `Int` re-typisieren. Deshalb wird ein `.0` erzwungen, wenn die Darstellung
/// keinen Dezimalpunkt (und keine Exponentialschreibweise) enthält.
fn render_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let s = f.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

const PLAIN_UNSAFE_FIRST: &[char] = &[
    '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`',
];

/// Plain, wenn der String beim Lesen eindeutig als String und nicht als
/// Struktur-Zeichen missverstanden werden kann — sonst quotiert. Die Reihenfolge
/// der Prüfungen folgt exakt der Spezifikation, keine zusätzliche Heuristik.
fn is_plain_safe(s: &str, context: Context) -> bool {
    if s.is_empty() || s.trim() != s {
        return false;
    }
    // Innerhalb von `[…]`/`{…}` sind diese Zeichen Struktur, nicht Inhalt.
    if context == Context::Flow && s.contains([',', '[', ']', '{', '}']) {
        return false;
    }
    let first = s.chars().next().expect("s ist nicht leer");
    if PLAIN_UNSAFE_FIRST.contains(&first) {
        return false;
    }
    if s.contains(": ") || s.contains(" #") || s.contains('\n') {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
    ) {
        return false;
    }
    if s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok() {
        return false;
    }
    true
}

fn needs_double_quote(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

fn render_str(s: &str, context: Context) -> String {
    if is_plain_safe(s, context) {
        s.to_string()
    } else if needs_double_quote(s) {
        render_double_quoted(s)
    } else {
        render_single_quoted(s)
    }
}

fn render_single_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn render_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Eine vorverarbeitete Zeile: Kommentar abgeschnitten, rechts getrimmt,
/// Leerzeilen und reine Kommentarzeilen bereits herausgefiltert. Trägt die
/// physische Zeilennummer für Fehlermeldungen.
struct Line<'a> {
    no: usize,
    indent: usize,
    content: &'a str,
}

pub fn parse_document(text: &str) -> Result<Vec<(String, YamlValue)>, GraphError> {
    let lines = preprocess(text)?;
    let mut pos = 0;
    let pairs = parse_block_mapping(&lines, &mut pos, 0)?;
    if pos != lines.len() {
        let line = &lines[pos];
        return Err(GraphError::Okf(format!(
            "Zeile {}: unerwartete Einrückung",
            line.no
        )));
    }
    Ok(pairs)
}

fn preprocess(text: &str) -> Result<Vec<Line<'_>>, GraphError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        if raw.contains('\t') {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: Tabs sind nicht erlaubt"
            )));
        }
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let after_indent = &raw[indent..];
        let stripped = strip_comment(after_indent);
        let content = stripped.trim_end();
        if content.trim().is_empty() {
            continue;
        }
        if content == "---" || content == "..." {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: Dokument-Trenner im Text werden nicht unterstützt"
            )));
        }
        out.push(Line {
            no: line_no,
            indent,
            content,
        });
    }
    Ok(out)
}

/// Schneidet einen Zeilenend-Kommentar ab (`#…` außerhalb von Quotes). Beachtet
/// `'…'`-Verdopplung und `\`-Escapes in `"…"`, damit ein `#` innerhalb eines
/// Strings nicht fälschlich als Kommentarstart gilt.
fn strip_comment(s: &str) -> &str {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let (idx, c) = chars[i];
        if in_double {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if in_single {
            if c == '\'' {
                if i + 1 < chars.len() && chars[i + 1].1 == '\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_double = true,
            '\'' => in_single = true,
            '#' => {
                let prev_is_space = i == 0 || chars[i - 1].1.is_whitespace();
                if prev_is_space {
                    return &s[..idx];
                }
            }
            _ => {}
        }
        i += 1;
    }
    s
}

fn parse_block_mapping<'a>(
    lines: &[Line<'a>],
    pos: &mut usize,
    indent: usize,
) -> Result<Vec<(String, YamlValue)>, GraphError> {
    let mut out = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();
    while *pos < lines.len() {
        let line = &lines[*pos];
        if line.indent < indent {
            break;
        }
        if line.indent > indent {
            return Err(GraphError::Okf(format!(
                "Zeile {}: unerwartete Einrückung",
                line.no
            )));
        }
        let content = line.content;
        if content == "-" || content.starts_with("- ") {
            return Err(GraphError::Okf(format!(
                "Zeile {}: Sequenz-Element außerhalb einer Sequenz",
                line.no
            )));
        }
        let (key, rest) = split_key(content, line.no)?;
        if !seen_keys.insert(key.clone()) {
            return Err(GraphError::Okf(format!(
                "Zeile {}: doppelter Schlüssel '{key}'",
                line.no
            )));
        }
        let key_line_no = line.no;
        *pos += 1;
        let value = parse_value_after_key(lines, pos, indent, rest, key_line_no)?;
        out.push((key, value));
    }
    Ok(out)
}

/// Sucht das erste `:`, das entweder das Zeilenende erreicht oder ein
/// Leerzeichen dahinter hat — das ist die Schlüssel/Wert-Grenze. Ein Flow-Wert
/// hinter dem Schlüssel (`resource: 'x://y'`) enthält selbst Doppelpunkte;
/// deshalb zählt nur der ERSTE Treffer, der Schlüssel steht immer vorn.
fn try_split_key(content: &str) -> Option<(&str, &str)> {
    if content.starts_with('[') || content.starts_with('{') {
        return None;
    }
    // Quotierter Schlüssel (`'a:b': 1`). Der Doppelpunkt INNERHALB der Quotes
    // gehört zum Schlüssel — deshalb erst ans schließende Quote springen und
    // erst dahinter nach dem Trenner suchen. Steht dort keiner, ist die Zeile
    // ein quotierter Skalar (ein Sequenz-Element) und kein Paar.
    if let Some(quote) = content.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let end = quoted_scalar_end(content, quote)?;
        let after = content[end..].trim_start().strip_prefix(':')?;
        if after.is_empty() || after.starts_with(' ') {
            return Some((&content[..end], after));
        }
        return None;
    }
    for (i, c) in content.char_indices() {
        if c == ':' {
            let next = content[i + 1..].chars().next();
            if next.is_none() || next == Some(' ') {
                return Some((&content[..i], &content[i + 1..]));
            }
        }
    }
    None
}

/// Byte-Index direkt HINTER dem schließenden Quote, `None` wenn ungeschlossen.
/// Berücksichtigt beide Escape-Formen: `''` in einfachen, `\x` in doppelten
/// Quotes.
fn quoted_scalar_end(content: &str, quote: char) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if quote == '"' && c == '\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if quote == '\'' && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn split_key(content: &str, line_no: usize) -> Result<(String, &str), GraphError> {
    let (raw, rest) = try_split_key(content).ok_or_else(|| {
        GraphError::Okf(format!(
            "Zeile {line_no}: kein ':' als Schlüsseltrenner gefunden"
        ))
    })?;
    let raw = raw.trim();
    // Ein quotierter Schlüssel wird über denselben Skalar-Pfad wie ein Wert
    // entpackt — sonst behielte er seine Quotes und wäre ein anderer Schlüssel
    // als der, der geschrieben wurde.
    let key = match raw.chars().next() {
        Some('\'') | Some('"') => parse_scalar_or_flow(raw, line_no)?
            .as_str()
            .unwrap_or_default()
            .to_string(),
        _ => raw.to_string(),
    };
    if key.is_empty() {
        return Err(GraphError::Okf(format!(
            "Zeile {line_no}: leerer Schlüssel"
        )));
    }
    Ok((key, rest.trim()))
}

/// Entscheidet, wie der Wert hinter einem Schlüssel weitergeht: inline
/// (Skalar/Flow auf derselben Zeile), als eingerückte Block-Sequenz — auch auf
/// Schlüsselspalte, das schreiben Menschen oft so —, als eingerücktes
/// Block-Mapping, oder `Null`, wenn nichts folgt.
fn parse_value_after_key<'a>(
    lines: &[Line<'a>],
    pos: &mut usize,
    key_indent: usize,
    rest: &'a str,
    key_line_no: usize,
) -> Result<YamlValue, GraphError> {
    if !rest.is_empty() {
        return parse_scalar_or_flow(rest, key_line_no);
    }
    match lines.get(*pos) {
        None => Ok(YamlValue::Null),
        Some(next) if next.indent < key_indent => Ok(YamlValue::Null),
        Some(next) if next.content == "-" || next.content.starts_with("- ") => {
            let seq_indent = next.indent;
            Ok(YamlValue::Seq(parse_block_seq(lines, pos, seq_indent)?))
        }
        Some(next) if next.indent > key_indent => {
            let map_indent = next.indent;
            let pairs = parse_block_mapping(lines, pos, map_indent)?;
            Ok(YamlValue::Map(pairs.into_iter().collect()))
        }
        Some(_) => Ok(YamlValue::Null),
    }
}

fn parse_block_seq<'a>(
    lines: &[Line<'a>],
    pos: &mut usize,
    indent: usize,
) -> Result<Vec<YamlValue>, GraphError> {
    let mut out = Vec::new();
    while *pos < lines.len() {
        let line = &lines[*pos];
        if line.indent != indent {
            break;
        }
        let content = line.content;
        if !(content == "-" || content.starts_with("- ")) {
            break;
        }
        let item_rest = if content == "-" { "" } else { &content[2..] };
        let item_line_no = line.no;
        *pos += 1;
        if item_rest.trim().is_empty() {
            match lines.get(*pos) {
                Some(next)
                    if next.indent > indent
                        && (next.content == "-" || next.content.starts_with("- ")) =>
                {
                    let seq_indent = next.indent;
                    out.push(YamlValue::Seq(parse_block_seq(lines, pos, seq_indent)?));
                }
                Some(next) if next.indent > indent => {
                    let map_indent = next.indent;
                    let pairs = parse_block_mapping(lines, pos, map_indent)?;
                    out.push(YamlValue::Map(pairs.into_iter().collect()));
                }
                _ => out.push(YamlValue::Null),
            }
        } else {
            let trimmed = item_rest.trim_start();
            let leading_spaces = item_rest.len() - trimmed.len();
            let item_col = indent + 2 + leading_spaces;
            if let Some((first_key, first_rest)) = try_split_key(trimmed) {
                let first_key = first_key.trim().to_string();
                if first_key.is_empty() {
                    return Err(GraphError::Okf(format!(
                        "Zeile {item_line_no}: leerer Schlüssel"
                    )));
                }
                let first_value =
                    parse_value_after_key(lines, pos, item_col, first_rest.trim(), item_line_no)?;
                let mut pairs = vec![(first_key, first_value)];
                pairs.extend(parse_block_mapping(lines, pos, item_col)?);
                out.push(YamlValue::Map(pairs.into_iter().collect()));
            } else {
                out.push(parse_scalar_or_flow(trimmed, item_line_no)?);
            }
        }
    }
    Ok(out)
}

fn parse_scalar_or_flow(s: &str, line_no: usize) -> Result<YamlValue, GraphError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(YamlValue::Null);
    }
    if let Some(first) = trimmed.chars().next() {
        let unsupported = match first {
            '|' | '>' => Some("Block-Skalare ('|'/'>')"),
            '&' => Some("Anker ('&')"),
            '*' => Some("Aliase ('*')"),
            '!' => Some("Tags ('!')"),
            _ => None,
        };
        if let Some(what) = unsupported {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: {what} werden nicht unterstützt"
            )));
        }
    }
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        let mut fp = FlowParser::new(trimmed, line_no);
        let value = fp.parse_value()?;
        fp.skip_ws();
        if fp.pos != fp.chars.len() {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: unerwarteter Inhalt nach Flow-Wert"
            )));
        }
        return Ok(value);
    }
    if trimmed.starts_with('\'') {
        let mut fp = FlowParser::new(trimmed, line_no);
        let value = fp.parse_single_quoted()?;
        fp.skip_ws();
        if fp.pos != fp.chars.len() {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: unerwarteter Inhalt nach quotierter Zeichenkette"
            )));
        }
        return Ok(YamlValue::Str(value));
    }
    if trimmed.starts_with('"') {
        let mut fp = FlowParser::new(trimmed, line_no);
        let value = fp.parse_double_quoted()?;
        fp.skip_ws();
        if fp.pos != fp.chars.len() {
            return Err(GraphError::Okf(format!(
                "Zeile {line_no}: unerwarteter Inhalt nach quotierter Zeichenkette"
            )));
        }
        return Ok(YamlValue::Str(value));
    }
    Ok(typed_scalar_from_plain(trimmed))
}

fn typed_scalar_from_plain(s: &str) -> YamlValue {
    if s.is_empty() || s == "~" || s.eq_ignore_ascii_case("null") {
        return YamlValue::Null;
    }
    if s.eq_ignore_ascii_case("true") {
        return YamlValue::Bool(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return YamlValue::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return YamlValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return YamlValue::Float(f);
    }
    YamlValue::Str(s.to_string())
}

fn multiline_flow_error(line_no: usize) -> GraphError {
    GraphError::Okf(format!(
        "Zeile {line_no}: mehrzeilige Flow-Notation wird nicht unterstützt"
    ))
}

/// Parser für eine einzeilige Flow-Notation (`[..]`/`{..}`, auch verschachtelt).
/// Läuft der Text aus, bevor eine öffnende Klammer geschlossen wurde, ist das
/// per Definition der Fall, den eine echte YAML-Implementierung als
/// „Flow über mehrere Zeilen" lesen würde — das unterstützt dieser Adapter
/// nicht, also ist genau das der Fehlerfall [`multiline_flow_error`].
struct FlowParser {
    chars: Vec<char>,
    pos: usize,
    line_no: usize,
    depth: usize,
}

/// Wie tief `[`/`{` ineinander stehen dürfen.
///
/// Jede Klammer kostet ein Rekursionspaar, und ein überlaufener Stack ist in
/// Rust **kein** abfangbarer Panic, sondern ein sofortiger Prozessabbruch über
/// die Guard Page — ohne Meldung, ohne Log, ohne Trace-Eintrag. Eine 6 KB
/// große Datei mit 3000 `[` reicht dafür. Da sie im Bundle liegen bleibt,
/// stirbt danach jeder weitere Start und jeder Viewer-Abruf erneut, bis
/// jemand sie von Hand löscht.
///
/// 64 ist bewusst weit über allem, was OKF je braucht (`sources[]`-Einträge
/// sind zwei Ebenen tief), und weit unter jeder Stack-Grenze.
const MAX_FLOW_DEPTH: usize = 64;

impl FlowParser {
    fn new(s: &str, line_no: usize) -> Self {
        FlowParser {
            chars: s.chars().collect(),
            pos: 0,
            depth: 0,
            line_no,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek() == Some(' ') {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: char) -> Result<(), GraphError> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(GraphError::Okf(format!(
                "Zeile {}: '{c}' erwartet",
                self.line_no
            )))
        }
    }

    /// Betritt eine Klammerebene. Siehe [`MAX_FLOW_DEPTH`].
    fn enter(&mut self) -> Result<(), GraphError> {
        self.depth += 1;
        if self.depth > MAX_FLOW_DEPTH {
            return Err(GraphError::Okf(format!(
                "Zeile {}: Flow-Notation ist tiefer als {MAX_FLOW_DEPTH} Ebenen verschachtelt",
                self.line_no
            )));
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<YamlValue, GraphError> {
        self.skip_ws();
        match self.peek() {
            Some('[') => self.parse_seq(),
            Some('{') => self.parse_map(),
            Some('\'') => self.parse_single_quoted().map(YamlValue::Str),
            Some('"') => self.parse_double_quoted().map(YamlValue::Str),
            Some(_) => self.parse_plain_scalar(),
            None => Err(multiline_flow_error(self.line_no)),
        }
    }

    fn parse_seq(&mut self) -> Result<YamlValue, GraphError> {
        self.enter()?;
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(YamlValue::Seq(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(']') {
                        self.pos += 1;
                        break;
                    }
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(multiline_flow_error(self.line_no)),
            }
        }
        self.depth -= 1;
        Ok(YamlValue::Seq(items))
    }

    fn parse_map(&mut self) -> Result<YamlValue, GraphError> {
        self.enter()?;
        self.expect('{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(YamlValue::Map(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_flow_key()?;
            self.skip_ws();
            self.expect(':')?;
            self.skip_ws();
            let value = self.parse_value()?;
            if map.insert(key.clone(), value).is_some() {
                return Err(GraphError::Okf(format!(
                    "Zeile {}: doppelter Schlüssel '{key}' in Flow-Mapping",
                    self.line_no
                )));
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some('}') {
                        self.pos += 1;
                        break;
                    }
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(multiline_flow_error(self.line_no)),
            }
        }
        self.depth -= 1;
        Ok(YamlValue::Map(map))
    }

    fn parse_flow_key(&mut self) -> Result<String, GraphError> {
        match self.peek() {
            Some('\'') => self.parse_single_quoted(),
            Some('"') => self.parse_double_quoted(),
            _ => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c == ':' {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(multiline_flow_error(self.line_no));
                }
                Ok(self.chars[start..self.pos]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string())
            }
        }
    }

    fn parse_plain_scalar(&mut self) -> Result<YamlValue, GraphError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == ',' || c == ']' || c == '}' {
                break;
            }
            self.pos += 1;
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(YamlValue::Null);
        }
        Ok(typed_scalar_from_plain(trimmed))
    }

    fn parse_single_quoted(&mut self) -> Result<String, GraphError> {
        self.expect('\'')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(multiline_flow_error(self.line_no)),
                Some('\'') => {
                    self.pos += 1;
                    if self.peek() == Some('\'') {
                        s.push('\'');
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(s)
    }

    fn parse_double_quoted(&mut self) -> Result<String, GraphError> {
        self.expect('"')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(multiline_flow_error(self.line_no)),
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some('n') => {
                            s.push('\n');
                            self.pos += 1;
                        }
                        Some('t') => {
                            s.push('\t');
                            self.pos += 1;
                        }
                        Some('\\') => {
                            s.push('\\');
                            self.pos += 1;
                        }
                        Some('"') => {
                            s.push('"');
                            self.pos += 1;
                        }
                        Some('u') => {
                            self.pos += 1;
                            let mut hex = String::with_capacity(4);
                            for _ in 0..4 {
                                match self.peek() {
                                    Some(c) => {
                                        hex.push(c);
                                        self.pos += 1;
                                    }
                                    None => return Err(multiline_flow_error(self.line_no)),
                                }
                            }
                            let code = u32::from_str_radix(&hex, 16).map_err(|_| {
                                GraphError::Okf(format!(
                                    "Zeile {}: ungültige \\u-Escape-Sequenz",
                                    self.line_no
                                ))
                            })?;
                            let ch = char::from_u32(code).ok_or_else(|| {
                                GraphError::Okf(format!(
                                    "Zeile {}: ungültiger Unicode-Codepoint in \\u-Escape",
                                    self.line_no
                                ))
                            })?;
                            s.push(ch);
                        }
                        _ => {
                            return Err(GraphError::Okf(format!(
                                "Zeile {}: unbekannte Escape-Sequenz",
                                self.line_no
                            )))
                        }
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(pairs: Vec<(String, YamlValue)>) {
        let text = emit_document(&pairs);
        let parsed = parse_document(&text).expect("muss parsen");
        assert_eq!(parsed, pairs, "Roundtrip fehlgeschlagen für:\n{text}");
    }

    /// Tief verschachtelte Flow-Notation muss ein FEHLER sein, kein Absturz.
    /// Ohne Grenze läuft der Stack über, und das ist in Rust kein abfangbarer
    /// Panic: der Prozess ist weg, ohne Meldung. Eine 6-KB-Datei mit 3000 `[`
    /// reichte dafür — und sie bleibt im Bundle liegen, bricht also jeden
    /// weiteren Start erneut ab.
    #[test]
    fn tief_verschachtelte_flow_notation_ist_ein_fehler_kein_absturz() {
        let text = format!("x: {}{}", "[".repeat(5000), "]".repeat(5000));
        let err = parse_document(&text).expect_err("muss abgelehnt werden");
        assert!(
            matches!(&err, GraphError::Okf(m) if m.contains("verschachtelt")),
            "unerwarteter Fehler: {err}"
        );
        // Und die erlaubte Tiefe funktioniert weiterhin.
        let ok = format!("x: {}{}", "[".repeat(60), "]".repeat(60));
        parse_document(&ok).expect("60 Ebenen müssen durchgehen");
    }

    /// Der Zähler muss beim VERLASSEN einer Ebene wieder sinken — sonst
    /// zählt eine lange flache Liste wie eine tiefe Verschachtelung und
    /// gültige Dokumente würden abgelehnt.
    #[test]
    fn viele_geschwister_zaehlen_nicht_als_tiefe() {
        let eintraege = (0..500).map(|i| format!("[{i}]")).collect::<Vec<_>>();
        let text = format!("x: [{}]", eintraege.join(", "));
        let parsed = parse_document(&text).expect("flache Geschwister sind keine Tiefe");
        assert_eq!(parsed[0].1.as_seq().map(<[YamlValue]>::len), Some(500));
    }

    /// Struktur-Zeichen in einem Flow-Container. Unquotiert würde
    /// `[Fehler in a, b]` als ZWEI Elemente zurückkommen und `{ resource: a} b }`
    /// gar nicht mehr parsen — beides stiller Datenverlust beim nächsten Lesen.
    #[test]
    fn flow_skalare_mit_struktur_zeichen_werden_quotiert() {
        let pairs = vec![
            (
                "tags".to_string(),
                YamlValue::seq([YamlValue::str("Fehler in a, b"), YamlValue::str("x]y")]),
            ),
            (
                "quelle".to_string(),
                YamlValue::map([("resource".to_string(), YamlValue::str("a} b"))]),
            ),
        ];
        let text = emit_document(&pairs);
        assert!(text.contains("'Fehler in a, b'"), "nicht quotiert:\n{text}");
        roundtrip(pairs);
    }

    /// Schlüssel aus `GraphEntity::extra` stammen aus handgeschriebenem
    /// Frontmatter und sind damit beliebig. Roh ausgegeben erzeugen sie eine
    /// Datei, die der eigene Parser danach ablehnt.
    #[test]
    fn exotische_schluessel_werden_quotiert() {
        let pairs = vec![
            ("stale after".to_string(), YamlValue::str("2026-12-31")),
            ("a:b".to_string(), YamlValue::Int(1)),
            ("#kommentar".to_string(), YamlValue::str("x")),
            ("2026".to_string(), YamlValue::str("jahr")),
        ];
        roundtrip(pairs);
    }

    #[test]
    fn roundtrip_deckt_alle_konstruktionen_ab() {
        let pairs = vec![
            ("id".to_string(), YamlValue::str("S-3")),
            ("revision".to_string(), YamlValue::Int(7)),
            ("confidence".to_string(), YamlValue::Float(0.82)),
            ("verified".to_string(), YamlValue::Bool(true)),
            ("deleted_at".to_string(), YamlValue::Null),
            (
                "tags".to_string(),
                YamlValue::seq([YamlValue::str("a"), YamlValue::str("b")]),
            ),
            ("note".to_string(), YamlValue::str("Zeile eins\nZeile zwei")),
            ("umlaut".to_string(), YamlValue::str("Über Größe hinüber")),
            ("leer".to_string(), YamlValue::str("")),
            ("als_string".to_string(), YamlValue::str("0.2")),
            (
                "resource".to_string(),
                YamlValue::map([
                    ("id".to_string(), YamlValue::str("S-3")),
                    ("kind".to_string(), YamlValue::str("web")),
                ]),
            ),
            (
                "claims".to_string(),
                YamlValue::seq([YamlValue::map([
                    ("id".to_string(), YamlValue::str("C-21")),
                    (
                        "sources".to_string(),
                        YamlValue::seq([YamlValue::str("S-3")]),
                    ),
                    (
                        "nested".to_string(),
                        YamlValue::seq([YamlValue::seq([YamlValue::Int(1), YamlValue::Int(2)])]),
                    ),
                ])]),
            ),
        ];
        roundtrip(pairs);
    }

    #[test]
    fn zweimaliges_emittieren_ist_byte_identisch() {
        let pairs = vec![(
            "claims".to_string(),
            YamlValue::seq([YamlValue::map([
                ("id".to_string(), YamlValue::str("C-1")),
                (
                    "sources".to_string(),
                    YamlValue::seq([YamlValue::str("S-1")]),
                ),
            ])]),
        )];
        let first = emit_document(&pairs);
        let second = emit_document(&pairs);
        assert_eq!(first, second);
    }

    #[test]
    fn quotierte_zahl_bleibt_string_unquotierte_wird_float() {
        let text = "a: '0.2'\nb: 0.2\n";
        let parsed = parse_document(text).unwrap();
        assert_eq!(parsed[0].1, YamlValue::str("0.2"));
        assert_eq!(parsed[1].1, YamlValue::Float(0.2));
    }

    #[test]
    fn block_seq_auf_schluesselspalte_wird_akzeptiert() {
        let text = "tags:\n- a\n- b\n";
        let parsed = parse_document(text).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "tags".to_string(),
                YamlValue::seq([YamlValue::str("a"), YamlValue::str("b")])
            )]
        );
    }

    #[test]
    fn block_seq_eingerueckt_wird_akzeptiert() {
        let text = "tags:\n  - a\n  - b\n";
        let parsed = parse_document(text).unwrap();
        assert_eq!(
            parsed,
            vec![(
                "tags".to_string(),
                YamlValue::seq([YamlValue::str("a"), YamlValue::str("b")])
            )]
        );
    }

    #[test]
    fn flow_map_verschachtelt_in_flow_seq() {
        let text = "x: [{ a: b, c: [d, e] }, 1]\n";
        let parsed = parse_document(text).unwrap();
        let expected = YamlValue::seq([
            YamlValue::map([
                ("a".to_string(), YamlValue::str("b")),
                (
                    "c".to_string(),
                    YamlValue::seq([YamlValue::str("d"), YamlValue::str("e")]),
                ),
            ]),
            YamlValue::Int(1),
        ]);
        assert_eq!(parsed[0].1, expected);
    }

    #[test]
    fn kommentare_und_leerzeilen_werden_ignoriert() {
        let text = "# Kommentarzeile\n\nkey: value # Zeilenkommentar\n";
        let parsed = parse_document(text).unwrap();
        assert_eq!(parsed, vec![("key".to_string(), YamlValue::str("value"))]);
    }

    #[test]
    fn raute_in_quotiertem_wert_ist_kein_kommentar() {
        let text = "key: 'a # b'\n";
        let parsed = parse_document(text).unwrap();
        assert_eq!(parsed, vec![("key".to_string(), YamlValue::str("a # b"))]);
    }

    #[test]
    fn fehler_bei_block_skalar() {
        let err = parse_document("key: |\n  a\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Block-Skalare")));
    }

    #[test]
    fn fehler_bei_anker() {
        let err = parse_document("key: &anchor value\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Anker")));
    }

    #[test]
    fn fehler_bei_alias() {
        let err = parse_document("key: *anchor\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Aliase")));
    }

    #[test]
    fn fehler_bei_tag() {
        let err = parse_document("key: !!str value\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Tags")));
    }

    #[test]
    fn fehler_bei_dokument_trenner() {
        let err = parse_document("key: value\n---\nother: 1\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Dokument-Trenner")));
    }

    #[test]
    fn fehler_bei_doppeltem_schluessel() {
        let err = parse_document("key: 1\nkey: 2\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("doppelter Schlüssel")));
    }

    #[test]
    fn fehler_bei_tabs() {
        let err = parse_document("key:\tvalue\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("Tabs")));
    }

    #[test]
    fn fehler_bei_mehrzeiligem_flow() {
        let err = parse_document("tags: [a, b\nother: 1\n").unwrap_err();
        assert!(matches!(err, GraphError::Okf(m) if m.contains("mehrzeilige Flow-Notation")));
    }
}
