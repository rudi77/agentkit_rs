use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::enums::OnToolRemoved;
use crate::error::CtxmanError;

/// Watermark-Schwellen relativ zum Modell-Context-Budget B (Spec §3.1 / §5).
/// Defaults: soft 0.60·B, hard 0.80·B, emergency 0.95·B.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watermarks {
    pub soft: f64,
    pub hard: f64,
    pub emergency: f64,
}

/// Pro-Kind-Policy (Spec §5). `ttl_turns == None` bildet unendliche TTL ab
/// (in der Spec als ∞ notiert, z. B. für decision/task).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KindPolicy {
    #[serde(default)]
    pub ttl_turns: Option<u32>,
    #[serde(default)]
    pub externalize: bool,
    #[serde(default)]
    pub refetchable: bool,
    #[serde(default)]
    pub promote: bool,
}

/// Konfiguration der Major-Collection-Compaction (Spec §3.3 / §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub model: String,
    pub prompt_template_id: String,
    pub max_share: f64,
}

/// Senke für die Fact-Promotion (Spec §3.3 / §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionSinkConfig {
    #[serde(rename = "type")]
    pub sink_type: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// Promotion-Konfiguration (Spec §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionConfig {
    pub sink: PromotionSinkConfig,
}

/// Blob-Retention/Mark-and-Sweep-Konfiguration (Spec §7.1). `blob_grace_hours` und
/// `sweep_interval` sind das Wire-/Config-Format; `blob_grace()` und `sweep_interval_span()`
/// liefern die konsumierten `Duration`-Werte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionConfig {
    pub blob_grace_hours: u32,
    pub evicted_blob_retention_days: u32,
    pub archived_session_blobs: String,
    pub sweep_interval: String,
}

impl RetentionConfig {
    /// Mindest-Alter vor Sweep als `Duration` (Spec §7.1: `blob_grace_hours`).
    pub fn blob_grace(&self) -> Duration {
        Duration::from_secs(u64::from(self.blob_grace_hours) * 3600)
    }

    /// Sweep-Intervall als `Duration` (Spec §7.1: `sweep_interval`, z. B. "24h").
    pub fn sweep_interval_span(&self) -> Result<Duration, CtxmanError> {
        parse_duration(&self.sweep_interval)
    }
}

/// Parst eine Dauer aus dem Config-Format (Spec §7.1): entweder mit Suffix (`h` Stunden,
/// `m` Minuten, `s` Sekunden, `d` Tage) wie "24h", oder eine reine Zahl, die als Stunden
/// interpretiert wird. Kultur-invariant (Dezimalpunkt).
pub fn parse_duration(value: &str) -> Result<Duration, CtxmanError> {
    let trimmed = value.trim();
    let invalid = || CtxmanError::InvalidDuration(value.to_string());

    let Some(unit) = trimmed.chars().last() else {
        return Err(invalid());
    };

    let (magnitude, seconds_per_unit) = if unit.is_ascii_alphabetic() {
        let number = &trimmed[..trimmed.len() - unit.len_utf8()];
        let magnitude: f64 = number.parse().map_err(|_| invalid())?;
        let factor = match unit.to_ascii_lowercase() {
            'h' => 3600.0,
            'm' => 60.0,
            's' => 1.0,
            'd' => 86_400.0,
            _ => return Err(invalid()),
        };
        (magnitude, factor)
    } else {
        // Reine Zahl ⇒ Stunden (Spec §7.1).
        let magnitude: f64 = trimmed.parse().map_err(|_| invalid())?;
        (magnitude, 3600.0)
    };

    let seconds = magnitude * seconds_per_unit;
    // Divergenz zu C# (TimeSpan kann negativ sein): Rusts Duration ist vorzeichenlos —
    // negative/nicht-endliche Dauern sind hier Konfigurationsfehler.
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// Effektive, deklarative Policy einer Session (Spec §5 + §7.1). Wird bei Session-Erstellung
/// als Snapshot eingefroren (Reproduzierbarkeit). `BTreeMap` statt HashMap: deterministische
/// Iterations-Reihenfolge (I4-Geist — keine zufällige Ordnung im Verhalten).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub budget_tokens: u32,
    pub watermarks: Watermarks,
    pub externalize_threshold_tokens: u32,
    pub tokenizer: String,
    pub kinds: BTreeMap<String, KindPolicy>,
    pub compaction: CompactionConfig,
    pub promotion: PromotionConfig,
    pub retention: RetentionConfig,
    #[serde(default)]
    pub on_tool_removed: OnToolRemoved,
}

impl PolicyConfig {
    /// Spec-Defaults aus §5 (Beispiel-Policy) und §3.1 (Watermarks) / §7.1 (retention) —
    /// exakter Port von `PolicyConfig.Default()`. Unendliche TTL (∞) ist als `None` in
    /// `KindPolicy::ttl_turns` abgebildet.
    pub fn default_policy() -> Self {
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "tool_result".to_string(),
            KindPolicy { ttl_turns: Some(2), externalize: true, ..Default::default() },
        );
        kinds.insert(
            "ref_expansion".to_string(),
            KindPolicy { ttl_turns: Some(1), externalize: true, ..Default::default() },
        );
        kinds.insert(
            "skill_content".to_string(),
            KindPolicy { ttl_turns: Some(8), refetchable: true, ..Default::default() },
        );
        kinds.insert(
            "mcp_resource".to_string(),
            KindPolicy { ttl_turns: Some(3), refetchable: true, ..Default::default() },
        );
        kinds.insert(
            "user_msg".to_string(),
            KindPolicy { ttl_turns: Some(40), ..Default::default() },
        );
        kinds.insert(
            "assistant_msg".to_string(),
            KindPolicy { ttl_turns: Some(40), ..Default::default() },
        );
        kinds.insert(
            "decision".to_string(),
            KindPolicy { ttl_turns: None, promote: true, ..Default::default() }, // ∞
        );
        kinds.insert(
            "task".to_string(),
            KindPolicy { ttl_turns: None, ..Default::default() }, // ∞
        );

        PolicyConfig {
            budget_tokens: 180_000,
            watermarks: Watermarks { soft: 0.60, hard: 0.80, emergency: 0.95 },
            externalize_threshold_tokens: 2000,
            tokenizer: "claude".to_string(),
            kinds,
            compaction: CompactionConfig {
                model: "claude-haiku-4-5".to_string(),
                prompt_template_id: "default-v1".to_string(),
                max_share: 0.5,
            },
            promotion: PromotionConfig {
                sink: PromotionSinkConfig {
                    sink_type: "webhook".to_string(),
                    url: Some("https://…/memory/ingest".to_string()),
                },
            },
            retention: RetentionConfig {
                blob_grace_hours: 72,
                evicted_blob_retention_days: 0,
                archived_session_blobs: "delete".to_string(),
                sweep_interval: "24h".to_string(),
            },
            // Spec §4.2: keep | externalize (Default) | evict — angewandt auf Units entfernter Tools.
            on_tool_removed: OnToolRemoved::Externalize,
        }
    }
}
