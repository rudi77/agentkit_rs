//! Der einzige Draht zum Modell.
//!
//! Ein LLM ist *Text rein -> Text raus*. Das [`Llm`]-Trait kapselt genau das:
//! - `complete()` : ein Call, eine fertige Antwort (für Compaction & Nicht-Streaming).
//! - `stream()`   : derselbe Call streamend -> Iterator über Chunks (Deltas).
//!
//! Anders als Pythons dünne Hülle um die OpenAI-SDK-Objekte definieren wir hier
//! ein kleines, providerneutrales Chunk-Modell ([`Chunk`]/[`Delta`]), das die
//! relevanten Felder des OpenAI-Streaming-Formats (`choices[0].delta…`) abbildet.
//! Das macht den Agent-Loop testbar (FakeLlm) und hält ihn vom HTTP-Code getrennt.

use serde_json::Value;

/// Ein fragmentierter Tool-Call aus einem Streaming-Delta (pro `index` zusammenzusetzen).
#[derive(Debug, Clone, Default)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Das `delta`-Feld eines Streaming-Chunks.
#[derive(Debug, Clone, Default)]
pub struct Delta {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallDelta>,
}

/// Token-Verbrauch EINES Modell-Calls, wie der Provider ihn meldet.
///
/// Providerneutral: OpenAI nennt es `prompt_tokens`/`completion_tokens`,
/// Anthropic `input_tokens`/`output_tokens` (plus Cache-Felder). Gemessen,
/// nicht geschätzt — die Zeichen/4-Heuristik der Kompaktierung bleibt davon
/// unberührt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    /// Alle Eingabe-Tokens des Calls, aus dem Cache gelesene eingeschlossen.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Der Teil von `input_tokens`, der aus dem Prompt-Cache kam (billiger).
    pub cached_input_tokens: u64,
}

impl Usage {
    /// Summiert einen weiteren Call hinzu.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }

    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Ein Streaming-Chunk (vereinfacht: genau ein `choice`).
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub delta: Delta,
    /// Verbrauch des ganzen Calls — trägt meist nur der LETZTE Chunk eines
    /// Streams (OpenAI: `stream_options.include_usage`, Anthropic:
    /// `message_delta`). `None` bei allen anderen und bei Providern, die
    /// nichts melden.
    pub usage: Option<Usage>,
    /// Die Antwort endete an der Ausgabegrenze des Modells (OpenAI:
    /// `finish_reason: "length"`, Anthropic: `stop_reason: "max_tokens"`).
    /// Der Stream ist dann vollständig übertragen, die ANTWORT aber nicht: der
    /// letzte Text oder die Argumente des letzten Tool-Aufrufs brechen mitten
    /// ab. Ohne das Signal hielt der Loop sie für fertig.
    pub truncated: bool,
}

impl Chunk {
    /// Bequemer Konstruktor für reine Text-Deltas (Tests/Benchmarks).
    pub fn text(s: &str) -> Self {
        Chunk {
            delta: Delta {
                content: Some(s.to_string()),
                tool_calls: Vec::new(),
            },
            usage: None,
            truncated: false,
        }
    }

    /// Bequemer Konstruktor für einen Tool-Call-Delta.
    pub fn tool(index: usize, id: &str, name: &str, arguments: &str) -> Self {
        Chunk {
            delta: Delta {
                content: None,
                tool_calls: vec![ToolCallDelta {
                    index,
                    id: Some(id.to_string()),
                    name: Some(name.to_string()),
                    arguments: Some(arguments.to_string()),
                }],
            },
            usage: None,
            truncated: false,
        }
    }

    /// Ein reiner Verbrauchs-Chunk (Tests/Fake-Modelle), wie ihn ein Provider
    /// am Ende des Streams schickt.
    pub fn usage(input_tokens: u64, output_tokens: u64, cached_input_tokens: u64) -> Self {
        Chunk {
            delta: Delta::default(),
            usage: Some(Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            }),
            truncated: false,
        }
    }

    /// Ein leerer Schluss-Chunk, der meldet, dass die Antwort an der
    /// Ausgabegrenze abgeschnitten wurde (Tests/Fake-Modelle).
    pub fn truncated() -> Self {
        Chunk {
            truncated: true,
            ..Chunk::default()
        }
    }
}

/// Eine fertige (nicht gestreamte) Antwort.
#[derive(Debug, Clone, Default)]
pub struct Message {
    pub content: Option<String>,
    pub tool_calls: Vec<Value>,
}

/// Iterator über Streaming-Chunks.
///
/// Ein Element kann fehlschlagen: reißt die Verbindung mitten im Stream ab, ist
/// das KEIN reguläres Ende. Ohne diese Unterscheidung hätte der Loop eine halbe
/// Antwort für eine fertige gehalten und wäre mit Exit 0 gelaufen. Entspricht der
/// Ausnahme, die Pythons Generator an derselben Stelle wirft.
pub type ChunkStream = Box<dyn Iterator<Item = Result<Chunk, String>> + Send>;

/// Fertige Chunks als [`ChunkStream`] — für alle `Llm`-Implementierungen, die
/// ihre Antwort schon vollständig kennen (FakeLlm, Demo-Modus, Beispiele) und
/// deshalb nie mittendrin abbrechen können.
pub fn chunk_stream(chunks: Vec<Chunk>) -> ChunkStream {
    Box::new(chunks.into_iter().map(Ok))
}

/// Der einzige Draht zum Modell. `Send + Sync`, damit Agenten über Threads laufen
/// können (Worker-Threads, parallele Sub-Agents).
pub trait Llm: Send + Sync {
    /// Ein Call -> EINE fertige `Message` (mit `content` und `tool_calls`).
    fn complete(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<Message, String>;

    /// Derselbe Call streamend -> Iterator über Chunks (Deltas).
    fn stream(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<ChunkStream, String>;
}

#[cfg(feature = "openai")]
pub use openai::{azure_from_env, openai_from_env, OpenAiLlm};

#[cfg(feature = "openai")]
mod anthropic;
#[cfg(feature = "openai")]
pub use anthropic::{anthropic_from_env, AnthropicLlm, ANTHROPIC_DEFAULT_MODEL};

#[cfg(feature = "openai")]
mod openai {
    //! Echter OpenAI/Azure-Pfad über `ureq` (synchron, SSE zeilenweise geparst).

    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Socket-Timeouts des HTTP-Agenten. Ohne sie hat ureq KEINE Lesegrenze: ein
    /// Provider, der die Verbindung offen hält aber nichts mehr schickt, fror den
    /// Agent-Loop unabbrechbar ein — `consume_stream` prüft den Stop-Knopf nur
    /// ZWISCHEN Chunks, und ohne Chunk gibt es keinen Prüfpunkt.
    ///
    /// Die Grenze gilt je Lese-/Schreibvorgang, nicht für den ganzen Call: ein
    /// langer, aber aktiver Stream läuft weiter, nur die Stille wird begrenzt.
    /// Großzügig gewählt — bei Reasoning-Modellen liegen zwischen Request und
    /// erstem Token gut und gern Minuten.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    const IO_TIMEOUT: Duration = Duration::from_secs(300);

    /// Der HTTP-Agent mit den Timeouts oben — geteilt mit dem Anthropic-Pfad,
    /// der dieselbe Stille-Grenze braucht.
    pub(super) fn http_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .build()
    }

    /// Wickelt einen OpenAI-kompatiblen Endpunkt + Modell/Deployment.
    pub struct OpenAiLlm {
        url: String,
        api_key: String,
        /// Azure verlangt den Key im Header `api-key`, OpenAI in `Authorization`.
        azure: bool,
        model: String,
        /// Optionales `response_format` (z. B. `{"type":"json_object"}` für die
        /// OpenAI/Azure Structured-Outputs- bzw. JSON-Mode-Erzwingung). Wird, falls
        /// gesetzt, unverändert in den Request-Body übernommen.
        response_format: Option<Value>,
        reasoning_effort: Option<(String, &'static str)>,
        /// Gesetzt, sobald der Endpunkt Function-Tools nur ohne Reasoning annimmt
        /// (siehe [`needs_no_reasoning`]); ab dann geht `reasoning_effort: "none"` mit.
        no_reasoning: AtomicBool,
        /// Einmal gebaut und wiederverwendet — der Agent hält den Verbindungs-Pool,
        /// ein neuer je Request würde für jeden Modell-Call neu TLS aushandeln.
        agent: ureq::Agent,
    }

    impl OpenAiLlm {
        fn new(url: String, api_key: &str, azure: bool, model: &str) -> Self {
            OpenAiLlm {
                url,
                api_key: api_key.to_string(),
                azure,
                model: model.to_string(),
                response_format: None,
                reasoning_effort: None,
                no_reasoning: AtomicBool::new(false),
                agent: http_agent(),
            }
        }

        /// Standard-OpenAI: `https://api.openai.com/v1/chat/completions`.
        pub fn openai(api_key: &str, model: &str) -> Self {
            Self::new(
                "https://api.openai.com/v1/chat/completions".to_string(),
                api_key,
                false,
                model,
            )
        }

        /// Beliebiger **OpenAI-kompatibler** Endpunkt (lokale Server: Ollama,
        /// LM Studio, vLLM, llama.cpp, …). `base_url` ist die API-Wurzel inkl.
        /// Versionspfad, z. B. `http://localhost:11434/v1` — angehängt wird nur
        /// noch `/chat/completions`. Ein leerer `api_key` ist erlaubt (lokale
        /// Server verlangen meist keinen); dann wird kein `Authorization`-Header
        /// gesendet.
        pub fn compatible(base_url: &str, api_key: &str, model: &str) -> Self {
            let base = base_url.trim_end_matches('/');
            Self::new(format!("{base}/chat/completions"), api_key, false, model)
        }

        /// Azure-OpenAI: vollständige Deployment-URL inkl. `api-version`.
        pub fn azure(endpoint: &str, api_key: &str, deployment: &str, api_version: &str) -> Self {
            let base = endpoint.trim_end_matches('/');
            let url = format!(
                "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
            );
            Self::new(url, api_key, true, deployment)
        }

        /// Setzt das `response_format` (Structured Outputs / JSON-Mode). Builder-Stil.
        pub fn with_response_format(mut self, fmt: Value) -> Self {
            self.response_format = Some(fmt);
            self
        }

        /// Kurzform für strikten JSON-Mode (`{"type":"json_object"}`).
        pub fn json_mode(self) -> Self {
            self.with_response_format(json!({"type": "json_object"}))
        }

        fn with_env_reasoning_effort(mut self) -> Self {
            let effort = std::env::var("OPENAI_REASONING_EFFORT").ok();
            self.set_reasoning_effort(effort.as_deref(), "OPENAI_REASONING_EFFORT");
            self
        }

        /// Setzt die Denktiefe aus dem CLI-Flag, falls es angegeben wurde.
        pub fn with_cli_reasoning_effort(mut self, effort: Option<&str>) -> Self {
            if effort.is_some() {
                self.set_reasoning_effort(effort, "--reasoning-effort");
            }
            self
        }

        fn set_reasoning_effort(&mut self, effort: Option<&str>, source: &'static str) {
            self.reasoning_effort = effort
                .map(str::trim)
                .filter(|effort| !effort.is_empty())
                .map(|effort| (effort.to_owned(), source));
        }

        fn body(&self, messages: &[Value], tools: Option<&[Value]>, stream: bool) -> Value {
            let mut body = json!({
                "model": self.model,
                "messages": messages,
            });
            if let Some(t) = tools {
                body["tools"] = json!(t);
                body["tool_choice"] = json!("auto");
            }
            if let Some((effort, _)) = &self.reasoning_effort {
                body["reasoning_effort"] = json!(effort);
            } else if tools.is_some() && self.no_reasoning.load(Ordering::Relaxed) {
                body["reasoning_effort"] = json!("none");
            }
            if let Some(fmt) = &self.response_format {
                body["response_format"] = fmt.clone();
            }
            if stream {
                body["stream"] = json!(true);
                // Ohne diese Option schickt der Stream KEINEN Verbrauch — die
                // Summe je Lauf (`token_usage`-Events) bliebe leer. Azure
                // (ab 2024-09) und die gängigen lokalen Server verstehen sie.
                body["stream_options"] = json!({"include_usage": true});
            }
            body
        }

        /// Schickt den Request. Lehnt der Endpunkt Tools wegen Reasoning ab,
        /// einmal mit `reasoning_effort: "none"` wiederholen und es sich merken.
        /// Nicht vorab senden: Modelle ohne Reasoning lehnen den Parameter ab.
        fn send(
            &self,
            messages: &[Value],
            tools: Option<&[Value]>,
            stream: bool,
        ) -> Result<ureq::Response, String> {
            let effort_source = self.reasoning_effort.as_ref().map(|(_, source)| *source);
            let post = || {
                self.request()
                    .send_json(self.body(messages, tools, stream))
                    .map_err(|error| describe_error_with_effort_source(error, effort_source))
            };
            // Vor dem Senden lesen: parallele Läufe (Sub-Agenten) teilen sich das
            // LLM — jeder, der noch OHNE den Parameter gesendet hat, darf wiederholen.
            let had = self.no_reasoning.load(Ordering::Relaxed);
            match post() {
                Err(e)
                    if effort_source.is_none()
                        && tools.is_some()
                        && !had
                        && needs_no_reasoning(&e) =>
                {
                    self.no_reasoning.store(true, Ordering::Relaxed);
                    post()
                }
                other => other,
            }
        }

        fn request(&self) -> ureq::Request {
            let req = self
                .agent
                .post(&self.url)
                .set("Content-Type", "application/json");
            if self.azure {
                req.set("api-key", &self.api_key)
            } else if self.api_key.is_empty() {
                req // lokaler Server ohne Key -> kein Authorization-Header
            } else {
                req.set("Authorization", &format!("Bearer {}", self.api_key))
            }
        }
    }

    /// Erkennt an der Meldung aus [`describe_error`] den 400er „Function tools
    /// with reasoning_effort are not supported … in /v1/chat/completions".
    fn needs_no_reasoning(error: &str) -> bool {
        error.starts_with("HTTP 400") && error.contains("reasoning_effort")
    }

    /// Ein SSE-`data:`-Payload -> [`Chunk`]. `None` für alles, was kein
    /// verwertbares Delta ist (Keep-Alives, unbekannte Ereignisse).
    #[cfg(test)]
    fn parse_delta(payload: &str) -> Option<Chunk> {
        let v: Value = serde_json::from_str(payload).ok()?;
        Some(delta_chunk(&v))
    }

    /// Meldet ein SSE-Payload einen Fehler statt eines Deltas? Dann ist der
    /// Stream gescheitert, auch wenn die Verbindung sauber endet.
    ///
    /// Zwei Fälle, die vorher als leeres Delta durchgingen — der Lauf endete
    /// mit einer leeren oder halben Antwort und Exit 0:
    /// - `{"error": …}` mitten im Stream (Überlast, Abbruch auf Serverseite).
    ///   Bewusst OHNE `HTTP <code>`-Präfix: der Loop behandelt es wie einen
    ///   Abriss und versucht es erneut, genau wie beim Anthropic-`error`-Ereignis.
    /// - `finish_reason: "content_filter"`. Der Marker `content_filter` im Text
    ///   ist der Vertrag mit `agent.rs`: wie eine Ablehnung (`refusal`) wird das
    ///   nicht wiederholt — der Filter schlüge wieder zu.
    fn stream_error(v: &Value) -> Option<String> {
        let e = &v["error"];
        if !e.is_null() {
            return Some(format!(
                "Fehler im Stream ({}): {}",
                e["type"].as_str().or(e["code"].as_str()).unwrap_or("?"),
                e["message"].as_str().unwrap_or("")
            ));
        }
        (v["choices"][0]["finish_reason"] == "content_filter").then(|| {
            "Der Inhaltsfilter des Anbieters hat die Antwort abgeschnitten (content_filter) — \
             sie ist unvollständig. Auftrag umformulieren oder anderes Modell wählen."
                .to_string()
        })
    }

    /// Die SSE-Zeilen eines Chat-Completions-Streams -> Chunks.
    ///
    /// Zeilen der Form `data: {json}` (das Leerzeichen ist laut SSE optional,
    /// manche kompatiblen Server lassen es weg); `data: [DONE]` beendet den
    /// Strom. Endet er OHNE `[DONE]` (Lesefehler oder stilles EOF), war er
    /// abgeschnitten — das muss als Fehler heraus, nicht als Dateiende.
    fn sse_chunks(
        mut lines: impl Iterator<Item = std::io::Result<String>> + Send + 'static,
    ) -> ChunkStream {
        let mut finished = false;
        Box::new(std::iter::from_fn(move || loop {
            if finished {
                return None;
            }
            match lines.next() {
                Some(Ok(line)) => {
                    let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    if payload == "[DONE]" {
                        finished = true;
                        return None;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(payload) else {
                        continue;
                    };
                    if let Some(e) = stream_error(&v) {
                        finished = true;
                        return Some(Err(e));
                    }
                    return Some(Ok(delta_chunk(&v)));
                }
                Some(Err(e)) => return Some(Err(format!("Stream-Lesefehler: {e}"))),
                None => {
                    finished = true;
                    return Some(Err(
                        "Stream endete ohne '[DONE]' — abgeschnitten".to_string()
                    ));
                }
            }
        }))
    }

    fn delta_chunk(v: &Value) -> Chunk {
        let delta = &v["choices"][0]["delta"];
        let tool_calls = delta["tool_calls"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|tc| ToolCallDelta {
                        index: tc["index"].as_u64().unwrap_or(0) as usize,
                        id: tc["id"].as_str().map(String::from),
                        name: tc["function"]["name"].as_str().map(String::from),
                        arguments: tc["function"]["arguments"].as_str().map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Chunk {
            delta: Delta {
                content: delta["content"].as_str().map(String::from),
                tool_calls,
            },
            usage: parse_usage(&v["usage"]),
            truncated: v["choices"][0]["finish_reason"] == "length",
        }
    }

    /// Das `usage`-Objekt eines OpenAI-Chunks (kommt mit `include_usage` im
    /// letzten Chunk, dessen `choices` leer ist). `None`, wenn es fehlt.
    fn parse_usage(u: &Value) -> Option<Usage> {
        let input = u["prompt_tokens"].as_u64()?;
        Some(Usage {
            input_tokens: input,
            output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
        })
    }

    /// Macht aus einem `ureq`-Fehler eine aussagekräftige Meldung: HTTP-Status
    /// (inkl. `Retry-After` bei 429) und Anfang des Response-Bodys statt nur
    /// "status code 429". Der Agent-Loop retryt darauf mit Backoff.
    pub(super) fn describe_error(e: ureq::Error) -> String {
        describe_error_with_effort_source(e, None)
    }

    fn describe_error_with_effort_source(e: ureq::Error, effort_source: Option<&str>) -> String {
        match e {
            ureq::Error::Status(code, resp) => {
                let retry_after = resp
                    .header("Retry-After")
                    .map(|s| format!(", Retry-After: {s}s"))
                    .unwrap_or_default();
                let body = resp.into_string().unwrap_or_default();
                let attribution = effort_source
                    .filter(|_| matches!(code, 400 | 422) && body.contains("reasoning_effort"));
                let body: String = body.chars().take(300).collect();
                let hint = match code {
                    429 => " (Rate-Limit)",
                    500..=599 => " (Server-Fehler, transient)",
                    _ => "",
                };
                let mut message = format!("HTTP {code}{hint}{retry_after}: {body}");
                if let Some(source) = attribution {
                    message.push_str(&format!("\nExplizite Denktiefe aus {source} abgelehnt."));
                }
                message
            }
            other => format!("Netzwerkfehler: {other}"),
        }
    }

    impl Llm for OpenAiLlm {
        fn complete(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<Message, String> {
            let resp = self.send(messages, tools, false)?;
            let v: Value = resp.into_json().map_err(|e| e.to_string())?;
            let msg = &v["choices"][0]["message"];
            Ok(Message {
                content: msg["content"].as_str().map(String::from),
                tool_calls: msg["tool_calls"].as_array().cloned().unwrap_or_default(),
            })
        }

        fn stream(
            &self,
            messages: &[Value],
            tools: Option<&[Value]>,
        ) -> Result<ChunkStream, String> {
            let resp = self.send(messages, tools, true)?;
            Ok(sse_chunks(BufReader::new(resp.into_reader()).lines()))
        }
    }

    /// Baut einen Azure-OpenAI-LLM aus Umgebungsvariablen (wie `azure_from_env`).
    pub fn azure_from_env() -> Result<OpenAiLlm, String> {
        let get = |k: &str| std::env::var(k).map_err(|_| format!("env-Variable {k} fehlt"));
        Ok(OpenAiLlm::azure(
            &get("AZURE_OPENAI_ENDPOINT")?,
            &get("AZURE_OPENAI_API_KEY")?,
            &get("AZURE_OPENAI_DEPLOYMENT")?,
            &std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "2024-10-21".to_string()),
        )
        .with_env_reasoning_effort())
    }

    /// Baut einen Standard-OpenAI-LLM (`OPENAI_API_KEY`, optional `OPENAI_MODEL`).
    ///
    /// Mit `OPENAI_BASE_URL` zeigt derselbe Pfad auf einen **lokalen
    /// OpenAI-kompatiblen Server** (Ollama, LM Studio, vLLM, llama.cpp, …),
    /// z. B. `http://localhost:11434/v1`. Der API-Key ist dann optional.
    pub fn openai_from_env() -> Result<OpenAiLlm, String> {
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
        if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
            if !base.trim().is_empty() {
                let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
                return Ok(
                    OpenAiLlm::compatible(base.trim(), &key, &model).with_env_reasoning_effort()
                );
            }
        }
        let key =
            std::env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY fehlt".to_string())?;
        Ok(OpenAiLlm::openai(&key, &model).with_env_reasoning_effort())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `compatible()` hängt genau `/chat/completions` an die Base-URL an
        /// (Schrägstrich am Ende wird toleriert) und merkt sich Modell + Key.
        #[test]
        fn compatible_builds_local_url() {
            let llm = OpenAiLlm::compatible("http://localhost:11434/v1/", "", "llama3.1");
            assert_eq!(llm.url, "http://localhost:11434/v1/chat/completions");
            assert_eq!(llm.model, "llama3.1");
            assert!(llm.api_key.is_empty() && !llm.azure);
        }

        /// Der letzte Chunk mit `include_usage` hat leere `choices` und trägt
        /// den Verbrauch inkl. Cache-Anteil.
        #[test]
        fn usage_chunk_wird_gelesen() {
            let c = parse_delta(
                r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":7,"total_tokens":127,"prompt_tokens_details":{"cached_tokens":100}}}"#,
            )
            .unwrap();
            assert_eq!(
                c.usage,
                Some(Usage {
                    input_tokens: 120,
                    output_tokens: 7,
                    cached_input_tokens: 100
                })
            );
            assert!(c.delta.content.is_none() && c.delta.tool_calls.is_empty());
            assert!(!c.truncated);
            // Normale Deltas tragen keinen Verbrauch.
            let d = parse_delta(r#"{"choices":[{"delta":{"content":"x"}}]}"#).unwrap();
            assert!(d.usage.is_none());
        }

        #[test]
        fn explizites_reasoning_effort_mit_und_ohne_tools() {
            let mut llm = OpenAiLlm::openai("test", "model");
            llm.set_reasoning_effort(Some("low"), "OPENAI_REASONING_EFFORT");
            let tools = [json!({"type": "function"})];
            assert_eq!(llm.body(&[], None, false)["reasoning_effort"], "low");
            assert_eq!(llm.body(&[], Some(&tools), true)["reasoning_effort"], "low");
        }

        #[test]
        fn reasoning_effort_trims_and_ignores_blank_environment_values() {
            let mut llm = OpenAiLlm::openai("test", "test");
            llm.set_reasoning_effort(Some("  low\t"), "OPENAI_REASONING_EFFORT");
            assert_eq!(llm.body(&[], None, false)["reasoning_effort"], "low");
            let llm = llm.with_cli_reasoning_effort(Some("high"));
            assert_eq!(llm.body(&[], None, false)["reasoning_effort"], "high");
            assert_eq!(llm.reasoning_effort.unwrap().1, "--reasoning-effort");

            let mut llm = OpenAiLlm::openai("test", "test");
            llm.set_reasoning_effort(Some(" \t"), "OPENAI_REASONING_EFFORT");
            assert!(llm.reasoning_effort.is_none());
            assert!(llm.body(&[], None, false).get("reasoning_effort").is_none());
        }

        /// Der 400er zu Tools + Reasoning schaltet `reasoning_effort: "none"`
        /// ein — aber nur für Requests mit Tools, und nicht bei anderen Fehlern.
        #[test]
        fn reasoning_effort_none_nach_tools_400() {
            assert!(needs_no_reasoning(
                r#"HTTP 400: { "error": { "message": "Function tools with reasoning_effort are not supported for gpt-6.1-terra in /v1/chat/completions.""#
            ));
            assert!(!needs_no_reasoning("HTTP 400: bad messages"));
            assert!(!needs_no_reasoning(
                "HTTP 429 (Rate-Limit): reasoning_effort"
            ));

            let llm = OpenAiLlm::openai("k", "m");
            let tools = [json!({"type": "function"})];
            assert!(llm.body(&[], Some(&tools), false)["reasoning_effort"].is_null());
            llm.no_reasoning.store(true, Ordering::Relaxed);
            assert_eq!(
                llm.body(&[], Some(&tools), false)["reasoning_effort"],
                "none"
            );
            assert!(llm.body(&[], None, false)["reasoning_effort"].is_null());
        }

        fn chunks_aus(sse: &[&str]) -> Vec<Result<Chunk, String>> {
            let lines: Vec<std::io::Result<String>> =
                sse.iter().map(|l| Ok(l.to_string())).collect();
            sse_chunks(lines.into_iter()).collect()
        }

        /// `data:` ohne Leerzeichen ist gültiges SSE; vorher wurde jede solche
        /// Zeile übersprungen und der Stream endete als „abgeschnitten".
        #[test]
        fn data_ohne_leerzeichen_wird_gelesen() {
            let out = chunks_aus(&[
                r#"data:{"choices":[{"delta":{"content":"Hal"}}]}"#,
                "",
                r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
                "data:[DONE]",
            ]);
            let text: String = out
                .into_iter()
                .map(|c| c.unwrap().delta.content.unwrap())
                .collect();
            assert_eq!(text, "Hallo");
        }

        /// Ein Fehler-Payload mitten im Stream ist ein Fehler mit der Meldung
        /// des Servers — kein leeres Delta. Danach kommt nichts mehr.
        #[test]
        fn fehler_payload_im_stream_ist_ein_fehler() {
            let out = chunks_aus(&[
                r#"data: {"choices":[{"delta":{"content":"Teil"}}]}"#,
                r#"data: {"error":{"message":"The server had an error","type":"server_error"}}"#,
                r#"data: {"choices":[{"delta":{"content":"nie"}}]}"#,
            ]);
            assert_eq!(out.len(), 2);
            assert_eq!(
                out[1].as_ref().unwrap_err(),
                "Fehler im Stream (server_error): The server had an error"
            );
        }

        /// `finish_reason: "content_filter"` ist kein reguläres Ende: der schon
        /// gelieferte Text kommt an, dann der Fehler mit dem Marker, an dem der
        /// Loop erkennt, dass eine Wiederholung nichts bringt.
        #[test]
        fn content_filter_ist_kein_regulaeres_ende() {
            let out = chunks_aus(&[
                r#"data: {"choices":[{"delta":{"content":"Anfang"}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#,
                "data: [DONE]",
            ]);
            assert_eq!(out.len(), 2);
            assert_eq!(
                out[0].as_ref().unwrap().delta.content.as_deref(),
                Some("Anfang")
            );
            let e = out[1].as_ref().unwrap_err();
            assert!(e.contains("Inhaltsfilter") && e.contains("content_filter"));
        }

        /// `finish_reason: "length"` = an der Ausgabegrenze abgeschnitten.
        #[test]
        fn finish_reason_length_meldet_abgeschnitten() {
            let c = parse_delta(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
            assert!(c.truncated);
            let d =
                parse_delta(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#).unwrap();
            assert!(!d.truncated);
        }
    }
}
