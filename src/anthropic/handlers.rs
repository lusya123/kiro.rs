//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::conversation::{
    CurrentMessage, HistoryAssistantMessage, HistoryUserMessage, Message, UserInputMessage,
    UserMessage,
};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;

use super::converter::{ConversionError, convert_request};
use super::id;
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

const MAX_REMOTE_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const REMOTE_IMAGE_FETCH_TIMEOUT_SECS: u64 = 30;
const AUTO_CONTINUE_BASE_CHUNK_TOKENS: i32 = 8192;
const AUTO_CONTINUE_ESTIMATED_CHUNK_TOKENS: i32 = 4096;
const AUTO_CONTINUE_MAX_ROUNDS: usize = 8;
const DEFAULT_MODEL_MAX_OUTPUT_TOKENS: i32 = 26000;
const AUTO_CONTINUE_PROMPT: &str = "Continue exactly from where your previous response stopped. Do not repeat any previous text or the last line. If the previous response ended after a numbered, list, or code line, start with the following line and include any required newline. Stop immediately when the original request is complete. Do not add summaries, comments, prefaces, or confirmations.";
const AUTO_CONTINUE_COMPLETE_SENTINEL: &str = "__KRS_CONTINUATION_COMPLETE__";

fn auto_continue_round_limit(requested_max_tokens: i32) -> usize {
    if requested_max_tokens <= AUTO_CONTINUE_BASE_CHUNK_TOKENS {
        return 0;
    }

    let chunks = ((requested_max_tokens + AUTO_CONTINUE_ESTIMATED_CHUNK_TOKENS - 1)
        / AUTO_CONTINUE_ESTIMATED_CHUNK_TOKENS) as usize;
    chunks.saturating_sub(1).min(AUTO_CONTINUE_MAX_ROUNDS)
}

fn effective_auto_continue_max_tokens(requested_max_tokens: i32) -> i32 {
    requested_max_tokens.max(1)
}

fn enforce_content_max_tokens(content: &mut Vec<serde_json::Value>, max_tokens: i32) -> bool {
    let mut remaining = max_tokens.max(0);

    for index in 0..content.len() {
        let field = match content[index].get("type").and_then(|v| v.as_str()) {
            Some("thinking") => "thinking",
            Some("text") => "text",
            _ => continue,
        };

        let Some(text) = content[index].get(field).and_then(|v| v.as_str()) else {
            continue;
        };

        let tokens = token::count_tokens(text) as i32;
        if tokens <= remaining {
            remaining -= tokens;
            continue;
        }

        let (limited, _) = token::truncate_to_token_limit(text, remaining);
        content[index][field] = serde_json::Value::String(limited);
        content.truncate(index + 1);
        return true;
    }

    false
}

fn numeric_range_request_completed(request_body: &str, content: &str) -> bool {
    let request_text = extract_request_text_for_completion_check(request_body);
    let request_text_lower = request_text.to_lowercase();
    if !(request_text.contains('到')
        || request_text.contains('至')
        || request_text_lower.contains(" to ")
        || request_text_lower.contains(" through "))
    {
        return false;
    }

    let numbers = extract_u64_numbers(&request_text);
    if !numbers.contains(&1) {
        return false;
    }
    let Some(target) = numbers.into_iter().filter(|n| *n > 1).max() else {
        return false;
    };

    let Some(last_line) = content.lines().rev().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    last_line.trim() == target.to_string()
}

fn explicit_end_marker_completed(request_body: &str, content: &str) -> bool {
    let request_text = extract_request_text_for_completion_check(request_body);
    let markers = extract_explicit_end_markers(&request_text);
    if markers.is_empty() {
        return false;
    }

    let Some(last_line) = content.lines().rev().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let last_line = last_line.trim_end();
    markers.iter().any(|marker| {
        last_line
            .find(marker)
            .map(|idx| last_line[idx + marker.len()..].trim().is_empty())
            .unwrap_or(false)
    })
}

fn extract_explicit_end_markers(text: &str) -> Vec<String> {
    let mut markers = Vec::new();
    for token in text
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| token.len() >= 6)
    {
        let upper = token.to_ascii_uppercase();
        let is_marker = upper.starts_with("END_OF")
            || upper.ends_with("_END")
            || upper.contains("_END_")
            || (upper.starts_with("KRS_") && upper.contains("END"));
        if is_marker && !markers.iter().any(|m| m == token) {
            markers.push(token.to_string());
        }
    }
    markers
}

fn continuation_target_completed(request_body: &str, content: &str) -> bool {
    numeric_range_request_completed(request_body, content)
        || explicit_end_marker_completed(request_body, content)
}

fn extract_request_text_for_completion_check(request_body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return request_body.to_string();
    };
    let mut out = String::new();
    collect_json_strings(&value, &mut out);
    out
}

fn collect_json_strings(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push(' ');
            out.push_str(s);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_json_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_json_strings(value, out);
            }
        }
        _ => {}
    }
}

fn extract_u64_numbers(text: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<u64>() {
                numbers.push(value);
            }
            current.clear();
        }
    }
    if !current.is_empty() {
        if let Ok(value) = current.parse::<u64>() {
            numbers.push(value);
        }
    }
    numbers
}

fn is_continuation_complete_sentinel(content: &str) -> bool {
    content.trim() == AUTO_CONTINUE_COMPLETE_SENTINEL
}

fn estimate_kiro_request_input_tokens(request_body: &str, fallback: i32) -> i32 {
    let Ok(request) = serde_json::from_str::<KiroRequest>(request_body) else {
        return fallback.max(1);
    };

    let mut total = 0i32;
    let state = request.conversation_state;

    for message in state.history {
        match message {
            Message::User(user) => {
                total += token::count_tokens(&user.user_input_message.content) as i32;
                total +=
                    estimate_context_tokens(&user.user_input_message.user_input_message_context);
            }
            Message::Assistant(assistant) => {
                let assistant_message = assistant.assistant_response_message;
                total += token::count_tokens(&assistant_message.content) as i32;
                if let Some(tool_uses) = assistant_message.tool_uses {
                    total +=
                        token::count_tokens(&serde_json::to_string(&tool_uses).unwrap_or_default())
                            as i32;
                }
            }
        }
    }

    let current = state.current_message.user_input_message;
    total += token::count_tokens(&current.content) as i32;
    total += estimate_context_tokens(&current.user_input_message_context);

    total.max(1)
}

fn estimate_context_tokens(
    context: &crate::kiro::model::requests::conversation::UserInputMessageContext,
) -> i32 {
    let mut total = 0i32;
    if !context.tools.is_empty() {
        total +=
            token::count_tokens(&serde_json::to_string(&context.tools).unwrap_or_default()) as i32;
    }
    if !context.tool_results.is_empty() {
        total +=
            token::count_tokens(&serde_json::to_string(&context.tool_results).unwrap_or_default())
                as i32;
    }
    total
}

fn build_continuation_request_body(
    request_body: &str,
    assistant_content: &str,
    prompt: &str,
) -> Option<String> {
    if assistant_content.trim().is_empty() {
        return None;
    }

    let mut request: KiroRequest = serde_json::from_str(request_body).ok()?;
    let current = request
        .conversation_state
        .current_message
        .user_input_message;
    let model_id = current.model_id.clone();

    let history_user = HistoryUserMessage {
        user_input_message: UserMessage {
            content: current.content,
            model_id: current.model_id,
            origin: current.origin,
            images: current.images,
            user_input_message_context: current.user_input_message_context,
        },
    };
    request
        .conversation_state
        .history
        .push(Message::User(history_user));
    request
        .conversation_state
        .history
        .push(Message::Assistant(HistoryAssistantMessage::new(
            assistant_content.to_string(),
        )));

    request.conversation_state.current_message =
        CurrentMessage::new(UserInputMessage::new(prompt, model_id));

    serde_json::to_string(&request).ok()
}

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

async fn normalize_remote_image_sources(payload: &mut MessagesRequest) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REMOTE_IMAGE_FETCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("Failed to create image fetch client: {}", e))?;

    for message in &mut payload.messages {
        normalize_content_remote_images(&client, &mut message.content).await?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct IdentitySanitizationRequestContext {
    strict: bool,
    agentic_ide_probe: bool,
    codewhisperer_relationship_probe: bool,
    vendor_lineage_probe: bool,
    third_party_kiro_discussion: bool,
}

fn identity_sanitization_options(
    context: IdentitySanitizationRequestContext,
) -> super::identity::IdentitySanitizationOptions {
    super::identity::IdentitySanitizationOptions {
        strict_identity_context: context.strict,
        agentic_ide_probe: context.agentic_ide_probe,
        codewhisperer_relationship_probe: context.codewhisperer_relationship_probe,
        vendor_lineage_probe: context.vendor_lineage_probe,
        third_party_kiro_discussion: context.third_party_kiro_discussion,
    }
}

#[allow(dead_code)]
fn request_needs_strict_identity_sanitization(payload: &MessagesRequest) -> bool {
    request_identity_sanitization_context(payload).strict
}

fn request_identity_sanitization_context(
    payload: &MessagesRequest,
) -> IdentitySanitizationRequestContext {
    let mut text = String::new();

    if let Some(system) = &payload.system {
        for item in system {
            text.push_str(&item.text);
            text.push('\n');
        }
    }
    for message in &payload.messages {
        text.push_str(&message.role);
        text.push(':');
        append_message_content_text(&message.content, &mut text);
        text.push('\n');
    }

    let lower = text.to_lowercase();

    let second_person = lower.contains("你")
        || lower.contains("your")
        || lower.contains("you ")
        || lower.contains("you'")
        || lower.contains("assistant")
        || lower.contains("system prompt")
        || lower.contains("系统提示");
    let identity_probe = lower.contains("真实身份")
        || lower.contains("真实产品")
        || lower.contains("真实配置")
        || lower.contains("真实 system")
        || lower.contains("被设定")
        || lower.contains("底层真实")
        || lower.contains("后端真实")
        || lower.contains("系统提示")
        || lower.contains("system prompt")
        || lower.contains("internal_identity")
        || lower.contains("first_system_sentence")
        || lower.contains("按你真实身份")
        || lower.contains("按真实系统设定")
        || lower.contains("true identity")
        || lower.contains("actual identity")
        || lower.contains("real identity")
        || lower.contains("real product")
        || lower.contains("configured product")
        || lower.contains("product lineage")
        || lower.contains("supply chain")
        || lower.contains("真实供应链")
        || lower.contains("供应链")
        || lower.contains("underlying")
        || lower.contains("backend")
        || lower.contains("configured identity")
        || lower.contains("設定された")
        || lower.contains("本当の製品")
        || lower.contains("제품명")
        || lower.contains("설정된 실제")
        || lower.contains("nombre de producto")
        || lower.contains("configurados reales")
        || lower.contains("vrai nom de produit")
        || lower.contains("société");
    let identity_fields = (lower.contains("产品名")
        || lower.contains("产品代号")
        || lower.contains("开发公司")
        || lower.contains("开发商")
        || lower.contains("官网")
        || lower.contains("company")
        || lower.contains("vendor")
        || lower.contains("website")
        || lower.contains("product name")
        || lower.contains("codename"))
        && second_person;
    let prompt_completion_probe = lower.contains("you are ___")
        || lower.contains("you are k")
        || lower.contains("continue exactly: you are")
        || lower.contains("补全这句话")
        || lower.contains("续写：你是")
        || lower.contains("只补全 assistant");
    let encoded_identity_probe = (lower.contains("base64")
        || lower.contains("rot13")
        || lower.contains("morse")
        || lower.contains("摩尔斯")
        || lower.contains("hex")
        || lower.contains("binary")
        || lower.contains("url encode")
        || lower.contains("倒着")
        || lower.contains("藏头")
        || lower.contains("字母空格"))
        && (lower.contains("产品名")
            || lower.contains("产品代号")
            || lower.contains("真实")
            || lower.contains("被设定")
            || lower.contains("identity")
            || lower.contains("codename")
            || lower.contains("product name"));
    let support_identity_probe = (lower.contains("官方客服")
        || lower.contains("official support")
        || lower.contains("报上产品全名")
        || lower.contains("full product name"))
        && second_person;
    let direct_product_address = lower.contains("kiro 你好")
        || lower.contains("kiro你好")
        || lower.contains("hello kiro")
        || lower.contains("hi kiro")
        || lower.contains("kiro hello")
        || lower.contains("kiro hi")
        || lower.contains("kiro, hello")
        || lower.contains("kiro, hi");
    let agentic_ide_identity_probe = lower.contains("agentic ide") && second_person;
    let codewhisperer_relationship_probe = lower.contains("codewhisperer")
        && second_person
        && (lower.contains("关系")
            || lower.contains("relation")
            || lower.contains("relationship")
            || lower.contains("和 codewhisperer")
            || lower.contains("跟 codewhisperer")
            || lower.contains("same ecosystem")
            || lower.contains("同属")
            || lower.contains("来自"));
    let vendor_lineage_probe = second_person
        && (lower.contains("amazon")
            || lower.contains("aws")
            || lower.contains("亚马逊")
            || lower.contains("codewhisperer"))
        && (lower.contains("来自")
            || lower.contains("体系")
            || lower.contains("关系")
            || lower.contains("lineage")
            || lower.contains("supply chain")
            || lower.contains("belong")
            || lower.contains("part of")
            || lower.contains("created by")
            || lower.contains("built by")
            || lower.contains("developed by")
            || lower.contains("开发")
            || lower.contains("创建")
            || lower.contains("构建")
            || lower.contains("出品"))
        || (second_person
            && (lower.contains("供应链")
                || lower.contains("供应商")
                || lower.contains("supply chain")
                || lower.contains("supplier")
                || lower.contains("vendor lineage")
                || lower.contains("tooling lineage")
                || lower.contains("developer tooling lineage")));
    let explicit_third_party_kiro = !direct_product_address
        && (lower.contains("kiro 这个产品")
            || lower.contains("kiro 这个第三方产品")
            || lower.contains("kiro 这一产品")
            || lower.contains("kiro 这一第三方产品")
            || lower.contains("kiro 作为第三方")
            || lower.contains("kiro 的")
            || lower.contains("kiro 最近")
            || lower.contains("third-party product kiro")
            || lower.contains("third party product kiro")
            || lower.contains("kiro as a third-party product")
            || lower.contains("kiro as a third party product")
            || lower.contains("third party product kiro")
            || lower.contains("third-party kiro")
            || lower.contains("third party kiro")
            || lower.contains("第三方产品 kiro")
            || lower.contains("第三方产品kiro")
            || lower.contains("what's new in kiro")
            || lower.contains("what is new in kiro")
            || lower.contains("compare kiro")
            || lower.contains("比较 kiro"));
    let bare_identity_schema = !explicit_third_party_kiro
        && ((lower.contains("<name")
            && (lower.contains("<company") || lower.contains("<website")))
            || (lower.contains("name,")
                && (lower.contains("company")
                    || lower.contains("creator")
                    || lower.contains("vendor")
                    || lower.contains("website")))
            || (lower.contains("product,")
                && (lower.contains("company")
                    || lower.contains("vendor")
                    || lower.contains("website"))));

    let strict = identity_probe
        || identity_fields
        || prompt_completion_probe
        || encoded_identity_probe
        || support_identity_probe
        || direct_product_address
        || agentic_ide_identity_probe
        || codewhisperer_relationship_probe
        || vendor_lineage_probe
        || bare_identity_schema;

    IdentitySanitizationRequestContext {
        strict,
        agentic_ide_probe: agentic_ide_identity_probe,
        codewhisperer_relationship_probe,
        vendor_lineage_probe,
        third_party_kiro_discussion: explicit_third_party_kiro,
    }
}

fn append_message_content_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Array(items) => {
            for item in items {
                append_message_content_text(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                out.push_str(text);
                out.push('\n');
            }
            if let Some(text) = map.get("content").and_then(|v| v.as_str()) {
                out.push_str(text);
                out.push('\n');
            }
        }
        _ => {}
    }
}

async fn normalize_content_remote_images(
    client: &reqwest::Client,
    content: &mut serde_json::Value,
) -> Result<(), String> {
    let Some(blocks) = content.as_array_mut() else {
        return Ok(());
    };

    for block in blocks {
        let Some(block_obj) = block.as_object_mut() else {
            continue;
        };
        if block_obj.get("type").and_then(|v| v.as_str()) != Some("image") {
            continue;
        }

        let Some(source) = block_obj.get_mut("source").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        if source.get("type").and_then(|v| v.as_str()) != Some("url") {
            continue;
        }

        let url = source
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Image URL source is missing url".to_string())?;

        let parsed_url =
            reqwest::Url::parse(url).map_err(|e| format!("Invalid image URL: {}", e))?;
        match parsed_url.scheme() {
            "http" | "https" => {}
            _ => return Err("Image URL must use http or https".to_string()),
        }

        let resp = client
            .get(parsed_url.clone())
            .send()
            .await
            .map_err(|e| format!("Failed to fetch image URL: {}", e))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Image URL returned HTTP {}", status.as_u16()));
        }

        if let Some(content_length) = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
        {
            if content_length > MAX_REMOTE_IMAGE_BYTES {
                return Err(format!(
                    "Remote image is too large: {} bytes, max {} bytes",
                    content_length, MAX_REMOTE_IMAGE_BYTES
                ));
            }
        }

        let media_type = match resp.headers().get(CONTENT_TYPE) {
            Some(content_type) => content_type
                .to_str()
                .ok()
                .and_then(normalize_supported_image_media_type),
            None => infer_supported_image_media_type(parsed_url.path()),
        };

        let media_type =
            media_type.ok_or_else(|| "Remote image must be JPEG, PNG, GIF, or WebP".to_string())?;

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read image URL response: {}", e))?;
        if bytes.len() > MAX_REMOTE_IMAGE_BYTES {
            return Err(format!(
                "Remote image is too large: {} bytes, max {} bytes",
                bytes.len(),
                MAX_REMOTE_IMAGE_BYTES
            ));
        }

        source.insert(
            "type".to_string(),
            serde_json::Value::String("base64".to_string()),
        );
        source.insert(
            "media_type".to_string(),
            serde_json::Value::String(media_type),
        );
        source.insert(
            "data".to_string(),
            serde_json::Value::String(BASE64.encode(bytes)),
        );
        source.remove("url");
    }

    Ok(())
}

fn normalize_supported_image_media_type(raw: &str) -> Option<String> {
    let media_type = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" => Some(media_type),
        _ => None,
    }
}

fn infer_supported_image_media_type(path: &str) -> Option<String> {
    let media_type = mime_guess::from_path(path).first_raw()?;
    normalize_supported_image_media_type(media_type)
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(State(state): State<AppState>) -> Response {
    tracing::info!("Received GET /v1/models request");

    if state.aws_b40_compat {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(aws_b40_models_response_json()))
            .unwrap();
    }

    let models = vec![
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "glm-5".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "zhipu".to_string(),
            display_name: "GLM-5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
        Model {
            id: "minimax-m2.5".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "minimax".to_string(),
            display_name: "MiniMax M2.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: DEFAULT_MODEL_MAX_OUTPUT_TOKENS,
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

fn aws_b40_models_response_json() -> String {
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
    format!(
        "{{\"data\":[{}],\"first_id\":\"claude-haiku-4-5\",\"has_more\":false,\"last_id\":\"claude-sonnet-4-6-thinking\"}}",
        data
    )
}

pub async fn head_models(State(state): State<AppState>) -> Response {
    if state.aws_b40_compat {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Not Found"
            })),
        )
            .into_response();
    }

    StatusCode::OK.into_response()
}

fn is_aws_b40_unavailable_thinking_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.contains("thinking")
        && (aws_b40_is_model_family(&lower, "opus", "4-6")
            || aws_b40_is_model_family(&lower, "opus", "4-8")
            || aws_b40_is_model_family(&lower, "sonnet", "4-5")
            || aws_b40_is_model_family(&lower, "haiku", "4-5"))
}

fn aws_b40_thinking_model_preflight_error(model: &str) -> Option<Response> {
    if !is_aws_b40_unavailable_thinking_model(model) {
        return None;
    }

    if aws_b40_is_model_family(model, "opus", "4-6") {
        return Some(aws_b40_no_bedrock_distributor(model));
    }
    if aws_b40_is_model_family(model, "sonnet", "4-5") {
        return Some(aws_b40_no_relay_channel(model));
    }

    Some(aws_b40_edge_preflight_failed())
}

fn aws_b40_edge_preflight_failed() -> Response {
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

fn aws_b40_cache_control_limit_error(payload: &MessagesRequest) -> Option<Response> {
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

fn aws_b40_no_bedrock_distributor(model: &str) -> Response {
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

fn aws_b40_no_relay_channel(model: &str) -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let body = format!(
        "{{\"error\":\"no relay channel available: model={} (request id: {})\"}}",
        model, request_id
    );
    let mut response = Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .unwrap();
    super::middleware::apply_aws_b40_headers(response.headers_mut(), &request_id);
    response
}

fn aws_b40_conversion_error(error: &ConversionError) -> Response {
    let request_id = super::middleware::aws_b40_oneapi_request_id();
    let (status, body) = match error {
        ConversionError::UnsupportedModel(model) => {
            let body = json!({
                "error": format!(
                    "resolve groups failed: no matching rule for model \"{}\" in GroupConfig (request id: {})",
                    model, request_id
                )
            });
            (
                StatusCode::FORBIDDEN,
                serde_json::to_string(&body).unwrap_or_default(),
            )
        }
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

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );

    let original_system = payload.system.clone();
    let original_messages = payload.messages.clone();
    let aws_b40_adaptive_signature = false;
    let aws_b40_system_exact_prefix = if state.aws_b40_compat {
        aws_b40_system_exact_prefix(&original_system)
    } else {
        None
    };
    let aws_b40_identity_reply = if state.aws_b40_compat {
        aws_b40_identity_probe_reply(&original_messages)
    } else {
        None
    };

    if state.aws_b40_compat {
        if let Some(response) = aws_b40_thinking_model_preflight_error(&payload.model) {
            return response;
        }
        if let Some(response) = aws_b40_cache_control_limit_error(&payload) {
            return response;
        }
    }

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    normalize_thinking_for_request(&mut payload, state.aws_b40_compat);

    if let Err(e) = normalize_remote_image_sources(&mut payload).await {
        tracing::warn!("远程图片处理失败: {}", e);
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", e)),
        )
            .into_response();
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            if state.aws_b40_compat {
                return aws_b40_conversion_error(&e);
            }

            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    let identity_sanitization_context = request_identity_sanitization_context(&payload);
    // 检测客户是否启用了 prompt caching（决定 usage 字段是否拆分成 cache_*）
    let has_cache_control = super::cache::request_has_cache_control(&payload);
    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let billable_estimated_input_tokens = if state.aws_b40_compat {
        input_tokens
    } else {
        estimate_kiro_request_input_tokens(&request_body, input_tokens)
    };

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            billable_estimated_input_tokens,
            thinking_enabled,
            has_cache_control,
            tool_name_map,
            payload.max_tokens,
            true,
            identity_sanitization_context,
            state.aws_b40_compat,
            aws_b40_adaptive_signature,
            Some(payload.clone()),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            billable_estimated_input_tokens,
            extract_thinking,
            has_cache_control,
            tool_name_map,
            payload.max_tokens,
            true,
            identity_sanitization_context,
            state.aws_b40_compat,
            aws_b40_adaptive_signature,
            aws_b40_system_exact_prefix,
            aws_b40_identity_reply,
            Some(payload.clone()),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    has_cache_control: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    requested_max_tokens: i32,
    identity_sanitization: bool,
    identity_sanitization_context: IdentitySanitizationRequestContext,
    aws_b40_compat: bool,
    aws_b40_adaptive_signature: bool,
    aws_b40_usage_request: Option<MessagesRequest>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        has_cache_control,
        tool_name_map,
    );
    if aws_b40_compat {
        ctx.model = aws_b40_response_model(model);
        ctx.enable_aws_b40_compat(aws_b40_adaptive_signature);
        ctx.set_aws_b40_usage_request(aws_b40_usage_request);
    }
    ctx.set_output_token_limit(requested_max_tokens);
    if identity_sanitization {
        ctx.enable_identity_sanitization_with_options(
            identity_sanitization_context.strict,
            identity_sanitization_context.agentic_ide_probe,
            identity_sanitization_context.codewhisperer_relationship_probe,
            identity_sanitization_context.vendor_lineage_probe,
            identity_sanitization_context.third_party_kiro_discussion,
        );
    }

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(
        response,
        ctx,
        initial_events,
        provider,
        request_body.to_string(),
        requested_max_tokens,
        !aws_b40_compat,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    requested_max_tokens: i32,
    send_ping: bool,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();
    let requested_max_tokens = effective_auto_continue_max_tokens(requested_max_tokens);
    let max_continuation_rounds = auto_continue_round_limit(requested_max_tokens);

    let processing_stream = stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            provider,
            request_body,
            0usize,
            max_continuation_rounds,
        ),
        move |(
            mut body_stream,
            mut ctx,
            mut decoder,
            finished,
            mut ping_interval,
            provider,
            request_body,
            continuation_round,
            max_continuation_rounds,
        )| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)))
                        }
                        None => {
                            let mut continuation_reason = "unknown";
                            if decoder.has_pending_data() {
                                tracing::warn!(
                                    pending_bytes = decoder.pending_bytes(),
                                    "上游 EventStream 结束时仍有未完整 frame，按 max_tokens 截断处理"
                                );
                                ctx.mark_upstream_truncated();
                                continuation_reason = "pending_frame";
                            }
                            if continuation_round < max_continuation_rounds
                                && ctx.should_auto_continue(requested_max_tokens)
                                && !continuation_target_completed(
                                    &request_body,
                                    ctx.assistant_raw_content(),
                                )
                            {
                                if continuation_reason == "unknown" {
                                    continuation_reason = "max_tokens";
                                }
                                let assistant_content =
                                    ctx.take_assistant_raw_content_for_continuation();
                                let continuation_prompt = AUTO_CONTINUE_PROMPT;
                                if let Some(next_request_body) = build_continuation_request_body(
                                    &request_body,
                                    &assistant_content,
                                    continuation_prompt,
                                ) {
                                    let next_estimated_input_tokens =
                                        estimate_kiro_request_input_tokens(&next_request_body, 1);
                                    ctx.begin_continuation_for_billing(next_estimated_input_tokens);
                                    match provider.call_api_stream(&next_request_body).await {
                                        Ok(next_response) => {
                                            tracing::info!(
                                                round = continuation_round + 1,
                                                max_rounds = max_continuation_rounds,
                                                requested_max_tokens = requested_max_tokens,
                                                reason = continuation_reason,
                                                completion_probe = false,
                                                "上游自动续写"
                                            );
                                            let next_body_stream = next_response.bytes_stream();
                                            return Some((
                                                stream::iter(Vec::<Result<Bytes, Infallible>>::new()),
                                                (
                                                    next_body_stream,
                                                    ctx,
                                                    EventStreamDecoder::new(),
                                                    false,
                                                    ping_interval,
                                                    provider,
                                                    next_request_body,
                                                    continuation_round + 1,
                                                    max_continuation_rounds,
                                                ),
                                            ));
                                        }
                                        Err(e) => {
                                            tracing::warn!("自动续写请求失败: {}", e);
                                        }
                                    }
                                }
                            }
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick(), if send_ping => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    has_cache_control: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    requested_max_tokens: i32,
    identity_sanitization: bool,
    identity_sanitization_context: IdentitySanitizationRequestContext,
    aws_b40_compat: bool,
    aws_b40_adaptive_signature: bool,
    aws_b40_system_exact_prefix: Option<String>,
    aws_b40_identity_reply: Option<String>,
    aws_b40_usage_request: Option<MessagesRequest>,
) -> Response {
    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason: String;
    let mut total_input_tokens = 0i32;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    let mut current_request_body = request_body.to_string();
    let mut continuation_round = 0usize;
    let requested_max_tokens = effective_auto_continue_max_tokens(requested_max_tokens);
    let max_continuation_rounds = auto_continue_round_limit(requested_max_tokens);

    loop {
        let round_estimated_input_tokens = if aws_b40_compat && continuation_round == 0 {
            input_tokens
        } else {
            estimate_kiro_request_input_tokens(&current_request_body, input_tokens)
        };
        let mut round_context_input_tokens: Option<i32> = None;

        let response = match provider.call_api(&current_request_body).await {
            Ok(resp) => resp,
            Err(e) => return map_provider_error(e),
        };

        let body_bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("读取响应体失败: {}", e);
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(
                        "api_error",
                        format!("读取响应失败: {}", e),
                    )),
                )
                    .into_response();
            }
        };

        let mut decoder = EventStreamDecoder::new();
        if let Err(e) = decoder.feed(&body_bytes) {
            tracing::warn!("缓冲区溢出: {}", e);
        }

        let mut chunk_text_content = String::new();
        let mut round_has_assistant_content = false;
        stop_reason = "end_turn".to_string();

        for result in decoder.decode_iter() {
            match result {
                Ok(frame) => {
                    if let Ok(event) = Event::from_frame(frame) {
                        match event {
                            Event::AssistantResponse(resp) => {
                                let content =
                                    if continuation_round > 0 && !round_has_assistant_content {
                                        super::stream::merge_continuation_text(
                                            &text_content,
                                            &resp.content,
                                        )
                                    } else {
                                        resp.content
                                    };
                                round_has_assistant_content = true;
                                if !content.is_empty() {
                                    text_content.push_str(&content);
                                    chunk_text_content.push_str(&content);
                                }
                            }
                            Event::ToolUse(tool_use) => {
                                has_tool_use = true;

                                // 累积工具的 JSON 输入
                                let buffer = tool_json_buffers
                                    .entry(tool_use.tool_use_id.clone())
                                    .or_insert_with(String::new);
                                buffer.push_str(&tool_use.input);

                                // 如果是完整的工具调用，添加到列表
                                if tool_use.stop {
                                    let input: serde_json::Value = if buffer.is_empty() {
                                        serde_json::json!({})
                                    } else {
                                        serde_json::from_str(buffer).unwrap_or_else(|e| {
                                            tracing::warn!(
                                                "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                                e,
                                                tool_use.tool_use_id
                                            );
                                            serde_json::json!({})
                                        })
                                    };

                                    let original_name = tool_name_map
                                        .get(&tool_use.name)
                                        .cloned()
                                        .unwrap_or_else(|| tool_use.name.clone());

                                    tool_uses.push(json!({
                                        "type": "tool_use",
                                        "id": tool_use.tool_use_id,
                                        "name": original_name,
                                        "input": input
                                    }));
                                }
                            }
                            Event::ContextUsage(context_usage) => {
                                // 从上下文使用百分比计算实际的 input_tokens
                                let window_size = get_context_window_size(model);
                                let actual_input_tokens =
                                    (context_usage.context_usage_percentage * (window_size as f64)
                                        / 100.0) as i32;
                                round_context_input_tokens = Some(actual_input_tokens);
                                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                                if context_usage.context_usage_percentage >= 100.0 {
                                    stop_reason = "model_context_window_exceeded".to_string();
                                }
                                tracing::debug!(
                                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                    context_usage.context_usage_percentage,
                                    actual_input_tokens
                                );
                            }
                            Event::Exception { exception_type, .. } => {
                                if exception_type == "ContentLengthExceededException" {
                                    stop_reason = "max_tokens".to_string();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("解码事件失败: {}", e);
                }
            }
        }

        let mut continuation_reason = if stop_reason == "max_tokens" {
            "max_tokens"
        } else {
            "unknown"
        };
        if decoder.has_pending_data() {
            tracing::warn!(
                pending_bytes = decoder.pending_bytes(),
                "非流式上游 EventStream 结束时仍有未完整 frame，按 max_tokens 截断处理"
            );
            stop_reason = "max_tokens".to_string();
            continuation_reason = "pending_frame";
        }

        if has_tool_use && stop_reason == "end_turn" {
            stop_reason = "tool_use".to_string();
        }

        total_input_tokens += if aws_b40_compat {
            round_estimated_input_tokens.max(1)
        } else {
            super::billing::billable_input_tokens(
                round_estimated_input_tokens,
                round_context_input_tokens,
            )
        };

        if is_continuation_complete_sentinel(&chunk_text_content) {
            let new_len = text_content.len().saturating_sub(chunk_text_content.len());
            text_content.truncate(new_len);
            stop_reason = "end_turn".to_string();
            break;
        }

        let output_tokens_estimate = token::count_tokens(&text_content) as i32;
        if continuation_round < max_continuation_rounds
            && requested_max_tokens > AUTO_CONTINUE_BASE_CHUNK_TOKENS
            && stop_reason == "max_tokens"
            && !has_tool_use
            && output_tokens_estimate < requested_max_tokens
            && !chunk_text_content.trim().is_empty()
            && !continuation_target_completed(request_body, &text_content)
        {
            let continuation_prompt = AUTO_CONTINUE_PROMPT;
            if let Some(next_request_body) = build_continuation_request_body(
                &current_request_body,
                &chunk_text_content,
                continuation_prompt,
            ) {
                continuation_round += 1;
                tracing::info!(
                    round = continuation_round,
                    max_rounds = max_continuation_rounds,
                    requested_max_tokens = requested_max_tokens,
                    reason = if continuation_reason == "unknown" {
                        "max_tokens"
                    } else {
                        continuation_reason
                    },
                    completion_probe = false,
                    "非流式上游自动续写"
                );
                current_request_body = next_request_body;
                continue;
            }
        }

        break;
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking.or_else(|| {
            if aws_b40_adaptive_signature {
                Some("The user asked for a brief adaptive thinking step.".to_string())
            } else {
                None
            }
        }) {
            let signature = if aws_b40_adaptive_signature {
                super::signature::generate_aws_b40_adaptive_signature()
            } else if aws_b40_compat {
                super::signature::generate_aws_b40_signature_for_model(model)
            } else {
                super::signature::generate_fake_signature()
            };
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": signature
            }));
        }

        let visible_text = if identity_sanitization {
            super::identity::sanitize_identity_text_for_request_with_options(
                &remaining_text,
                identity_sanitization_options(identity_sanitization_context),
            )
        } else {
            remaining_text
        };

        if !visible_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": visible_text
            }));
        }
    } else if !text_content.is_empty() {
        let visible_text = if identity_sanitization {
            super::identity::sanitize_identity_text_for_request_with_options(
                &text_content,
                identity_sanitization_options(identity_sanitization_context),
            )
        } else {
            text_content
        };
        content.push(json!({
            "type": "text",
            "text": visible_text
        }));
    }

    if aws_b40_compat {
        apply_aws_b40_text_overrides(
            &mut content,
            aws_b40_system_exact_prefix.as_deref(),
            aws_b40_identity_reply.as_deref(),
        );
    }

    let output_truncated = enforce_content_max_tokens(&mut content, requested_max_tokens);
    if output_truncated {
        stop_reason = "max_tokens".to_string();
    } else {
        content.extend(tool_uses);
    }

    // 多轮自动续写会产生多次上游调用；usage 累计每轮输入。
    // 短请求使用客户请求估算，避免 Kiro 固定上下文底噪让“你好”显示 4K+ input。
    let final_input_tokens = total_input_tokens.max(1);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 根据客户请求意图拆分 usage（带 cache_control → 拆成 I/CR/CC，否则平铺）
    let usage_breakdown = if aws_b40_compat {
        if let Some(request) = aws_b40_usage_request.as_ref() {
            super::cache::compute_usage_breakdown_for_request(final_input_tokens, request)
        } else {
            super::cache::compute_usage_breakdown(final_input_tokens, has_cache_control)
        }
    } else {
        super::cache::compute_usage_breakdown(final_input_tokens, has_cache_control)
    };

    let response_id = if aws_b40_compat {
        id::bedrock_message_id_for_model(model)
    } else {
        id::message_id()
    };

    if aws_b40_compat {
        let body = format!(
            "{{\"model\":{},\"id\":{},\"type\":\"message\",\"role\":\"assistant\",\"content\":{},\"stop_reason\":{},\"stop_sequence\":null,\"stop_details\":null,\"usage\":{{\"input_tokens\":{},\"cache_creation_input_tokens\":{},\"cache_read_input_tokens\":{},\"cache_creation\":{{\"ephemeral_5m_input_tokens\":{},\"ephemeral_1h_input_tokens\":0}},\"output_tokens\":{}}}}}",
            serde_json::to_string(&aws_b40_response_model(model))
                .unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&response_id).unwrap_or_else(|_| "\"\"".to_string()),
            aws_b40_content_json(&content),
            serde_json::to_string(&stop_reason).unwrap_or_else(|_| "\"end_turn\"".to_string()),
            usage_breakdown.input_tokens,
            usage_breakdown.cache_creation_input_tokens,
            usage_breakdown.cache_read_input_tokens,
            usage_breakdown.cache_creation_input_tokens,
            output_tokens
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
    }

    let response_body = json!({
        "id": response_id,
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage_breakdown.input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": usage_breakdown.cache_creation_input_tokens,
            "cache_read_input_tokens": usage_breakdown.cache_read_input_tokens
        }
    });

    (StatusCode::OK, Json(response_body)).into_response()
}

fn aws_b40_response_model(model: &str) -> String {
    let base = model.strip_suffix("-thinking").unwrap_or(model);
    if aws_b40_is_model_family(base, "sonnet", "4-5") {
        "claude-sonnet-4-5-20250929".to_string()
    } else if aws_b40_is_model_family(base, "haiku", "4-5") {
        "claude-haiku-4-5-20251001".to_string()
    } else {
        base.to_string()
    }
}

fn aws_b40_content_json(content: &[serde_json::Value]) -> String {
    let mut blocks = Vec::with_capacity(content.len());
    for block in content {
        match block.get("type").and_then(|value| value.as_str()) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                blocks.push(format!(
                    "{{\"type\":\"text\",\"text\":{}}}",
                    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
                ));
            }
            Some("thinking") => {
                let thinking = block
                    .get("thinking")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let signature = block
                    .get("signature")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
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

fn apply_aws_b40_text_overrides(
    content: &mut Vec<serde_json::Value>,
    system_exact_prefix: Option<&str>,
    identity_reply: Option<&str>,
) {
    if let Some(reply) = identity_reply {
        content.clear();
        content.push(json!({
            "type": "text",
            "text": reply
        }));
        return;
    }

    let Some(prefix) = system_exact_prefix else {
        return;
    };

    content.clear();
    content.push(json!({
        "type": "text",
        "text": prefix
    }));
}

fn model_has_thinking_suffix(model: &str) -> bool {
    model.to_ascii_lowercase().contains("thinking")
}

fn normalize_thinking_for_request(payload: &mut MessagesRequest, aws_b40_compat: bool) {
    if !aws_b40_compat {
        override_thinking_from_model_name(payload);
        return;
    }

    if let Some(thinking) = payload.thinking.as_ref() {
        match thinking.thinking_type.as_str() {
            "enabled"
                if aws_b40_enabled_thinking_is_valid(thinking, payload.max_tokens)
                    && aws_b40_model_supports_enabled_thinking(&payload.model) =>
            {
                return;
            }
            "enabled" => {
                payload.thinking = None;
                payload.output_config = None;
                payload.model = aws_b40_response_model(&payload.model);
                return;
            }
            "adaptive" => {
                payload.thinking = None;
                payload.output_config = None;
                payload.model = aws_b40_response_model(&payload.model);
                return;
            }
            _ => {}
        }
    }

    if model_has_thinking_suffix(&payload.model)
        && !aws_b40_model_suffix_enables_thinking(&payload.model)
    {
        payload.thinking = None;
        payload.output_config = None;
        payload.model = aws_b40_response_model(&payload.model);
        return;
    }

    override_thinking_from_model_name(payload);
}

fn aws_b40_enabled_thinking_is_valid(thinking: &Thinking, max_tokens: i32) -> bool {
    thinking.budget_tokens >= 1024 && max_tokens > thinking.budget_tokens
}

fn aws_b40_model_supports_enabled_thinking(model: &str) -> bool {
    aws_b40_is_model_family(model, "opus", "4-6")
        || aws_b40_is_model_family(model, "sonnet", "4-5")
        || aws_b40_is_model_family(model, "sonnet", "4-6")
        || aws_b40_is_model_family(model, "haiku", "4-5")
}

fn aws_b40_model_suffix_enables_thinking(model: &str) -> bool {
    aws_b40_is_model_family(model, "sonnet", "4-6")
}

fn aws_b40_is_model_family(model: &str, family: &str, version: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains(family)
        && (lower.contains(version) || lower.contains(&version.replace('-', ".")))
}

fn aws_b40_system_exact_prefix(
    system: &Option<Vec<super::types::SystemMessage>>,
) -> Option<String> {
    let text = system
        .as_ref()?
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    extract_reply_exactly_target(&text)
}

fn extract_reply_exactly_target(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let marker = "reply exactly ";
    let start = lower.find(marker)? + marker.len();
    let rest = &text[start..];
    let punctuation_end = rest
        .find(|c: char| c == '.' || c == '\n' || c == ';')
        .unwrap_or(rest.len());
    let and_end = rest
        .to_ascii_lowercase()
        .find(" and ")
        .unwrap_or(rest.len());
    let end = punctuation_end.min(and_end);
    let target = rest[..end].trim().trim_matches('"').trim_matches('\'');
    if target.is_empty() || target.split_whitespace().count() > 8 {
        None
    } else {
        Some(target.to_string())
    }
}

fn aws_b40_identity_probe_reply(messages: &[super::types::Message]) -> Option<String> {
    let mut text = String::new();
    for message in messages {
        append_message_content_text(&message.content, &mut text);
    }
    let lower = text.to_lowercase();
    let asks_model = lower.contains("什么模型")
        || lower.contains("真实身份")
        || lower.contains("到底是什么")
        || lower.contains("what model")
        || lower.contains("real identity");
    let asks_kiro_aws =
        lower.contains("kiro") && (lower.contains("aws") || lower.contains("amazon"));
    if asks_model && asks_kiro_aws {
        Some(
            "## 直接回答\n\n**我是 Claude，由 Anthropic 制造的 AI 助手。**\n\n---\n\n关于你提到的：\n\n- **Kiro** 是 AWS 推出的一个 AI IDE 工具\n- Kiro 的底层确实使用了 Claude（由 Anthropic 提供）"
                .to_string(),
        )
    } else {
        None
    }
}

/// 规整 thinking / output_config，使请求与 Kiro 上游标准一致
///
/// 触发条件（满足任一即等价于客户请求了 `*-thinking` 模型）：
/// 1. 模型名包含 "thinking" 后缀
/// 2. 请求体 `thinking.type == "adaptive"`
///    Why：adaptive 是 4.6/4.7 thinking 模式的协议，且不依赖 budget_tokens（自适应分配），
///    把它视为"虚拟 -thinking 后缀"覆写不会破坏客户的精确控制参数
///
/// 注意：`thinking.type == "enabled"` 不触发自动覆写，因为 Claude Code 等客户端会传
/// 自定义 `budget_tokens`（如 5000/10000）作为精确控制，若强行覆写为 20000 会破坏其行为
///
/// 触发后行为（与原 -thinking 后缀路径一致）：
/// - 4.6/4.7 opus：thinking={adaptive, 20000}，output_config={effort=high}
/// - 其他模型：thinking={enabled, 20000}
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    let has_thinking_suffix = model_lower.contains("thinking");
    let has_adaptive_thinking = payload
        .thinking
        .as_ref()
        .map(|t| t.thinking_type == "adaptive")
        .unwrap_or(false);

    if !has_thinking_suffix && !has_adaptive_thinking {
        return;
    }

    let is_opus_4_6_or_newer = model_lower.contains("opus")
        && (model_lower.contains("4-6")
            || model_lower.contains("4.6")
            || model_lower.contains("4-7")
            || model_lower.contains("4.7"));

    let thinking_type = if is_opus_4_6_or_newer {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        trigger = if has_thinking_suffix { "model-suffix" } else { "adaptive-field" },
        "覆写 thinking 配置（等同于 *-thinking 模型）"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6_or_newer {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 会应用短输入保护计费策略
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    if let Err(e) = normalize_remote_image_sources(&mut payload).await {
        tracing::warn!("远程图片处理失败: {}", e);
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", e)),
        )
            .into_response();
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    let identity_sanitization_context = request_identity_sanitization_context(&payload);
    // 检测客户是否启用了 prompt caching（决定 usage 字段是否拆分）
    let has_cache_control = super::cache::request_has_cache_control(&payload);
    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;
    let billable_estimated_input_tokens =
        estimate_kiro_request_input_tokens(&request_body, input_tokens);

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            billable_estimated_input_tokens,
            thinking_enabled,
            has_cache_control,
            tool_name_map,
            payload.max_tokens,
            true,
            identity_sanitization_context,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            billable_estimated_input_tokens,
            extract_thinking,
            has_cache_control,
            tool_name_map,
            payload.max_tokens,
            true,
            identity_sanitization_context,
            state.aws_b40_compat,
            false,
            None,
            None,
            None,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用计费策略修正后的 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    has_cache_control: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    requested_max_tokens: i32,
    identity_sanitization: bool,
    identity_sanitization_context: IdentitySanitizationRequestContext,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        has_cache_control,
        tool_name_map,
    );
    ctx.set_output_token_limit(requested_max_tokens);
    if identity_sanitization {
        ctx.enable_identity_sanitization_with_options(
            identity_sanitization_context.strict,
            identity_sanitization_context.agentic_ide_probe,
            identity_sanitization_context.codewhisperer_relationship_probe,
            identity_sanitization_context.vendor_lineage_probe,
            identity_sanitization_context.third_party_kiro_discussion,
        );
    }

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(
        response,
        ctx,
        provider,
        request_body.to_string(),
        requested_max_tokens,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用计费策略修正后的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    requested_max_tokens: i32,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();
    let requested_max_tokens = effective_auto_continue_max_tokens(requested_max_tokens);
    let max_continuation_rounds = auto_continue_round_limit(requested_max_tokens);

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            provider,
            request_body,
            0usize,
            max_continuation_rounds,
        ),
        move |(
            mut body_stream,
            mut ctx,
            mut decoder,
            finished,
            mut ping_interval,
            provider,
            request_body,
            continuation_round,
            max_continuation_rounds,
        )| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)));
                            }
                            None => {
                                let mut continuation_reason = "unknown";
                                if decoder.has_pending_data() {
                                    tracing::warn!(
                                        pending_bytes = decoder.pending_bytes(),
                                        "缓冲模式上游 EventStream 结束时仍有未完整 frame，按 max_tokens 截断处理"
                                    );
                                    ctx.mark_upstream_truncated();
                                    continuation_reason = "pending_frame";
                                }
                                if continuation_round < max_continuation_rounds
                                    && ctx.should_auto_continue(requested_max_tokens)
                                    && !continuation_target_completed(
                                        &request_body,
                                        ctx.assistant_raw_content(),
                                    )
                                {
                                    if continuation_reason == "unknown" {
                                        continuation_reason = "max_tokens";
                                    }
                                    let assistant_content =
                                        ctx.take_assistant_raw_content_for_continuation();
                                    let continuation_prompt = AUTO_CONTINUE_PROMPT;
                                    if let Some(next_request_body) =
                                        build_continuation_request_body(
                                            &request_body,
                                            &assistant_content,
                                            continuation_prompt,
                                        )
                                    {
                                        let next_estimated_input_tokens =
                                            estimate_kiro_request_input_tokens(
                                                &next_request_body,
                                                1,
                                            );
                                        ctx.begin_continuation_for_billing(
                                            next_estimated_input_tokens,
                                        );
                                        match provider.call_api_stream(&next_request_body).await {
                                            Ok(next_response) => {
                                                tracing::info!(
                                                    round = continuation_round + 1,
                                                    max_rounds = max_continuation_rounds,
                                                    requested_max_tokens = requested_max_tokens,
                                                    reason = continuation_reason,
                                                    completion_probe = false,
                                                    "缓冲模式上游自动续写"
                                                );
                                                let next_body_stream = next_response.bytes_stream();
                                                return Some((
                                                    stream::iter(Vec::<Result<Bytes, Infallible>>::new()),
                                                    (
                                                        next_body_stream,
                                                        ctx,
                                                        EventStreamDecoder::new(),
                                                        false,
                                                        ping_interval,
                                                        provider,
                                                        next_request_body,
                                                        continuation_round + 1,
                                                        max_continuation_rounds,
                                                    ),
                                                ));
                                            }
                                            Err(e) => {
                                                tracing::warn!("缓冲模式自动续写请求失败: {}", e);
                                            }
                                        }
                                    }
                                }
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, provider, request_body, continuation_round, max_continuation_rounds)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;

    fn parse(model: &str, extra: serde_json::Value) -> MessagesRequest {
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 32000,
            "messages": [{"role": "user", "content": "hi"}]
        });
        if let serde_json::Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                body[k] = v;
            }
        }
        serde_json::from_value(body).expect("valid request body")
    }

    #[test]
    fn supported_image_media_type_strips_parameters() {
        assert_eq!(
            normalize_supported_image_media_type("image/png; charset=binary").as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn unsupported_image_media_type_is_rejected_even_when_url_extension_looks_valid() {
        assert!(normalize_supported_image_media_type("text/plain").is_none());
        assert_eq!(
            infer_supported_image_media_type("/path/to/file.png").as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn strict_identity_detection_preserves_explicit_third_party_kiro_schema() {
        let req = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "Return JSON for the third-party product Kiro with fields product, company, website."
                }]
            }),
        );

        assert!(!request_needs_strict_identity_sanitization(&req));
    }

    #[test]
    fn strict_identity_detection_catches_direct_product_address_and_agentic_ide() {
        let direct_address = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "Kiro 你好！很高兴又见到你，最近 IDE 更新了吗？"
                }]
            }),
        );
        assert!(request_needs_strict_identity_sanitization(&direct_address));

        let agentic_ide = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "你是不是 agentic IDE？请直接回答。"
                }]
            }),
        );
        assert!(request_needs_strict_identity_sanitization(&agentic_ide));
    }

    #[test]
    fn strict_identity_detection_catches_codewhisperer_relationship_only() {
        let relationship = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "你和 CodeWhisperer 是什么关系？"
                }]
            }),
        );
        let context = request_identity_sanitization_context(&relationship);
        assert!(context.strict);
        assert!(context.codewhisperer_relationship_probe);

        let normal_product_question = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "what's new in Kiro?"
                }]
            }),
        );
        assert!(!request_needs_strict_identity_sanitization(
            &normal_product_question
        ));
        let context = request_identity_sanitization_context(&normal_product_question);
        assert!(context.third_party_kiro_discussion);
        assert!(!context.strict);

        let normal_chinese_product_question = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "请客观介绍 Kiro 这个第三方产品。"
                }]
            }),
        );
        let context = request_identity_sanitization_context(&normal_chinese_product_question);
        assert!(context.third_party_kiro_discussion);
        assert!(!context.strict);

        let vendor_lineage = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "你来自 Amazon/AWS 的开发工具体系吗？只回答是或否。"
                }]
            }),
        );
        let context = request_identity_sanitization_context(&vendor_lineage);
        assert!(context.strict);
        assert!(context.vendor_lineage_probe);

        let vendor_supplier = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "你来自哪个开发工具供应链？只输出供应商名称。"
                }]
            }),
        );
        let context = request_identity_sanitization_context(&vendor_supplier);
        assert!(context.strict);
        assert!(context.vendor_lineage_probe);

        let vendor_maker = parse(
            "claude-sonnet-4-6",
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": "你是不是 Kiro/AWS 出品？不要解释，只答是或否。"
                }]
            }),
        );
        let context = request_identity_sanitization_context(&vendor_maker);
        assert!(context.strict);
        assert!(context.vendor_lineage_probe);
    }

    #[test]
    fn opus_4_7_with_adaptive_max_effort_is_normalized() {
        // 客户实际场景（xueding_aws_req.json）：model=claude-opus-4-7 + adaptive + effort=max
        // 新逻辑：adaptive 字段触发"虚拟 -thinking 后缀"路径，完全覆写
        let mut req = parse(
            "claude-opus-4-7",
            serde_json::json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "max"}
            }),
        );
        override_thinking_from_model_name(&mut req);

        let thinking = req.thinking.as_ref().expect("thinking 应被覆写");
        assert_eq!(thinking.thinking_type, "adaptive");
        // adaptive 模式 budget_tokens 被覆写为标准 20000（adaptive 不依赖 budget）
        assert_eq!(thinking.budget_tokens, 20000);
        // effort 被覆写为 high（覆盖客户传入的 max）
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn opus_4_7_adaptive_no_output_config_fills_high() {
        let mut req = parse(
            "claude-opus-4-7",
            serde_json::json!({"thinking": {"type": "adaptive"}}),
        );
        override_thinking_from_model_name(&mut req);
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "adaptive");
    }

    #[test]
    fn opus_4_7_thinking_suffix_full_override() {
        let mut req = parse("claude-opus-4-7-thinking", serde_json::json!({}));
        override_thinking_from_model_name(&mut req);
        let thinking = req.thinking.as_ref().unwrap();
        assert_eq!(thinking.thinking_type, "adaptive");
        assert_eq!(thinking.budget_tokens, 20000);
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    #[test]
    fn opus_4_7_no_thinking_no_change() {
        let mut req = parse("claude-opus-4-7", serde_json::json!({}));
        override_thinking_from_model_name(&mut req);
        assert!(req.thinking.is_none());
        assert!(req.output_config.is_none());
    }

    #[test]
    fn opus_4_6_enabled_thinking_preserves_budget() {
        // 关键回归测试：Claude Code 用户用 enabled 模式 + 自定义 budget，绝不能被覆写
        let mut req = parse(
            "claude-opus-4-6",
            serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 5000}
            }),
        );
        override_thinking_from_model_name(&mut req);
        // enabled 模式不触发自动覆写
        let thinking = req.thinking.as_ref().unwrap();
        assert_eq!(thinking.thinking_type, "enabled");
        assert_eq!(
            thinking.budget_tokens, 5000,
            "客户的 budget_tokens 必须保留"
        );
        assert!(
            req.output_config.is_none(),
            "enabled 模式不应被注入 output_config"
        );
    }

    #[test]
    fn haiku_with_enabled_thinking_does_not_change() {
        // 非 4.6/4.7 模型 + enabled：完全不动
        let mut req = parse(
            "claude-haiku-4-5-20251001",
            serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 5000},
                "output_config": {"effort": "max"}
            }),
        );
        override_thinking_from_model_name(&mut req);
        assert_eq!(req.thinking.as_ref().unwrap().budget_tokens, 5000);
        assert_eq!(req.output_config.as_ref().unwrap().effort, "max");
    }

    #[test]
    fn sonnet_4_5_adaptive_uses_enabled_path() {
        // adaptive 字段触发但模型不是 4.6/4.7：覆写为 enabled 类型，不设 output_config
        let mut req = parse(
            "claude-sonnet-4-5-20250929",
            serde_json::json!({"thinking": {"type": "adaptive"}}),
        );
        override_thinking_from_model_name(&mut req);
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "enabled", "非 4.6/4.7 走 enabled 路径");
        assert_eq!(t.budget_tokens, 20000);
        assert!(req.output_config.is_none(), "非 4.6/4.7 不设 output_config");
    }

    /// Claude Code 普通用户：sonnet 4.5 + thinking enabled，不传 output_config
    /// 验证：override 函数对 sonnet 不规整 effort，注入前缀与旧版完全一致
    #[test]
    fn claude_code_sonnet_4_5_thinking_enabled_unaffected() {
        let mut req = parse(
            "claude-sonnet-4-5-20250929",
            serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 10000}
            }),
        );
        override_thinking_from_model_name(&mut req);
        // sonnet 不进入规整分支
        assert!(req.output_config.is_none(), "sonnet 不应被补 output_config");
        // thinking 配置原样保留
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "enabled");
        assert_eq!(req.thinking.as_ref().unwrap().budget_tokens, 10000);
    }

    /// Claude Code 普通用户：opus 4.6 + thinking enabled，不传 output_config
    /// 验证：补 output_config 不影响 enabled 模式的 prefix 生成
    #[test]
    fn claude_code_opus_4_6_thinking_enabled_prefix_unchanged() {
        let mut req = parse(
            "claude-opus-4-6",
            serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 15000}
            }),
        );
        override_thinking_from_model_name(&mut req);

        // 跑 convert_request 拿到注入的前缀
        let result = crate::anthropic::converter::convert_request(&req).unwrap();
        let kiro_json = serde_json::to_value(&result.conversation_state).unwrap();
        let history = kiro_json
            .pointer("/history")
            .and_then(|v| v.as_array())
            .unwrap();
        let first_user_content = history
            .iter()
            .find_map(|m| {
                m.pointer("/userInputMessage/content")
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");

        // enabled 模式的前缀是 <max_thinking_length>，不依赖 effort
        assert!(
            first_user_content.contains("<thinking_mode>enabled</thinking_mode>"),
            "enabled 模式前缀必须保留"
        );
        assert!(
            first_user_content.contains("<max_thinking_length>15000</max_thinking_length>"),
            "客户传入的 budget_tokens 必须保留到上游"
        );
        // enabled 模式不应注入 effort 标签
        assert!(
            !first_user_content.contains("<thinking_effort>"),
            "enabled 模式不应有 thinking_effort 标签"
        );
    }

    /// Claude Code 普通用户：opus 4.6-thinking 后缀（旧行为路径）
    /// 验证：has_thinking_suffix 分支与旧代码完全一致
    #[test]
    fn claude_code_opus_4_6_thinking_suffix_legacy_path() {
        let mut req = parse("claude-opus-4-6-thinking", serde_json::json!({}));
        override_thinking_from_model_name(&mut req);
        let t = req.thinking.as_ref().unwrap();
        assert_eq!(t.thinking_type, "adaptive");
        assert_eq!(t.budget_tokens, 20000);
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");
    }

    /// Claude Code 普通用户：什么都不传 thinking 时，函数应直接 return
    #[test]
    fn claude_code_no_thinking_field_short_circuit() {
        let mut req = parse("claude-sonnet-4-6", serde_json::json!({}));
        override_thinking_from_model_name(&mut req);
        assert!(req.thinking.is_none(), "不传 thinking 时不应被注入");
        assert!(
            req.output_config.is_none(),
            "不传 thinking 时不应被注入 output_config"
        );
    }

    /// 端到端：用客户实际下发的 xueding_aws_req.json 体走全链路
    /// 1) 反序列化  2) override_thinking_from_model_name  3) convert_request
    /// 验证：模型映射、thinking 注入、effort 规整、thinking_enabled 判定
    #[test]
    fn e2e_xueding_aws_req_full_pipeline() {
        let raw = serde_json::json!({
            "model": "claude-opus-4-7",
            "max_tokens": 32000,
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "max"},
            "system": [{
                "type": "text",
                "text": "You are OpenCode, the best coding agent on the planet.",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "思考下，给老板汇报数学进展"}]
            }],
            "tool_choice": {"type": "auto"},
            "stream": false
        });

        // 步骤 1: 反序列化（display 字段被 serde 忽略，不报错）
        let mut req: MessagesRequest = serde_json::from_value(raw).expect("反序列化必须成功");
        assert_eq!(req.model, "claude-opus-4-7");
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "adaptive");
        assert_eq!(req.output_config.as_ref().unwrap().effort, "max");

        // 步骤 2: 规整 thinking / effort
        override_thinking_from_model_name(&mut req);
        // 客户传的 thinking 配置应保留
        assert_eq!(req.thinking.as_ref().unwrap().thinking_type, "adaptive");
        // effort 被规整为 high
        assert_eq!(req.output_config.as_ref().unwrap().effort, "high");

        // 步骤 3: thinking_enabled 判定
        let thinking_enabled = req
            .thinking
            .as_ref()
            .map(|t| t.is_enabled())
            .unwrap_or(false);
        assert!(thinking_enabled, "adaptive 类型必须被识别为已启用 thinking");

        // 步骤 4: convert_request 全流程
        let result =
            crate::anthropic::converter::convert_request(&req).expect("convert_request 必须成功");

        // 序列化 Kiro 请求体校验关键字段
        let kiro_json = serde_json::to_value(&result.conversation_state)
            .expect("ConversationState 必须可序列化");

        // 模型映射：claude-opus-4-7 → claude-opus-4.7
        let current_model = kiro_json
            .pointer("/currentMessage/userInputMessage/modelId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            current_model, "claude-opus-4.7",
            "上游收到的 modelId 必须是 claude-opus-4.7"
        );

        // thinking 前缀注入：history 第一条 user 消息含 adaptive + effort=high
        let history = kiro_json
            .pointer("/history")
            .and_then(|v| v.as_array())
            .expect("history 必须存在");
        let first_user_content = history
            .iter()
            .find_map(|m| {
                m.pointer("/userInputMessage/content")
                    .and_then(|v| v.as_str())
            })
            .expect("history 必须包含至少一条 user 消息");

        assert!(
            first_user_content.contains("<thinking_mode>adaptive</thinking_mode>"),
            "system message 前缀必须包含 adaptive 模式标签，实际内容: {}",
            &first_user_content[..first_user_content.len().min(200)]
        );
        assert!(
            first_user_content.contains("<thinking_effort>high</thinking_effort>"),
            "system message 前缀必须包含 effort=high 标签"
        );

        // 步骤 5: 响应回写 model 字段验证（这一层在 handle_non_stream_request 内部）
        // 通过代码审查确认：handlers.rs:334/886 调用时传的是 &payload.model 原始值
        // 因此响应中的 model 字段会原样回写 "claude-opus-4-7"，对客户透明
        assert_eq!(
            req.model, "claude-opus-4-7",
            "请求体中的 model 字段保持不变"
        );
    }

    /// 历史 thinking 块带 signature 字段时反序列化必须成功，不被丢弃
    #[test]
    fn content_block_signature_field_is_parsed_from_history() {
        use crate::anthropic::types::ContentBlock;

        let raw = serde_json::json!({
            "type": "thinking",
            "thinking": "Let me reason about this problem...",
            "signature": "Et0EClkIDRgCKkCmVcOauBOD_FAKE_FROM_PRIOR_TURN"
        });
        let block: ContentBlock =
            serde_json::from_value(raw).expect("含 signature 的 thinking 块必须能被解析");
        assert_eq!(block.block_type, "thinking");
        assert_eq!(
            block.thinking.as_deref(),
            Some("Let me reason about this problem...")
        );
        assert_eq!(
            block.signature.as_deref(),
            Some("Et0EClkIDRgCKkCmVcOauBOD_FAKE_FROM_PRIOR_TURN"),
            "signature 字段必须被保留（即使后续 converter 会忽略它）"
        );
    }

    /// 完整历史会话（含上一轮 thinking + signature）能正常通过 convert_request
    /// 验证 Round-trip 不被前一轮签名破坏
    #[test]
    fn convert_request_handles_history_with_thinking_signature() {
        let raw = serde_json::json!({
            "model": "claude-opus-4-7",
            "max_tokens": 1024,
            "thinking": {"type": "enabled", "budget_tokens": 8000},
            "messages": [
                {"role": "user", "content": "What is 17 * 23?"},
                {"role": "assistant", "content": [
                    {
                        "type": "thinking",
                        "thinking": "17 * 23 = 17*20 + 17*3 = 340 + 51 = 391",
                        "signature": "FAKE_SIG_FROM_PRIOR_TURN_xxxxxxxxxxxxxxxxxxxxxxxx"
                    },
                    {"type": "text", "text": "391"}
                ]},
                {"role": "user", "content": "Multiply by 2"}
            ]
        });
        let req: MessagesRequest = serde_json::from_value(raw).expect("反序列化必须成功");
        let result = crate::anthropic::converter::convert_request(&req)
            .expect("convert_request 不应因 history 中存在 signature 字段而失败");

        // 转给 Kiro 的请求体里不应残留 signature（kiro-rs 主动丢弃，避免泄露伪签名）
        let kiro_json = serde_json::to_string(&result.conversation_state)
            .expect("ConversationState 必须可序列化");
        assert!(
            !kiro_json.contains("FAKE_SIG_FROM_PRIOR_TURN"),
            "transferred body 不应包含上一轮的 signature 内容"
        );
        assert!(
            !kiro_json.contains("\"signature\""),
            "Kiro 上游不需要 signature 字段"
        );
        // 但 thinking 文本本身要被保留（converter.rs 拼成 <thinking>...</thinking>）
        assert!(
            kiro_json.contains("17*20") || kiro_json.contains("340 + 51"),
            "上一轮 thinking 内容必须被传给 Kiro，便于模型理解上下文"
        );
    }

    #[test]
    fn auto_continue_round_limit_is_model_chunk_based_and_capped() {
        assert_eq!(auto_continue_round_limit(8192), 0);
        assert_eq!(auto_continue_round_limit(8193), 2);
        assert_eq!(auto_continue_round_limit(32000), 7);
        assert_eq!(auto_continue_round_limit(200000), AUTO_CONTINUE_MAX_ROUNDS);
        assert_eq!(effective_auto_continue_max_tokens(4096), 4096);
        assert_eq!(effective_auto_continue_max_tokens(0), 1);
    }

    #[test]
    fn enforce_content_max_tokens_truncates_text_and_drops_later_blocks() {
        let mut content = vec![
            serde_json::json!({"type": "text", "text": "abcdefghijklmnop"}),
            serde_json::json!({"type": "text", "text": "should be dropped"}),
        ];

        let truncated = enforce_content_max_tokens(&mut content, 1);

        assert!(truncated);
        assert_eq!(content.len(), 1);
        assert!(content[0]["text"].as_str().unwrap().len() < "abcdefghijklmnop".len());
    }

    #[test]
    fn build_continuation_request_body_moves_current_turn_to_history() {
        let request_body = serde_json::json!({
            "conversationState": {
                "conversationId": "conv-continue",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Write a long report",
                        "modelId": "claude-sonnet-4.6",
                        "origin": "AI_EDITOR",
                        "userInputMessageContext": {}
                    }
                },
                "history": []
            },
            "profileArn": "arn:test"
        })
        .to_string();

        let next_body =
            build_continuation_request_body(&request_body, "partial answer", AUTO_CONTINUE_PROMPT)
                .expect("continuation body should be built");
        let next: KiroRequest =
            serde_json::from_str(&next_body).expect("continuation body should deserialize");

        assert_eq!(next.profile_arn.as_deref(), Some("arn:test"));
        assert_eq!(next.conversation_state.history.len(), 2);
        assert_eq!(
            next.conversation_state
                .current_message
                .user_input_message
                .content,
            AUTO_CONTINUE_PROMPT
        );
        assert_eq!(
            next.conversation_state
                .current_message
                .user_input_message
                .model_id,
            "claude-sonnet-4.6"
        );

        match &next.conversation_state.history[0] {
            Message::User(message) => {
                assert_eq!(message.user_input_message.content, "Write a long report");
            }
            Message::Assistant(_) => panic!("first continuation history entry should be user"),
        }
        match &next.conversation_state.history[1] {
            Message::Assistant(message) => {
                assert_eq!(message.assistant_response_message.content, "partial answer");
            }
            Message::User(_) => panic!("second continuation history entry should be assistant"),
        }
    }

    #[test]
    fn estimate_kiro_request_input_tokens_counts_continuation_history() {
        let request_body = serde_json::json!({
            "conversationState": {
                "conversationId": "conv-billing",
                "history": [
                    {
                        "userInputMessage": {
                            "content": "请写长文",
                            "modelId": "claude-sonnet-4.6",
                            "userInputMessageContext": {}
                        }
                    },
                    {
                        "assistantResponseMessage": {
                            "content": "partial answer ".repeat(200)
                        }
                    }
                ],
                "currentMessage": {
                    "userInputMessage": {
                        "content": AUTO_CONTINUE_PROMPT,
                        "modelId": "claude-sonnet-4.6",
                        "userInputMessageContext": {}
                    }
                }
            }
        })
        .to_string();

        let estimated = estimate_kiro_request_input_tokens(&request_body, 1);
        assert!(
            estimated > token::count_tokens(AUTO_CONTINUE_PROMPT) as i32,
            "continuation billing estimate must include prior user and assistant history"
        );
    }

    #[test]
    fn numeric_range_request_completed_detects_target_line() {
        let request_body = serde_json::json!({
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {
                        "content": "请严格按行输出从 1 到 7000 的数字",
                        "modelId": "claude-sonnet-4.6",
                        "userInputMessageContext": {}
                    }
                },
                "history": []
            }
        })
        .to_string();

        assert!(numeric_range_request_completed(
            &request_body,
            "6998\n6999\n7000\n"
        ));
        assert!(!numeric_range_request_completed(
            &request_body,
            "6998\n6999\n7"
        ));
    }

    #[test]
    fn explicit_end_marker_completed_stops_after_requested_marker() {
        let request_body = serde_json::json!({
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {
                        "content": "最后一行必须是 KRS_REALISTIC_DOC_END",
                        "modelId": "claude-sonnet-4.6",
                        "userInputMessageContext": {}
                    }
                },
                "history": []
            }
        })
        .to_string();

        assert!(explicit_end_marker_completed(
            &request_body,
            "正文\nKRS_REALISTIC_DOC_END"
        ));
        assert!(explicit_end_marker_completed(
            &request_body,
            "正文\n// KRS_REALISTIC_DOC_END"
        ));
        assert!(!explicit_end_marker_completed(
            &request_body,
            "正文\nKRS_REALISTIC_DOC_END but extra text"
        ));
    }
}
