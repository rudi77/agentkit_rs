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

/// Ein Streaming-Chunk (vereinfacht: genau ein `choice`).
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub delta: Delta,
}

impl Chunk {
    /// Bequemer Konstruktor für reine Text-Deltas (Tests/Benchmarks).
    pub fn text(s: &str) -> Self {
        Chunk {
            delta: Delta {
                content: Some(s.to_string()),
                tool_calls: Vec::new(),
            },
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
pub type ChunkStream = Box<dyn Iterator<Item = Chunk> + Send>;

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
mod openai {
    //! Echter OpenAI/Azure-Pfad über `ureq` (synchron, SSE zeilenweise geparst).

    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader};

    /// Wickelt einen OpenAI-kompatiblen Endpunkt + Modell/Deployment.
    pub struct OpenAiLlm {
        url: String,
        api_key: String,
        /// Azure verlangt den Key im Header `api-key`, OpenAI in `Authorization`.
        azure: bool,
        model: String,
    }

    impl OpenAiLlm {
        /// Standard-OpenAI: `https://api.openai.com/v1/chat/completions`.
        pub fn openai(api_key: &str, model: &str) -> Self {
            OpenAiLlm {
                url: "https://api.openai.com/v1/chat/completions".to_string(),
                api_key: api_key.to_string(),
                azure: false,
                model: model.to_string(),
            }
        }

        /// Azure-OpenAI: vollständige Deployment-URL inkl. `api-version`.
        pub fn azure(endpoint: &str, api_key: &str, deployment: &str, api_version: &str) -> Self {
            let base = endpoint.trim_end_matches('/');
            let url = format!(
                "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
            );
            OpenAiLlm {
                url,
                api_key: api_key.to_string(),
                azure: true,
                model: deployment.to_string(),
            }
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
            if stream {
                body["stream"] = json!(true);
            }
            body
        }

        fn request(&self) -> ureq::Request {
            let req = ureq::post(&self.url).set("Content-Type", "application/json");
            if self.azure {
                req.set("api-key", &self.api_key)
            } else {
                req.set("Authorization", &format!("Bearer {}", self.api_key))
            }
        }
    }

    impl Llm for OpenAiLlm {
        fn complete(&self, messages: &[Value], tools: Option<&[Value]>) -> Result<Message, String> {
            let body = self.body(messages, tools, false);
            let resp = self.request().send_json(body).map_err(|e| e.to_string())?;
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
            let body = self.body(messages, tools, true);
            let resp = self.request().send_json(body).map_err(|e| e.to_string())?;
            let reader = BufReader::new(resp.into_reader());
            // SSE: Zeilen der Form `data: {json}`; `data: [DONE]` beendet den Strom.
            let iter = reader.lines().filter_map(|line| {
                let line = line.ok()?;
                let payload = line.strip_prefix("data: ")?;
                if payload.trim() == "[DONE]" {
                    return None;
                }
                let v: Value = serde_json::from_str(payload).ok()?;
                let delta = &v["choices"][0]["delta"];
                let content = delta["content"].as_str().map(String::from);
                let mut tool_calls = Vec::new();
                if let Some(arr) = delta["tool_calls"].as_array() {
                    for tc in arr {
                        tool_calls.push(ToolCallDelta {
                            index: tc["index"].as_u64().unwrap_or(0) as usize,
                            id: tc["id"].as_str().map(String::from),
                            name: tc["function"]["name"].as_str().map(String::from),
                            arguments: tc["function"]["arguments"].as_str().map(String::from),
                        });
                    }
                }
                Some(Chunk {
                    delta: Delta {
                        content,
                        tool_calls,
                    },
                })
            });
            Ok(Box::new(iter))
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
        ))
    }

    /// Baut einen Standard-OpenAI-LLM (`OPENAI_API_KEY`, optional `OPENAI_MODEL`).
    pub fn openai_from_env() -> Result<OpenAiLlm, String> {
        let key =
            std::env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY fehlt".to_string())?;
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
        Ok(OpenAiLlm::openai(&key, &model))
    }
}
