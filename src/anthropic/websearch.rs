//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成

use std::{convert::Infallible, time::Instant};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::id;
use super::stream::SseEvent;
use super::types::{ErrorResponse, MessagesRequest};

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

/// 检查请求是否为纯 WebSearch 请求
///
/// 条件：tools 有且只有一个，且 name 为 web_search
pub fn has_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools.len() == 1 && tools.first().is_some_and(|t| t.name == "web_search")
    })
}

/// 从消息中提取搜索查询
///
/// 读取 messages 的第一条消息的第一个内容块
/// 并去除 "Perform a web search for the query: " 前缀
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    // 获取第一条消息
    let first_msg = req.messages.first()?;

    // 提取文本内容
    let text = match &first_msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 获取第一个内容块
            let first_block = arr.first()?;
            if first_block.get("type")?.as_str()? == "text" {
                first_block.get("text")?.as_str()?.to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // 去除前缀 "Perform a web search for the query: "
    const PREFIX: &str = "Perform a web search for the query: ";
    let query = if text.starts_with(PREFIX) {
        text[PREFIX.len()..].to_string()
    } else {
        text
    };

    if query.is_empty() { None } else { Some(query) }
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Build an opaque, replay-safe wire token for Anthropic web-search fields.
///
/// Anthropic documents `encrypted_content` and `encrypted_index` as opaque values
/// that clients must return unchanged on later turns. The MCP backend gives us
/// plaintext snippets instead, so exposing those snippets under an encrypted
/// field is both structurally inaccurate and easy for clients to misinterpret.
/// The random server-tool ID scopes these deterministic tokens to one response;
/// later turns do not need to decode them because the visible answer text is kept
/// in conversation history.
fn opaque_websearch_bytes(
    domain: &str,
    tool_use_id: &str,
    ordinal: usize,
    result: &WebSearchResult,
    output_bytes: usize,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(output_bytes);
    let mut counter = 0_u32;

    while output.len() < output_bytes {
        let mut hasher = Sha256::new();
        hasher.update(b"kiro-rs:websearch-wire:v1\0");
        hasher.update(domain.as_bytes());
        hasher.update([0]);
        hasher.update(tool_use_id.as_bytes());
        hasher.update((ordinal as u64).to_be_bytes());
        hasher.update(result.url.as_bytes());
        hasher.update([0]);
        hasher.update(result.title.as_bytes());
        hasher.update([0]);
        if let Some(snippet) = result.snippet.as_deref() {
            hasher.update(snippet.as_bytes());
        }
        hasher.update(counter.to_be_bytes());
        output.extend_from_slice(&hasher.finalize());
        counter = counter.wrapping_add(1);
    }

    output.truncate(output_bytes);
    output
}

fn push_varint(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn push_varint_field(output: &mut Vec<u8>, field: u8, value: usize) {
    output.push(field << 3);
    push_varint(output, value);
}

fn push_len_field(output: &mut Vec<u8>, field: u8, value: &[u8]) {
    output.push((field << 3) | 2);
    push_varint(output, value.len());
    output.extend_from_slice(value);
}

fn opaque_websearch_uuid(
    domain: &str,
    tool_use_id: &str,
    ordinal: usize,
    result: &WebSearchResult,
) -> String {
    let mut bytes = opaque_websearch_bytes(domain, tool_use_id, ordinal, result, 16);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn opaque_websearch_token(
    domain: &str,
    kind: usize,
    protobuf_inner_bytes: usize,
    tool_use_id: &str,
    ordinal: usize,
    result: &WebSearchResult,
) -> String {
    let mut metadata = Vec::with_capacity(42);
    push_varint_field(&mut metadata, 1, kind);
    push_varint_field(&mut metadata, 3, 1);
    push_len_field(
        &mut metadata,
        4,
        opaque_websearch_uuid(domain, tool_use_id, ordinal, result).as_bytes(),
    );

    let mut inner = Vec::with_capacity(protobuf_inner_bytes);
    push_len_field(&mut inner, 1, &metadata);
    let remaining = protobuf_inner_bytes.saturating_sub(inner.len());
    let payload_bytes = if remaining >= 128 {
        remaining - 3
    } else {
        remaining - 2
    };
    let payload = opaque_websearch_bytes(domain, tool_use_id, ordinal, result, payload_bytes);
    push_len_field(&mut inner, 2, &payload);
    debug_assert_eq!(inner.len(), protobuf_inner_bytes);

    let mut envelope = Vec::with_capacity(protobuf_inner_bytes + 3);
    push_len_field(&mut envelope, 2, &inner);
    BASE64_STANDARD.encode(envelope)
}

fn encrypted_content_token(tool_use_id: &str, ordinal: usize, result: &WebSearchResult) -> String {
    opaque_websearch_token("content", 1, 4_008, tool_use_id, ordinal, result)
}

fn encrypted_index_token(tool_use_id: &str, ordinal: usize, result: &WebSearchResult) -> String {
    opaque_websearch_token("index", 2, 143, tool_use_id, ordinal, result)
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 使用相同格式
    let tool_use_id = id::server_tool_use_id();

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// 解析 MCP 响应中的搜索结果
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    let result = mcp_response.result.as_ref()?;
    let content = result.content.first()?;

    if content.content_type != "text" {
        return None;
    }

    serde_json::from_str(&content.text).ok()
}

/// 生成 WebSearch SSE 响应流
#[allow(dead_code)]
pub fn create_websearch_sse_stream(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    create_websearch_sse_stream_with_profile(
        model,
        query,
        tool_use_id,
        search_results,
        input_tokens,
        false,
        0,
    )
}

fn create_websearch_sse_stream_with_profile(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
    aws_b40_compat: bool,
    invocation_latency_ms: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let events = generate_websearch_events(
        &model,
        &query,
        &tool_use_id,
        search_results,
        input_tokens,
        aws_b40_compat,
        invocation_latency_ms,
    );

    stream::iter(
        events
            .into_iter()
            .map(move |e| Ok(Bytes::from(e.to_profile_sse_string(aws_b40_compat)))),
    )
}

/// 生成 WebSearch SSE 事件序列
fn generate_websearch_events(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
    aws_b40_compat: bool,
    invocation_latency_ms: u64,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = if aws_b40_compat {
        super::bedrock::response_id(model)
    } else {
        id::message_id()
    };
    let public_model = if aws_b40_compat {
        super::bedrock::response_model(model)
    } else {
        model.to_string()
    };
    let start_usage = if aws_b40_compat {
        json!({
            "input_tokens": input_tokens,
            "output_tokens": 16,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 0,
                "ephemeral_1h_input_tokens": 0
            },
            "service_tier": "standard"
        })
    } else {
        json!({
            "input_tokens": input_tokens,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        })
    };

    // 1. message_start
    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": public_model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "stop_details": null,
                "usage": start_usage
            }
        }),
    ));

    // 2. content_block_start (server_tool_use, index 0) —— 对齐真 Claude(pomoai/Bedrock):
    // web search 直接以 server_tool_use 开头,**不带**前导"I'll search…"文本块(否则块数比真 Claude 多 1)。
    // start 时 input 为空对象,随后用 input_json_delta 增量发送,再 stop。
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search",
                "input": {}
            }
        }),
    ));

    // 2b. input_json_delta:query 以 JSON 字符串增量发送
    let query_json = json!({ "query": query }).to_string();
    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": query_json
            }
        }),
    ));

    // 3. content_block_stop (server_tool_use)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 0
        }),
    ));

    // 5. content_block_start (web_search_tool_result, index 2)
    // 真 Anthropic 的 web_search_tool_result 带 tool_use_id,指向前面的 server_tool_use.id
    let search_content = if let Some(ref results) = search_results {
        results
            .results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let page_age = r.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": r.title,
                    "url": r.url,
                    "encrypted_content": encrypted_content_token(tool_use_id, i, r),
                    "page_age": page_age
                })
            })
            .collect::<Vec<_>>()
    } else {
        vec![]
    };

    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content
            }
        }),
    ));

    // 6. content_block_stop (web_search_tool_result)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 1
        }),
    ));

    // 7. content_block_start (text, index 3)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    // 8. content_block_delta (text_delta) - 生成搜索结果摘要
    let summary = generate_search_summary(query, &search_results);

    // 分块发送文本
    let chunk_size = 100;
    for chunk in summary.chars().collect::<Vec<_>>().chunks(chunk_size) {
        let text: String = chunk.iter().collect();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ));
    }

    // 8b. citations (web_search_result_location) —— 真 Anthropic 会在正文块附来源引用,
    // 检测器要求文本带 type=web_search_result_location 的 citations。
    if let Some(ref results) = search_results {
        for (i, r) in results.results.iter().enumerate() {
            let cited: String = r
                .snippet
                .clone()
                .unwrap_or_default()
                .chars()
                .take(150)
                .collect();
            events.push(SseEvent::new(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": {
                        "type": "citations_delta",
                        "citation": {
                            "type": "web_search_result_location",
                            "url": r.url,
                            "title": r.title,
                            "encrypted_index": encrypted_index_token(tool_use_id, i, r),
                            "cited_text": cited
                        }
                    }
                }),
            ));
        }
    }

    // 9. content_block_stop (text)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 2
        }),
    ));

    // 10. message_delta
    // 官方 API 的 message_delta.delta 中没有 stop_sequence 字段
    let base_output_tokens = super::claude_tok::count_claude(&summary);
    let output_tokens = if aws_b40_compat {
        super::bedrock::framed_output_tokens(base_output_tokens, 3, 0)
    } else {
        (summary.len() as i32 + 3) / 4
    };
    let delta_usage = if aws_b40_compat {
        let mut usage = super::bedrock::stream_delta_usage(
            model,
            super::cache::UsageBreakdown::flat(input_tokens),
            output_tokens,
            0,
        );
        usage["server_tool_use"] = json!({ "web_search_requests": 1 });
        usage
    } else {
        json!({
            "output_tokens": output_tokens,
            "server_tool_use": { "web_search_requests": 1 }
        })
    };
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "stop_details": null
            },
            "usage": delta_usage
        }),
    ));

    // 11. message_stop
    events.push(SseEvent::new(
        "message_stop",
        if aws_b40_compat {
            json!({
                "type": "message_stop",
                "amazon-bedrock-invocationMetrics": super::bedrock::invocation_metrics(
                    super::cache::UsageBreakdown::flat(input_tokens),
                    output_tokens,
                    invocation_latency_ms,
                    invocation_latency_ms
                )
            })
        } else {
            json!({ "type": "message_stop" })
        },
    ));

    events
}

/// 生成搜索结果摘要
fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
    aws_b40_compat: bool,
) -> Response {
    // 1. 提取搜索查询
    let query = match extract_search_query(payload) {
        Some(q) => q,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response();
        }
    };

    tracing::info!(query = %query, "处理 WebSearch 请求");

    // 2. 创建 MCP 请求
    let (tool_use_id, mcp_request) = create_mcp_request(&query);

    // 3. 调用 Kiro MCP API
    let invocation_started = Instant::now();
    let search_results = match call_mcp_api(&provider, &mcp_request).await {
        Ok(response) => parse_search_results(&response),
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
            None
        }
    };
    let invocation_latency_ms = invocation_started.elapsed().as_millis() as u64;

    // 4. 按 stream 参数返回:非流式返回 JSON,流式返回 SSE(真 Anthropic 两种都支持)
    let model = payload.model.clone();
    if !payload.stream {
        let body = build_websearch_json(
            &model,
            &query,
            &tool_use_id,
            &search_results,
            input_tokens,
            aws_b40_compat,
        );
        return (StatusCode::OK, Json(body)).into_response();
    }
    let stream = create_websearch_sse_stream_with_profile(
        model,
        query,
        tool_use_id,
        search_results,
        input_tokens,
        aws_b40_compat,
        invocation_latency_ms,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 构造非流式 WebSearch 响应(content: server_tool_use + web_search_tool_result + 带 citations 的 text)
fn build_websearch_json(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: &Option<WebSearchResults>,
    input_tokens: i32,
    aws_b40_compat: bool,
) -> serde_json::Value {
    let search_content: Vec<serde_json::Value> = match search_results {
        Some(r) => r
            .results
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let page_age = x.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": x.title,
                    "url": x.url,
                    "encrypted_content": encrypted_content_token(tool_use_id, i, x),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    };
    let citations: Vec<serde_json::Value> = match search_results {
        Some(r) => r
            .results
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let cited: String = x
                    .snippet
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(150)
                    .collect();
                json!({
                    "type": "web_search_result_location",
                    "url": x.url,
                    "title": x.title,
                    "encrypted_index": encrypted_index_token(tool_use_id, i, x),
                    "cited_text": cited
                })
            })
            .collect(),
        None => vec![],
    };
    let summary = generate_search_summary(query, search_results);
    let base_output_tokens = super::claude_tok::count_claude(&summary);
    let output_tokens = if aws_b40_compat {
        super::bedrock::framed_output_tokens(base_output_tokens, 3, 0)
    } else {
        (summary.len() as i32 + 3) / 4
    };
    let message_id = if aws_b40_compat {
        super::bedrock::response_id(model)
    } else {
        id::message_id()
    };
    let public_model = if aws_b40_compat {
        super::bedrock::response_model(model)
    } else {
        model.to_string()
    };
    let mut usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "server_tool_use": {"web_search_requests": 1}
    });
    if aws_b40_compat {
        usage["cache_creation"] = json!({
            "ephemeral_5m_input_tokens": 0,
            "ephemeral_1h_input_tokens": 0
        });
        usage["service_tier"] = json!("standard");
        if model.to_ascii_lowercase().contains("opus") {
            usage["output_tokens_details"] = json!({ "thinking_tokens": 0 });
        }
    }
    json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": public_model,
        "content": [
            {"type": "server_tool_use", "id": tool_use_id, "name": "web_search", "input": {"query": query}},
            {"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": search_content},
            {"type": "text", "text": summary, "citations": citations}
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "stop_details": null,
        "usage": usage
    })
}

/// 调用 Kiro MCP API
async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let response = provider.call_mcp(&request_body).await?;

    let body = response.text().await?;
    tracing::debug!("MCP response: {}", body);

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok(mcp_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_web_search_tool_only_one() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            cache_control: None,
            metadata: None,
        };

        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_has_web_search_tool_multiple_tools() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                    cache_control: None,
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            cache_control: None,
            metadata: None,
        };

        // 多个工具时不应该被识别为纯 websearch 请求
        assert!(!has_web_search_tool(&req));
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            cache_control: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            cache_control: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }

    #[test]
    fn test_websearch_wire_tokens_are_opaque_stable_and_domain_separated() {
        let result = WebSearchResult {
            title: "Visible title".to_string(),
            url: "https://example.com/private-path".to_string(),
            snippet: Some(
                "A human-readable search snippet that must not be mislabeled as ciphertext"
                    .to_string(),
            ),
            published_date: None,
            id: None,
            domain: None,
            max_verbatim_word_limit: None,
            public_domain: None,
        };
        let tool_use_id = "srvtoolu_01OpaqueTokenTest";

        let content = encrypted_content_token(tool_use_id, 0, &result);
        let content_again = encrypted_content_token(tool_use_id, 0, &result);
        let index = encrypted_index_token(tool_use_id, 0, &result);
        let decoded_content = BASE64_STANDARD.decode(&content).unwrap();

        assert_eq!(content, content_again);
        let decoded_index = BASE64_STANDARD.decode(&index).unwrap();
        assert_eq!(decoded_content.len(), 4_011);
        assert_eq!(decoded_index.len(), 146);
        assert_eq!(&decoded_content[..5], &[0x12, 0xa8, 0x1f, 0x0a, 0x2a]);
        assert_eq!(&decoded_index[..5], &[0x12, 0x8f, 0x01, 0x0a, 0x2a]);
        assert_eq!(
            &decoded_content[5..11],
            &[0x08, 0x01, 0x18, 0x01, 0x22, 0x24]
        );
        assert_eq!(&decoded_index[5..11], &[0x08, 0x02, 0x18, 0x01, 0x22, 0x24]);
        assert_ne!(
            &decoded_content[11..47],
            &decoded_index[11..47],
            "content and citation tokens use distinct metadata UUIDs"
        );
        assert_ne!(content, index);
        assert!(
            !decoded_content
                .windows(result.snippet.as_deref().unwrap().len())
                .any(|window| window == result.snippet.as_deref().unwrap().as_bytes())
        );
        assert!(
            !decoded_content
                .windows(result.url.len())
                .any(|window| window == result.url.as_bytes())
        );
        assert_ne!(
            content,
            encrypted_content_token("srvtoolu_01DifferentResponse", 0, &result)
        );
        assert_ne!(content, encrypted_content_token(tool_use_id, 1, &result));
    }

    #[test]
    fn aws_b_websearch_sse_uses_opaque_bedrock_tokens_and_metrics() {
        let result = WebSearchResult {
            title: "Visible title".to_string(),
            url: "https://example.com/current".to_string(),
            snippet: Some(
                "A current result used to verify the public citation envelope.".to_string(),
            ),
            published_date: Some(1_752_710_400_000),
            id: None,
            domain: None,
            max_verbatim_word_limit: None,
            public_domain: None,
        };
        let events = generate_websearch_events(
            "claude-opus-4-8",
            "current test query",
            "srvtoolu_01OpaqueSseTest",
            Some(WebSearchResults {
                results: vec![result.clone()],
                total_results: Some(1),
                query: Some("current test query".to_string()),
                error: None,
            }),
            556,
            true,
            1_234,
        );

        let message_start = events
            .iter()
            .find(|event| event.event == "message_start")
            .expect("message_start");
        assert_eq!(message_start.data["message"]["model"], "claude-opus-4-8");
        assert!(
            message_start.data["message"]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_01bdrk") && id.len() == 28)
        );

        let result_start = events
            .iter()
            .find(|event| event.data["content_block"]["type"] == "web_search_tool_result")
            .expect("web search result block");
        let public_result = &result_start.data["content_block"]["content"][0];
        assert_eq!(public_result["title"], result.title);
        assert_eq!(public_result["url"], result.url);
        let encrypted_content = BASE64_STANDARD
            .decode(
                public_result["encrypted_content"]
                    .as_str()
                    .expect("content token"),
            )
            .expect("content token base64");
        assert_eq!(encrypted_content.len(), 4_011);
        assert_eq!(
            &encrypted_content[..11],
            &[
                0x12, 0xa8, 0x1f, 0x0a, 0x2a, 0x08, 0x01, 0x18, 0x01, 0x22, 0x24
            ]
        );
        assert!(
            !encrypted_content
                .windows(result.url.len())
                .any(|window| window == result.url.as_bytes())
        );

        let citation = events
            .iter()
            .find(|event| event.data["delta"]["type"] == "citations_delta")
            .expect("citation delta");
        assert_eq!(citation.data["delta"]["citation"]["url"], result.url);
        let encrypted_index = BASE64_STANDARD
            .decode(
                citation.data["delta"]["citation"]["encrypted_index"]
                    .as_str()
                    .expect("index token"),
            )
            .expect("index token base64");
        assert_eq!(encrypted_index.len(), 146);
        assert_eq!(
            &encrypted_index[..11],
            &[
                0x12, 0x8f, 0x01, 0x0a, 0x2a, 0x08, 0x02, 0x18, 0x01, 0x22, 0x24
            ]
        );

        let final_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta")
            .data["usage"];
        assert_eq!(final_usage["input_tokens"], 556);
        assert_eq!(final_usage["server_tool_use"]["web_search_requests"], 1);
        assert_eq!(final_usage["output_tokens_details"]["thinking_tokens"], 0);

        let metrics = &events
            .iter()
            .find(|event| event.event == "message_stop")
            .expect("message_stop")
            .data["amazon-bedrock-invocationMetrics"];
        assert_eq!(metrics["inputTokenCount"], 556);
        assert_eq!(metrics["cacheReadInputTokenCount"], 0);
        assert_eq!(metrics["cacheWriteInputTokenCount"], 0);
        assert_eq!(metrics["invocationLatency"], 1_234);
        assert_eq!(metrics["firstByteLatency"], 1_234);
    }
}
