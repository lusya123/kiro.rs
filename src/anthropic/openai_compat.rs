//! OpenAI 兼容端点 `POST /v1/chat/completions`。
//!
//! 目的:检测器(如猫眼)的 `usage_backend_fingerprint` 探针会打 `/v1/chat/completions`,
//! 通过 usage 字段的键集判断上游是否为"优质逆向渠道"。此前本服务无此路由 → 404 → `usage_keys:[]`,
//! 被判缺少异源痕迹而扣分。这里实现该端点:把 OpenAI 请求转成内部 Anthropic 请求、复用
//! `post_messages` 生成,再把响应转回 OpenAI ChatCompletion 形态,usage 输出**混合键**
//! (OpenAI `prompt_tokens/completion_tokens/...` + Anthropic `input_tokens/output_tokens` +
//! Claude 缓存键),与真实 reverse-channel(new-api 类)一致。
//!
//! 不影响用户正常使用:真 Claude Code 走 `/v1/messages`;本路由是**新增**的,只对显式打
//! `/v1/chat/completions` 的客户端(检测器 / OpenAI 客户端)生效。

use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};

use super::handlers::{RawApiJson, post_messages};
use super::middleware::AppState;
use super::types::{Message, MessagesRequest, Metadata, SystemMessage, Tool};

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

fn openai_tools_to_anthropic(oai: &Value) -> Option<Vec<Tool>> {
    let tools: Vec<Tool> = oai
        .get("tools")?
        .as_array()?
        .iter()
        .filter_map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return None;
            }
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            let description = function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input_schema = function
                .get("parameters")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default();
            Some(Tool {
                tool_type: None,
                name,
                description,
                input_schema,
                max_uses: None,
                cache_control: None,
            })
        })
        .collect();
    (!tools.is_empty()).then_some(tools)
}

fn openai_tool_choice_to_anthropic(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    if let Some(value) = choice.as_str() {
        return match value {
            "required" => Some(json!({"type": "any"})),
            "auto" => Some(json!({"type": "auto"})),
            "none" => None,
            _ => None,
        };
    }
    let function_name = choice
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)?;
    Some(json!({"type": "tool", "name": function_name}))
}

fn openai_max_tokens(oai: &Value) -> i32 {
    oai.get("max_tokens")
        .or_else(|| oai.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .filter(|tokens| (1..=i32::MAX as i64).contains(tokens))
        .map(|tokens| tokens as i32)
        .unwrap_or(1024)
}

fn openai_usage(usage: &Value, aws_b40_compat: bool) -> Value {
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

    if aws_b40_compat {
        return json!({
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
        });
    }

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
    let tool_calls: Vec<Value> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|block| {
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            json!({
                "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
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
    let source_id = a.get("id").and_then(|v| v.as_str()).unwrap_or("msg_kiro");
    let id = if aws_b40_compat {
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
    if !aws_b40_compat {
        choice["logprobs"] = Value::Null;
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [choice],
        "usage": openai_usage(&usage, aws_b40_compat)
    })
}

struct OpenAiStreamState {
    id: String,
    model: String,
    created: u64,
    aws_b40_compat: bool,
    include_usage: bool,
    usage: Value,
    tool_indices: HashMap<i64, usize>,
    next_tool_index: usize,
    done: bool,
}

impl OpenAiStreamState {
    fn new(model: String, created: u64, include_usage: bool, aws_b40_compat: bool) -> Self {
        Self {
            id: if aws_b40_compat {
                "msg_bdrk_pending".to_string()
            } else {
                "chatcmpl-kiro".to_string()
            },
            model,
            created,
            aws_b40_compat,
            include_usage,
            usage: json!({}),
            tool_indices: HashMap::new(),
            next_tool_index: 0,
            done: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        if self.aws_b40_compat {
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
        if self.aws_b40_compat {
            return json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "system_fingerprint": null,
                "choices": [],
                "usage": openai_usage(&self.usage, true)
            });
        }
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": openai_usage(&self.usage, false)
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

fn transform_anthropic_sse_event(
    state: &mut OpenAiStreamState,
    event_name: &str,
    event: &Value,
) -> String {
    match event_name {
        "ping" => ": ping\n\n".to_string(),
        "message_start" => {
            if let Some(message) = event.get("message") {
                if let Some(id) = message.get("id").and_then(Value::as_str) {
                    state.id = if state.aws_b40_compat {
                        id.to_string()
                    } else {
                        id.replacen("msg_", "chatcmpl-", 1)
                    };
                }
                if let Some(model) = message.get("model").and_then(Value::as_str) {
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
            openai_sse_json(&state.chunk(
                json!({
                    "tool_calls": [{
                        "index": tool_index,
                        "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
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
            if state.done {
                return String::new();
            }
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
                            return Some((
                                Ok::<Bytes, Infallible>(Bytes::from(transformed)),
                                (input, buffer, state, false),
                            ));
                        }
                    }
                    Some(Err(_)) => {
                        state.done = true;
                        let error = json!({
                            "error": {
                                "message": "upstream stream terminated unexpectedly",
                                "type": "server_error"
                            }
                        });
                        let output = format!("{}data: [DONE]\n\n", openai_sse_json(&error));
                        return Some((Ok(Bytes::from(output)), (input, buffer, state, true)));
                    }
                    None => {
                        let mut transformed = drain_anthropic_sse_buffer(&mut buffer, &mut state);
                        if !state.done {
                            if state.include_usage {
                                transformed.push_str(&openai_sse_json(&state.usage_chunk()));
                            }
                            transformed.push_str("data: [DONE]\n\n");
                            state.done = true;
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

/// POST /v1/chat/completions —— OpenAI 兼容。
pub async fn post_chat_completions(
    State(state): State<AppState>,
    Json(oai): Json<Value>,
) -> Response {
    let aws_b40_compat = state.aws_b40_compat;
    let model = oai
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-opus-4-8")
        .to_string();
    let max_tokens = openai_max_tokens(&oai);
    let stream_requested = oai.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = oai
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // OpenAI messages → Anthropic system + messages。
    let mut system: Vec<SystemMessage> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    if let Some(arr) = oai.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
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
            } else if role == "assistant" && m.get("tool_calls").and_then(Value::as_array).is_some()
            {
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
    }
    if messages.is_empty() {
        messages.push(Message {
            role: "user".to_string(),
            content: Value::String("Hello".to_string()),
        });
    }

    let tools_disabled = oai.get("tool_choice").and_then(Value::as_str) == Some("none");
    let tools = if tools_disabled {
        None
    } else {
        openai_tools_to_anthropic(&oai)
    };
    let tool_choice = openai_tool_choice_to_anthropic(oai.get("tool_choice"));

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
        return openai_stream_response(
            resp.into_body(),
            model,
            created,
            include_usage,
            aws_b40_compat,
        );
    }

    let body_bytes = match axum::body::to_bytes(resp.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };

    if !status.is_success() {
        // 错误原样透传(保持 Anthropic 错误体)。
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            body_bytes,
        )
            .into_response();
    }

    let anthropic: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let openai = anthropic_to_openai_chat(&anthropic, &model, created, aws_b40_compat);
    (StatusCode::OK, Json(openai)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tools = openai_tools_to_anthropic(&request).expect("mapped tools");
        assert_eq!(tools[0].name, "calculator");
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(
            openai_tool_choice_to_anthropic(Some(&json!("required"))),
            Some(json!({"type": "any"}))
        );
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
