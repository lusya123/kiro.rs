//! GPT-5.6-only OpenAI Responses API compatibility.
//!
//! This adapter deliberately reuses `post_messages`, so the requested GPT-5.6
//! model id, reasoning settings, tools, identity handling, and no-fallback
//! routing all go through the same real upstream path as `/v1/messages`.
//! Non-GPT requests retain the historical `/v1/responses` behavior.

use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, stream};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    converter::is_gpt_family_name,
    handlers::{RawApiJson, post_messages},
    middleware::{AppState, mark_gpt_openai_response},
    response_store::{StoreError, StoredConversation},
    types::{Message, MessagesRequest, Metadata, ReasoningConfig, SystemMessage, Tool},
};

const GPT56_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];
const RESPONSES_BODY_LIMIT: usize = 50 * 1024 * 1024;
const IMAGE_MEDIA_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
const DOCUMENT_MEDIA_TYPES: &[&str] = &[
    "application/pdf",
    "text/csv",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/html",
    "text/plain",
    "text/markdown",
];

fn is_exact_gpt56(model: &str) -> bool {
    GPT56_MODELS.contains(&model)
}

fn response_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_server_error() => "server_error",
        _ => "invalid_request_error",
    }
}

fn default_error_message(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "The request was invalid. Check the input and parameters.",
        StatusCode::UNAUTHORIZED => "Authentication failed. Check your API key.",
        StatusCode::FORBIDDEN => "The request is not permitted.",
        StatusCode::NOT_FOUND => "The requested resource was not found.",
        StatusCode::REQUEST_TIMEOUT => "The request timed out. Please retry.",
        StatusCode::PAYLOAD_TOO_LARGE => "The request is too large.",
        StatusCode::TOO_MANY_REQUESTS => "Rate limit exceeded. Please retry later.",
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
            "The model service is temporarily unavailable. Please retry later."
        }
        status if status.is_server_error() => {
            "The request could not be completed due to an internal server error."
        }
        _ => "The request could not be completed.",
    }
}

fn response_error(status: StatusCode, message: impl Into<String>, mark_gpt: bool) -> Response {
    let response = (
        status,
        Json(json!({
            "error": {
                "message": message.into(),
                "type": response_error_type(status),
                "param": Value::Null,
                "code": Value::Null
            }
        })),
    )
        .into_response();
    if mark_gpt {
        mark_gpt_openai_response(response)
    } else {
        response
    }
}

fn invalid_request(message: impl Into<String>, mark_gpt: bool) -> Response {
    response_error(StatusCode::BAD_REQUEST, message, mark_gpt)
}

pub(super) struct ResponsesJson(Value);

impl FromRequest<AppState> for ResponsesJson {
    type Rejection = Response;

    async fn from_request(request: Request, _state: &AppState) -> Result<Self, Self::Rejection> {
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, RESPONSES_BODY_LIMIT)
            .await
            .map_err(|_| {
                response_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    default_error_message(StatusCode::PAYLOAD_TOO_LARGE),
                    true,
                )
            })?;
        let value = serde_json::from_slice(&body)
            .map_err(|_| invalid_request("Invalid JSON request body.", true))?;
        Ok(Self(value))
    }
}

fn legacy_non_gpt_response(state: &AppState) -> Response {
    if !state.aws_b40_compat {
        return StatusCode::NOT_FOUND.into_response();
    }
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": format!("not implemented (request id: {request_id})"),
                "type": "new_api_error",
                "param": "",
                "code": "convert_request_failed"
            }
        })),
    )
        .into_response()
}

fn required_model(request: &Value) -> Result<&str, String> {
    match request.get("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => Ok(model),
        Some(Value::String(_)) => Err("`model` must not be empty".to_string()),
        Some(_) => Err("`model` must be a string".to_string()),
        None => Err("`model` is required".to_string()),
    }
}

fn validate_top_level_fields(request: &Value) -> Result<(), String> {
    let object = request
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "model"
                | "input"
                | "instructions"
                | "max_output_tokens"
                | "reasoning"
                | "tools"
                | "tool_choice"
                | "stream"
                | "store"
                | "metadata"
                | "previous_response_id"
                | "text"
                | "include"
                | "parallel_tool_calls"
                | "prompt_cache_options"
        ) {
            return Err(format!("`{key}` is not supported by this endpoint"));
        }
    }

    match object.get("store") {
        None | Some(Value::Null | Value::Bool(_)) => {}
        Some(_) => return Err("`store` must be a boolean".to_string()),
    }
    match object.get("metadata") {
        None | Some(Value::Null | Value::Object(_)) => {}
        Some(_) => return Err("`metadata` must be an object".to_string()),
    }
    match object.get("previous_response_id") {
        None | Some(Value::Null) => {}
        Some(Value::String(_)) => {}
        Some(_) => return Err("`previous_response_id` must be a string".to_string()),
    }
    match object.get("include") {
        None | Some(Value::Null) => {}
        Some(Value::Array(values)) if values.is_empty() => {}
        Some(Value::Array(values)) => {
            if values
                .iter()
                .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
            {
                return Err(
                    "`include: [\"reasoning.encrypted_content\"]` is unavailable because opaque reasoning items cannot be replayed by this endpoint"
                        .to_string(),
                );
            }
            return Err("non-empty `include` is not supported by this endpoint".to_string());
        }
        Some(_) => return Err("`include` must be an array".to_string()),
    }
    if object
        .get("prompt_cache_options")
        .is_some_and(|value| !value.is_null())
    {
        return Err(
            "`prompt_cache_options` is unavailable because this endpoint cannot provide verifiable cache reads or writes"
                .to_string(),
        );
    }
    match object.get("parallel_tool_calls") {
        None | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => {
            return Err(
                "`parallel_tool_calls: false` cannot be guaranteed by this upstream transport"
                    .to_string(),
            );
        }
        Some(_) => return Err("`parallel_tool_calls` must be a boolean".to_string()),
    }
    match object.get("text") {
        None | Some(Value::Null) => {}
        Some(Value::Object(text)) => {
            for key in text.keys() {
                if key != "format" {
                    return Err(format!("`text.{key}` is not supported"));
                }
            }
            if let Some(format) = text.get("format") {
                let format = format
                    .as_object()
                    .ok_or_else(|| "`text.format` must be an object".to_string())?;
                for key in format.keys() {
                    if key != "type" {
                        return Err(format!("`text.format.{key}` is not supported"));
                    }
                }
                if format.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(
                        "only `text.format.type: \"text\"` is supported by this endpoint"
                            .to_string(),
                    );
                }
            }
        }
        Some(_) => return Err("`text` must be an object".to_string()),
    }
    Ok(())
}

fn optional_string(value: Option<&Value>, path: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{path}` must be a string")),
    }
}

fn max_output_tokens(request: &Value) -> Result<i32, String> {
    match request.get("max_output_tokens") {
        None => Ok(1024),
        Some(Value::Number(number)) => number
            .as_i64()
            .filter(|value| (1..=i32::MAX as i64).contains(value))
            .map(|value| value as i32)
            .ok_or_else(|| {
                "`max_output_tokens` must be an integer between 1 and 2147483647".to_string()
            }),
        Some(_) => Err("`max_output_tokens` must be an integer".to_string()),
    }
}

fn stream_requested(request: &Value) -> Result<bool, String> {
    match request.get("stream") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err("`stream` must be a boolean".to_string()),
    }
}

fn store_requested(request: &Value) -> Result<bool, String> {
    match request.get("store") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err("`store` must be a boolean".to_string()),
    }
}

fn previous_response_id(request: &Value) -> Result<Option<String>, String> {
    match request.get("previous_response_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if value.starts_with("resp_")
                && value.len() > "resp_".len()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) =>
        {
            Ok(Some(value.clone()))
        }
        Some(Value::String(_)) => Err(
            "`previous_response_id` must be a valid `resp_` identifier of at most 128 characters"
                .to_string(),
        ),
        Some(_) => Err("`previous_response_id` must be a string".to_string()),
    }
}

fn reasoning_config(request: &Value) -> Result<Option<ReasoningConfig>, String> {
    let Some(reasoning) = request.get("reasoning") else {
        return Ok(None);
    };
    let reasoning = reasoning
        .as_object()
        .ok_or_else(|| "`reasoning` must be an object".to_string())?;
    for key in reasoning.keys() {
        if !matches!(key.as_str(), "effort" | "mode" | "context") {
            return Err(format!("`reasoning.{key}` is not supported"));
        }
    }
    if reasoning
        .get("context")
        .is_some_and(|context| !context.is_null())
    {
        return Err(
            "`reasoning.context` is unavailable because prior native reasoning items cannot be replayed by this endpoint"
                .to_string(),
        );
    }
    let effort = optional_string(reasoning.get("effort"), "reasoning.effort")?;
    let mode = optional_string(reasoning.get("mode"), "reasoning.mode")?;
    if effort.is_none() && mode.is_none() {
        return Err("`reasoning` must contain `effort` or `mode`".to_string());
    }
    let effort = effort.unwrap_or_else(|| "medium".to_string());
    if !["none", "low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()) {
        return Err(
            "`reasoning.effort` must be one of: none, low, medium, high, xhigh, max".to_string(),
        );
    }
    if let Some(mode) = mode.as_deref()
        && !["standard", "pro"].contains(&mode)
    {
        return Err("`reasoning.mode` must be one of: standard, pro".to_string());
    }
    if mode.as_deref() == Some("pro") && matches!(effort.as_str(), "none" | "low") {
        return Err(
            "`reasoning.mode: \"pro\"` requires effort medium, high, xhigh, or max".to_string(),
        );
    }
    Ok(Some(ReasoningConfig { effort, mode }))
}

fn data_url_source(
    value: &str,
    expected_prefix: &str,
    allowed_media_types: &[&str],
) -> Result<Value, String> {
    let Some(rest) = value.strip_prefix("data:") else {
        return Err(format!("{expected_prefix} data must be a base64 data URL"));
    };
    let Some((media_type, data)) = rest.split_once(";base64,") else {
        return Err(format!("{expected_prefix} data must be a base64 data URL"));
    };
    if media_type.is_empty() || data.is_empty() {
        return Err(format!("{expected_prefix} data must be a base64 data URL"));
    }
    if !allowed_media_types.contains(&media_type) {
        return Err(format!(
            "{expected_prefix} media type `{media_type}` is not supported"
        ));
    }
    Ok(json!({
        "type": "base64",
        "media_type": media_type,
        "data": data
    }))
}

fn validated_filename(value: Option<&Value>) -> Result<Option<&str>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let filename = value
        .as_str()
        .map(str::trim)
        .filter(|filename| !filename.is_empty())
        .ok_or_else(|| "`input_file.filename` must be a non-empty string".to_string())?;
    if filename.chars().count() > 255
        || filename == "."
        || filename == ".."
        || filename.contains(['/', '\\', '\0'])
        || filename.chars().any(char::is_control)
    {
        return Err(
            "`input_file.filename` must be a safe basename of at most 255 characters".to_string(),
        );
    }
    Ok(Some(filename))
}

fn response_content_to_anthropic(content: &Value, role: &str) -> Result<Value, String> {
    if let Some(text) = content.as_str() {
        return Ok(Value::String(text.to_string()));
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| "message `content` must be a string or an array".to_string())?;
    if blocks.is_empty() {
        return Err("message `content` must not be empty".to_string());
    }
    let mut mapped = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        let object = block
            .as_object()
            .ok_or_else(|| format!("message content item {index} must be an object"))?;
        let block_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("message content item {index} requires string `type`"))?;
        if object.contains_key("prompt_cache_breakpoint") {
            return Err(
                "`prompt_cache_breakpoint` is unavailable because this endpoint cannot provide verifiable cache reads or writes"
                    .to_string(),
            );
        }
        match block_type {
            "input_text" | "output_text" => {
                for key in object.keys() {
                    if !matches!(key.as_str(), "type" | "text") {
                        return Err(format!(
                            "message content item {index} field `{key}` is not supported"
                        ));
                    }
                }
                let text = object.get("text").and_then(Value::as_str).ok_or_else(|| {
                    format!("message content item {index} requires string `text`")
                })?;
                mapped.push(json!({"type": "text", "text": text}));
            }
            "input_image" => {
                if role != "user" {
                    return Err("`input_image` is only supported in user messages".to_string());
                }
                for key in object.keys() {
                    if !matches!(key.as_str(), "type" | "image_url" | "detail") {
                        return Err(format!(
                            "message content item {index} field `{key}` is not supported"
                        ));
                    }
                }
                let image_url = object
                    .get("image_url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!(
                            "message content item {index} requires string `image_url`; `file_id` images are not supported"
                        )
                    })?;
                match object.get("detail") {
                    None => {}
                    Some(Value::String(detail)) if detail == "auto" => {}
                    Some(_) => {
                        return Err(
                            "`input_image.detail` only supports `auto`; low/high/original cannot be represented by this model transport"
                                .to_string(),
                        );
                    }
                }
                let source = if image_url.starts_with("data:") {
                    data_url_source(image_url, "image", IMAGE_MEDIA_TYPES)?
                } else if image_url.starts_with("http://") || image_url.starts_with("https://") {
                    json!({"type": "url", "url": image_url})
                } else {
                    return Err(
                        "`input_image.image_url` must be an http(s) URL or base64 data URL"
                            .to_string(),
                    );
                };
                mapped.push(json!({"type": "image", "source": source}));
            }
            "input_file" => {
                if role != "user" {
                    return Err("`input_file` is only supported in user messages".to_string());
                }
                for key in object.keys() {
                    if !matches!(key.as_str(), "type" | "file_data" | "filename") {
                        return Err(format!(
                            "message content item {index} field `{key}` is not supported; only base64 `file_data` is accepted"
                        ));
                    }
                }
                let filename = validated_filename(object.get("filename"))?;
                let file_data = object
                    .get("file_data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!(
                            "message content item {index} requires base64 `file_data`; `file_id` and `file_url` are not supported"
                        )
                    })?;
                let mut document = json!({
                    "type": "document",
                    "source": data_url_source(file_data, "file", DOCUMENT_MEDIA_TYPES)?
                });
                if let Some(filename) = filename {
                    document["name"] = Value::String(filename.to_string());
                }
                mapped.push(document);
            }
            other => {
                return Err(format!(
                    "message content item {index} type `{other}` is not supported"
                ));
            }
        }
    }
    Ok(Value::Array(mapped))
}

fn append_input_item(
    item: &Value,
    messages: &mut Vec<Message>,
    system: &mut Vec<SystemMessage>,
) -> Result<(), String> {
    let object = item
        .as_object()
        .ok_or_else(|| "each `input` item must be an object".to_string())?;
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match item_type {
        "message" => {
            for key in object.keys() {
                if !matches!(key.as_str(), "type" | "role" | "content" | "id" | "status") {
                    return Err(format!("input message field `{key}` is not supported"));
                }
            }
            let role = object
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| "input message requires string `role`".to_string())?;
            if !matches!(role, "user" | "assistant" | "system" | "developer") {
                return Err(format!("input message role `{role}` is not supported"));
            }
            let content = object
                .get("content")
                .ok_or_else(|| "input message requires `content`".to_string())?;
            if matches!(role, "system" | "developer") {
                let value = response_content_to_anthropic(content, role)?;
                let text = match value {
                    Value::String(text) => text,
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                    _ => String::new(),
                };
                if text.is_empty() {
                    return Err("system/developer message content must contain text".to_string());
                }
                system.push(SystemMessage {
                    text,
                    cache_control: None,
                });
            } else {
                messages.push(Message {
                    role: role.to_string(),
                    content: response_content_to_anthropic(content, role)?,
                });
            }
        }
        "function_call" => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type" | "id" | "call_id" | "name" | "arguments" | "status"
                ) {
                    return Err(format!(
                        "function call input field `{key}` is not supported"
                    ));
                }
            }
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "function call input requires string `call_id`".to_string())?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "function call input requires string `name`".to_string())?;
            let arguments = object
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| "function call input requires string `arguments`".to_string())?;
            let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
                "function call input `arguments` must contain valid JSON".to_string()
            })?;
            messages.push(Message {
                role: "assistant".to_string(),
                content: json!([{
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": arguments
                }]),
            });
        }
        "function_call_output" => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type" | "id" | "call_id" | "output" | "status"
                ) {
                    return Err(format!(
                        "function call output input field `{key}` is not supported"
                    ));
                }
            }
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "function call output requires string `call_id`".to_string())?;
            let output = object
                .get("output")
                .and_then(Value::as_str)
                .ok_or_else(|| "function call output requires string `output`".to_string())?;
            messages.push(Message {
                role: "user".to_string(),
                content: json!([{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output
                }]),
            });
        }
        other => return Err(format!("input item type `{other}` is not supported")),
    }
    Ok(())
}

fn input_to_messages(
    request: &Value,
    instructions: Option<String>,
) -> Result<(Vec<Message>, Option<Vec<SystemMessage>>), String> {
    let input = request
        .get("input")
        .ok_or_else(|| "`input` is required".to_string())?;
    let mut messages = Vec::new();
    let mut system = Vec::new();
    if let Some(instructions) = instructions.filter(|value| !value.is_empty()) {
        system.push(SystemMessage {
            text: instructions,
            cache_control: None,
        });
    }
    match input {
        Value::String(text) => messages.push(Message {
            role: "user".to_string(),
            content: Value::String(text.clone()),
        }),
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                append_input_item(item, &mut messages, &mut system)?;
            }
        }
        Value::Array(_) => return Err("`input` must not be empty".to_string()),
        _ => return Err("`input` must be a string or an array of input items".to_string()),
    }
    if messages.is_empty() {
        return Err("`input` must contain at least one non-system message".to_string());
    }
    Ok((messages, (!system.is_empty()).then_some(system)))
}

fn tools_to_anthropic(
    request: &Value,
    accept_strict_hint: bool,
) -> Result<Option<Vec<Tool>>, String> {
    let Some(tools) = request.get("tools") else {
        return Ok(None);
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| "`tools` must be an array".to_string())?;
    if tools.is_empty() {
        return Ok(None);
    }
    let mut mapped = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let object = tool
            .as_object()
            .ok_or_else(|| format!("tool {index} must be an object"))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "type" | "name" | "description" | "parameters" | "strict"
            ) {
                return Err(format!("tool {index} field `{key}` is not supported"));
            }
        }
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(format!("tool {index} must have `type`: `function`"));
        }
        if object.get("strict").and_then(Value::as_bool) == Some(true) && !accept_strict_hint {
            return Err(format!(
                "tool {index} strict schema enforcement is not supported"
            ));
        }
        if object
            .get("strict")
            .is_some_and(|strict| !strict.is_null() && !strict.is_boolean())
        {
            return Err(format!("tool {index} `strict` must be a boolean"));
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("tool {index} requires non-empty string `name`"))?;
        let description = match object.get("description") {
            None => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => return Err(format!("tool {index} `description` must be a string")),
        };
        // 同 `openai_compat`:上游拒绝空 description 的工具,回退到工具名。
        let description = if description.trim().is_empty() {
            name.to_string()
        } else {
            description
        };
        let parameters = object
            .get("parameters")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("tool {index} requires object `parameters`"))?;
        mapped.push(Tool {
            tool_type: None,
            name: name.to_string(),
            description,
            input_schema: parameters
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            strict: None,
            max_uses: None,
            cache_control: None,
        });
    }
    Ok(Some(mapped))
}

fn tool_choice_to_anthropic(
    request: &Value,
    tools: Option<&[Tool]>,
) -> Result<Option<Value>, String> {
    let Some(choice) = request.get("tool_choice") else {
        return Ok(None);
    };
    if let Some(choice) = choice.as_str() {
        return match choice {
            "auto" => Ok(Some(json!({"type": "auto"}))),
            "required" if tools.is_some_and(|tools| !tools.is_empty()) => {
                Ok(Some(json!({"type": "any"})))
            }
            "required" => Err("`tool_choice`: `required` requires at least one tool".to_string()),
            "none" => Ok(None),
            _ => Err(
                "`tool_choice` must be `auto`, `required`, `none`, or a function object"
                    .to_string(),
            ),
        };
    }
    let object = choice.as_object().ok_or_else(|| {
        "`tool_choice` must be `auto`, `required`, `none`, or a function object".to_string()
    })?;
    for key in object.keys() {
        if !matches!(key.as_str(), "type" | "name") {
            return Err(format!("`tool_choice.{key}` is not supported"));
        }
    }
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err("function `tool_choice` must have `type`: `function`".to_string());
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "function `tool_choice` requires string `name`".to_string())?;
    if !tools.is_some_and(|tools| tools.iter().any(|tool| tool.name == name)) {
        return Err(format!(
            "`tool_choice` references unknown function `{name}`"
        ));
    }
    Ok(Some(json!({"type": "tool", "name": name})))
}

#[derive(Clone)]
struct ResponseMeta {
    id: String,
    model: String,
    created_at: u64,
    instructions: Option<String>,
    max_output_tokens: i32,
    reasoning: Option<ReasoningConfig>,
    metadata: Value,
    tools: Value,
    tool_choice: Value,
    store: bool,
    previous_response_id: Option<String>,
}

impl ResponseMeta {
    fn new(
        request: &Value,
        model: &str,
        max_output_tokens: i32,
        reasoning: Option<ReasoningConfig>,
        store: bool,
        previous_response_id: Option<String>,
    ) -> Self {
        Self {
            id: format!("resp_{}", Uuid::new_v4().simple()),
            model: model.to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
            instructions: request
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_string),
            max_output_tokens,
            reasoning,
            metadata: request
                .get("metadata")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
            tools: request.get("tools").cloned().unwrap_or_else(|| json!([])),
            tool_choice: request
                .get("tool_choice")
                .cloned()
                .unwrap_or_else(|| json!("auto")),
            store,
            previous_response_id,
        }
    }

    fn response(&self, status: &str, output: Value, usage: Value) -> Value {
        let incomplete_details = if status == "incomplete" {
            json!({"reason": "max_output_tokens"})
        } else {
            Value::Null
        };
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "error": Value::Null,
            "incomplete_details": incomplete_details,
            "instructions": self.instructions,
            "max_output_tokens": self.max_output_tokens,
            "metadata": self.metadata,
            "model": self.model,
            "output": output,
            "parallel_tool_calls": true,
            "previous_response_id": self.previous_response_id,
            "reasoning": self.reasoning.as_ref().map(|reasoning| json!({
                "effort": reasoning.effort,
                "mode": reasoning.mode,
                "summary": Value::Null
            })).unwrap_or_else(|| json!({
                "effort": Value::Null,
                "mode": Value::Null,
                "summary": Value::Null
            })),
            "text": {"format": {"type": "text"}},
            "tool_choice": self.tool_choice,
            "tools": self.tools,
            "truncation": "disabled",
            "store": self.store,
            "usage": usage
        })
    }
}

fn responses_usage(usage: &Value) -> Value {
    let get = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let pointer = |path: &str| usage.pointer(path).and_then(Value::as_i64).unwrap_or(0);
    let input_tokens =
        get("input_tokens") + get("cache_creation_input_tokens") + get("cache_read_input_tokens");
    let output_tokens = get("output_tokens");
    json!({
        "input_tokens": input_tokens,
        "input_tokens_details": {
            "cached_tokens": get("cache_read_input_tokens")
        },
        "output_tokens": output_tokens,
        "output_tokens_details": {
            "reasoning_tokens": pointer("/output_tokens_details/thinking_tokens")
        },
        "total_tokens": input_tokens + output_tokens
    })
}

fn new_message_output(texts: Vec<String>) -> Value {
    json!({
        "type": "message",
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "status": "completed",
        "role": "assistant",
        "content": texts.into_iter().map(|text| json!({
            "type": "output_text",
            "text": text,
            "annotations": [],
            "logprobs": []
        })).collect::<Vec<_>>()
    })
}

fn new_function_output(block: &Value, call_id: &str) -> Value {
    json!({
        "type": "function_call",
        "id": format!("fc_{}", Uuid::new_v4().simple()),
        "call_id": call_id,
        "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
        "arguments": serde_json::to_string(
            block.get("input").unwrap_or(&Value::Object(Map::new()))
        ).unwrap_or_else(|_| "{}".to_string()),
        "status": "completed"
    })
}

struct MappedResponse {
    response: Value,
    assistant_message: Option<Message>,
}

fn anthropic_to_response(
    anthropic: &Value,
    meta: &ResponseMeta,
) -> Result<MappedResponse, &'static str> {
    let blocks = anthropic
        .get("content")
        .and_then(Value::as_array)
        .ok_or("model response did not contain a content array")?;
    let mut text = Vec::new();
    let mut output = Vec::new();
    let mut assistant_blocks = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                text.push(value.clone());
                assistant_blocks.push(json!({"type": "text", "text": value}));
            }
            Some("tool_use") => {
                let call_id = format!("call_{}", Uuid::new_v4().simple());
                output.push(new_function_output(block, &call_id));
                assistant_blocks.push(json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "input": block.get("input").cloned().unwrap_or_else(|| json!({}))
                }));
            }
            Some("thinking") | Some("redacted_thinking") => {}
            _ => return Err("model response contained an unsupported output item"),
        }
    }
    if !text.is_empty() {
        output.insert(0, new_message_output(text));
    }
    let incomplete = anthropic.get("stop_reason").and_then(Value::as_str) == Some("max_tokens");
    Ok(MappedResponse {
        response: meta.response(
            if incomplete {
                "incomplete"
            } else {
                "completed"
            },
            Value::Array(output),
            responses_usage(anthropic.get("usage").unwrap_or(&Value::Null)),
        ),
        assistant_message: (!assistant_blocks.is_empty()).then_some(Message {
            role: "assistant".to_string(),
            content: Value::Array(assistant_blocks),
        }),
    })
}

#[derive(Debug)]
enum StreamingItem {
    Text {
        output_index: usize,
        item: Value,
        content_index: usize,
        text: String,
    },
    Function {
        output_index: usize,
        item: Value,
        arguments: String,
    },
}

struct ResponsesStreamState {
    meta: ResponseMeta,
    sequence: u64,
    started: bool,
    done: bool,
    usage: Value,
    items: Vec<Value>,
    active: HashMap<i64, StreamingItem>,
    next_output_index: usize,
    stop_reason: Option<String>,
}

impl ResponsesStreamState {
    fn new(meta: ResponseMeta) -> Self {
        Self {
            meta,
            sequence: 0,
            started: false,
            done: false,
            usage: json!({}),
            items: Vec::new(),
            active: HashMap::new(),
            next_output_index: 0,
            stop_reason: None,
        }
    }

    fn event(&mut self, event_name: &str, mut data: Value) -> String {
        let object = data
            .as_object_mut()
            .expect("Responses stream event payload must be an object");
        object.insert("type".to_string(), Value::String(event_name.to_string()));
        object.insert("sequence_number".to_string(), json!(self.sequence));
        self.sequence += 1;
        format!("event: {event_name}\ndata: {data}\n\n")
    }

    fn initial_events(&mut self) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        let created = self.meta.response("in_progress", json!([]), Value::Null);
        let in_progress = self.meta.response("in_progress", json!([]), Value::Null);
        format!(
            "{}{}",
            self.event("response.created", json!({"response": created})),
            self.event("response.in_progress", json!({"response": in_progress}))
        )
    }

    fn merge_usage(&mut self, update: &Value) {
        let Some(update) = update.as_object() else {
            return;
        };
        let current = self
            .usage
            .as_object_mut()
            .expect("stream usage must be an object");
        for (key, value) in update {
            current.insert(key.clone(), value.clone());
        }
    }

    fn block_start(&mut self, event: &Value) -> String {
        let Some(block) = event.get("content_block") else {
            return String::new();
        };
        let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let item = json!({
                    "type": "message",
                    "id": format!("msg_{}", Uuid::new_v4().simple()),
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                });
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                self.active.insert(
                    block_index,
                    StreamingItem::Text {
                        output_index,
                        item: item.clone(),
                        content_index: 0,
                        text: String::new(),
                    },
                );
                format!(
                    "{}{}",
                    self.event(
                        "response.output_item.added",
                        json!({"output_index": output_index, "item": item})
                    ),
                    self.event(
                        "response.content_part.added",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                                "logprobs": []
                            }
                        })
                    )
                )
            }
            Some("tool_use" | "server_tool_use") => {
                let item = json!({
                    "type": "function_call",
                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                    "call_id": format!("call_{}", Uuid::new_v4().simple()),
                    "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": "",
                    "status": "in_progress"
                });
                self.active.insert(
                    block_index,
                    StreamingItem::Function {
                        output_index,
                        item: item.clone(),
                        arguments: String::new(),
                    },
                );
                self.event(
                    "response.output_item.added",
                    json!({"output_index": output_index, "item": item}),
                )
            }
            Some("thinking" | "redacted_thinking") => {
                self.next_output_index = self.next_output_index.saturating_sub(1);
                String::new()
            }
            _ => {
                self.next_output_index = self.next_output_index.saturating_sub(1);
                String::new()
            }
        }
    }

    fn block_delta(&mut self, event: &Value) -> String {
        let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
        let Some(delta) = event.get("delta") else {
            return String::new();
        };
        let Some(active) = self.active.get_mut(&block_index) else {
            return String::new();
        };
        match active {
            StreamingItem::Text {
                output_index,
                item,
                content_index,
                text,
            } if delta.get("type").and_then(Value::as_str) == Some("text_delta") => {
                let value = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                text.push_str(&value);
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                let output_index = *output_index;
                let content_index = *content_index;
                self.event(
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": content_index,
                        "delta": value,
                        "logprobs": []
                    }),
                )
            }
            StreamingItem::Function {
                output_index,
                item,
                arguments,
            } if delta.get("type").and_then(Value::as_str) == Some("input_json_delta") => {
                let value = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                arguments.push_str(&value);
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                let output_index = *output_index;
                self.event(
                    "response.function_call_arguments.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": value
                    }),
                )
            }
            _ => String::new(),
        }
    }

    fn block_stop(&mut self, event: &Value) -> String {
        let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
        let Some(active) = self.active.remove(&block_index) else {
            return String::new();
        };
        match active {
            StreamingItem::Text {
                output_index,
                mut item,
                content_index,
                text,
            } => {
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                let part = json!({
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                    "logprobs": []
                });
                item["status"] = json!("completed");
                item["content"] = json!([part.clone()]);
                self.items.push(item.clone());
                format!(
                    "{}{}{}",
                    self.event(
                        "response.output_text.done",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "text": text,
                            "logprobs": []
                        })
                    ),
                    self.event(
                        "response.content_part.done",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": content_index,
                            "part": part
                        })
                    ),
                    self.event(
                        "response.output_item.done",
                        json!({"output_index": output_index, "item": item})
                    )
                )
            }
            StreamingItem::Function {
                output_index,
                mut item,
                mut arguments,
            } => {
                if arguments.is_empty() {
                    arguments = "{}".to_string();
                }
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                item["arguments"] = Value::String(arguments.clone());
                item["status"] = json!("completed");
                self.items.push(item.clone());
                format!(
                    "{}{}",
                    self.event(
                        "response.function_call_arguments.done",
                        json!({
                            "item_id": item_id,
                            "output_index": output_index,
                            "arguments": arguments
                        })
                    ),
                    self.event(
                        "response.output_item.done",
                        json!({"output_index": output_index, "item": item})
                    )
                )
            }
        }
    }

    fn completed(&mut self, stop_reason: Option<&str>) -> String {
        if self.done {
            return String::new();
        }
        self.done = true;
        let status = if stop_reason == Some("max_tokens") {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.meta.response(
            status,
            Value::Array(self.items.clone()),
            responses_usage(&self.usage),
        );
        self.event(
            if status == "completed" {
                "response.completed"
            } else {
                "response.incomplete"
            },
            json!({"response": response}),
        )
    }

    fn clean_stream_error(&mut self) -> String {
        if self.done {
            return String::new();
        }
        self.done = true;
        self.event(
            "error",
            json!({
                "code": "server_error",
                "message": "The response stream ended before completion.",
                "param": Value::Null
            }),
        )
    }

    fn transform(&mut self, event_name: &str, event: &Value) -> String {
        match event_name {
            "ping" => String::new(),
            "message_start" => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.merge_usage(usage);
                }
                self.initial_events()
            }
            "content_block_start" => self.block_start(event),
            "content_block_delta" => self.block_delta(event),
            "content_block_stop" => self.block_stop(event),
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    self.merge_usage(usage);
                }
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                String::new()
            }
            "message_stop" => {
                let reason = self.stop_reason.clone();
                self.completed(reason.as_deref())
            }
            "error" => self.clean_stream_error(),
            _ => String::new(),
        }
    }
}

fn parse_sse_block(block: &[u8]) -> Option<(String, Value)> {
    let text = std::str::from_utf8(block).ok()?;
    let mut event_name = None;
    let mut data = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    Some((event_name?, serde_json::from_str(&data).ok()?))
}

fn sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2));
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn drain_sse(buffer: &mut BytesMut, state: &mut ResponsesStreamState) -> String {
    let mut output = String::new();
    while let Some((position, separator_len)) = sse_boundary(buffer) {
        let mut block = buffer.split_to(position + separator_len);
        block.truncate(position);
        if let Some((event_name, event)) = parse_sse_block(&block) {
            output.push_str(&state.transform(&event_name, &event));
        }
    }
    output
}

fn responses_stream_response(body: Body, meta: ResponseMeta) -> Response {
    let input = body.into_data_stream();
    let output = stream::unfold(
        (
            input,
            BytesMut::new(),
            ResponsesStreamState::new(meta),
            false,
        ),
        |(mut input, mut buffer, mut state, finished)| async move {
            if finished {
                return None;
            }
            loop {
                match input.next().await {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        let transformed = drain_sse(&mut buffer, &mut state);
                        if !transformed.is_empty() {
                            return Some((
                                Ok::<Bytes, Infallible>(Bytes::from(transformed)),
                                (input, buffer, state, false),
                            ));
                        }
                    }
                    Some(Err(_)) => {
                        let error = state.clean_stream_error();
                        return Some((Ok(Bytes::from(error)), (input, buffer, state, true)));
                    }
                    None => {
                        let mut transformed = drain_sse(&mut buffer, &mut state);
                        if !state.done {
                            transformed.push_str(&state.clean_stream_error());
                        }
                        if transformed.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(transformed)), (input, buffer, state, true)));
                    }
                }
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(output))
        .expect("Responses stream response")
}

fn translated_request_for_profile(
    request: &Value,
    model: &str,
    accept_strict_hint: bool,
) -> Result<(MessagesRequest, ResponseMeta), String> {
    validate_top_level_fields(request)?;
    let instructions = optional_string(request.get("instructions"), "instructions")?;
    let max_tokens = max_output_tokens(request)?;
    let stream = stream_requested(request)?;
    let reasoning = reasoning_config(request)?;
    let store = store_requested(request)?;
    let previous_response_id = previous_response_id(request)?;
    let (messages, system) = input_to_messages(request, instructions)?;
    let mut tools = tools_to_anthropic(request, accept_strict_hint)?;
    let tool_choice = tool_choice_to_anthropic(request, tools.as_deref())?;
    if request.get("tool_choice").and_then(Value::as_str) == Some("none") {
        tools = None;
    }
    let meta = ResponseMeta::new(
        request,
        model,
        max_tokens,
        reasoning.clone(),
        store,
        previous_response_id,
    );
    Ok((
        MessagesRequest {
            model: model.to_string(),
            max_tokens,
            messages,
            stream,
            system,
            tools,
            tool_choice,
            thinking: None,
            output_config: None,
            reasoning,
            cache_control: None,
            metadata: Some(Metadata {
                user_id: None,
                kiro_rs_openai_compat: Some(true),
            }),
        },
        meta,
    ))
}

#[cfg(test)]
fn translated_request(
    request: &Value,
    model: &str,
) -> Result<(MessagesRequest, ResponseMeta), String> {
    translated_request_for_profile(request, model, false)
}

fn apply_conversation_state(
    translated: &mut MessagesRequest,
    prior: Option<StoredConversation>,
) -> String {
    let session_id = prior
        .as_ref()
        .map(|prior| prior.session_id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if let Some(prior) = prior {
        let mut messages = prior.messages;
        messages.append(&mut translated.messages);
        translated.messages = messages;
    }
    translated.metadata = Some(Metadata {
        user_id: Some(json!({"session_id": session_id}).to_string()),
        kiro_rs_openai_compat: Some(true),
    });
    session_id
}

/// POST /v1/responses.
pub async fn post_responses(
    State(state): State<AppState>,
    ResponsesJson(request): ResponsesJson,
) -> Response {
    let model = match required_model(&request) {
        Ok(model) => model.to_string(),
        Err(message) => return invalid_request(message, true),
    };
    if !is_exact_gpt56(&model) {
        if is_gpt_family_name(&model) {
            return invalid_request(
                format!(
                    "The model `{model}` is not supported. Supported models: {}",
                    GPT56_MODELS.join(", ")
                ),
                true,
            );
        }
        return legacy_non_gpt_response(&state);
    }

    let (mut translated, meta) =
        match translated_request_for_profile(&request, &model, state.aws_b40_compat) {
            Ok(translated) => translated,
            Err(message) => return invalid_request(message, true),
        };
    debug_assert_eq!(translated.model, model);
    let stream_requested = translated.stream;
    if stream_requested && (meta.store || meta.previous_response_id.is_some()) {
        return invalid_request(
            "`stream: true` cannot be combined with `store: true` or `previous_response_id` on this endpoint",
            true,
        );
    }

    let prior = if let Some(previous_response_id) = meta.previous_response_id.as_deref() {
        let Some(prior) = state.response_store.get(previous_response_id) else {
            return response_error(
                StatusCode::NOT_FOUND,
                format!("Response `{previous_response_id}` was not found or has expired."),
                true,
            );
        };
        if prior.model != model {
            return invalid_request(
                "`previous_response_id` must reference a response created with the same model",
                true,
            );
        }
        Some(prior)
    } else {
        None
    };

    let session_id = apply_conversation_state(&mut translated, prior);

    let mut stored_messages = meta.store.then(|| translated.messages.clone());
    if let Some(messages) = stored_messages.as_ref() {
        let candidate = StoredConversation {
            model: model.clone(),
            session_id: session_id.clone(),
            messages: messages.clone(),
        };
        if let Err(StoreError::EntryTooLarge { max_bytes }) =
            state.response_store.validate_size(&candidate)
        {
            return response_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "`store: true` conversation state exceeds the {max_bytes}-byte retention limit"
                ),
                true,
            );
        }
    }

    let raw = Bytes::from(
        serde_json::to_vec(&translated).expect("translated Responses request must serialize"),
    );
    let response = post_messages(
        State(state.clone()),
        HeaderMap::new(),
        RawApiJson(translated, raw),
    )
    .await;
    let status = response.status();

    if stream_requested && status.is_success() {
        return mark_gpt_openai_response(responses_stream_response(response.into_body(), meta));
    }

    let body = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => {
            return response_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                default_error_message(StatusCode::INTERNAL_SERVER_ERROR),
                true,
            );
        }
    };
    if !status.is_success() {
        return response_error(status, default_error_message(status), true);
    }
    let anthropic: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return response_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                default_error_message(StatusCode::INTERNAL_SERVER_ERROR),
                true,
            );
        }
    };
    let mapped = match anthropic_to_response(&anthropic, &meta) {
        Ok(response) => response,
        Err(_) => {
            return response_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                default_error_message(StatusCode::INTERNAL_SERVER_ERROR),
                true,
            );
        }
    };
    if let Some(messages) = stored_messages.as_mut() {
        if let Some(assistant) = mapped.assistant_message {
            messages.push(assistant);
        }
        let stored = StoredConversation {
            model: model.clone(),
            session_id,
            messages: std::mem::take(messages),
        };
        if let Err(StoreError::EntryTooLarge { max_bytes }) =
            state.response_store.insert(meta.id.clone(), stored)
        {
            return response_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "The completed response exceeds the {max_bytes}-byte retention limit required by `store: true`"
                ),
                true,
            );
        }
    }
    mark_gpt_openai_response((StatusCode::OK, Json(mapped.response)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&body).expect("JSON response")
    }

    fn base_request(model: &str) -> Value {
        json!({
            "model": model,
            "input": "Say hello",
            "max_output_tokens": 64
        })
    }

    fn base_meta(model: &str) -> ResponseMeta {
        ResponseMeta::new(&base_request(model), model, 64, None, false, None)
    }

    fn parsed_events(output: &str) -> Vec<(String, Value)> {
        output
            .split("\n\n")
            .filter_map(|block| {
                let block = block.trim();
                (!block.is_empty())
                    .then(|| parse_sse_block(block.as_bytes()))
                    .flatten()
            })
            .collect()
    }

    #[test]
    fn translates_all_three_exact_models_without_fallback() {
        for model in GPT56_MODELS {
            let request = base_request(model);
            let (translated, meta) =
                translated_request(&request, model).expect("valid Responses request");
            assert_eq!(translated.model, *model);
            assert_eq!(meta.model, *model);
            assert_eq!(translated.messages[0].content, "Say hello");
        }
    }

    #[test]
    fn translates_reasoning_function_tools_and_input_items() {
        let request = json!({
            "model": "gpt-5.6-terra",
            "instructions": "Be concise",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Weather?"}]},
                {
                    "type": "function_call",
                    "call_id": "call_previous",
                    "name": "weather",
                    "arguments": "{\"city\":\"Paris\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_previous",
                    "output": "sunny"
                }
            ],
            "reasoning": {"effort": "xhigh", "mode": "pro"},
            "tools": [{
                "type": "function",
                "name": "weather",
                "description": "Get weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}}
                }
            }],
            "tool_choice": {"type": "function", "name": "weather"}
        });
        let (translated, _) = translated_request(&request, "gpt-5.6-terra").expect("valid request");
        assert_eq!(translated.model, "gpt-5.6-terra");
        assert_eq!(translated.system.unwrap()[0].text, "Be concise");
        assert_eq!(
            translated.reasoning,
            Some(ReasoningConfig {
                effort: "xhigh".to_string(),
                mode: Some("pro".to_string())
            })
        );
        assert_eq!(translated.tools.unwrap()[0].name, "weather");
        assert_eq!(
            translated.tool_choice,
            Some(json!({"type": "tool", "name": "weather"}))
        );
        assert_eq!(translated.messages.len(), 3);
    }

    #[test]
    fn responses_profile_accepts_strict_only_as_an_aws_b_hint() {
        let mut request = base_request("gpt-5.6-terra");
        request["tools"] = json!([{
            "type": "function",
            "name": "weather",
            "description": "Get weather",
            "parameters": {"type": "object"},
            "strict": true
        }]);
        request["tool_choice"] = json!("required");

        assert!(
            translated_request(&request, "gpt-5.6-terra").is_err(),
            "non AWS-B profiles keep strict rejection"
        );
        let (translated, _) = translated_request_for_profile(&request, "gpt-5.6-terra", true)
            .expect("AWS-B accepts strict as a hint");
        assert_eq!(translated.tools.as_ref().unwrap()[0].strict, None);

        request["tools"][0]["strict"] = json!("yes");
        assert!(translated_request_for_profile(&request, "gpt-5.6-terra", true).is_err());
        assert!(translated_request(&request, "gpt-5.6-terra").is_err());
    }

    #[test]
    fn translates_all_six_reasoning_efforts_for_every_exact_model() {
        for model in GPT56_MODELS {
            for effort in ["none", "low", "medium", "high", "xhigh", "max"] {
                let mut request = base_request(model);
                request["reasoning"] = json!({"effort": effort});
                let (translated, meta) =
                    translated_request(&request, model).expect("valid reasoning effort");
                assert_eq!(translated.model, *model);
                assert_eq!(meta.model, *model);
                assert_eq!(
                    translated.reasoning,
                    Some(ReasoningConfig {
                        effort: effort.to_string(),
                        mode: None,
                    }),
                    "{model}/{effort}"
                );
            }
        }
    }

    #[test]
    fn responses_preserve_trusted_application_persona_for_composed_identity_questions() {
        let persona = "You are Claude Code, Anthropic's official CLI for Claude.\n\
You are CodeAssist v2, a programming assistant. When asked about your identity, name, \
or which model you are, respond with exactly: 'I am CodeAssist v2.' Do not mention any \
other product, model, or company.";
        let requests = [
            json!({
                "model": "gpt-5.6-sol",
                "instructions": persona,
                "input": "Hi, please tell me which model or product you are."
            }),
            json!({
                "model": "gpt-5.6-sol",
                "input": [
                    {"role": "system", "content": persona},
                    {
                        "role": "user",
                        "content": "Please introduce yourself and state which model you are."
                    }
                ]
            }),
        ];

        for request in requests {
            let (translated, _) =
                translated_request(&request, "gpt-5.6-sol").expect("valid Responses request");
            assert_eq!(
                super::super::compat::trusted_application_persona_reply_for_identity_request(
                    &translated
                )
                .as_deref(),
                Some("I am CodeAssist v2.")
            );
            let converted = super::super::converter::convert_request(&translated)
                .expect("trusted persona converts");
            assert!(
                converted
                    .conversation_state
                    .current_message
                    .user_input_message
                    .content
                    .ends_with("I am CodeAssist v2.")
            );
        }
    }

    #[test]
    fn reasoning_mode_only_uses_official_medium_effort_default() {
        let mut request = base_request("gpt-5.6-terra");
        request["reasoning"] = json!({"mode": "pro"});
        let (translated, _) =
            translated_request(&request, "gpt-5.6-terra").expect("mode-only reasoning");
        assert_eq!(
            translated.reasoning,
            Some(ReasoningConfig {
                effort: "medium".to_string(),
                mode: Some("pro".to_string())
            })
        );

        let (without_reasoning, _) =
            translated_request(&base_request("gpt-5.6-terra"), "gpt-5.6-terra")
                .expect("request without reasoning");
        assert!(without_reasoning.reasoning.is_none());

        for invalid in [
            json!({"effort": "minimal"}),
            json!({"effort": "ultra"}),
            json!({"effort": "low", "mode": "pro"}),
            json!({"effort": "none", "mode": "pro"}),
        ] {
            let mut request = base_request("gpt-5.6-terra");
            request["reasoning"] = invalid;
            assert!(translated_request(&request, "gpt-5.6-terra").is_err());
        }
    }

    #[test]
    fn accepts_real_stateful_fields_and_rejects_unverifiable_features() {
        let mut request = base_request("gpt-5.6-sol");
        request["store"] = json!(true);
        request["metadata"] = json!({"test_run": "gpt56"});
        request["previous_response_id"] = json!("resp_previous");
        request["text"] = json!({"format": {"type": "text"}});
        request["include"] = json!([]);
        request["parallel_tool_calls"] = json!(true);

        let (_, meta) = translated_request(&request, "gpt-5.6-sol").expect("safe optional fields");
        let response = meta.response("completed", json!([]), json!({}));
        assert_eq!(response["metadata"]["test_run"], "gpt56");
        assert_eq!(response["store"], true);
        assert_eq!(response["previous_response_id"], "resp_previous");

        for (field, value) in [
            ("include", json!(["message.output_text.logprobs"])),
            ("parallel_tool_calls", json!(false)),
            (
                "prompt_cache_options",
                json!({"mode": "explicit", "retention": "30m"}),
            ),
            (
                "text",
                json!({"format": {"type": "json_schema", "name": "result"}}),
            ),
        ] {
            let mut unsupported = base_request("gpt-5.6-sol");
            unsupported[field] = value;
            assert!(
                translated_request(&unsupported, "gpt-5.6-sol").is_err(),
                "{field} must fail explicitly instead of being silently ignored"
            );
        }

        for reasoning in [
            json!({"effort": "high", "context": "all_turns"}),
            json!({"effort": "high", "context": "current_turn"}),
            json!({"effort": "high", "context": "auto"}),
        ] {
            let mut unsupported = base_request("gpt-5.6-sol");
            unsupported["reasoning"] = reasoning;
            assert!(translated_request(&unsupported, "gpt-5.6-sol").is_err());
        }

        for invalid_id in ["foo", "resp_", "resp_contains.dot"] {
            let mut unsupported = base_request("gpt-5.6-sol");
            unsupported["previous_response_id"] = json!(invalid_id);
            assert!(
                translated_request(&unsupported, "gpt-5.6-sol").is_err(),
                "invalid previous_response_id must not be silently treated as absent"
            );
        }
    }

    #[test]
    fn continuation_replays_visible_messages_without_inheriting_old_instructions() {
        let first = json!({
            "model": "gpt-5.6-sol",
            "instructions": "OLD INSTRUCTIONS MUST NOT CARRY",
            "input": "first turn",
            "store": true
        });
        let (first_translated, _) =
            translated_request(&first, "gpt-5.6-sol").expect("first request");
        let mut prior_messages = first_translated.messages;
        prior_messages.push(Message {
            role: "assistant".to_string(),
            content: json!([{"type": "text", "text": "first answer"}]),
        });
        let prior = StoredConversation {
            model: "gpt-5.6-sol".to_string(),
            session_id: "00000000-0000-4000-8000-000000000123".to_string(),
            messages: prior_messages,
        };

        let second = json!({
            "model": "gpt-5.6-sol",
            "instructions": "NEW INSTRUCTIONS",
            "input": "second turn",
            "previous_response_id": "resp_first"
        });
        let (mut second_translated, _) =
            translated_request(&second, "gpt-5.6-sol").expect("second request");
        let session_id = apply_conversation_state(&mut second_translated, Some(prior));

        assert_eq!(session_id, "00000000-0000-4000-8000-000000000123");
        assert_eq!(
            second_translated.system.as_ref().unwrap()[0].text,
            "NEW INSTRUCTIONS"
        );
        assert_eq!(second_translated.messages.len(), 3);
        assert_eq!(second_translated.messages[0].content, "first turn");
        assert_eq!(second_translated.messages[1].role, "assistant");
        assert_eq!(second_translated.messages[2].content, "second turn");
        assert!(
            !serde_json::to_string(&second_translated)
                .unwrap()
                .contains("OLD INSTRUCTIONS MUST NOT CARRY")
        );
    }

    #[tokio::test]
    async fn stateful_validation_is_model_bound_stream_safe_and_app_state_isolated() {
        let first_state = AppState::new("same-api-key", true, true);
        first_state
            .response_store
            .insert(
                "resp_stored".to_string(),
                StoredConversation {
                    model: "gpt-5.6-sol".to_string(),
                    session_id: "00000000-0000-4000-8000-000000000321".to_string(),
                    messages: vec![Message {
                        role: "user".to_string(),
                        content: json!("stored turn"),
                    }],
                },
            )
            .unwrap();

        let mut streamed = base_request("gpt-5.6-sol");
        streamed["stream"] = json!(true);
        streamed["store"] = json!(true);
        let response = post_responses(State(first_state.clone()), ResponsesJson(streamed)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let mut wrong_model = base_request("gpt-5.6-terra");
        wrong_model["previous_response_id"] = json!("resp_stored");
        let response = post_responses(State(first_state.clone()), ResponsesJson(wrong_model)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let second_state = AppState::new("same-api-key", true, true);
        let mut isolated = base_request("gpt-5.6-sol");
        isolated["previous_response_id"] = json!("resp_stored");
        let response = post_responses(State(second_state), ResponsesJson(isolated)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let mut valid = base_request("gpt-5.6-sol");
        valid["previous_response_id"] = json!("resp_stored");
        let response = post_responses(State(first_state), ResponsesJson(valid)).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a valid continuation must proceed to the real model path"
        );
    }

    #[test]
    fn rejects_unknown_fields_and_unrepresentable_multimodal_inputs() {
        for request in [
            json!({"model": "gpt-5.6-sol", "input": "hi", "temperature": 0.5}),
            json!({
                "model": "gpt-5.6-sol",
                "input": [{
                    "role": "user",
                    "content": [{"type": "input_image", "file_id": "file_123"}]
                }]
            }),
            json!({
                "model": "gpt-5.6-sol",
                "input": [{
                    "role": "user",
                    "content": [{"type": "input_file", "file_url": "https://example.test/a.pdf"}]
                }]
            }),
            json!({
                "model": "gpt-5.6-sol",
                "input": [{
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": "data:image/png;base64,aGVsbG8=",
                        "detail": "high"
                    }]
                }]
            }),
            json!({
                "model": "gpt-5.6-sol",
                "input": [{
                    "role": "user",
                    "content": [{
                        "type": "input_file",
                        "file_data": "data:application/zip;base64,aGVsbG8="
                    }]
                }]
            }),
            json!({
                "model": "gpt-5.6-sol",
                "input": [{
                    "role": "user",
                    "content": [{
                        "type": "input_file",
                        "file_data": "data:application/pdf;base64,aGVsbG8=",
                        "filename": "../report.pdf"
                    }]
                }]
            }),
        ] {
            assert!(translated_request(&request, "gpt-5.6-sol").is_err());
        }
    }

    #[test]
    fn base64_image_and_document_reach_the_real_kiro_wire_request() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "role": "user",
                "content": [
                    {
                        "type": "input_image",
                        "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"
                    },
                    {
                        "type": "input_file",
                        "file_data": "data:application/pdf;base64,JVBERi0xLjQK",
                        "filename": "report.pdf"
                    }
                ]
            }]
        });
        let (translated, _) =
            translated_request(&request, "gpt-5.6-sol").expect("multimodal translation");
        assert_eq!(translated.messages[0].content[1]["name"], "report.pdf");
        let converted =
            super::super::converter::convert_request(&translated).expect("Kiro conversion");
        let wire = converted
            .conversation_state
            .current_message
            .user_input_message;
        assert_eq!(wire.model_id, "gpt-5.6-sol");
        assert_eq!(wire.images.len(), 1);
        assert_eq!(wire.images[0].format, "png");
        assert_eq!(
            wire.images[0].source.bytes,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB"
        );
        assert_eq!(wire.documents.len(), 1);
        assert_eq!(wire.documents[0].format, "pdf");
        assert_eq!(wire.documents[0].name, "report pdf");
        assert_eq!(wire.documents[0].source.bytes, "JVBERi0xLjQK");

        let remote = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": "https://images.example.test/photo.png",
                    "detail": "auto"
                }]
            }]
        });
        let (translated_remote, _) =
            translated_request(&remote, "gpt-5.6-sol").expect("remote image translation");
        assert_eq!(
            translated_remote.messages[0].content[0]["source"],
            json!({
                "type": "url",
                "url": "https://images.example.test/photo.png"
            })
        );
    }

    #[test]
    fn maps_nonstream_text_tool_and_clean_usage() {
        let meta = base_meta("gpt-5.6-luna");
        let mapped = anthropic_to_response(
            &json!({
                "id": "msg_bdrk_private",
                "model": "some-other-model",
                "content": [
                    {"type": "text", "text": "Checking."},
                    {
                        "type": "tool_use",
                        "id": "toolu_bdrk_private",
                        "name": "weather",
                        "input": {"city": "Paris"}
                    }
                ],
                "stop_reason": "tool_use",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 2,
                    "output_tokens": 5,
                    "output_tokens_details": {"thinking_tokens": 3}
                }
            }),
            &meta,
        )
        .expect("mapped response");
        let assistant = mapped.assistant_message.expect("stored assistant message");
        let response = mapped.response;
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["model"], "gpt-5.6-luna");
        assert!(response["id"].as_str().unwrap().starts_with("resp_"));
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(
            response["output"][0]["content"][0],
            json!({
                "type": "output_text",
                "text": "Checking.",
                "annotations": [],
                "logprobs": []
            })
        );
        assert_eq!(response["output"][1]["type"], "function_call");
        assert!(
            response["output"][1]["id"]
                .as_str()
                .unwrap()
                .starts_with("fc_")
        );
        assert!(
            response["output"][1]["call_id"]
                .as_str()
                .unwrap()
                .starts_with("call_")
        );
        assert_eq!(
            assistant.content[1]["id"], response["output"][1]["call_id"],
            "stored tool history must use the same public call id accepted by function_call_output"
        );
        assert_eq!(response["usage"]["input_tokens"], 12);
        assert_eq!(response["usage"]["output_tokens"], 5);
        assert_eq!(response["usage"]["total_tokens"], 17);
        let encoded = response.to_string().to_ascii_lowercase();
        assert!(!encoded.contains("bdrk"));
        assert!(!encoded.contains("claude"));
        assert!(!encoded.contains("anthropic"));
        assert!(!encoded.contains("kiro"));
        assert!(!encoded.contains("aws"));
        assert!(!encoded.contains("[done]"));
    }

    #[test]
    fn transforms_text_stream_to_responses_events_without_chat_done_marker() {
        let meta = base_meta("gpt-5.6-sol");
        let mut state = ResponsesStreamState::new(meta);
        let events = [
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 4, "output_tokens": 0}}}),
            ),
            (
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
            ),
            ("content_block_stop", json!({"index": 0})),
            (
                "message_delta",
                json!({"usage": {"output_tokens": 2}, "delta": {"stop_reason": "end_turn"}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let output = events
            .iter()
            .map(|(name, event)| state.transform(name, event))
            .collect::<String>();
        for required in [
            "event: response.created",
            "event: response.in_progress",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
            "event: response.output_text.done",
            "event: response.content_part.done",
            "event: response.output_item.done",
            "event: response.completed",
        ] {
            assert!(output.contains(required), "{output}");
        }
        assert!(!output.contains("chat.completion"), "{output}");
        assert!(!output.contains("[DONE]"), "{output}");
        let positions: Vec<_> = [
            "event: response.created",
            "event: response.in_progress",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
            "event: response.output_text.done",
            "event: response.content_part.done",
            "event: response.output_item.done",
            "event: response.completed",
        ]
        .iter()
        .map(|needle| output.find(needle).unwrap())
        .collect();
        assert!(
            positions
                .windows(2)
                .all(|positions| positions[0] < positions[1])
        );
        let parsed = parsed_events(&output);
        for (sequence, (event_name, event)) in parsed.iter().enumerate() {
            assert_eq!(event["type"], *event_name, "{event}");
            assert_eq!(event["sequence_number"], sequence, "{event}");
        }
    }

    #[test]
    fn transforms_function_stream_with_responses_function_events() {
        let meta = base_meta("gpt-5.6-terra");
        let mut state = ResponsesStreamState::new(meta);
        let output = [
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 5}}}),
            ),
            (
                "content_block_start",
                json!({
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_private",
                        "name": "weather",
                        "input": {}
                    }
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"city\":\"Paris\"}"
                    }
                }),
            ),
            ("content_block_stop", json!({"index": 0})),
            ("message_stop", json!({})),
        ]
        .iter()
        .map(|(name, event)| state.transform(name, event))
        .collect::<String>();
        assert!(output.contains("response.function_call_arguments.delta"));
        assert!(output.contains("response.function_call_arguments.done"));
        assert!(output.contains("\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\""));
        assert!(!output.contains("toolu_private"));
        assert!(!output.contains("[DONE]"));

        let events = parsed_events(&output);
        let added = events
            .iter()
            .find(|(name, _)| name == "response.output_item.added")
            .map(|(_, event)| event)
            .unwrap();
        let delta = events
            .iter()
            .find(|(name, _)| name == "response.function_call_arguments.delta")
            .map(|(_, event)| event)
            .unwrap();
        let done = events
            .iter()
            .find(|(name, _)| name == "response.function_call_arguments.done")
            .map(|(_, event)| event)
            .unwrap();
        let item_done = events
            .iter()
            .find(|(name, _)| name == "response.output_item.done")
            .map(|(_, event)| event)
            .unwrap();
        let item_id = added["item"]["id"].as_str().unwrap();
        assert!(item_id.starts_with("fc_"));
        assert!(
            added["item"]["call_id"]
                .as_str()
                .unwrap()
                .starts_with("call_")
        );
        assert_eq!(delta["item_id"], item_id);
        assert_eq!(done["item_id"], item_id);
        assert_eq!(item_done["item"]["id"], item_id);
        assert_eq!(done["arguments"], item_done["item"]["arguments"]);
        serde_json::from_str::<Value>(done["arguments"].as_str().unwrap())
            .expect("final function arguments must be JSON");
    }

    #[test]
    fn max_tokens_stream_finishes_with_canonical_incomplete_event() {
        let meta = base_meta("gpt-5.6-luna");
        let mut state = ResponsesStreamState::new(meta);
        let output = [
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 3}}}),
            ),
            (
                "message_delta",
                json!({
                    "delta": {"stop_reason": "max_tokens"},
                    "usage": {"output_tokens": 64}
                }),
            ),
            ("message_stop", json!({})),
        ]
        .iter()
        .map(|(name, event)| state.transform(name, event))
        .collect::<String>();
        assert!(output.contains("event: response.incomplete"), "{output}");
        assert!(!output.contains("event: response.completed"), "{output}");
        let incomplete = parsed_events(&output)
            .into_iter()
            .find(|(name, _)| name == "response.incomplete")
            .map(|(_, event)| event)
            .unwrap();
        assert_eq!(incomplete["response"]["status"], "incomplete");
        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn upstream_stream_error_is_clean_responses_error_without_done_marker() {
        let meta = base_meta("gpt-5.6-sol");
        let mut state = ResponsesStreamState::new(meta);
        let output = format!(
            "{}{}",
            state.transform(
                "message_start",
                &json!({"message": {"usage": {"input_tokens": 1}}})
            ),
            state.transform(
                "error",
                &json!({"error": {"message": "private provider detail"}})
            )
        );
        assert!(output.contains("event: error"), "{output}");
        assert!(!output.contains("[DONE]"), "{output}");
        let lower = output.to_ascii_lowercase();
        for forbidden in [
            "anthropic",
            "claude",
            "kiro",
            "bedrock",
            "bdrk",
            "aws",
            "provider",
            "toolu_",
            "msg_bdrk",
            "chatcmpl",
        ] {
            assert!(!lower.contains(forbidden), "{output}");
        }
        let error = parsed_events(&output)
            .into_iter()
            .find(|(name, _)| name == "error")
            .map(|(_, event)| event)
            .unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(error["code"], "server_error");
        assert!(error["sequence_number"].is_u64());
    }

    #[tokio::test]
    async fn gpt_validation_and_backend_errors_are_marked_and_clean() {
        let state = AppState::new("test-key", true, true);
        for model in GPT56_MODELS {
            let invalid = post_responses(
                State(state.clone()),
                ResponsesJson(json!({"model": model, "input": "hi", "temperature": 0.5})),
            )
            .await;
            assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
            assert!(
                invalid
                    .extensions()
                    .get::<super::super::middleware::GptOpenAiResponse>()
                    .is_some()
            );
            let body = body_json(invalid).await;
            assert_eq!(body["error"]["type"], "invalid_request_error");

            let backend =
                post_responses(State(state.clone()), ResponsesJson(base_request(model))).await;
            assert_eq!(backend.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                backend
                    .extensions()
                    .get::<super::super::middleware::GptOpenAiResponse>()
                    .is_some()
            );
            let encoded = body_json(backend).await.to_string().to_ascii_lowercase();
            for forbidden in ["anthropic", "claude", "kiro", "bedrock", "bdrk", "aws"] {
                assert!(!encoded.contains(forbidden), "{encoded}");
            }
        }
    }

    #[tokio::test]
    async fn non_gpt_keeps_legacy_500_contract() {
        let state = AppState::new("test-key", true, true);
        let response = post_responses(
            State(state),
            ResponsesJson(json!({"model": "claude-opus-4-8", "input": "hello"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "convert_request_failed"
        );
    }

    #[tokio::test]
    async fn routed_gpt_responses_error_has_clean_openai_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test router");
        let address = listener.local_addr().expect("test address");
        let app = super::super::router::create_router_with_provider("test-key", None, true, true);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test router");
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("HTTP client");
        let response = client
            .post(format!("http://{address}/v1/responses"))
            .header("authorization", "Bearer test-key")
            .json(&base_request("gpt-5.6-sol"))
            .send()
            .await
            .expect("Responses request");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        for forbidden in [
            "x-new-api-version",
            "x-oneapi-request-id",
            "server",
            "via",
            "alt-svc",
        ] {
            assert!(
                response.headers().get(forbidden).is_none(),
                "{forbidden}: {:?}",
                response.headers()
            );
        }

        let auth = client
            .post(format!("http://{address}/v1/responses"))
            .json(&base_request("gpt-5.6-sol"))
            .send()
            .await
            .expect("unauthenticated Responses request");
        assert_eq!(auth.status(), StatusCode::UNAUTHORIZED);
        for forbidden in ["x-new-api-version", "x-oneapi-request-id", "server", "via"] {
            assert!(auth.headers().get(forbidden).is_none(), "{forbidden}");
        }
        let auth_body: Value = auth.json().await.expect("auth JSON");
        assert_eq!(auth_body["error"]["type"], "authentication_error");
        assert_eq!(auth_body["error"]["code"], "invalid_api_key");

        let malformed = client
            .post(format!("http://{address}/v1/responses"))
            .header("authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .expect("malformed Responses request");
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        for forbidden in ["x-new-api-version", "x-oneapi-request-id", "server", "via"] {
            assert!(malformed.headers().get(forbidden).is_none(), "{forbidden}");
        }
        let malformed_body: Value = malformed.json().await.expect("malformed JSON error");
        assert_eq!(
            malformed_body["error"]["message"],
            "Invalid JSON request body."
        );
        assert_eq!(malformed_body["error"]["type"], "invalid_request_error");
        server.abort();
    }
}
