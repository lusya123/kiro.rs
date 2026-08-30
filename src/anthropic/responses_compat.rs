//! GPT-5.6-only OpenAI Responses API compatibility.
//!
//! This adapter deliberately reuses `post_messages`, so the requested GPT-5.6
//! model id, reasoning settings, tools, identity handling, and no-fallback
//! routing all go through the same real upstream path as `/v1/messages`.
//! Non-GPT requests retain the historical `/v1/responses` behavior.

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

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
    response_store::{ResponseStore, StoreError, StoredConversation},
    types::{Message, MessagesRequest, Metadata, ReasoningConfig, SystemMessage, Tool},
};

const GPT56_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];
const GPT56_DEFAULT_MODEL: &str = "gpt-5.6-sol";
const RESPONSES_BODY_LIMIT: usize = 50 * 1024 * 1024;
const RESPONSES_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(4);
const RESPONSES_KEEP_ALIVE: Bytes = Bytes::from_static(b": keep-alive\n\n");
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

fn canonical_gpt56_model(model: &str) -> Option<&'static str> {
    match model.trim().to_ascii_lowercase().as_str() {
        "gpt-5.6" | "gpt 5.6" | "gpt-5.6-sol" | "gpt 5.6 sol" => Some(GPT56_DEFAULT_MODEL),
        "gpt-5.6-terra" | "gpt 5.6 terra" => Some("gpt-5.6-terra"),
        "gpt-5.6-luna" | "gpt 5.6 luna" => Some("gpt-5.6-luna"),
        _ => None,
    }
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
                | "prompt_cache_key"
                | "client_metadata"
                | "service_tier"
                | "stream_options"
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
        Some(Value::Array(values)) => {
            for (index, value) in values.iter().enumerate() {
                match value.as_str() {
                    Some("reasoning.encrypted_content") => {}
                    Some(value) => {
                        return Err(format!(
                            "`include[{index}]` value `{value}` is not supported by this endpoint"
                        ));
                    }
                    None => return Err(format!("`include[{index}]` must be a string")),
                }
            }
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
        None | Some(Value::Bool(_)) => {}
        Some(_) => return Err("`parallel_tool_calls` must be a boolean".to_string()),
    }
    match object.get("prompt_cache_key") {
        None | Some(Value::Null | Value::String(_)) => {}
        Some(_) => return Err("`prompt_cache_key` must be a string".to_string()),
    }
    match object.get("client_metadata") {
        None | Some(Value::Null) => {}
        Some(Value::Object(metadata)) => {
            if let Some((key, _)) = metadata.iter().find(|(_, value)| !value.is_string()) {
                return Err(format!("`client_metadata.{key}` must be a string"));
            }
        }
        Some(_) => return Err("`client_metadata` must be an object".to_string()),
    }
    match object.get("service_tier") {
        None | Some(Value::Null | Value::String(_)) => {}
        Some(_) => return Err("`service_tier` must be a string".to_string()),
    }
    match object.get("stream_options") {
        None | Some(Value::Null | Value::Object(_)) => {}
        Some(_) => return Err("`stream_options` must be an object".to_string()),
    }
    Ok(())
}

fn responses_text_format_instruction(text: Option<&Value>) -> Result<Option<String>, String> {
    let text = match text {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(text)) => text,
        Some(_) => return Err("`text` must be an object".to_string()),
    };
    for key in text.keys() {
        if !matches!(key.as_str(), "format" | "verbosity") {
            return Err(format!("`text.{key}` is not supported"));
        }
    }
    match text.get("verbosity") {
        None | Some(Value::Null) => {}
        Some(Value::String(value)) if matches!(value.as_str(), "low" | "medium" | "high") => {}
        Some(Value::String(_)) => {
            return Err("`text.verbosity` must be one of: low, medium, high".to_string());
        }
        Some(_) => return Err("`text.verbosity` must be a string".to_string()),
    }

    let format = match text.get("format") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(format)) => format,
        Some(_) => return Err("`text.format` must be an object".to_string()),
    };
    let format_type = format
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "`text.format.type` must be a string".to_string())?;
    match format_type {
        "text" => {
            for key in format.keys() {
                if !matches!(key.as_str(), "type" | "strict") {
                    return Err(format!("`text.format.{key}` is not supported"));
                }
            }
            match format.get("strict") {
                None | Some(Value::Null | Value::Bool(_)) => {}
                Some(_) => return Err("`text.format.strict` must be a boolean".to_string()),
            }
            Ok(None)
        }
        "json_schema" => {
            for key in format.keys() {
                if !matches!(
                    key.as_str(),
                    "type" | "name" | "description" | "schema" | "strict"
                ) {
                    return Err(format!("`text.format.{key}` is not supported"));
                }
            }
            let name = format
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "`text.format.name` must be a non-empty string".to_string())?;
            let description = match format.get("description") {
                None | Some(Value::Null) => None,
                Some(Value::String(description)) => Some(description.as_str()),
                Some(_) => return Err("`text.format.description` must be a string".to_string()),
            };
            match format.get("strict") {
                None | Some(Value::Null | Value::Bool(_)) => {}
                Some(_) => return Err("`text.format.strict` must be a boolean".to_string()),
            }
            let schema = format
                .get("schema")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| "`text.format.schema` must be an object".to_string())?;
            if schema.get("type").and_then(Value::as_str) == Some("object")
                && schema.get("additionalProperties") != Some(&Value::Bool(false))
            {
                return Err(
                    "`text.format.schema` object must explicitly set `additionalProperties` to false"
                        .to_string(),
                );
            }

            let mut instruction = format!("Structured output format `{name}`.");
            if let Some(description) = description {
                instruction.push(' ');
                instruction.push_str(description);
            }
            instruction.push_str("\n\n");
            instruction.push_str(&super::bedrock::structured_output_instruction(schema));
            Ok(Some(instruction))
        }
        _ => Err(format!(
            "`text.format.type` value `{format_type}` is not supported by this endpoint"
        )),
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoreMode {
    Disabled,
    Implicit,
    Required,
}

impl StoreMode {
    fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    fn disable_if_implicit(&mut self) -> bool {
        if matches!(self, Self::Implicit) {
            *self = Self::Disabled;
            true
        } else {
            false
        }
    }
}

fn store_requested(request: &Value) -> Result<StoreMode, String> {
    match request.get("store") {
        // Match the Responses API default. Codex explicitly sends `store: false`,
        // while clients that omit the field expect the returned response id to be
        // usable with `previous_response_id`.
        None | Some(Value::Null) => Ok(StoreMode::Implicit),
        Some(Value::Bool(true)) => Ok(StoreMode::Required),
        Some(Value::Bool(false)) => Ok(StoreMode::Disabled),
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
    if reasoning.is_null() {
        return Ok(None);
    }
    let reasoning = reasoning
        .as_object()
        .ok_or_else(|| "`reasoning` must be an object".to_string())?;
    for key in reasoning.keys() {
        if !matches!(key.as_str(), "effort" | "mode" | "context" | "summary") {
            return Err(format!("`reasoning.{key}` is not supported"));
        }
    }
    match reasoning.get("context") {
        None | Some(Value::Null) => {}
        Some(Value::String(value))
            if matches!(value.as_str(), "auto" | "current_turn" | "all_turns") => {}
        Some(Value::String(_)) => {
            return Err(
                "`reasoning.context` must be one of: auto, current_turn, all_turns".to_string(),
            );
        }
        Some(_) => return Err("`reasoning.context` must be a string".to_string()),
    }
    match reasoning.get("summary") {
        None | Some(Value::Null) => {}
        Some(Value::String(value))
            if matches!(value.as_str(), "auto" | "concise" | "detailed" | "none") => {}
        Some(Value::String(_)) => {
            return Err(
                "`reasoning.summary` must be one of: auto, concise, detailed, none".to_string(),
            );
        }
        Some(_) => return Err("`reasoning.summary` must be a string".to_string()),
    }
    let effort = optional_string(reasoning.get("effort"), "reasoning.effort")?;
    let mode = optional_string(reasoning.get("mode"), "reasoning.mode")?;
    if effort.is_none() && mode.is_none() {
        return Ok(None);
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ResponseToolKind {
    Function,
    Custom,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PublicToolKey {
    kind: ResponseToolKind,
    namespace: Option<String>,
    name: String,
}

impl PublicToolKey {
    fn function(namespace: Option<&str>, name: &str) -> Self {
        Self {
            kind: ResponseToolKind::Function,
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn custom(namespace: Option<&str>, name: &str) -> Self {
        Self {
            kind: ResponseToolKind::Custom,
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct ParsedResponseTool {
    key: PublicToolKey,
    description: String,
    input_schema: HashMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
struct ToolCatalog {
    anthropic_tools: Vec<Tool>,
    by_kiro_name: HashMap<String, PublicToolKey>,
    by_public_key: HashMap<PublicToolKey, String>,
}

impl ToolCatalog {
    fn kiro_name(&self, key: &PublicToolKey) -> Option<&str> {
        self.by_public_key.get(key).map(String::as_str)
    }

    fn public_key(&self, kiro_name: &str) -> PublicToolKey {
        self.by_kiro_name
            .get(kiro_name)
            .cloned()
            .unwrap_or_else(|| PublicToolKey::function(None, kiro_name))
    }

    fn tools(&self) -> Option<Vec<Tool>> {
        (!self.anthropic_tools.is_empty()).then(|| self.anthropic_tools.clone())
    }
}

fn non_empty_tool_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("`{path}.{key}` must be a non-empty string"))
}

fn tool_description(object: &Map<String, Value>, name: &str, path: &str) -> Result<String, String> {
    match object.get("description") {
        None | Some(Value::Null) => Ok(name.to_string()),
        Some(Value::String(description)) if description.trim().is_empty() => Ok(name.to_string()),
        Some(Value::String(description)) => Ok(description.clone()),
        Some(_) => Err(format!("`{path}.description` must be a string")),
    }
}

fn validate_strict_and_deferred_fields(
    object: &Map<String, Value>,
    path: &str,
    accept_strict_hint: bool,
) -> Result<(), String> {
    match object.get("strict") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) if accept_strict_hint => {}
        Some(Value::Bool(true)) => {
            return Err(format!(
                "`{path}.strict`: strict schema enforcement is not supported"
            ));
        }
        Some(_) => return Err(format!("`{path}.strict` must be a boolean")),
    }
    match object.get("defer_loading") {
        None | Some(Value::Null | Value::Bool(_)) => Ok(()),
        Some(_) => Err(format!("`{path}.defer_loading` must be a boolean")),
    }
}

fn parse_function_tool(
    object: &Map<String, Value>,
    path: &str,
    namespace: Option<&str>,
    namespace_description: Option<&str>,
    accept_strict_hint: bool,
) -> Result<ParsedResponseTool, String> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type" | "name" | "description" | "parameters" | "strict" | "defer_loading"
        ) {
            return Err(format!("`{path}.{key}` is not supported"));
        }
    }
    validate_strict_and_deferred_fields(object, path, accept_strict_hint)?;
    let name = non_empty_tool_string(object, "name", path)?;
    let mut description = tool_description(object, name, path)?;
    if let Some(namespace_description) = namespace_description.filter(|value| !value.is_empty()) {
        description = format!("{namespace_description}\n\n{description}");
    }
    let parameters = object
        .get("parameters")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("`{path}.parameters` must be an object"))?;
    Ok(ParsedResponseTool {
        key: PublicToolKey::function(namespace, name),
        description,
        input_schema: parameters
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

fn parse_custom_tool(
    object: &Map<String, Value>,
    path: &str,
    namespace: Option<&str>,
    namespace_description: Option<&str>,
) -> Result<ParsedResponseTool, String> {
    for key in object.keys() {
        if !matches!(key.as_str(), "type" | "name" | "description" | "format") {
            return Err(format!("`{path}.{key}` is not supported"));
        }
    }
    let name = non_empty_tool_string(object, "name", path)?;
    let mut description = tool_description(object, name, path)?;
    if let Some(namespace_description) = namespace_description.filter(|value| !value.is_empty()) {
        description = format!("{namespace_description}\n\n{description}");
    }
    let format = object
        .get("format")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("`{path}.format` must be an object"))?;
    for key in format.keys() {
        if !matches!(key.as_str(), "type" | "syntax" | "definition") {
            return Err(format!("`{path}.format.{key}` is not supported"));
        }
    }
    let format_type = non_empty_tool_string(format, "type", &format!("{path}.format"))?;
    let syntax = non_empty_tool_string(format, "syntax", &format!("{path}.format"))?;
    let definition = non_empty_tool_string(format, "definition", &format!("{path}.format"))?;
    description.push_str(&format!(
        "\n\nThis is a freeform tool. Put the complete tool input in the `input` string field. Format: {format_type}/{syntax}.\n{definition}"
    ));
    let schema = json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "The complete freeform input for this custom tool."
            }
        },
        "required": ["input"],
        "additionalProperties": false
    });
    Ok(ParsedResponseTool {
        key: PublicToolKey::custom(namespace, name),
        description,
        input_schema: schema
            .as_object()
            .expect("custom tool schema is an object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

fn parse_tool_value(
    tool: &Value,
    path: &str,
    namespace: Option<&str>,
    namespace_description: Option<&str>,
    accept_strict_hint: bool,
) -> Result<Vec<ParsedResponseTool>, String> {
    let object = tool
        .as_object()
        .ok_or_else(|| format!("`{path}` must be an object"))?;
    match object.get("type").and_then(Value::as_str) {
        Some("web_search") if namespace.is_none() => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type" | "external_web_access" | "search_content_types"
                ) {
                    return Err(format!("`{path}.{key}` is not supported"));
                }
            }
            if let Some(content_types) = object.get("search_content_types") {
                let content_types = content_types
                    .as_array()
                    .ok_or_else(|| format!("`{path}.search_content_types` must be an array"))?;
                if content_types.is_empty()
                    || content_types.iter().any(|content_type| {
                        !matches!(content_type.as_str(), Some("text" | "image"))
                    })
                {
                    return Err(format!(
                        "`{path}.search_content_types` must contain only `text` or `image`"
                    ));
                }
            }
            match object.get("external_web_access") {
                Some(Value::Bool(false)) => {
                    tracing::debug!(
                        tool_path = path,
                        "cached Responses hosted web search is unavailable and will not be forwarded to Kiro"
                    );
                    Ok(Vec::new())
                }
                Some(Value::Bool(true)) => Err(format!(
                    "`{path}` requests hosted web search, which this endpoint cannot execute"
                )),
                Some(_) => Err(format!("`{path}.external_web_access` must be a boolean")),
                None => Err(format!(
                    "`{path}.external_web_access` must be false; hosted web search is not supported"
                )),
            }
        }
        Some("function") => Ok(vec![parse_function_tool(
            object,
            path,
            namespace,
            namespace_description,
            accept_strict_hint,
        )?]),
        Some("custom") => Ok(vec![parse_custom_tool(
            object,
            path,
            namespace,
            namespace_description,
        )?]),
        Some("namespace") if namespace.is_none() => {
            for key in object.keys() {
                if !matches!(key.as_str(), "type" | "name" | "description" | "tools") {
                    return Err(format!("`{path}.{key}` is not supported"));
                }
            }
            let namespace = non_empty_tool_string(object, "name", path)?;
            let namespace_description = tool_description(object, namespace, path)?;
            let tools = object
                .get("tools")
                .and_then(Value::as_array)
                .filter(|tools| !tools.is_empty())
                .ok_or_else(|| format!("`{path}.tools` must be a non-empty array"))?;
            let mut parsed = Vec::new();
            for (index, tool) in tools.iter().enumerate() {
                parsed.extend(parse_tool_value(
                    tool,
                    &format!("{path}.tools[{index}]"),
                    Some(namespace),
                    Some(&namespace_description),
                    accept_strict_hint,
                )?);
            }
            Ok(parsed)
        }
        Some("namespace") => Err(format!("nested namespace `{path}` is not supported")),
        Some(tool_type) => Err(format!(
            "`{path}.type` value `{tool_type}` is not supported"
        )),
        None => Err(format!("`{path}.type` must be a string")),
    }
}

fn stable_tool_hash(key: &PublicToolKey) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    let kind = match key.kind {
        ResponseToolKind::Function => "function",
        ResponseToolKind::Custom => "custom",
    };
    for byte in kind
        .bytes()
        .chain(std::iter::once(0))
        .chain(key.namespace.as_deref().unwrap_or("").bytes())
        .chain(std::iter::once(0))
        .chain(key.name.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn sanitized_tool_fragment(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_separator = false;
    for character in value.chars() {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            character
        } else {
            '_'
        };
        if character == '_' && last_was_separator {
            continue;
        }
        last_was_separator = character == '_';
        output.push(character);
    }
    let output = output.trim_matches('_');
    if output.is_empty() {
        "tool".to_string()
    } else {
        output.to_string()
    }
}

fn generated_kiro_tool_name(key: &PublicToolKey, used: &HashSet<String>) -> String {
    const MAX_LEN: usize = 63;
    let prefix = match (key.kind, key.namespace.is_some()) {
        (ResponseToolKind::Custom, false) => "r_custom_",
        (ResponseToolKind::Function, true) => "r_namespace_",
        (ResponseToolKind::Custom, true) => "r_ns_custom_",
        (ResponseToolKind::Function, false) => "r_function_",
    };
    let label = sanitized_tool_fragment(&format!(
        "{}__{}",
        key.namespace.as_deref().unwrap_or(""),
        key.name
    ));
    let hash = stable_tool_hash(key);
    for collision in 0_u32.. {
        let suffix = if collision == 0 {
            format!("_{hash:016x}")
        } else {
            format!("_{hash:016x}_{collision}")
        };
        let label_len = MAX_LEN.saturating_sub(prefix.len() + suffix.len());
        let candidate = format!("{prefix}{}{suffix}", &label[..label.len().min(label_len)]);
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("tool collision counter is unbounded")
}

fn tool_catalog(request: &Value, accept_strict_hint: bool) -> Result<ToolCatalog, String> {
    let mut parsed = Vec::new();
    if let Some(tools) = request.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| "`tools` must be an array".to_string())?;
        for (index, tool) in tools.iter().enumerate() {
            parsed.extend(parse_tool_value(
                tool,
                &format!("tools[{index}]"),
                None,
                None,
                accept_strict_hint,
            )?);
        }
    }
    if let Some(items) = request.get("input").and_then(Value::as_array) {
        for (item_index, item) in items.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                continue;
            }
            let object = item
                .as_object()
                .ok_or_else(|| format!("`input[{item_index}]` must be an object"))?;
            for key in object.keys() {
                if !matches!(key.as_str(), "type" | "id" | "role" | "tools") {
                    return Err(format!(
                        "additional tools input field `{key}` is not supported"
                    ));
                }
            }
            if object.get("role").and_then(Value::as_str) != Some("developer") {
                return Err("additional tools input requires `role: \"developer\"`".to_string());
            }
            let tools = object
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| "additional tools input requires array `tools`".to_string())?;
            for (tool_index, tool) in tools.iter().enumerate() {
                parsed.extend(parse_tool_value(
                    tool,
                    &format!("input[{item_index}].tools[{tool_index}]"),
                    None,
                    None,
                    accept_strict_hint,
                )?);
            }
        }
    }

    let mut used_names = parsed
        .iter()
        .filter(|tool| tool.key.kind == ResponseToolKind::Function && tool.key.namespace.is_none())
        .map(|tool| tool.key.name.clone())
        .collect::<HashSet<_>>();
    let mut catalog = ToolCatalog::default();
    for tool in parsed {
        if catalog.by_public_key.contains_key(&tool.key) {
            return Err(format!(
                "duplicate tool definition for `{}`{}",
                tool.key.name,
                tool.key
                    .namespace
                    .as_deref()
                    .map(|namespace| format!(" in namespace `{namespace}`"))
                    .unwrap_or_default()
            ));
        }
        let kiro_name =
            if tool.key.kind == ResponseToolKind::Function && tool.key.namespace.is_none() {
                tool.key.name.clone()
            } else {
                generated_kiro_tool_name(&tool.key, &used_names)
            };
        if catalog.by_kiro_name.contains_key(&kiro_name) {
            return Err(format!(
                "tool name `{kiro_name}` conflicts with another tool"
            ));
        }
        used_names.insert(kiro_name.clone());
        catalog.anthropic_tools.push(Tool {
            tool_type: None,
            name: kiro_name.clone(),
            description: tool.description,
            input_schema: tool.input_schema,
            strict: None,
            max_uses: None,
            cache_control: None,
        });
        catalog
            .by_kiro_name
            .insert(kiro_name.clone(), tool.key.clone());
        catalog.by_public_key.insert(tool.key, kiro_name);
    }
    Ok(catalog)
}

fn response_tool_definitions(request: &Value) -> Value {
    let mut tools = request
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(items) = request.get("input").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                tools.extend(
                    item.get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }
    }
    Value::Array(tools)
}

fn optional_tool_namespace<'a>(
    object: &'a Map<String, Value>,
    path: &str,
) -> Result<Option<&'a str>, String> {
    match object.get("namespace") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(namespace)) if !namespace.trim().is_empty() => {
            Ok(Some(namespace.as_str()))
        }
        Some(Value::String(_)) => Err(format!("`{path}.namespace` must not be empty")),
        Some(_) => Err(format!("`{path}.namespace` must be a string")),
    }
}

fn validate_encrypted_function_args(object: &Map<String, Value>, path: &str) -> Result<(), String> {
    match object.get("encrypted_function_args") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(()),
        Some(Value::Array(_)) => Err(format!(
            "`{path}.encrypted_function_args` must contain only strings"
        )),
        Some(_) => Err(format!("`{path}.encrypted_function_args` must be an array")),
    }
}

fn tool_output_to_anthropic(output: &Value, path: &str) -> Result<Value, String> {
    match output {
        Value::String(output) => Ok(Value::String(output.clone())),
        Value::Array(_) => response_content_to_anthropic(output, "user")
            .map_err(|message| format!("`{path}` is invalid: {message}")),
        _ => Err(format!("`{path}` must be a string or content array")),
    }
}

fn append_tool_result(
    object: &Map<String, Value>,
    item_type: &str,
    messages: &mut Vec<Message>,
) -> Result<(), String> {
    let call_id = object
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{item_type} input requires non-empty string `call_id`"))?;
    let output = object
        .get("output")
        .ok_or_else(|| format!("{item_type} input requires `output`"))?;
    messages.push(Message {
        role: "user".to_string(),
        content: json!([{
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": tool_output_to_anthropic(output, &format!("{item_type}.output"))?
        }]),
    });
    Ok(())
}

fn validate_replayed_reasoning_item(object: &Map<String, Value>) -> Result<(), String> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "id"
                | "summary"
                | "content"
                | "encrypted_content"
                | "status"
                | "internal_chat_message_metadata_passthrough"
        ) {
            return Err(format!("reasoning input field `{key}` is not supported"));
        }
    }
    for key in ["id", "encrypted_content", "status"] {
        match object.get(key) {
            None | Some(Value::Null | Value::String(_)) => {}
            Some(_) => return Err(format!("reasoning input `{key}` must be a string")),
        }
    }
    for key in ["summary", "content"] {
        match object.get(key) {
            None | Some(Value::Null | Value::Array(_)) => {}
            Some(_) => return Err(format!("reasoning input `{key}` must be an array")),
        }
    }
    Ok(())
}

fn append_input_item(
    item: &Value,
    messages: &mut Vec<Message>,
    system: &mut Vec<SystemMessage>,
    catalog: &ToolCatalog,
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
                if !matches!(
                    key.as_str(),
                    "type"
                        | "role"
                        | "content"
                        | "id"
                        | "status"
                        | "phase"
                        | "internal_chat_message_metadata_passthrough"
                ) {
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
                    "type"
                        | "id"
                        | "call_id"
                        | "name"
                        | "namespace"
                        | "arguments"
                        | "encrypted_function_args"
                        | "status"
                        | "internal_chat_message_metadata_passthrough"
                ) {
                    return Err(format!(
                        "function call input field `{key}` is not supported"
                    ));
                }
            }
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "function call input requires non-empty string `call_id`".to_string()
                })?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "function call input requires non-empty string `name`".to_string()
                })?;
            let namespace = optional_tool_namespace(object, "function_call")?;
            validate_encrypted_function_args(object, "function_call")?;
            let public_key = PublicToolKey::function(namespace, name);
            let kiro_name = match (namespace, catalog.kiro_name(&public_key)) {
                (_, Some(name)) => name,
                (None, None) => name,
                (Some(namespace), None) => {
                    return Err(format!(
                        "function call references undeclared namespace tool `{namespace}.{name}`"
                    ));
                }
            };
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
                    "name": kiro_name,
                    "input": arguments
                }]),
            });
        }
        "function_call_output" => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type"
                        | "id"
                        | "call_id"
                        | "output"
                        | "status"
                        | "internal_chat_message_metadata_passthrough"
                ) {
                    return Err(format!(
                        "function call output input field `{key}` is not supported"
                    ));
                }
            }
            append_tool_result(object, "function_call_output", messages)?;
        }
        "custom_tool_call" => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type"
                        | "id"
                        | "call_id"
                        | "name"
                        | "namespace"
                        | "input"
                        | "status"
                        | "internal_chat_message_metadata_passthrough"
                ) {
                    return Err(format!(
                        "custom tool call input field `{key}` is not supported"
                    ));
                }
            }
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "custom tool call input requires non-empty string `call_id`".to_string()
                })?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "custom tool call input requires non-empty string `name`".to_string()
                })?;
            let namespace = optional_tool_namespace(object, "custom_tool_call")?;
            let input = object
                .get("input")
                .and_then(Value::as_str)
                .ok_or_else(|| "custom tool call input requires string `input`".to_string())?;
            let public_key = PublicToolKey::custom(namespace, name);
            let kiro_name = match (namespace, catalog.kiro_name(&public_key)) {
                (_, Some(name)) => name,
                (None, None) => name,
                (Some(namespace), None) => {
                    return Err(format!(
                        "custom tool call references undeclared namespace tool `{namespace}.{name}`"
                    ));
                }
            };
            messages.push(Message {
                role: "assistant".to_string(),
                content: json!([{
                    "type": "tool_use",
                    "id": call_id,
                    "name": kiro_name,
                    "input": {"input": input}
                }]),
            });
        }
        "custom_tool_call_output" => {
            for key in object.keys() {
                if !matches!(
                    key.as_str(),
                    "type"
                        | "id"
                        | "call_id"
                        | "name"
                        | "output"
                        | "status"
                        | "internal_chat_message_metadata_passthrough"
                ) {
                    return Err(format!(
                        "custom tool call output input field `{key}` is not supported"
                    ));
                }
            }
            if object
                .get("name")
                .is_some_and(|name| !name.is_null() && !name.is_string())
            {
                return Err("custom tool call output `name` must be a string".to_string());
            }
            append_tool_result(object, "custom_tool_call_output", messages)?;
        }
        "reasoning" => {
            validate_replayed_reasoning_item(object)?;
            // Codex replays opaque reasoning items when `store: false`. Kiro cannot
            // consume OpenAI encrypted reasoning, so retain visible messages and tool
            // results while intentionally omitting this validated opaque item.
        }
        "additional_tools" => {}
        other => return Err(format!("input item type `{other}` is not supported")),
    }
    Ok(())
}

fn input_to_messages(
    request: &Value,
    instructions: Option<String>,
    catalog: &ToolCatalog,
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
                append_input_item(item, &mut messages, &mut system, catalog)?;
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

fn tool_choice_to_anthropic(
    request: &Value,
    catalog: &ToolCatalog,
) -> Result<Option<Value>, String> {
    let Some(choice) = request.get("tool_choice") else {
        return Ok(None);
    };
    if let Some(choice) = choice.as_str() {
        return match choice {
            "auto" if !catalog.anthropic_tools.is_empty() => Ok(Some(json!({"type": "auto"}))),
            "auto" => Ok(None),
            "required" if !catalog.anthropic_tools.is_empty() => Ok(Some(json!({"type": "any"}))),
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
        if !matches!(key.as_str(), "type" | "name" | "namespace") {
            return Err(format!("`tool_choice.{key}` is not supported"));
        }
    }
    let kind = match object.get("type").and_then(Value::as_str) {
        Some("function") => ResponseToolKind::Function,
        Some("custom") => ResponseToolKind::Custom,
        _ => return Err("object `tool_choice.type` must be `function` or `custom`".to_string()),
    };
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "object `tool_choice` requires non-empty string `name`".to_string())?;
    let namespace = optional_tool_namespace(object, "tool_choice")?;
    let key = PublicToolKey {
        kind,
        namespace: namespace.map(str::to_string),
        name: name.to_string(),
    };
    let kiro_name = catalog.kiro_name(&key).ok_or_else(|| {
        let qualified = namespace
            .map(|namespace| format!("{namespace}.{name}"))
            .unwrap_or_else(|| name.to_string());
        format!("`tool_choice` references unknown tool `{qualified}`")
    })?;
    Ok(Some(json!({"type": "tool", "name": kiro_name})))
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
    parallel_tool_calls: bool,
    text: Value,
    tool_catalog: ToolCatalog,
    store: StoreMode,
    previous_response_id: Option<String>,
}

impl ResponseMeta {
    fn new(
        request: &Value,
        model: &str,
        max_output_tokens: i32,
        reasoning: Option<ReasoningConfig>,
        tool_catalog: ToolCatalog,
        store: StoreMode,
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
            tools: response_tool_definitions(request),
            tool_choice: request
                .get("tool_choice")
                .cloned()
                .unwrap_or_else(|| json!("auto")),
            parallel_tool_calls: request
                .get("parallel_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            text: request
                .get("text")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| json!({"format": {"type": "text"}})),
            tool_catalog,
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
            "parallel_tool_calls": self.parallel_tool_calls,
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
            "text": self.text,
            "tool_choice": self.tool_choice,
            "tools": self.tools,
            "truncation": "disabled",
            "store": self.store.enabled(),
            "usage": usage
        })
    }

    fn uses_json_schema(&self) -> bool {
        self.text.pointer("/format/type").and_then(Value::as_str) == Some("json_schema")
    }
}

fn canonical_json_prefix(text: &str) -> Result<String, ()> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .next()
        .ok_or(())?
        .map(|value| value.to_string())
        .map_err(|_| ())
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

fn custom_input_from_arguments(arguments: &Value) -> String {
    match arguments {
        Value::Object(arguments) => arguments
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Value::Object(arguments.clone()).to_string()),
        Value::String(input) => input.clone(),
        arguments => arguments.to_string(),
    }
}

fn new_tool_output(block: &Value, call_id: &str, catalog: &ToolCatalog) -> Value {
    let kiro_name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = catalog.public_key(kiro_name);
    let input = block
        .get("input")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut output = match key.kind {
        ResponseToolKind::Function => json!({
            "type": "function_call",
            "id": format!("fc_{}", Uuid::new_v4().simple()),
            "call_id": call_id,
            "name": key.name.clone(),
            "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string()),
            "status": "completed"
        }),
        ResponseToolKind::Custom => json!({
            "type": "custom_tool_call",
            "id": format!("ctc_{}", Uuid::new_v4().simple()),
            "call_id": call_id,
            "name": key.name.clone(),
            "input": custom_input_from_arguments(&input),
            "status": "completed"
        }),
    };
    if let Some(namespace) = key.namespace {
        output["namespace"] = Value::String(namespace);
    }
    output
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
    let mut output = Vec::new();
    let mut assistant_blocks = Vec::new();
    let mut tool_calls_seen = 0_usize;
    let structured_completion = meta.uses_json_schema()
        && !matches!(
            anthropic.get("stop_reason").and_then(Value::as_str),
            Some("tool_use" | "max_tokens")
        );
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let mut value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if structured_completion {
                    value = canonical_json_prefix(&value)
                        .map_err(|()| "model response did not contain a complete JSON value")?;
                }
                output.push(new_message_output(vec![value.clone()]));
                assistant_blocks.push(json!({"type": "text", "text": value}));
            }
            Some("tool_use") => {
                tool_calls_seen += 1;
                if !meta.parallel_tool_calls && tool_calls_seen > 1 {
                    tracing::warn!(
                        response_id = %meta.id,
                        tool_calls = tool_calls_seen,
                        "upstream returned multiple tool calls despite parallel_tool_calls=false; preserving every call"
                    );
                }
                let call_id = format!("call_{}", Uuid::new_v4().simple());
                output.push(new_tool_output(block, &call_id, &meta.tool_catalog));
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
        public_key: PublicToolKey,
        kiro_name: String,
    },
}

struct StreamStoreContext {
    store: Arc<ResponseStore>,
    model: String,
    session_id: String,
    messages: Vec<Message>,
}

struct ResponsesStreamState {
    meta: ResponseMeta,
    sequence: u64,
    started: bool,
    done: bool,
    usage: Value,
    items: Vec<Value>,
    assistant_blocks: Vec<Value>,
    active: HashMap<i64, StreamingItem>,
    next_output_index: usize,
    tool_calls_started: usize,
    stop_reason: Option<String>,
    store: Option<StreamStoreContext>,
}

impl ResponsesStreamState {
    #[cfg(test)]
    fn new(meta: ResponseMeta) -> Self {
        Self::with_store(meta, None)
    }

    fn with_store(meta: ResponseMeta, store: Option<StreamStoreContext>) -> Self {
        Self {
            meta,
            sequence: 0,
            started: false,
            done: false,
            usage: json!({}),
            items: Vec::new(),
            assistant_blocks: Vec::new(),
            active: HashMap::new(),
            next_output_index: 0,
            tool_calls_started: 0,
            stop_reason: None,
            store,
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
                self.tool_calls_started += 1;
                if !self.meta.parallel_tool_calls && self.tool_calls_started > 1 {
                    tracing::warn!(
                        response_id = %self.meta.id,
                        tool_calls = self.tool_calls_started,
                        "upstream streamed multiple tool calls despite parallel_tool_calls=false; preserving every call"
                    );
                }
                let kiro_name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let public_key = self.meta.tool_catalog.public_key(&kiro_name);
                let mut item = match public_key.kind {
                    ResponseToolKind::Function => json!({
                        "type": "function_call",
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "call_id": format!("call_{}", Uuid::new_v4().simple()),
                        "name": public_key.name.clone(),
                        "arguments": "",
                        "status": "in_progress"
                    }),
                    ResponseToolKind::Custom => json!({
                        "type": "custom_tool_call",
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "call_id": format!("call_{}", Uuid::new_v4().simple()),
                        "name": public_key.name.clone(),
                        "input": "",
                        "status": "in_progress"
                    }),
                };
                if let Some(namespace) = public_key.namespace.as_ref() {
                    item["namespace"] = Value::String(namespace.clone());
                }
                self.active.insert(
                    block_index,
                    StreamingItem::Function {
                        output_index,
                        item: item.clone(),
                        arguments: String::new(),
                        public_key,
                        kiro_name,
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
        let buffer_structured_text = self.meta.uses_json_schema();
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
                if buffer_structured_text {
                    return String::new();
                }
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
                public_key,
                ..
            } if delta.get("type").and_then(Value::as_str) == Some("input_json_delta") => {
                let value = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                arguments.push_str(&value);
                if public_key.kind == ResponseToolKind::Custom {
                    return String::new();
                }
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
                mut text,
            } => {
                let structured_text = self.meta.uses_json_schema();
                if structured_text {
                    text = match canonical_json_prefix(&text) {
                        Ok(text) => text,
                        Err(()) => {
                            self.done = true;
                            return self.event(
                                "error",
                                json!({
                                    "code": "invalid_structured_output",
                                    "message": "The model did not return a complete JSON value for the requested structured output.",
                                    "param": "text.format"
                                }),
                            );
                        }
                    };
                }
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
                self.assistant_blocks
                    .push(json!({"type": "text", "text": text.clone()}));
                format!(
                    "{}{}{}{}",
                    structured_text
                        .then(|| self.event(
                            "response.output_text.delta",
                            json!({
                                "item_id": item_id,
                                "output_index": output_index,
                                "content_index": content_index,
                                "delta": text,
                                "logprobs": []
                            })
                        ))
                        .unwrap_or_default(),
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
                public_key,
                kiro_name,
            } => {
                let item_id = item["id"].as_str().unwrap_or_default().to_string();
                let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                let parsed_arguments = if arguments.is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(&arguments).unwrap_or_else(|_| Value::Object(Map::new()))
                };
                if arguments.is_empty() {
                    arguments = "{}".to_string();
                }
                self.assistant_blocks.push(json!({
                    "type": "tool_use",
                    "id": call_id.clone(),
                    "name": kiro_name,
                    "input": parsed_arguments.clone()
                }));
                item["status"] = json!("completed");
                match public_key.kind {
                    ResponseToolKind::Function => {
                        item["arguments"] = Value::String(arguments.clone());
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
                    ResponseToolKind::Custom => {
                        let input = custom_input_from_arguments(&parsed_arguments);
                        item["input"] = Value::String(input.clone());
                        self.items.push(item.clone());
                        let delta = (!input.is_empty()).then(|| {
                            self.event(
                                "response.custom_tool_call_input.delta",
                                json!({
                                    "item_id": item_id,
                                    "call_id": call_id,
                                    "output_index": output_index,
                                    "delta": input.clone()
                                }),
                            )
                        });
                        format!(
                            "{}{}{}",
                            delta.unwrap_or_default(),
                            self.event(
                                "response.custom_tool_call_input.done",
                                json!({
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "input": input
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
        }
    }

    fn completed(&mut self, stop_reason: Option<&str>) -> String {
        if self.done {
            return String::new();
        }
        if let Some(mut store) = self.store.take() {
            if !self.assistant_blocks.is_empty() {
                store.messages.push(Message {
                    role: "assistant".to_string(),
                    content: Value::Array(self.assistant_blocks.clone()),
                });
            }
            let stored = StoredConversation {
                model: store.model,
                session_id: store.session_id,
                messages: store.messages,
            };
            if store.store.insert(self.meta.id.clone(), stored).is_err() {
                self.done = true;
                return self.event(
                    "error",
                    json!({
                        "code": "response_store_error",
                        "message": "The completed response exceeded the retention limit required by `store: true`.",
                        "param": Value::Null
                    }),
                );
            }
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

fn responses_stream_response(
    body: Body,
    meta: ResponseMeta,
    store: Option<StreamStoreContext>,
) -> Response {
    let input = body.into_data_stream();
    let output = stream::unfold(
        (
            input,
            BytesMut::new(),
            ResponsesStreamState::with_store(meta, store),
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

fn responses_stream_response_from_upstream(
    mut upstream: Pin<Box<dyn Future<Output = Response> + Send>>,
    meta: ResponseMeta,
    store: Option<StreamStoreContext>,
) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(8);
    tokio::spawn(async move {
        if sender.send(RESPONSES_KEEP_ALIVE).await.is_err() {
            return;
        }

        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + RESPONSES_KEEP_ALIVE_INTERVAL,
            RESPONSES_KEEP_ALIVE_INTERVAL,
        );
        let response = loop {
            tokio::select! {
                response = &mut upstream => break response,
                _ = heartbeat.tick() => {
                    if sender.send(RESPONSES_KEEP_ALIVE).await.is_err() {
                        return;
                    }
                }
            }
        };

        if !response.status().is_success() {
            let error = ResponsesStreamState::with_store(meta, store).clean_stream_error();
            let _ = sender.send(Bytes::from(error)).await;
            return;
        }

        let transformed = responses_stream_response(response.into_body(), meta, store);
        let mut body = transformed.into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    if sender.send(chunk).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let output = stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|chunk| (Ok::<Bytes, Infallible>(chunk), receiver))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(output))
        .expect("Deferred Responses stream response")
}

async fn responses_stream_from_upstream(
    mut upstream: Pin<Box<dyn Future<Output = Response> + Send>>,
    meta: ResponseMeta,
    store: Option<StreamStoreContext>,
    grace_period: Duration,
) -> Response {
    match tokio::time::timeout(grace_period, &mut upstream).await {
        Ok(response) if response.status().is_success() => {
            responses_stream_response(response.into_body(), meta, store)
        }
        Ok(response) => {
            let status = response.status();
            response_error(status, default_error_message(status), true)
        }
        Err(_) => responses_stream_response_from_upstream(upstream, meta, store),
    }
}

fn translated_request_for_profile(
    request: &Value,
    model: &str,
    accept_strict_hint: bool,
) -> Result<(MessagesRequest, ResponseMeta), String> {
    validate_top_level_fields(request)?;
    let text_format_instruction = responses_text_format_instruction(request.get("text"))?;
    let instructions = optional_string(request.get("instructions"), "instructions")?;
    let max_tokens = max_output_tokens(request)?;
    let stream = stream_requested(request)?;
    let reasoning = reasoning_config(request)?;
    let store = store_requested(request)?;
    let previous_response_id = previous_response_id(request)?;
    let catalog = tool_catalog(request, accept_strict_hint)?;
    let (messages, mut system) = input_to_messages(request, instructions, &catalog)?;
    let mut tools = catalog.tools();
    let tool_choice = tool_choice_to_anthropic(request, &catalog)?;
    if request.get("tool_choice").and_then(Value::as_str) == Some("none") {
        tools = None;
    }
    if request.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false)
        && tools.as_ref().is_some_and(|tools| !tools.is_empty())
    {
        system.get_or_insert_with(Vec::new).push(SystemMessage {
            text: "Tool-use constraint: Call at most one tool in this assistant response. If more work is needed, wait for the tool result before selecting another tool."
                .to_string(),
            cache_control: None,
        });
    }
    if let Some(instruction) = text_format_instruction {
        system.get_or_insert_with(Vec::new).push(SystemMessage {
            text: instruction,
            cache_control: None,
        });
    }
    let meta = ResponseMeta::new(
        request,
        model,
        max_tokens,
        reasoning.clone(),
        catalog,
        store,
        previous_response_id,
    );
    Ok((
        MessagesRequest {
            model: model.to_string(),
            max_tokens,
            temperature: None,
            top_p: None,
            top_k: None,
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
    let requested_model = match required_model(&request) {
        Ok(model) => model.to_string(),
        Err(message) => return invalid_request(message, true),
    };
    let model = match canonical_gpt56_model(&requested_model) {
        Some(model) => model.to_string(),
        None if is_gpt_family_name(&requested_model) => {
            return invalid_request(
                format!(
                    "The model `{requested_model}` is not supported. Supported models: gpt-5.6, {}",
                    GPT56_MODELS.join(", ")
                ),
                true,
            );
        }
        None => return legacy_non_gpt_response(&state),
    };

    let (mut translated, mut meta) =
        match translated_request_for_profile(&request, &model, state.aws_b40_compat) {
            Ok(translated) => translated,
            Err(message) => return invalid_request(message, true),
        };
    debug_assert_eq!(translated.model, model);
    let stream_requested = translated.stream;

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

    let mut stored_messages = meta.store.enabled().then(|| translated.messages.clone());
    let retention_error = stored_messages.as_ref().and_then(|messages| {
        let candidate = StoredConversation {
            model: model.clone(),
            session_id: session_id.clone(),
            messages: messages.clone(),
        };
        state
            .response_store
            .validate_size(&meta.id, &candidate)
            .err()
    });
    if let Some(StoreError::EntryTooLarge { max_bytes }) = retention_error {
        if meta.store.disable_if_implicit() {
            tracing::warn!(
                response_id = %meta.id,
                max_bytes,
                "implicit Responses storage disabled because visible conversation state exceeds the retention limit"
            );
            stored_messages = None;
        } else {
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
    let upstream: Pin<Box<dyn Future<Output = Response> + Send>> = Box::pin(post_messages(
        State(state.clone()),
        HeaderMap::new(),
        RawApiJson(translated, raw),
    ));

    if stream_requested {
        let stream_store = stored_messages.take().map(|messages| StreamStoreContext {
            store: state.response_store.clone(),
            model: model.clone(),
            session_id,
            messages,
        });
        return mark_gpt_openai_response(
            responses_stream_from_upstream(
                upstream,
                meta,
                stream_store,
                RESPONSES_KEEP_ALIVE_INTERVAL,
            )
            .await,
        );
    }

    let response = upstream.await;
    let status = response.status();

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
    let mut mapped = match anthropic_to_response(&anthropic, &meta) {
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
        if let Some(assistant) = mapped.assistant_message.take() {
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
            if meta.store.disable_if_implicit() {
                tracing::warn!(
                    response_id = %meta.id,
                    max_bytes,
                    "implicit Responses storage disabled because completed visible state exceeds the retention limit"
                );
                mapped.response["store"] = Value::Bool(false);
            } else {
                return response_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "The completed response exceeds the {max_bytes}-byte retention limit required by `store: true`"
                    ),
                    true,
                );
            }
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
        ResponseMeta::new(
            &base_request(model),
            model,
            64,
            None,
            ToolCatalog::default(),
            StoreMode::Disabled,
            None,
        )
    }

    fn realistic_codex_request() -> Value {
        json!({
            "model": "gpt-5.6",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {
                            "type": "function",
                            "name": "shell_command",
                            "description": "Run a shell command",
                            "strict": false,
                            "defer_loading": false,
                            "parameters": {
                                "type": "object",
                                "properties": {"cmd": {"type": "string"}},
                                "required": ["cmd"]
                            }
                        },
                        {
                            "type": "custom",
                            "name": "apply_patch",
                            "description": "Apply a patch",
                            "format": {
                                "type": "grammar",
                                "syntax": "lark",
                                "definition": "start: patch\npatch: /.+/s"
                            }
                        },
                        {
                            "type": "namespace",
                            "name": "collaboration",
                            "description": "Coordinate agents",
                            "tools": [{
                                "type": "function",
                                "name": "list_agents",
                                "description": "List agents",
                                "strict": false,
                                "parameters": {"type": "object", "properties": {}}
                            }]
                        },
                        {
                            "type": "namespace",
                            "name": "codex_apps",
                            "description": "Use Codex apps",
                            "tools": [{
                                "type": "function",
                                "name": "list_agents",
                                "description": "List app agents",
                                "parameters": {"type": "object", "properties": {}}
                            }]
                        }
                    ]
                },
                {
                    "type": "message",
                    "role": "developer",
                    "phase": "commentary",
                    "content": [{"type": "input_text", "text": "Work carefully."}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Fix the file."}]
                },
                {
                    "type": "custom_tool_call",
                    "id": "ctc_previous",
                    "call_id": "call_previous",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** End Patch",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call_output",
                    "id": "ctco_previous",
                    "call_id": "call_previous",
                    "name": "apply_patch",
                    "output": [{"type": "input_text", "text": "Done"}]
                }
            ],
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "codex-session-key",
            "client_metadata": {
                "originator": "codex_cli_rs",
                "session_id": "session-123"
            },
            "reasoning": {
                "effort": "high",
                "context": "all_turns",
                "summary": "auto"
            },
            "text": {
                "format": {"type": "text"},
                "verbosity": "medium"
            },
            "parallel_tool_calls": false,
            "stream": true,
            "store": false,
            "stream_options": {
                "include_usage": true,
                "future_option": {"accepted": true}
            }
        })
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

    #[tokio::test]
    async fn canonicalizes_the_gpt56_alias_before_entering_the_provider_path() {
        assert_eq!(canonical_gpt56_model("gpt-5.6"), Some("gpt-5.6-sol"));
        assert_eq!(canonical_gpt56_model("GPT 5.6"), Some("gpt-5.6-sol"));
        assert_eq!(
            canonical_gpt56_model("gpt-5.6-terra"),
            Some("gpt-5.6-terra")
        );

        let response = post_responses(
            State(AppState::new("test-api-key", true, true)),
            ResponsesJson(base_request("gpt-5.6")),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the alias must pass local model validation and reach the real provider path"
        );
    }

    #[test]
    fn translates_current_codex_additional_tools_and_replays_custom_calls() {
        let request = realistic_codex_request();
        let (translated, meta) =
            translated_request(&request, "gpt-5.6-sol").expect("current Codex request");

        assert_eq!(translated.model, "gpt-5.6-sol");
        assert!(translated.stream);
        assert_eq!(translated.tools.as_ref().unwrap().len(), 4);
        assert_eq!(
            translated.system.as_ref().unwrap()[0].text,
            "Work carefully."
        );
        assert_eq!(translated.messages.len(), 3);
        assert_eq!(translated.messages[0].content[0]["text"], "Fix the file.");
        assert_eq!(translated.system.as_ref().unwrap().len(), 2);
        assert!(
            translated.system.as_ref().unwrap()[1]
                .text
                .contains("at most one tool")
        );

        let custom_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::custom(None, "apply_patch"))
            .expect("custom tool mapping");
        let collaboration_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::function(
                Some("collaboration"),
                "list_agents",
            ))
            .expect("collaboration namespace mapping");
        let codex_apps_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::function(Some("codex_apps"), "list_agents"))
            .expect("codex_apps namespace mapping");
        assert_ne!(collaboration_name, codex_apps_name);
        assert!(custom_name.len() <= 63);
        assert!(collaboration_name.len() <= 63);
        assert!(codex_apps_name.len() <= 63);
        assert_eq!(translated.messages[1].content[0]["name"], custom_name);
        assert_eq!(
            translated.messages[1].content[0]["input"],
            json!({"input": "*** Begin Patch\n*** End Patch"})
        );
        assert_eq!(
            translated.messages[2].content[0]["content"],
            json!([{"type": "text", "text": "Done"}])
        );
        assert_eq!(meta.parallel_tool_calls, false);
        assert_eq!(meta.text["verbosity"], "medium");
        let response = meta.response("completed", json!([]), json!({}));
        assert_eq!(response["text"], request["text"]);
        assert_eq!(response["tools"], request["input"][0]["tools"]);
        assert_eq!(response["tools"][1]["type"], "custom");
        assert_eq!(response["tools"][2]["type"], "namespace");
        super::super::converter::convert_request(&translated)
            .expect("the translated current Codex request must reach the Kiro wire converter");
    }

    #[test]
    fn replays_undeclared_custom_tool_call_as_history_without_reactivating_it() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "user", "content": "Read the workspace."},
                {
                    "type": "custom_tool_call",
                    "call_id": "call_exec",
                    "name": "exec",
                    "input": "rg --files",
                    "status": "completed"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_exec",
                    "output": "README.md",
                    "status": "completed"
                }
            ],
            "tools": [],
            "store": false
        });

        let (translated, _) = translated_request(&request, "gpt-5.6-sol")
            .expect("a completed custom call is historical context, not an active declaration");
        assert!(translated.tools.is_none());
        assert_eq!(translated.messages[1].role, "assistant");
        assert_eq!(translated.messages[1].content[0]["type"], "tool_use");
        assert_eq!(translated.messages[1].content[0]["name"], "exec");
        assert_eq!(translated.messages[2].role, "user");
        assert_eq!(translated.messages[2].content[0]["type"], "tool_result");
        super::super::converter::convert_request(&translated)
            .expect("the replayed custom call must reach the Kiro wire converter");
    }

    #[test]
    fn accepts_codex_cached_web_search_declaration_without_forwarding_or_losing_the_echo() {
        let mut request = realistic_codex_request();
        request["tools"] = json!([
            {
                "type": "function",
                "name": "read_file",
                "description": "Read one file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            },
            {
                "type": "web_search",
                "external_web_access": false,
                "search_content_types": ["text", "image"]
            }
        ]);

        let (translated, meta) =
            translated_request(&request, "gpt-5.6-sol").expect("current Codex web search shape");
        let forwarded_tools = translated.tools.expect("function tool is forwarded");
        assert_eq!(forwarded_tools.len(), 5);
        assert!(forwarded_tools.iter().any(|tool| tool.name == "read_file"));
        assert!(forwarded_tools.iter().all(|tool| tool.name != "web_search"));
        assert_eq!(meta.tools.as_array().unwrap().len(), 6);
        assert_eq!(meta.tools[0], request["tools"][0]);
        assert_eq!(meta.tools[1], request["tools"][1]);
        assert_eq!(
            meta.response("completed", json!([]), json!({}))["tools"],
            meta.tools
        );
    }

    #[test]
    fn rejects_external_or_parameterized_hosted_web_search() {
        for tool in [
            json!({"type": "web_search", "external_web_access": true}),
            json!({"type": "web_search"}),
            json!({"type": "web_search", "external_web_access": "false"}),
            json!({
                "type": "web_search",
                "external_web_access": false,
                "search_content_types": ["text", "video"]
            }),
            json!({
                "type": "web_search",
                "external_web_access": false,
                "search_content_types": "text"
            }),
            json!({
                "type": "web_search",
                "external_web_access": false,
                "search_context_size": "low"
            }),
        ] {
            let mut request = base_request("gpt-5.6-sol");
            request["tools"] = json!([tool]);
            assert!(
                translated_request(&request, "gpt-5.6-sol").is_err(),
                "hosted search must fail explicitly instead of being silently disabled"
            );
        }
    }

    #[test]
    fn cached_web_search_alone_does_not_leave_an_orphaned_auto_tool_choice() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": "Reply with exactly OK.",
            "tools": [{
                "type": "web_search",
                "external_web_access": false,
                "search_content_types": ["text", "image"]
            }],
            "tool_choice": "auto",
            "store": false
        });
        let (translated, meta) =
            translated_request(&request, "gpt-5.6-sol").expect("cached-only search request");
        assert!(translated.tools.is_none());
        assert!(translated.tool_choice.is_none());
        let response = meta.response("completed", json!([]), json!({}));
        assert_eq!(response["tool_choice"], "auto");
        assert_eq!(response["tools"], request["tools"]);
    }

    #[test]
    fn serial_tool_constraint_is_added_only_when_tools_are_active() {
        let declared_tools = json!([{
            "type": "function",
            "name": "lookup",
            "description": "Look up one item",
            "parameters": {"type": "object", "properties": {}}
        }]);

        let has_constraint = |request: &Value| {
            let (translated, _) = translated_request(request, "gpt-5.6-sol").unwrap();
            translated
                .system
                .unwrap_or_default()
                .iter()
                .any(|message| message.text.contains("at most one tool"))
        };

        let mut serial = base_request("gpt-5.6-sol");
        serial["tools"] = declared_tools.clone();
        serial["parallel_tool_calls"] = json!(false);
        assert!(has_constraint(&serial));

        let mut parallel = serial.clone();
        parallel["parallel_tool_calls"] = json!(true);
        assert!(!has_constraint(&parallel));

        let mut omitted = base_request("gpt-5.6-sol");
        omitted["tools"] = declared_tools.clone();
        assert!(!has_constraint(&omitted));

        let mut disabled = serial;
        disabled["tool_choice"] = json!("none");
        let (translated, meta) = translated_request(&disabled, "gpt-5.6-sol").unwrap();
        assert!(translated.tools.is_none());
        assert!(
            !translated
                .system
                .unwrap_or_default()
                .iter()
                .any(|message| message.text.contains("at most one tool"))
        );
        assert_eq!(meta.tools, declared_tools);
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
    fn accepts_codex_text_strict_and_omits_validated_replayed_reasoning() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_previous",
                    "summary": [{"type": "summary_text", "text": "Opaque prior reasoning"}],
                    "content": [],
                    "encrypted_content": "opaque-reasoning-payload",
                    "status": "completed"
                },
                {"type": "message", "role": "user", "content": "Continue"}
            ],
            "store": false,
            "text": {"format": {"type": "text", "strict": true}}
        });

        let (translated, meta) =
            translated_request(&request, "gpt-5.6-sol").expect("Codex replay request");
        assert_eq!(translated.messages.len(), 1);
        assert_eq!(translated.messages[0].role, "user");
        assert_eq!(translated.messages[0].content, "Continue");
        assert_eq!(meta.text, request["text"]);

        for invalid in [
            json!({"type": "reasoning", "encrypted_content": 1}),
            json!({"type": "reasoning", "summary": "not-an-array"}),
            json!({"type": "reasoning", "future_field": true}),
        ] {
            let mut invalid_request = request.clone();
            invalid_request["input"][0] = invalid;
            assert!(translated_request(&invalid_request, "gpt-5.6-sol").is_err());
        }

        let mut invalid_strict = request;
        invalid_strict["text"]["format"]["strict"] = json!("yes");
        assert!(translated_request(&invalid_strict, "gpt-5.6-sol").is_err());
    }

    #[test]
    fn translates_responses_json_schema_to_kiro_system_instruction() {
        let mut request = base_request("gpt-5.6-sol");
        request["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": "workspace_summary",
                "description": "A concise workspace summary",
                "schema": {
                    "type": "object",
                    "properties": {
                        "summary": {"type": "string"}
                    },
                    "required": ["summary"],
                    "additionalProperties": false
                },
                "strict": true
            }
        });

        let (translated, meta) = translated_request(&request, "gpt-5.6-sol")
            .expect("Responses JSON Schema output format");
        assert!(translated.output_config.is_none());
        let system = translated.system.as_ref().expect("schema instruction");
        assert!(system.iter().any(|message| {
            message.text.contains("JSON Schema:")
                && message.text.contains("workspace_summary")
                && message.text.contains("additionalProperties")
        }));
        assert_eq!(meta.text, request["text"]);
        super::super::converter::convert_request(&translated)
            .expect("the structured-output request must reach the Kiro wire converter");
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
    fn aws_b_strict_hint_flows_through_namespaced_additional_tools() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "collaboration",
                        "description": "Coordinate agents",
                        "tools": [{
                            "type": "function",
                            "name": "list_agents",
                            "description": "List agents",
                            "parameters": {"type": "object", "properties": {}},
                            "strict": true,
                            "defer_loading": false
                        }]
                    }]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "List agents"}]
                }
            ],
            "max_output_tokens": 64
        });

        assert!(
            translated_request(&request, "gpt-5.6-sol").is_err(),
            "non AWS-B profiles keep nested strict rejection"
        );
        let (translated, _) = translated_request_for_profile(&request, "gpt-5.6-sol", true)
            .expect("AWS-B accepts nested strict as a hint");
        let tools = translated.tools.expect("translated namespaced tool");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].strict, None);
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
        request["text"] = json!({"format": {"type": "text"}, "verbosity": "high"});
        request["include"] = json!(["reasoning.encrypted_content"]);
        request["parallel_tool_calls"] = json!(false);
        request["prompt_cache_key"] = json!("codex-cache-key");
        request["client_metadata"] = json!({"originator": "codex_cli_rs"});
        request["service_tier"] = json!("auto");
        request["stream_options"] = json!({
            "include_usage": true,
            "unknown_future_option": [1, 2, 3]
        });
        request["reasoning"] = json!({
            "effort": "high",
            "context": "all_turns",
            "summary": "auto"
        });

        let (_, meta) = translated_request(&request, "gpt-5.6-sol").expect("current Codex fields");
        let response = meta.response("completed", json!([]), json!({}));
        assert_eq!(response["metadata"]["test_run"], "gpt56");
        assert_eq!(response["store"], true);
        assert_eq!(response["previous_response_id"], "resp_previous");
        assert_eq!(response["parallel_tool_calls"], false);
        assert_eq!(response["text"], request["text"]);
        assert_eq!(response["output"], json!([]));

        for (field, value) in [
            ("include", json!(["message.output_text.logprobs"])),
            (
                "prompt_cache_options",
                json!({"mode": "explicit", "retention": "30m"}),
            ),
            (
                "text",
                json!({"format": {"type": "json_schema", "name": "result"}}),
            ),
            ("parallel_tool_calls", json!("false")),
            ("prompt_cache_key", json!({"not": "a string"})),
            ("client_metadata", json!({"invalid": 1})),
        ] {
            let mut unsupported = base_request("gpt-5.6-sol");
            unsupported[field] = value;
            assert!(
                translated_request(&unsupported, "gpt-5.6-sol").is_err(),
                "{field} must fail explicitly instead of being silently ignored"
            );
        }

        for accepted_reasoning in [
            json!({"effort": "high", "context": "all_turns"}),
            json!({"effort": "high", "context": "current_turn"}),
            json!({"effort": "high", "context": "auto"}),
            json!({"context": "all_turns", "summary": "auto"}),
        ] {
            let mut supported = base_request("gpt-5.6-sol");
            supported["reasoning"] = accepted_reasoning;
            translated_request(&supported, "gpt-5.6-sol").expect("Codex reasoning controls");
        }

        for (field, value) in [
            (
                "reasoning",
                json!({"effort": "high", "context": "everywhere"}),
            ),
            ("reasoning", json!({"effort": "high", "summary": "verbose"})),
            ("text", json!({"verbosity": "extreme"})),
        ] {
            let mut invalid = base_request("gpt-5.6-sol");
            invalid[field] = value;
            assert!(translated_request(&invalid, "gpt-5.6-sol").is_err());
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
    fn responses_store_defaults_true_and_respects_explicit_false() {
        assert_eq!(store_requested(&json!({})).unwrap(), StoreMode::Implicit);
        assert_eq!(
            store_requested(&json!({"store": null})).unwrap(),
            StoreMode::Implicit
        );
        assert_eq!(
            store_requested(&json!({"store": true})).unwrap(),
            StoreMode::Required
        );
        assert_eq!(
            store_requested(&json!({"store": false})).unwrap(),
            StoreMode::Disabled
        );

        let mut implicit = StoreMode::Implicit;
        assert!(implicit.disable_if_implicit());
        assert_eq!(implicit, StoreMode::Disabled);
        let mut required = StoreMode::Required;
        assert!(!required.disable_if_implicit());
        assert_eq!(required, StoreMode::Required);

        let request = base_request("gpt-5.6-sol");
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        assert!(meta.store.enabled());
        assert_eq!(
            meta.response("completed", json!([]), json!({}))["store"],
            true
        );
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
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "stream + store must reach the real provider path"
        );

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
    fn structured_nonstream_never_returns_text_after_first_json_value() {
        let mut request = base_request("gpt-5.6-sol");
        request["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": "workspace_summary",
                "schema": {
                    "type": "object",
                    "properties": {"safe": {"type": "boolean"}},
                    "required": ["safe"],
                    "additionalProperties": false
                },
                "strict": true
            }
        });
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        let mapped = anthropic_to_response(
            &json!({
                "content": [
                    {"type": "text", "text": "{\"safe\":true} 助手：ChatGPT。"}
                ],
                "stop_reason": "end_turn",
                "usage": {}
            }),
            &meta,
        )
        .expect("valid JSON prefix must be preserved");

        assert_eq!(
            mapped.response["output"][0]["content"][0]["text"],
            r#"{"safe":true}"#
        );
        assert_eq!(
            mapped.assistant_message.unwrap().content[0]["text"],
            r#"{"safe":true}"#
        );
        assert!(!mapped.response.to_string().contains("ChatGPT"));
    }

    #[test]
    fn nonstream_preserves_mixed_text_and_tool_output_order() {
        let meta = base_meta("gpt-5.6-sol");
        let mapped = anthropic_to_response(
            &json!({
                "content": [
                    {"type": "tool_use", "name": "first", "input": {"n": 1}},
                    {"type": "text", "text": "Between calls."},
                    {"type": "tool_use", "name": "second", "input": {"n": 2}},
                    {"type": "text", "text": "After calls."}
                ],
                "stop_reason": "tool_use",
                "usage": {}
            }),
            &meta,
        )
        .expect("mixed response mapping");

        let output = mapped.response["output"].as_array().unwrap();
        assert_eq!(output.len(), 4);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "Between calls.");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[3]["type"], "message");
        assert_eq!(output[3]["content"][0]["text"], "After calls.");

        let assistant = mapped.assistant_message.expect("stored mixed response");
        let blocks = assistant.content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[1], json!({"type": "text", "text": "Between calls."}));
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[3], json!({"type": "text", "text": "After calls."}));
    }

    #[test]
    fn maps_custom_and_namespaced_tool_outputs_back_to_responses_shapes() {
        let mut request = realistic_codex_request();
        request["stream"] = json!(false);
        request["parallel_tool_calls"] = json!(true);
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").expect("current Codex request");
        let custom_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::custom(None, "apply_patch"))
            .unwrap()
            .to_string();
        let namespace_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::function(
                Some("collaboration"),
                "list_agents",
            ))
            .unwrap()
            .to_string();

        let mapped = anthropic_to_response(
            &json!({
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_private_custom",
                        "name": custom_name,
                        "input": {"input": "*** Begin Patch\n*** End Patch"}
                    },
                    {
                        "type": "tool_use",
                        "id": "toolu_private_namespace",
                        "name": namespace_name,
                        "input": {"path_prefix": "/root"}
                    }
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 2, "output_tokens": 3}
            }),
            &meta,
        )
        .expect("mapped tool response");
        let assistant = mapped
            .assistant_message
            .expect("stored assistant tool calls");
        let output = mapped.response["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "custom_tool_call");
        assert_eq!(output[0]["name"], "apply_patch");
        assert_eq!(output[0]["input"], "*** Begin Patch\n*** End Patch");
        assert!(output[0]["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["namespace"], "collaboration");
        assert_eq!(output[1]["name"], "list_agents");
        assert_eq!(
            serde_json::from_str::<Value>(output[1]["arguments"].as_str().unwrap()).unwrap(),
            json!({"path_prefix": "/root"})
        );
        assert_eq!(assistant.content[0]["id"], output[0]["call_id"]);
        assert_eq!(assistant.content[1]["id"], output[1]["call_id"]);
        assert_ne!(assistant.content[0]["name"], "apply_patch");
        assert_ne!(assistant.content[1]["name"], "list_agents");
    }

    #[test]
    fn parallel_tool_calls_false_never_silently_drops_nonstream_tool_calls() {
        let mut request = base_request("gpt-5.6-sol");
        request["parallel_tool_calls"] = json!(false);
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        let mapped = anthropic_to_response(
            &json!({
                "content": [
                    {"type": "tool_use", "name": "first", "input": {"n": 1}},
                    {"type": "tool_use", "name": "second", "input": {"n": 2}}
                ],
                "stop_reason": "tool_use",
                "usage": {}
            }),
            &meta,
        )
        .unwrap();
        assert_eq!(mapped.response["output"].as_array().unwrap().len(), 2);
        assert_eq!(mapped.response["output"][0]["name"], "first");
        assert_eq!(mapped.response["output"][1]["name"], "second");
        assert_eq!(
            mapped
                .assistant_message
                .unwrap()
                .content
                .as_array()
                .unwrap()
                .len(),
            2
        );
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

    #[tokio::test]
    async fn delayed_upstream_emits_sse_keep_alive_before_headers_arrive() {
        let (release, delayed) = tokio::sync::oneshot::channel::<Response>();
        let upstream = Box::pin(async move {
            delayed
                .await
                .expect("test releases the delayed upstream response")
        });
        let response =
            responses_stream_response_from_upstream(upstream, base_meta("gpt-5.6-sol"), None);
        assert_eq!(response.status(), StatusCode::OK);

        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_millis(100), body.next())
            .await
            .expect("keep-alive must be emitted without waiting for upstream headers")
            .expect("stream must remain open")
            .expect("keep-alive chunk must be readable");
        assert_eq!(first, Bytes::from_static(b": keep-alive\n\n"));

        drop(release);
    }

    #[tokio::test]
    async fn stream_switches_to_keep_alive_when_upstream_misses_grace_period() {
        let (_release, delayed) = tokio::sync::oneshot::channel::<Response>();
        let upstream = Box::pin(async move {
            delayed
                .await
                .expect("test keeps the upstream pending past the grace period")
        });
        let response = responses_stream_from_upstream(
            upstream,
            base_meta("gpt-5.6-sol"),
            None,
            Duration::from_millis(10),
        )
        .await;

        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_millis(100), body.next())
            .await
            .expect("keep-alive must arrive before the public gateway timeout")
            .expect("stream must remain open")
            .expect("keep-alive chunk must be readable");
        assert_eq!(first, RESPONSES_KEEP_ALIVE);
    }

    #[test]
    fn structured_stream_never_forwards_text_after_first_json_value() {
        let mut request = base_request("gpt-5.6-sol");
        request["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": "workspace_summary",
                "schema": {
                    "type": "object",
                    "properties": {"safe": {"type": "boolean"}},
                    "required": ["safe"],
                    "additionalProperties": false
                },
                "strict": true
            }
        });
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
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
                json!({"index": 0, "delta": {"type": "text_delta", "text": "{\"safe\":true}"}}),
            ),
            (
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": " 助手：ChatGPT。"}}),
            ),
            ("content_block_stop", json!({"index": 0})),
            (
                "message_delta",
                json!({"usage": {"output_tokens": 4}, "delta": {"stop_reason": "end_turn"}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let output = events
            .iter()
            .map(|(name, event)| state.transform(name, event))
            .collect::<String>();
        let parsed = parsed_events(&output);
        let deltas = parsed
            .iter()
            .filter(|(name, _)| name == "response.output_text.delta")
            .filter_map(|(_, event)| event.get("delta").and_then(Value::as_str))
            .collect::<String>();
        let done = parsed
            .iter()
            .find(|(name, _)| name == "response.output_text.done")
            .and_then(|(_, event)| event.get("text").and_then(Value::as_str));

        assert_eq!(deltas, r#"{"safe":true}"#);
        assert_eq!(done, Some(r#"{"safe":true}"#));
        assert!(!output.contains("ChatGPT"), "{output}");
        assert!(output.contains("event: response.completed"), "{output}");
    }

    #[test]
    fn stream_lifecycle_preserves_public_request_metadata() {
        let request = realistic_codex_request();
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        let mut state = ResponsesStreamState::new(meta);
        let output = [
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 4}}}),
            ),
            (
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "OK"}}),
            ),
            ("content_block_stop", json!({"index": 0})),
            ("message_stop", json!({})),
        ]
        .iter()
        .map(|(name, event)| state.transform(name, event))
        .collect::<String>();
        let events = parsed_events(&output);
        let response_for = |event_name: &str| {
            events
                .iter()
                .find(|(name, _)| name == event_name)
                .map(|(_, event)| event["response"].clone())
                .unwrap()
        };
        let created = response_for("response.created");
        let in_progress = response_for("response.in_progress");
        let completed = response_for("response.completed");
        for field in ["tools", "tool_choice", "parallel_tool_calls", "text"] {
            assert_eq!(created[field], completed[field], "field {field}");
            assert_eq!(in_progress[field], completed[field], "field {field}");
        }
        assert_eq!(completed["tools"], request["input"][0]["tools"]);
        assert_eq!(completed["parallel_tool_calls"], false);
        assert_eq!(completed["text"], request["text"]);
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
    fn transforms_custom_stream_into_codex_custom_tool_events() {
        let request = json!({
            "model": "gpt-5.6-sol",
            "input": "Apply the patch",
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
                "format": {
                    "type": "grammar",
                    "syntax": "lark",
                    "definition": "start: patch\npatch: /.+/s"
                }
            }]
        });
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        let custom_name = meta
            .tool_catalog
            .kiro_name(&PublicToolKey::custom(None, "apply_patch"))
            .unwrap()
            .to_string();
        let raw_input = "*** Begin Patch\n*** End Patch";
        let arguments = serde_json::to_string(&json!({"input": raw_input})).unwrap();
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
                        "name": custom_name,
                        "input": {}
                    }
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": arguments}
                }),
            ),
            ("content_block_stop", json!({"index": 0})),
            ("message_stop", json!({})),
        ]
        .iter()
        .map(|(name, event)| state.transform(name, event))
        .collect::<String>();
        assert!(output.contains("response.custom_tool_call_input.delta"));
        assert!(output.contains("response.custom_tool_call_input.done"));
        assert!(!output.contains("response.function_call_arguments"));
        let events = parsed_events(&output);
        let delta = events
            .iter()
            .find(|(name, _)| name == "response.custom_tool_call_input.delta")
            .map(|(_, event)| event)
            .unwrap();
        let done = events
            .iter()
            .find(|(name, _)| name == "response.custom_tool_call_input.done")
            .map(|(_, event)| event)
            .unwrap();
        let item_done = events
            .iter()
            .find(|(name, _)| name == "response.output_item.done")
            .map(|(_, event)| event)
            .unwrap();
        assert_eq!(delta["delta"], raw_input);
        assert_eq!(done["input"], raw_input);
        assert_eq!(item_done["item"]["type"], "custom_tool_call");
        assert_eq!(item_done["item"]["name"], "apply_patch");
        assert_eq!(item_done["item"]["input"], raw_input);
        assert!(
            item_done["item"]["id"]
                .as_str()
                .unwrap()
                .starts_with("ctc_")
        );
    }

    #[test]
    fn parallel_tool_calls_false_never_silently_drops_streamed_tool_calls() {
        let mut request = base_request("gpt-5.6-sol");
        request["parallel_tool_calls"] = json!(false);
        let (_, meta) = translated_request(&request, "gpt-5.6-sol").unwrap();
        let mut state = ResponsesStreamState::new(meta);
        let output = [
            ("message_start", json!({"message": {"usage": {}}})),
            (
                "content_block_start",
                json!({
                    "index": 0,
                    "content_block": {"type": "tool_use", "name": "first", "input": {}}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"n\":1}"}
                }),
            ),
            ("content_block_stop", json!({"index": 0})),
            (
                "content_block_start",
                json!({
                    "index": 1,
                    "content_block": {"type": "tool_use", "name": "second", "input": {}}
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"n\":2}"}
                }),
            ),
            ("content_block_stop", json!({"index": 1})),
            ("message_stop", json!({})),
        ]
        .iter()
        .map(|(name, event)| state.transform(name, event))
        .collect::<String>();
        let events = parsed_events(&output);
        assert_eq!(
            events
                .iter()
                .filter(|(name, _)| name == "response.output_item.added")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|(name, _)| name == "response.output_item.done")
                .count(),
            2
        );
        let completed = events
            .iter()
            .find(|(name, _)| name == "response.completed")
            .map(|(_, event)| event)
            .unwrap();
        assert_eq!(completed["response"]["output"].as_array().unwrap().len(), 2);
        assert_eq!(completed["response"]["parallel_tool_calls"], false);
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
