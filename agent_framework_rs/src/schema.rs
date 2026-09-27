//! JSON-Schema für `--schema` und `--check`: prüfen, ob eine Antwort passt,
//! und entscheiden, ob der Anbieter die Struktur selbst erzwingen kann.
//!
//! Bewusst **keine** vollständige JSON-Schema-Implementierung und keine neue
//! Abhängigkeit: geprüft wird die Teilmenge, die Extraktions-Schemas
//! tatsächlich benutzen — `type`, `enum`, `const`, `properties`, `required`,
//! `additionalProperties`, `items`, `minItems`/`maxItems`,
//! `minLength`/`maxLength`, `minimum`/`maximum`, `anyOf`/`oneOf`/`allOf` und
//! lokale `$ref` (`#/$defs/…`, `#/definitions/…`). Unbekannte Schlüsselwörter
//! (z. B. `pattern`, `format`) werden ignoriert, nicht abgelehnt — eine
//! Antwort fällt also nie an einer Regel durch, die hier niemand kennt.

use serde_json::{json, Value};

/// Prüft `value` gegen `schema`. `Err` trägt die Verstöße als lesbare Zeilen
/// mit JSON-Pfad (`$.betrag: erwartet number, gefunden string`) — genau diese
/// Zeilen bekommt das Modell beim nächsten Versuch als Rückmeldung.
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, schema, value, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check(root: &Value, schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    // `true`/`false` sind gültige Schemas: alles bzw. nichts erlaubt.
    let Some(s) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            errors.push(format!("{path}: hier ist kein Wert erlaubt"));
        }
        return;
    };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        match resolve_ref(root, r) {
            Some(target) => check(root, target, value, path, errors),
            None => errors.push(format!("{path}: unauflösbare Referenz {r}")),
        }
    }
    if let Some(t) = s.get("type") {
        let erlaubt: Vec<&str> = match t {
            Value::String(x) => vec![x.as_str()],
            Value::Array(xs) => xs.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !erlaubt.is_empty() && !erlaubt.iter().any(|t| type_matches(t, value)) {
            errors.push(format!(
                "{path}: erwartet {}, gefunden {}",
                erlaubt.join("|"),
                type_name(value)
            ));
            // Die übrigen Regeln setzen den Typ voraus — Folgefehler wären Rauschen.
            return;
        }
    }
    if let Some(options) = s.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            errors.push(format!(
                "{path}: {value} ist nicht erlaubt (erlaubt: {})",
                options
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if let Some(c) = s.get("const") {
        if c != value {
            errors.push(format!("{path}: erwartet genau {c}"));
        }
    }
    match value {
        Value::Object(map) => {
            let props = s.get("properties").and_then(Value::as_object);
            for key in s
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !map.contains_key(key) {
                    errors.push(format!("{path}: Pflichtfeld '{key}' fehlt"));
                }
            }
            for (key, v) in map {
                let child = format!("{path}.{key}");
                match props.and_then(|p| p.get(key)) {
                    Some(sub) => check(root, sub, v, &child, errors),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errors.push(format!("{path}: Feld '{key}' ist nicht vorgesehen"))
                        }
                        Some(extra @ Value::Object(_)) => check(root, extra, v, &child, errors),
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(n) = s.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < n {
                    errors.push(format!("{path}: mindestens {n} Einträge erwartet"));
                }
            }
            if let Some(n) = s.get("maxItems").and_then(Value::as_u64) {
                if (items.len() as u64) > n {
                    errors.push(format!("{path}: höchstens {n} Einträge erlaubt"));
                }
            }
            if let Some(item_schema) = s.get("items") {
                for (i, v) in items.iter().enumerate() {
                    check(root, item_schema, v, &format!("{path}[{i}]"), errors);
                }
            }
        }
        Value::String(text) => {
            let len = text.chars().count() as u64;
            if let Some(n) = s.get("minLength").and_then(Value::as_u64) {
                if len < n {
                    errors.push(format!("{path}: mindestens {n} Zeichen erwartet"));
                }
            }
            if let Some(n) = s.get("maxLength").and_then(Value::as_u64) {
                if len > n {
                    errors.push(format!("{path}: höchstens {n} Zeichen erlaubt"));
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            if let Some(min) = s.get("minimum").and_then(Value::as_f64) {
                if x < min {
                    errors.push(format!("{path}: {x} < Minimum {min}"));
                }
            }
            if let Some(max) = s.get("maximum").and_then(Value::as_f64) {
                if x > max {
                    errors.push(format!("{path}: {x} > Maximum {max}"));
                }
            }
        }
        _ => {}
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for sub in all {
            check(root, sub, value, path, errors);
        }
    }
    for (key, exactly_one) in [("anyOf", false), ("oneOf", true)] {
        if let Some(options) = s.get(key).and_then(Value::as_array) {
            let passend = options
                .iter()
                .filter(|sub| {
                    let mut e = Vec::new();
                    check(root, sub, value, path, &mut e);
                    e.is_empty()
                })
                .count();
            if passend == 0 || (exactly_one && passend > 1) {
                errors.push(format!("{path}: passt nicht zu {key}"));
            }
        }
    }
}

/// Löst eine lokale Referenz (`#/…`) als JSON-Pointer im Wurzel-Schema auf.
/// Entfernte Referenzen (`http://…`) gibt es hier nicht — das bliebe ein Netzzugriff.
fn resolve_ref<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    root.pointer(r.strip_prefix('#')?)
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        // 3.0 zählt als Ganzzahl — so sieht es JSON Schema, und manche Modelle
        // schreiben Ganzzahlen mit Nachkommastelle.
        "integer" => {
            v.as_i64().is_some()
                || v.as_u64().is_some()
                || v.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => true,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Kann der Anbieter das Schema selbst erzwingen?
///
/// OpenAI (`strict: true`) und Anthropic (`output_config.format`) verlangen
/// beide, dass JEDES Objekt `additionalProperties: false` trägt; OpenAI
/// zusätzlich, dass alle Felder in `required` stehen, Anthropic lehnt Zahlen-
/// und Längengrenzen ab. Statt je Anbieter eine eigene Regel zu pflegen, gilt
/// die Schnittmenge: erfüllt ein Schema sie nicht, bleibt es bei Prüfen und
/// Wiederholen auf agentkit-Seite — lieber ein Versuch mehr als ein HTTP 400.
pub fn native_compatible(schema: &Value) -> bool {
    match schema {
        Value::Object(s) => {
            const UNSUPPORTED: &[&str] = &[
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "multipleOf",
                "minLength",
                "maxLength",
                "pattern",
                "minItems",
                "maxItems",
                "oneOf",
            ];
            if UNSUPPORTED.iter().any(|k| s.contains_key(*k)) {
                return false;
            }
            if let Some(props) = s.get("properties").and_then(Value::as_object) {
                if s.get("additionalProperties") != Some(&Value::Bool(false)) {
                    return false;
                }
                let required: Vec<&str> = s
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|r| r.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if props.keys().any(|k| !required.contains(&k.as_str())) {
                    return false;
                }
            } else if s.get("type").and_then(Value::as_str) == Some("object")
                && s.get("additionalProperties") != Some(&Value::Bool(false))
            {
                return false;
            }
            s.values().all(|v| match v {
                Value::Object(_) => native_compatible(v),
                Value::Array(xs) => xs.iter().all(native_compatible),
                _ => true,
            })
        }
        Value::Array(xs) => xs.iter().all(native_compatible),
        _ => true,
    }
}

/// Das `response_format` für OpenAI/Azure (Structured Outputs, strikt).
pub fn openai_response_format(schema: &Value) -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {"name": "antwort", "strict": true, "schema": schema},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rechnung() -> Value {
        json!({
            "type": "object",
            "properties": {
                "betrag": {"type": "number", "minimum": 0},
                "waehrung": {"enum": ["EUR", "USD"]},
                "posten": {"type": "array", "items": {"$ref": "#/$defs/posten"}, "minItems": 1}
            },
            "required": ["betrag", "waehrung"],
            "additionalProperties": false,
            "$defs": {"posten": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}
        })
    }

    #[test]
    fn gueltige_antwort_besteht() {
        let v = json!({"betrag": 12.5, "waehrung": "EUR", "posten": [{"text": "Kaffee"}]});
        assert_eq!(validate(&rechnung(), &v), Ok(()));
    }

    #[test]
    fn verstoesse_nennen_den_pfad() {
        let v = json!({"betrag": "12", "waehrung": "CHF", "posten": [{}], "extra": 1});
        let errors = validate(&rechnung(), &v).unwrap_err();
        let text = errors.join("\n");
        assert!(
            text.contains("$.betrag: erwartet number, gefunden string"),
            "{text}"
        );
        assert!(text.contains("$.waehrung"), "{text}");
        assert!(
            text.contains("$.posten[0]: Pflichtfeld 'text' fehlt"),
            "{text}"
        );
        assert!(text.contains("'extra' ist nicht vorgesehen"), "{text}");
    }

    #[test]
    fn pflichtfeld_und_grenzen() {
        let errors = validate(&rechnung(), &json!({"betrag": -1, "posten": []})).unwrap_err();
        let text = errors.join("\n");
        assert!(text.contains("Pflichtfeld 'waehrung' fehlt"));
        assert!(text.contains("Minimum"));
        assert!(text.contains("mindestens 1 Einträge"));
    }

    #[test]
    fn integer_anyof_und_oneof() {
        assert!(validate(&json!({"type": "integer"}), &json!(3.0)).is_ok());
        assert!(validate(&json!({"type": "integer"}), &json!(3.5)).is_err());
        let s = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
        assert!(validate(&s, &json!(null)).is_ok());
        assert!(validate(&s, &json!(1)).is_err());
        let s = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
        assert!(validate(&s, &json!(1)).is_err(), "passt auf beide");
    }

    #[test]
    fn unbekannte_schluesselwoerter_werden_ignoriert() {
        let s = json!({"type": "string", "pattern": "^[A-Z]+$", "format": "email"});
        assert!(validate(&s, &json!("klein")).is_ok());
    }

    #[test]
    fn nativ_nur_mit_geschlossenen_objekten() {
        let strikt = json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}, "liste": {"type": "array", "items": {"type": "string"}}},
            "required": ["ok", "liste"],
            "additionalProperties": false
        });
        assert!(native_compatible(&strikt));
        // Offene Objekte, optionale Felder und Grenzen schließen die
        // Anbieter-Erzwingung aus — geprüft wird dann nur lokal.
        assert!(!native_compatible(&rechnung()));
        let offen =
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        assert!(!native_compatible(&offen));
        let optional = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": [], "additionalProperties": false});
        assert!(!native_compatible(&optional));
    }
}
