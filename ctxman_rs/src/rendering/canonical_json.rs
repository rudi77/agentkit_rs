use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Kanonische JSON-Serialisierung und Content-Hashing für den deterministischen Render-Prefix
/// (Spec §4.6). Garantiert byte-identische Bytes für identischen Segment-Stand: Keys werden auf
/// jeder Ebene ordinal sortiert, Whitespace ist kompakt, Array-Reihenfolge bleibt erhalten
/// (sie trägt Bedeutung — Message- und Block-Order). Port von `CanonicalJson.cs`.
///
/// Divergenz zum C#-Original (dokumentiert, README): `System.Text.Json` escapet non-ASCII- und
/// HTML-sensitive Zeichen als `\uXXXX`, serde_json emittiert rohes UTF-8. Die Golden-Fixtures
/// sind rein ASCII und bleiben damit byte-identisch; für beliebige Inhalte gilt die Garantie
/// Intra-Bibliotheks-Determinismus, nicht Byte-Parität mit C#.
///
/// Alle JSON-Keys der Render-Ausgabe müssen ASCII sein: C# sortiert ordinal über UTF-16-Code-
/// Units, Rust über UTF-8-Bytes — nur für ASCII sind beide Ordnungen identisch.
pub fn serialize(value: &Value) -> String {
    let canonical = canonicalize(value);
    // serde_json auf einem bereits kanonisierten Value ist deterministisch: kompakt,
    // kultur-invariant, Map-Reihenfolge = Einfüge-Reihenfolge (hier: sortiert).
    serde_json::to_string(&canonical).expect("kanonisierter Value ist immer serialisierbar")
}

/// SHA-256 (hex, lowercase) über die kanonischen JSON-Bytes (UTF-8) des Static-Prefix
/// (System-Prompt + Tool-Defs). Cache-Stabilität ist hierüber messbar. (Spec §6, I4)
pub fn compute_cache_prefix_hash(static_prefix: &Value) -> String {
    sha256_hex(serialize(static_prefix).as_bytes())
}

/// SHA-256 (hex, lowercase) des Content-Strings (UTF-8). Dient dem Planner zur kanonischen
/// Sortierung der Static-Segmente nach `(source, kind, content_hash)`. (Spec §4.2, I4)
pub fn content_hash(content: &str) -> String {
    sha256_hex(content.as_bytes())
}

/// Rekursive Kanonisierung: Objekt-Keys ordinal sortieren, Arrays in Ordnung belassen,
/// Skalare unverändert. (Spec §4.6)
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(obj) => {
            // Keys ordinal (Byte-Ordnung) sortieren — kultur-stabil. (Spec §4.6)
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort_unstable();
            let mut sorted = Map::with_capacity(obj.len());
            for key in keys {
                debug_assert!(key.is_ascii(), "JSON-Keys müssen ASCII sein (Sortier-Parität zu C#)");
                sorted.insert(key.clone(), canonicalize(&obj[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(array) => {
            // Array-Reihenfolge MUSS erhalten bleiben (sie trägt Bedeutung). (Spec §4.6)
            Value::Array(array.iter().map(canonicalize).collect())
        }
        scalar => {
            // Floats erscheinen in Render-Fragmenten/Hash-Inputs nicht — Zahlenformat-Parität
            // zu System.Text.Json ist nur für Integer garantiert.
            debug_assert!(
                !matches!(scalar, Value::Number(n) if n.is_f64()),
                "Render-Fragmente/Hash-Inputs dürfen keine Floats enthalten (Format-Parität zu C#)"
            );
            scalar.clone()
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(hex, "{byte:02x}").expect("write! auf String schlägt nie fehl");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sortiert_keys_rekursiv_ordinal() {
        let value = json!({"zeta": {"b": 1, "a": 2}, "alpha": [{"y": 1, "x": 2}]});
        assert_eq!(
            serialize(&value),
            r#"{"alpha":[{"x":2,"y":1}],"zeta":{"a":2,"b":1}}"#
        );
    }

    #[test]
    fn bewahrt_array_reihenfolge() {
        let value = json!({"list": [3, 1, 2, {"b": 1, "a": 2}]});
        assert_eq!(serialize(&value), r#"{"list":[3,1,2,{"a":2,"b":1}]}"#);
    }

    #[test]
    fn kompakt_ohne_whitespace() {
        let value = json!({"a": "x y", "b": [1, 2]});
        assert_eq!(serialize(&value), r#"{"a":"x y","b":[1,2]}"#);
    }

    #[test]
    fn content_hash_bekannter_vektor() {
        // sha256("abc") — Standard-Testvektor (FIPS 180-2), lowercase hex wie in C#.
        assert_eq!(
            content_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cache_prefix_hash_ist_eingabe_reihenfolge_invariant() {
        let a = json!({"system": "s", "tools": [{"name": "git", "description": "d"}]});
        let b = json!({"tools": [{"description": "d", "name": "git"}], "system": "s"});
        assert_eq!(compute_cache_prefix_hash(&a), compute_cache_prefix_hash(&b));
    }

    #[test]
    fn golden_roundtrip_ist_byte_identisch() {
        // Paritäts-Gate (Plan M0): die C#-Golden-Fixtures durch parse → serialize schicken
        // muss byte-identisch zum Input sein (beweist Escaping-/Format-Parität fürs Korpus).
        for name in ["render-anthropic.json", "render-openai.json"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden")
                .join(name);
            let raw = std::fs::read_to_string(&path).expect("Golden-Datei lesbar");
            let trimmed = raw.trim_end_matches(['\r', '\n']);
            let parsed: Value = serde_json::from_str(trimmed).expect("Golden-Datei ist JSON");
            assert_eq!(serialize(&parsed), trimmed, "Roundtrip-Divergenz in {name}");
        }
    }
}
