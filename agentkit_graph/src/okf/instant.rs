//! Zeitrechnung für OKF-Zeitstempel — ohne `chrono`, damit dieses Crate keine
//! Speicher-/Zeit-Dependency zieht. Days-from-civil nach Howard Hinnants
//! öffentlich-domänen Algorithmus (<https://howardhinnant.github.io/date_algorithms.html>),
//! proleptischer gregorianischer Kalender, alles UTC.
//!
//! **Sekundengenau.** [`GraphEntity::created_at`](crate::model) ist laut
//! `src/model.rs:362` reine Anzeige/Audit — Sortierung und Ranking laufen über
//! die Revision, damit Tests deterministisch bleiben. Deshalb ist der
//! Millisekunden-Verlust bei `from_rfc3339` → `to_rfc3339` bewusst in Kauf
//! genommen: ein zweiter Schreibdurchlauf liest denselben (sekundengenauen)
//! Wert wieder ein und schreibt wieder dieselben Bytes — idempotent, auch wenn
//! die ursprünglichen Millisekunden nicht überleben.

/// Ergebnis von `days_from_civil` bzw. Eingabe von `civil_from_days`: Tage seit
/// 1970-01-01 (negativ für Daten davor — hier praktisch nie erreicht, da `ms`
/// als `u64` nie vor der Epoche liegt, aber der Algorithmus bleibt korrekt).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(m) + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_valid_date(y: i64, m: u32, d: u32) -> bool {
    (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)
}

/// Unix-Millisekunden ⇒ `"2026-09-12T10:11:12Z"`. Sekundenanteil wird
/// abgeschnitten (nicht gerundet) — konsistent mit [`from_rfc3339`], das
/// Sekundenbruchteile ebenfalls nur überliest.
pub fn to_rfc3339(ms: u64) -> String {
    let total_secs = ms / 1000;
    let days = (total_secs / 86400) as i64;
    let secs_of_day = total_secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Unix-Millisekunden ⇒ `"2026-09-12"` (nur das Datum, UTC-Kalendertag).
pub fn to_iso_date(ms: u64) -> String {
    let days = (ms / 1000 / 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `^\d{4}-\d{2}-\d{2}$` UND ein tatsächlich existierendes Kalenderdatum
/// (kein `2024-02-30`).
pub fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    match (
        ascii_digits(bytes, 0, 4),
        ascii_digits(bytes, 5, 2),
        ascii_digits(bytes, 8, 2),
    ) {
        (Some(y), Some(m), Some(d)) => is_valid_date(i64::from(y), m, d),
        _ => false,
    }
}

/// Liest genau `n` ASCII-Ziffern ab Byte-Position `at`.
///
/// Arbeitet bewusst auf **Bytes** statt auf `&str`-Slices: beide Parser hier
/// sehen handgeschriebenes Frontmatter und damit beliebigen Text. Ein
/// `text[8..10]` paniked, sobald an Position 8 ein Mehrbyte-Zeichen beginnt
/// (`"0000-00-€00"` erfüllt die vorgelagerten Byte-Prüfungen auf `-` und
/// zerlegt das `€` mittendrin) — eine Formatabweichung muss `None` liefern,
/// nicht den Prozess abbrechen.
fn ascii_digits(bytes: &[u8], at: usize, n: usize) -> Option<u32> {
    let slice = bytes.get(at..at.checked_add(n)?)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        slice
            .iter()
            .fold(0u32, |acc, b| acc * 10 + u32::from(b - b'0')),
    )
}

/// Liest einen Zeitstempel wie der Referenz-Validator: `T`, `t` oder ein
/// Leerzeichen als Datum/Zeit-Trenner, optionale Sekundenbruchteile (werden
/// überlesen — sekundengenau, siehe Modul-Kommentar), optionaler Offset
/// (`Z`, `z`, `+HH:MM`, `-HH:MM`, auch ohne Doppelpunkt), oder reines Datum
/// (⇒ Mitternacht UTC). `None` bei jeder Abweichung — kein Best-Effort-Parse.
pub fn from_rfc3339(text: &str) -> Option<u64> {
    if text.len() < 10 {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = i64::from(ascii_digits(bytes, 0, 4)?);
    let month = ascii_digits(bytes, 5, 2)?;
    let day = ascii_digits(bytes, 8, 2)?;
    if !is_valid_date(year, month, day) {
        return None;
    }
    let days = days_from_civil(year, month, day);

    let rest = &text[10..];
    if rest.is_empty() {
        let secs = days.checked_mul(86400)?;
        let secs: u64 = secs.try_into().ok()?;
        return secs.checked_mul(1000);
    }

    let mut chars = rest.chars();
    let sep = chars.next()?;
    if sep != 'T' && sep != 't' && sep != ' ' {
        return None;
    }
    let rest = &rest[sep.len_utf8()..];
    if rest.len() < 8 {
        return None;
    }
    let rbytes = rest.as_bytes();
    if rbytes[2] != b':' || rbytes[5] != b':' {
        return None;
    }
    let hh = ascii_digits(rbytes, 0, 2)?;
    let mm = ascii_digits(rbytes, 3, 2)?;
    let ss = ascii_digits(rbytes, 6, 2)?;
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }

    let mut idx = 8;
    if idx < rbytes.len() && rbytes[idx] == b'.' {
        idx += 1;
        let start = idx;
        while idx < rbytes.len() && rbytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start {
            return None; // '.' ohne folgende Ziffern
        }
    }
    let offset_str = &rest[idx..];
    let offset_minutes: i64 = if offset_str.is_empty() || offset_str == "Z" || offset_str == "z" {
        0
    } else {
        parse_offset(offset_str)?
    };

    let secs_of_day = i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss);
    let total_secs = days
        .checked_mul(86400)?
        .checked_add(secs_of_day)?
        .checked_sub(offset_minutes * 60)?;
    if total_secs < 0 {
        return None;
    }
    (total_secs as u64).checked_mul(1000)
}

/// `+HH:MM` / `-HH:MM` / `+HHMM` / `-HHMM` ⇒ Minuten relativ zu UTC (positiv
/// heißt „voraus", wird beim Umrechnen ins UTC also subtrahiert).
fn parse_offset(s: &str) -> Option<i64> {
    let mut chars = s.chars();
    let sign = match chars.next()? {
        '+' => 1i64,
        '-' => -1i64,
        _ => return None,
    };
    let digits: String = chars.filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hh: i64 = digits[0..2].parse().ok()?;
    let mm: i64 = digits[2..4].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 60 + mm))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Beide Parser bekommen handgeschriebenes Frontmatter zu sehen und müssen
    /// deshalb JEDEN Text verkraften. Die Byte-Prüfungen auf `-` und `:` lassen
    /// Eingaben durch, bei denen ein Mehrbyte-Zeichen genau auf einer
    /// Slice-Grenze sitzt — mit `&str`-Slices ist das ein Panic statt eines
    /// Formatfehlers.
    #[test]
    fn mehrbyte_zeichen_an_den_feldgrenzen_paniken_nicht() {
        for text in [
            "0000-00-\u{20AC}00",
            "2026-09-\u{20AC}0T00:00:00Z",
            "2026-09-12T00:\u{20AC}0:00Z",
            "2026-09-12T\u{20AC}0:00:00Z",
            "\u{20AC}000-00-00",
            "202\u{20AC}-09-12",
        ] {
            assert_eq!(from_rfc3339(text), None, "from_rfc3339({text:?})");
            assert!(!is_iso_date(text), "is_iso_date({text:?})");
        }
    }

    #[test]
    fn epoche() {
        assert_eq!(to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn bekannter_fixpunkt() {
        let ms = from_rfc3339("2026-09-12T10:11:12Z").unwrap();
        assert_eq!(to_rfc3339(ms), "2026-09-12T10:11:12Z");
    }

    #[test]
    fn schaltjahr_2024() {
        let ms = from_rfc3339("2024-02-29T00:00:00Z").unwrap();
        assert_eq!(to_iso_date(ms), "2024-02-29");
        // kein 2023-02-29
        assert_eq!(from_rfc3339("2023-02-29T00:00:00Z"), None);
    }

    #[test]
    fn jahrhundert_grenzen() {
        // 2000 ist Schaltjahr (durch 400 teilbar), 2100 nicht (durch 100, nicht 400).
        assert_eq!(
            from_rfc3339("2000-02-29T00:00:00Z").map(to_iso_date),
            Some("2000-02-29".to_string())
        );
        assert_eq!(from_rfc3339("2100-02-29T00:00:00Z"), None);
        let ms_2000 = from_rfc3339("2000-03-01T00:00:00Z").unwrap();
        assert_eq!(to_iso_date(ms_2000), "2000-03-01");
        let ms_2100 = from_rfc3339("2100-03-01T00:00:00Z").unwrap();
        assert_eq!(to_iso_date(ms_2100), "2100-03-01");
    }

    #[test]
    fn roundtrip_ueber_viele_werte() {
        for tag in [
            "1970-01-02",
            "1999-12-31",
            "2000-01-01",
            "2024-02-29",
            "2038-01-19",
            "2100-12-31",
        ] {
            let text = format!("{tag}T13:37:59Z");
            let ms = from_rfc3339(&text).expect("muss parsen");
            assert_eq!(to_rfc3339(ms), text, "Roundtrip fehlgeschlagen für {tag}");
            // zweiter Schreibdurchlauf ist idempotent
            assert_eq!(
                to_rfc3339(ms),
                to_rfc3339(from_rfc3339(&to_rfc3339(ms)).unwrap())
            );
        }
    }

    #[test]
    fn akzeptierte_trennzeichen_und_offsets() {
        let base = from_rfc3339("2026-09-12T10:11:12Z").unwrap();
        assert_eq!(from_rfc3339("2026-09-12t10:11:12Z"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12 10:11:12Z"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12T10:11:12z"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12T10:11:12.999Z"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12T12:11:12+02:00"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12T12:11:12+0200"), Some(base));
        assert_eq!(from_rfc3339("2026-09-12T08:11:12-02:00"), Some(base));
        assert_eq!(
            from_rfc3339("2026-09-12"),
            Some(from_rfc3339("2026-09-12T00:00:00Z").unwrap())
        );
    }

    #[test]
    fn is_iso_date_erkennt_gueltige_und_ungueltige_daten() {
        assert!(is_iso_date("2026-09-12"));
        assert!(is_iso_date("2024-02-29"));
        assert!(!is_iso_date("2023-02-29"));
        assert!(!is_iso_date("2026-13-01"));
        assert!(!is_iso_date("2026-09-12T00:00:00Z"));
        assert!(!is_iso_date("not-a-date"));
        assert!(!is_iso_date("26-09-12"));
    }

    #[test]
    fn lehnt_muell_ab() {
        assert_eq!(from_rfc3339("not-a-date"), None);
        assert_eq!(from_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(from_rfc3339("2026-09-12T25:00:00Z"), None);
        assert_eq!(from_rfc3339("2026-09-12T10:11:12+25:00"), None);
        assert_eq!(from_rfc3339("2026-09-12X10:11:12Z"), None);
        assert_eq!(from_rfc3339(""), None);
        assert_eq!(from_rfc3339("2026/09/12"), None);
    }
}
