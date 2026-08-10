//! 事件模型
//!
//! 定义 generateAssistantResponse 流式响应的事件类型

mod assistant;
mod base;
mod context_usage;
mod invalid_state;
mod metadata;
mod metering;
mod reasoning;
mod tool_use;

pub use assistant::AssistantResponseEvent;
pub use base::Event;
pub use context_usage::ContextUsageEvent;
pub use invalid_state::InvalidStateEvent;
// Keep raw payload models public so downstream code can distinguish ordinary
// uncached input from total logical input without re-parsing EventStream JSON.
pub(crate) use metadata::anthropic_stop_reason;
#[allow(unused_imports)]
pub use metadata::{MetadataEvent, TokenUsage};
pub use metering::MeteringEvent;
pub use reasoning::ReasoningContentEvent;
pub use tool_use::ToolUseEvent;
