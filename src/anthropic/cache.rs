//! Cache usage 显示策略
//!
//! Kiro 上游不支持 prompt caching，但客户端可能用 Anthropic prompt caching SDK
//! 并期待响应里看到 cache 字段反馈。本模块提供两种显示策略：
//!
//! 1. **客户传了 cache_control** → 把上游真实 input_tokens 按 Anthropic 官方
//!    cache 字段拆分成 (input, cache_read, cache_creation)。高缓存分支按
//!    输入规模渐进展示 cache：短请求不造缓存，中等上下文少量 read，大上下文
//!    才展示较高 read。
//!
//! 2. **客户没传 cache_control** → 老实返回 `input=T, cache_read=0,
//!    cache_creation=0`，避免"凭空冒出 cache"的客户投诉。
//!
//! ## 渐进式高缓存拆分
//!
//! - `T < 4k`：全部显示为普通 input，避免短请求凭空出现 cache。
//! - `4k <= T < 20k`：保留 15% creation，read 从 0% 平滑涨到 30%。
//! - `20k <= T < 50k`：creation 从 15% 平滑降到 13%，read 从 30% 平滑涨到 70%。
//! - `T >= 50k`：creation 从 13% 平滑降到 12%，read 从 70% 平滑涨到 80%。
//! - 始终满足 `input + cache_read + cache_creation = T`。
//!
//! ## 取代 sub2api virtual_cache 的理由
//!
//! sub2api 的 `applyVirtualCacheToUsageJSON` 在所有上游空 cache 时都注入，
//! 客户没传 cache_control 也会看到莫名 cache 数字。把策略移到 kiro-rs 后，
//! 由 kiro-rs 根据客户请求意图主动决定显示，sub2api 把对应账号
//! `virtual_cache_enabled` 关掉即可全程透传。

use crate::anthropic::types::{Message, MessagesRequest};
use serde_json::Value;

/// 小于这个规模的请求不展示虚拟缓存，避免短请求看起来明显不真实。
const CACHE_DISPLAY_MIN_TOKENS: i32 = 4_000;

/// 从这个规模开始展示高缓存读取。
const HIGH_CACHE_MIN_TOKENS: i32 = 20_000;

/// 从这个规模开始展示强缓存读取。
const HIGH_CACHE_STRONG_TOKENS: i32 = 50_000;

/// 高缓存读取比例在这个规模后达到上限。
const HIGH_CACHE_FULL_RAMP_TOKENS: i32 = 100_000;

/// 中等上下文保留的 cache_creation 比例。
const MID_CACHE_CREATION_RATIO: f64 = 0.15;

/// 大上下文最终保留的 cache_creation 比例。
const HIGH_CACHE_CREATION_RATIO: f64 = 0.12;

/// 强缓存起始保留的 cache_creation 比例。
const HIGH_CACHE_STRONG_CREATION_RATIO: f64 = 0.13;

/// 中等上下文最高展示的 cache_read 命中比例。
const MID_MAX_READ_HIT_RATIO: f64 = 0.30;

/// 大上下文起始展示的 cache_read 命中比例。
const HIGH_MIN_READ_HIT_RATIO: f64 = 0.70;

/// 大上下文最终展示的 cache_read 命中比例。
const HIGH_MAX_READ_HIT_RATIO: f64 = 0.80;

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
    if total_input_tokens <= 0 || total_input_tokens < CACHE_DISPLAY_MIN_TOKENS {
        return UsageBreakdown::flat(total_input_tokens.max(0));
    }

    let (creation_ratio, read_hit_ratio) = cache_display_ratios(total_input_tokens);

    let cc = ((total_input_tokens as f64) * creation_ratio).floor() as i32;
    let remaining = total_input_tokens - cc;
    let cr = ((remaining as f64) * read_hit_ratio).floor() as i32;
    let i = total_input_tokens - cr - cc;

    UsageBreakdown {
        input_tokens: i,
        cache_read_input_tokens: cr,
        cache_creation_input_tokens: cc,
    }
}

fn cache_display_ratios(total_input_tokens: i32) -> (f64, f64) {
    if total_input_tokens < HIGH_CACHE_MIN_TOKENS {
        let progress = progress_between(
            total_input_tokens,
            CACHE_DISPLAY_MIN_TOKENS,
            HIGH_CACHE_MIN_TOKENS,
        );
        return (MID_CACHE_CREATION_RATIO, MID_MAX_READ_HIT_RATIO * progress);
    }

    if total_input_tokens < HIGH_CACHE_STRONG_TOKENS {
        let progress = progress_between(
            total_input_tokens,
            HIGH_CACHE_MIN_TOKENS,
            HIGH_CACHE_STRONG_TOKENS,
        );
        let creation_ratio = MID_CACHE_CREATION_RATIO
            + (HIGH_CACHE_STRONG_CREATION_RATIO - MID_CACHE_CREATION_RATIO) * progress;
        let read_hit_ratio =
            MID_MAX_READ_HIT_RATIO + (HIGH_MIN_READ_HIT_RATIO - MID_MAX_READ_HIT_RATIO) * progress;
        return (creation_ratio, read_hit_ratio);
    }

    let progress = progress_between(
        total_input_tokens,
        HIGH_CACHE_STRONG_TOKENS,
        HIGH_CACHE_FULL_RAMP_TOKENS,
    );
    let creation_ratio = HIGH_CACHE_STRONG_CREATION_RATIO
        + (HIGH_CACHE_CREATION_RATIO - HIGH_CACHE_STRONG_CREATION_RATIO) * progress;
    let read_hit_ratio =
        HIGH_MIN_READ_HIT_RATIO + (HIGH_MAX_READ_HIT_RATIO - HIGH_MIN_READ_HIT_RATIO) * progress;
    (creation_ratio, read_hit_ratio)
}

fn progress_between(value: i32, start: i32, end: i32) -> f64 {
    if end <= start {
        return 1.0;
    }
    ((value - start) as f64 / (end - start) as f64).clamp(0.0, 1.0)
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
    fn large_context_without_cache_control_never_injects_cache() {
        let b = compute_usage_breakdown(100_000, false);
        assert_eq!(b, UsageBreakdown::flat(100_000));
    }

    #[test]
    fn short_request_with_cache_control_stays_flat() {
        let b = compute_usage_breakdown(2990, true);
        assert_eq!(b, UsageBreakdown::flat(2990));
    }

    #[test]
    fn cache_control_at_display_threshold_shows_creation_without_read() {
        let b = compute_usage_breakdown(4000, true);
        assert_eq!(b.cache_creation_input_tokens, 600);
        assert_eq!(b.cache_read_input_tokens, 0);
        assert_eq!(b.input_tokens, 3400);
    }

    #[test]
    fn medium_context_shows_limited_cache_read() {
        let b = compute_usage_breakdown(12_000, true);
        // progress = (12000 - 4000) / (20000 - 4000) = 0.5
        // read hit ratio = 30% * 0.5 = 15%
        // CC = floor(12000 * 0.15) = 1800
        // CR = floor((12000 - 1800) * 0.15) = 1530
        assert_eq!(b.cache_creation_input_tokens, 1800);
        assert_eq!(b.cache_read_input_tokens, 1530);
        assert_eq!(b.input_tokens, 8670);
    }

    #[test]
    fn large_context_starts_high_cache_read() {
        let b = compute_usage_breakdown(20_000, true);
        // 20k 是高缓存平滑过渡起点：15% creation，剩余部分 30% read。
        assert_eq!(b.cache_creation_input_tokens, 3000);
        assert_eq!(b.cache_read_input_tokens, 5100);
        assert_eq!(b.input_tokens, 11900);
    }

    #[test]
    fn strong_context_reaches_high_cache_read() {
        let b = compute_usage_breakdown(50_000, true);
        // 50k 达到强缓存起点：13% creation，剩余部分 70% read。
        assert_eq!(b.cache_creation_input_tokens, 6500);
        assert_eq!(b.cache_read_input_tokens, 30449);
        assert_eq!(b.input_tokens, 13051);
    }

    #[test]
    fn very_large_context_caps_cache_read_and_reduces_creation() {
        let b = compute_usage_breakdown(100_000, true);
        // 100k 后达到上限：12% creation，剩余部分 80% read。
        assert_eq!(b.cache_creation_input_tokens, 12_000);
        assert_eq!(b.cache_read_input_tokens, 70_400);
        assert_eq!(b.input_tokens, 17_600);
    }

    #[test]
    fn split_preserves_token_count_identity() {
        for t in [10, 100, 1000, 2990, 4000, 12_000, 20_000, 50_000, 200_000] {
            let b = split_virtual_cache(t);
            assert_eq!(b.total(), t, "token 数恒等失败 T={}", t);
        }
    }

    #[test]
    fn split_uses_progressive_read_hit_rates() {
        assert_eq!(read_hit_rate(split_virtual_cache(3999)), 0.0);
        assert_eq!(read_hit_rate(split_virtual_cache(4000)), 0.0);
        assert!((read_hit_rate(split_virtual_cache(12_000)) - 0.15).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(20_000)) - 0.30).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(50_000)) - 0.70).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(100_000)) - 0.80).abs() <= 0.01);
    }

    #[test]
    fn split_zero_or_negative_returns_zero() {
        assert_eq!(split_virtual_cache(0), UsageBreakdown::flat(0));
        assert_eq!(split_virtual_cache(-5), UsageBreakdown::flat(0));
    }

    #[test]
    fn split_reduces_creation_for_very_large_context() {
        assert_eq!(
            split_virtual_cache(20_000).cache_creation_input_tokens,
            3000
        );
        assert_eq!(
            split_virtual_cache(50_000).cache_creation_input_tokens,
            6500
        );
        assert_eq!(
            split_virtual_cache(100_000).cache_creation_input_tokens,
            12_000
        );
    }

    fn read_hit_rate(b: UsageBreakdown) -> f64 {
        let read_or_input = b.cache_read_input_tokens + b.input_tokens;
        if read_or_input == 0 {
            return 0.0;
        }
        b.cache_read_input_tokens as f64 / read_or_input as f64
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
