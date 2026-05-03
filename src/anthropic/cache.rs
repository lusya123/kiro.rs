//! Cache usage 显示策略
//!
//! Kiro 上游不支持 prompt caching，但客户端可能用 Anthropic prompt caching SDK
//! 并期待响应里看到 cache 字段反馈。本模块提供两种显示策略：
//!
//! 1. **客户传了 cache_control** → 把上游真实 input_tokens 按 Anthropic 官方
//!    cache 价格比例拆分成 (input, cache_read, cache_creation)，让客户感觉
//!    "命中了缓存"。**总计费成本不变**（数学恒等式保证）。
//!
//! 2. **客户没传 cache_control** → 老实返回 `input=T, cache_read=0,
//!    cache_creation=0`，避免"凭空冒出 cache"的客户投诉。
//!
//! ## 数学恒等
//!
//! 给定上游真实 `T` tokens 与拆分比 `α`：
//!
//! ```text
//! CR = ⌊T × α⌋
//! CC = ⌊3.6 × CR⌋          (Anthropic 价格比 1.25 / 0.1 × cancel = 3.6)
//! I  = T - CR - CC
//! ```
//!
//! 同时满足：
//! - **token 数恒等**：`I + CR + CC = T`
//! - **成本恒等**（按 input 单价归一化）：
//!   `I + 0.1·CR + 1.25·CC = T`
//!
//! ## 取代 sub2api virtual_cache 的理由
//!
//! sub2api 的 `applyVirtualCacheToUsageJSON` 在所有上游空 cache 时都注入，
//! 客户没传 cache_control 也会看到莫名 cache 数字。把策略移到 kiro-rs 后，
//! 由 kiro-rs 根据客户请求意图主动决定显示，sub2api 把对应账号
//! `virtual_cache_enabled` 关掉即可全程透传。

use crate::anthropic::types::{Message, MessagesRequest};
use serde_json::Value;

/// CC / CR 比例（Anthropic 官方价格 1.25× / 0.1× → 比值固定 3.6）
const CACHE_CREATION_TO_READ_RATIO: f64 = 3.6;

/// I 不为负的最大 readRatio：1 / (1 + 3.6) ≈ 0.2174
const MAX_SAFE_READ_RATIO: f64 = 1.0 / (1.0 + CACHE_CREATION_TO_READ_RATIO);

/// 默认 readRatio（与 sub2api virtual_cache 默认值一致）
const DEFAULT_READ_RATIO: f64 = 0.15;

/// Usage 拆分结果（满足 token 数恒等 + 成本恒等）
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

/// 把真实总 token `total_input_tokens` 按 `read_ratio` 拆分成 (I, CR, CC)。
///
/// `read_ratio` 自动 clamp 到 `[0, MAX_SAFE_READ_RATIO]`，防止 I 为负。
/// `total_input_tokens <= 0` 直接返回全 0。
pub fn split_virtual_cache(total_input_tokens: i32, read_ratio: f64) -> UsageBreakdown {
    if total_input_tokens <= 0 {
        return UsageBreakdown::flat(0);
    }
    if read_ratio <= 0.0 {
        return UsageBreakdown::flat(total_input_tokens);
    }
    let alpha = read_ratio.min(MAX_SAFE_READ_RATIO);
    let t = total_input_tokens as f64;

    let cr = (t * alpha).floor() as i32;
    let cr = cr.max(0);
    let cc = (CACHE_CREATION_TO_READ_RATIO * cr as f64).floor() as i32;
    let cc = cc.max(0);
    let i = total_input_tokens - cr - cc;

    if i < 0 {
        // clamp 失败兜底：用 max ratio 重算
        let cr_fallback = (t / (1.0 + CACHE_CREATION_TO_READ_RATIO)) as i32;
        let cc_fallback = (CACHE_CREATION_TO_READ_RATIO * cr_fallback as f64).floor() as i32;
        let mut i_fallback = total_input_tokens - cr_fallback - cc_fallback;
        let mut cc_fallback = cc_fallback;
        if i_fallback < 0 {
            cc_fallback += i_fallback;
            i_fallback = 0;
        }
        return UsageBreakdown {
            input_tokens: i_fallback,
            cache_read_input_tokens: cr_fallback,
            cache_creation_input_tokens: cc_fallback,
        };
    }

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
        split_virtual_cache(total_input_tokens, DEFAULT_READ_RATIO)
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
        // CR = floor(2990 × 0.15) = 448
        // CC = floor(3.6 × 448) = 1612
        // I = 2990 - 448 - 1612 = 930
        assert_eq!(b.cache_read_input_tokens, 448);
        assert_eq!(b.cache_creation_input_tokens, 1612);
        assert_eq!(b.input_tokens, 930);
    }

    #[test]
    fn split_preserves_token_count_identity() {
        for t in [10, 100, 1000, 2990, 50_000, 200_000] {
            let b = split_virtual_cache(t, DEFAULT_READ_RATIO);
            assert_eq!(b.total(), t, "token 数恒等失败 T={}", t);
        }
    }

    #[test]
    fn split_preserves_cost_identity() {
        // I + 0.1·CR + 1.25·CC ≈ T（允许 ±2 token 整数舍入误差）
        for t in [10, 100, 1000, 2990, 50_000, 200_000] {
            let b = split_virtual_cache(t, DEFAULT_READ_RATIO);
            let cost = b.input_tokens as f64
                + 0.1 * b.cache_read_input_tokens as f64
                + 1.25 * b.cache_creation_input_tokens as f64;
            let diff = (cost - t as f64).abs();
            assert!(
                diff <= 2.0,
                "成本恒等偏差过大 T={} cost={} diff={}",
                t,
                cost,
                diff
            );
        }
    }

    #[test]
    fn split_zero_or_negative_returns_zero() {
        assert_eq!(split_virtual_cache(0, 0.15), UsageBreakdown::flat(0));
        assert_eq!(split_virtual_cache(-5, 0.15), UsageBreakdown::flat(0));
    }

    #[test]
    fn split_zero_ratio_returns_flat() {
        let b = split_virtual_cache(1000, 0.0);
        assert_eq!(b, UsageBreakdown::flat(1000));
    }

    #[test]
    fn split_clamps_excessive_ratio() {
        // 超过 max safe ratio 时自动 clamp
        let b = split_virtual_cache(1000, 0.5);
        assert!(b.input_tokens >= 0, "I 不能为负");
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
