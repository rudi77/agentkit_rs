//! Erkennung von Geheimnissen in Belegstellen.
//!
//! Eine Belegstelle ist ein Auszug aus einem Tool-Ergebnis — aus einer `.env`,
//! einer Shell-Ausgabe, einer Konfigurationsdatei. Was im Graphen steht, liegt
//! als Klartext auf der Platte, wird in künftige Prompts eingespielt und kann
//! auf die Benutzer-Ebene wandern. Ein Schlüssel hat dort nichts verloren.
//!
//! Die Erkennung ist bewusst einfach: bekannte Formate und die üblichen
//! Zuweisungen, ohne Regex-Dependency. Sie irrt lieber in Richtung „nicht
//! speichern" — ein fehlender Auszug kostet eine Belegstelle, ein
//! gespeicherter Schlüssel kostet mehr. Sie ist kein Scanner: was hier nicht
//! anschlägt, ist nicht deshalb unbedenklich.

/// Steht anstelle eines Auszugs, der nach einem Geheimnis aussah.
pub const REDACTED: &str = "[Auszug nicht gespeichert: sah nach einem Geheimnis aus]";

/// Präfix und Mindestlänge des Rests (Buchstaben, Ziffern, `-`, `_`).
const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),  // OpenAI, Anthropic (sk-ant-…)
    ("ghp_", 20), // GitHub
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("glpat-", 16), // GitLab
    ("xoxb-", 16),  // Slack
    ("xoxp-", 16),
    ("xoxa-", 16),
    ("AKIA", 16), // AWS Access Key ID
    ("ASIA", 16),
    ("AIza", 30), // Google API
    ("eyJ", 40),  // JWT (Header beginnt base64-kodiert mit `{"`)
];

/// Schlüsselwörter, hinter denen ein Wert steht (`api_key = …`, `"token": "…"`).
const ASSIGNMENT_WORDS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "secret",
    "token",
    "password",
    "passwd",
    "passwort",
    "private_key",
    "access_key",
];

/// Mindestlänge eines zugewiesenen Werts — kürzere sind eher Platzhalter.
const MIN_ASSIGNED: usize = 12;

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Sieht `text` so aus, als enthielte er ein Geheimnis?
pub fn looks_like_secret(text: &str) -> bool {
    if text.contains("PRIVATE KEY-----") {
        return true;
    }
    for (prefix, min) in TOKEN_PREFIXES {
        for (start, _) in text.match_indices(prefix) {
            // Nur am Wortanfang: `task-` soll `sk-` nicht auslösen.
            let boundary = text[..start]
                .chars()
                .next_back()
                .map_or(true, |c| !c.is_ascii_alphanumeric());
            let rest = text[start + prefix.len()..]
                .chars()
                .take_while(|c| is_token_char(*c))
                .count();
            if boundary && rest >= *min {
                return true;
            }
        }
    }
    let lower = text.to_lowercase();
    for word in ASSIGNMENT_WORDS {
        for (start, _) in lower.match_indices(word) {
            let after = &lower[start + word.len()..];
            // Zwischen Wort und Wert: Anführungszeichen, Leerraum, ein `=`/`:`.
            let separator: String = after
                .chars()
                .take_while(|c| matches!(c, '"' | '\'' | ' ' | '\t' | '=' | ':'))
                .collect();
            if !separator.contains(['=', ':']) {
                continue;
            }
            let value = after[separator.len()..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | ';'))
                .count();
            if value >= MIN_ASSIGNED {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bekannte_formate_und_zuweisungen_schlagen_an() {
        for text in [
            "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz012345",
            "Authorization: Bearer ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow…",
            r#"{"password": "korrekt-pferd-batterie"}"#,
            "token: 9f8e7d6c5b4a39281706",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc",
        ] {
            assert!(looks_like_secret(text), "nicht erkannt: {text}");
        }
    }

    #[test]
    fn gewoehnliche_belegstellen_bleiben() {
        for text in [
            "test result: FAILED. 7 passed; 2 failed",
            "task-runner: pytest -q tests/",
            "fn token_count(text: &str) -> usize",
            "password: <dein Passwort>",
            "Der Token-Zähler meldet 4096 Tokens.",
            "api_key = os.environ[\"KEY\"]",
        ] {
            assert!(!looks_like_secret(text), "fälschlich erkannt: {text}");
        }
    }
}
