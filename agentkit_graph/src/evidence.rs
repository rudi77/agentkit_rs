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

use crate::write::SourceDraft;

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
    /// Was das hier ist — zugleich der Quellentyp der Aussage, die es belegt.
    source_type: &'static str,
    /// Kennung des Aufrufs (`<tool>#<laufende Nummer>` bzw. die echte
    /// `tool_call_id`) — steht später an der Quelle. `None` für eine Aussage
    /// des Nutzers.
    call: Option<String>,
    /// Leerraum zusammengezogen (siehe [`squash`]).
    text: String,
}

/// Ein Fund im Buch: was eine Belegstelle stützt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Witness {
    pub source_type: &'static str,
    pub call: Option<String>,
}

impl Witness {
    /// Die Quelle, die eine so belegte Aussage bekommt.
    pub(crate) fn source(&self, excerpt: &str) -> SourceDraft {
        let source = SourceDraft::new(self.source_type).excerpt(excerpt);
        match &self.call {
            Some(call) => source.tool_call(call),
            None => source,
        }
    }
}

const TOOL_RESULT: &str = "tool_result";
/// Quellentyp für das, was der Nutzer selbst gesagt hat.
pub const USER_STATEMENT: &str = "user_statement";

#[derive(Default)]
pub(crate) struct EvidenceLog {
    inner: Mutex<(u64, VecDeque<Witnessed>)>,
}

impl EvidenceLog {
    fn push(&self, source_type: &'static str, label: Option<&str>, text: &str) {
        let mut inner = self.inner.lock().expect("Evidenz-Lock nicht poisoned");
        inner.0 += 1;
        let call = label.map(|label| format!("{label}#{}", inner.0));
        if inner.1.len() == KEEP {
            inner.1.pop_front();
        }
        inner.1.push_back(Witnessed {
            source_type,
            call,
            text: clip(text),
        });
    }

    fn record(&self, tool: &str, result: &str) {
        self.push(TOOL_RESULT, Some(tool), result);
    }

    /// Ein Buch aus einem fertigen Verlauf statt aus laufenden Aufrufen:
    /// Tool-Ergebnisse unter ihrer echten Aufruf-Kennung, dazu die Nachrichten
    /// des Nutzers. Ohne die Obergrenze von [`KEEP`] — der Verlauf ist schon
    /// da, er wächst hier nicht.
    pub(crate) fn from_transcript<'a>(
        tool_results: impl Iterator<Item = (String, &'a str)>,
        user_messages: impl Iterator<Item = &'a str>,
    ) -> Self {
        let seen: VecDeque<Witnessed> = tool_results
            .map(|(call, text)| Witnessed {
                source_type: TOOL_RESULT,
                call: Some(call),
                text: clip(text),
            })
            .chain(user_messages.map(|text| Witnessed {
                source_type: USER_STATEMENT,
                call: None,
                text: clip(text),
            }))
            .collect();
        EvidenceLog {
            inner: Mutex::new((seen.len() as u64, seen)),
        }
    }

    /// Ob ein Tool als Beleg zählt (siehe [`NOT_EVIDENCE`]).
    pub(crate) fn counts(tool: &str) -> bool {
        !NOT_EVIDENCE.contains(&tool) && !NOT_EVIDENCE_PREFIXES.iter().any(|p| tool.starts_with(p))
    }

    /// Der jüngste Eintrag — Tool-Ergebnis oder Nutzer-Aussage —, der `excerpt` wörtlich enthält
    /// (Leerraum zählt nicht — Modelle geben Umbrüche und Einrückung selten
    /// zeichengenau wieder).
    pub(crate) fn find(&self, excerpt: &str) -> Option<Witness> {
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
            .map(|seen| Witness {
                source_type: seen.source_type,
                call: seen.call.clone(),
            })
    }

    /// Hängt das Buch an alle Tools, die JETZT in der Registry stehen. Die
    /// Graph-Tools selbst kommen danach dazu und sind damit nie Beleg.
    pub(crate) fn watch(self: &Arc<Self>, tools: &mut ToolRegistry) {
        for name in tools.names() {
            if !Self::counts(&name) {
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

/// Das Buch eines Laufs, von außen erreichbar: für das, was die Graph-Tools
/// nicht selbst mitschneiden können.
///
/// `register_federated_graph_tools` hängt sich an die Tools, die bei seinem
/// Aufruf schon registriert sind. Zwei Dinge sieht es so nie: Tools, die
/// später dazukommen (MCP-Server), und das, was der Nutzer sagt. Beides reicht
/// das Frontend über diesen Griff nach — er gehört zum [`crate::GraphFederation`]
/// und wird mit ihm geteilt.
#[derive(Clone, Default)]
pub struct GraphEvidence(pub(crate) Arc<EvidenceLog>);

impl GraphEvidence {
    /// Ein Tool-Ergebnis, das nicht über die Registry mitgeschnitten wird.
    /// Tools, deren Ergebnis selbst von einem Modell stammt, zählen auch hier
    /// nicht.
    pub fn witness_tool(&self, tool: &str, result: &str) {
        if EvidenceLog::counts(tool) {
            self.0.record(tool, result);
        }
    }

    /// Was der Nutzer gesagt hat. Eine Aussage, die ihn wörtlich zitiert, hat
    /// damit eine fremde Quelle (`user_statement`).
    pub fn witness_user(&self, text: &str) {
        self.0.push(USER_STATEMENT, None, text);
    }
}

fn clip(result: &str) -> String {
    let mut text = squash(result);
    if let Some((cut, _)) = text.char_indices().nth(MAX_CHARS) {
        text.truncate(cut);
    }
    text
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
            log.find("2 passed;   1 failed")
                .and_then(|w| w.call)
                .as_deref(),
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
