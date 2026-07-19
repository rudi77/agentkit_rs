use std::collections::HashMap;

use crate::domain::Segment;

/// Eine logische Einheit im Sinne von Spec §2.4: ein `tool_call`-Segment plus sein
/// korrespondierendes `tool_result` (gekoppelt über `tool_call_id`). Ungekoppelte Segmente
/// bilden je eine Single-Segment-Unit. GC operiert ausschließlich auf Units, nie auf
/// gekoppelten Einzel-Segmenten (Spec §2.4 / §3.2).
#[derive(Debug)]
pub struct Unit<'a> {
    pub segments: Vec<&'a Segment>,
}

impl<'a> Unit<'a> {
    /// true ⇔ gekoppelte Unit (tool_call + tool_result via tool_call_id, Spec §2.4).
    pub fn is_coupled(&self) -> bool {
        self.segments.len() > 1
    }

    /// Unit-Identität (Spec §2.4): die gemeinsame `tool_call_id` einer gekoppelten Unit,
    /// sonst die ID des einzelnen Segments. Basis für das `unit_evicted`-Event (Spec §6).
    pub fn unit_id(&self) -> String {
        self.segments
            .iter()
            .find_map(|s| s.tool_call_id())
            .map(str::to_string)
            .unwrap_or_else(|| self.segments[0].id().to_string())
    }
}

/// Gruppiert eine Segment-Liste in Units (Spec §2.4). Segmente mit gleichem, nicht-leerem
/// `tool_call_id` der Kinds `tool_call`/`tool_result` werden zu einer Unit gekoppelt; alle
/// übrigen Segmente bilden je eine eigene Single-Segment-Unit. Die Reihenfolge folgt dem
/// ersten Auftreten. I/O-frei und deterministisch. (Port von `UnitGrouping.cs`.)
pub fn group_into_units<'a>(segments: &[&'a Segment]) -> Vec<Unit<'a>> {
    let mut units: Vec<Unit<'a>> = Vec::new();
    // tool_call_id → Index der bereits angelegten Unit (wahrt die Erst-Auftreten-Reihenfolge).
    let mut coupled: HashMap<&str, usize> = HashMap::new();

    for segment in segments {
        // Spec §2.4: nur tool_call/tool_result werden über tool_call_id gekoppelt.
        if let (Some(id), "tool_call" | "tool_result") = (segment.tool_call_id(), segment.kind()) {
            match coupled.get(id) {
                Some(&index) => units[index].segments.push(segment),
                None => {
                    coupled.insert(id, units.len());
                    units.push(Unit { segments: vec![segment] });
                }
            }
        } else {
            units.push(Unit { segments: vec![segment] });
        }
    }

    units
}
