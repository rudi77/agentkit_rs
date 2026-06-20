//! Memory — Kurzzeit (die Konversation) und Langzeit (über Sessions hinweg).
//!
//! - [`ShortTermMemory`]: die `messages`-Liste = das Arbeitsgedächtnis. Misst Tokens,
//!   kürzt (truncation) und fasst alte Historie zusammen (compaction).
//! - [`LongTermMemory`]: ein dateibasierter Notizspeicher (JSONL), der Sessions
//!   überdauert. Bewusst ohne Embeddings: Abruf per Stichwort-Overlap. Wird dem
//!   Agenten als Tools `remember` / `recall` angeboten.

use crate::llm::Llm;
use crate::tools::ToolRegistry;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Grobe Token-Schätzung (Zeichen/4) — entspricht Pythons Fallback ohne tiktoken.
/// Bewusst dieselbe Heuristik wie der Python-Port, damit Benchmarks vergleichbar sind.
pub fn count_tokens_text(text: &str) -> usize {
    text.chars().count() / 4
}

/// Kürzt riesige Tool-Outputs, statt sie ungefiltert anzuhängen (limit in Zeichen).
pub fn truncate(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n…[{} Zeichen gekürzt]", total - limit)
}

pub const TRUNCATE_LIMIT: usize = 2000;

/// Die Message-Historie + Context-Engineering darauf.
pub struct ShortTermMemory {
    pub messages: Vec<Value>,
}

impl ShortTermMemory {
    pub fn new(system: Option<&str>) -> Self {
        let mut messages = Vec::new();
        if let Some(s) = system {
            if !s.is_empty() {
                messages.push(json!({"role": "system", "content": s}));
            }
        }
        ShortTermMemory { messages }
    }

    pub fn add(&mut self, message: Value) {
        self.messages.push(message);
    }

    pub fn add_user(&mut self, content: &str) {
        self.messages
            .push(json!({"role": "user", "content": content}));
    }

    pub fn tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|m| count_tokens_text(m.get("content").and_then(Value::as_str).unwrap_or("")))
            .sum()
    }

    /// Fasst alte Nachrichten zu einer kurzen Notiz zusammen; behält die letzten
    /// paar im Original. System-Nachricht bleibt erhalten. Achtet darauf, dass der
    /// behaltene Schwanz nicht mit verwaisten `tool`-Nachrichten beginnt (würde das
    /// tool_call/tool-Pairing brechen). Gibt `true` zurück, wenn komprimiert wurde.
    pub fn compact(&mut self, llm: &dyn Llm, keep_last: usize) -> bool {
        let role = |m: &Value| {
            m.get("role")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };

        let system: Vec<Value> = self
            .messages
            .iter()
            .filter(|m| role(m) == "system")
            .take(1)
            .cloned()
            .collect();
        let body: Vec<Value> = self
            .messages
            .iter()
            .filter(|m| role(m) != "system")
            .cloned()
            .collect();
        if body.len() <= keep_last {
            return false;
        }

        let split = body.len() - keep_last;
        let mut head: Vec<Value> = body[..split].to_vec();
        let mut tail: Vec<Value> = body[split..].to_vec();
        while !tail.is_empty() && role(&tail[0]) == "tool" {
            head.push(tail.remove(0));
        }
        if head.is_empty() {
            return false;
        }

        let digest_items: Vec<Value> = head
            .iter()
            .map(|m| json!({"role": m.get("role"), "content": m.get("content")}))
            .collect();
        let digest = serde_json::to_string(&digest_items).unwrap_or_default();

        let prompt = format!(
            "Fasse den folgenden Agenten-Verlauf in 3-5 Stichpunkten zusammen \
             (wichtige Fakten, Zwischenergebnisse, offene Punkte):\n{digest}"
        );
        let summary = llm
            .complete(&[json!({"role": "user", "content": prompt})], None)
            .map(|m| m.content.unwrap_or_default())
            .unwrap_or_default();

        let mut rebuilt = system;
        rebuilt.push(json!({
            "role": "system",
            "content": format!("Bisheriger Verlauf (komprimiert):\n{summary}"),
        }));
        rebuilt.extend(tail);
        self.messages = rebuilt;
        true
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct MemoryItem {
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Persistentes Langzeitgedächtnis (JSONL-Datei) mit Stichwort-Abruf.
///
/// `Clone` über einen geteilten `Arc<Mutex<…>>`-Kern, damit die als Tools
/// registrierten Closures denselben Speicher sehen wie das `LongTermMemory`-Handle.
#[derive(Clone)]
pub struct LongTermMemory {
    path: PathBuf,
    items: Arc<Mutex<Vec<MemoryItem>>>,
}

impl LongTermMemory {
    pub fn new(path: &str) -> Self {
        let path = PathBuf::from(path);
        let mut items = Vec::new();
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    if let Ok(item) = serde_json::from_str::<MemoryItem>(line) {
                        items.push(item);
                    }
                }
            }
        }
        LongTermMemory {
            path,
            items: Arc::new(Mutex::new(items)),
        }
    }

    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn remember(&self, text: &str, tags: Vec<String>) -> String {
        let item = MemoryItem {
            text: text.to_string(),
            tags: tags.iter().map(|t| t.to_lowercase()).collect(),
        };
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Ok(line) = serde_json::to_string(&item) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.items.lock().unwrap().push(item);
        "gespeichert.".to_string()
    }

    pub fn recall(&self, query: &str, k: usize) -> String {
        use std::collections::HashSet;
        let q_lower = query.to_lowercase();
        let q: HashSet<&str> = q_lower.split_whitespace().collect();
        let items = self.items.lock().unwrap();
        // Treffer als (score, &text) sammeln — erst die Top-k werden kopiert.
        let mut scored: Vec<(usize, &str)> = Vec::new();
        for it in items.iter() {
            let text_lower = it.text.to_lowercase();
            let mut words: HashSet<&str> = text_lower.split_whitespace().collect();
            words.extend(it.tags.iter().map(String::as_str));
            let score = q.intersection(&words).count();
            if score > 0 {
                scored.push((score, it.text.as_str()));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let hits: Vec<String> = scored
            .into_iter()
            .take(k)
            .map(|(_, t)| format!("- {t}"))
            .collect();
        if hits.is_empty() {
            format!("(nichts zu '{query}' gefunden)")
        } else {
            hits.join("\n")
        }
    }

    /// Bietet dem Agenten `remember`/`recall` als Tools an.
    pub fn register_tools(&self, registry: &mut ToolRegistry) {
        let me = self.clone();
        registry.add(
            "remember",
            "Speichert eine wichtige Information dauerhaft im Langzeitgedächtnis.",
            json!({"type": "object",
                   "properties": {"text": {"type": "string", "description": "Die zu merkende Information."}},
                   "required": ["text"]}),
            move |args: Value| {
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                Ok(me.remember(text, Vec::new()))
            },
        );
        let me = self.clone();
        registry.add(
            "recall",
            "Durchsucht das Langzeitgedächtnis nach relevanten, früher gespeicherten Informationen.",
            json!({"type": "object",
                   "properties": {"query": {"type": "string", "description": "Wonach gesucht wird."}},
                   "required": ["query"]}),
            move |args: Value| {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                Ok(me.recall(query, 3))
            },
        );
    }
}
