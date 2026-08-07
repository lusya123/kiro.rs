//! Completion metadata returned by Kiro event streams.
//!
//! The currently deployed IDE endpoint may emit only `stopReason`, while
//! newer runtimes may additionally include an exact `tokenUsage` breakdown.
//! Both shapes are valid and unknown future fields remain ignored by serde.

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default)]
    pub uncached_input_tokens: i32,
    #[serde(default)]
    pub cache_read_input_tokens: i32,
    #[serde(default)]
    pub cache_write_input_tokens: i32,
    #[serde(default)]
    pub output_tokens: i32,
    #[serde(default)]
    pub total_tokens: i32,
}

impl TokenUsage {
    /// Input that was neither read from nor written to a prompt cache.
    ///
    /// This is the value that maps to Anthropic's ordinary `input_tokens`
    /// bucket; it intentionally excludes both cache-read and cache-write
    /// tokens.
    pub fn ordinary_input_tokens(&self) -> i32 {
        self.uncached_input_tokens
    }

    /// Total logical input represented by all mutually exclusive buckets.
    pub fn total_input_tokens(&self) -> i32 {
        self.ordinary_input_tokens()
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_write_input_tokens)
    }

    /// Backward-compatible alias for [`Self::total_input_tokens`].
    ///
    /// New code should prefer the explicit method name so total logical input
    /// is not confused with ordinary uncached input.
    #[allow(dead_code)]
    pub fn input_tokens(&self) -> i32 {
        self.total_input_tokens()
    }

    pub fn is_present(&self) -> bool {
        self.uncached_input_tokens > 0
            || self.cache_read_input_tokens > 0
            || self.cache_write_input_tokens > 0
            || self.output_tokens > 0
            || self.total_tokens > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// Native Kiro completion reason, for example `END_TURN`.
    ///
    /// Kept as a string rather than an enum so new upstream reasons remain
    /// forward compatible.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Exact accounting is absent on older/current IDE responses.
    #[serde(default)]
    pub token_usage: TokenUsage,
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_exact_cache_breakdown() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{
                "stopReason": "END_TURN",
                "tokenUsage": {
                    "uncachedInputTokens": 7,
                    "cacheReadInputTokens": 1000,
                    "cacheWriteInputTokens": 25,
                    "outputTokens": 42,
                    "totalTokens": 1074,
                    "futureTokenField": 99
                },
                "futureMetadataField": {"enabled": true}
            }"#,
        )
        .unwrap();

        assert_eq!(event.stop_reason.as_deref(), Some("END_TURN"));
        assert_eq!(event.token_usage.ordinary_input_tokens(), 7);
        assert_eq!(event.token_usage.total_input_tokens(), 1032);
        // Preserve the historical aggregate accessor for downstream callers.
        assert_eq!(event.token_usage.input_tokens(), 1032);
        assert!(event.token_usage.is_present());
    }

    #[test]
    fn accepts_live_stop_reason_only_payload() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{
                "stopReason": "END_TURN",
                "futureField": "ignored"
            }"#,
        )
        .unwrap();

        assert_eq!(event.stop_reason.as_deref(), Some("END_TURN"));
        assert_eq!(event.token_usage, TokenUsage::default());
        assert_eq!(event.token_usage.ordinary_input_tokens(), 0);
        assert_eq!(event.token_usage.total_input_tokens(), 0);
        assert!(!event.token_usage.is_present());
    }

    #[test]
    fn missing_stop_reason_and_token_usage_use_safe_defaults() {
        let event: MetadataEvent = serde_json::from_str(r#"{"unknown":true}"#).unwrap();

        assert_eq!(event, MetadataEvent::default());
    }
}
