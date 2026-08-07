//! Metering data returned by Kiro's generateAssistantResponse stream.

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Legacy metering events contain the credit usage fields. Newer Kiro
/// versions may also include exact request token counts in the same event.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub unit_plural: String,
    #[serde(default)]
    pub usage: f64,
    /// Optional aggregate input count supplied by some runtimes.
    ///
    /// This field has no cache-bucket semantics; exact cache accounting, when
    /// present, belongs to `metadataEvent.tokenUsage`.
    #[serde(default)]
    pub input_tokens: i32,
    #[serde(default)]
    pub output_tokens: i32,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unit = if self.usage == 1.0 {
            &self.unit
        } else {
            &self.unit_plural
        };
        write!(f, "{:.3} {}", self.usage, unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_legacy_and_token_aware_payloads() {
        let legacy: MeteringEvent = serde_json::from_str(
            r#"{
                "unit":"credit",
                "unitPlural":"credits",
                "usage":1.25,
                "futureMeteringField":"ignored"
            }"#,
        )
        .unwrap();
        assert_eq!(legacy.unit, "credit");
        assert_eq!(legacy.unit_plural, "credits");
        assert_eq!(legacy.usage, 1.25);
        assert_eq!(legacy.input_tokens, 0);
        assert_eq!(legacy.output_tokens, 0);

        let token_aware: MeteringEvent = serde_json::from_str(
            r#"{
                "usage":2,
                "inputTokens":1234,
                "outputTokens":56,
                "futureTokenBreakdown":{"cached":1000}
            }"#,
        )
        .unwrap();
        assert_eq!(token_aware.input_tokens, 1234);
        assert_eq!(token_aware.output_tokens, 56);
    }

    #[test]
    fn missing_fields_use_safe_defaults() {
        let event: MeteringEvent = serde_json::from_str(r#"{"unknown":true}"#).unwrap();

        assert!(event.unit.is_empty());
        assert!(event.unit_plural.is_empty());
        assert_eq!(event.usage, 0.0);
        assert_eq!(event.input_tokens, 0);
        assert_eq!(event.output_tokens, 0);
    }
}
