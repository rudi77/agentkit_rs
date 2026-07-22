use std::collections::HashSet;

use ulid::Ulid;

use super::units::{group_into_units, Unit};
use crate::domain::{PolicyConfig, Region, Segment, SegmentState};

/// Ein Externalisierungs-Kandidat aus Phase 2 der Minor Collection (Spec §3.2 Schritt 2).
/// Der Collector führt KEIN I/O aus und mutiert nichts: er liefert Ziel-Segment-ID plus die zu
/// schreibende `content` und die berechnete `summary`. Der Aufrufer (`run_minor_gc`) führt den
/// Blob-Write aus und ruft anschließend `Segment::externalize`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalizationCandidate {
    pub segment_id: Ulid,
    pub content: String,
    pub summary: Option<String>,
}

/// Eine durch TTL-Eviction (Phase 3, Spec §3.2.3) zu entfernende Unit (Spec §2.4). Eine
/// ungekoppelte Unit trägt genau ein Segment; eine gekoppelte tool_call+tool_result-Unit beide.
/// `unit_id` ist die Unit-Identität (tool_call_id bei Kopplung, sonst die ID des einzelnen
/// Segments) — der Aufrufer emittiert genau EIN `unit_evicted` je Unit (Spec §6).
#[derive(Debug, Clone, PartialEq)]
pub struct EvictedUnit {
    pub unit_id: String,
    pub segment_ids: Vec<Ulid>,
}

/// Strukturierter Plan einer vollständigen Minor Collection (Spec §3.2). Anders als das
/// C#-Original mutiert der Rust-Collector NICHT in-place: alle drei Phasen sind reine
/// Planungs-Ausgaben (Segment-IDs), die der Aufrufer anwendet. Damit bleibt der
/// Emergency-Pfad atomar — bei `BudgetExceeded` wird schlicht nichts angewendet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MinorCollectionPlan {
    /// Phase 1 (Spec §3.2.1): refetchable Segmente jenseits ihrer TTL → evict, OHNE Blob-Write.
    pub clean_page_evicted: Vec<Ulid>,
    /// Phase 3 (Spec §3.2.3): TTL-fällige Units → evict beider gekoppelter Segmente.
    pub unit_evicted: Vec<EvictedUnit>,
    /// Phase 2 (Spec §3.2.2): auszuführende Externalisierungen (Blob-Write beim Aufrufer).
    pub externalization_candidates: Vec<ExternalizationCandidate>,
}

impl MinorCollectionPlan {
    pub fn is_empty(&self) -> bool {
        self.clean_page_evicted.is_empty()
            && self.unit_evicted.is_empty()
            && self.externalization_candidates.is_empty()
    }
}

/// Notfall-Minor-Collection ohne I/O-Seiteneffekte (Spec §3.1): nur die I/O-freien Phasen 1
/// (Clean-Page) und 3 (TTL-Eviction). Phase 2 (Externalisierung) wird übersprungen, weil sie
/// einen Blob-Write erfordert und im Hot Path verboten ist.
pub fn collect_emergency_no_io(
    segments: &[Segment],
    policy: &PolicyConfig,
    current_turn: u32,
) -> MinorCollectionPlan {
    plan(segments, policy, current_turn, false)
}

/// Vollständige Minor Collection als Plan (Spec §3.2, alle drei Phasen in normativer
/// Reihenfolge): (1) Clean-Page-Eviction → (2) Externalisierung → (3) TTL-Eviction;
/// billig vor teuer, lossless vor lossy. Gepinnte und Static-Segmente werden in JEDER Phase
/// übersprungen (Spec §2.2 I1 / §3.2).
pub fn plan_full(
    segments: &[Segment],
    policy: &PolicyConfig,
    current_turn: u32,
) -> MinorCollectionPlan {
    plan(segments, policy, current_turn, true)
}

fn plan(
    segments: &[Segment],
    policy: &PolicyConfig,
    current_turn: u32,
    with_externalization: bool,
) -> MinorCollectionPlan {
    let refs: Vec<&Segment> = segments.iter().collect();
    let units = group_into_units(&refs);

    let mut plan = MinorCollectionPlan::default();
    // Bereits in einer früheren Phase evictete Segmente gelten für spätere Phasen als tot —
    // spiegelt die In-Place-Mutation des C#-Originals, ohne hier zu mutieren.
    let mut evicted: HashSet<Ulid> = HashSet::new();

    // Spec §3.2: billig vor teuer, lossless vor lossy.
    collect_clean_page(&units, policy, current_turn, &mut plan, &mut evicted); // 1
    if with_externalization {
        collect_externalization(&units, policy, &evicted, &mut plan); // 2
    }
    collect_ttl(&units, policy, current_turn, &mut plan, &mut evicted); // 3

    plan
}

/// Effektiver Zustand eines Segments unter Berücksichtigung der bereits geplanten Evictions.
fn effective_state(segment: &Segment, evicted: &HashSet<Ulid>) -> SegmentState {
    if evicted.contains(&segment.id()) {
        SegmentState::Evicted
    } else {
        segment.state()
    }
}

/// Spec §2.2 I1 / §3.2: gepinnte und Static-Segmente sind für den GC unantastbar.
/// Bereits nicht-live/externalized Segmente werden nicht erneut angefasst.
fn is_gc_eligible(segment: &Segment, evicted: &HashSet<Ulid>) -> bool {
    segment.region() != Region::Static
        && !segment.pinned()
        && matches!(
            effective_state(segment, evicted),
            SegmentState::Live | SegmentState::Externalized
        )
}

/// Spec §3.2: TTL überschritten ⇔ `current_turn − last_referenced_turn > ttl_turns`.
/// Kind ohne Policy oder mit ttl_turns == None (∞) ist nie TTL-fällig.
fn is_ttl_expired(segment: &Segment, policy: &PolicyConfig, current_turn: u32) -> bool {
    let Some(kind_policy) = policy.kinds.get(segment.kind()) else {
        return false;
    };
    let Some(ttl) = kind_policy.ttl_turns else {
        return false; // ∞ — nie TTL-evicten.
    };
    current_turn.saturating_sub(segment.last_referenced_turn()) > ttl
}

/// true ⇔ Kind hat eine endliche TTL (Policy vorhanden, ttl_turns != None/∞).
fn has_defined_ttl(segment: &Segment, policy: &PolicyConfig) -> bool {
    policy
        .kinds
        .get(segment.kind())
        .is_some_and(|k| k.ttl_turns.is_some())
}

/// Spec §3.2.2: Kind-Eignung für Externalisierung (KindPolicy.externalize).
fn is_externalize_eligible(segment: &Segment, policy: &PolicyConfig) -> bool {
    policy
        .kinds
        .get(segment.kind())
        .is_some_and(|k| k.externalize)
}

/// Phase 1 — Clean-Page-Eviction (Spec §3.2 Schritt 1): refetchable Segmente jenseits ihrer
/// Kind-TTL → evicted, OHNE Blob-Write (Source of Truth liegt außerhalb). origin bleibt.
fn collect_clean_page(
    units: &[Unit<'_>],
    policy: &PolicyConfig,
    current_turn: u32,
    plan: &mut MinorCollectionPlan,
    evicted: &mut HashSet<Ulid>,
) {
    for unit in units {
        for segment in &unit.segments {
            if !is_gc_eligible(segment, evicted) {
                continue;
            }
            if !segment.refetchable() {
                continue;
            }
            if !is_ttl_expired(segment, policy, current_turn) {
                continue;
            }

            // Spec §3.2.1: ohne Blob-Write; origin/audit bleibt erhalten.
            evicted.insert(segment.id());
            plan.clean_page_evicted.push(segment.id());
        }
    }
}

/// Phase 2 — Externalisierung (Spec §3.2 Schritt 2): nicht-refetchable Segmente mit
/// `tokens > externalize_threshold_tokens` und Kind-Eignung als Kandidaten. Bei gekoppelten
/// Units (Spec §2.4) wird nur das tool_result externalisiert — der tool_call bleibt live.
/// KEIN I/O: nur Plan, der Aufrufer führt aus.
fn collect_externalization(
    units: &[Unit<'_>],
    policy: &PolicyConfig,
    evicted: &HashSet<Ulid>,
    plan: &mut MinorCollectionPlan,
) {
    for unit in units {
        for segment in &unit.segments {
            // Spec §2.4: in einer gekoppelten Unit nur das tool_result externalisieren;
            // der tool_call bleibt live, damit keine verwaisten tool_use-Blöcke entstehen.
            if unit.is_coupled() && segment.kind() == "tool_call" {
                continue;
            }
            if !is_gc_eligible(segment, evicted) {
                continue;
            }
            let Some(content) = segment.content() else {
                continue;
            };
            if effective_state(segment, evicted) != SegmentState::Live {
                continue;
            }
            if segment.refetchable() {
                continue;
            }
            if segment.tokens() <= policy.externalize_threshold_tokens {
                continue;
            }
            if !is_externalize_eligible(segment, policy) {
                continue;
            }

            // Spec §3.2.2: summary := first_n_chars + Strukturhinweis. char-basiert statt
            // UTF-16-Units wie in C# — kein Byte-Slicing (UTF-8-Boundary-Panik).
            let summary = if content.chars().count() <= 200 {
                content.to_string()
            } else {
                let mut s: String = content.chars().take(200).collect();
                s.push('…');
                s
            };

            plan.externalization_candidates.push(ExternalizationCandidate {
                segment_id: segment.id(),
                content: content.to_string(),
                summary: Some(summary),
            });
        }
    }
}

/// Phase 3 — TTL-Eviction (Spec §3.2 Schritt 3): Units, deren Kind-TTL überschritten ist und
/// die weder pinned noch Static sind → evicted. Operiert auf Unit-Ebene: eine gekoppelte Unit
/// verschwindet ganz.
fn collect_ttl(
    units: &[Unit<'_>],
    policy: &PolicyConfig,
    current_turn: u32,
    plan: &mut MinorCollectionPlan,
    evicted: &mut HashSet<Ulid>,
) {
    for unit in units {
        // Spec §2.4: GC auf Unit-Ebene. Ist ein gekoppeltes Segment unantastbar (pinned/Static)
        // oder noch nicht render-tot, bleibt die ganze Unit erhalten.
        let live: Vec<&Segment> = unit
            .segments
            .iter()
            .filter(|s| {
                matches!(
                    effective_state(s, evicted),
                    SegmentState::Live | SegmentState::Externalized
                )
            })
            .copied()
            .collect();

        if live.is_empty() {
            continue;
        }
        if live.iter().any(|s| !is_gc_eligible(s, evicted)) {
            continue;
        }

        // Spec §2.4 / §3.2.3: Die TTL einer Unit wird von ihren Segmenten mit definierter
        // Kind-TTL getragen (z. B. tool_result; ein tool_call hat keine eigene TTL). Die Unit
        // ist fällig ⇔ mindestens ein Segment hat eine definierte TTL UND alle definierten
        // TTLs sind überschritten.
        let ttl_bearing: Vec<&&Segment> =
            live.iter().filter(|s| has_defined_ttl(s, policy)).collect();
        if ttl_bearing.is_empty() {
            continue;
        }
        if !ttl_bearing
            .iter()
            .all(|s| is_ttl_expired(s, policy, current_turn))
        {
            continue;
        }

        for segment in &live {
            evicted.insert(segment.id());
        }

        // Spec §6: eine Unit-Eviction ist EIN unit_evicted-Event — bei gekoppelten Units
        // deckt es beide entfernten Segmente ab (Spec §2.4).
        plan.unit_evicted.push(EvictedUnit {
            unit_id: unit.unit_id(),
            segment_ids: live.iter().map(|s| s.id()).collect(),
        });
    }
}
