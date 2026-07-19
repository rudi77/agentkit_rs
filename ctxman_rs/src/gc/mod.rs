//! Garbage-Collection-Lifecycle (Spec §3): Units, Minor Collection (deterministisch, I/O-frei
//! geplant), Major Collection (Compaction-Fenster).

pub mod major;
pub mod minor;
pub mod units;

pub use major::{plan_compaction, CompactionPlan};
pub use minor::{
    collect_emergency_no_io, plan_full, EvictedUnit, ExternalizationCandidate,
    MinorCollectionPlan,
};
pub use units::{group_into_units, Unit};

use serde::{Deserialize, Serialize};

/// GC-Stufe (Spec §3.1): Minor (deterministisch, billig) oder Major (LLM-gestützt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcLevel {
    Minor,
    Major,
}

impl GcLevel {
    pub fn wire_name(&self) -> &'static str {
        match self {
            GcLevel::Minor => "minor",
            GcLevel::Major => "major",
        }
    }
}
