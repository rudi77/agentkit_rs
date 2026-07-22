use serde::{Deserialize, Serialize};

use super::policy::PolicyConfig;

/// Watermark-Status ok|soft|hard|emergency (Spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkLevel {
    Ok,
    Soft,
    Hard,
    Emergency,
}

impl WatermarkLevel {
    /// Leitet den Watermark-Status aus den verbrauchten Tokens relativ zum Budget ab
    /// (Spec §3.1): höchste überschrittene Schwelle gewinnt; keine überschritten ⇒ `Ok`.
    /// Reine Berechnung — kein GC wird ausgelöst. (Port von `WatermarkState.Derive`.)
    pub fn derive(tokens_used: i64, policy: &PolicyConfig) -> Self {
        let ratio = if policy.budget_tokens == 0 {
            0.0
        } else {
            tokens_used as f64 / f64::from(policy.budget_tokens)
        };
        let w = &policy.watermarks;

        if ratio >= w.emergency {
            WatermarkLevel::Emergency
        } else if ratio >= w.hard {
            WatermarkLevel::Hard
        } else if ratio >= w.soft {
            WatermarkLevel::Soft
        } else {
            WatermarkLevel::Ok
        }
    }

    /// snake_case-Wire-Repräsentation (Spec §3.1 / §6).
    pub fn wire_name(&self) -> &'static str {
        match self {
            WatermarkLevel::Ok => "ok",
            WatermarkLevel::Soft => "soft",
            WatermarkLevel::Hard => "hard",
            WatermarkLevel::Emergency => "emergency",
        }
    }
}

impl std::fmt::Display for WatermarkLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}
