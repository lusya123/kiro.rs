//! Cache usage 显示策略
//!
//! Kiro 上游不支持 prompt caching，但客户端可能用 Anthropic prompt caching SDK
//! 并期待响应里看到 cache 字段反馈。本模块提供两种显示策略：
//!
//! 1. **客户传了 cache_control** → 把上游真实 input_tokens 按 Anthropic 官方
//!    cache 字段拆分成 (input, cache_read, cache_creation)，让客户感觉
//!    "命中了缓存"。高缓存分支固定保留 15% cache_creation，其余部分按
//!    90% cache_read / 10% input 拆分。
//!
//! 2. **客户没传 cache_control** → 老实返回 `input=T, cache_read=0,
//!    cache_creation=0`，避免"凭空冒出 cache"的客户投诉。
//!
//! ## 高缓存拆分
//!
//! 给定上游真实 `T` tokens：
//!
//! ```text
//! CC = ⌊T × 0.15⌋
//! remaining = T - CC
//! CR = ⌊remaining × 0.90⌋
//! I  = T - CR - CC
//! ```
//!
//! 同时满足 `I + CR + CC = T`。按 `cache_read / (input + cache_read)` 口径，
//! 用户看到的缓存命中率约为 90%。
//!
//! ## 取代 sub2api virtual_cache 的理由
//!
//! sub2api 的 `applyVirtualCacheToUsageJSON` 在所有上游空 cache 时都注入，
//! 客户没传 cache_control 也会看到莫名 cache 数字。把策略移到 kiro-rs 后，
//! 由 kiro-rs 根据客户请求意图主动决定显示，sub2api 把对应账号
//! `virtual_cache_enabled` 关掉即可全程透传。

use crate::anthropic::types::{Message, MessagesRequest};
use serde_json::Value;

/// 高缓存分支保留的 cache_creation 占真实输入 token 比例。
const HIGH_CACHE_CREATION_RATIO: f64 = 0.15;

/// cache_creation 之外的部分，按 90% 展示为 cache_read。
const HIGH_CACHE_READ_HIT_RATIO: f64 = 0.90;

/// Usage 拆分结果（满足 token 数恒等）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageBreakdown {
    pub input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
}

impl UsageBreakdown {
    /// 平凡情况：所有 token 算作普通 input，cache 字段为 0
    pub fn flat(input_tokens: i32) -> Self {
        Self {
            input_tokens,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    /// total = input + cache_read + cache_creation
    #[cfg(test)]
    pub fn total(&self) -> i32 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

/// 把真实总 token `total_input_tokens` 拆分成高缓存展示口径 (I, CR, CC)。
///
/// `total_input_tokens <= 0` 直接返回全 0。
pub fn split_virtual_cache(total_input_tokens: i32) -> UsageBreakdown {
    if total_input_tokens <= 0 {
        return UsageBreakdown::flat(0);
    }

    let t = total_input_tokens as f64;

    let cc = (t * HIGH_CACHE_CREATION_RATIO).floor() as i32;
    let remaining = total_input_tokens - cc;
    let cr = ((remaining as f64) * HIGH_CACHE_READ_HIT_RATIO).floor() as i32;
    let i = total_input_tokens - cr - cc;

    UsageBreakdown {
        input_tokens: i,
        cache_read_input_tokens: cr,
        cache_creation_input_tokens: cc,
    }
}

/// 检查请求里有没有 `cache_control` 字段。
///
/// Anthropic 协议中 `cache_control` 可以出现在：
/// - `system[*].cache_control`
/// - `messages[*].content[*].cache_control`（content 是数组形态）
/// - `tools[*].cache_control`
///
/// 任何一处出现都视为"客户开启了 prompt caching"。
pub fn request_has_cache_control(req: &MessagesRequest) -> bool {
    // system blocks
    if let Some(system) = &req.system {
        for s in system {
            if has_cache_control_in_value(&serde_json::to_value(s).unwrap_or(Value::Null)) {
                return true;
            }
        }
    }

    // messages
    for msg in &req.messages {
        if message_has_cache_control(msg) {
            return true;
        }
    }

    // tools
    if let Some(tools) = &req.tools {
        for tool in tools {
            if has_cache_control_in_value(&serde_json::to_value(tool).unwrap_or(Value::Null)) {
                return true;
            }
        }
    }

    false
}

fn message_has_cache_control(msg: &Message) -> bool {
    match &msg.content {
        Value::String(_) => false,
        Value::Array(arr) => arr.iter().any(has_cache_control_in_value),
        v => has_cache_control_in_value(v),
    }
}

fn has_cache_control_in_value(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            if map.contains_key("cache_control") {
                return true;
            }
            map.values().any(has_cache_control_in_value)
        }
        Value::Array(arr) => arr.iter().any(has_cache_control_in_value),
        _ => false,
    }
}

/// 根据请求意图决定 usage 字段的最终形态。
///
/// - `has_cache_control = true` → 拆分成 (I, CR, CC) 让客户感受 cache 命中
/// - `has_cache_control = false` → 老实返回 (T, 0, 0)
pub fn compute_usage_breakdown(total_input_tokens: i32, has_cache_control: bool) -> UsageBreakdown {
    if has_cache_control {
        split_virtual_cache(total_input_tokens)
    } else {
        UsageBreakdown::flat(total_input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_when_no_cache_control() {
        let b = compute_usage_breakdown(2990, false);
        assert_eq!(b.input_tokens, 2990);
        assert_eq!(b.cache_read_input_tokens, 0);
        assert_eq!(b.cache_creation_input_tokens, 0);
    }

    #[test]
    fn split_when_has_cache_control() {
        let b = compute_usage_breakdown(2990, true);
        // CC = floor(2990 × 0.15) = 448
        // remaining = 2542
        // CR = floor(2542 × 0.90) = 2287
        // I = 2990 - 448 - 2287 = 255
        assert_eq!(b.cache_read_input_tokens, 2287);
        assert_eq!(b.cache_creation_input_tokens, 448);
        assert_eq!(b.input_tokens, 255);
    }

    #[test]
    fn split_preserves_token_count_identity() {
        for t in [10, 100, 1000, 2990, 50_000, 200_000] {
            let b = split_virtual_cache(t);
            assert_eq!(b.total(), t, "token 数恒等失败 T={}", t);
        }
    }

    #[test]
    fn split_shows_roughly_ninety_percent_read_hit_rate() {
        for t in [1000, 2990, 50_000, 200_000] {
            let b = split_virtual_cache(t);
            let read_or_input = b.cache_read_input_tokens + b.input_tokens;
            let hit_rate = b.cache_read_input_tokens as f64 / read_or_input as f64;
            assert!(
                (hit_rate - 0.90).abs() <= 0.01,
                "命中率应接近 90% T={} hit_rate={}",
                t,
                hit_rate
            );
        }
    }

    #[test]
    fn split_zero_or_negative_returns_zero() {
        assert_eq!(split_virtual_cache(0), UsageBreakdown::flat(0));
        assert_eq!(split_virtual_cache(-5), UsageBreakdown::flat(0));
    }

    #[test]
    fn split_keeps_creation_at_fifteen_percent() {
        let b = split_virtual_cache(1000);
        assert_eq!(b.cache_creation_input_tokens, 150);
        assert_eq!(b.cache_read_input_tokens, 765);
        assert_eq!(b.input_tokens, 85);
        assert_eq!(b.total(), 1000);
    }

    fn parse_request(extra: serde_json::Value) -> MessagesRequest {
        let mut body = serde_json::json!({
            "model": "claude-opus-4-7",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}]
        });
        if let serde_json::Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                body[k] = v;
            }
        }
        serde_json::from_value(body).expect("valid")
    }

    #[test]
    fn detects_cache_control_in_system() {
        let req = parse_request(serde_json::json!({
            "system": [{
                "type": "text",
                "text": "...",
                "cache_control": {"type": "ephemeral"}
            }]
        }));
        assert!(request_has_cache_control(&req));
    }

    #[test]
    fn detects_cache_control_in_message_content() {
        let req = parse_request(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "long context", "cache_control": {"type": "ephemeral"}}
                ]
            }]
        }));
        assert!(request_has_cache_control(&req));
    }

    #[test]
    fn detects_cache_control_in_tools() {
        let req = parse_request(serde_json::json!({
            "tools": [{
                "name": "calculator",
                "description": "math",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }]
        }));
        assert!(request_has_cache_control(&req));
    }

    #[test]
    fn no_cache_control_returns_false() {
        let req = parse_request(serde_json::json!({
            "messages": [{"role": "user", "content": "plain question"}]
        }));
        assert!(!request_has_cache_control(&req));
    }

    #[test]
    fn customer_xueding_request_has_cache_control() {
        // 客户 sk-cde0... 的真实请求含 system.cache_control={"type":"ephemeral"}
        let req = parse_request(serde_json::json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "max"},
            "system": [{
                "type": "text",
                "text": "You are OpenCode",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert!(
            request_has_cache_control(&req),
            "客户原始请求 system 含 cache_control 必须被识别"
        );
    }
}
