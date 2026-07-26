//! Native reasoning event returned by newer Kiro models.

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// A reasoning update. Kiro sends reasoning text separately from visible
/// assistant content and sends the opaque signature in a trailing event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// Reasoning text. Depending on the upstream version this may be cumulative.
    #[serde(default)]
    pub text: String,
    /// Opaque upstream reasoning signature.
    #[serde(default)]
    pub signature: String,
    /// Base64-encoded encrypted reasoning payload.
    #[serde(default)]
    pub redacted_content: String,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_text_signature_and_redacted_content() {
        let event: ReasoningContentEvent = serde_json::from_str(
            r#"{
                "text":"Inspect the request.",
                "signature":"opaque-signature",
                "redactedContent":"cmVkYWN0ZWQ="
            }"#,
        )
        .unwrap();

        assert_eq!(event.text, "Inspect the request.");
        assert_eq!(event.signature, "opaque-signature");
        assert_eq!(event.redacted_content, "cmVkYWN0ZWQ=");
    }
}
