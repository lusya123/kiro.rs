//! Completion metadata returned by Kiro event streams.
//!
//! The currently deployed IDE endpoint may emit only `stopReason`, while
//! newer runtimes may additionally include an exact `tokenUsage` breakdown.
//! Both shapes are valid and unknown future fields remain ignored by serde.

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Map Kiro's native completion reason onto the public Anthropic Messages API
/// vocabulary.
///
/// Kiro reasons are intentionally kept as strings on [`MetadataEvent`] so a
/// newer runtime cannot make event deserialization fail.  At the HTTP boundary
/// we only expose reasons whose semantics are known, however: forwarding an
/// unknown provider value as `stop_reason` would violate Anthropic's enum
/// contract.  Callers should retain their existing conservative fallback when
/// this function returns `None`.
pub(crate) fn anthropic_stop_reason(native: &str) -> Option<&'static str> {
    match native.trim().to_ascii_uppercase().as_str() {
        "END_TURN" => Some("end_turn"),
        "MAX_TOKENS" => Some("max_tokens"),
        "TOOL_USE" => Some("tool_use"),
        "STOP_SEQUENCE" => Some("stop_sequence"),
        "REFUSAL" => Some("refusal"),
        "MODEL_CONTEXT_WINDOW_EXCEEDED" => Some("model_context_window_exceeded"),
        _ => None,
    }
}

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

    #[test]
    fn maps_all_known_completion_reasons_to_anthropic_values() {
        for (native, anthropic) in [
            ("END_TURN", "end_turn"),
            ("MAX_TOKENS", "max_tokens"),
            ("TOOL_USE", "tool_use"),
            ("STOP_SEQUENCE", "stop_sequence"),
            ("REFUSAL", "refusal"),
            (
                "MODEL_CONTEXT_WINDOW_EXCEEDED",
                "model_context_window_exceeded",
            ),
        ] {
            assert_eq!(anthropic_stop_reason(native), Some(anthropic));
        }

        // Be tolerant of harmless wire casing/whitespace differences without
        // widening the public enum accepted from an upstream runtime.
        assert_eq!(anthropic_stop_reason("  refusal  "), Some("refusal"));
        assert_eq!(anthropic_stop_reason("max_tokens"), Some("max_tokens"));
    }

    #[test]
    fn unknown_completion_reason_is_not_exposed_as_public_stop_reason() {
        assert_eq!(anthropic_stop_reason("FUTURE_PROVIDER_REASON"), None);
        assert_eq!(anthropic_stop_reason(""), None);
        assert_eq!(anthropic_stop_reason("   "), None);
    }
}
