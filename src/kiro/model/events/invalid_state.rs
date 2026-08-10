//! Terminal invalid-state event returned by the Kiro runtime.

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvalidStateEvent {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: String,
}

impl EventPayload for InvalidStateEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_reason_and_message() {
        let event: InvalidStateEvent = serde_json::from_str(
            r#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD","message":"too large"}"#,
        )
        .unwrap();
        assert_eq!(event.reason, "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        assert_eq!(event.message, "too large");
    }
}
