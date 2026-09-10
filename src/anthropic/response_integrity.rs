//! Completion evidence is independent of HTTP EOF and cache eligibility.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::kiro::model::events::{Event, anthropic_stop_reason};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

pub(super) const INCOMPLETE_MESSAGE: &str = "Upstream stream ended before a completion event";

/// Shared only within one response. Streaming bodies set this before their
/// outer, buffered format adapter decides whether the request can be retried.
#[derive(Clone, Default)]
pub(super) struct IncompleteResponse(Arc<AtomicBool>);

impl IncompleteResponse {
    pub fn mark(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_incomplete(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(super) fn incomplete_response() -> Response {
    let mut response = (
        StatusCode::BAD_GATEWAY,
        Json(super::types::ErrorResponse::new(
            "api_error",
            INCOMPLETE_MESSAGE,
        )),
    )
        .into_response();
    let marker = IncompleteResponse::default();
    marker.mark();
    response.extensions_mut().insert(marker);
    response
}

#[derive(Default)]
pub(super) struct CompletionEvidence {
    terminal: bool,
    completed_assistant: bool,
    visible_text: bool,
    unknown_stop: bool,
}

impl CompletionEvidence {
    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::AssistantResponse(response) => {
                self.completed_assistant |= response.is_completed();
                self.visible_text |= !response.content.trim().is_empty();
            }
            Event::ToolUse(tool) => self.terminal |= tool.stop,
            Event::Metering(_) => self.terminal = true,
            Event::Metadata(metadata) => {
                let reason = metadata.stop_reason.as_deref();
                let known = reason.and_then(anthropic_stop_reason).is_some();
                self.terminal |= known || metadata.token_usage.is_present();
                self.unknown_stop |= !known && reason.is_some_and(|s| !s.trim().is_empty());
            }
            Event::ContextUsage(usage) if usage.context_usage_percentage >= 100.0 => {
                self.terminal = true;
            }
            Event::Exception { exception_type, .. }
                if exception_type == "ContentLengthExceededException" =>
            {
                self.terminal = true;
            }
            _ => {}
        }
    }

    pub fn is_complete(&self) -> bool {
        self.terminal || (self.completed_assistant && self.visible_text && !self.unknown_stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_status_is_independent_of_billing_and_refusal_policy() {
        for (status, text, expected) in [
            ("COMPLETED", "done", true),
            ("IN_PROGRESS", "partial", false),
            ("COMPLETED", "", false),
        ] {
            let mut evidence = CompletionEvidence::default();
            evidence.observe(&Event::AssistantResponse(
                serde_json::from_value(serde_json::json!({"content":text,"messageStatus":status}))
                    .unwrap(),
            ));
            assert_eq!(evidence.is_complete(), expected);
        }
        for reason in [
            "REFUSAL",
            "MAX_TOKENS",
            "STOP_SEQUENCE",
            "TOOL_USE",
            "END_TURN",
        ] {
            let mut evidence = CompletionEvidence::default();
            evidence.observe(&Event::Metadata(
                serde_json::from_value(serde_json::json!({"stopReason":reason})).unwrap(),
            ));
            assert!(evidence.is_complete(), "{reason}");
        }
        let mut evidence = CompletionEvidence::default();
        evidence.observe(&Event::AssistantResponse(
            serde_json::from_value(
                serde_json::json!({"content":"partial","messageStatus":"COMPLETED"}),
            )
            .unwrap(),
        ));
        evidence.observe(&Event::Metadata(
            serde_json::from_value(serde_json::json!({"stopReason":"FUTURE_UNKNOWN_REASON"}))
                .unwrap(),
        ));
        assert!(!evidence.is_complete());
    }
}
