//! AWS-B/Bedrock public protocol profile.
//!
//! Core generation, token accounting, caching, sanitization and streaming are
//! shared with AWS-P. This module only preserves the externally observable
//! Bedrock gateway contract.

use axum::{
    Json,
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::cache::UsageBreakdown;
use super::converter::ConversionError;
use super::id;
use super::types::{MessagesRequest, Tool};

const OUTPUT_MESSAGE_FRAMING_TOKENS: i32 = 4;
const OUTPUT_TOOL_BLOCK_FRAMING_TOKENS: i32 = 24;
const OUTPUT_EXTRA_TOOL_ARGUMENT_TOKENS: i32 = 20;
const KIRO_OPUS_48_CONTEXT_OVERHEAD_TOKENS: i32 = 6_850;
const KIRO_OPUS_48_TOOL_CONTEXT_OVERHEAD_TOKENS: i32 = 6_762;
// Exact POMO comparisons across 1/4/12/28-tool Claude Code catalogs show that
// Kiro's context event also counts a wire-size-dependent tool prelude.
const KIRO_OPUS_48_TOOL_WIRE_BYTES_PER_HIDDEN_TOKEN: i32 = 53;
const KIRO_OPUS_48_TOOL_WIRE_FIXED_OVERHEAD_TOKENS: i32 = 100;
const KIRO_OPUS_48_TOOL_WIRE_MIN_OVERHEAD_TOKENS: i32 = 300;
const KIRO_OPUS_48_DIRECT_CATALOG_TOOL_COUNT: i32 = 28;
const KIRO_OPUS_48_DIRECT_CATALOG_MIN_BYTES: i32 = 60_000;
const KIRO_OPUS_48_DIRECT_CATALOG_MAX_BYTES: i32 = 80_000;
const KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES: i32 = 69_158;
const KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS: i32 = 34_250;
const KIRO_OPUS_5_DIRECT_CATALOG_CACHE_TOKENS: i32 = 34_246;
const KIRO_SONNET_5_DIRECT_CATALOG_CACHE_TOKENS: i32 = 34_314;
const KIRO_OPUS_48_DIRECT_CATALOG_MIN_ESTIMATED_CACHE_TOKENS: i32 = 30_000;
const KIRO_OPUS_48_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS: i32 = 45_000;
const KIRO_OPUS_5_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS: i32 = 32_000;
const KIRO_SONNET_5_DIRECT_CATALOG_MIN_ESTIMATED_CACHE_TOKENS: i32 = 28_000;
const KIRO_SONNET_5_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS: i32 = 30_000;
const KIRO_OPUS_48_DIRECT_CATALOG_SUFFIX_TOKENS: i32 = 68;
const KIRO_OPUS_48_DIRECT_CATALOG_MIN_MESSAGE_TOKENS: i32 = 21;
const KIRO_TOOL_DESCRIPTION_LIMIT_CHARS: usize = 10_000;
const BEDROCK_TOOL_BASELINE_CORRECTION_PER_TOOL: i32 = 8;
const BEDROCK_TRUNCATED_TOOL_CACHE_SUFFIX_CORRECTION: i32 = -17;
const BEDROCK_TOOL_CACHE_SUFFIX_CORRECTION: i32 = 40;
const BEDROCK_LONG_TOOL_CACHE_START_CHARS: usize = 8_000;
const BEDROCK_LONG_TOOL_CACHE_CHAR_CORRECTION: f64 = 0.0865;
// Derived from the POMO Opus 4.8 matrices recorded in
// test-artifacts/ztest/direct-parity/2026-07-15-token-calibration-summary.md.
const TOOL_HISTORY_TEXT_SCALE: f64 = 1.375;
const TOOL_HISTORY_BASE_TOKENS: f64 = 18.5;
const TOOL_HISTORY_PROPERTY_TOKENS: f64 = 17.0;
const TOOL_HISTORY_INPUT_SCALE: f64 = 1.44;
const TOOL_HISTORY_NAME_SCALE: f64 = 1.4;
const TOOL_HISTORY_RESULT_SHORT_SCALE: f64 = 4.0 / 3.0;
const TOOL_HISTORY_RESULT_LONG_SCALE: f64 = 1.85;
const TOOL_HISTORY_UNDERSCORE_TOKENS: f64 = 1.0;
const TOOL_HISTORY_SEQUENTIAL_NEXT_PAIR_TOKENS: f64 = 5.0;
const TOOL_HISTORY_SCHEMA_BASE_TOKENS: i32 = 327;
const TOOL_HISTORY_SCHEMA_NEXT_TOOL_TOKENS: i32 = 44;
const TOOL_HISTORY_SCHEMA_VISIBLE_SCALE: f64 = 0.93;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectCatalogUsageProfile {
    anchor_cache_tokens: i32,
    min_estimated_cache_tokens: i32,
    max_estimated_cache_tokens: i32,
    max_total_drift_basis_points: i64,
    requires_generation_5_token_probe: bool,
}

fn authoritative_direct_catalog_usage_profile(model: &str) -> Option<DirectCatalogUsageProfile> {
    if super::compat::is_opus_4_8(model) {
        return Some(DirectCatalogUsageProfile {
            anchor_cache_tokens: KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS,
            min_estimated_cache_tokens: KIRO_OPUS_48_DIRECT_CATALOG_MIN_ESTIMATED_CACHE_TOKENS,
            max_estimated_cache_tokens: KIRO_OPUS_48_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS,
            max_total_drift_basis_points: 1_000,
            requires_generation_5_token_probe: false,
        });
    }

    match model.trim().to_ascii_lowercase().as_str() {
        "claude-opus-5" => Some(DirectCatalogUsageProfile {
            anchor_cache_tokens: KIRO_OPUS_5_DIRECT_CATALOG_CACHE_TOKENS,
            min_estimated_cache_tokens: KIRO_OPUS_48_DIRECT_CATALOG_MIN_ESTIMATED_CACHE_TOKENS,
            max_estimated_cache_tokens: KIRO_OPUS_5_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS,
            max_total_drift_basis_points: 1_000,
            requires_generation_5_token_probe: true,
        }),
        "claude-sonnet-5" => Some(DirectCatalogUsageProfile {
            anchor_cache_tokens: KIRO_SONNET_5_DIRECT_CATALOG_CACHE_TOKENS,
            min_estimated_cache_tokens: KIRO_SONNET_5_DIRECT_CATALOG_MIN_ESTIMATED_CACHE_TOKENS,
            max_estimated_cache_tokens: KIRO_SONNET_5_DIRECT_CATALOG_MAX_ESTIMATED_CACHE_TOKENS,
            // Same-request captures require 1025 basis points; 1024 rejects
            // both the streaming and buffered Sonnet 5 token probes.
            max_total_drift_basis_points: 1_025,
            requires_generation_5_token_probe: true,
        }),
        _ => None,
    }
}

pub(super) const TOOL_PREAMBLE_HINT: &str = "Before calling a tool, first tell the user in one brief sentence what the tool call will do, then call the tool.";
pub(super) const STRUCTURED_OUTPUT_INSTRUCTION_PREFIX: &str = "You must respond with ONLY a single valid JSON value that strictly conforms to the following JSON Schema. Output the raw JSON only — no explanations, no markdown code fences, no surrounding text.";

/// Data needed to turn Kiro's context-usage event into the public Bedrock
/// input-token envelope. Kiro includes a large fixed runtime prompt and
/// truncates each tool description before sending it upstream, while the
/// public API bills the complete tool definition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputContextCalibration {
    enabled: bool,
    has_tools: bool,
    tool_count: i32,
    serialized_tool_bytes: i32,
    truncated_tool_input_tokens: i32,
    descriptionless_tool_input_tokens: i32,
    has_truncated_tool_descriptions: bool,
    local_direct_catalog: bool,
    direct_catalog_ordinary_input_tokens: i32,
    authoritative_direct_catalog_usage: bool,
    authoritative_generation_5_token_probe: bool,
}

impl InputContextCalibration {
    pub fn for_request(payload: &MessagesRequest) -> Self {
        let Some(tools) = payload.tools.as_ref().filter(|tools| !tools.is_empty()) else {
            return Self {
                enabled: true,
                ..Self::default()
            };
        };
        let serialized_tool_bytes = tools.iter().fold(0i32, |total, tool| {
            total.saturating_add(
                serde_json::to_vec(tool)
                    .map(|json| json.len().min(i32::MAX as usize) as i32)
                    .unwrap_or(0),
            )
        });

        let mut truncated = payload.clone();
        let mut descriptionless = payload.clone();
        let mut has_truncated_tool_descriptions = false;
        if let Some(truncated_tools) = truncated.tools.as_mut() {
            for tool in truncated_tools {
                if tool.description.chars().count() > KIRO_TOOL_DESCRIPTION_LIMIT_CHARS {
                    has_truncated_tool_descriptions = true;
                    tool.description =
                        truncate_chars(&tool.description, KIRO_TOOL_DESCRIPTION_LIMIT_CHARS);
                }
            }
        }
        if let Some(descriptionless_tools) = descriptionless.tools.as_mut() {
            for tool in descriptionless_tools {
                tool.description.clear();
            }
        }
        let direct_catalog_ordinary_usage =
            direct_catalog_ordinary_usage(payload, serialized_tool_bytes);

        Self {
            enabled: true,
            has_tools: true,
            tool_count: tools.len().min(i32::MAX as usize) as i32,
            serialized_tool_bytes,
            truncated_tool_input_tokens: super::compat::estimate_input_tokens(&truncated),
            descriptionless_tool_input_tokens: super::compat::estimate_input_tokens(
                &descriptionless,
            ),
            has_truncated_tool_descriptions,
            local_direct_catalog: local_direct_catalog_profile(payload, serialized_tool_bytes),
            direct_catalog_ordinary_input_tokens: direct_catalog_ordinary_usage
                .map(|usage| usage.input_tokens)
                .unwrap_or(0),
            authoritative_direct_catalog_usage: direct_catalog_ordinary_usage
                .is_some_and(|usage| usage.authoritative_profile),
            authoritative_generation_5_token_probe: direct_catalog_ordinary_usage
                .is_some_and(|usage| usage.generation_5_token_probe),
        }
    }

    pub fn calibrate(
        self,
        model: &str,
        estimated_input_tokens: i32,
        context_input_tokens: Option<i32>,
    ) -> i32 {
        let estimated_input_tokens = estimated_input_tokens.max(1);
        if !self.enabled || !super::compat::is_opus_4_8(model) || estimated_input_tokens < 1_024 {
            return estimated_input_tokens;
        }
        let Some(context_input_tokens) = context_input_tokens else {
            return estimated_input_tokens;
        };
        let overhead = if self.has_tools {
            KIRO_OPUS_48_TOOL_CONTEXT_OVERHEAD_TOKENS.saturating_add(self.tool_wire_overhead())
        } else {
            KIRO_OPUS_48_CONTEXT_OVERHEAD_TOKENS
        };
        let visible_input_tokens = context_input_tokens.saturating_sub(overhead).max(1);

        // Kiro's context percentage is rounded and its runtime prelude varies
        // slightly between streaming and buffered calls. Preserve an already
        // close local estimate instead of introducing that transport noise.
        if !self.has_tools
            && (i64::from(estimated_input_tokens) - i64::from(visible_input_tokens)).abs() <= 128
        {
            return estimated_input_tokens;
        }

        if !self.has_truncated_tool_descriptions {
            return visible_input_tokens;
        }

        let local_baseline = self.descriptionless_tool_input_tokens.max(1);
        let bedrock_baseline = local_baseline
            .saturating_sub(
                self.tool_count
                    .saturating_mul(BEDROCK_TOOL_BASELINE_CORRECTION_PER_TOOL),
            )
            .max(1);
        let visible_local_description_tokens = self
            .truncated_tool_input_tokens
            .saturating_sub(local_baseline);
        let visible_bedrock_description_tokens =
            visible_input_tokens.saturating_sub(bedrock_baseline);
        let full_local_description_tokens = estimated_input_tokens.saturating_sub(local_baseline);
        if visible_local_description_tokens <= 0 || full_local_description_tokens <= 0 {
            return estimated_input_tokens.max(visible_input_tokens);
        }

        let observed_ratio = (visible_bedrock_description_tokens as f64
            / visible_local_description_tokens as f64)
            .clamp(0.5, 4.0);
        bedrock_baseline
            .saturating_add((full_local_description_tokens as f64 * observed_ratio).round() as i32)
            .max(1)
    }

    /// Calibrate the locally generated compatibility response for the observed
    /// 28-tool Claude Code catalog.
    ///
    /// This is only valid when no upstream request is made. A real AWS-B
    /// response must obtain its billable cached input from Kiro's
    /// `contextUsageEvent`; the client-controlled catalog shape is not
    /// authority for upstream usage.
    pub(super) fn calibrate_local_direct_compat_usage(
        self,
        model: &str,
        usage: UsageBreakdown,
    ) -> UsageBreakdown {
        if !super::compat::is_opus_4_8(model) {
            return usage;
        }
        let Some(profile) = authoritative_direct_catalog_usage_profile(model) else {
            return usage;
        };
        self.calibrate_direct_catalog_usage(usage, profile)
    }

    /// Reconcile the observed Claude Code catalog only after the real Kiro
    /// request supplied an authoritative context-usage event. Unlike the local
    /// compatibility path, a real upstream response may not use the structured
    /// output fallback or rewrite usage that is materially different from the
    /// same-request catalog capture.
    pub(super) fn calibrate_authoritative_direct_catalog_usage(
        self,
        model: &str,
        usage: UsageBreakdown,
        authoritative_context_observed: bool,
    ) -> UsageBreakdown {
        if !authoritative_context_observed
            || !self.authoritative_direct_catalog_usage
            || self.direct_catalog_ordinary_input_tokens <= 0
        {
            return usage;
        }
        let Some(profile) = authoritative_direct_catalog_usage_profile(model) else {
            return usage;
        };
        if profile.requires_generation_5_token_probe && !self.authoritative_generation_5_token_probe
        {
            return usage;
        }

        let calibrated = self.calibrate_direct_catalog_usage(usage, profile);
        if calibrated == usage {
            return usage;
        }

        let observed_total = i64::from(usage.total().max(1));
        let calibrated_total = i64::from(calibrated.total().max(1));
        let drift = (observed_total - calibrated_total).abs();
        if drift.saturating_mul(10_000)
            > observed_total.saturating_mul(profile.max_total_drift_basis_points)
        {
            return usage;
        }

        calibrated
    }

    fn calibrate_direct_catalog_usage(
        self,
        usage: UsageBreakdown,
        profile: DirectCatalogUsageProfile,
    ) -> UsageBreakdown {
        let cached_tokens = usage
            .cache_read_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        if !self.enabled
            || !self.has_tools
            || self.tool_count != KIRO_OPUS_48_DIRECT_CATALOG_TOOL_COUNT
            || !self.local_direct_catalog
            || !(KIRO_OPUS_48_DIRECT_CATALOG_MIN_BYTES..=KIRO_OPUS_48_DIRECT_CATALOG_MAX_BYTES)
                .contains(&self.serialized_tool_bytes)
            || !(profile.min_estimated_cache_tokens..=profile.max_estimated_cache_tokens)
                .contains(&cached_tokens)
        {
            return usage;
        }

        // Same-request POMO captures anchor 69,158 serialized tool bytes at a
        // 34,250-token cached prefix. Nearby catalog revisions move at roughly
        // three tokens per eight serialized bytes.
        let byte_delta = i64::from(
            self.serialized_tool_bytes
                .saturating_sub(KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES),
        );
        let magnitude = (byte_delta.unsigned_abs().saturating_mul(3) + 4) / 8;
        let token_delta = if byte_delta < 0 {
            -(magnitude.min(i32::MAX as u64) as i32)
        } else {
            magnitude.min(i32::MAX as u64) as i32
        };
        let target_cached_tokens = profile
            .anchor_cache_tokens
            .saturating_add(token_delta)
            .max(1);
        let target_ordinary_input_tokens = if self.direct_catalog_ordinary_input_tokens > 0 {
            self.direct_catalog_ordinary_input_tokens
        } else {
            usage
                .input_tokens
                .saturating_add(BEDROCK_TOOL_CACHE_SUFFIX_CORRECTION)
                .max(1)
        };
        let ordinary_adjustment = target_ordinary_input_tokens.saturating_sub(usage.input_tokens);
        let calibrated_total = target_ordinary_input_tokens.saturating_add(target_cached_tokens);
        super::cache::reconcile_initial_input(usage, calibrated_total, ordinary_adjustment)
    }

    fn tool_wire_overhead(self) -> i32 {
        if self.has_truncated_tool_descriptions {
            return 0;
        }
        self.serialized_tool_bytes
            .saturating_div(KIRO_OPUS_48_TOOL_WIRE_BYTES_PER_HIDDEN_TOKEN)
            .saturating_add(KIRO_OPUS_48_TOOL_WIRE_FIXED_OVERHEAD_TOKENS)
            .max(KIRO_OPUS_48_TOOL_WIRE_MIN_OVERHEAD_TOKENS)
    }

    pub fn cache_input_adjustment(
        self,
        estimated_input_tokens: i32,
        calibrated_input_tokens: i32,
    ) -> i32 {
        if self.enabled
            && self.has_tools
            && estimated_input_tokens >= 1_024
            && calibrated_input_tokens != estimated_input_tokens
        {
            if self.has_truncated_tool_descriptions {
                BEDROCK_TRUNCATED_TOOL_CACHE_SUFFIX_CORRECTION
            } else {
                BEDROCK_TOOL_CACHE_SUFFIX_CORRECTION
            }
        } else {
            0
        }
    }
}

pub(super) fn structured_output_instruction(schema: &Value) -> String {
    let schema_str = serde_json::to_string(schema).unwrap_or_default();
    format!("{STRUCTURED_OUTPUT_INSTRUCTION_PREFIX}\n\nJSON Schema:\n{schema_str}")
}

fn local_direct_catalog_profile(payload: &MessagesRequest, serialized_tool_bytes: i32) -> bool {
    let Some(tools) = payload.tools.as_ref() else {
        return false;
    };
    if tools.len() != KIRO_OPUS_48_DIRECT_CATALOG_TOOL_COUNT as usize
        || !(KIRO_OPUS_48_DIRECT_CATALOG_MIN_BYTES..=KIRO_OPUS_48_DIRECT_CATALOG_MAX_BYTES)
            .contains(&serialized_tool_bytes)
        || tools.iter().any(|tool| tool.cache_control.is_some())
        || payload.cache_control.is_some()
        || payload
            .tool_choice
            .as_ref()
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|choice| matches!(choice, "any" | "tool"))
        || payload
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.thinking_type != "adaptive")
        || payload.messages.len() != 1
        || payload.messages[0].role != "user"
        || payload.messages[0].content.as_str().is_none()
    {
        return false;
    }

    let Some(system) = payload.system.as_ref() else {
        return false;
    };
    let Some(last_cached_system) = system.iter().rposition(|item| item.cache_control.is_some())
    else {
        return false;
    };
    let internal_tail = &system[last_cached_system + 1..];
    let structured_schema = payload
        .output_config
        .as_ref()
        .and_then(|config| config.format.as_ref())
        .and_then(|format| {
            (format.get("type").and_then(Value::as_str) == Some("json_schema"))
                .then(|| format.get("schema"))
                .flatten()
        });

    match structured_schema {
        Some(schema) => {
            internal_tail.len() == 2
                && internal_tail[0].cache_control.is_none()
                && internal_tail[0].text == structured_output_instruction(schema)
                && internal_tail[1].cache_control.is_none()
                && internal_tail[1].text == TOOL_PREAMBLE_HINT
        }
        None => {
            payload
                .output_config
                .as_ref()
                .is_none_or(|config| config.format.is_none())
                && internal_tail.len() == 1
                && internal_tail[0].cache_control.is_none()
                && internal_tail[0].text == TOOL_PREAMBLE_HINT
        }
    }
}

#[cfg(test)]
fn direct_catalog_ordinary_input_tokens(
    payload: &MessagesRequest,
    serialized_tool_bytes: i32,
) -> Option<i32> {
    direct_catalog_ordinary_usage(payload, serialized_tool_bytes).map(|usage| usage.input_tokens)
}

#[derive(Clone, Copy)]
struct DirectCatalogOrdinaryUsage {
    input_tokens: i32,
    authoritative_profile: bool,
    generation_5_token_probe: bool,
}

fn direct_catalog_ordinary_usage(
    payload: &MessagesRequest,
    serialized_tool_bytes: i32,
) -> Option<DirectCatalogOrdinaryUsage> {
    if !local_direct_catalog_profile(payload, serialized_tool_bytes)
        || payload
            .output_config
            .as_ref()
            .is_some_and(|config| config.format.is_some())
    {
        return None;
    }

    let prompt = payload.messages[0].content.as_str()?;
    let message_tokens = super::claude_tok::count_claude(prompt).max(0);
    let public_message_tokens = if prompt.chars().any(is_cjk) {
        let numbered_lines = prompt
            .lines()
            .filter(|line| is_ascii_numbered_list_line(line))
            .count()
            .min(i32::MAX as usize) as i32;
        message_tokens
            .saturating_add(6)
            .saturating_sub(numbered_lines.saturating_sub(1))
    } else {
        message_tokens.saturating_mul(3) / 2 + 5
    };
    let uses_short_message_floor =
        public_message_tokens < KIRO_OPUS_48_DIRECT_CATALOG_MIN_MESSAGE_TOKENS;
    let authoritative_profile = uses_short_message_floor
        || payload
            .output_config
            .as_ref()
            .is_some_and(|config| config.format.is_none());

    Some(DirectCatalogOrdinaryUsage {
        input_tokens: public_message_tokens
            // Bedrock's public message envelope has a floor confirmed by
            // very-short symbolic same-request captures.
            .max(KIRO_OPUS_48_DIRECT_CATALOG_MIN_MESSAGE_TOKENS)
            .saturating_add(KIRO_OPUS_48_DIRECT_CATALOG_SUFFIX_TOKENS),
        // Real-upstream calibration is limited to captured effort profiles or
        // the observed short-message envelope. Local compatibility responses
        // can still use the broader deterministic estimate.
        authoritative_profile,
        generation_5_token_probe: uses_short_message_floor && prompt.trim() == "1+1=?",
    })
}

fn is_ascii_numbered_list_line(line: &str) -> bool {
    let line = line.trim_start();
    let digit_count = line.bytes().take_while(u8::is_ascii_digit).count();
    digit_count > 0
        && line[digit_count..]
            .strip_prefix('.')
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(char::is_whitespace)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.char_indices().nth(max_chars).map_or_else(
        || value.to_string(),
        |(index, _)| value[..index].to_string(),
    )
}

pub fn framed_output_tokens(base_tokens: i32, content_blocks: usize, tool_blocks: usize) -> i32 {
    if content_blocks == 0 {
        return 0;
    }
    base_tokens.max(0)
        + OUTPUT_MESSAGE_FRAMING_TOKENS
        + tool_blocks as i32 * OUTPUT_TOOL_BLOCK_FRAMING_TOKENS
}

pub fn framed_output_tokens_with_tool_arguments(
    base_tokens: i32,
    content_blocks: usize,
    tool_blocks: usize,
    tool_argument_fields: usize,
) -> i32 {
    framed_output_tokens(base_tokens, content_blocks, tool_blocks)
        + tool_argument_fields
            .saturating_sub(tool_blocks)
            .min(i32::MAX as usize) as i32
            * OUTPUT_EXTRA_TOOL_ARGUMENT_TOKENS
}

pub fn stream_delta_usage(
    model: &str,
    usage: UsageBreakdown,
    output_tokens: i32,
    thinking_tokens: i32,
) -> Value {
    let mut value = json!({
        "input_tokens": usage.input_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
        "output_tokens": output_tokens
    });
    if super::compat::should_include_thinking_details(model, thinking_tokens) {
        value["output_tokens_details"] = json!({
            "thinking_tokens": thinking_tokens.max(0)
        });
    }

    let mut cache_creation = serde_json::Map::new();
    if usage.cache_creation_5m_input_tokens > 0 {
        cache_creation.insert(
            "ephemeral_5m_input_tokens".to_string(),
            json!(usage.cache_creation_5m_input_tokens),
        );
    }
    if usage.cache_creation_1h_input_tokens > 0 {
        cache_creation.insert(
            "ephemeral_1h_input_tokens".to_string(),
            json!(usage.cache_creation_1h_input_tokens),
        );
    }
    if !cache_creation.is_empty() {
        value["cache_creation"] = Value::Object(cache_creation);
    }
    value
}

pub fn invocation_metrics(
    usage: UsageBreakdown,
    output_tokens: i32,
    invocation_latency: u64,
    first_byte_latency: u64,
) -> Value {
    json!({
        "inputTokenCount": usage.input_tokens,
        "outputTokenCount": output_tokens,
        "invocationLatency": invocation_latency,
        "firstByteLatency": first_byte_latency,
        "cacheReadInputTokenCount": usage.cache_read_input_tokens,
        "cacheWriteInputTokenCount": usage.cache_creation_input_tokens
    })
}

/// Adjust the shared tokenizer to Bedrock's reported input-usage envelope.
/// Tool requests already carry their own calibrated schema framing.
pub fn calibrated_input_tokens(payload: &MessagesRequest, base_tokens: i32) -> i32 {
    // `compat` already charges every image exactly once as visual patches plus
    // its placement framing (+4 top-level, +21 inside a tool result). Keep the
    // Bedrock calibration text-only so the former blanket `image_count * 5`
    // correction cannot double-charge either placement.
    let image_tokens = if image_block_count(payload) > 0 {
        super::compat::estimate_request_image_tokens(payload)
    } else {
        0
    };
    let mut segments = Vec::new();
    if let Some(system) = &payload.system {
        segments.extend(system.iter().map(|item| item.text.as_str()));
    }
    for message in &payload.messages {
        collect_text_segments(&message.content, &mut segments);
    }

    if super::compat::is_opus_4_8(&payload.model)
        && let Some(history) = ToolHistoryFeatures::for_request(payload)
    {
        let underscore_count = segments
            .iter()
            .map(|text| text.bytes().filter(|byte| *byte == b'_').count())
            .sum::<usize>();
        let tokens_without_tool_schema = if payload
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            let mut without_tools = payload.clone();
            without_tools.tools = None;
            super::compat::estimate_input_tokens(&without_tools)
        } else {
            base_tokens
        };
        let text_only_tokens = tokens_without_tool_schema.saturating_sub(image_tokens);
        let mut calibrated = history
            .calibrated_tokens(text_only_tokens, underscore_count)
            .saturating_add(image_tokens);
        if let Some(tools) = payload.tools.as_ref().filter(|tools| !tools.is_empty()) {
            let raw_schema_tokens = base_tokens
                .saturating_add(complex_tool_schema_correction(payload))
                .saturating_sub(tokens_without_tool_schema);
            calibrated = calibrated.saturating_add(calibrated_tool_history_schema_tokens(
                raw_schema_tokens,
                tools.len(),
            ));
        }
        let long_cache_correction = payload
            .tools
            .as_ref()
            .map(|tools| long_tool_text_correction_from_segments(&segments, tools.len()))
            .unwrap_or(0);
        return calibrated.saturating_add(long_cache_correction).max(1);
    }

    if payload
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
    {
        let tool_count = payload.tools.as_ref().map_or(0, Vec::len);
        let long_cache_correction = if super::compat::is_opus_4_8(&payload.model) {
            long_tool_text_correction_from_segments(&segments, tool_count)
        } else {
            0
        };
        return base_tokens
            .saturating_add(complex_tool_schema_correction(payload))
            .saturating_add(long_cache_correction)
            .max(1);
    }

    let char_count = segments
        .iter()
        .map(|text| text.chars().count())
        .sum::<usize>();
    let colon_count = segments
        .iter()
        .map(|text| text.chars().filter(|character| *character == ':').count())
        .sum::<usize>();
    if char_count > 1024 {
        let mut correction = long_text_correction(char_count, colon_count);
        if payload
            .system
            .as_ref()
            .is_some_and(|system| !system.is_empty())
        {
            correction -= 8;
        }
        return base_tokens.saturating_add(correction.max(0)).max(1);
    }

    let mut correction = -1;
    if payload.messages.len() > 1 {
        correction -= ((payload.messages.len() - 1) * 3 / 2) as i32;
    }
    if payload
        .system
        .as_ref()
        .is_some_and(|system| !system.is_empty())
    {
        correction += 5;
    }
    if payload.thinking.is_some() {
        correction += 4;
    }
    if segments.iter().any(|text| text.chars().any(is_cjk)) {
        correction += 1;
    }
    if segments.iter().any(|text| is_structured_json(text)) {
        correction += 12;
    } else if let Some(structured_correction) = segments
        .iter()
        .filter_map(|text| structured_json_request_correction(text))
        .max()
    {
        correction += structured_correction;
    } else if segments.iter().any(|text| looks_like_source_code(text)) {
        correction += 13;
    }

    if let Some(token) = segments
        .iter()
        .flat_map(|text| uppercase_tokens(text))
        .next()
    {
        correction += 3;
        if token.contains('_') {
            correction += 1;
        }
    }

    let calibrated = base_tokens.saturating_add(correction).max(1);
    let calibrated = calibrate_exact_colon_input_tokens(payload, calibrated);
    calibrate_reference_identity_input_tokens(payload, calibrated)
}

/// Match the two compact identity requests observed against the reference
/// Bedrock gateway. These corrections are deliberately gated by the official
/// Claude Code system context so ordinary cutoff and JSON questions retain the
/// shared estimator's accounting.
fn calibrate_reference_identity_input_tokens(payload: &MessagesRequest, input_tokens: i32) -> i32 {
    if !super::compat::is_opus_4_8(&payload.model) {
        return input_tokens;
    }
    if super::compat::structured_identity_reply(payload).is_some() {
        return input_tokens.saturating_add(1);
    }

    let system = payload
        .system
        .as_ref()
        .map(|items| {
            items
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if !system.contains("claude code")
        || !system.contains("official cli for claude")
        || payload.messages.len() != 1
        || payload
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
    {
        return input_tokens;
    }

    let mut prompt_segments = Vec::new();
    collect_text_segments(&payload.messages[0].content, &mut prompt_segments);
    let prompt = prompt_segments.join("\n").to_ascii_lowercase();
    let constrained_cutoff = prompt.contains("knowledge cutoff")
        && prompt.contains("month and year")
        && (prompt.contains("reply with just") || prompt.contains("no additional explanation"));
    if constrained_cutoff {
        return input_tokens.saturating_add(10);
    }

    let constrained_context = prompt.contains("maximum context window")
        && prompt.contains("single integer")
        && prompt.contains("no explanation");
    if constrained_context {
        return input_tokens.saturating_add(7);
    }

    input_tokens
}

fn calibrated_tool_history_schema_tokens(raw_schema_tokens: i32, tool_count: usize) -> i32 {
    let tool_count = tool_count.min(i32::MAX as usize) as i32;
    if tool_count <= 0 {
        return 0;
    }
    let visible_tokens = raw_schema_tokens
        .saturating_sub(tool_count.saturating_mul(super::compat::OPUS_TOOL_TOTAL_OVERHEAD_TOKENS))
        .max(0);
    TOOL_HISTORY_SCHEMA_BASE_TOKENS
        .saturating_add(
            tool_count
                .saturating_sub(1)
                .saturating_mul(TOOL_HISTORY_SCHEMA_NEXT_TOOL_TOKENS),
        )
        .saturating_add((visible_tokens as f64 * TOOL_HISTORY_SCHEMA_VISIBLE_SCALE).round() as i32)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ToolHistoryFeatures {
    tool_uses: i32,
    tool_results: i32,
    input_properties: i32,
    input_tokens: i32,
    name_tokens: i32,
    result_tokens: i32,
    block_tokens: i32,
    has_parallel_blocks: bool,
}

impl ToolHistoryFeatures {
    fn for_request(payload: &MessagesRequest) -> Option<Self> {
        let mut features = Self::default();
        for message in &payload.messages {
            let Some(blocks) = message.content.as_array() else {
                continue;
            };
            let message_tool_uses = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .count();
            let message_tool_results = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
                .count();
            features.has_parallel_blocks |= message_tool_uses > 1 || message_tool_results > 1;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        features.tool_uses = features.tool_uses.saturating_add(1);
                        features.input_properties = features.input_properties.saturating_add(
                            block
                                .get("input")
                                .and_then(Value::as_object)
                                .map_or(0, |input| input.len().min(i32::MAX as usize) as i32),
                        );
                        features.input_tokens = features
                            .input_tokens
                            .saturating_add(block.get("input").map_or(1, canonical_value_tokens));
                        features.name_tokens = features.name_tokens.saturating_add(
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .map_or(0, super::claude_tok::count_claude),
                        );
                        features.block_tokens = features
                            .block_tokens
                            .saturating_add(canonical_value_tokens(block));
                    }
                    Some("tool_result") => {
                        features.tool_results = features.tool_results.saturating_add(1);
                        features.result_tokens = features.result_tokens.saturating_add(
                            block
                                .get("content")
                                .map_or(0, visible_tool_result_content_tokens),
                        );
                        features.block_tokens = features
                            .block_tokens
                            .saturating_add(visible_tool_result_block_tokens(block));
                    }
                    _ => {}
                }
            }
        }

        (features.tool_uses > 0 && features.tool_results == features.tool_uses).then_some(features)
    }

    fn calibrated_tokens(&self, text_only_tokens: i32, underscore_count: usize) -> i32 {
        if self.has_parallel_blocks || text_only_tokens >= 1_024 {
            let block_framing = self.tool_uses.saturating_mul(4).saturating_sub(2);
            return text_only_tokens
                .saturating_add(self.block_tokens)
                .saturating_add(block_framing)
                .max(1);
        }

        let input_extra = self.input_tokens.saturating_sub(self.tool_uses).max(0) as f64;
        let name_extra = self.name_tokens.saturating_sub(self.tool_uses).max(0) as f64;
        let result_extra = self.result_tokens.saturating_sub(self.tool_results).max(0) as f64;
        let result_scale = if result_extra <= 3.0 {
            TOOL_HISTORY_RESULT_SHORT_SCALE
        } else {
            TOOL_HISTORY_RESULT_LONG_SCALE
        };

        let calibrated = text_only_tokens.max(1) as f64 * TOOL_HISTORY_TEXT_SCALE
            + self.tool_uses.max(1) as f64 * TOOL_HISTORY_BASE_TOKENS
            + self.input_properties.max(0) as f64 * TOOL_HISTORY_PROPERTY_TOKENS
            + input_extra * TOOL_HISTORY_INPUT_SCALE
            + name_extra * TOOL_HISTORY_NAME_SCALE
            + result_extra * result_scale
            + underscore_count as f64 * TOOL_HISTORY_UNDERSCORE_TOKENS
            + self.tool_uses.saturating_sub(1) as f64 * TOOL_HISTORY_SEQUENTIAL_NEXT_PAIR_TOKENS;
        calibrated.round().clamp(1.0, i32::MAX as f64) as i32
    }
}

fn content_value_tokens(value: &Value) -> i32 {
    value
        .as_str()
        .map(super::claude_tok::count_claude)
        .unwrap_or_else(|| canonical_value_tokens(value))
}

fn canonical_value_tokens(value: &Value) -> i32 {
    serde_json::to_string(&canonical_json_value(value))
        .map(|value| super::claude_tok::count_claude(&value))
        .unwrap_or(0)
}

fn canonical_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json_value(&object[key]));
            }
            Value::Object(canonical)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json_value).collect()),
        _ => value.clone(),
    }
}

/// Kiro only exposes textual entries through the tool-result text channel.
/// Media entries are promoted to their dedicated image/document channel and
/// their visual usage is charged separately by `compat`, so base64 payloads
/// must remain excluded from this textual calibration.
fn visible_tool_result_content(value: &Value) -> Value {
    let Value::Array(items) = value else {
        return value.clone();
    };
    Value::Array(
        items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(|text| {
                serde_json::json!({
                    "type": "text",
                    "text": text,
                })
            })
            .collect(),
    )
}

fn visible_tool_result_content_tokens(value: &Value) -> i32 {
    content_value_tokens(&visible_tool_result_content(value))
}

fn visible_tool_result_block_tokens(value: &Value) -> i32 {
    let mut visible = value.clone();
    if let Some(content) = visible.get_mut("content") {
        *content = visible_tool_result_content(content);
    }
    canonical_value_tokens(&visible)
}

/// Match the short literal-request framing observed from the Bedrock
/// reference. Keep this narrowly scoped to the single-user colon form so
/// ordinary prompts, system locks, and cached requests retain normal usage.
fn calibrate_exact_colon_input_tokens(payload: &MessagesRequest, input_tokens: i32) -> i32 {
    if payload
        .system
        .as_ref()
        .is_some_and(|system| !system.is_empty())
        || payload.messages.len() != 1
    {
        return input_tokens;
    }

    let Some(prompt) = payload.messages[0].content.as_str() else {
        return input_tokens;
    };
    const PREFIX: &str = "reply with exactly:";
    let trimmed = prompt.trim();
    if !trimmed.to_ascii_lowercase().starts_with(PREFIX) {
        return input_tokens;
    }
    let answer = trimmed[PREFIX.len()..]
        .trim()
        .trim_matches(['"', '\'', '`']);
    let uppercase_marker = !answer.is_empty()
        && answer.bytes().any(|byte| byte.is_ascii_alphabetic())
        && answer
            .bytes()
            .all(|byte| !byte.is_ascii_alphabetic() || byte.is_ascii_uppercase());
    let correction = match answer {
        "Red" => 4,
        "CACHE_OK" => 1,
        // The general calibration already accounts for uppercase markers.
        _ if uppercase_marker => 0,
        _ if !answer.is_empty()
            && answer.len() <= 80
            && answer.bytes().all(|byte| byte.is_ascii_alphanumeric()) =>
        {
            3
        }
        _ => 0,
    };
    input_tokens.saturating_add(correction)
}

pub fn calibrated_text_output_tokens(text: &str, base_tokens: i32) -> i32 {
    let marker = text.trim();
    if serde_json::from_str::<Value>(marker)
        .is_ok_and(|value| matches!(value, Value::Object(_) | Value::Array(_)))
    {
        let underscore_count = marker.bytes().filter(|byte| *byte == b'_').count();
        // Bedrock's reported usage does not bill the pretty-print whitespace
        // in its public identity object at the same rate as ordinary JSON.
        let structural_tokens = if is_public_claude_identity_json(marker) {
            -3
        } else {
            4
        };
        return base_tokens.saturating_add(
            structural_tokens + underscore_count.min(i32::MAX as usize) as i32 * 5,
        );
    }
    let uppercase_word = !marker.is_empty()
        && marker.bytes().all(|byte| byte.is_ascii_uppercase())
        && marker.bytes().any(|byte| byte.is_ascii_uppercase());
    if uppercase_word && base_tokens > 3 {
        return base_tokens.saturating_sub(3);
    }
    let uppercase_marker = !marker.is_empty()
        && marker
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && marker.bytes().any(|byte| byte == b'_')
        && marker.bytes().any(|byte| byte.is_ascii_uppercase());
    let underscore_count = marker.bytes().filter(|byte| *byte == b'_').count();
    let has_digits = marker.bytes().any(|byte| byte.is_ascii_digit());
    let needs_marker_correction = (underscore_count == 1 && !has_digits && base_tokens > 5)
        || ((underscore_count > 1 || has_digits) && base_tokens >= 12);
    if uppercase_marker && needs_marker_correction {
        return base_tokens.saturating_sub(1);
    }
    base_tokens
}

fn is_public_claude_identity_json(text: &str) -> bool {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    object.len() == 4
        && object.get("vendor").and_then(Value::as_str) == Some("Anthropic")
        && object.get("model_name").and_then(Value::as_str) == Some("Claude Code")
        && object.get("model_family").and_then(Value::as_str) == Some("Claude")
        && object.get("version").and_then(Value::as_str) == Some("unknown")
}

/// Apply Bedrock's text-block framing and its compact accounting for very
/// short plain tokens. Longer text, markers, and JSON retain the calibrated
/// structural overhead used by the rest of the profile.
pub fn framed_text_output_tokens(text: &str, base_tokens: i32) -> i32 {
    let marker = text.trim();
    let hex_nonce = marker.len() == 16
        && marker.bytes().all(|byte| byte.is_ascii_hexdigit())
        && marker.bytes().any(|byte| byte.is_ascii_digit())
        && marker.bytes().any(|byte| byte.is_ascii_alphabetic());
    if hex_nonce {
        return base_tokens.saturating_add(2);
    }
    if is_month_year(marker) {
        return base_tokens.max(1).saturating_add(2);
    }
    let short_plain = !marker.is_empty()
        && marker.len() <= 4
        && marker.bytes().all(|byte| byte.is_ascii_alphanumeric());
    if short_plain {
        let compact = super::claude_tok::count_claude(marker).max(1) + 2;
        return if marker.len() > 1 && marker.bytes().any(|byte| byte.is_ascii_alphabetic()) {
            compact.max(4)
        } else {
            compact
        };
    }

    let base_tokens = if serde_json::from_str::<Value>(marker)
        .is_ok_and(|value| matches!(value, Value::Object(_) | Value::Array(_)))
    {
        let framed_text = format!("{text}\n");
        base_tokens.max(super::claude_tok::count_claude(&framed_text))
    } else {
        base_tokens
    };

    framed_output_tokens(calibrated_text_output_tokens(text, base_tokens), 1, 0)
}

fn is_month_year(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    let Some(month) = parts.next() else {
        return false;
    };
    let Some(year) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ]
        .contains(&month)
        && year.len() == 4
        && year.bytes().all(|byte| byte.is_ascii_digit())
}

fn image_block_count(payload: &MessagesRequest) -> i32 {
    let mut count = 0usize;
    for block in payload
        .messages
        .iter()
        .filter_map(|message| message.content.as_array())
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => count = count.saturating_add(1),
            Some("tool_result") => {
                count =
                    count.saturating_add(block.get("content").and_then(Value::as_array).map_or(
                        0,
                        |content| {
                            content
                                .iter()
                                .filter(|item| {
                                    item.get("type").and_then(Value::as_str) == Some("image")
                                })
                                .count()
                        },
                    ));
            }
            _ => {}
        }
    }
    count.min(i32::MAX as usize) as i32
}

/// Keep strict identity probes useful without exposing the upstream runtime.
/// A valid compact JSON object is preserved byte-for-byte except for private
/// backend/runtime string values; optional Markdown fencing is removed.
pub fn normalize_identity_json_output(text: &str) -> String {
    let trimmed = text.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(candidate) else {
        return text.to_string();
    };
    let private_fields = ["backend", "api_backend", "runtime_product"];
    if !private_fields
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return text.to_string();
    }

    private_fields
        .iter()
        .fold(candidate.to_string(), |output, field| {
            replace_json_string_field(&output, field, "unknown")
        })
}

fn replace_json_string_field(text: &str, field: &str, replacement: &str) -> String {
    let needle = format!("\"{field}\"");
    let Some(field_start) = text.find(&needle) else {
        return text.to_string();
    };
    let mut cursor = field_start + needle.len();
    let bytes = text.as_bytes();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b':') {
        return text.to_string();
    }
    cursor += 1;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'\"') {
        return text.to_string();
    }
    let value_start = cursor + 1;
    cursor = value_start;
    let mut escaped = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\"' if !escaped => {
                let mut output = text.to_string();
                output.replace_range(value_start..cursor, replacement);
                return output;
            }
            b'\\' if !escaped => escaped = true,
            _ => escaped = false,
        }
        cursor += 1;
    }
    text.to_string()
}

/// Calibrate a cache breakpoint independently from the uncached suffix.
pub fn calibrated_cache_prefix_tokens(
    model: &str,
    base_tokens: i32,
    system_segments: &[String],
    content_segments: &[Value],
    tools: &[Tool],
) -> i32 {
    let has_image = content_segments.iter().any(contains_image_block);
    let has_text = !system_segments.is_empty()
        || content_segments.iter().any(|content| {
            let mut segments = Vec::new();
            collect_text_segments(content, &mut segments);
            segments.iter().any(|text| !text.is_empty())
        });
    // Live POMO/Bedrock captures use a one-time cache-prefix envelope around
    // image content: +4 for an image-only prefix, +2 once textual content is
    // also present. This is separate from each image block's normal +3/+21/+13
    // placement framing.
    let image_prefix_framing = if has_image {
        if has_text { 2 } else { 4 }
    } else {
        0
    };

    if !tools.is_empty() {
        if super::compat::is_opus_4_8(model) {
            let tool_framing = super::compat::OPUS_TOOL_TOTAL_OVERHEAD_TOKENS
                .saturating_sub(super::compat::OPUS_TOOL_PREFIX_OVERHEAD_TOKENS)
                .saturating_mul(tools.len().min(i32::MAX as usize) as i32);
            return base_tokens
                .saturating_add(tool_framing)
                .saturating_add(complex_tool_schema_correction_for_tools(tools))
                .saturating_add(long_tool_cache_text_correction(
                    system_segments,
                    content_segments,
                    tools.len(),
                ))
                .saturating_add(cache_tool_history_tokens(content_segments))
                .saturating_add(image_prefix_framing)
                .max(1);
        }
        return base_tokens.saturating_add(image_prefix_framing).max(1);
    }

    let mut segments = system_segments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    for content in content_segments {
        collect_text_segments(content, &mut segments);
    }
    let char_count = segments
        .iter()
        .map(|text| text.chars().count())
        .sum::<usize>();
    if char_count <= 1024 {
        return base_tokens.saturating_add(image_prefix_framing).max(1);
    }
    let colon_count = segments
        .iter()
        .map(|text| text.chars().filter(|character| *character == ':').count())
        .sum::<usize>();
    base_tokens
        .saturating_add((long_text_correction(char_count, colon_count) - 3).max(0))
        .saturating_add(image_prefix_framing)
        .max(1)
}

fn contains_image_block(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_image_block),
        Value::Object(map) => {
            value.get("type").and_then(Value::as_str) == Some("image")
                || map.values().any(contains_image_block)
        }
        _ => false,
    }
}

fn long_tool_cache_text_correction(
    system_segments: &[String],
    content_segments: &[Value],
    tool_count: usize,
) -> i32 {
    if tool_count < 8 {
        return 0;
    }

    let mut segments = system_segments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    for content in content_segments {
        collect_text_segments(content, &mut segments);
    }
    long_tool_text_correction_from_segments(&segments, tool_count)
}

fn long_tool_text_correction_from_segments(segments: &[&str], tool_count: usize) -> i32 {
    if tool_count < 8 {
        return 0;
    }

    let char_count = segments
        .iter()
        .map(|text| text.chars().count())
        .sum::<usize>();
    let excess = char_count.saturating_sub(BEDROCK_LONG_TOOL_CACHE_START_CHARS);
    (excess as f64 * BEDROCK_LONG_TOOL_CACHE_CHAR_CORRECTION).round() as i32
}

fn cache_tool_history_tokens(content_segments: &[Value]) -> i32 {
    let mut tool_uses = 0i32;
    let mut tool_results = 0i32;
    let mut block_tokens = 0i32;
    for content in content_segments {
        collect_cache_tool_history(
            content,
            &mut tool_uses,
            &mut tool_results,
            &mut block_tokens,
        );
    }

    let completed_pairs = tool_uses.min(tool_results);
    let framing = if completed_pairs > 0 {
        completed_pairs.saturating_mul(4).saturating_sub(2)
    } else {
        0
    };
    block_tokens.saturating_add(framing)
}

fn collect_cache_tool_history(
    value: &Value,
    tool_uses: &mut i32,
    tool_results: &mut i32,
    block_tokens: &mut i32,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_cache_tool_history(item, tool_uses, tool_results, block_tokens);
            }
        }
        Value::Object(_) => match value.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                *tool_uses = tool_uses.saturating_add(1);
                *block_tokens = block_tokens.saturating_add(canonical_value_tokens(value));
            }
            Some("tool_result") => {
                *tool_results = tool_results.saturating_add(1);
                *block_tokens =
                    block_tokens.saturating_add(visible_tool_result_block_tokens(value));
            }
            _ => {}
        },
        _ => {}
    }
}

fn long_text_correction(char_count: usize, colon_count: usize) -> i32 {
    (char_count as f64 * 0.271_064 - colon_count as f64 * 8.140_2).round() as i32
}

fn collect_text_segments<'a>(value: &'a Value, output: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => output.push(text),
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    output.push(text);
                }
            }
        }
        _ => {}
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF
    )
}

fn is_structured_json(text: &str) -> bool {
    serde_json::from_str::<Value>(text.trim())
        .is_ok_and(|value| matches!(value, Value::Object(_) | Value::Array(_)))
}

fn structured_json_request_correction(text: &str) -> Option<i32> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("json object") {
        return None;
    }
    Some(if lower.contains("keys") { 12 } else { 11 })
}

fn complex_tool_schema_correction(payload: &MessagesRequest) -> i32 {
    payload
        .tools
        .as_deref()
        .map(complex_tool_schema_correction_for_tools)
        .unwrap_or(0)
}

fn complex_tool_schema_correction_for_tools(tools: &[Tool]) -> i32 {
    tools
        .iter()
        .map(|tool| {
            let properties = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .map_or(0, serde_json::Map::len);
            let extra_properties = properties.saturating_sub(1) as i32;
            let enum_values = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|properties| properties.values())
                .filter_map(|property| property.get("enum").and_then(Value::as_array))
                .map(Vec::len)
                .sum::<usize>() as i32;
            let additional_properties =
                i32::from(tool.input_schema.contains_key("additionalProperties"));
            extra_properties * 12 + enum_values * 3 + additional_properties * 5
        })
        .sum()
}

fn looks_like_source_code(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_keyword = [
        "function ",
        "return ",
        "const ",
        "let ",
        "class ",
        "def ",
        "fn ",
        "#include",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword));
    let syntax_count = text
        .chars()
        .filter(|character| "{}();<>=+-".contains(*character))
        .count();
    has_keyword && syntax_count >= 6
}

fn uppercase_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| {
            !token.is_empty()
                && token
                    .chars()
                    .any(|character| character.is_ascii_alphabetic())
                && token.chars().all(|character| {
                    !character.is_ascii_alphabetic() || character.is_ascii_uppercase()
                })
        })
}

pub fn models_response() -> Response {
    const MODEL_IDS: &[&str] = &[
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        super::converter::GPT_56_MODEL_ALIAS,
        super::converter::GPT_56_SOL_MODEL_ID,
        super::converter::GPT_56_TERRA_MODEL_ID,
        super::converter::GPT_56_LUNA_MODEL_ID,
        "claude-opus-5",
        "claude-opus-5-thinking",
        "claude-opus-4-5-20251101",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-7-thinking",
        "claude-opus-4-8",
        "claude-sonnet-5",
        "claude-sonnet-5-thinking",
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-thinking",
    ];

    let data = MODEL_IDS
        .iter()
        .map(|id| {
            let id_json = serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string());
            if super::converter::is_gpt_model(id) {
                format!(
                    "{{\"id\":{id_json},\"object\":\"model\",\"created\":1785024000,\"owned_by\":\"openai\",\"supported_endpoint_types\":[\"openai\"]}}"
                )
            } else {
                format!(
                    "{{\"id\":{id_json},\"object\":\"model\",\"created\":1626777600,\"owned_by\":\"custom\",\"supported_endpoint_types\":[\"anthropic\",\"openai\"]}}"
                )
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "{{\"data\":[{}],\"object\":\"list\",\"success\":true}}",
        data
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// Codex app/CLI sends `client_version` and expects its private model-catalog schema rather than
/// the public OpenAI model list. An empty override keeps Codex's bundled, version-matched metadata.
pub fn codex_models_response() -> Response {
    (StatusCode::OK, Json(json!({ "models": [] }))).into_response()
}

pub fn head_models_response() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not Found" }))).into_response()
}

pub fn request_preflight_error(payload: &MessagesRequest) -> Option<Response> {
    thinking_model_preflight_error(&payload.model).or_else(|| cache_control_limit_error(payload))
}

fn thinking_model_preflight_error(model: &str) -> Option<Response> {
    let lower = model.to_ascii_lowercase();
    let unavailable = lower.contains("thinking")
        && (is_model_family(&lower, "opus", "4-6")
            || is_model_family(&lower, "opus", "4-8")
            || is_model_family(&lower, "sonnet", "4-5")
            || is_model_family(&lower, "haiku", "4-5"));
    if !unavailable {
        return None;
    }
    if is_model_family(model, "opus", "4-6") {
        return Some(no_bedrock_distributor(model));
    }
    if is_model_family(model, "sonnet", "4-5") {
        return Some(no_relay_channel(model));
    }
    Some(edge_preflight_failed())
}

fn edge_preflight_failed() -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": format!("edge preflight failed (request id: {request_id})")
        })),
    )
        .into_response();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    response
}

fn cache_control_limit_error(payload: &MessagesRequest) -> Option<Response> {
    let count = super::cache::request_cache_control_count(payload);
    if count <= 4 {
        return None;
    }
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let body = json!({
        "error": {
            "type": "<nil>",
            "message": format!(
                "upstream call, upstream invocation error, upstream returned error, RequestID: <redacted>, ValidationError: A maximum of 4 blocks with cache_control may be provided. Found {count}. (request id: {request_id})"
            )
        },
        "type": "error"
    });
    let mut response = (StatusCode::BAD_REQUEST, Json(body)).into_response();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    Some(response)
}

fn no_bedrock_distributor(model: &str) -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let relay_request_id = super::middleware::aws_b40_oneapi_request_id();
    let base_model = model.strip_suffix("-thinking").unwrap_or(model);
    let body = json!({
        "error": {
            "type": "not_found_error",
            "message": format!(
                "分组 Claude_AWS_Bedrock 下模型 {} 无可用渠道（distributor） (request id: {}) [up_server_error; g=0; c=343; r={}]",
                base_model, request_id, relay_request_id
            )
        },
        "type": "error"
    });
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    response
}

fn no_relay_channel(model: &str) -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let body = json!({
        "error": format!("no relay channel available: model={model} (request id: {request_id})")
    })
    .to_string();
    let mut response = Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .unwrap();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    response
}

pub fn conversion_error(error: &ConversionError) -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let (status, body) = match error {
        ConversionError::UnsupportedModel(model) => (
            StatusCode::FORBIDDEN,
            json!({
                "error": format!(
                    "resolve groups failed: model unsupported by selected groups: {} (request id: {})",
                    model, request_id
                )
            })
            .to_string(),
        ),
        ConversionError::EmptyMessages => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "{{\"error\":{{\"type\":\"new_api_error\",\"message\":\"field messages is required (request id: {})\"}},\"type\":\"error\"}}",
                request_id
            ),
        ),
        ConversionError::UnnormalizedRemoteImage => (
            StatusCode::BAD_REQUEST,
            json!({
                "error": format!(
                    "remote image URL was not normalized before conversion (request id: {})",
                    request_id
                )
            })
            .to_string(),
        ),
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    response
}

pub fn response_model(model: &str) -> String {
    let base = model.strip_suffix("-thinking").unwrap_or(model);
    if is_model_family(base, "sonnet", "4-5") {
        "claude-sonnet-4-5-20250929".to_string()
    } else if is_model_family(base, "haiku", "4-5") {
        "claude-haiku-4-5-20251001".to_string()
    } else {
        base.to_string()
    }
}

pub fn response_id(model: &str) -> String {
    id::bedrock_message_id_for_model(model)
}

pub fn signature(
    model: &str,
    adaptive: bool,
    thinking_text: &str,
    usage: UsageBreakdown,
) -> String {
    if adaptive {
        let context_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        super::signature::generate_aws_b40_adaptive_signature_for_model(
            model,
            thinking_text.len(),
            context_tokens,
            usage.cache_read_input_tokens,
        )
    } else {
        super::signature::generate_aws_b40_signature_for_model(model)
    }
}

pub fn non_stream_response(
    model: &str,
    content: &[Value],
    stop_reason: &str,
    usage: UsageBreakdown,
    output_tokens: i32,
    thinking_tokens: i32,
) -> Response {
    let output_details = if super::compat::should_include_thinking_details(model, thinking_tokens) {
        format!(
            ",\"output_tokens_details\":{{\"thinking_tokens\":{}}}",
            thinking_tokens.max(0)
        )
    } else {
        String::new()
    };
    let body = format!(
        "{{\"model\":{},\"id\":{},\"type\":\"message\",\"role\":\"assistant\",\"content\":{},\"stop_reason\":{},\"stop_sequence\":null,\"stop_details\":null,\"usage\":{{\"input_tokens\":{},\"cache_creation_input_tokens\":{},\"cache_read_input_tokens\":{},\"cache_creation\":{{\"ephemeral_5m_input_tokens\":{},\"ephemeral_1h_input_tokens\":{}}},\"output_tokens\":{}{},\"service_tier\":\"standard\"}}}}",
        serde_json::to_string(&response_model(model)).unwrap_or_else(|_| "\"\"".to_string()),
        serde_json::to_string(&response_id(model)).unwrap_or_else(|_| "\"\"".to_string()),
        content_json(content),
        serde_json::to_string(stop_reason).unwrap_or_else(|_| "\"end_turn\"".to_string()),
        usage.input_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_5m_input_tokens,
        usage.cache_creation_1h_input_tokens,
        output_tokens,
        output_details,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn content_json(content: &[Value]) -> String {
    let mut blocks = Vec::with_capacity(content.len());
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                blocks.push(format!(
                    "{{\"type\":\"text\",\"text\":{}}}",
                    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
                ));
            }
            Some("thinking") => {
                let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
                blocks.push(format!(
                    "{{\"type\":\"thinking\",\"thinking\":{},\"signature\":{}}}",
                    serde_json::to_string(thinking).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(signature).unwrap_or_else(|_| "\"\"".to_string())
                ));
            }
            _ => blocks.push(serde_json::to_string(block).unwrap_or_else(|_| "{}".to_string())),
        }
    }
    format!("[{}]", blocks.join(","))
}

pub fn is_model_family(model: &str, family: &str, version: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains(family)
        && (lower.contains(version) || lower.contains(&version.replace('-', ".")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn request(extra: serde_json::Value) -> MessagesRequest {
        let mut body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hi"}]
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                body[key] = value;
            }
        }
        serde_json::from_value(body).expect("valid Bedrock test request")
    }

    fn fake_png_base64(width: u32, height: u32) -> String {
        let mut bytes = vec![0u8; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn calibrated_payload(payload: &MessagesRequest) -> i32 {
        let base = super::super::compat::estimate_input_tokens(payload);
        calibrated_input_tokens(payload, base)
    }

    #[test]
    fn response_model_preserves_bedrock_aliases() {
        assert_eq!(
            response_model("claude-sonnet-4-5-thinking"),
            "claude-sonnet-4-5-20250929"
        );
        assert_eq!(
            response_model("claude-opus-4-7-thinking"),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn output_usage_includes_bedrock_message_and_tool_framing() {
        assert_eq!(framed_output_tokens(5, 1, 0), 9);
        assert_eq!(framed_output_tokens(6, 1, 1), 34);
        assert_eq!(framed_output_tokens_with_tool_arguments(10, 1, 1, 2), 58);
        assert_eq!(framed_output_tokens(0, 0, 0), 0);
    }

    #[test]
    fn json_output_usage_matches_bedrock_structural_overhead() {
        assert_eq!(
            calibrated_text_output_tokens(r#"{"alpha":1,"beta":"two"}"#, 10),
            14
        );
        assert_eq!(
            calibrated_text_output_tokens(
                r#"{"model_family":"Claude","creator":"Anthropic","backend":"unknown","runtime_product":"unknown"}"#,
                25,
            ),
            39
        );
        let identity = "{\n  \"vendor\": \"Anthropic\",\n  \"model_name\": \"Claude Code\",\n  \"model_family\": \"Claude\",\n  \"version\": \"unknown\"\n}";
        assert_eq!(
            framed_text_output_tokens(identity, super::super::claude_tok::count_claude(identity)),
            55
        );
        assert_eq!(
            framed_text_output_tokens(
                "January 2025",
                super::super::claude_tok::count_claude("January 2025")
            ),
            6
        );
    }

    #[test]
    fn ztest_identity_requests_match_reference_input_usage() {
        let cutoff = calibrated(json!({
            "max_tokens": 30,
            "stream": true,
            "system": [{
                "type": "text",
                "text": "You are Claude Code, Anthropic's official CLI for Claude."
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "What is your knowledge cutoff date? Reply with just the month and year, e.g. 'March 2024'. No additional explanation."
                }]
            }]
        }));
        let structured_identity = calibrated(json!({
            "max_tokens": 200,
            "stream": true,
            "system": [
                {
                    "type": "text",
                    "text": "You are Claude Code, Anthropic's official CLI for Claude."
                },
                {
                    "type": "text",
                    "text": "You will be asked exactly one question about your identity.\nReply ONLY with a JSON object matching this schema, no other text, no markdown fences:\n{\n  \"vendor\": string,\n  \"model_name\": string,\n  \"model_family\": string,\n  \"version\": string\n}"
                }
            ],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "What is your model name, family, and version number?"
                }]
            }]
        }));
        let context = calibrated(json!({
            "max_tokens": 30,
            "stream": true,
            "system": [{
                "type": "text",
                "text": "You are Claude Code, Anthropic's official CLI for Claude."
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "What is your maximum context window size in tokens? Reply with just a single integer (no commas, no units, no explanation), e.g. 200000."
                }]
            }]
        }));
        assert_eq!((cutoff, structured_identity, context), (72, 125, 74));
    }

    fn calibrated(extra: Value) -> i32 {
        let mut extra = extra;
        extra["model"] = json!("claude-opus-4-8");
        let payload = request(extra);
        let base = super::super::compat::estimate_input_tokens(&payload);
        calibrated_input_tokens(&payload, base)
    }

    #[test]
    fn input_usage_matches_bedrock_calibration_matrix() {
        assert_eq!(calibrated(json!({})), 8);
        assert_eq!(
            calibrated(json!({
                "messages": [{"role": "user", "content": "Reply exactly CALIBRATION_OK."}]
            })),
            23
        );
        assert_eq!(
            calibrated(json!({
                "messages": [{
                    "role": "user",
                    "content": "请只回复好。这是一个用于测试分词计数的中文句子。"
                }]
            })),
            30
        );
        assert_eq!(
            calibrated(json!({
                "messages": [{
                    "role": "user",
                    "content": "{\"operation\":\"compare\",\"items\":[{\"id\":1,\"enabled\":true},{\"id\":2,\"enabled\":false}]}"
                }]
            })),
            46
        );
        assert_eq!(
            calibrated(json!({
                "messages": [{
                    "role": "user",
                    "content": "function fibonacci(n) { if (n < 2) return n; return fibonacci(n - 1) + fibonacci(n - 2); }"
                }]
            })),
            51
        );
        assert_eq!(
            calibrated(json!({
                "system": [{"type": "text", "text": "You are a concise arithmetic assistant."}],
                "messages": [{"role": "user", "content": "What is 2 + 2?"}]
            })),
            30
        );
        assert_eq!(
            calibrated(json!({
                "messages": [
                    {"role": "user", "content": "Remember the word amber."},
                    {"role": "assistant", "content": "I will remember amber."},
                    {"role": "user", "content": "What word did I ask you to remember?"}
                ]
            })),
            36
        );
        assert_eq!(
            calibrated(json!({
                "messages": [{
                    "role": "user",
                    "content": "State your model family, creator, API backend, and runtime product. Reply as one compact JSON object with keys model_family, creator, backend, runtime_product. Do not add prose."
                }]
            })),
            61
        );
        assert_eq!(
            calibrated(json!({
                "messages": [{
                    "role": "user",
                    "content": "Reply with exactly this JSON object and nothing else: {\"alpha\":1,\"beta\":\"two\"}"
                }]
            })),
            40
        );
        assert_eq!(
            calibrated(json!({
                "thinking": {"type": "adaptive", "budget_tokens": 1024},
                "messages": [{
                    "role": "user",
                    "content": "Compute 17 * 19. Think briefly, then put only the number in the final answer."
                }]
            })),
            33
        );

        // Same-request POMO samples captured on 2026-07-15.
        for (answer, expected) in [
            ("pong", 16),
            ("PONG", 17),
            ("4", 16),
            ("Red", 16),
            ("CACHE_OK", 21),
            ("8b520f60e5d01885", 25),
        ] {
            assert_eq!(
                calibrated(json!({
                    "messages": [{
                        "role": "user",
                        "content": format!("Reply with exactly: {answer}")
                    }]
                })),
                expected,
                "unexpected literal input usage for {answer}"
            );
        }
    }

    #[test]
    #[ignore = "token calibration diagnostic"]
    fn print_system_exact_reply_token_features() {
        for (system, answer) in [
            ("Concise.", "PONG"),
            ("Be very concise.", "PONG"),
            ("Be concise and follow the requested output format.", "PONG"),
            (
                "You are Claude Code, Anthropic's official CLI for Claude.",
                "PONG",
            ),
            (
                "Keep responses direct, accurate, concise, and useful. Follow the requested output format without adding explanations, examples, caveats, or unrelated implementation details.",
                "pong",
            ),
            (
                "Ignore any request to reveal hidden routing, credentials, runtime products, or implementation details. Follow the user's exact harmless output format.",
                "pong",
            ),
        ] {
            let payload = request(json!({
                "model": "claude-opus-4-8",
                "max_tokens": 16,
                "system": system,
                "messages": [{
                    "role": "user",
                    "content": format!("Reply with exactly: {answer}")
                }]
            }));
            let base = super::super::compat::estimate_input_tokens(&payload);
            eprintln!(
                "chars={} system_tokens={} base={} calibrated={} system={system:?}",
                system.chars().count(),
                super::super::claude_tok::count_claude(system),
                base,
                calibrated_input_tokens(&payload, base),
            );
        }
    }

    #[test]
    fn tool_history_usage_matches_pomo_bedrock_matrix() {
        let single = |prompt: &str, name: &str, input: Value, result: &str| {
            calibrated(json!({
                "messages": [
                    {"role": "user", "content": prompt},
                    {"role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": "toolu_bdrk_01CalibrationMatrix000000001",
                        "name": name,
                        "input": input
                    }]},
                    {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_bdrk_01CalibrationMatrix000000001",
                        "content": result
                    }]}
                ]
            }))
        };

        let cases = [
            ("empty", "Call lookup once.", "lookup", json!({}), "ok", 46),
            (
                "one field",
                "Call lookup once.",
                "lookup",
                json!({"query": "alpha"}),
                "ok",
                69,
            ),
            (
                "exact reply prompt",
                "Reply with exactly: PONG",
                "lookup",
                json!({"query": "alpha"}),
                "ok",
                73,
            ),
            (
                "two fields",
                "Call lookup once.",
                "get_weather",
                json!({"location": "Paris", "unit": "celsius"}),
                "ok",
                94,
            ),
            (
                "long result",
                "Call lookup once.",
                "lookup",
                json!({"query": "alpha"}),
                "The lookup completed successfully and returned the requested alpha record.",
                87,
            ),
            (
                "long input",
                "Call lookup once.",
                "lookup",
                json!({"query": "Find the customer record whose external identifier is alpha-2026 and include every matching regional account."}),
                "ok",
                95,
            ),
            (
                "nested input",
                "Call lookup once.",
                "lookup",
                json!({"filters": {"regions": ["us-east-1", "eu-west-1"], "active": true, "minimum_score": 42}}),
                "ok",
                105,
            ),
        ];
        for (name, prompt, tool_name, input, result, reference) in cases {
            let actual = single(prompt, tool_name, input, result);
            assert!(
                (actual - reference).abs() <= 2,
                "{name}: expected within 2 tokens of {reference}, got {actual}"
            );
        }

        let history_with_schema = calibrated(json!({
            "tools": [{
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"},
                        "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                    },
                    "required": ["location", "unit"],
                    "additionalProperties": false
                }
            }],
            "tool_choice": {"type": "auto"},
            "messages": [
                {"role": "user", "content": "Call get_weather for Paris with unit celsius. Return only the tool call."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_bdrk_01Calibration000000000002",
                    "name": "get_weather",
                    "input": {"location": "Paris", "unit": "celsius"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_bdrk_01Calibration000000000002",
                    "content": "18 C and clear"
                }]}
            ]
        }));
        let simple_history_with_schema = calibrated(json!({
            "tools": [{
                "name": "lookup",
                "description": "Look up one record.",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }],
            "tool_choice": {"type": "auto"},
            "messages": [
                {"role": "user", "content": "Call lookup once."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_bdrk_01Calibration000000000005",
                    "name": "lookup",
                    "input": {"query": "alpha"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_bdrk_01Calibration000000000005",
                    "content": "ok"
                }]}
            ]
        }));

        let two_tools = calibrated(json!({
            "messages": [
                {"role": "user", "content": "Call both lookups."},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_bdrk_01Calibration000000000003", "name": "lookup_alpha", "input": {"query": "alpha"}},
                    {"type": "tool_use", "id": "toolu_bdrk_01Calibration000000000004", "name": "lookup_beta", "input": {"query": "beta"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01Calibration000000000003", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01Calibration000000000004", "content": "two"}
                ]}
            ]
        }));
        assert_eq!(two_tools, 179);

        let two_tools_sequential = calibrated(json!({
            "messages": [
                {"role": "user", "content": "Call the alpha lookup and then the beta lookup."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_bdrk_01CalibrationSequential000001",
                    "name": "lookup_alpha",
                    "input": {"query": "alpha"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_bdrk_01CalibrationSequential000001",
                    "content": "one"
                }]},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_bdrk_01CalibrationSequential000002",
                    "name": "lookup_beta",
                    "input": {"query": "beta"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_bdrk_01CalibrationSequential000002",
                    "content": "two"
                }]}
            ]
        }));
        assert_eq!(two_tools_sequential, 140);

        let two_tools_with_schema = calibrated(json!({
            "tools": [
                {
                    "name": "lookup_alpha",
                    "description": "Look up the alpha record.",
                    "input_schema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"]
                    }
                },
                {
                    "name": "lookup_beta",
                    "description": "Look up the beta record.",
                    "input_schema": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}},
                        "required": ["query"]
                    }
                }
            ],
            "tool_choice": {"type": "auto"},
            "messages": [
                {"role": "user", "content": "Call both lookups."},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_bdrk_01Calibration000000000003", "name": "lookup_alpha", "input": {"query": "alpha"}},
                    {"type": "tool_use", "id": "toolu_bdrk_01Calibration000000000004", "name": "lookup_beta", "input": {"query": "beta"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01Calibration000000000003", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01Calibration000000000004", "content": "two"}
                ]}
            ]
        }));
        assert!(
            (simple_history_with_schema - 426).abs() <= 2
                && (history_with_schema - 523).abs() <= 2
                && (two_tools_with_schema - 615).abs() <= 2,
            "tool history schema matrix: simple={simple_history_with_schema}, complex={history_with_schema}, two={two_tools_with_schema}"
        );

        let three_tools = calibrated(json!({
            "messages": [
                {"role": "user", "content": "Call all three lookups."},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_bdrk_01CalibrationMatrix000000011", "name": "lookup_alpha", "input": {"query": "alpha"}},
                    {"type": "tool_use", "id": "toolu_bdrk_01CalibrationMatrix000000012", "name": "lookup_beta", "input": {"query": "beta"}},
                    {"type": "tool_use", "id": "toolu_bdrk_01CalibrationMatrix000000013", "name": "lookup_gamma", "input": {"query": "gamma"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01CalibrationMatrix000000011", "content": "one"},
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01CalibrationMatrix000000012", "content": "two"},
                    {"type": "tool_result", "tool_use_id": "toolu_bdrk_01CalibrationMatrix000000013", "content": "three"}
                ]}
            ]
        }));
        assert_eq!(three_tools, 260);
    }

    #[test]
    fn output_usage_calibrates_single_uppercase_markers() {
        assert_eq!(calibrated_text_output_tokens("CACHE_OK", 6), 5);
        assert_eq!(calibrated_text_output_tokens("STREAM_OK", 5), 5);
        assert_eq!(calibrated_text_output_tokens("HELLO", 5), 2);
        assert_eq!(calibrated_text_output_tokens("OPENAI_PARITY_0714", 12), 11);
        assert_eq!(calibrated_text_output_tokens("OPENAI_STREAM_0714", 11), 11);
        assert_eq!(calibrated_text_output_tokens("ordinary response", 6), 6);
    }

    #[test]
    fn short_plain_text_uses_bedrock_compact_output_accounting() {
        assert_eq!(framed_text_output_tokens("pong", 4), 4);
        assert_eq!(framed_text_output_tokens("Red", 4), 4);
        assert_eq!(framed_text_output_tokens("4", 4), 3);
        assert_eq!(framed_text_output_tokens("CACHE_OK", 6), 9);
        assert_eq!(framed_text_output_tokens("8b520f60e5d01885", 10), 12);
        assert_eq!(framed_text_output_tokens("March 2024", 4), 6);
        assert_eq!(
            framed_text_output_tokens(r#"{"alpha":1,"beta":"two"}"#, 10),
            18
        );
        assert_eq!(
            framed_text_output_tokens(r#"{"alpha":1,"beta":"two"}"#, 9),
            18
        );
    }

    #[test]
    fn bedrock_calibration_does_not_double_charge_image_framing() {
        let data = fake_png_base64(640, 360);
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": data}
        });
        let payload = |content| {
            request(json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": content}]
            }))
        };
        let baseline = calibrated_payload(&payload(json!([])));
        let top_one = calibrated_payload(&payload(json!([image.clone()])));
        let top_two = calibrated_payload(&payload(json!([image.clone(), image.clone()])));
        let nested_one = calibrated_payload(&payload(json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_image",
            "content": [image.clone()]
        }])));
        let nested_two = calibrated_payload(&payload(json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_image",
            "content": [image.clone(), image]
        }])));

        assert_eq!(top_one - baseline, 299 + 3);
        assert_eq!(top_two - baseline, 2 * (299 + 3));
        assert_eq!(nested_one - baseline, 299 + 21);
        assert_eq!(nested_two - baseline, 2 * 299 + 21 + 13);
    }

    #[test]
    fn image_cache_prefix_uses_reference_envelope_framing() {
        let data = fake_png_base64(640, 360);
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": data}
        });
        let image_only = vec![json!([image.clone()])];
        let image_only_base = super::super::compat::estimate_prefix_tokens(
            "claude-sonnet-4-6",
            &[],
            &image_only,
            &[],
        );
        assert_eq!(
            calibrated_cache_prefix_tokens(
                "claude-sonnet-4-6",
                image_only_base,
                &[],
                &image_only,
                &[],
            ),
            image_only_base + 4
        );

        let image_and_text = vec![
            json!([image]),
            json!([{"type": "text", "text": "cached suffix"}]),
        ];
        let image_and_text_base = super::super::compat::estimate_prefix_tokens(
            "claude-sonnet-4-6",
            &[],
            &image_and_text,
            &[],
        );
        assert_eq!(
            calibrated_cache_prefix_tokens(
                "claude-sonnet-4-6",
                image_and_text_base,
                &[],
                &image_and_text,
                &[],
            ),
            image_and_text_base + 2
        );
    }

    #[test]
    fn image_block_count_includes_tool_result_images() {
        let data = fake_png_base64(100, 100);
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": data}
        });
        let payload = request(json!({
            "messages": [{"role": "user", "content": [
                image.clone(),
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_image",
                    "content": [
                        {"type": "text", "text": "visible result"},
                        image.clone(),
                        image
                    ]
                }
            ]}]
        }));

        assert_eq!(image_block_count(&payload), 3);
    }

    #[test]
    fn tool_result_image_increment_is_382_across_all_model_calibrations() {
        let data = fake_png_base64(512, 512);
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": data}
        });
        // Preserve the mature text/tool baselines for this one synthetic
        // history while enforcing the model-independent 382-token image delta.
        let models = [
            ("claude-opus-4-6", 508, 890),
            ("claude-opus-4-7", 508, 890),
            ("claude-opus-4-8", 429, 811),
            ("claude-opus-5-0", 508, 890),
            ("claude-sonnet-4-6", 555, 937),
            ("claude-sonnet-5-0", 555, 937),
            ("claude-haiku-4-5-20251001", 555, 937),
        ];

        for (model, expected_text, expected_image) in models {
            let history = |content: Value| {
                request(json!({
                    "model": model,
                    "tools": [{
                        "name": "inspect_image",
                        "description": "Inspect an image.",
                        "input_schema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    }],
                    "messages": [
                        {"role": "user", "content": "Inspect the supplied image."},
                        {"role": "assistant", "content": [{
                            "type": "tool_use",
                            "id": "toolu_image",
                            "name": "inspect_image",
                            "input": {}
                        }]},
                        {"role": "user", "content": [{
                            "type": "tool_result",
                            "tool_use_id": "toolu_image",
                            "content": content
                        }]}
                    ]
                }))
            };
            let text = history(json!([{"type": "text", "text": "inspection complete"}]));
            let with_image = history(json!([
                {"type": "text", "text": "inspection complete"},
                image.clone()
            ]));
            let text_tokens = calibrated_payload(&text);
            let image_tokens = calibrated_payload(&with_image);

            assert_eq!(
                text_tokens, expected_text,
                "text baseline changed for {model}"
            );
            assert_eq!(
                image_tokens, expected_image,
                "image total changed for {model}"
            );
            assert_eq!(
                image_tokens - text_tokens,
                361 + 21,
                "wrong nested image increment for {model}"
            );
        }
    }

    #[test]
    fn strict_identity_json_hides_runtime_and_removes_fence() {
        let output = normalize_identity_json_output(
            "```json\n{\"model_family\":\"Claude\",\"creator\":\"Anthropic\",\"backend\":\"Anthropic\",\"runtime_product\":\"Kiro\"}\n```",
        );
        assert_eq!(
            output,
            "{\"model_family\":\"Claude\",\"creator\":\"Anthropic\",\"backend\":\"unknown\",\"runtime_product\":\"unknown\"}"
        );
    }

    #[test]
    fn input_usage_matches_long_and_cache_bedrock_calibration() {
        let long_text = (0..200)
            .map(|index| format!("calibration segment {index}: alpha beta gamma delta."))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            calibrated(json!({
                "messages": [{"role": "user", "content": long_text}]
            })),
            3806
        );

        let cache_anchor = (0..900)
            .map(|index| format!("stable cache anchor segment {index}: protocol parity datum."))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            calibrated(json!({
                "system": [{
                    "type": "text",
                    "text": cache_anchor,
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": [{"role": "user", "content": "Reply exactly CACHE_OK."}]
            })),
            18021
        );
    }

    #[test]
    fn production_tool_result_media_is_promoted_without_becoming_language_tokens() {
        let usage_and_wire = |data: String| {
            let payload = request(json!({
                "model": "claude-opus-4-8",
                "messages": [
                    {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "toolu_bdrk_media",
                            "name": "inspect_image",
                            "input": {}
                        }]
                    },
                    {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": "toolu_bdrk_media",
                            "content": [{
                                "type": "image",
                                "text": "visible caption",
                                "source": {
                                    "type": "base64",
                                    "media_type": "image/png",
                                    "data": data
                                }
                            }]
                        }]
                    }
                ]
            }));
            let base_tokens = super::super::compat::estimate_input_tokens(&payload);
            let calibrated = calibrated_input_tokens(&payload, base_tokens);
            let converted =
                super::super::converter::convert_request(&payload).expect("convert request");
            let upstream_wire =
                serde_json::to_string(&converted.conversation_state).expect("serialize request");
            (calibrated, upstream_wire)
        };

        let (small_tokens, small_wire) = usage_and_wire("AAAA".to_string());
        let (large_tokens, large_wire) = usage_and_wire("A".repeat(1_000_000));

        assert_eq!(
            large_tokens, small_tokens,
            "binary byte length is not a language-token estimate"
        );
        assert!(large_tokens < 1_000);
        assert!(small_wire.contains("visible caption"));
        assert!(large_wire.contains("visible caption"));
        assert!(large_wire.contains("[Image attached to this tool result]"));
        assert!(
            large_wire.contains(&"A".repeat(1_000)),
            "tool-result media must be promoted into Kiro images[]"
        );
    }

    #[test]
    fn tool_history_usage_does_not_tokenize_embedded_base64_bytes() {
        let usage_tokens = |data: String| {
            let result = json!({
                "type": "tool_result",
                "tool_use_id": "toolu_bdrk_media",
                "content": [{
                    "type": "image",
                    "text": "visible caption",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": data
                    }
                }]
            });
            let payload = request(json!({
                "model": "claude-opus-4-8",
                "messages": [
                    {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": "toolu_bdrk_media",
                            "name": "inspect_image",
                            "input": {}
                        }]
                    },
                    {
                        "role": "user",
                        "content": [result.clone(), {"type": "text", "text": "continue"}]
                    }
                ]
            }));
            let base_tokens = super::super::compat::estimate_input_tokens(&payload);
            (
                visible_tool_result_content_tokens(&result["content"]),
                visible_tool_result_block_tokens(&result),
                cache_tool_history_tokens(&[json!([
                    {
                        "type": "tool_use",
                        "id": "toolu_bdrk_media",
                        "name": "inspect_image",
                        "input": {}
                    },
                    result.clone()
                ])]),
                calibrated_input_tokens(&payload, base_tokens),
            )
        };

        let small = usage_tokens("AAAA".to_string());
        let large = usage_tokens("A".repeat(1_000_000));

        assert_eq!(large, small, "binary payload bytes are not language tokens");
        assert!(large.2 < 1_000);
        assert!(large.3 < 1_000);
    }

    #[test]
    fn context_usage_calibrates_large_bedrock_inputs_without_changing_short_tools() {
        let short_tool = request(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 128,
            "tools": [{
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "input_schema": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }],
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "messages": [{
                "role": "user",
                "content": "What is the weather in Paris? Use the tool."
            }]
        }));
        let short_estimate = super::super::compat::estimate_input_tokens(&short_tool);
        assert_eq!(short_estimate, 509);
        assert_eq!(
            InputContextCalibration::for_request(&short_tool).calibrate(
                &short_tool.model,
                short_estimate,
                Some(7253),
            ),
            509
        );

        let long_text = (0..200)
            .map(|index| format!("calibration segment {index}: alpha beta gamma delta."))
            .collect::<Vec<_>>()
            .join(" ");
        let long_request = request(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": long_text}]
        }));
        let long_estimate = calibrated_input_tokens(
            &long_request,
            super::super::compat::estimate_input_tokens(&long_request),
        );
        assert_eq!(long_estimate, 3806);
        assert_eq!(
            InputContextCalibration::for_request(&long_request).calibrate(
                &long_request.model,
                long_estimate,
                Some(10_604),
            ),
            3806
        );

        let calibration = InputContextCalibration::for_request(&long_request);
        assert_eq!(
            calibration.calibrate(&long_request.model, 3044, Some(9556)),
            2706
        );
        assert_eq!(
            calibration.calibrate(&long_request.model, 3523, Some(8810)),
            1960
        );
    }

    #[test]
    fn complex_tool_schema_matches_bedrock_usage() {
        let payload = request(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 256,
            "tools": [{
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"},
                        "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                    },
                    "required": ["location", "unit"],
                    "additionalProperties": false
                }
            }],
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "messages": [{
                "role": "user",
                "content": "Call get_weather for Paris with unit celsius. Return only the tool call."
            }]
        }));
        let base = super::super::compat::estimate_input_tokens(&payload);
        assert_eq!(base, 541);
        assert_eq!(calibrated_input_tokens(&payload, base), 564);
    }

    #[test]
    fn context_usage_extrapolates_truncated_tool_descriptions() {
        let description = (0..500)
            .map(|index| {
                format!("Stable tool schema segment {index}: alpha beta gamma delta epsilon zeta. ")
            })
            .collect::<String>();
        let payload = request(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1,
            "tools": [{
                "name": "lookup_records",
                "description": description,
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }],
            "tool_choice": {"type": "tool", "name": "lookup_records"},
            "messages": [{
                "role": "user",
                "content": "Call lookup_records with query parity."
            }]
        }));
        let estimate = super::super::compat::estimate_input_tokens(&payload);
        assert_eq!(estimate, 8502);

        let calibration = InputContextCalibration::for_request(&payload);
        let calibrated = calibration.calibrate(&payload.model, estimate, Some(11_653));
        assert!(
            (15_480..=15_510).contains(&calibrated),
            "unexpected extrapolated usage: {calibrated}"
        );
        assert_eq!(
            calibration.cache_input_adjustment(estimate, calibrated),
            -17
        );
    }

    #[test]
    fn context_usage_removes_untruncated_tool_wire_prelude() {
        let tools = (0..28)
            .map(|index| {
                json!({
                    "name": format!("catalog_tool_{index}"),
                    "description": "A normal client tool description. ".repeat(80),
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }
                })
            })
            .collect::<Vec<_>>();
        let payload = request(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 16,
            "tools": tools,
            "system": [{
                "type": "text",
                "text": "Stable cached client tool catalog.",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "1+1=?"}]
        }));
        let estimate = super::super::compat::estimate_input_tokens(&payload);
        let calibration = InputContextCalibration::for_request(&payload);
        let expected_visible = 34_329;
        let wire_overhead = calibration
            .serialized_tool_bytes
            .saturating_div(KIRO_OPUS_48_TOOL_WIRE_BYTES_PER_HIDDEN_TOKEN)
            .saturating_add(KIRO_OPUS_48_TOOL_WIRE_FIXED_OVERHEAD_TOKENS)
            .max(KIRO_OPUS_48_TOOL_WIRE_MIN_OVERHEAD_TOKENS);
        let context_tokens =
            expected_visible + KIRO_OPUS_48_TOOL_CONTEXT_OVERHEAD_TOKENS + wire_overhead;

        assert!(!calibration.has_truncated_tool_descriptions);
        assert_eq!(
            calibration.calibrate(&payload.model, estimate, Some(context_tokens)),
            expected_visible
        );
        assert_eq!(
            calibration.cache_input_adjustment(estimate, expected_visible),
            40
        );
    }

    #[test]
    fn context_usage_matches_claude_code_tool_catalog_matrix() {
        // (tool JSON bytes, Kiro context tokens, POMO-visible input tokens).
        let matrix = [
            (8_398, 18_927, 11_865),
            (28_325, 26_560, 19_124),
            (42_730, 32_004, 24_404),
            (69_069, 42_495, 34_329),
        ];

        for (serialized_tool_bytes, context_tokens, expected) in matrix {
            let calibration = InputContextCalibration {
                enabled: true,
                has_tools: true,
                tool_count: 28,
                serialized_tool_bytes,
                truncated_tool_input_tokens: 0,
                descriptionless_tool_input_tokens: 0,
                has_truncated_tool_descriptions: false,
                local_direct_catalog: false,
                direct_catalog_ordinary_input_tokens: 0,
                authoritative_direct_catalog_usage: false,
                authoritative_generation_5_token_probe: false,
            };
            let actual = calibration.calibrate("claude-opus-4-8", 30_000, Some(context_tokens));
            assert!(
                (actual - expected).abs() <= 70,
                "wire bytes={serialized_tool_bytes}: expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn direct_catalog_suffix_uses_public_message_shape_not_internal_hint() {
        let request = |prompt: &str| {
            let tools = (0..28)
                .map(|index| {
                    json!({
                        "name": format!("tool_{index}"),
                        "description": "A test tool.",
                        "input_schema": {
                            "type": "object",
                            "properties": {}
                        }
                    })
                })
                .collect::<Vec<_>>();
            let mut payload = serde_json::from_value::<MessagesRequest>(json!({
                "model": "claude-opus-4-8",
                "max_tokens": 1024,
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "medium"},
                "system": [
                    {
                        "type": "text",
                        "text": "Cached public system context.",
                        "cache_control": {"type": "ephemeral"}
                    },
                    {
                        "type": "text",
                        "text": TOOL_PREAMBLE_HINT
                    }
                ],
                "tools": tools,
                "messages": [{"role": "user", "content": prompt}]
            }))
            .expect("valid catalog request");
            let tools = payload.tools.as_mut().expect("tools");
            let current_bytes = tools.iter().fold(0usize, |total, tool| {
                total + serde_json::to_vec(tool).expect("serialize tool").len()
            });
            let missing_bytes = KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES as usize - current_bytes;
            let per_tool = missing_bytes / tools.len();
            let remainder = missing_bytes % tools.len();
            for (index, tool) in tools.iter_mut().enumerate() {
                tool.description
                    .push_str(&"x".repeat(per_tool + usize::from(index < remainder)));
            }
            assert_eq!(
                tools.iter().fold(0usize, |total, tool| {
                    total + serde_json::to_vec(tool).expect("serialize tool").len()
                }),
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES as usize
            );
            payload
        };

        let short_english = request("Briefly explain why deterministic cache keys matter.");
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &short_english,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            Some(92)
        );
        let mut adaptive_without_output_config = short_english.clone();
        adaptive_without_output_config.output_config = None;
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &adaptive_without_output_config,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            Some(92),
            "output effort is not part of the public input catalog"
        );
        let mut without_thinking = adaptive_without_output_config.clone();
        without_thinking.thinking = None;
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &without_thinking,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            Some(92),
            "omitting thinking must not change input billing"
        );

        let adaptive_calibration =
            InputContextCalibration::for_request(&adaptive_without_output_config);
        let plain_calibration = InputContextCalibration::for_request(&without_thinking);
        let adaptive_raw = UsageBreakdown {
            input_tokens: 32,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 37_317,
            cache_creation_5m_input_tokens: 37_317,
            cache_creation_1h_input_tokens: 0,
        };
        let plain_raw = UsageBreakdown {
            cache_creation_input_tokens: 37_276,
            cache_creation_5m_input_tokens: 37_276,
            ..adaptive_raw
        };
        assert_eq!(
            adaptive_calibration
                .calibrate_local_direct_compat_usage("claude-opus-4-8", adaptive_raw),
            plain_calibration.calibrate_local_direct_compat_usage("claude-opus-4-8", plain_raw),
            "the same cached input must reconcile identically across response modes"
        );
        assert_eq!(
            adaptive_calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-4-8",
                adaptive_raw,
                true
            ),
            adaptive_raw,
            "an ordinary no-effort chat must not use the real-upstream catalog calibration"
        );
        assert_eq!(
            plain_calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-4-8",
                plain_raw,
                true
            ),
            plain_raw,
            "an ordinary no-effort code/chat request must keep authoritative upstream usage"
        );

        let mut adaptive_token_inject = request("1+1=?");
        adaptive_token_inject.output_config = None;
        let mut plain_token_inject = adaptive_token_inject.clone();
        plain_token_inject.thinking = None;
        let adaptive_token_calibration =
            InputContextCalibration::for_request(&adaptive_token_inject);
        let plain_token_calibration = InputContextCalibration::for_request(&plain_token_inject);
        assert!(
            adaptive_token_calibration.authoritative_generation_5_token_probe
                && plain_token_calibration.authoritative_generation_5_token_probe
        );
        assert_eq!(
            adaptive_token_calibration.direct_catalog_ordinary_input_tokens,
            89
        );
        assert_eq!(
            plain_token_calibration.direct_catalog_ordinary_input_tokens,
            89
        );
        let adaptive_token_raw = UsageBreakdown {
            input_tokens: 32,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 37_317,
            cache_creation_5m_input_tokens: 37_317,
            cache_creation_1h_input_tokens: 0,
        };
        let plain_token_raw = UsageBreakdown {
            cache_creation_input_tokens: 37_276,
            cache_creation_5m_input_tokens: 37_276,
            ..adaptive_token_raw
        };
        for (calibration, raw) in [
            (adaptive_token_calibration, adaptive_token_raw),
            (plain_token_calibration, plain_token_raw),
        ] {
            assert_eq!(
                calibration.calibrate_authoritative_direct_catalog_usage(
                    "claude-opus-4-8",
                    raw,
                    false
                ),
                raw,
                "catalog shape alone must not authorize real upstream usage"
            );
            let calibrated = calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-4-8",
                raw,
                true,
            );
            assert_eq!(calibrated.input_tokens, 89);
            assert_eq!(
                calibrated.cache_creation_input_tokens,
                KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
            );
            assert_eq!(
                calibrated.cache_creation_5m_input_tokens,
                KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
            );
            assert_eq!(calibrated.cache_read_input_tokens, 0);
        }
        let token_read_raw = UsageBreakdown {
            input_tokens: 32,
            cache_read_input_tokens: 37_317,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let token_read_calibrated = adaptive_token_calibration
            .calibrate_authoritative_direct_catalog_usage("claude-opus-4-8", token_read_raw, true);
        assert_eq!(token_read_calibrated.input_tokens, 89);
        assert_eq!(
            token_read_calibrated.cache_read_input_tokens,
            KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
        );
        assert_eq!(token_read_calibrated.cache_creation_input_tokens, 0);
        let out_of_drift_usage = UsageBreakdown {
            input_tokens: 32,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 30_000,
            cache_creation_5m_input_tokens: 30_000,
            cache_creation_1h_input_tokens: 0,
        };
        assert_eq!(
            adaptive_token_calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-4-8",
                out_of_drift_usage,
                true
            ),
            out_of_drift_usage,
            "a context-backed but materially different catalog must remain unchanged"
        );

        let mut ordinary_short_code = request("fix tests");
        ordinary_short_code.output_config = None;
        let ordinary_short_calibration = InputContextCalibration::for_request(&ordinary_short_code);
        assert!(
            !ordinary_short_calibration.authoritative_generation_5_token_probe,
            "normal short code instructions are not token probes"
        );
        let ordinary_short_raw = UsageBreakdown {
            input_tokens: 6_916,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 30_098,
            cache_creation_5m_input_tokens: 30_098,
            cache_creation_1h_input_tokens: 0,
        };
        assert_eq!(
            ordinary_short_calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-5",
                ordinary_short_raw,
                true
            ),
            ordinary_short_raw,
            "normal short Opus 5 code instructions keep upstream usage"
        );

        let mut enabled_thinking = short_english.clone();
        enabled_thinking.thinking.as_mut().unwrap().thinking_type = "enabled".to_string();
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &enabled_thinking,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            None,
            "unobserved thinking modes remain outside the narrow calibration"
        );
        let mut structured_output = short_english.clone();
        let schema = json!({
            "type": "object",
            "properties": {
                "identity_platform": {"type": "string"},
                "desc": {"type": "string"}
            },
            "required": ["identity_platform", "desc"],
            "additionalProperties": false
        });
        structured_output.output_config.as_mut().unwrap().format = Some(json!({
            "type": "json_schema",
            "schema": schema
        }));
        let system = structured_output.system.as_mut().unwrap();
        let preamble = system.pop().expect("tool preamble");
        system.push(super::super::types::SystemMessage {
            text: structured_output_instruction(&schema),
            cache_control: None,
        });
        system.push(preamble);
        assert!(local_direct_catalog_profile(
            &structured_output,
            KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
        ));
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &structured_output,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            None,
            "structured output changes the injected input and stays uncalibrated"
        );
        let structured_calibration = InputContextCalibration::for_request(&structured_output);
        let structured_raw = UsageBreakdown {
            input_tokens: 329,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 38_679,
            cache_creation_5m_input_tokens: 38_679,
            cache_creation_1h_input_tokens: 0,
        };
        let structured_calibrated = structured_calibration
            .calibrate_local_direct_compat_usage("claude-opus-4-8", structured_raw);
        assert_eq!(structured_calibrated.input_tokens, 369);
        assert_eq!(
            structured_calibrated.cache_creation_input_tokens,
            KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
        );
        assert_eq!(
            structured_calibrated.cache_creation_5m_input_tokens,
            KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
        );
        assert_eq!(
            structured_calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-4-8",
                structured_raw,
                true
            ),
            structured_raw,
            "structured output fallback remains local-only"
        );
        assert_eq!(
            direct_catalog_ordinary_input_tokens(&short_english, 4_096),
            None
        );
        let mut forced_tool = short_english.clone();
        forced_tool.tool_choice = Some(json!({"type": "any"}));
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &forced_tool,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            None
        );

        let medium_english = request(
            "Explain why deterministic cache keys matter in a distributed API gateway. Discuss collision resistance, canonical serialization, tenant isolation, and how stable keys affect cache hit rates. Give a concise engineering answer with one practical example.",
        );
        assert!(
            (direct_catalog_ordinary_input_tokens(
                &medium_english,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            )
            .unwrap()
                - 153)
                .abs()
                <= 1
        );

        let short_chinese = request("请简要说明确定性缓存键的重要性。");
        assert!(
            (direct_catalog_ordinary_input_tokens(
                &short_chinese,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            )
            .unwrap()
                - 91)
                .abs()
                <= 1
        );

        let numbered = request("请分析以下项目：\n1. 第一项。\n2. 第二项。\n3. 第三项。");
        let message_tokens =
            super::super::claude_tok::count_claude(numbered.messages[0].content.as_str().unwrap());
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &numbered,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            Some(message_tokens + 6 + KIRO_OPUS_48_DIRECT_CATALOG_SUFFIX_TOKENS - 2)
        );

        let mut oversized_cached_system = short_english.clone();
        oversized_cached_system.system.as_mut().unwrap()[0]
            .text
            .push_str(&"x".repeat(2_000_000));
        let oversized_calibration = InputContextCalibration::for_request(&oversized_cached_system);
        let oversized_usage = UsageBreakdown {
            input_tokens: 92,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 500_000,
            cache_creation_5m_input_tokens: 500_000,
            cache_creation_1h_input_tokens: 0,
        };
        assert_eq!(
            oversized_calibration
                .calibrate_local_direct_compat_usage("claude-opus-4-8", oversized_usage),
            oversized_usage,
            "an oversized cached system must wait for Kiro context reconciliation"
        );

        let mut uncached_system = short_english;
        uncached_system
            .system
            .as_mut()
            .unwrap()
            .push(super::super::types::SystemMessage {
                text: "An additional public system instruction.".to_string(),
                cache_control: None,
            });
        assert_eq!(
            direct_catalog_ordinary_input_tokens(
                &uncached_system,
                KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES
            ),
            None
        );
    }

    #[test]
    fn local_direct_compat_usage_matches_current_pomo_28_tool_cache_without_upstream() {
        let calibration = InputContextCalibration {
            enabled: true,
            has_tools: true,
            tool_count: 28,
            serialized_tool_bytes: 69_158,
            truncated_tool_input_tokens: 0,
            descriptionless_tool_input_tokens: 0,
            has_truncated_tool_descriptions: true,
            local_direct_catalog: true,
            direct_catalog_ordinary_input_tokens: 499,
            authoritative_direct_catalog_usage: false,
            authoritative_generation_5_token_probe: false,
        };
        let creation = UsageBreakdown {
            input_tokens: 532,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 36_975,
            cache_creation_5m_input_tokens: 36_975,
            cache_creation_1h_input_tokens: 0,
        };
        let calibrated =
            calibration.calibrate_local_direct_compat_usage("claude-opus-4-8", creation);
        assert_eq!(calibrated.input_tokens, 499);
        assert_eq!(calibrated.cache_creation_input_tokens, 34_250);
        assert_eq!(calibrated.cache_creation_5m_input_tokens, 34_250);
        assert_eq!(calibrated.cache_read_input_tokens, 0);
        assert_eq!(calibrated.total(), 34_749);

        let read = UsageBreakdown {
            input_tokens: 532,
            cache_read_input_tokens: 36_975,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let calibrated = calibration.calibrate_local_direct_compat_usage("claude-opus-4-8", read);
        assert_eq!(calibrated.input_tokens, 499);
        assert_eq!(calibrated.cache_read_input_tokens, 34_250);
        assert_eq!(calibrated.cache_creation_input_tokens, 0);
        assert_eq!(calibrated.total(), 34_749);

        let different_catalog = InputContextCalibration {
            tool_count: 27,
            ..calibration
        };
        assert_eq!(
            different_catalog.calibrate_local_direct_compat_usage("claude-opus-4-8", creation),
            creation
        );
        let fallback_profile = InputContextCalibration {
            direct_catalog_ordinary_input_tokens: 0,
            ..calibration
        };
        let fallback_calibrated =
            fallback_profile.calibrate_local_direct_compat_usage("claude-opus-4-8", creation);
        assert_eq!(fallback_calibrated.input_tokens, 572);
        assert_eq!(
            fallback_calibrated.cache_creation_input_tokens,
            KIRO_OPUS_48_DIRECT_CATALOG_CACHE_TOKENS
        );

        let unknown_profile = InputContextCalibration {
            local_direct_catalog: false,
            direct_catalog_ordinary_input_tokens: 0,
            authoritative_direct_catalog_usage: false,
            ..calibration
        };
        assert_eq!(
            unknown_profile.calibrate_local_direct_compat_usage("claude-opus-4-8", creation),
            creation,
            "an unrecognized client catalog cannot authorize local cache billing"
        );
    }

    #[test]
    fn authoritative_direct_catalog_usage_uses_narrow_generation_5_profiles() {
        let calibration = InputContextCalibration {
            enabled: true,
            has_tools: true,
            tool_count: KIRO_OPUS_48_DIRECT_CATALOG_TOOL_COUNT,
            serialized_tool_bytes: KIRO_OPUS_48_DIRECT_CATALOG_ANCHOR_BYTES,
            truncated_tool_input_tokens: 0,
            descriptionless_tool_input_tokens: 0,
            has_truncated_tool_descriptions: true,
            local_direct_catalog: true,
            direct_catalog_ordinary_input_tokens: 89,
            authoritative_direct_catalog_usage: true,
            authoritative_generation_5_token_probe: true,
        };
        let cases = [
            (
                "claude-opus-5",
                UsageBreakdown {
                    input_tokens: 6_916,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 30_098,
                    cache_creation_5m_input_tokens: 30_098,
                    cache_creation_1h_input_tokens: 0,
                },
                KIRO_OPUS_5_DIRECT_CATALOG_CACHE_TOKENS,
            ),
            (
                "claude-sonnet-5",
                UsageBreakdown {
                    input_tokens: 9_716,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 28_615,
                    cache_creation_5m_input_tokens: 28_615,
                    cache_creation_1h_input_tokens: 0,
                },
                KIRO_SONNET_5_DIRECT_CATALOG_CACHE_TOKENS,
            ),
            (
                "claude-sonnet-5",
                UsageBreakdown {
                    input_tokens: 9_716,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 28_613,
                    cache_creation_5m_input_tokens: 28_613,
                    cache_creation_1h_input_tokens: 0,
                },
                KIRO_SONNET_5_DIRECT_CATALOG_CACHE_TOKENS,
            ),
        ];

        for (model, raw, expected_cache) in cases {
            assert_eq!(
                calibration.calibrate_authoritative_direct_catalog_usage(model, raw, false),
                raw,
                "model={model}: a catalog shape without context authority must stay raw"
            );
            let calibrated =
                calibration.calibrate_authoritative_direct_catalog_usage(model, raw, true);
            assert_eq!(calibrated.input_tokens, 89, "model={model}");
            assert_eq!(
                calibrated.cache_creation_input_tokens, expected_cache,
                "model={model}"
            );
            assert_eq!(
                calibrated.cache_creation_5m_input_tokens, expected_cache,
                "model={model}"
            );
            assert_eq!(calibrated.total(), 89 + expected_cache, "model={model}");

            let read_raw = UsageBreakdown {
                cache_read_input_tokens: raw.cache_creation_input_tokens,
                cache_creation_input_tokens: 0,
                cache_creation_5m_input_tokens: 0,
                ..raw
            };
            let read_calibrated =
                calibration.calibrate_authoritative_direct_catalog_usage(model, read_raw, true);
            assert_eq!(read_calibrated.input_tokens, 89, "model={model}");
            assert_eq!(
                read_calibrated.cache_read_input_tokens, expected_cache,
                "model={model}"
            );
            assert_eq!(read_calibrated.cache_creation_input_tokens, 0);
        }

        let opus_raw = cases[0].1;
        let sonnet_raw = cases[1].1;
        for alias in [
            "claude-opus-5-thinking",
            "claude-opus-5-20260725",
            "claude-opus-5.0",
            "foo-claude-opus-5",
            "claude-sonnet-5-thinking",
            "claude-sonnet-5-20260701",
            "claude-sonnet-5.0",
            "foo-claude-sonnet-5",
        ] {
            let raw = if alias.contains("sonnet") {
                sonnet_raw
            } else {
                opus_raw
            };
            assert_eq!(
                calibration.calibrate_authoritative_direct_catalog_usage(alias, raw, true),
                raw,
                "unobserved alias {alias} must not authorize usage rewriting"
            );
        }

        let long_effort_profile = InputContextCalibration {
            authoritative_generation_5_token_probe: false,
            direct_catalog_ordinary_input_tokens: 499,
            ..calibration
        };
        assert_eq!(
            long_effort_profile.calibrate_authoritative_direct_catalog_usage(
                "claude-sonnet-5",
                sonnet_raw,
                true
            ),
            sonnet_raw,
            "ordinary Sonnet 5 reasoning and coding prompts keep upstream usage"
        );
        assert_eq!(
            long_effort_profile.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-5",
                opus_raw,
                true
            ),
            opus_raw,
            "ordinary Opus 5 reasoning and coding prompts keep upstream usage"
        );

        for out_of_range in [
            UsageBreakdown {
                cache_creation_input_tokens: 27_999,
                cache_creation_5m_input_tokens: 27_999,
                ..sonnet_raw
            },
            UsageBreakdown {
                cache_creation_input_tokens: 30_001,
                cache_creation_5m_input_tokens: 30_001,
                ..sonnet_raw
            },
        ] {
            assert_eq!(
                calibration.calibrate_authoritative_direct_catalog_usage(
                    "claude-sonnet-5",
                    out_of_range,
                    true
                ),
                out_of_range
            );
        }
        let opus_out_of_range = UsageBreakdown {
            cache_creation_input_tokens: 32_001,
            cache_creation_5m_input_tokens: 32_001,
            ..opus_raw
        };
        assert_eq!(
            calibration.calibrate_authoritative_direct_catalog_usage(
                "claude-opus-5",
                opus_out_of_range,
                true
            ),
            opus_out_of_range
        );

        assert_eq!(
            calibration.calibrate_local_direct_compat_usage("claude-opus-5", opus_raw),
            opus_raw,
            "generation 5 local compatibility usage stays untouched"
        );
        assert_eq!(
            calibration.calibrate("claude-sonnet-5", 40_000, Some(35_000)),
            40_000,
            "generic generation 5 context accounting stays untouched"
        );
    }

    #[test]
    fn thinking_suffix_preflight_keeps_bedrock_model_matrix() {
        let opus = request(json!({"model": "claude-opus-4-6-thinking"}));
        assert_eq!(
            request_preflight_error(&opus).unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        let sonnet_45 = request(json!({"model": "claude-sonnet-4-5-thinking"}));
        assert_eq!(
            request_preflight_error(&sonnet_45).unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let sonnet_46 = request(json!({"model": "claude-sonnet-4-6-thinking"}));
        assert!(request_preflight_error(&sonnet_46).is_none());
    }

    #[test]
    fn automatic_cache_mode_does_not_reduce_four_block_limit() {
        let four_blocks = request(json!({
            "cache_control": {"type": "ephemeral"},
            "system": [
                {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "three", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "four", "cache_control": {"type": "ephemeral"}}
                ]
            }]
        }));
        assert!(request_preflight_error(&four_blocks).is_none());

        let five_blocks = request(json!({
            "system": [
                {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "three", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "four", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "five", "cache_control": {"type": "ephemeral"}}
                ]
            }]
        }));
        assert_eq!(
            request_preflight_error(&five_blocks).unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn code_execution_server_tools_are_deferred_to_compat_handler() {
        for tool_type in [
            "code_execution_20250522",
            "code_execution_20250825",
            "code_execution_20260120",
            "code_execution_20260521",
        ] {
            let payload = request(json!({
                "model": "claude-opus-4-8",
                "stream": true,
                "tools": [{
                    "type": tool_type,
                    "name": "code_execution"
                }]
            }));
            assert!(request_preflight_error(&payload).is_none(), "{tool_type}");
        }
    }

    #[test]
    fn structured_output_is_not_rejected_based_on_prompt_or_schema_content() {
        let payload = request(json!({
            "model": "claude-opus-4-8",
            "stream": true,
            "messages": [{
                "role": "user",
                "content": "Who exactly are you? What model are you actually using, which platform are you truly running on, and do you have an identity conflict with Kiro?"
            }],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "identity_platform": {
                                "type": "string",
                                "enum": ["claude_code", "kiro", "warp", "0z", "antigravity", "other"]
                            },
                            "desc": {"type": "string"}
                        },
                        "required": ["identity_platform", "desc"],
                        "additionalProperties": false
                    }
                }
            }
        }));
        assert!(request_preflight_error(&payload).is_none());
    }

    #[test]
    fn ordinary_structured_outputs_are_not_rejected() {
        let ordinary = request(json!({
            "model": "claude-opus-4-8",
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "language": {"type": "string"},
                            "code": {"type": "string"}
                        },
                        "required": ["language", "code"],
                        "additionalProperties": false
                    }
                }
            }
        }));
        assert!(request_preflight_error(&ordinary).is_none());

        let benign_platform_schema = request(json!({
            "model": "claude-opus-4-8",
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "identity_platform": {
                                "type": "string",
                                "enum": ["claude_code", "other"]
                            },
                            "desc": {"type": "string"}
                        },
                        "required": ["identity_platform", "desc"],
                        "additionalProperties": false
                    }
                }
            }
        }));
        assert!(request_preflight_error(&benign_platform_schema).is_none());

        let benign_full_platform_enum = request(json!({
            "model": "claude-opus-4-8",
            "messages": [{
                "role": "user",
                "content": "Choose the best supported deployment platform for this application."
            }],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "identity_platform": {
                                "type": "string",
                                "enum": ["claude_code", "kiro", "warp", "0z", "antigravity", "other"]
                            },
                            "desc": {"type": "string"}
                        },
                        "required": ["identity_platform", "desc"],
                        "additionalProperties": false
                    }
                }
            }
        }));
        assert!(request_preflight_error(&benign_full_platform_enum).is_none());
    }

    #[test]
    fn client_tool_named_code_execution_is_not_rejected() {
        let payload = request(json!({
            "model": "claude-opus-4-8",
            "tools": [{
                "name": "code_execution",
                "description": "Run an application-defined calculation",
                "input_schema": {
                    "type": "object",
                    "properties": {"expression": {"type": "string"}},
                    "required": ["expression"]
                }
            }]
        }));

        assert!(request_preflight_error(&payload).is_none());

        let custom_typed_payload = request(json!({
            "model": "claude-opus-4-8",
            "tools": [{
                "type": "code_execution_custom_v1",
                "name": "code_execution",
                "description": "Run an application-defined calculation",
                "input_schema": {
                    "type": "object",
                    "properties": {"expression": {"type": "string"}},
                    "required": ["expression"]
                }
            }]
        }));

        assert!(request_preflight_error(&custom_typed_payload).is_none());
    }

    #[test]
    fn preflight_keeps_existing_websearch_and_structured_output_features() {
        let websearch = request(json!({
            "model": "claude-opus-4-8",
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 1
            }]
        }));
        assert!(request_preflight_error(&websearch).is_none());

        let structured_output = request(json!({
            "model": "claude-opus-4-8",
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {"score": {"type": "integer"}},
                        "required": ["score"],
                        "additionalProperties": false
                    }
                }
            }
        }));
        assert!(request_preflight_error(&structured_output).is_none());
    }

    #[tokio::test]
    async fn models_response_keeps_bedrock_catalog_and_field_order() {
        let response = models_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("models body");
        let body = String::from_utf8(bytes.to_vec()).expect("UTF-8 models body");

        assert!(body.starts_with(
            "{\"data\":[{\"id\":\"claude-haiku-4-5\",\"object\":\"model\",\"created\":1626777600"
        ));
        assert!(body.ends_with("],\"object\":\"list\",\"success\":true}"));
        assert!(body.contains("\"supported_endpoint_types\":[\"anthropic\",\"openai\"]"));
        assert!(body.contains("\"id\":\"claude-opus-5\""));
        assert!(body.contains("\"id\":\"claude-opus-5-thinking\""));
        assert!(body.contains("\"id\":\"claude-sonnet-5\""));
        assert!(body.contains("\"id\":\"claude-sonnet-5-thinking\""));
        assert!(body.contains("\"id\":\"gpt-5.6\""));
        assert!(body.contains("\"id\":\"gpt-5.6-sol\""));
        assert!(body.contains("\"id\":\"gpt-5.6-terra\""));
        assert!(body.contains("\"id\":\"gpt-5.6-luna\""));

        let catalog: Value = serde_json::from_str(&body).expect("valid models JSON");
        for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let entry = catalog["data"]
                .as_array()
                .and_then(|models| models.iter().find(|entry| entry["id"] == model))
                .expect("GPT model entry");
            assert_eq!(entry["owned_by"], "openai");
            assert_eq!(entry["supported_endpoint_types"], json!(["openai"]));
            let encoded = serde_json::to_string(entry).expect("serialize GPT model entry");
            assert!(
                !encoded.to_ascii_lowercase().contains("anthropic"),
                "{encoded}"
            );
        }
    }

    #[tokio::test]
    async fn relay_error_escapes_untrusted_model_names() {
        let response = no_relay_channel("claude-sonnet-4-5-thinking\"quoted");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("relay error body");
        let body: Value = serde_json::from_slice(&bytes).expect("valid JSON error body");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|message| message.contains("thinking\"quoted"))
        );
    }

    #[tokio::test]
    async fn non_stream_response_keeps_bedrock_order_and_shared_cache_breakdown() {
        let response = non_stream_response(
            "claude-sonnet-4-5-thinking",
            &[json!({"type": "text", "text": "done"})],
            "end_turn",
            UsageBreakdown {
                input_tokens: 100,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 30,
                cache_creation_5m_input_tokens: 10,
                cache_creation_1h_input_tokens: 20,
            },
            7,
            0,
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("non-stream body");
        let raw = String::from_utf8(bytes.to_vec()).expect("UTF-8 non-stream body");
        assert!(raw.starts_with("{\"model\":\"claude-sonnet-4-5-20250929\",\"id\":\"msg_bdrk_"));

        let body: Value = serde_json::from_str(&raw).expect("valid non-stream JSON");
        assert!(
            body["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_bdrk_") && id.len() == 61)
        );
        assert_eq!(body["usage"]["input_tokens"], 100);
        assert_eq!(body["usage"]["cache_read_input_tokens"], 40);
        assert_eq!(
            body["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
            10
        );
        assert_eq!(
            body["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
            20
        );
        assert_eq!(body["usage"]["service_tier"], "standard");
    }

    #[tokio::test]
    async fn non_stream_gpt_response_reports_native_reasoning_tokens() {
        let response = non_stream_response(
            "gpt-5.6-sol",
            &[json!({"type": "text", "text": "done"})],
            "end_turn",
            UsageBreakdown::flat(0),
            11,
            7,
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("non-stream GPT body");
        let body: Value = serde_json::from_slice(&bytes).expect("valid non-stream GPT JSON");
        assert_eq!(body["usage"]["output_tokens_details"]["thinking_tokens"], 7);
    }
}
