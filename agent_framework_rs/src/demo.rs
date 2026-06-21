//! Demo-LLM, Demo-Tools und LLM-Auswahl — der netzfreie Fallback, den sowohl die
//! Kommandozeile (`bin/agentkit`) als auch das TUI (`bin/tui`) nutzen.
//!
//! Bewusst hier in der Library (statt in einem einzelnen Binary), damit beide
//! Frontends denselben Code teilen: Ohne API-Key läuft ein winziges,
//! deterministisches Modell, das echte Tool-Calls auslöst — so ist die installierte
//! Executable auch ohne Netz/Schlüssel sofort interaktiv.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::llm::{Chunk, ChunkStream, Llm, Message};
use crate::ToolRegistry;

/// Wählt den LLM: Azure -> OpenAI -> Demo (Fallback). Gibt zusätzlich ein
/// Label für die Titelzeile / Statusausgabe zurück.
pub fn build_llm(force_demo: bool) -> (Arc<dyn Llm>, String) {
    if !force_demo {
        #[cfg(feature = "openai")]
        {
            if std::env::var("AZURE_OPENAI_API_KEY").is_ok() {
                if let Ok(llm) = crate::azure_from_env() {
                    let dep = std::env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "?".into());
                    return (Arc::new(llm), format!("azure:{dep}"));
                }
            }
            if std::env::var("OPENAI_API_KEY").is_ok() {
                if let Ok(llm) = crate::openai_from_env() {
                    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
                    return (Arc::new(llm), format!("openai:{model}"));
                }
            }
        }
    }
    (Arc::new(DemoLlm), "demo (kein Netz)".to_string())
}

/// Ein kleiner Demo-Werkzeugkasten — dieselben Tools, die das `DemoLlm` ansteuert,
/// aber auch ein echtes Modell kann sie nutzen.
pub fn demo_tools() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.add(
        "add",
        "Addiert zwei ganze Zahlen a und b.",
        json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
        |args: Value| {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok((a + b).to_string())
        },
    );
    reg.add(
        "wetter",
        "Liefert (frei erfundenes) Wetter für eine Stadt.",
        json!({"type":"object","properties":{"stadt":{"type":"string"}},"required":["stadt"]}),
        |args: Value| {
            let stadt = args["stadt"].as_str().unwrap_or("");
            Ok(format!("In {stadt}: 18°C, leicht bewölkt, schwacher Wind."))
        },
    );
    reg.add(
        "reverse",
        "Dreht eine Zeichenkette um.",
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
        |args: Value| {
            let t = args["text"].as_str().unwrap_or("");
            Ok(t.chars().rev().collect())
        },
    );
    reg
}

// ------------------------------------------------------------------------- Demo-LLM

/// Ein winziger, deterministischer LLM ohne Netz — für den Demo-Modus.
///
/// Er schaut auf die letzte Nachricht: liegt schon ein Tool-Ergebnis vor, fasst er
/// es zusammen; sonst sucht er in der letzten User-Nachricht nach einem passenden
/// Tool-Aufruf (Addition `a + b`, `wetter in <Stadt>`) und ruft es auf — andernfalls
/// antwortet er direkt. Dadurch ist die Anwendung auch ohne API-Key interaktiv.
pub struct DemoLlm;

impl DemoLlm {
    fn answer_chunks(text: &str) -> Vec<Chunk> {
        // Wort für Wort streamen — zeigt den Streaming-Pfad. `split_inclusive`
        // behält das trennende Leerzeichen am Wort, sodass die Stücke wieder den
        // Originaltext ergeben.
        text.split_inclusive(' ').map(Chunk::text).collect()
    }
}

impl Llm for DemoLlm {
    fn complete(&self, _messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        Ok(Message {
            content: Some("(komprimierte Zusammenfassung)".to_string()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        let last = messages.last();
        let last_role = last.and_then(|m| m["role"].as_str()).unwrap_or("");

        // Schon ein Tool-Ergebnis da -> finale Antwort.
        if last_role == "tool" {
            let result = last.and_then(|m| m["content"].as_str()).unwrap_or("");
            let text = format!("Ergebnis: {result}");
            return Ok(Box::new(DemoLlm::answer_chunks(&text).into_iter()));
        }

        // Letzte User-Nachricht heranziehen.
        let user = messages
            .iter()
            .rev()
            .find(|m| m["role"].as_str() == Some("user"))
            .and_then(|m| m["content"].as_str())
            .unwrap_or("")
            .to_string();
        let lower = user.to_lowercase();

        // 1) Addition "a + b"?
        if let Some((a, b)) = parse_addition(&user) {
            let args = json!({"a": a, "b": b}).to_string();
            return Ok(Box::new(
                vec![Chunk::tool(0, "demo-add", "add", &args)].into_iter(),
            ));
        }

        // 2) Wetter?
        if lower.contains("wetter") || lower.contains("weather") {
            let stadt = parse_city(&user).unwrap_or_else(|| "Wien".to_string());
            let args = json!({"stadt": stadt}).to_string();
            return Ok(Box::new(
                vec![Chunk::tool(0, "demo-wetter", "wetter", &args)].into_iter(),
            ));
        }

        // 3) Sonst: direkte Demo-Antwort.
        let text = format!(
            "Demo-Modus (kein Netz): Ich habe »{}« erhalten. Setze einen API-Key \
             (OPENAI_API_KEY oder AZURE_OPENAI_*), um ein echtes Modell zu nutzen. \
             Probier z. B. »17 + 25« oder »Wetter in Berlin«.",
            user.trim()
        );
        Ok(Box::new(DemoLlm::answer_chunks(&text).into_iter()))
    }
}

/// Findet das erste Muster `<int> + <int>` in einem Text: den Ziffernlauf direkt
/// links bzw. rechts vom ersten `+` (Satzzeichen/Wörter drumherum werden ignoriert).
fn parse_addition(text: &str) -> Option<(i64, i64)> {
    let (left, right) = text.split_once('+')?;
    let a = left
        .trim_end()
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?;
    let b = right
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Sehr einfache Stadt-Extraktion: das Wort nach einem alleinstehenden "in".
fn parse_city(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if w.eq_ignore_ascii_case("in") {
            if let Some(next) = words.get(i + 1) {
                let city: String = next
                    .chars()
                    .filter(|c| c.is_alphabetic() || *c == '-')
                    .collect();
                if !city.is_empty() {
                    return Some(city);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agent, Strategy};

    #[test]
    fn addition_is_parsed() {
        assert_eq!(parse_addition("Was ist 17 + 25?"), Some((17, 25)));
        assert_eq!(parse_addition("rechne 3+4"), Some((3, 4)));
        assert_eq!(parse_addition("kein plus hier"), None);
    }

    #[test]
    fn city_is_extracted() {
        assert_eq!(
            parse_city("Wie ist das Wetter in Berlin?").as_deref(),
            Some("Berlin")
        );
        assert_eq!(parse_city("Wetter heute").as_deref(), None);
    }

    /// Demo-LLM treibt einen echten Agent-Loop: Tool-Call -> Ergebnis -> Antwort.
    #[test]
    fn demo_agent_runs_tool_then_answers() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Was ist 17 + 25?");
        assert!(answer.contains("42"), "Antwort war: {answer}");
    }

    #[test]
    fn demo_agent_handles_weather() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Wie ist das Wetter in Graz?");
        assert!(
            answer.to_lowercase().contains("graz"),
            "Antwort war: {answer}"
        );
    }

    #[test]
    fn demo_agent_plain_reply_without_tool() {
        let mut agent = Agent::builder(Arc::new(DemoLlm))
            .tools(demo_tools())
            .strategy(Strategy::Plain)
            .build();
        let answer = agent.run("Hallo!");
        assert!(answer.contains("Demo-Modus"), "Antwort war: {answer}");
    }
}
