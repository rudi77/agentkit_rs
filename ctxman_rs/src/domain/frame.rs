use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::enums::FrameStatus;

/// Bildet einen Subagent-Aufruf als Stack-Frame ab (Spec §2.5). Neue Segmente erhalten die
/// `id` des obersten offenen Frames; ein `pop` evicted alle Segmente des Frames und legt ein
/// `subagent_return`-Segment im Parent-Frame an. LIFO-Disziplin (§2.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    id: Ulid,
    session_id: Ulid,
    parent_frame_id: Option<Ulid>,
    label: String,
    opened_turn: u32,
    status: FrameStatus,
}

impl Frame {
    pub fn new(
        id: Ulid,
        session_id: Ulid,
        parent_frame_id: Option<Ulid>,
        label: String,
        opened_turn: u32,
    ) -> Self {
        Frame {
            id,
            session_id,
            parent_frame_id,
            label,
            opened_turn,
            status: FrameStatus::Open,
        }
    }

    /// ULID des Frames (Spec §2.5).
    pub fn id(&self) -> Ulid {
        self.id
    }

    /// Session, zu der der Frame gehört (Spec §2.5).
    pub fn session_id(&self) -> Ulid {
        self.session_id
    }

    /// `None` = Root-Frame (Spec §2.5).
    pub fn parent_frame_id(&self) -> Option<Ulid> {
        self.parent_frame_id
    }

    /// Label, z. B. "research_subtask" (Spec §2.5).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Turn, in dem der Frame geöffnet wurde (Spec §2.5).
    pub fn opened_turn(&self) -> u32 {
        self.opened_turn
    }

    /// open | popped (Spec §2.5).
    pub fn status(&self) -> FrameStatus {
        self.status
    }

    /// Markiert den Frame als gepoppt (Spec §2.5 pop).
    pub fn pop(&mut self) {
        self.status = FrameStatus::Popped;
    }
}
