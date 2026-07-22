//! Domänenmodell (Spec §2): Session, Segment, Frame, BlobRef, Policy, Watermarks.

pub mod blob_ref;
pub mod enums;
pub mod frame;
pub mod policy;
pub mod segment;
pub mod session;
pub mod watermark;

pub use blob_ref::BlobRef;
pub use enums::{FrameStatus, OnToolRemoved, Region, RenderScope, Role, SegmentState, SessionStatus};
pub use frame::Frame;
pub use policy::{
    parse_duration, CompactionConfig, KindPolicy, PolicyConfig, PromotionConfig,
    PromotionSinkConfig, RetentionConfig, Watermarks,
};
pub use segment::{Segment, SegmentDraft};
pub use session::Session;
pub use watermark::WatermarkLevel;
