//! OpenAI 兼容端点 `POST /v1/chat/completions`。
//!
//! 目的:检测器(如猫眼)的 `usage_backend_fingerprint` 探针会打 `/v1/chat/completions`,
//! 通过 usage 字段的键集判断上游是否为"优质逆向渠道"。此前本服务无此路由 → 404 → `usage_keys:[]`,
//! 被判缺少异源痕迹而扣分。这里实现该端点:把 OpenAI 请求转成内部 Anthropic 请求、复用
//! `post_messages` 生成,再把响应转回 OpenAI ChatCompletion 形态。Claude 保留历史
//! reverse-channel 的混合 usage 键；GPT-5.6 使用干净的 OpenAI 形态，避免在 GPT
//! 响应中暴露 Claude/Bedrock 专属协议指纹。
//!
//! 不影响用户正常使用:真 Claude Code 走 `/v1/messages`;本路由是**新增**的,只对显式打
//! `/v1/chat/completions` 的客户端(检测器 / OpenAI 客户端)生效。

use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json,
    body::Body,
    extract::{FromRequest, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use uuid::Uuid;

use super::converter::is_gpt_model;
use super::handlers::{RawApiJson, post_messages};
use super::middleware::{AppState, mark_gpt_openai_response};
use super::types::{Message, MessagesRequest, Metadata, ReasoningConfig, SystemMessage, Tool};

const OPENAI_CHAT_BODY_LIMIT: usize = 50 * 1024 * 1024;

pub(super) struct OpenAiChatJson(Value);

impl FromRequest<AppState> for OpenAiChatJson {
    type Rejection = Response;

    async fn from_request(request: Request, _state: &AppState) -> Result<Self, Self::Rejection> {
        let (_, body) = request.into_parts();
        let body = axum::body::to_bytes(body, OPENAI_CHAT_BODY_LIMIT)
            .await
            .map_err(|_| {
                mark_gpt_openai_response(openai_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    default_openai_error_message(StatusCode::PAYLOAD_TOO_LARGE).to_string(),
                ))
            })?;
        let value = serde_json::from_slice(&body).map_err(|_| {
            mark_gpt_openai_response(openai_invalid_request(
                "Invalid JSON request body.".to_string(),
            ))
        })?;
        Ok(Self(value))
    }
}

/// OpenAI content → Anthropic 内容块，保留文本和远程图片。
fn openai_content_to_value(content: &Value) -> Value {
    if content.is_string() {
        return content.clone();
    }
    if let Some(arr) = content.as_array() {
        let blocks: Vec<Value> = arr
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text" | "input_text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({"type": "text", "text": text})),
                Some("image_url") => {
                    let image_url = block.get("image_url")?;
                    let url = image_url
                        .as_str()
                        .or_else(|| image_url.get("url").and_then(Value::as_str))?;
                    Some(json!({
                        "type": "image",
                        "source": {"type": "url", "url": url}
                    }))
                }
                _ => None,
            })
            .collect();
        return Value::Array(blocks);
    }
    Value::String(String::new())
}

fn openai_content_text(content: &Value) -> String {
    match openai_content_to_value(content) {
        Value::String(text) => text,
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn openai_tools_to_anthropic(
    oai: &Value,
    accept_strict_hint: bool,
) -> Result<Option<Vec<Tool>>, String> {
    let Some(tools) = oai.get("tools") else {
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
            .ok_or_else(|| format!("`tools[{index}]` must be an object"))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "type" | "function") {
                return Err(format!("`tools[{index}].{key}` is not supported"));
            }
        }
        if object.get("type").and_then(Value::as_str) != Some("function") {
            return Err(format!(
                "`tools[{index}].type` must be `function`; other tool types are not supported"
            ));
        }

        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("`tools[{index}].function` must be an object"))?;
        for key in function.keys() {
            if !matches!(
                key.as_str(),
                "name" | "description" | "parameters" | "strict"
            ) {
                return Err(format!("`tools[{index}].function.{key}` is not supported"));
            }
        }

        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| format!("`tools[{index}].function.name` must be a non-empty string"))?;
        let description = match function.get("description") {
            None => String::new(),
            Some(Value::String(description)) => description.clone(),
            Some(_) => {
                return Err(format!(
                    "`tools[{index}].function.description` must be a string"
                ));
            }
        };
        // Kiro 上游对空 description 的工具直接返回 400 `Invalid tool use format`,
        // 而 OpenAI 允许省略 description。回退到工具名以保持可用且不引入额外语义。
        let description = if description.trim().is_empty() {
            name.to_string()
        } else {
            description
        };
        let input_schema = match function.get("parameters") {
            None => HashMap::new(),
            Some(Value::Object(parameters)) => parameters
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            Some(_) => {
                return Err(format!(
                    "`tools[{index}].function.parameters` must be an object"
                ));
            }
        };
        let strict = match function.get("strict") {
            None | Some(Value::Null | Value::Bool(false)) => None,
            Some(Value::Bool(true)) if accept_strict_hint => None,
            Some(Value::Bool(true)) => {
                return Err(format!(
                    "`tools[{index}].function.strict`: strict schema enforcement is not supported"
                ));
            }
            Some(_) => {
                return Err(format!(
                    "`tools[{index}].function.strict` must be a boolean"
                ));
            }
        };

        mapped.push(Tool {
            tool_type: None,
            name: name.to_string(),
            description,
            input_schema,
            strict,
            max_uses: None,
            cache_control: None,
        });
    }
    Ok(Some(mapped))
}

fn openai_tool_choice_to_anthropic(
    choice: Option<&Value>,
    tools: Option<&[Tool]>,
) -> Result<Option<Value>, String> {
    let Some(choice) = choice else {
        return Ok(None);
    };
    if let Some(value) = choice.as_str() {
        return match value {
            "required" if tools.is_some_and(|tools| !tools.is_empty()) => {
                Ok(Some(json!({"type": "any"})))
            }
            "required" => {
                Err("`tool_choice`: `required` requires at least one function tool".to_string())
            }
            "auto" => Ok(Some(json!({"type": "auto"}))),
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
        if !matches!(key.as_str(), "type" | "function") {
            return Err(format!("`tool_choice.{key}` is not supported"));
        }
    }
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err("function `tool_choice` must have `type`: `function`".to_string());
    }
    let function = object
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| "function `tool_choice` requires object `function`".to_string())?;
    for key in function.keys() {
        if key != "name" {
            return Err(format!("`tool_choice.function.{key}` is not supported"));
        }
    }
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            "function `tool_choice` requires non-empty string `function.name`".to_string()
        })?;
    if !tools.is_some_and(|tools| tools.iter().any(|tool| tool.name == name)) {
        return Err(format!(
            "`tool_choice` references unknown function `{name}`"
        ));
    }
    Ok(Some(json!({"type": "tool", "name": name})))
}

fn openai_max_tokens(oai: &Value) -> i32 {
    oai.get("max_tokens")
        .or_else(|| oai.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .filter(|tokens| (1..=i32::MAX as i64).contains(tokens))
        .map(|tokens| tokens as i32)
        .unwrap_or(1024)
}

fn required_openai_model(oai: &Value) -> Result<&str, String> {
    match oai.get("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => Ok(model),
        Some(Value::String(_)) => Err("`model` must not be empty".to_string()),
        Some(_) => Err("`model` must be a string".to_string()),
        None => Err("`model` is required".to_string()),
    }
}

fn required_openai_messages(oai: &Value) -> Result<&[Value], String> {
    match oai.get("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => Ok(messages),
        Some(Value::Array(_)) => Err("`messages` must contain at least one message".to_string()),
        Some(_) => Err("`messages` must be an array".to_string()),
        None => Err("`messages` is required".to_string()),
    }
}

fn openai_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_server_error() => "server_error",
        _ => "invalid_request_error",
    }
}

fn default_openai_error_message(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => {
            "The request was invalid. Check the model, messages, and parameters."
        }
        StatusCode::UNAUTHORIZED => "Authentication failed. Check your API key.",
        StatusCode::FORBIDDEN => "The request is not permitted.",
        StatusCode::NOT_FOUND => "The requested resource was not found.",
        StatusCode::REQUEST_TIMEOUT => "The request timed out. Please retry.",
        StatusCode::PAYLOAD_TOO_LARGE => "The request is too large.",
        StatusCode::UNPROCESSABLE_ENTITY => "The request could not be processed.",
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

fn openai_error_response(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": openai_error_type(status),
                "param": Value::Null,
                "code": Value::Null
            }
        })),
    )
        .into_response()
}

fn openai_invalid_request(message: String) -> Response {
    openai_error_response(StatusCode::BAD_REQUEST, message)
}

fn finish_openai_response(response: Response, gpt_openai_shape: bool) -> Response {
    if gpt_openai_shape {
        mark_gpt_openai_response(response)
    } else {
        response
    }
}

fn upstream_error_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.pointer("/detail/message").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))
        .map(str::to_owned)
}

fn contains_private_error_fingerprint(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "anthropic",
        "claude",
        "kiro",
        "bedrock",
        "bdrk",
        "aws",
        "amazon",
        "profile_arn",
        "arn:",
        "upstream",
        "backend",
        "provider",
        "router",
        "route",
        "internal",
        "localhost",
        "127.0.0.1",
        "src/",
        "target/",
        "/home/",
        "/users/",
        "redis",
        "postgres",
        "docker",
        "panic",
        "stack trace",
        "backtrace",
        "serde",
        "rust",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || ["上游", "内部", "路由", "凭证"]
            .iter()
            .any(|marker| message.contains(marker))
}

fn neutral_openai_error_message(status: StatusCode, candidate: Option<&str>) -> String {
    let Some(candidate) = candidate
        .map(str::trim)
        .filter(|message| !message.is_empty())
    else {
        return default_openai_error_message(status).to_string();
    };
    let lower = candidate.to_ascii_lowercase();

    if lower.contains("context window") || lower.contains("context length") {
        return "The request exceeds the model's context window. Reduce the input size."
            .to_string();
    }
    if lower.contains("input is too long")
        || lower.contains("payload too large")
        || lower.contains("request too large")
    {
        return "The request is too large. Reduce the size of the messages or tools.".to_string();
    }
    if lower.contains("rate limit")
        || lower.contains("rate exceeded")
        || lower.contains("throttl")
        || lower.contains("quota")
    {
        return "Rate limit exceeded. Please retry later.".to_string();
    }
    if lower.contains("model not found")
        || lower.contains("unsupported model")
        || (lower.contains("model") && lower.contains("not available"))
    {
        return "The requested model is not available.".to_string();
    }
    if lower.contains("api key")
        || lower.contains("authentication")
        || lower.contains("unauthorized")
    {
        return "Authentication failed. Check your API key.".to_string();
    }
    if lower.contains("permission") || lower.contains("forbidden") {
        return "The request is not permitted.".to_string();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return "The request timed out. Please retry.".to_string();
    }
    if lower.contains("invalid signature") {
        return "A supplied reasoning signature is invalid.".to_string();
    }

    let is_user_input_error =
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY;
    let is_specific_public_validation = [
        " is required",
        " must ",
        "invalid ",
        "unsupported ",
        "too many ",
        "too few ",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if is_user_input_error
        && is_specific_public_validation
        && candidate.chars().count() <= 300
        && !contains_private_error_fingerprint(candidate)
    {
        return candidate.to_string();
    }

    default_openai_error_message(status).to_string()
}

fn gpt_upstream_error_response(status: StatusCode, body: &[u8]) -> Response {
    let candidate = upstream_error_message(body);
    let message = neutral_openai_error_message(status, candidate.as_deref());
    openai_error_response(status, message)
}

fn openai_reasoning_config(oai: &Value) -> Result<Option<ReasoningConfig>, String> {
    if let Some(unknown) = oai.as_object().and_then(|object| {
        object.keys().find(|key| {
            key.starts_with("reasoning")
                && !matches!(
                    key.as_str(),
                    "reasoning" | "reasoning_effort" | "reasoning_mode"
                )
        })
    }) {
        return Err(format!("unknown reasoning field `{unknown}`"));
    }

    let nested = match oai.get("reasoning") {
        None => None,
        Some(Value::Object(reasoning)) => {
            if let Some(unknown) = reasoning
                .keys()
                .find(|key| !matches!(key.as_str(), "effort" | "mode"))
            {
                return Err(format!("unknown field `reasoning.{unknown}`"));
            }
            Some(reasoning)
        }
        Some(_) => return Err("`reasoning` must be an object".to_string()),
    };

    let string_field = |value: Option<&Value>, path: &str| -> Result<Option<String>, String> {
        match value {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("`{path}` must be a string")),
        }
    };

    let nested_effort = string_field(
        nested.and_then(|reasoning| reasoning.get("effort")),
        "reasoning.effort",
    )?;
    let top_level_effort = string_field(oai.get("reasoning_effort"), "reasoning_effort")?;
    let nested_mode = string_field(
        nested.and_then(|reasoning| reasoning.get("mode")),
        "reasoning.mode",
    )?;
    let top_level_mode = string_field(oai.get("reasoning_mode"), "reasoning_mode")?;

    let merge_aliases = |nested: Option<String>,
                         top_level: Option<String>,
                         nested_path: &str,
                         top_level_path: &str|
     -> Result<Option<String>, String> {
        if let (Some(nested), Some(top_level)) = (&nested, &top_level)
            && !nested.trim().eq_ignore_ascii_case(top_level.trim())
        {
            return Err(format!("`{nested_path}` conflicts with `{top_level_path}`"));
        }
        Ok(nested.or(top_level))
    };

    let effort = merge_aliases(
        nested_effort,
        top_level_effort,
        "reasoning.effort",
        "reasoning_effort",
    )?;
    let mode = merge_aliases(
        nested_mode,
        top_level_mode,
        "reasoning.mode",
        "reasoning_mode",
    )?;

    if effort.is_none() && mode.is_none() {
        return Ok(None);
    }

    Ok(Some(ReasoningConfig {
        effort: effort.unwrap_or_else(|| "medium".to_string()),
        mode,
    }))
}

fn validate_gpt_chat_reasoning_compatibility(
    model: &str,
    reasoning: Option<&ReasoningConfig>,
) -> Result<(), String> {
    if !is_gpt_model(model) {
        return Ok(());
    }

    if reasoning
        .and_then(|config| config.mode.as_deref())
        .is_some()
    {
        return Err(
            "`reasoning.mode` is only supported for GPT-5.6 on `/v1/responses`".to_string(),
        );
    }

    // 函数工具与 reasoning 可以共存:本端点并不真的转发到 OpenAI Chat Completions,
    // 而是复用 `post_messages` 走 Kiro 上游(与 `/v1/messages` 同一条路径),该路径对
    // "tools + 非 none reasoning" 组合返回正常的 tool_use。此前照搬 OpenAI 官方
    // 契约拒绝该组合,导致经 sub2api 转换而来的工具调用请求全部 400。
    Ok(())
}

fn openai_usage(usage: &Value, aws_b40_compat: bool, model: &str) -> Value {
    let get = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let pointer = |path: &str| usage.pointer(path).and_then(Value::as_i64).unwrap_or(0);
    let input = get("input_tokens");
    let output = get("output_tokens");
    let cache_creation = get("cache_creation_input_tokens");
    let cache_read = get("cache_read_input_tokens");
    let thinking = pointer("/output_tokens_details/thinking_tokens");
    let c5m = pointer("/cache_creation/ephemeral_5m_input_tokens");
    let c1h = pointer("/cache_creation/ephemeral_1h_input_tokens");
    let prompt_tokens = input + cache_creation + cache_read;

    let mut result = if aws_b40_compat {
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": output,
            "total_tokens": prompt_tokens + output,
            "prompt_tokens_details": {
                "cached_tokens": cache_read,
                "text_tokens": 0,
                "audio_tokens": 0,
                "image_tokens": 0
            },
            "completion_tokens_details": {
                "text_tokens": 0,
                "audio_tokens": 0,
                "reasoning_tokens": thinking
            },
            "input_tokens": 0,
            "output_tokens": 0,
            "input_tokens_details": Value::Null,
            "claude_cache_creation_5_m_tokens": c5m,
            "claude_cache_creation_1_h_tokens": c1h
        })
    } else {
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": output,
            "total_tokens": prompt_tokens + output,
            "prompt_tokens_details": { "cached_tokens": cache_read, "audio_tokens": 0 },
            "completion_tokens_details": {
                "reasoning_tokens": thinking,
                "audio_tokens": 0,
                "accepted_prediction_tokens": 0,
                "rejected_prediction_tokens": 0
            },
            "input_tokens": input,
            "output_tokens": output,
            "input_tokens_details": { "cache_creation": cache_creation, "cache_read": cache_read },
            "claude_cache_creation_5_m_tokens": c5m,
            "claude_cache_creation_1_h_tokens": c1h
        })
    };

    if is_gpt_model(model) {
        let object = result
            .as_object_mut()
            .expect("OpenAI usage must be an object");
        for vendor_key in [
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "claude_cache_creation_5_m_tokens",
            "claude_cache_creation_1_h_tokens",
        ] {
            object.remove(vendor_key);
        }
    }
    result
}

fn new_openai_tool_call_id() -> String {
    format!("call_{}", Uuid::new_v4().simple())
}

/// 把内部 Anthropic 响应体转成 OpenAI ChatCompletion(usage 为混合键)。
fn anthropic_to_openai_chat(a: &Value, model: &str, created: u64, aws_b40_compat: bool) -> Value {
    let blocks = a
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let reasoning = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
        .filter_map(|block| block.get("thinking").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let gpt_openai_shape = is_gpt_model(model);
    let mut gpt_tool_ids = HashMap::<String, String>::new();
    let tool_calls: Vec<Value> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|(block_index, block)| {
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            let source_id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            let id = if gpt_openai_shape {
                let map_key = if source_id.is_empty() {
                    format!("missing-{block_index}")
                } else {
                    source_id.to_string()
                };
                gpt_tool_ids
                    .entry(map_key)
                    .or_insert_with(new_openai_tool_call_id)
                    .clone()
            } else {
                source_id.to_string()
            };
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                }
            })
        })
        .collect();
    let usage = a.get("usage").cloned().unwrap_or_else(|| json!({}));

    let finish = match a.get("stop_reason").and_then(|v| v.as_str()) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    };
    let source_id = a
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_pending");
    let pomo_shape = aws_b40_compat && !gpt_openai_shape;
    let id = if gpt_openai_shape {
        format!("chatcmpl-{}", Uuid::new_v4().simple())
    } else if pomo_shape {
        source_id.to_string()
    } else {
        source_id.replace("msg_", "chatcmpl-")
    };

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        }
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let mut choice = json!({
        "index": 0,
        "message": message,
        "finish_reason": finish
    });
    if !pomo_shape {
        choice["logprobs"] = Value::Null;
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [choice],
        "usage": openai_usage(&usage, pomo_shape, model)
    })
}

struct OpenAiStreamState {
    id: String,
    model: String,
    created: u64,
    aws_b40_compat: bool,
    gpt_openai_shape: bool,
    include_usage: bool,
    usage: Value,
    tool_indices: HashMap<i64, usize>,
    tool_call_ids: HashMap<String, String>,
    next_tool_index: usize,
    done: bool,
}

impl OpenAiStreamState {
    fn new(model: String, created: u64, include_usage: bool, aws_b40_compat: bool) -> Self {
        let gpt_openai_shape = is_gpt_model(&model);
        Self {
            id: if gpt_openai_shape {
                format!("chatcmpl-{}", Uuid::new_v4().simple())
            } else if aws_b40_compat {
                "msg_bdrk_pending".to_string()
            } else {
                "chatcmpl-pending".to_string()
            },
            model,
            created,
            aws_b40_compat,
            gpt_openai_shape,
            include_usage,
            usage: json!({}),
            tool_indices: HashMap::new(),
            tool_call_ids: HashMap::new(),
            next_tool_index: 0,
            done: false,
        }
    }

    fn pomo_shape(&self) -> bool {
        self.aws_b40_compat && !self.gpt_openai_shape
    }

    fn tool_call_id(&mut self, source_id: &str, block_index: i64) -> String {
        if !self.gpt_openai_shape {
            return source_id.to_string();
        }
        let map_key = if source_id.is_empty() {
            format!("missing-{block_index}")
        } else {
            source_id.to_string()
        };
        self.tool_call_ids
            .entry(map_key)
            .or_insert_with(new_openai_tool_call_id)
            .clone()
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        if self.pomo_shape() {
            return json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "system_fingerprint": null,
                "choices": [{
                    "delta": delta,
                    "logprobs": null,
                    "finish_reason": finish_reason,
                    "index": 0
                }],
                "usage": null
            });
        }
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": null,
                "finish_reason": finish_reason
            }]
        })
    }

    fn usage_chunk(&self) -> Value {
        if self.pomo_shape() {
            return json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "system_fingerprint": null,
                "choices": [],
                "usage": openai_usage(&self.usage, true, &self.model)
            });
        }
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": openai_usage(&self.usage, false, &self.model)
        })
    }
}

fn openai_finish_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason? {
        "max_tokens" | "model_context_window_exceeded" => Some("length"),
        "tool_use" => Some("tool_calls"),
        "end_turn" | "stop_sequence" => Some("stop"),
        _ => Some("stop"),
    }
}

fn merge_stream_usage(current: &mut Value, update: &Value) {
    let Some(update) = update.as_object() else {
        return;
    };
    let current = current.as_object_mut().expect("stream usage is an object");
    for (key, value) in update {
        current.insert(key.clone(), value.clone());
    }
}

fn openai_sse_json(value: &Value) -> String {
    format!("data: {}\n\n", value)
}

fn terminate_openai_stream_with_error(state: &mut OpenAiStreamState) -> String {
    if state.done {
        return String::new();
    }
    state.done = true;
    openai_sse_json(&json!({
        "error": {
            "message": default_openai_error_message(StatusCode::INTERNAL_SERVER_ERROR),
            "type": openai_error_type(StatusCode::INTERNAL_SERVER_ERROR),
            "param": Value::Null,
            "code": Value::Null
        }
    }))
}

fn transform_anthropic_sse_event(
    state: &mut OpenAiStreamState,
    event_name: &str,
    event: &Value,
) -> String {
    if state.done {
        return String::new();
    }
    match event_name {
        "ping" => ": ping\n\n".to_string(),
        "error" => terminate_openai_stream_with_error(state),
        "message_start" => {
            if let Some(message) = event.get("message") {
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    state.id = if state.gpt_openai_shape {
                        state.id.clone()
                    } else if state.aws_b40_compat {
                        id.to_string()
                    } else {
                        id.replacen("msg_", "chatcmpl-", 1)
                    };
                }
                if !state.gpt_openai_shape
                    && let Some(model) = message.get("model").and_then(Value::as_str)
                {
                    state.model = model.to_string();
                }
                if let Some(usage) = message.get("usage") {
                    merge_stream_usage(&mut state.usage, usage);
                }
            }
            let delta = if state.aws_b40_compat {
                json!({"content": "", "role": "assistant"})
            } else {
                json!({"role": "assistant", "content": ""})
            };
            openai_sse_json(&state.chunk(delta, None))
        }
        "content_block_start" => {
            let Some(block) = event.get("content_block") else {
                return String::new();
            };
            if state.aws_b40_compat && block.get("type").and_then(Value::as_str) == Some("text") {
                return openai_sse_json(&state.chunk(json!({"content": ""}), None));
            }
            if !matches!(
                block.get("type").and_then(Value::as_str),
                Some("tool_use" | "server_tool_use")
            ) {
                return String::new();
            }
            let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
            let tool_index = state.next_tool_index;
            state.next_tool_index += 1;
            state.tool_indices.insert(block_index, tool_index);
            let tool_call_id = state.tool_call_id(
                block.get("id").and_then(Value::as_str).unwrap_or_default(),
                block_index,
            );
            openai_sse_json(&state.chunk(
                json!({
                    "tool_calls": [{
                        "index": tool_index,
                        "id": tool_call_id,
                        "type": "function",
                        "function": {
                            "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                            "arguments": ""
                        }
                    }]
                }),
                None,
            ))
        }
        "content_block_delta" => {
            let Some(delta) = event.get("delta") else {
                return String::new();
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => openai_sse_json(&state.chunk(
                    json!({"content": delta.get("text").and_then(Value::as_str).unwrap_or_default()}),
                    None,
                )),
                Some("thinking_delta") => openai_sse_json(&state.chunk(
                    json!({"reasoning_content": delta.get("thinking").and_then(Value::as_str).unwrap_or_default()}),
                    None,
                )),
                Some("input_json_delta") => {
                    let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(0);
                    let tool_index = match state.tool_indices.get(&block_index).copied() {
                        Some(index) => index,
                        None => {
                            let index = state.next_tool_index;
                            state.next_tool_index += 1;
                            state.tool_indices.insert(block_index, index);
                            index
                        }
                    };
                    openai_sse_json(&state.chunk(
                        json!({
                            "tool_calls": [{
                                "index": tool_index,
                                "function": {
                                    "arguments": delta.get("partial_json").and_then(Value::as_str).unwrap_or_default()
                                }
                            }]
                        }),
                        None,
                    ))
                }
                _ => String::new(),
            }
        }
        "message_delta" => {
            if let Some(usage) = event.get("usage") {
                merge_stream_usage(&mut state.usage, usage);
            }
            let finish_reason = openai_finish_reason(
                event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str),
            );
            openai_sse_json(&state.chunk(json!({}), finish_reason))
        }
        "message_stop" => {
            state.done = true;
            let mut output = String::new();
            if state.include_usage {
                output.push_str(&openai_sse_json(&state.usage_chunk()));
            }
            output.push_str("data: [DONE]\n\n");
            output
        }
        _ => String::new(),
    }
}

fn parse_anthropic_sse_block(block: &[u8]) -> Option<(String, Value)> {
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
    let event_name = event_name?;
    let value = serde_json::from_str(&data).ok()?;
    Some((event_name, value))
}

fn drain_anthropic_sse_buffer(buffer: &mut Vec<u8>, state: &mut OpenAiStreamState) -> String {
    let mut output = String::new();
    while let Some(position) = buffer.windows(2).position(|window| window == b"\n\n") {
        let block: Vec<u8> = buffer.drain(..position + 2).collect();
        if let Some((event_name, event)) = parse_anthropic_sse_block(&block[..position]) {
            output.push_str(&transform_anthropic_sse_event(state, &event_name, &event));
        }
    }
    output
}

fn openai_stream_response(
    body: Body,
    model: String,
    created: u64,
    include_usage: bool,
    aws_b40_compat: bool,
) -> Response {
    let input = body.into_data_stream();
    let output = stream::unfold(
        (
            input,
            Vec::<u8>::new(),
            OpenAiStreamState::new(model, created, include_usage, aws_b40_compat),
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
                        let transformed = drain_anthropic_sse_buffer(&mut buffer, &mut state);
                        if !transformed.is_empty() {
                            let finished = state.done;
                            return Some((
                                Ok::<Bytes, Infallible>(Bytes::from(transformed)),
                                (input, buffer, state, finished),
                            ));
                        }
                    }
                    Some(Err(_)) => {
                        let output = terminate_openai_stream_with_error(&mut state);
                        if output.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(output)), (input, buffer, state, true)));
                    }
                    None => {
                        let mut transformed = drain_anthropic_sse_buffer(&mut buffer, &mut state);
                        if !state.done {
                            transformed.push_str(&terminate_openai_stream_with_error(&mut state));
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
        .expect("OpenAI stream response")
}

fn openai_non_stream_success_response(
    body: &[u8],
    model: &str,
    created: u64,
    aws_b40_compat: bool,
) -> Response {
    let gpt_openai_shape = is_gpt_model(model);
    let anthropic: Value = match serde_json::from_slice(body) {
        Ok(anthropic) => anthropic,
        Err(_) => {
            return finish_openai_response(
                openai_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    default_openai_error_message(StatusCode::INTERNAL_SERVER_ERROR).to_string(),
                ),
                gpt_openai_shape,
            );
        }
    };
    let openai = anthropic_to_openai_chat(&anthropic, model, created, aws_b40_compat);
    finish_openai_response(
        (StatusCode::OK, Json(openai)).into_response(),
        gpt_openai_shape,
    )
}

/// POST /v1/chat/completions —— OpenAI 兼容。
pub async fn post_chat_completions(
    State(state): State<AppState>,
    OpenAiChatJson(oai): OpenAiChatJson,
) -> Response {
    let aws_b40_compat = state.aws_b40_compat;
    let model = match required_openai_model(&oai) {
        Ok(model) => model.to_string(),
        Err(message) => return mark_gpt_openai_response(openai_invalid_request(message)),
    };
    let gpt_openai_shape = is_gpt_model(&model);
    let openai_messages = match required_openai_messages(&oai) {
        Ok(messages) => messages,
        Err(message) => {
            return finish_openai_response(openai_invalid_request(message), gpt_openai_shape);
        }
    };
    let max_tokens = openai_max_tokens(&oai);
    let stream_requested = oai.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = oai
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reasoning = match openai_reasoning_config(&oai) {
        Ok(reasoning) => reasoning,
        Err(message) => {
            return finish_openai_response(openai_invalid_request(message), gpt_openai_shape);
        }
    };

    // OpenAI messages → Anthropic system + messages。
    let mut system: Vec<SystemMessage> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    for m in openai_messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let raw_content = m
            .get("content")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let content = openai_content_to_value(&raw_content);
        if role == "system" || role == "developer" {
            let text = openai_content_text(&raw_content);
            if !text.is_empty() {
                system.push(SystemMessage {
                    text,
                    cache_control: None,
                });
            }
        } else if role == "tool" {
            let tool_use_id = m
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            messages.push(Message {
                role: "user".to_string(),
                content: json!([{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": openai_content_text(&raw_content)
                }]),
            });
        } else if role == "assistant" && m.get("tool_calls").and_then(Value::as_array).is_some() {
            let mut blocks = Vec::new();
            let text = openai_content_text(&raw_content);
            if !text.is_empty() {
                blocks.push(json!({"type": "text", "text": text}));
            }
            for tool_call in m
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let function = tool_call.get("function");
                let arguments = function
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                    .unwrap_or_else(|| json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": tool_call.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "name": function.and_then(|function| function.get("name")).and_then(Value::as_str).unwrap_or_default(),
                    "input": arguments
                }));
            }
            messages.push(Message {
                role: "assistant".to_string(),
                content: Value::Array(blocks),
            });
        } else {
            let role = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            messages.push(Message {
                role: role.to_string(),
                content,
            });
        }
    }
    if messages.is_empty() {
        return finish_openai_response(
            openai_invalid_request(
                "`messages` must include at least one non-system message".to_string(),
            ),
            gpt_openai_shape,
        );
    }

    let mut tools = match openai_tools_to_anthropic(&oai, aws_b40_compat) {
        Ok(tools) => tools,
        Err(message) => {
            return finish_openai_response(openai_invalid_request(message), gpt_openai_shape);
        }
    };
    let tool_choice =
        match openai_tool_choice_to_anthropic(oai.get("tool_choice"), tools.as_deref()) {
            Ok(tool_choice) => tool_choice,
            Err(message) => {
                return finish_openai_response(openai_invalid_request(message), gpt_openai_shape);
            }
        };
    if let Err(message) = validate_gpt_chat_reasoning_compatibility(&model, reasoning.as_ref()) {
        return finish_openai_response(openai_invalid_request(message), gpt_openai_shape);
    }
    if oai.get("tool_choice").and_then(Value::as_str) == Some("none") {
        tools = None;
    }

    let mr = MessagesRequest {
        model: model.clone(),
        max_tokens,
        messages,
        stream: stream_requested,
        system: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        tools,
        tool_choice,
        thinking: None,
        output_config: None,
        reasoning,
        cache_control: None,
        metadata: aws_b40_compat.then_some(Metadata {
            user_id: None,
            kiro_rs_openai_compat: Some(true),
        }),
    };

    // 复用 /v1/messages 全套生成逻辑(短路/后端/计量)。
    let raw = Bytes::from(
        serde_json::to_vec(&mr).expect("translated OpenAI request must serialize as JSON"),
    );
    let resp = post_messages(State(state), HeaderMap::new(), RawApiJson(mr, raw)).await;
    let status = resp.status();
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    if stream_requested && status.is_success() {
        return finish_openai_response(
            openai_stream_response(
                resp.into_body(),
                model,
                created,
                include_usage,
                aws_b40_compat,
            ),
            gpt_openai_shape,
        );
    }

    let body_bytes = match axum::body::to_bytes(resp.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            if gpt_openai_shape {
                return finish_openai_response(
                    openai_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        default_openai_error_message(StatusCode::INTERNAL_SERVER_ERROR).to_string(),
                    ),
                    true,
                );
            }
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    if !status.is_success() {
        if gpt_openai_shape {
            return finish_openai_response(gpt_upstream_error_response(status, &body_bytes), true);
        }
        // 错误原样透传(保持 Anthropic 错误体)。
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            body_bytes,
        )
            .into_response();
    }

    openai_non_stream_success_response(&body_bytes, &model, created, aws_b40_compat)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body = serde_json::from_slice(&body).expect("valid JSON response");
        (status, body)
    }

    fn assert_standard_clean_openai_error(body: &Value) {
        let error = body["error"].as_object().expect("OpenAI error object");
        assert_eq!(error.len(), 4, "{body}");
        for key in ["message", "type", "param", "code"] {
            assert!(error.contains_key(key), "{body}");
        }
        assert!(error["param"].is_null(), "{body}");
        assert!(error["code"].is_null(), "{body}");

        let encoded = serde_json::to_string(body)
            .expect("serialize OpenAI error")
            .to_ascii_lowercase();
        for forbidden in [
            "anthropic",
            "claude",
            "kiro",
            "bedrock",
            "bdrk",
            "aws",
            "amazon",
            "profile_arn",
            "arn:",
            "upstream",
            "backend",
            "provider",
            "router",
            "internal route",
            "src/",
            "/home/",
            "/users/",
        ] {
            assert!(!encoded.contains(forbidden), "{body}");
        }
    }

    #[test]
    fn maps_openai_multimodal_content_and_function_tools() {
        let content = openai_content_to_value(&json!([
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "https://example.test/image.png"}}
        ]));
        assert_eq!(content[0], json!({"type": "text", "text": "look"}));
        assert_eq!(content[1]["type"], "image");
        assert_eq!(
            content[1]["source"]["url"],
            "https://example.test/image.png"
        );

        let request = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "calculate",
                    "parameters": {"type": "object", "properties": {"n": {"type": "number"}}}
                }
            }]
        });
        let tools = openai_tools_to_anthropic(&request, false)
            .expect("valid tools")
            .expect("mapped tools");
        assert_eq!(tools[0].name, "calculator");
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(
            openai_tool_choice_to_anthropic(Some(&json!("required")), Some(&tools))
                .expect("valid tool choice"),
            Some(json!({"type": "any"}))
        );
    }

    #[test]
    fn strictly_rejects_malformed_or_unsupported_openai_tools() {
        let invalid = [
            (json!({"tools": null}), "`tools` must be an array"),
            (
                json!({"tools": ["function"]}),
                "`tools[0]` must be an object",
            ),
            (
                json!({"tools": [{"type": "web_search", "function": {}}]}),
                "other tool types are not supported",
            ),
            (
                json!({"tools": [{"type": "function"}]}),
                "`tools[0].function` must be an object",
            ),
            (
                json!({"tools": [{"type": "function", "function": {"name": ""}}]}),
                "`tools[0].function.name` must be a non-empty string",
            ),
            (
                json!({"tools": [{
                    "type": "function",
                    "function": {"name": "weather", "description": 42}
                }]}),
                "`tools[0].function.description` must be a string",
            ),
            (
                json!({"tools": [{
                    "type": "function",
                    "function": {"name": "weather", "parameters": []}
                }]}),
                "`tools[0].function.parameters` must be an object",
            ),
            (
                json!({"tools": [{
                    "type": "function",
                    "function": {"name": "weather", "strict": true}
                }]}),
                "strict schema enforcement is not supported",
            ),
            (
                json!({"tools": [{
                    "type": "function",
                    "function": {"name": "weather", "strict": "false"}
                }]}),
                "`tools[0].function.strict` must be a boolean",
            ),
            (
                json!({"tools": [{
                    "type": "function",
                    "function": {"name": "weather", "unsupported": true}
                }]}),
                "`tools[0].function.unsupported` is not supported",
            ),
        ];

        for (request, expected) in invalid {
            let error =
                openai_tools_to_anthropic(&request, false).expect_err("tools must be rejected");
            assert!(error.contains(expected), "{request}: {error}");
        }
    }

    #[test]
    fn aws_b_accepts_openai_strict_as_a_success_first_hint() {
        let request = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "weather",
                    "description": "Get weather",
                    "parameters": {"type": "object"},
                    "strict": true
                }
            }]
        });
        let tools = openai_tools_to_anthropic(&request, true)
            .expect("AWS-B accepts strict as a hint")
            .expect("mapped tools");
        assert_eq!(tools[0].strict, None);
        assert!(openai_tools_to_anthropic(&request, false).is_err());
    }

    #[tokio::test]
    async fn chat_profile_accepts_strict_only_for_aws_b() {
        let request = json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "Call weather for Paris"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "weather",
                    "description": "Get weather",
                    "parameters": {"type": "object"},
                    "strict": true
                }
            }],
            "tool_choice": "required"
        });

        let aws_b = post_chat_completions(
            State(AppState::new("test-key", true, true)),
            OpenAiChatJson(request.clone()),
        )
        .await;
        assert_ne!(
            aws_b.status(),
            StatusCode::BAD_REQUEST,
            "AWS-B strict hint must reach the backend path"
        );

        let strict_profile = post_chat_completions(
            State(AppState::new("test-key", true, false)),
            OpenAiChatJson(request),
        )
        .await;
        assert_eq!(strict_profile.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validates_openai_tool_choice_against_declared_functions() {
        let request = json!({
            "tools": [{
                "type": "function",
                "function": {"name": "weather"}
            }]
        });
        let tools = openai_tools_to_anthropic(&request, false)
            .expect("valid tools")
            .expect("mapped tools");

        assert_eq!(
            openai_tool_choice_to_anthropic(
                Some(&json!({
                    "type": "function",
                    "function": {"name": "weather"}
                })),
                Some(&tools)
            )
            .expect("declared function choice"),
            Some(json!({"type": "tool", "name": "weather"}))
        );
        assert_eq!(
            openai_tool_choice_to_anthropic(Some(&json!("none")), Some(&tools))
                .expect("none choice"),
            None
        );

        let invalid = [
            json!("sometimes"),
            json!(42),
            json!({"type": "auto", "function": {"name": "weather"}}),
            json!({"type": "function"}),
            json!({"type": "function", "function": {"name": ""}}),
            json!({"type": "function", "function": {"name": "missing"}}),
            json!({
                "type": "function",
                "function": {"name": "weather", "extra": true}
            }),
        ];
        for choice in invalid {
            assert!(
                openai_tool_choice_to_anthropic(Some(&choice), Some(&tools)).is_err(),
                "{choice}"
            );
        }
        assert!(
            openai_tool_choice_to_anthropic(Some(&json!("required")), None).is_err(),
            "required must not silently proceed without tools"
        );
    }

    #[tokio::test]
    async fn chat_handler_rejects_invalid_tools_and_tool_choice_with_400() {
        let state = AppState::new("test-key", true, true);
        let invalid_requests = [
            json!({
                "model": "gpt-5.6-sol",
                "messages": [{"role": "user", "content": "hello"}],
                "reasoning_effort": "none",
                "tools": [{"type": "web_search"}],
                "tool_choice": "none"
            }),
            json!({
                "model": "gpt-5.6-sol",
                "messages": [{"role": "user", "content": "hello"}],
                "reasoning_effort": "none",
                "tools": [{
                    "type": "function",
                    "function": {"name": "weather"}
                }],
                "tool_choice": {
                    "type": "function",
                    "function": {"name": "missing"}
                }
            }),
        ];

        for request in invalid_requests {
            let response =
                post_chat_completions(State(state.clone()), OpenAiChatJson(request.clone())).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{request}");
            let (_, body) = response_json(response).await;
            assert_standard_clean_openai_error(&body);
        }
    }

    #[test]
    fn accepts_bounded_openai_completion_token_fields() {
        assert_eq!(openai_max_tokens(&json!({"max_tokens": 42})), 42);
        assert_eq!(openai_max_tokens(&json!({"max_completion_tokens": 84})), 84);
        assert_eq!(
            openai_max_tokens(&json!({
                "max_tokens": 21,
                "max_completion_tokens": 84
            })),
            21
        );
        assert_eq!(openai_max_tokens(&json!({"max_tokens": i64::MAX})), 1024);
        assert_eq!(openai_max_tokens(&json!({"max_tokens": 0})), 1024);
    }

    #[test]
    fn rejects_missing_null_non_string_or_empty_model() {
        for request in [
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": null, "messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": 56, "messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": "", "messages": [{"role": "user", "content": "hello"}]}),
        ] {
            assert!(required_openai_model(&request).is_err(), "{request}");
        }
        assert_eq!(
            required_openai_model(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
            "gpt-5.6-sol"
        );
    }

    #[test]
    fn rejects_missing_empty_or_non_array_messages() {
        for request in [
            json!({"model": "gpt-5.6-sol"}),
            json!({"model": "gpt-5.6-sol", "messages": null}),
            json!({"model": "gpt-5.6-sol", "messages": {}}),
            json!({"model": "gpt-5.6-sol", "messages": "hello"}),
            json!({"model": "gpt-5.6-sol", "messages": []}),
        ] {
            assert!(required_openai_messages(&request).is_err(), "{request}");
        }
        assert_eq!(
            required_openai_messages(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap()
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn invalid_openai_request_uses_400_error_envelope() {
        let response = openai_invalid_request("`model` is required".to_string());
        let (status, body) = response_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "`model` is required");
        assert_standard_clean_openai_error(&body);
    }

    #[tokio::test]
    async fn missing_or_invalid_model_errors_are_marked_and_clean() {
        let state = AppState::new("test-key", true, true);
        for request in [
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": null, "messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": 56, "messages": [{"role": "user", "content": "hello"}]}),
            json!({"model": "", "messages": [{"role": "user", "content": "hello"}]}),
        ] {
            let response =
                post_chat_completions(State(state.clone()), OpenAiChatJson(request.clone())).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{request}");
            assert!(
                response
                    .extensions()
                    .get::<super::super::middleware::GptOpenAiResponse>()
                    .is_some(),
                "{request}"
            );
            let (_, body) = response_json(response).await;
            assert_standard_clean_openai_error(&body);
        }
    }

    #[tokio::test]
    async fn gpt_handler_marks_validation_and_backend_errors_but_claude_does_not() {
        let state = AppState::new("test-key", true, true);

        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let validation = post_chat_completions(
                State(state.clone()),
                OpenAiChatJson(json!({"model": model, "messages": []})),
            )
            .await;
            assert_eq!(validation.status(), StatusCode::BAD_REQUEST, "{model}");
            assert!(
                validation
                    .extensions()
                    .get::<super::super::middleware::GptOpenAiResponse>()
                    .is_some(),
                "{model}"
            );
        }

        let backend = post_chat_completions(
            State(state.clone()),
            OpenAiChatJson(json!({
                "model": "gpt-5.6-terra",
                "messages": [{"role": "user", "content": "hello"}]
            })),
        )
        .await;
        assert!(!backend.status().is_success());
        assert!(
            backend
                .extensions()
                .get::<super::super::middleware::GptOpenAiResponse>()
                .is_some()
        );

        let claude = post_chat_completions(
            State(state),
            OpenAiChatJson(json!({"model": "claude-opus-4-8", "messages": []})),
        )
        .await;
        assert_eq!(claude.status(), StatusCode::BAD_REQUEST);
        assert!(
            claude
                .extensions()
                .get::<super::super::middleware::GptOpenAiResponse>()
                .is_none()
        );
    }

    #[tokio::test]
    async fn aws_b_router_emits_clean_gpt_error_headers_and_keeps_claude_control() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OpenAI header test router");
        let address = listener.local_addr().expect("OpenAI header test address");
        let app = super::super::router::create_router_with_provider("test-key", None, true, true);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve OpenAI header test router");
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("OpenAI header test client");

        let malformed = client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("test-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await
            .expect("malformed Chat Completions response");
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        for forbidden in ["x-new-api-version", "x-oneapi-request-id", "server", "via"] {
            assert!(
                malformed.headers().get(forbidden).is_none(),
                "{forbidden}: {:?}",
                malformed.headers()
            );
        }
        let malformed_body: Value = malformed.json().await.expect("malformed JSON error");
        assert_eq!(
            malformed_body["error"]["message"],
            "Invalid JSON request body."
        );
        assert_standard_clean_openai_error(&malformed_body);

        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let response = client
                .post(format!("http://{address}/v1/chat/completions"))
                .bearer_auth("test-key")
                .json(&json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .send()
                .await
                .expect("GPT OpenAI error response");
            assert!(!response.status().is_success(), "{model}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .expect("content type")
                    .starts_with("application/json"),
                "{model}"
            );
            for forbidden in [
                "x-new-api-version",
                "x-oneapi-request-id",
                "x-accel-buffering",
                "strict-transport-security",
                "server",
                "via",
                "alt-svc",
                "referrer-policy",
                "x-content-type-options",
                "x-frame-options",
            ] {
                assert!(
                    response.headers().get(forbidden).is_none(),
                    "{forbidden} leaked for {model}"
                );
            }
        }

        let claude = client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("test-key")
            .json(&json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("Claude OpenAI control response");
        assert!(!claude.status().is_success());
        assert_eq!(claude.headers()["x-new-api-version"], "d47d4a8b");
        assert_eq!(claude.headers()["server"], "lyywafcdn");
        assert!(claude.headers().get("x-oneapi-request-id").is_some());

        server.abort();
    }

    #[tokio::test]
    async fn gpt_error_conversion_cleans_anthropic_error_response() {
        let upstream = json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": "Kiro API failed while routing Anthropic Claude through Amazon Bedrock arn:aws:bedrock:private"
            }
        });
        let response = gpt_upstream_error_response(
            StatusCode::BAD_GATEWAY,
            &serde_json::to_vec(&upstream).expect("serialize upstream error"),
        );
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(
            body["error"]["message"],
            "The model service is temporarily unavailable. Please retry later."
        );
        assert_standard_clean_openai_error(&body);
    }

    #[tokio::test]
    async fn gpt_error_conversion_cleans_aws_string_error() {
        let upstream = json!({
            "error": "AWS Bedrock bdrk ThrottlingException: rate exceeded on private route"
        });
        let response = gpt_upstream_error_response(
            StatusCode::TOO_MANY_REQUESTS,
            &serde_json::to_vec(&upstream).expect("serialize upstream error"),
        );
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(
            body["error"]["message"],
            "Rate limit exceeded. Please retry later."
        );
        assert_standard_clean_openai_error(&body);
    }

    #[tokio::test]
    async fn gpt_error_conversion_replaces_non_json_internal_leak() {
        let response = gpt_upstream_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            b"KiroProvider panic at src/anthropic/router.rs:42; backend=/home/service",
        );
        let (status, body) = response_json(response).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["type"], "server_error");
        assert_eq!(
            body["error"]["message"],
            "The request could not be completed due to an internal server error."
        );
        assert_standard_clean_openai_error(&body);
    }

    #[tokio::test]
    async fn gpt_non_json_success_body_becomes_marked_clean_500() {
        let response = openai_non_stream_success_response(
            b"<html>Kiro Anthropic Bedrock private upstream</html>",
            "gpt-5.6-sol",
            123,
            true,
        );
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response
                .extensions()
                .get::<super::super::middleware::GptOpenAiResponse>()
                .is_some()
        );
        let (_, body) = response_json(response).await;
        assert_eq!(body["error"]["type"], "server_error");
        assert_standard_clean_openai_error(&body);
    }

    #[test]
    fn maps_openai_reasoning_controls_to_kiro_reasoning() {
        assert_eq!(
            openai_reasoning_config(&json!({
                "reasoning": {"effort": "max", "mode": "pro"}
            }))
            .unwrap(),
            Some(ReasoningConfig {
                effort: "max".to_string(),
                mode: Some("pro".to_string())
            })
        );
        assert_eq!(
            openai_reasoning_config(&json!({
                "reasoning_effort": "low",
                "reasoning_mode": "standard"
            }))
            .unwrap(),
            Some(ReasoningConfig {
                effort: "low".to_string(),
                mode: Some("standard".to_string())
            })
        );
        assert_eq!(
            openai_reasoning_config(&json!({
                "reasoning_mode": "pro"
            }))
            .unwrap(),
            Some(ReasoningConfig {
                effort: "medium".to_string(),
                mode: Some("pro".to_string())
            })
        );
        assert_eq!(openai_reasoning_config(&json!({})).unwrap(), None);
        for invalid in [
            json!({"reasoning": "high"}),
            json!({"reasoning": {"effort": 123}}),
            json!({"reasoning": {"efforrt": "max"}}),
            json!({"reasoning_efforrt": "max"}),
            json!({"reasoning_mod": "pro"}),
            json!({"reasoning_effort": 123}),
            json!({"reasoning": {"mode": false}}),
            json!({"reasoning_mode": false}),
            json!({
                "reasoning": {"effort": "low"},
                "reasoning_effort": "max"
            }),
            json!({
                "reasoning": {"mode": "standard"},
                "reasoning_mode": "pro"
            }),
        ] {
            assert!(openai_reasoning_config(&invalid).is_err());
        }
        assert_eq!(
            openai_reasoning_config(&json!({
                "reasoning": {"effort": " XHIGH ", "mode": "PRO"},
                "reasoning_effort": "xhigh",
                "reasoning_mode": "pro"
            }))
            .unwrap(),
            Some(ReasoningConfig {
                effort: " XHIGH ".to_string(),
                mode: Some("PRO".to_string())
            })
        );
    }

    #[test]
    fn gpt_chat_reasoning_allows_function_tools_at_every_effort() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            for effort in ["none", "low", "medium", "high", "xhigh", "max"] {
                let reasoning = openai_reasoning_config(&json!({
                    "reasoning_effort": effort
                }))
                .expect("valid Chat reasoning")
                .expect("reasoning config");
                assert_eq!(reasoning.effort, effort);
                // 工具与 reasoning 可共存:底层复用 post_messages,与 /v1/messages 同路径。
                assert!(
                    validate_gpt_chat_reasoning_compatibility(model, Some(&reasoning)).is_ok(),
                    "{model}/{effort} with function tools"
                );
            }

            assert!(
                validate_gpt_chat_reasoning_compatibility(model, None).is_ok(),
                "{model} tools inherit the medium default without error"
            );
            for mode in ["standard", "pro"] {
                let reasoning = ReasoningConfig {
                    effort: "medium".to_string(),
                    mode: Some(mode.to_string()),
                };
                assert!(
                    validate_gpt_chat_reasoning_compatibility(model, Some(&reasoning)).is_err(),
                    "{model}/{mode} mode belongs to Responses"
                );
            }
        }
    }

    #[test]
    fn tool_without_description_falls_back_to_its_name() {
        let tools = openai_tools_to_anthropic(
            &json!({
                "tools": [{
                    "type": "function",
                    "function": {"name": "get_weather", "parameters": {"type": "object"}}
                }]
            }),
            false,
        )
        .expect("tools convert")
        .expect("tools present");
        // 上游对空 description 返回 400 Invalid tool use format。
        assert_eq!(tools[0].description, "get_weather");
    }

    #[test]
    fn non_stream_response_exposes_tool_calls_and_reasoning() {
        let anthropic = json!({
            "id": "msg_bdrk_01abc",
            "content": [
                {"type": "thinking", "thinking": "reason"},
                {"type": "tool_use", "id": "toolu_1", "name": "calculator", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let response = anthropic_to_openai_chat(&anthropic, "claude-sonnet-4-6", 123, false);
        let message = &response["choices"][0]["message"];
        assert!(message["content"].is_null());
        assert_eq!(message["reasoning_content"], "reason");
        assert_eq!(message["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            r#"{"a":1}"#
        );
        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn aws_b_non_stream_response_matches_pomo_identity_and_usage_shape() {
        let anthropic = json!({
            "id": "msg_bdrk_01abc",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 26,
                "output_tokens": 16,
                "cache_creation_input_tokens": 7,
                "cache_read_input_tokens": 5,
                "output_tokens_details": {"thinking_tokens": 3},
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 7,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        });

        let response = anthropic_to_openai_chat(&anthropic, "claude-opus-4-8", 123, true);

        assert_eq!(response["id"], "msg_bdrk_01abc");
        assert!(response["choices"][0].get("logprobs").is_none());
        assert_eq!(response["usage"]["prompt_tokens"], 38);
        assert_eq!(response["usage"]["completion_tokens"], 16);
        assert_eq!(response["usage"]["total_tokens"], 54);
        assert_eq!(
            response["usage"]["prompt_tokens_details"]["cached_tokens"],
            5
        );
        assert_eq!(
            response["usage"]["completion_tokens_details"]["reasoning_tokens"],
            3
        );
        assert_eq!(response["usage"]["input_tokens"], 0);
        assert_eq!(response["usage"]["output_tokens"], 0);
        assert!(response["usage"]["input_tokens_details"].is_null());
    }

    #[test]
    fn aws_b_gpt_response_uses_clean_openai_shape_without_vendor_markers() {
        let anthropic = json!({
            "id": "msg_01bdrk_private_marker",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_bdrk_Claude_Anthropic_Kiro",
                    "name": "calculator",
                    "input": {"a": 1}
                },
                {
                    "type": "tool_use",
                    "id": "toolu_bdrk_Claude_Anthropic_Kiro",
                    "name": "calculator_again",
                    "input": {"a": 2}
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 26,
                "output_tokens": 16,
                "cache_creation_input_tokens": 7,
                "cache_read_input_tokens": 5,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 7,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        });

        let response = anthropic_to_openai_chat(&anthropic, "gpt-5.6-sol", 123, true);
        let id = response["id"].as_str().expect("GPT response id");
        assert!(id.starts_with("chatcmpl-"), "{id}");
        assert!(!id.to_ascii_lowercase().contains("bdrk"), "{id}");
        assert!(response["choices"][0]["logprobs"].is_null());
        let tool_calls = response["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("GPT tool calls");
        assert_eq!(tool_calls.len(), 2);
        let first_tool_id = tool_calls[0]["id"].as_str().expect("first tool call id");
        let second_tool_id = tool_calls[1]["id"].as_str().expect("second tool call id");
        assert!(first_tool_id.starts_with("call_"), "{first_tool_id}");
        assert_eq!(first_tool_id, second_tool_id);

        let usage = response["usage"].as_object().expect("GPT usage object");
        assert_eq!(usage.len(), 5, "{usage:?}");
        for standard_key in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        ] {
            assert!(usage.contains_key(standard_key), "{usage:?}");
        }
        for vendor_key in [
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "claude_cache_creation_5_m_tokens",
            "claude_cache_creation_1_h_tokens",
        ] {
            assert!(!usage.contains_key(vendor_key), "{usage:?}");
        }

        let encoded = serde_json::to_string(&response).expect("serialize GPT response");
        let lower = encoded.to_ascii_lowercase();
        for forbidden in ["kiro", "claude", "anthropic", "bdrk", "bedrock"] {
            assert!(!lower.contains(forbidden), "{encoded}");
        }
    }

    #[test]
    fn anthropic_error_event_emits_clean_chat_error_without_done() {
        let mut state = OpenAiStreamState::new("gpt-5.6-sol".to_string(), 123, true, true);
        let output = transform_anthropic_sse_event(
            &mut state,
            "error",
            &json!({
                "type": "error",
                "error": {
                    "message": "Kiro Anthropic Claude Bedrock upstream failed at /home/private"
                }
            }),
        );

        assert!(state.done);
        assert!(!output.contains("[DONE]"), "{output}");
        let body: Value = serde_json::from_str(
            output
                .strip_prefix("data: ")
                .expect("OpenAI SSE data prefix")
                .trim(),
        )
        .expect("valid OpenAI stream error");
        assert_standard_clean_openai_error(&body);
        assert!(
            transform_anthropic_sse_event(
                &mut state,
                "message_stop",
                &json!({"type": "message_stop"})
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn stream_io_error_emits_clean_chat_error_without_done() {
        let upstream = stream::iter([Err::<Bytes, std::io::Error>(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Kiro Anthropic Bedrock private stream failure",
        ))]);
        let response = openai_stream_response(
            Body::from_stream(upstream),
            "gpt-5.6-terra".to_string(),
            123,
            true,
            true,
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read OpenAI stream error");
        let output = std::str::from_utf8(&body).expect("UTF-8 OpenAI stream error");

        assert!(!output.contains("[DONE]"), "{output}");
        let error: Value = serde_json::from_str(
            output
                .strip_prefix("data: ")
                .expect("OpenAI SSE data prefix")
                .trim(),
        )
        .expect("valid OpenAI stream error");
        assert_standard_clean_openai_error(&error);
    }

    #[tokio::test]
    async fn unexpected_stream_eof_emits_clean_chat_error_without_done() {
        let upstream = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"id\":\"msg_private\",\"model\":\"claude-private\"}}\n\n"
        );
        let response = openai_stream_response(
            Body::from(upstream),
            "gpt-5.6-luna".to_string(),
            123,
            false,
            true,
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read truncated OpenAI stream");
        let output = std::str::from_utf8(&body).expect("UTF-8 truncated OpenAI stream");

        assert!(
            output.contains("\"object\":\"chat.completion.chunk\""),
            "{output}"
        );
        assert!(!output.contains("[DONE]"), "{output}");
        let error_block = output
            .split("\n\n")
            .filter(|block| !block.is_empty())
            .last()
            .expect("terminal stream error block");
        let error: Value = serde_json::from_str(
            error_block
                .strip_prefix("data: ")
                .expect("OpenAI SSE data prefix"),
        )
        .expect("valid terminal OpenAI stream error");
        assert_standard_clean_openai_error(&error);
    }

    #[test]
    fn stream_translation_emits_chunks_usage_and_done() {
        let mut state = OpenAiStreamState::new("requested-model".to_string(), 123, true, false);
        let start = transform_anthropic_sse_event(
            &mut state,
            "message_start",
            &json!({
                "message": {
                    "id": "msg_bdrk_01abc",
                    "model": "claude-sonnet-4-6",
                    "usage": {"input_tokens": 11, "output_tokens": 1}
                }
            }),
        );
        let text = transform_anthropic_sse_event(
            &mut state,
            "content_block_delta",
            &json!({"index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
        );
        let finish = transform_anthropic_sse_event(
            &mut state,
            "message_delta",
            &json!({
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 11, "output_tokens": 3}
            }),
        );
        let stop = transform_anthropic_sse_event(
            &mut state,
            "message_stop",
            &json!({"type": "message_stop"}),
        );

        assert!(start.contains("chatcmpl-bdrk_01abc"));
        assert!(start.contains("\"role\":\"assistant\""));
        assert!(text.contains("\"content\":\"hello\""));
        assert!(finish.contains("\"finish_reason\":\"stop\""));
        assert!(stop.contains("\"prompt_tokens\":11"));
        assert!(stop.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn aws_b_gpt_stream_uses_clean_openai_ids_and_usage() {
        let mut state = OpenAiStreamState::new("gpt-5.6-luna".to_string(), 123, true, true);
        let start = transform_anthropic_sse_event(
            &mut state,
            "message_start",
            &json!({
                "message": {
                    "id": "msg_01bdrk_private_marker",
                    "model": "claude-opus-4-8",
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 1,
                        "cache_creation_input_tokens": 7,
                        "cache_read_input_tokens": 5,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": 7,
                            "ephemeral_1h_input_tokens": 0
                        }
                    }
                }
            }),
        );
        let first_tool = transform_anthropic_sse_event(
            &mut state,
            "content_block_start",
            &json!({
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_bdrk_Claude_Anthropic_Kiro",
                    "name": "calculator"
                }
            }),
        );
        let repeated_tool = transform_anthropic_sse_event(
            &mut state,
            "content_block_start",
            &json!({
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_bdrk_Claude_Anthropic_Kiro",
                    "name": "calculator_again"
                }
            }),
        );
        let finish = transform_anthropic_sse_event(
            &mut state,
            "message_delta",
            &json!({
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 11, "output_tokens": 3}
            }),
        );
        let stop = transform_anthropic_sse_event(
            &mut state,
            "message_stop",
            &json!({"type": "message_stop"}),
        );

        let parse_chunk = |s: &str| -> Value {
            serde_json::from_str(
                s.strip_prefix("data: ")
                    .expect("OpenAI SSE data prefix")
                    .trim(),
            )
            .expect("valid OpenAI chunk JSON")
        };
        let first_tool = parse_chunk(&first_tool);
        let repeated_tool = parse_chunk(&repeated_tool);
        let first_tool_id = first_tool["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .expect("first streamed tool call id");
        let repeated_tool_id = repeated_tool["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .expect("repeated streamed tool call id");
        assert!(first_tool_id.starts_with("call_"), "{first_tool_id}");
        assert_eq!(first_tool_id, repeated_tool_id);

        let usage_line = stop.lines().next().expect("GPT usage chunk");
        let usage_chunk = parse_chunk(usage_line);
        let usage = usage_chunk["usage"].as_object().expect("GPT usage object");
        assert_eq!(usage.len(), 5, "{usage:?}");
        for vendor_key in [
            "input_tokens",
            "output_tokens",
            "input_tokens_details",
            "claude_cache_creation_5_m_tokens",
            "claude_cache_creation_1_h_tokens",
        ] {
            assert!(!usage.contains_key(vendor_key), "{usage:?}");
        }

        let encoded =
            format!("{start}{first_tool}{repeated_tool}{finish}{stop}").to_ascii_lowercase();
        assert!(encoded.contains("\"id\":\"chatcmpl-"), "{encoded}");
        for forbidden in ["kiro", "claude", "anthropic", "bdrk", "bedrock"] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
        assert!(encoded.contains("\"prompt_tokens\":23"), "{encoded}");
        assert!(stop.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn aws_b_stream_translation_matches_pomo_chunk_envelope() {
        let mut state = OpenAiStreamState::new("claude-opus-4-8".to_string(), 123, true, true);
        let start = transform_anthropic_sse_event(
            &mut state,
            "message_start",
            &json!({
                "message": {
                    "id": "msg_bdrk_01abc",
                    "model": "claude-opus-4-8",
                    "usage": {"input_tokens": 11, "output_tokens": 1}
                }
            }),
        );
        let block_start = transform_anthropic_sse_event(
            &mut state,
            "content_block_start",
            &json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        );
        let finish = transform_anthropic_sse_event(
            &mut state,
            "message_delta",
            &json!({
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 11, "output_tokens": 3}
            }),
        );
        let stop = transform_anthropic_sse_event(
            &mut state,
            "message_stop",
            &json!({"type": "message_stop"}),
        );

        let parse_chunk = |s: &str| -> Value {
            serde_json::from_str(
                s.strip_prefix("data: ")
                    .expect("OpenAI SSE data prefix")
                    .trim(),
            )
            .expect("valid OpenAI chunk JSON")
        };
        let start = parse_chunk(&start);
        let block_start = parse_chunk(&block_start);
        let finish = parse_chunk(&finish);
        let usage_line = stop.lines().next().expect("usage chunk");
        let usage = parse_chunk(usage_line);

        assert_eq!(start["id"], "msg_bdrk_01abc");
        assert_eq!(start["model"], "claude-opus-4-8");
        assert!(start["system_fingerprint"].is_null());
        assert!(start["usage"].is_null());
        assert_eq!(start["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(block_start["choices"][0]["delta"]["content"], "");
        assert_eq!(finish["choices"][0]["finish_reason"], "stop");
        assert_eq!(usage["model"], "claude-opus-4-8");
        assert!(
            usage["choices"]
                .as_array()
                .expect("choices array")
                .is_empty()
        );
        assert_eq!(usage["usage"]["prompt_tokens"], 11);
        assert_eq!(usage["usage"]["input_tokens"], 0);
        assert!(stop.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn stream_translation_preserves_incremental_tool_arguments() {
        let mut state = OpenAiStreamState::new("model".to_string(), 123, false, false);
        let start = transform_anthropic_sse_event(
            &mut state,
            "content_block_start",
            &json!({
                "index": 2,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "calculator"}
            }),
        );
        let delta = transform_anthropic_sse_event(
            &mut state,
            "content_block_delta",
            &json!({
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}
            }),
        );
        assert!(start.contains("\"name\":\"calculator\""));
        let chunk: Value = serde_json::from_str(
            delta
                .strip_prefix("data: ")
                .expect("OpenAI SSE data prefix")
                .trim(),
        )
        .expect("valid OpenAI chunk JSON");
        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"{"a":1}"#
        );
    }
}
