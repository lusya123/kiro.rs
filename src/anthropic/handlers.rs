//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse, OutputConfig, Thinking};
use super::websearch;

const MAX_REMOTE_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const REMOTE_IMAGE_FETCH_TIMEOUT_SECS: u64 = 30;

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

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776384000, // Apr 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776384000, // Apr 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "glm-5".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "zhipu".to_string(),
            display_name: "GLM-5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "minimax-m2.5".to_string(),
            object: "model".to_string(),
            created: 1770314400,
            owned_by: "minimax".to_string(),
            display_name: "MiniMax M2.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
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

    // 检测客户是否启用了 prompt caching（决定 usage 字段是否拆分成 cache_*）
    let has_cache_control = super::cache::request_has_cache_control(&payload);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

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
            input_tokens,
            thinking_enabled,
            has_cache_control,
            tool_name_map,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            has_cache_control,
            tool_name_map,
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

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events);

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
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS))),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
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

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                        None => {
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
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
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
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

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
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
                                    serde_json::from_str(buffer)
                                        .unwrap_or_else(|e| {
                                            tracing::warn!(
                                                "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                                e, tool_use.tool_use_id
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
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (window_size as f64)
                                / 100.0)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
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

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text,
                "signature": super::signature::generate_fake_signature()
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 根据客户请求意图拆分 usage（带 cache_control → 拆成 I/CR/CC，否则平铺）
    let usage_breakdown = super::cache::compute_usage_breakdown(final_input_tokens, has_cache_control);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
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
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
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

    // 检测客户是否启用了 prompt caching（决定 usage 字段是否拆分）
    let has_cache_control = super::cache::request_has_cache_control(&payload);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

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
            input_tokens,
            thinking_enabled,
            has_cache_control,
            tool_name_map,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            has_cache_control,
            tool_name_map,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    has_cache_control: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        has_cache_control,
        tool_name_map,
    );

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx);

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
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
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
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)));
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
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
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
        assert_eq!(thinking.budget_tokens, 5000, "客户的 budget_tokens 必须保留");
        assert!(req.output_config.is_none(), "enabled 模式不应被注入 output_config");
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
        let history = kiro_json.pointer("/history").and_then(|v| v.as_array()).unwrap();
        let first_user_content = history.iter().find_map(|m| {
            m.pointer("/userInputMessage/content").and_then(|v| v.as_str())
        }).unwrap_or("");

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
        assert!(req.output_config.is_none(), "不传 thinking 时不应被注入 output_config");
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
        let thinking_enabled = req.thinking.as_ref().map(|t| t.is_enabled()).unwrap_or(false);
        assert!(thinking_enabled, "adaptive 类型必须被识别为已启用 thinking");

        // 步骤 4: convert_request 全流程
        let result = crate::anthropic::converter::convert_request(&req)
            .expect("convert_request 必须成功");

        // 序列化 Kiro 请求体校验关键字段
        let kiro_json = serde_json::to_value(&result.conversation_state)
            .expect("ConversationState 必须可序列化");

        // 模型映射：claude-opus-4-7 → claude-opus-4.6
        let current_model = kiro_json
            .pointer("/currentMessage/userInputMessage/modelId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            current_model, "claude-opus-4.6",
            "上游收到的 modelId 必须是 claude-opus-4.6（4.7 兜底映射）"
        );

        // thinking 前缀注入：history 第一条 user 消息含 adaptive + effort=high
        let history = kiro_json.pointer("/history").and_then(|v| v.as_array())
            .expect("history 必须存在");
        let first_user_content = history.iter().find_map(|m| {
            m.pointer("/userInputMessage/content").and_then(|v| v.as_str())
        }).expect("history 必须包含至少一条 user 消息");

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
        assert_eq!(req.model, "claude-opus-4-7", "请求体中的 model 字段保持不变");
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
        let block: ContentBlock = serde_json::from_value(raw)
            .expect("含 signature 的 thinking 块必须能被解析");
        assert_eq!(block.block_type, "thinking");
        assert_eq!(block.thinking.as_deref(), Some("Let me reason about this problem..."));
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
}
