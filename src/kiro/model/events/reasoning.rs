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

/// Trailing native-thinking metadata used by several Kiro model families.
///
/// Some Kiro runtimes send readable reasoning through `reasoningContentEvent`,
/// then deliver the opaque provider signature separately in a trailing
/// `thinkingMetadataEvent`. Treating this event as unknown drops the only
/// signature and consequently forces the Anthropic compatibility layer to hide
/// the whole thinking block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingMetadataEvent {
    /// Opaque upstream reasoning signature.
    #[serde(default)]
    pub signature: String,
    /// Native reasoning-token count. The public response still uses the shared
    /// output-usage reconciliation, but retaining this field keeps the wire
    /// model forward compatible and available for diagnostics.
    #[serde(default)]
    pub token_count: i32,
}

impl EventPayload for ThinkingMetadataEvent {
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

    #[test]
    fn deserializes_trailing_thinking_metadata() {
        let event: ThinkingMetadataEvent =
            serde_json::from_str(r#"{"signature":"opaque-native-signature","tokenCount":42}"#)
                .unwrap();

        assert_eq!(event.signature, "opaque-native-signature");
        assert_eq!(event.token_count, 42);
    }
}
