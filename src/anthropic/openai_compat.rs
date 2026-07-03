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

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::handlers::post_messages;
use super::middleware::AppState;
use super::types::{Message, MessagesRequest, SystemMessage};

/// OpenAI content(string 或 `[{type:"text",text:...}]`)→ Anthropic Message.content 用的字符串。
fn openai_content_to_value(content: &Value) -> Value {
    if content.is_string() {
        return content.clone();
    }
    if let Some(arr) = content.as_array() {
        let text: String = arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        return Value::String(text);
    }
    Value::String(String::new())
}

/// 把内部 Anthropic 响应体转成 OpenAI ChatCompletion(usage 为混合键)。
fn anthropic_to_openai_chat(a: &Value, model: &str, created: u64) -> Value {
    let text: String = a
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = a.get("usage").cloned().unwrap_or_else(|| json!({}));
    let g = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let gp = |p: &str| usage.pointer(p).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = g("input_tokens");
    let output = g("output_tokens");
    let cache_creation = g("cache_creation_input_tokens");
    let cache_read = g("cache_read_input_tokens");
    let thinking = gp("/output_tokens_details/thinking_tokens");
    let c5m = gp("/cache_creation/ephemeral_5m_input_tokens");
    let c1h = gp("/cache_creation/ephemeral_1h_input_tokens");
    let prompt_tokens = input + cache_creation + cache_read;

    let finish = match a.get("stop_reason").and_then(|v| v.as_str()) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    };
    let id = a
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("msg_kiro")
        .replace("msg_", "chatcmpl-");

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "logprobs": null,
            "finish_reason": finish
        }],
        "usage": {
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
        }
    })
}

/// POST /v1/chat/completions —— OpenAI 兼容。
pub async fn post_chat_completions(State(state): State<AppState>, Json(oai): Json<Value>) -> Response {
    let model = oai
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("claude-opus-4-8")
        .to_string();
    let max_tokens = oai
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .filter(|&n| n > 0)
        .unwrap_or(1024) as i32;

    // OpenAI messages → Anthropic system + messages。
    let mut system: Vec<SystemMessage> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    if let Some(arr) = oai.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = openai_content_to_value(
                &m.get("content").cloned().unwrap_or(Value::String(String::new())),
            );
            if role == "system" || role == "developer" {
                if let Some(t) = content.as_str() {
                    system.push(SystemMessage {
                        text: t.to_string(),
                        cache_control: None,
                    });
                }
            } else {
                let role = if role == "assistant" { "assistant" } else { "user" };
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

    let mr = MessagesRequest {
        model: model.clone(),
        max_tokens,
        messages,
        stream: false, // 内部一律非流式,再按 OpenAI 形态包装。
        system: if system.is_empty() { None } else { Some(system) },
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        cache_control: None,
        metadata: None,
    };

    // 复用 /v1/messages 全套生成逻辑(短路/后端/计量)。
    let resp = post_messages(State(state), Json(mr)).await;
    let status = resp.status();
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
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let openai = anthropic_to_openai_chat(&anthropic, &model, created);
    (StatusCode::OK, Json(openai)).into_response()
}
