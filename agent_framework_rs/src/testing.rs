//! FakeLlm — spielt vorgegebene Streaming-"Turns" ab, ohne Netz.
//!
//! Pendant zu Pythons `FakeLLM` aus den Tests: jeder Turn ist eine Liste von
//! [`Chunk`]s (Streaming-Deltas). Damit lassen sich der Agent-Loop, Tools, Memory,
//! Events und Sub-Agents deterministisch testen — und die Benchmarks messen reinen
//! Framework-Overhead statt Netzwerklatenz.

use crate::llm::{chunk_stream, Chunk, ChunkStream, Llm, Message};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Spielt eine Liste von Turns ab; jeder Turn ist eine Liste von Chunks.
pub struct FakeLlm {
    turns: Vec<Vec<Chunk>>,
    i: AtomicUsize,
    /// Bei jedem `stream()`-Aufruf die gesehenen Nachrichten (für Assertions).
    pub seen_messages: Mutex<Vec<Vec<Value>>>,
    /// Dasselbe für `complete()` — dort läuft die Compaction, deren Prompt
    /// sich sonst nicht prüfen ließe (`/compact <hinweis>`).
    pub seen_completes: Mutex<Vec<Vec<Value>>>,
    /// Was `complete()` (Compaction) zurückgibt.
    complete_reply: String,
    /// Anzahl der `complete()`-Aufrufe (z. B. Compaction/Fact-Extraction).
    completes: AtomicUsize,
}

impl FakeLlm {
    pub fn new(turns: Vec<Vec<Chunk>>) -> Self {
        FakeLlm {
            turns,
            i: AtomicUsize::new(0),
            seen_messages: Mutex::new(Vec::new()),
            seen_completes: Mutex::new(Vec::new()),
            complete_reply: "komprimierte Zusammenfassung".to_string(),
            completes: AtomicUsize::new(0),
        }
    }

    /// Anzahl bisher konsumierter Turns.
    pub fn calls(&self) -> usize {
        self.i.load(Ordering::SeqCst)
    }

    /// Anzahl bisheriger `complete()`-Aufrufe (Compaction/Fact-Extraction) —
    /// damit lässt sich prüfen, WELCHES LLM die Kompaktierung wirklich fährt.
    pub fn complete_calls(&self) -> usize {
        self.completes.load(Ordering::SeqCst)
    }
}

impl Llm for FakeLlm {
    fn complete(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<Message, String> {
        self.seen_completes.lock().unwrap().push(messages.to_vec());
        self.completes.fetch_add(1, Ordering::SeqCst);
        Ok(Message {
            content: Some(self.complete_reply.clone()),
            tool_calls: Vec::new(),
        })
    }

    fn stream(&self, messages: &[Value], _tools: Option<&[Value]>) -> Result<ChunkStream, String> {
        self.seen_messages.lock().unwrap().push(messages.to_vec());
        let idx = self.i.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.get(idx).cloned().unwrap_or_default();
        Ok(chunk_stream(turn))
    }
}
