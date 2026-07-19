//! Deterministische Render-Pipeline (Spec §4.6): kanonisches JSON, neutraler Render-Plan,
//! Provider-Adapter (anthropic, openai).

pub mod adapter;
pub mod anthropic;
pub mod canonical_json;
pub mod model;
pub mod openai;
pub mod planner;
pub mod static_diff;
pub mod static_prefix;

pub use adapter::{adapter_for, registered_providers, ProviderAdapter};
pub use anthropic::AnthropicMessagesAdapter;
pub use model::{
    CacheBreakpoint, CacheBreakpointKind, RenderBlockKind, RenderContentBlock, RenderMessage,
    RenderModel, RenderPlanResult, RenderResult, RenderStaticItem,
};
pub use openai::OpenAiChatAdapter;
pub use static_diff::{StaticRegionDiffResult, StaticSegmentSpec};
