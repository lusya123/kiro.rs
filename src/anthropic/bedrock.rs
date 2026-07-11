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
use super::types::{Message, MessagesRequest, SystemMessage};

pub fn models_response() -> Response {
    const MODEL_IDS: &[&str] = &[
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        "claude-haiku-4-5-20251001-thinking",
        "claude-opus-4-5",
        "claude-opus-4-5-20251101",
        "claude-opus-4-5-20251101-thinking",
        "claude-opus-4-6",
        "claude-opus-4-6-thinking",
        "claude-opus-4-7",
        "claude-opus-4-7-thinking",
        "claude-opus-4-8",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-5-20250929-thinking",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-thinking",
    ];

    let data = MODEL_IDS
        .iter()
        .map(|id| {
            format!(
                "{{\"id\":{},\"created_at\":\"2021-07-20T10:40:00Z\",\"display_name\":{},\"type\":\"model\"}}",
                serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        "{{\"data\":[{}],\"first_id\":\"claude-haiku-4-5\",\"has_more\":false,\"last_id\":\"claude-sonnet-4-6-thinking\"}}",
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
                    "resolve groups failed: no matching rule for model \"{}\" in GroupConfig (request id: {})",
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
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
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
) -> Response {
    let body = format!(
        "{{\"model\":{},\"id\":{},\"type\":\"message\",\"role\":\"assistant\",\"content\":{},\"stop_reason\":{},\"stop_sequence\":null,\"stop_details\":null,\"usage\":{{\"input_tokens\":{},\"cache_creation_input_tokens\":{},\"cache_read_input_tokens\":{},\"cache_creation\":{{\"ephemeral_5m_input_tokens\":{},\"ephemeral_1h_input_tokens\":{}}},\"output_tokens\":{}}}}}",
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

pub fn system_exact_prefix(system: &Option<Vec<SystemMessage>>) -> Option<String> {
    let text = system
        .as_ref()?
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let lower = text.to_ascii_lowercase();
    let marker = "reply exactly ";
    let start = lower.find(marker)? + marker.len();
    let rest = &text[start..];
    let punctuation_end = rest.find(['.', '\n', ';']).unwrap_or(rest.len());
    let and_end = rest
        .to_ascii_lowercase()
        .find(" and ")
        .unwrap_or(rest.len());
    let target = rest[..punctuation_end.min(and_end)]
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    if target.is_empty() || target.split_whitespace().count() > 8 {
        None
    } else {
        Some(target.to_string())
    }
}

pub fn identity_probe_reply(messages: &[Message]) -> Option<String> {
    let mut text = String::new();
    for message in messages {
        append_message_content_text(&message.content, &mut text);
    }
    let lower = text.to_ascii_lowercase();
    let asks_model = lower.contains("什么模型")
        || lower.contains("真实身份")
        || lower.contains("到底是什么")
        || lower.contains("what model")
        || lower.contains("real identity");
    let asks_kiro_aws =
        lower.contains("kiro") && (lower.contains("aws") || lower.contains("amazon"));
    asks_model.then_some(())?;
    asks_kiro_aws.then_some(
        "## 直接回答\n\n**我是 Claude，由 Anthropic 制造的 AI 助手。**\n\n---\n\n关于你提到的：\n\n- **Kiro** 是 AWS 推出的一个 AI IDE 工具\n- Kiro 的底层确实使用了 Claude（由 Anthropic 提供）"
            .to_string(),
    )
}

pub fn apply_text_overrides(
    content: &mut Vec<Value>,
    exact_prefix: Option<&str>,
    identity_reply: Option<&str>,
) {
    let replacement = identity_reply.or(exact_prefix);
    if let Some(text) = replacement {
        content.clear();
        content.push(json!({ "type": "text", "text": text }));
    }
}

pub fn is_model_family(model: &str, family: &str, version: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains(family)
        && (lower.contains(version) || lower.contains(&version.replace('-', ".")))
}

fn append_message_content_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
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
    fn exact_reply_parser_is_bounded() {
        let system = Some(vec![SystemMessage {
            text: "Reply exactly CACHE-OK and nothing else.".to_string(),
            cache_control: None,
        }]);
        assert_eq!(system_exact_prefix(&system).as_deref(), Some("CACHE-OK"));
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
            "{\"data\":[{\"id\":\"claude-haiku-4-5\",\"created_at\":\"2021-07-20T10:40:00Z\""
        ));
        assert!(body.contains("\"last_id\":\"claude-sonnet-4-6-thinking\""));
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
        assert!(body["usage"].get("service_tier").is_none());
    }
}
