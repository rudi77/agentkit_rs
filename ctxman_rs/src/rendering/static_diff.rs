use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::domain::{Role, Segment};

/// Neutraler Static-Segment-Input für Diff und Epoch-Bump (Spec §4.2;
/// Port von `StaticSegmentInput`).
#[derive(Debug, Clone, PartialEq)]
pub struct StaticSegmentSpec {
    pub kind: String,
    pub role: Option<Role>,
    pub content: String,
    pub source: Option<String>,
}

/// Diff der Static-Region über `source` und Tool-Namen der `tool_def`-Segmente (Spec §4.2).
/// Alle Listen sind ordinal sortiert (deterministische Event-Payloads).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StaticRegionDiffResult {
    pub added_tools: Vec<String>,
    pub removed_tools: Vec<String>,
    pub added_sources: Vec<String>,
    pub removed_sources: Vec<String>,
}

/// Berechnet den Tool-/Source-Diff zwischen alter Static-Region und neuem Input (Spec §4.2).
/// (Port von `StaticRegionDiff.Compute`.)
pub fn compute(old_static: &[Segment], new_segments: &[StaticSegmentSpec]) -> StaticRegionDiffResult {
    let old_tools: HashSet<String> = old_static
        .iter()
        .filter(|s| s.kind() == "tool_def")
        .filter_map(|s| try_extract_tool_name(s.content()))
        .collect();
    let new_tools: HashSet<String> = new_segments
        .iter()
        .filter(|s| s.kind == "tool_def")
        .filter_map(|s| try_extract_tool_name(Some(&s.content)))
        .collect();
    let old_sources: HashSet<String> = old_static
        .iter()
        .filter_map(|s| s.source())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .collect();
    let new_sources: HashSet<String> = new_segments
        .iter()
        .filter_map(|s| s.source.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .collect();

    let sorted_diff = |a: &HashSet<String>, b: &HashSet<String>| {
        let mut d: Vec<String> = a.difference(b).cloned().collect();
        d.sort_unstable();
        d
    };

    StaticRegionDiffResult {
        added_tools: sorted_diff(&new_tools, &old_tools),
        removed_tools: sorted_diff(&old_tools, &new_tools),
        added_sources: sorted_diff(&new_sources, &old_sources),
        removed_sources: sorted_diff(&old_sources, &new_sources),
    }
}

/// Sammelt die `tool_call_id`s aller Working-`tool_call`-Segmente, die ein entferntes Tool
/// bzw. eine entfernte Quelle referenzieren (Basis für `on_tool_removed`, Spec §4.2).
pub fn affected_tool_call_ids(
    working_segments: &[&Segment],
    diff: &StaticRegionDiffResult,
) -> HashSet<String> {
    let removed_tools: HashSet<&str> = diff.removed_tools.iter().map(String::as_str).collect();
    let removed_sources: HashSet<&str> = diff.removed_sources.iter().map(String::as_str).collect();

    working_segments
        .iter()
        .filter(|s| s.kind() == "tool_call" && s.tool_call_id().is_some())
        .filter(|s| {
            s.source()
                .map(|source| removed_tools.contains(source) || removed_sources.contains(source))
                .unwrap_or(false)
        })
        .filter_map(|s| s.tool_call_id().map(str::to_string))
        .collect()
}

/// Extrahiert den Tool-Namen aus einem `tool_def`-Content: JSON-Property `name`;
/// Nicht-JSON-Content fällt auf den getrimmten Gesamt-Content zurück.
fn try_extract_tool_name(content: Option<&str>) -> Option<String> {
    let content = content?;
    if content.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(value) => value
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string),
        Err(_) => Some(content.trim().to_string()),
    }
}
