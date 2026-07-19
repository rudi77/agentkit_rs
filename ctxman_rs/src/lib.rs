//! # ctxman — Context-Management für LLM-Agents (Rust-Port)
//!
//! Rust-Port des deterministischen Cores von ctxman (C#/.NET 9, `docs/ctxman-spec.md` v0.2).
//! Mentales Modell: Speicherverwaltung — Static-Region (Stack), Working Set (Heap),
//! Garbage Collector (Externalisierung, Eviction, Compaction, Promotion).
//!
//! Eigenständige, synchrone Bibliothek ohne Web-Interface: der Host (z. B. ein Agent-Loop)
//! ruft `ContextSession` direkt auf. ctxman ruft **nie** selbst das LLM des Agents auf
//! (Spec Non-Goal N1) — Compaction/Promotion laufen über vom Host implementierte Traits.

pub mod domain;
pub mod error;
pub mod rendering;
pub mod tokenization;

pub use error::CtxmanError;
