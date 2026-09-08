//! Public Opus validation shared by both Messages routes and the Chat adapter.
//!
//! Error envelopes follow captured POMO responses. These errors are generated
//! locally; their request IDs are correlation values, not AWS invocation proof.
use super::types::MessagesRequest;
use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

pub(super) fn is_opus(model: &str) -> bool {
    matches!(
        super::converter::map_model(model).as_deref(),
        Some("claude-opus-5" | "claude-opus-4.8")
    )
}

fn unsupported_tool(kind: &str) -> Option<String> {
    ["web_search_", "advisor_", "code_execution_"]
        .iter()
        .any(|prefix| kind.starts_with(prefix))
        .then(|| format!("tool type '{kind}' is not supported for this model"))
}

fn unexpected_role(role: &str) -> String {
    format!(
        "messages: Unexpected role {}. Allowed roles are \"user\" or \"assistant\"",
        json!(role)
    )
}

fn has_url_image(value: &Value) -> bool {
    value.as_array().into_iter().flatten().any(|block| {
        (block.get("type").and_then(Value::as_str) == Some("image")
            && block.pointer("/source/type").and_then(Value::as_str) == Some("url"))
            || (block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("content").is_some_and(has_url_image))
    })
}

pub(super) fn messages_validation_detail(request: &MessagesRequest) -> Option<String> {
    if request
        .temperature
        .is_some_and(|t| !(0.0..=1.0).contains(&t))
    {
        return Some("temperature: range: 0..1".into());
    }
    // Keep the established top-level-system migration error at the ingress.
    if request.messages.first().is_some_and(|m| m.role == "system") {
        return None;
    }
    if let Some(message) = request
        .messages
        .iter()
        .find(|m| !matches!(m.role.as_str(), "user" | "assistant"))
    {
        return Some(unexpected_role(&message.role));
    }
    if let Some(detail) = request
        .tools
        .as_ref()
        .into_iter()
        .flatten()
        .find_map(|tool| tool.tool_type.as_deref().and_then(unsupported_tool))
    {
        return Some(detail);
    }
    if request
        .output_config
        .as_ref()
        .is_some_and(|o| o.format.is_some())
    {
        return Some("output_config.format: Extra inputs are not permitted".into());
    }
    if request
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "enabled")
    {
        return Some("\"***.***.enabled\" is not supported for this model. Use \"***.***.adaptive\" and \"output_config.effort\" to control thinking behavior.".into());
    }
    if request.messages.iter().any(|m| has_url_image(&m.content)) {
        return Some("URL content sources are not yet supported for this model".into());
    }
    None
}

pub(super) fn chat_validation_detail(request: &Value) -> Option<String> {
    if request
        .get("temperature")
        .and_then(Value::as_f64)
        .is_some_and(|t| !(0.0..=1.0).contains(&t))
    {
        return Some("temperature: range: 0..1".into());
    }
    for message in request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(role) = message.get("role").and_then(Value::as_str)
            && !matches!(role, "system" | "developer" | "user" | "assistant" | "tool")
        {
            return Some(unexpected_role(role));
        }
        for (index, block) in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if block.get("type").and_then(Value::as_str) == Some("image_url") {
                return Some(format!(
                    "***.***.content.{index}: Input tag 'image_url' found using 'type' does not match any of the expected tags: 'connector_text', 'document', 'image', 'mid_conv_system', 'redacted_thinking', 'search_result', 'server_tool_use', 'text', 'thinking', 'tool_result', 'tool_search_tool_result', 'tool_use'"
                ));
            }
        }
    }
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|tool| {
            tool.get("type")
                .and_then(Value::as_str)
                .and_then(unsupported_tool)
        })
}

pub(super) fn validation_response(stream: bool, detail: impl AsRef<str>, chat: bool) -> Response {
    let operation = if stream {
        "InvokeModelWithResponseStream"
    } else {
        "InvokeModel"
    };
    let message = format!(
        "{operation}: operation error Bedrock Runtime: {operation}, https response error StatusCode: 400, RequestID: {}, ValidationException: {} (request id: {})",
        uuid::Uuid::new_v4(),
        detail.as_ref(),
        super::compat::oneapi_request_id()
    );
    let body = if chat {
        json!({"error":{"message":message,"type":"upstream_error","param":"","code":"up_bad_request"}})
    } else {
        json!({"error":{"type":"<nil>","message":message},"type":"error"})
    };
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        Json(body),
    )
        .into_response()
}
