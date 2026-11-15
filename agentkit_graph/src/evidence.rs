//! Was die Tools dieses Laufs wirklich geliefert haben — die Grundlage dafür,
//! eine Belegstelle als **fremde Quelle** anzuerkennen.
//!
//! `graph_remember` nimmt eine Belegstelle vom Modell entgegen. Ungeprüft wäre
//! sie nur eine weitere Behauptung des Modells, und das Beförderungs-Gate
//! (`write::promote_claim`) prüfte nichts. Deshalb führt die Laufzeit hier Buch:
//! jedes Tool-Ergebnis wird mitgeschrieben, und eine Belegstelle zählt genau
//! dann, wenn sie **wörtlich** in einem dieser Ergebnisse steht. Das Modell
//! muss dafür keine Aufruf-ID kennen — es zitiert, die Laufzeit findet.
//!
//! Grenzen, bewusst: Tools, die erst NACH den Graph-Tools registriert werden
//! (MCP-Server), sieht das Buch nicht. Und wer eine Datei schreibt und wieder
//! liest, kann sich selbst zitieren — das Gate hebt die Schwelle von „sagen"
//! auf „in der Welt vorfinden", es beweist keine Wahrheit.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agentkit::ToolRegistry;

/// So viele Ergebnisse bleiben im Buch. Eine Belegstelle stammt praktisch
/// immer aus den letzten paar Aufrufen; ohne Grenze wüchse das Buch mit der
/// Sitzung.
const KEEP: usize = 64;

/// Zeichen, die von einem einzelnen Ergebnis behalten werden.
const MAX_CHARS: usize = 64_000;

/// Kürzere Belegstellen zählen nicht: `ok` oder `0` steht in fast jedem
/// Ergebnis und belegte damit alles.
const MIN_EXCERPT_CHARS: usize = 12;

/// Tools, deren Ergebnis das Modell selbst geschrieben hat oder das von einem
/// anderen Modell stammt — kein Beleg von außen.
const NOT_EVIDENCE: &[&str] = &["task", "swarm", "update_plan", "remember", "recall"];
const NOT_EVIDENCE_PREFIXES: &[&str] = &["graph_", "swarm_", "work_"];

struct Witnessed {
    /// `<tool>#<laufende Nummer>` — steht später als `tool_call_id` an der Quelle.
    call: String,
    /// Leerraum zusammengezogen (siehe [`squash`]).
    text: String,
}

#[derive(Default)]
pub(crate) struct EvidenceLog {
    inner: Mutex<(u64, VecDeque<Witnessed>)>,
}

impl EvidenceLog {
    fn record(&self, tool: &str, result: &str) {
        let mut text = squash(result);
        if let Some((cut, _)) = text.char_indices().nth(MAX_CHARS) {
            text.truncate(cut);
        }
        let mut inner = self.inner.lock().expect("Evidenz-Lock nicht poisoned");
        inner.0 += 1;
        let call = format!("{tool}#{}", inner.0);
        if inner.1.len() == KEEP {
            inner.1.pop_front();
        }
        inner.1.push_back(Witnessed { call, text });
    }

    /// Der jüngste Tool-Aufruf, dessen Ergebnis `excerpt` wörtlich enthält
    /// (Leerraum zählt nicht — Modelle geben Umbrüche und Einrückung selten
    /// zeichengenau wieder).
    pub(crate) fn find(&self, excerpt: &str) -> Option<String> {
        let needle = squash(excerpt);
        if needle.chars().count() < MIN_EXCERPT_CHARS {
            return None;
        }
        let inner = self.inner.lock().expect("Evidenz-Lock nicht poisoned");
        inner
            .1
            .iter()
            .rev()
            .find(|seen| seen.text.contains(&needle))
            .map(|seen| seen.call.clone())
    }

    /// Hängt das Buch an alle Tools, die JETZT in der Registry stehen. Die
    /// Graph-Tools selbst kommen danach dazu und sind damit nie Beleg.
    pub(crate) fn watch(self: &Arc<Self>, tools: &mut ToolRegistry) {
        for name in tools.names() {
            if NOT_EVIDENCE.contains(&name.as_str())
                || NOT_EVIDENCE_PREFIXES.iter().any(|p| name.starts_with(p))
            {
                continue;
            }
            let log = self.clone();
            let tool = name.clone();
            tools.wrap(&name, move |inner| {
                Arc::new(move |args| {
                    let result = inner(args);
                    if let Ok(text) = &result {
                        log.record(&tool, text);
                    }
                    result
                })
            });
        }
    }
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eine_belegstelle_zaehlt_nur_woertlich_und_ab_mindestlaenge() {
        let log = EvidenceLog::default();
        log.record("run_shell", "test result: FAILED.\n   2 passed; 1 failed");
        assert_eq!(
            log.find("2 passed;   1 failed").as_deref(),
            Some("run_shell#1"),
            "Leerraum zählt nicht"
        );
        assert_eq!(log.find("3 passed; 0 failed"), None, "steht nirgends");
        assert_eq!(log.find("FAILED"), None, "zu kurz, um etwas zu belegen");
    }

    #[test]
    fn das_buch_vergisst_die_aeltesten_ergebnisse() {
        let log = EvidenceLog::default();
        log.record("read_file", "der allererste Dateiinhalt");
        for i in 0..KEEP {
            log.record("read_file", &format!("späterer Inhalt Nummer {i}"));
        }
        assert_eq!(log.find("der allererste Dateiinhalt"), None);
        assert!(log.find("späterer Inhalt Nummer 0").is_some());
    }
}
