use serde::{Deserialize, Serialize};

/// Region eines Segments. Spec §2.2: `static | working`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    Static,
    Working,
}

/// Rolle eines Segments. Spec §2.2: `system | user | assistant | tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Lebenszyklus-Zustand eines Segments. Spec §2.2: `live | externalized | compacted | evicted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Live,
    Externalized,
    Compacted,
    Evicted,
}

/// Status einer Session. Spec §2.1: `active | archived`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Archived,
}

/// Status eines Frames. Spec §2.5: `open | popped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    Open,
    Popped,
}

/// Render-Scope-Filter (Spec §2.5). Steuert, welche Working-Segmente gerendert werden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderScope {
    /// Default: Static + Working-Segmente des aktuellen Frame-Pfads (Root + alle offenen Frames).
    #[default]
    Path,
    /// Isolierter Subagent-View: Static + gepinnte Root-Segmente + Segmente des Tip-Frames.
    Frame,
}

/// Umgang mit den Units eines entfernten Tools beim Epoch-Bump (Spec §4.2):
/// `keep | externalize (Default) | evict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnToolRemoved {
    Keep,
    #[default]
    Externalize,
    Evict,
}

/// snake_case-Wire-Namen der Enums (Port von `EnumWire.cs`) — für Event-Payloads und
/// Fehlermeldungen; die JSON-Abbildung läuft über serde (`rename_all = "snake_case"`).
macro_rules! wire_impl {
    ($ty:ty { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $ty {
            /// snake_case-Wire-Repräsentation (Spec §2, Wire-Format).
            pub fn wire_name(&self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }

        impl std::str::FromStr for $ty {
            type Err = String;

            fn from_str(wire: &str) -> Result<Self, Self::Err> {
                match wire {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(format!(
                        "Unbekannter Wire-Wert '{wire}' für {}.",
                        stringify!($ty)
                    )),
                }
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.wire_name())
            }
        }
    };
}

wire_impl!(Region { Static => "static", Working => "working" });
wire_impl!(Role { System => "system", User => "user", Assistant => "assistant", Tool => "tool" });
wire_impl!(SegmentState {
    Live => "live",
    Externalized => "externalized",
    Compacted => "compacted",
    Evicted => "evicted",
});
wire_impl!(SessionStatus { Active => "active", Archived => "archived" });
wire_impl!(FrameStatus { Open => "open", Popped => "popped" });
wire_impl!(RenderScope { Path => "path", Frame => "frame" });
wire_impl!(OnToolRemoved { Keep => "keep", Externalize => "externalize", Evict => "evict" });
