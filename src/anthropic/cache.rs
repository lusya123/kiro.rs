//! Cache usage 显示策略
//!
//! AWS-P 口径不维护本地 prompt-cache 命中状态，而是在客户请求包含
//! `cache_control` 时，把真实 input_tokens 按 Anthropic cache 字段做虚拟拆分。
//! 没有 `cache_control` 的请求始终平铺为普通 input。
//!
//! 渐进式拆分规则：
//! - `T < 4k`：全部显示为普通 input。
//! - `4k <= T < 20k`：保留 15% creation，read 从 10% 平滑涨到 45%。
//! - `20k <= T < 50k`：creation 从 15% 平滑降到 13%，read 从 45% 平滑涨到 80%。
//! - `T >= 50k`：creation 从 13% 平滑降到 10%，read 从 80% 平滑涨到 90%。
//! - 始终满足 `input + cache_read + cache_creation = T`。

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
const HIGH_CACHE_CREATION_RATIO: f64 = 0.10;

/// 强缓存起始保留的 cache_creation 比例。
const HIGH_CACHE_STRONG_CREATION_RATIO: f64 = 0.13;

/// 中等上下文起始展示的 cache_read 命中比例。
const MID_MIN_READ_HIT_RATIO: f64 = 0.10;

/// 中等上下文最高展示的 cache_read 命中比例。
const MID_MAX_READ_HIT_RATIO: f64 = 0.45;

/// 大上下文起始展示的 cache_read 命中比例。
const HIGH_MIN_READ_HIT_RATIO: f64 = 0.80;

/// 大上下文最终展示的 cache_read 命中比例。
const HIGH_MAX_READ_HIT_RATIO: f64 = 0.90;

/// Usage 拆分结果（满足 token 数恒等）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageBreakdown {
    pub input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
}

impl UsageBreakdown {
    /// 平凡情况：所有 token 算作普通 input，cache 字段为 0。
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
pub fn split_virtual_cache(total_input_tokens: i32) -> UsageBreakdown {
    if total_input_tokens <= 0 || total_input_tokens < CACHE_DISPLAY_MIN_TOKENS {
        return UsageBreakdown::flat(total_input_tokens.max(0));
    }

    let (creation_ratio, read_hit_ratio) = cache_display_ratios(total_input_tokens);

    let cc = ((total_input_tokens as f64) * creation_ratio).floor() as i32;
    let remaining = total_input_tokens - cc;
    let cr = ((remaining as f64) * read_hit_ratio).floor() as i32;
    let input = total_input_tokens - cr - cc;

    UsageBreakdown {
        input_tokens: input,
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
        let read_hit_ratio =
            MID_MIN_READ_HIT_RATIO + (MID_MAX_READ_HIT_RATIO - MID_MIN_READ_HIT_RATIO) * progress;
        return (MID_CACHE_CREATION_RATIO, read_hit_ratio);
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
pub fn request_has_cache_control(req: &MessagesRequest) -> bool {
    request_cache_control_count(req) > 0
}

/// 统计请求里出现的 `cache_control` 个数。
///
/// 当前 handlers 仍用这个函数保留 Anthropic 最多 4 个 cache breakpoint 的校验。
pub fn request_cache_control_count(req: &MessagesRequest) -> usize {
    let mut count = 0;

    if let Some(system) = &req.system {
        for s in system {
            count += cache_control_count_in_value(&serde_json::to_value(s).unwrap_or(Value::Null));
        }
    }

    for msg in &req.messages {
        count += message_cache_control_count(msg);
    }

    if let Some(tools) = &req.tools {
        for tool in tools {
            count +=
                cache_control_count_in_value(&serde_json::to_value(tool).unwrap_or(Value::Null));
        }
    }

    count
}

fn message_cache_control_count(msg: &Message) -> usize {
    match &msg.content {
        Value::String(_) => 0,
        Value::Array(arr) => arr.iter().map(cache_control_count_in_value).sum(),
        v => cache_control_count_in_value(v),
    }
}

fn cache_control_count_in_value(v: &Value) -> usize {
    match v {
        Value::Object(map) => {
            let current = usize::from(map.contains_key("cache_control"));
            current
                + map
                    .values()
                    .map(cache_control_count_in_value)
                    .sum::<usize>()
        }
        Value::Array(arr) => arr.iter().map(cache_control_count_in_value).sum(),
        _ => 0,
    }
}

/// 根据请求意图决定 usage 字段的最终形态。
pub fn compute_usage_breakdown(total_input_tokens: i32, has_cache_control: bool) -> UsageBreakdown {
    if has_cache_control {
        split_virtual_cache(total_input_tokens)
    } else {
        UsageBreakdown::flat(total_input_tokens)
    }
}

/// 当前分支保留的请求级入口；AWS-P 口径不提交/读取状态，只看请求是否含 cache_control。
pub fn compute_usage_breakdown_for_request(
    total_input_tokens: i32,
    req: &MessagesRequest,
) -> UsageBreakdown {
    compute_usage_breakdown(total_input_tokens, request_has_cache_control(req))
}

/// 当前分支保留的预览入口；AWS-P 口径没有状态副作用，因此与正式计算一致。
pub fn preview_usage_breakdown_for_request(
    total_input_tokens: i32,
    req: &MessagesRequest,
) -> UsageBreakdown {
    compute_usage_breakdown_for_request(total_input_tokens, req)
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
    fn cache_control_at_display_threshold_shows_initial_read() {
        let b = compute_usage_breakdown(4000, true);
        assert_eq!(b.cache_creation_input_tokens, 600);
        assert_eq!(b.cache_read_input_tokens, 340);
        assert_eq!(b.input_tokens, 3060);
    }

    #[test]
    fn medium_context_shows_limited_cache_read() {
        let b = compute_usage_breakdown(12_000, true);
        assert_eq!(b.cache_creation_input_tokens, 1800);
        assert_eq!(b.cache_read_input_tokens, 2805);
        assert_eq!(b.input_tokens, 7395);
    }

    #[test]
    fn large_context_starts_high_cache_read() {
        let b = compute_usage_breakdown(20_000, true);
        assert_eq!(b.cache_creation_input_tokens, 3000);
        assert_eq!(b.cache_read_input_tokens, 7650);
        assert_eq!(b.input_tokens, 9350);
    }

    #[test]
    fn strong_context_reaches_high_cache_read() {
        let b = compute_usage_breakdown(50_000, true);
        assert_eq!(b.cache_creation_input_tokens, 6500);
        assert_eq!(b.cache_read_input_tokens, 34800);
        assert_eq!(b.input_tokens, 8700);
    }

    #[test]
    fn very_large_context_caps_cache_read_and_reduces_creation() {
        let b = compute_usage_breakdown(100_000, true);
        assert_eq!(b.cache_creation_input_tokens, 10_000);
        assert_eq!(b.cache_read_input_tokens, 81_000);
        assert_eq!(b.input_tokens, 9000);
    }

    #[test]
    fn split_preserves_token_count_identity() {
        for t in [10, 100, 1000, 2990, 4000, 12_000, 20_000, 50_000, 200_000] {
            let b = split_virtual_cache(t);
            assert_eq!(b.total(), t, "token 数恒等失败 T={}", t);
        }
    }

    #[test]
    fn request_level_entry_uses_virtual_cache_without_state() {
        let req = parse_request(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "long context", "cache_control": {"type": "ephemeral"}}
                ]
            }]
        }));

        let first = compute_usage_breakdown_for_request(12_000, &req);
        let second = compute_usage_breakdown_for_request(12_000, &req);

        assert_eq!(first, compute_usage_breakdown(12_000, true));
        assert_eq!(second, first);
    }

    #[test]
    fn split_zero_or_negative_returns_zero() {
        assert_eq!(split_virtual_cache(0), UsageBreakdown::flat(0));
        assert_eq!(split_virtual_cache(-5), UsageBreakdown::flat(0));
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
        assert_eq!(request_cache_control_count(&req), 1);
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
        assert_eq!(request_cache_control_count(&req), 1);
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
        assert_eq!(request_cache_control_count(&req), 1);
    }

    #[test]
    fn no_cache_control_returns_false() {
        let req = parse_request(serde_json::json!({
            "messages": [{"role": "user", "content": "plain question"}]
        }));
        assert!(!request_has_cache_control(&req));
        assert_eq!(request_cache_control_count(&req), 0);
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
}
