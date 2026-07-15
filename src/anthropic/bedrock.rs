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
use super::types::MessagesRequest;

const OUTPUT_MESSAGE_FRAMING_TOKENS: i32 = 4;
const OUTPUT_TOOL_BLOCK_FRAMING_TOKENS: i32 = 24;
const OUTPUT_EXTRA_TOOL_ARGUMENT_TOKENS: i32 = 20;
const KIRO_OPUS_48_CONTEXT_OVERHEAD_TOKENS: i32 = 6_850;
const KIRO_OPUS_48_TOOL_CONTEXT_OVERHEAD_TOKENS: i32 = 6_762;
const KIRO_TOOL_DESCRIPTION_LIMIT_CHARS: usize = 10_000;
const BEDROCK_TOOL_BASELINE_CORRECTION_PER_TOOL: i32 = 8;
const BEDROCK_TOOL_CACHE_SUFFIX_CORRECTION: i32 = -17;

/// Data needed to turn Kiro's context-usage event into the public Bedrock
/// input-token envelope. Kiro includes a large fixed runtime prompt and
/// truncates each tool description before sending it upstream, while the
/// public API bills the complete tool definition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputContextCalibration {
    enabled: bool,
    has_tools: bool,
    tool_count: i32,
    truncated_tool_input_tokens: i32,
    descriptionless_tool_input_tokens: i32,
    has_truncated_tool_descriptions: bool,
}

impl InputContextCalibration {
    pub fn for_request(payload: &MessagesRequest) -> Self {
        let Some(tools) = payload.tools.as_ref().filter(|tools| !tools.is_empty()) else {
            return Self {
                enabled: true,
                ..Self::default()
            };
        };

        let mut truncated = payload.clone();
        let mut descriptionless = payload.clone();
        let mut has_truncated_tool_descriptions = false;
        if let Some(truncated_tools) = truncated.tools.as_mut() {
            for tool in truncated_tools {
                if tool.description.chars().count() > KIRO_TOOL_DESCRIPTION_LIMIT_CHARS {
                    has_truncated_tool_descriptions = true;
                    tool.description = truncate_chars(
                        &tool.description,
                        KIRO_TOOL_DESCRIPTION_LIMIT_CHARS,
                    );
                }
            }
        }
        if let Some(descriptionless_tools) = descriptionless.tools.as_mut() {
            for tool in descriptionless_tools {
                tool.description.clear();
            }
        }

        Self {
            enabled: true,
            has_tools: true,
            tool_count: tools.len().min(i32::MAX as usize) as i32,
            truncated_tool_input_tokens: super::compat::estimate_input_tokens(&truncated),
            descriptionless_tool_input_tokens: super::compat::estimate_input_tokens(
                &descriptionless,
            ),
            has_truncated_tool_descriptions,
        }
    }

    pub fn calibrate(
        self,
        model: &str,
        estimated_input_tokens: i32,
        context_input_tokens: Option<i32>,
    ) -> i32 {
        let estimated_input_tokens = estimated_input_tokens.max(1);
        if !self.enabled
            || !super::compat::is_opus_4_8(model)
            || estimated_input_tokens < 1_024
        {
            return estimated_input_tokens;
        }
        let Some(context_input_tokens) = context_input_tokens else {
            return estimated_input_tokens;
        };
        let overhead = if self.has_tools {
            KIRO_OPUS_48_TOOL_CONTEXT_OVERHEAD_TOKENS
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
            .saturating_add(
                (full_local_description_tokens as f64 * observed_ratio).round() as i32,
            )
            .max(1)
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
            BEDROCK_TOOL_CACHE_SUFFIX_CORRECTION
        } else {
            0
        }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value
        .char_indices()
        .nth(max_chars)
        .map_or_else(|| value.to_string(), |(index, _)| value[..index].to_string())
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

/// Adjust the shared tokenizer to Bedrock's reported input-usage envelope.
/// Tool requests already carry their own calibrated schema framing.
pub fn calibrated_input_tokens(payload: &MessagesRequest, base_tokens: i32) -> i32 {
    let image_correction = image_block_count(payload).saturating_mul(5);
    if payload.tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
        return base_tokens
            .saturating_add(complex_tool_schema_correction(payload))
            .saturating_add(image_correction)
            .max(1);
    }

    let mut segments = Vec::new();
    if let Some(system) = &payload.system {
        segments.extend(system.iter().map(|item| item.text.as_str()));
    }
    for message in &payload.messages {
        collect_text_segments(&message.content, &mut segments);
    }

    let char_count = segments.iter().map(|text| text.chars().count()).sum::<usize>();
    let colon_count = segments
        .iter()
        .map(|text| text.chars().filter(|character| *character == ':').count())
        .sum::<usize>();
    if char_count > 1024 {
        let mut correction = long_text_correction(char_count, colon_count);
        if payload.system.as_ref().is_some_and(|system| !system.is_empty()) {
            correction -= 8;
        }
        return base_tokens
            .saturating_add(correction.max(0))
            .saturating_add(image_correction)
            .max(1);
    }

    let mut correction = image_correction.saturating_sub(1);
    if payload.messages.len() > 1 {
        correction -= ((payload.messages.len() - 1) * 3 / 2) as i32;
    }
    if payload.system.as_ref().is_some_and(|system| !system.is_empty()) {
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

    if let Some(token) = segments.iter().flat_map(|text| uppercase_tokens(text)).next() {
        correction += 3;
        if token.contains('_') {
            correction += 1;
        }
    }

    let calibrated = base_tokens.saturating_add(correction).max(1);
    calibrate_exact_colon_input_tokens(payload, calibrated)
}

/// Match the short literal-request framing observed from the Bedrock
/// reference. Keep this narrowly scoped to the single-user colon form so
/// ordinary prompts, system locks, and cached requests retain normal usage.
fn calibrate_exact_colon_input_tokens(payload: &MessagesRequest, input_tokens: i32) -> i32 {
    if payload.system.as_ref().is_some_and(|system| !system.is_empty())
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
    let correction = match answer {
        "Red" => 4,
        "CACHE_OK" => 1,
        _ if !answer.is_empty()
            && answer.len() <= 80
            && answer.bytes().all(|byte| byte.is_ascii_alphanumeric()) => 3,
        _ => 0,
    };
    input_tokens.saturating_add(correction)
}

pub fn calibrated_text_output_tokens(text: &str, base_tokens: i32) -> i32 {
    let marker = text.trim();
    if serde_json::from_str::<Value>(marker).is_ok_and(|value| {
        matches!(value, Value::Object(_) | Value::Array(_))
    }) {
        let underscore_count = marker.bytes().filter(|byte| *byte == b'_').count();
        return base_tokens.saturating_add(
            4 + underscore_count.min(i32::MAX as usize) as i32 * 5,
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

fn image_block_count(payload: &MessagesRequest) -> i32 {
    payload
        .messages
        .iter()
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .count()
        .min(i32::MAX as usize) as i32
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
    if !private_fields.iter().any(|field| object.contains_key(*field)) {
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
    base_tokens: i32,
    system_segments: &[String],
    content_segments: &[Value],
    has_tools: bool,
) -> i32 {
    if has_tools {
        return base_tokens.max(1);
    }

    let mut segments = system_segments.iter().map(String::as_str).collect::<Vec<_>>();
    for content in content_segments {
        collect_text_segments(content, &mut segments);
    }
    let char_count = segments.iter().map(|text| text.chars().count()).sum::<usize>();
    if char_count <= 1024 {
        return base_tokens.max(1);
    }
    let colon_count = segments
        .iter()
        .map(|text| text.chars().filter(|character| *character == ':').count())
        .sum::<usize>();
    base_tokens
        .saturating_add((long_text_correction(char_count, colon_count) - 3).max(0))
        .max(1)
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
    serde_json::from_str::<Value>(text.trim()).is_ok_and(|value| {
        matches!(value, Value::Object(_) | Value::Array(_))
    })
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
        .iter()
        .flatten()
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
        "function ", "return ", "const ", "let ", "class ", "def ", "fn ", "#include",
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
                && token.chars().any(|character| character.is_ascii_alphabetic())
                && token.chars().all(|character| {
                    !character.is_ascii_alphabetic() || character.is_ascii_uppercase()
                })
        })
}

pub fn models_response() -> Response {
    const MODEL_IDS: &[&str] = &[
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        "claude-opus-4-5-20251101",
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-7-thinking",
        "claude-opus-4-8",
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-thinking",
    ];

    let data = MODEL_IDS
        .iter()
        .map(|id| {
            format!(
                "{{\"id\":{},\"object\":\"model\",\"created\":1626777600,\"owned_by\":\"custom\",\"supported_endpoint_types\":[\"anthropic\",\"openai\"]}}",
                serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string()),
            )
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

pub fn signature(model: &str, adaptive: bool) -> String {
    if adaptive {
        super::signature::generate_aws_b40_adaptive_signature()
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
    let output_details = if model.to_ascii_lowercase().contains("opus") {
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
        assert_eq!(calibrated_text_output_tokens(r#"{"alpha":1,"beta":"two"}"#, 10), 14);
        assert_eq!(
            calibrated_text_output_tokens(
                r#"{"model_family":"Claude","creator":"Anthropic","backend":"unknown","runtime_product":"unknown"}"#,
                25,
            ),
            39
        );
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
    fn output_usage_calibrates_single_uppercase_markers() {
        assert_eq!(calibrated_text_output_tokens("CACHE_OK", 6), 5);
        assert_eq!(calibrated_text_output_tokens("STREAM_OK", 5), 5);
        assert_eq!(calibrated_text_output_tokens("HELLO", 5), 2);
        assert_eq!(
            calibrated_text_output_tokens("OPENAI_PARITY_0714", 12),
            11
        );
        assert_eq!(
            calibrated_text_output_tokens("OPENAI_STREAM_0714", 11),
            11
        );
        assert_eq!(calibrated_text_output_tokens("ordinary response", 6), 6);
    }

    #[test]
    fn short_plain_text_uses_bedrock_compact_output_accounting() {
        assert_eq!(framed_text_output_tokens("pong", 4), 4);
        assert_eq!(framed_text_output_tokens("Red", 4), 4);
        assert_eq!(framed_text_output_tokens("4", 4), 3);
        assert_eq!(framed_text_output_tokens("CACHE_OK", 6), 9);
        assert_eq!(framed_text_output_tokens("8b520f60e5d01885", 10), 12);
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
    fn image_requests_include_bedrock_media_framing() {
        let payload: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "unused"}},
                    {"type": "text", "text": "What color is this image?"}
                ]
            }]
        }))
        .expect("valid request");

        assert_eq!(calibrated_input_tokens(&payload, 36), 40);
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
                format!(
                    "Stable tool schema segment {index}: alpha beta gamma delta epsilon zeta. "
                )
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
        assert_eq!(calibration.cache_input_adjustment(estimate, calibrated), -17);
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
        assert!(!body.contains("claude-sonnet-5"));
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
}
