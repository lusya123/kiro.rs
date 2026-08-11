//! Anthropic prompt-cache usage compatibility.
//!
//! The Kiro transport can carry native cache points, but the upstream event
//! stream does not expose the complete Anthropic cache contract consistently.
//! This module therefore has two deliberately separated accounting paths:
//!
//! - authoritative `metadataEvent.tokenUsage`, when present, supplies the
//!   ordinary/read/write totals; request breakpoints only split an exact write
//!   between the 5-minute and 1-hour buckets;
//! - otherwise a deterministic prefix registry provides the compatibility
//!   fallback, and becomes warm only after a successful upstream completion.
//!
//! Requests without public `cache_control` remain flat even if Kiro used an
//! internal cache. Cache-point wire objects are transport controls and never
//! count as language input. In every path, ordinary input plus cache read plus
//! cache creation preserves the aggregate input-token identity.

use crate::anthropic::types::{Message, MessagesRequest, Tool};
use crate::kiro::model::events::TokenUsage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const CACHE_MIN_TOKENS_OPUS_5: i32 = 512;
const CACHE_MIN_TOKENS_OPUS_48_SONNET_5_46_45: i32 = 1_024;
const CACHE_MIN_TOKENS_OPUS_47: i32 = 2_048;
const CACHE_MIN_TOKENS_OPUS_46_45_HAIKU_45: i32 = 4_096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const MAX_READ_CANDIDATES: usize = 20;
// 缓存登记表已迁移到 `crate::cluster_cache`(跨容器共享 + 本地回退)。

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExactCacheTtlSegment {
    end_tokens: i32,
    one_hour: bool,
}

/// Request-derived cache-prefix layout used to split an authoritative native
/// cache write into Anthropic's 5m and 1h creation buckets.
///
/// This plan is deliberately independent of the local hot/cold registry. A
/// hot local estimate has zero creation buckets and therefore cannot tell us
/// whether a native cache miss wrote a 5m or a 1h prefix. The request's ordered
/// breakpoints retain that information for both cold and hot invocations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ExactCacheTtlPlan {
    segments: [ExactCacheTtlSegment; MAX_CACHE_BREAKPOINTS],
    len: usize,
}

/// Validated native token totals plus the optional public cache-bucket split.
///
/// Kiro exposes `metadataEvent.tokenUsage` for several model families, but the
/// cache read/write fields are not equally reliable across those families.
/// The aggregate input/output counts remain useful for every model.  Keeping
/// the public cache split optional lets affected models retain the shared,
/// deterministic prefix registry without throwing away the native totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReconciledNativeUsage {
    pub(super) aggregate_input_tokens: i32,
    pub(super) output_tokens: i32,
    pub(super) public_cache_usage: Option<UsageBreakdown>,
}

impl ExactCacheTtlPlan {
    fn is_empty(self) -> bool {
        self.len == 0
    }

    fn split_cache_write(self, cache_read: i32, cache_write: i32) -> Option<(i32, i32)> {
        let cache_read = cache_read.max(0);
        let cache_write = cache_write.max(0);
        if cache_write == 0 {
            return Some((0, 0));
        }
        let segments = self.segments.get(..self.len)?;
        let plan_total = segments.last()?.end_tokens.max(0);
        let exact_cache_total = cache_read.saturating_add(cache_write);
        if plan_total <= 0 || exact_cache_total <= 0 {
            return None;
        }

        // Native metadata reports aggregate read/write counts, while the
        // request plan is measured by the local tokenizer. Scale breakpoint
        // endpoints onto the native cached-token total, then treat the write
        // as the suffix after the authoritative read prefix. This preserves
        // mixed-TTL requests when (for example) a 1h prefix is read and an
        // expired 5m suffix is recreated.
        let write_start = cache_read.min(exact_cache_total);
        let write_end = exact_cache_total;
        let mut previous_end = 0i32;
        let mut creation_5m = 0i32;
        let mut creation_1h = 0i32;

        for segment in segments {
            let scaled_end =
                scale_cache_endpoint(segment.end_tokens, plan_total, exact_cache_total)
                    .clamp(previous_end, exact_cache_total);
            let overlap_start = previous_end.max(write_start);
            let overlap_end = scaled_end.min(write_end);
            let overlap = overlap_end.saturating_sub(overlap_start);
            if segment.one_hour {
                creation_1h = creation_1h.saturating_add(overlap);
            } else {
                creation_5m = creation_5m.saturating_add(overlap);
            }
            previous_end = scaled_end;
        }

        let allocated = creation_5m.saturating_add(creation_1h);
        if allocated != cache_write {
            return None;
        }
        Some((creation_5m, creation_1h))
    }
}

fn scale_cache_endpoint(endpoint: i32, plan_total: i32, exact_total: i32) -> i32 {
    if endpoint <= 0 || plan_total <= 0 || exact_total <= 0 {
        return 0;
    }
    ((i64::from(endpoint) * i64::from(exact_total) + i64::from(plan_total) / 2)
        / i64::from(plan_total))
    .clamp(0, i64::from(exact_total)) as i32
}

#[cfg(test)]
std::thread_local! {
    static PREFIX_TOKENIZATION_CALLS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

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

/// 2026-07-25/27 事故中由旧窗口钳制造成的公开计费哨兵值。
const INCIDENT_SENTINEL_USAGE_TOKENS: i32 = 999_999;

/// Usage 拆分结果（满足 token 数恒等）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageBreakdown {
    pub input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub cache_creation_5m_input_tokens: i32,
    pub cache_creation_1h_input_tokens: i32,
}

impl UsageBreakdown {
    /// 平凡情况：所有 token 算作普通 input，cache 字段为 0
    pub fn flat(input_tokens: i32) -> Self {
        Self {
            input_tokens,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        }
    }

    /// total = input + cache_read + cache_creation
    pub fn total(&self) -> i32 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub fn has_cache_usage(&self) -> bool {
        self.cache_read_input_tokens > 0 || self.cache_creation_input_tokens > 0
    }

    /// Map a native runtime token breakdown onto the public Anthropic cache
    /// buckets.  This is the only path allowed to replace deterministic cache
    /// estimates: unlike `contextUsageEvent`, these fields explicitly identify
    /// uncached, cache-read and cache-write input.
    #[cfg(test)]
    pub fn from_exact_token_usage(initial: Self, exact: &TokenUsage) -> Option<Self> {
        Self::from_exact_token_usage_with_ttl_plan(initial, exact, ExactCacheTtlPlan::default())
    }

    pub(super) fn from_exact_token_usage_with_ttl_plan(
        initial: Self,
        exact: &TokenUsage,
        ttl_plan: ExactCacheTtlPlan,
    ) -> Option<Self> {
        if !exact.is_present() {
            return None;
        }
        let fields = [
            exact.uncached_input_tokens,
            exact.cache_read_input_tokens,
            exact.cache_write_input_tokens,
            exact.output_tokens,
            exact.total_tokens,
        ];
        if fields.iter().any(|value| *value < 0) {
            return None;
        }

        let total_input = exact.total_input_tokens();
        if total_input <= 0 {
            return None;
        }
        let input_and_output = total_input.saturating_add(exact.output_tokens);
        if exact.total_tokens > 0 && exact.total_tokens < input_and_output {
            return None;
        }

        // Kiro may use an internal runtime cache even when the Anthropic
        // caller did not opt into prompt caching. Preserve the authoritative
        // aggregate input count, but never expose or bill those internal
        // buckets unless this invocation actually carried a native cachePoint.
        // The initial split alone is insufficient here because a valid native
        // prefix can sit below the local fallback minimum.
        // Exact Kiro cache buckets are public only when the converter actually
        // emitted at least one native cachePoint for this invocation. A local
        // compatibility breakpoint can exist at an Anthropic block boundary
        // which Kiro cannot represent (for example, a non-terminal block in a
        // message); it must not authorize unrelated internal runtime caching.
        if ttl_plan.is_empty() {
            return Some(Self::flat(total_input));
        }

        let (creation_5m, creation_1h) = ttl_plan
            .split_cache_write(
                exact.cache_read_input_tokens,
                exact.cache_write_input_tokens,
            )
            .unwrap_or_else(|| {
                split_exact_cache_creation(
                    exact.cache_write_input_tokens,
                    initial.cache_creation_5m_input_tokens,
                    initial.cache_creation_1h_input_tokens,
                )
            });
        Some(Self {
            input_tokens: exact.ordinary_input_tokens(),
            cache_read_input_tokens: exact.cache_read_input_tokens,
            cache_creation_input_tokens: exact.cache_write_input_tokens,
            cache_creation_5m_input_tokens: creation_5m,
            cache_creation_1h_input_tokens: creation_1h,
        })
    }

    /// 把 usage 钳制到物理可能的范围内，供所有对外出口在发出前调用。
    ///
    /// 单个请求的 input + cache_read + cache_creation 不可能超过模型上下文窗口。
    /// 一旦超过，只可能来自多轮累计、上游异常重连的重复计量，或本地估算把二进制
    /// 内容（例如 tool_result 里内嵌的 base64 截图）当作文本计数造成的放大。
    /// 下游网关按 usage 逐 token 计费，放大值会直接变成客户账单，因此必须在出口
    /// 处兜底：宁可暂停这一次输入计费，也不可把一个已知不可能的值改写成看似
    /// 合法的窗口上限后继续向客户收费。
    ///
    /// 同时强制 `cache_creation == 5m + 1h`。二者失配时，下游会把 message_start
    /// 的分量和 message_delta 的总量混用（`compat::stream_delta_usage` 不带分量
    /// 子字段），得到远超真实值的缓存写入量。
    pub fn clamp_to_context_window(self, context_window_tokens: i32) -> Self {
        let limit = context_window_tokens.max(1);

        let mut cache_creation_5m = self.cache_creation_5m_input_tokens.max(0);
        let cache_creation_1h = self.cache_creation_1h_input_tokens.max(0);
        let mut cache_creation = self.cache_creation_input_tokens.max(0);
        if cache_creation_5m.saturating_add(cache_creation_1h) != cache_creation {
            if cache_creation_5m > 0 || cache_creation_1h > 0 {
                cache_creation = cache_creation_5m.saturating_add(cache_creation_1h);
            } else {
                cache_creation_5m = cache_creation;
            }
        }

        let cache_read = self.cache_read_input_tokens.max(0);
        let input = self.input_tokens.max(0);

        let total = input
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        let contains_incident_sentinel = [
            input,
            cache_read,
            cache_creation,
            cache_creation_5m,
            cache_creation_1h,
            total,
        ]
        .contains(&INCIDENT_SENTINEL_USAGE_TOKENS);
        if total <= limit && !contains_incident_sentinel {
            return Self {
                input_tokens: input,
                cache_read_input_tokens: cache_read,
                cache_creation_input_tokens: cache_creation,
                cache_creation_5m_input_tokens: cache_creation_5m,
                cache_creation_1h_input_tokens: cache_creation_1h,
            };
        }

        tracing::warn!(
            limit,
            original_input = self.input_tokens,
            original_cache_read = self.cache_read_input_tokens,
            original_cache_creation = self.cache_creation_input_tokens,
            contains_incident_sentinel,
            held_input = 1,
            "usage 超过模型上下文窗口或命中事故哨兵值，已暂停异常输入计费"
        );

        Self::flat(1)
    }

    /// 按模型自身的上下文窗口钳制（opus-4.8 / sonnet-4.6 等为 1M，其余 200K）。
    pub fn clamp_for_model(self, model: &str) -> Self {
        self.clamp_to_context_window(super::converter::get_context_window_size(model))
    }
}

/// Reconcile a native Kiro usage frame without allowing an unstable model's
/// cache buckets to overwrite the shared compatibility cache.
///
/// Production billing evidence on 2026-08-09 showed identical Sonnet 5
/// requests repeatedly reporting the same 88k-89k cache write and zero reads,
/// while Opus 4.7/4.8/5 correctly transitioned from write to read.  Therefore
/// only those verified Opus families may replace the public cache split.  All
/// other models still use the validated native aggregate totals, but keep the
/// deterministic cache plan computed before the request.
pub(super) fn reconcile_native_usage(
    model: &str,
    initial: UsageBreakdown,
    exact: &TokenUsage,
    ttl_plan: ExactCacheTtlPlan,
) -> Option<ReconciledNativeUsage> {
    let mut exact_usage =
        UsageBreakdown::from_exact_token_usage_with_ttl_plan(initial, exact, ttl_plan)?
            .clamp_for_model(model);
    let calibrated_aggregate = super::bedrock::calibrate_authoritative_input_tokens(
        model,
        initial.total(),
        exact_usage.total(),
    );
    if calibrated_aggregate > exact_usage.total() {
        exact_usage.input_tokens = exact_usage
            .input_tokens
            .saturating_add(calibrated_aggregate - exact_usage.total());
    }
    Some(ReconciledNativeUsage {
        aggregate_input_tokens: calibrated_aggregate,
        output_tokens: exact.output_tokens,
        public_cache_usage: native_cache_buckets_are_trusted(model).then_some(exact_usage),
    })
}

fn native_cache_buckets_are_trusted(model: &str) -> bool {
    matches!(
        super::converter::map_model(model).as_deref(),
        Some("claude-opus-4.7" | "claude-opus-4.8" | "claude-opus-5")
    )
}

fn split_exact_cache_creation(total: i32, initial_5m: i32, initial_1h: i32) -> (i32, i32) {
    let total = total.max(0);
    let initial_5m = initial_5m.max(0);
    let initial_1h = initial_1h.max(0);
    let initial_total = initial_5m.saturating_add(initial_1h);
    if total == 0 {
        return (0, 0);
    }
    if initial_total == 0 {
        // Native metadata currently exposes only aggregate cache writes.  In
        // the absence of a request-side TTL split, Anthropic's default is 5m.
        return (total, 0);
    }
    let creation_1h = ((i64::from(total) * i64::from(initial_1h) + i64::from(initial_total) / 2)
        / i64::from(initial_total))
    .clamp(0, i64::from(total)) as i32;
    (total - creation_1h, creation_1h)
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
        cache_creation_5m_input_tokens: cc,
        cache_creation_1h_input_tokens: 0,
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
///
/// Anthropic 协议中 `cache_control` 可以出现在：
/// - `system[*].cache_control`
/// - `messages[*].content[*].cache_control`（content 是数组形态）
/// - `tools[*].cache_control`
///
/// 任何一处出现都视为"客户开启了 prompt caching"。
#[cfg(test)]
pub fn request_has_cache_control(req: &MessagesRequest) -> bool {
    req.cache_control.is_some() || request_cache_control_count(req) > 0
}

/// Count explicit cache breakpoints for the Bedrock-compatible four-block limit.
pub fn request_cache_control_count(req: &MessagesRequest) -> usize {
    let system = req
        .system
        .as_ref()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.cache_control.is_some())
                .count()
        })
        .unwrap_or(0);
    let messages = req
        .messages
        .iter()
        .map(message_cache_control_count)
        .sum::<usize>();
    let tools = req
        .tools
        .as_ref()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.cache_control.is_some())
                .count()
        })
        .unwrap_or(0);
    system + messages + tools
}

/// Count all public cache slots. Top-level automatic caching consumes one of
/// the same four slots as explicit block-level breakpoints.
fn request_cache_breakpoint_slot_count(req: &MessagesRequest) -> usize {
    request_cache_control_count(req) + usize::from(req.cache_control.is_some())
}

fn message_cache_control_count(msg: &Message) -> usize {
    match &msg.content {
        Value::String(_) => 0,
        Value::Array(items) => items.iter().map(direct_cache_control_count).sum(),
        value => direct_cache_control_count(value),
    }
}

fn direct_cache_control_count(value: &Value) -> usize {
    usize::from(
        value
            .as_object()
            .and_then(|map| map.get("cache_control"))
            .is_some_and(|control| !control.is_null()),
    )
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
pub async fn compute_request_usage_breakdown(
    total_input_tokens: i32,
    req: &MessagesRequest,
) -> UsageBreakdown {
    compute_request_usage_breakdown_with_profile(total_input_tokens, req, false).await
}

pub async fn compute_request_usage_breakdown_with_profile(
    total_input_tokens: i32,
    req: &MessagesRequest,
    aws_b40_compat: bool,
) -> UsageBreakdown {
    let total_input_tokens = total_input_tokens.max(0);
    let Some(cache_plan) = cache_plan_for_request(total_input_tokens, req, aws_b40_compat).await
    else {
        return UsageBreakdown::flat(total_input_tokens);
    };

    let planned_cache_tokens = cache_plan
        .cache_read_tokens
        .saturating_add(cache_plan.cache_creation_5m_tokens)
        .saturating_add(cache_plan.cache_creation_1h_tokens);
    let mut ordinary_input = if aws_b40_compat {
        total_input_tokens
            .saturating_sub(planned_cache_tokens)
            .max(1)
            .min(total_input_tokens)
    } else {
        total_input_tokens.saturating_sub(planned_cache_tokens)
    };
    // Cache read and creation are disjoint prefixes of this request's input.
    // Everything after the final breakpoint stays ordinary input. Bound both
    // cache buckets against one shared budget so estimator drift can never
    // make their sum exceed the request total.
    let cache_budget = total_input_tokens.saturating_sub(ordinary_input);
    let cache_read = cache_plan.cache_read_tokens.clamp(0, cache_budget);
    let creation_budget = cache_budget.saturating_sub(cache_read);
    let (creation_5m, creation_1h) = clamp_cache_creation(
        cache_plan.cache_creation_5m_tokens,
        cache_plan.cache_creation_1h_tokens,
        creation_budget,
    );
    let initial_creation = creation_5m.saturating_add(creation_1h);
    // Prefix and full-request estimators can legitimately differ by framing
    // tokens. Those tokens are outside the cached prefix, so they must remain
    // ordinary input instead of being charged at cache-write rates.
    let residual = total_input_tokens.saturating_sub(
        ordinary_input
            .saturating_add(cache_read)
            .saturating_add(initial_creation),
    );
    if residual > 0 {
        ordinary_input = ordinary_input.saturating_add(residual);
    }
    let cache_creation = creation_5m.saturating_add(creation_1h);

    UsageBreakdown {
        input_tokens: ordinary_input,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        cache_creation_5m_input_tokens: creation_5m,
        cache_creation_1h_input_tokens: creation_1h,
    }
}

/// 把 5m / 1h 两档缓存写入按比例压进 `budget`，并保持 `总量 == 5m + 1h` 恒等。
fn clamp_cache_creation(creation_5m: i32, creation_1h: i32, budget: i32) -> (i32, i32) {
    let creation_5m = creation_5m.max(0);
    let creation_1h = creation_1h.max(0);
    let total = creation_5m.saturating_add(creation_1h);
    if budget <= 0 {
        return (0, 0);
    }
    if total <= budget {
        return (creation_5m, creation_1h);
    }
    // 按原比例缩放，1h 档优先保留整数精度，余量归 5m，确保两档之和恰为 budget。
    let scaled_1h = ((i64::from(creation_1h) * i64::from(budget)) / i64::from(total)) as i32;
    let scaled_1h = scaled_1h.clamp(0, budget);
    (budget - scaled_1h, scaled_1h)
}

/// Produce the final public usage after validating every upstream round.
///
/// Every upstream AWS-B cache split requires Kiro's context event; without it
/// the locally fabricated cache estimate is held instead of billed. Local
/// short-circuit responses explicitly opt out because they make no upstream
/// request and keep their usage physically bounded here.
///
/// A model context window applies to one upstream invocation. Continuation
/// rounds are therefore validated independently and only then accumulated.
/// The final aggregate may legitimately exceed one context window.
pub fn finalize_request_usage(
    initial: UsageBreakdown,
    authoritative_first_round_input_tokens: Option<i32>,
    estimated_first_round_input_tokens: i32,
    additional_round_input_tokens: &[i32],
    ordinary_input_adjustment: i32,
    model: &str,
    authoritative_cache_context_required: bool,
) -> UsageBreakdown {
    let first_round = if authoritative_cache_context_required
        && initial.has_cache_usage()
        && authoritative_first_round_input_tokens.is_none()
    {
        tracing::warn!(
            estimated_input = estimated_first_round_input_tokens,
            estimated_cache_read = initial.cache_read_input_tokens,
            estimated_cache_creation = initial.cache_creation_input_tokens,
            "AWS-B 缓存请求缺少 Kiro contextUsageEvent，已暂停首轮异常输入计费"
        );
        UsageBreakdown::flat(1)
    } else {
        let first_round_input_tokens = authoritative_first_round_input_tokens
            .unwrap_or(estimated_first_round_input_tokens)
            .max(1);
        reconcile_initial_input(initial, first_round_input_tokens, ordinary_input_adjustment)
            .clamp_for_model(model)
    };

    let additional_input_tokens =
        additional_round_input_tokens
            .iter()
            .fold(0i32, |total, round| {
                let validated = UsageBreakdown::flat((*round).max(1)).clamp_for_model(model);
                total.saturating_add(validated.input_tokens)
            });
    UsageBreakdown {
        input_tokens: first_round
            .input_tokens
            .saturating_add(additional_input_tokens),
        ..first_round
    }
}

/// Reconcile the first-round cache split after an upstream context event.
///
/// A cache prefix is a deterministic function of the model, canonical prompt
/// prefix and tokenizer/accounting version. Kiro's `contextUsageEvent` is an
/// end-of-turn context occupancy value: it includes this turn's generated text
/// and hidden reasoning and it contains no cache read/write breakdown. It may
/// therefore validate a request envelope, but it must never resize cache-read
/// or cache-creation buckets. Only an exact upstream token-usage event may
/// replace those buckets.
pub fn reconcile_initial_input(
    initial: UsageBreakdown,
    calibrated_total_input_tokens: i32,
    ordinary_input_adjustment: i32,
) -> UsageBreakdown {
    let calibrated_total_input_tokens = calibrated_total_input_tokens.max(1);
    let initial_cached = initial
        .cache_read_input_tokens
        .saturating_add(initial.cache_creation_input_tokens);
    if initial_cached <= 0 {
        return UsageBreakdown::flat(calibrated_total_input_tokens);
    }

    let ordinary_input = initial
        .input_tokens
        .saturating_add(ordinary_input_adjustment)
        .max(1);

    UsageBreakdown {
        input_tokens: ordinary_input,
        cache_read_input_tokens: initial.cache_read_input_tokens,
        cache_creation_input_tokens: initial.cache_creation_input_tokens,
        cache_creation_5m_input_tokens: initial.cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens: initial.cache_creation_1h_input_tokens,
    }
}

struct CachePlan {
    cache_read_tokens: i32,
    cache_creation_5m_tokens: i32,
    cache_creation_1h_tokens: i32,
}

/// Minimum cacheable prefix for the currently supported Claude Platform on
/// AWS model families. Unknown/future models deliberately have no local
/// compatibility cache: authoritative native metadata may still report cache
/// activity, but the registry must not invent it from an assumed threshold.
fn local_cache_min_tokens(model: &str) -> Option<i32> {
    let mapped = super::converter::map_model(model)?;
    let family = mapped.strip_suffix("-1m").unwrap_or(&mapped);
    match family {
        "claude-opus-5" => Some(CACHE_MIN_TOKENS_OPUS_5),
        "claude-opus-4.8" | "claude-sonnet-5" | "claude-sonnet-4.6" | "claude-sonnet-4.5" => {
            Some(CACHE_MIN_TOKENS_OPUS_48_SONNET_5_46_45)
        }
        "claude-opus-4.7" => Some(CACHE_MIN_TOKENS_OPUS_47),
        "claude-opus-4.6" | "claude-opus-4.5" | "claude-haiku-4.5" => {
            Some(CACHE_MIN_TOKENS_OPUS_46_45_HAIKU_45)
        }
        _ => None,
    }
}

/// Cache-registry mutation prepared from the exact request prefix.
///
/// Planning usage is deliberately read-only.  The caller owns this value and
/// commits it only after the first upstream model invocation has completed
/// successfully.  This prevents validation failures, local compatibility
/// replies and failed provider calls from warming the public cache-accounting
/// registry even though no upstream prompt cache could have been created.
#[derive(Debug, Default)]
pub(super) struct CacheCommit {
    entries: Vec<(CacheKey, CacheTtl)>,
    exact_ttl_plan: ExactCacheTtlPlan,
}

impl CacheCommit {
    pub(super) fn exact_ttl_plan(&self) -> ExactCacheTtlPlan {
        self.exact_ttl_plan
    }

    pub(super) async fn commit(self) {
        tracing::debug!(
            entries = self.entries.len(),
            "committing prompt-cache registry entries"
        );
        for (key, ttl) in self.entries {
            register_cache_key(key, ttl).await;
        }
    }
}

/// Prepare the registry keys that may become warm after a successful upstream
/// invocation.  This is intentionally separate from usage planning: the
/// latter may run for a local short-circuit response and must never mutate
/// cache state.
pub(super) fn prepare_cache_commit(
    total_input_tokens: i32,
    req: &MessagesRequest,
    aws_b40_compat: bool,
) -> CacheCommit {
    let breakpoint_slots = request_cache_breakpoint_slot_count(req);
    if breakpoint_slots == 0 || breakpoint_slots > MAX_CACHE_BREAKPOINTS {
        return CacheCommit::default();
    }

    let CacheBuild {
        mut breakpoints,
        exact_ttl_plan,
        ..
    } = build_cache_breakpoints(req, total_input_tokens.max(0), aws_b40_compat);
    breakpoints.sort_by_key(|breakpoint| breakpoint.tokens);
    breakpoints.truncate(MAX_CACHE_BREAKPOINTS);
    let local_minimum = local_cache_min_tokens(&req.model);
    breakpoints
        .retain(|breakpoint| local_minimum.is_some_and(|minimum| breakpoint.tokens >= minimum));

    let mut entries = Vec::new();
    for breakpoint in breakpoints {
        if breakpoint.readable
            && !entries
                .iter()
                .any(|(key, ttl)| *key == breakpoint.key && *ttl == breakpoint.ttl)
        {
            entries.push((breakpoint.key, breakpoint.ttl));
        }
    }
    tracing::debug!(
        model = %req.model,
        breakpoint_slots,
        entries = entries.len(),
        keys = ?entries
            .iter()
            .map(|(key, ttl)| (key.redis_key(), *ttl))
            .collect::<Vec<_>>(),
        "prepared prompt-cache registry commit"
    );
    CacheCommit {
        entries,
        exact_ttl_plan,
    }
}

async fn cache_plan_for_request(
    total_input_tokens: i32,
    req: &MessagesRequest,
    aws_b40_compat: bool,
) -> Option<CachePlan> {
    let local_minimum = local_cache_min_tokens(&req.model)?;
    let breakpoint_slots = request_cache_breakpoint_slot_count(req);
    if breakpoint_slots == 0 {
        return None;
    }
    // The Anthropic/Bedrock contract allows at most four explicit blocks.
    // HTTP preflight owns the client-facing error; this accounting layer fails
    // closed so an unvalidated route cannot turn malformed input into B×N work.
    if breakpoint_slots > MAX_CACHE_BREAKPOINTS {
        return None;
    }

    let CacheBuild {
        mut breakpoints,
        token_context,
        ..
    } = build_cache_breakpoints(req, total_input_tokens, aws_b40_compat);
    breakpoints.retain(|b| b.tokens >= local_minimum);
    if breakpoints.is_empty() {
        return None;
    }

    breakpoints.sort_by_key(|b| b.tokens);
    breakpoints.truncate(MAX_CACHE_BREAKPOINTS);

    let mut read_match: Option<CacheReadMatch> = None;
    let mut candidate_tokens = HashMap::new();
    for breakpoint in breakpoints.iter().rev() {
        if let Some(candidate) = cache_entry_match(
            req,
            breakpoint,
            &token_context,
            aws_b40_compat,
            &mut candidate_tokens,
        )
        .await
            && read_match
                .as_ref()
                .is_none_or(|current| candidate.tokens > current.tokens)
        {
            read_match = Some(candidate);
        }
    }

    let terminal_breakpoint = breakpoints.last()?;
    let max_cache_tokens = terminal_breakpoint.tokens;
    let read_tokens = read_match
        .as_ref()
        .map(|candidate| candidate.tokens.min(max_cache_tokens))
        .unwrap_or(0);
    let mut creation_5m = 0;
    let mut creation_1h = 0;
    let mut previous = read_tokens;

    for breakpoint in breakpoints
        .iter()
        .filter(|breakpoint| breakpoint.tokens > read_tokens)
    {
        let delta = (breakpoint.tokens - previous).max(0);
        match breakpoint.ttl {
            CacheTtl::Ephemeral1h => creation_1h += delta,
            CacheTtl::Ephemeral5m => creation_5m += delta,
        }
        previous = breakpoint.tokens;
    }

    Some(CachePlan {
        cache_read_tokens: read_tokens,
        cache_creation_5m_tokens: creation_5m,
        cache_creation_1h_tokens: creation_1h,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheTtl {
    Ephemeral5m,
    Ephemeral1h,
}

impl CacheTtl {
    fn duration(self) -> Duration {
        match self {
            Self::Ephemeral5m => Duration::from_secs(5 * 60),
            Self::Ephemeral1h => Duration::from_secs(60 * 60),
        }
    }

    fn cache_key_label(self) -> &'static [u8] {
        match self {
            Self::Ephemeral5m => b"Ephemeral5m",
            Self::Ephemeral1h => b"Ephemeral1h",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CacheKey([u8; 32]);

impl CacheKey {
    fn redis_key(self) -> String {
        format!("krcc:{}", hex::encode(self.0))
    }
}

#[derive(Clone, Copy)]
struct CacheKeyPair {
    ephemeral_5m: CacheKey,
    ephemeral_1h: CacheKey,
}

impl CacheKeyPair {
    fn for_ttl(self, ttl: CacheTtl) -> CacheKey {
        match ttl {
            CacheTtl::Ephemeral5m => self.ephemeral_5m,
            CacheTtl::Ephemeral1h => self.ephemeral_1h,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct PrefixPosition {
    tool_count: usize,
    system_count: usize,
    content_count: usize,
}

#[derive(Clone)]
struct CacheReadCandidate {
    keys: CacheKeyPair,
    position: PrefixPosition,
    /// Precomputed token endpoint for converter-derived wire layouts. Legacy
    /// public layouts leave this at zero and hydrate it lazily on a real hit.
    tokens: i32,
}

struct CacheReadMatch {
    tokens: i32,
}

#[derive(Clone, Copy)]
struct CacheBreakpointOptions {
    readable: bool,
    warm_on_first_use: bool,
}

struct CacheBreakpoint {
    tokens: i32,
    ttl: CacheTtl,
    key: CacheKey,
    position: PrefixPosition,
    readable: bool,
    read_candidates: Vec<CacheReadCandidate>,
    warm_on_first_use: bool,
}

struct PrefixTokenContext {
    tools: Vec<Tool>,
    system_segments: Vec<String>,
    content_segments: Vec<Value>,
}

struct CacheBuild {
    breakpoints: Vec<CacheBreakpoint>,
    token_context: PrefixTokenContext,
    exact_ttl_plan: ExactCacheTtlPlan,
}

impl ExactCacheTtlPlan {
    /// Build the ordered TTL layout directly from the request, without
    /// consulting or mutating the local hot/cold registry.
    #[allow(dead_code)]
    pub(super) fn for_request(
        total_input_tokens: i32,
        req: &MessagesRequest,
        aws_b40_compat: bool,
    ) -> Self {
        let breakpoint_slots = request_cache_breakpoint_slot_count(req);
        if breakpoint_slots == 0 || breakpoint_slots > MAX_CACHE_BREAKPOINTS {
            return Self::default();
        }

        build_cache_breakpoints(req, total_input_tokens.max(0), aws_b40_compat).exact_ttl_plan
    }
}

#[derive(Clone, Copy)]
struct NativeCachePointSpec {
    position: PrefixPosition,
    ttl: CacheTtl,
}

/// Return only cache points which `converter.rs` can put on the Kiro wire.
///
/// The public Anthropic fallback deliberately supports finer block boundaries,
/// but Kiro has message-level cache points: every marked tool is representable,
/// the synthetic system message uses only the final system item, and a merged
/// history/current message uses only its terminal content block. Trailing
/// assistant prefill is dropped by the converter and therefore contributes no
/// native point.
fn native_cache_point_specs(req: &MessagesRequest) -> Vec<NativeCachePointSpec> {
    let tool_count = req.tools.as_ref().map_or(0, Vec::len);
    let system_count = req.system.as_ref().map_or(0, Vec::len);
    let mut specs = Vec::new();

    if let Some(tools) = &req.tools {
        for (index, tool) in tools.iter().enumerate() {
            if tool.cache_control.is_some() {
                specs.push(NativeCachePointSpec {
                    position: PrefixPosition {
                        tool_count: index + 1,
                        system_count: 0,
                        content_count: 0,
                    },
                    ttl: cache_ttl(tool.cache_control.as_ref()),
                });
            }
        }
    }

    if let Some(system) = &req.system
        && let Some(last) = system.last()
        && last.cache_control.is_some()
    {
        specs.push(NativeCachePointSpec {
            position: PrefixPosition {
                tool_count,
                system_count,
                content_count: 0,
            },
            ttl: cache_ttl(last.cache_control.as_ref()),
        });
    }

    let Some(current_index) = converter_current_message_index(&req.messages) else {
        return specs;
    };
    let history_terminals = converter_history_group_terminals(&req.messages, current_index);
    let mut content_count = 0usize;
    for (index, message) in req.messages.iter().enumerate() {
        content_count = content_count.saturating_add(message_content_segment_count(message));
        if index > current_index {
            break;
        }

        let converter_emits_this_message = index == current_index || history_terminals[index];
        let terminal_control = converter_emits_this_message
            .then(|| terminal_message_cache_control(&message.content))
            .flatten();
        if let Some(cache_control) = terminal_control {
            specs.push(NativeCachePointSpec {
                position: PrefixPosition {
                    tool_count,
                    system_count,
                    content_count,
                },
                ttl: cache_ttl(Some(cache_control)),
            });
        } else if index == current_index
            && let Some(cache_control) = req.cache_control.as_ref()
        {
            // Current-message explicit cache control wins over automatic
            // caching, exactly matching converter's Option::or ordering.
            specs.push(NativeCachePointSpec {
                position: PrefixPosition {
                    tool_count,
                    system_count,
                    content_count,
                },
                ttl: cache_ttl(Some(cache_control)),
            });
        }
    }

    specs
}

fn converter_current_message_index(messages: &[Message]) -> Option<usize> {
    let last = messages.last()?;
    if last.role == "user" {
        Some(messages.len() - 1)
    } else {
        messages.iter().rposition(|message| message.role == "user")
    }
}

/// Mirror `build_history`: consecutive messages with the same recognized role
/// are merged and only the final member's terminal cache marker survives.
fn converter_history_group_terminals(messages: &[Message], history_end: usize) -> Vec<bool> {
    let mut terminals = vec![false; messages.len()];
    let mut pending_role: Option<&str> = None;
    let mut pending_last: Option<usize> = None;

    for (index, message) in messages.iter().take(history_end).enumerate() {
        let role = message.role.as_str();
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        if pending_role.is_some_and(|pending| pending != role) {
            if let Some(last) = pending_last {
                terminals[last] = true;
            }
        }
        pending_role = Some(role);
        pending_last = Some(index);
    }
    if let Some(last) = pending_last {
        terminals[last] = true;
    }
    terminals
}

fn message_content_segment_count(message: &Message) -> usize {
    match &message.content {
        Value::Array(items) => items.len(),
        _ => 1,
    }
}

fn terminal_message_cache_control(content: &Value) -> Option<&Value> {
    let cache_control = match content {
        Value::Array(items) => items.last().and_then(|item| item.get("cache_control")),
        Value::Object(object) => object.get("cache_control"),
        _ => None,
    };
    cache_control.filter(|control| !control.is_null())
}

fn build_exact_cache_ttl_plan(
    req: &MessagesRequest,
    total_input_tokens: i32,
    aws_b40_compat: bool,
    state: &PrefixState,
    local_breakpoints: &[CacheBreakpoint],
) -> ExactCacheTtlPlan {
    if request_cache_breakpoint_slot_count(req) > MAX_CACHE_BREAKPOINTS {
        return ExactCacheTtlPlan::default();
    }

    let mut plan = ExactCacheTtlPlan::default();
    for spec in native_cache_point_specs(req)
        .into_iter()
        .take(MAX_CACHE_BREAKPOINTS)
    {
        let tokens = local_breakpoints
            .iter()
            .find(|breakpoint| breakpoint.position == spec.position && breakpoint.ttl == spec.ttl)
            .map(|breakpoint| breakpoint.tokens)
            .or_else(|| {
                calibrated_prefix_tokens_at(
                    req,
                    &state.tools,
                    &state.system_segments,
                    &state.content_segments,
                    spec.position,
                    aws_b40_compat,
                )
            })
            .unwrap_or(0)
            .min(total_input_tokens)
            .max(0);
        if tokens <= 0 {
            continue;
        }
        plan.segments[plan.len] = ExactCacheTtlSegment {
            end_tokens: tokens,
            one_hour: spec.ttl == CacheTtl::Ephemeral1h,
        };
        plan.len += 1;
    }
    plan
}

fn build_cache_breakpoints(
    req: &MessagesRequest,
    total_input_tokens: i32,
    aws_b40_compat: bool,
) -> CacheBuild {
    if request_cache_breakpoint_slot_count(req) > MAX_CACHE_BREAKPOINTS {
        return empty_cache_build();
    }
    if aws_b40_compat {
        return build_forwarded_cache_breakpoints(req, total_input_tokens)
            .unwrap_or_else(empty_cache_build);
    }

    let mut state = PrefixState::new(&req.model);
    let mut breakpoints = Vec::new();

    // Thinking mode and resolved effort are rendered ahead of the user prompt
    // by this transport. Hash the effective values, not raw API JSON, so an
    // omitted effort and explicit `high` share identity while a real behavior
    // change cannot reuse an incompatible prefix.
    if let Some(config) = rendered_thinking_and_effort_key(req) {
        state.push_key_part(&format!("model-config:{config}"));
    }

    if let Some(tools) = &req.tools {
        for tool in tools {
            state.tools.push(tool.clone());
            let mut tool_key = serde_json::to_value(tool).unwrap_or(Value::Null);
            strip_cache_control(&mut tool_key);
            state.push_key_part(&format!("tool:{}", canonical_json(&tool_key)));
            if tool.cache_control.is_some() {
                let warm_on_first_use =
                    aws_b40_compat && cache_control_is_global(tool.cache_control.as_ref());
                push_breakpoint(
                    &state,
                    &mut breakpoints,
                    cache_ttl(tool.cache_control.as_ref()),
                    CacheBreakpointOptions {
                        readable: true,
                        warm_on_first_use,
                    },
                );
            }
        }
    }

    // The Kiro transport currently renders forced tool choice into its system
    // instruction. Keep tool-definition keys reusable, but invalidate system
    // and message prefixes when that effective instruction changes.
    if let Some(tool_choice) = effective_forced_tool_choice_key(req) {
        state.push_key_part(&format!("forced-tool-choice:{tool_choice}"));
    }

    if let Some(system) = &req.system {
        for item in system {
            state.system_segments.push(item.text.clone());
            state.push_key_part(&format!("system:{}", item.text));
            if item.cache_control.is_some() {
                let warm_on_first_use =
                    aws_b40_compat && cache_control_is_global(item.cache_control.as_ref());
                push_breakpoint(
                    &state,
                    &mut breakpoints,
                    cache_ttl(item.cache_control.as_ref()),
                    CacheBreakpointOptions {
                        readable: true,
                        warm_on_first_use,
                    },
                );
            }
        }
    }

    // Claude invalidates every message-level cache when image presence toggles,
    // even if the image occurs after an earlier message breakpoint. This salt
    // deliberately starts after tools/system so those layers remain reusable.
    if !req.messages.is_empty() {
        state.push_key_part(if request_has_user_image(req) {
            "message-image-presence:true"
        } else {
            "message-image-presence:false"
        });
    }

    for message in &req.messages {
        collect_message_prefix(message, &mut state, &mut breakpoints, aws_b40_compat);
    }

    if req.cache_control.is_some() && state.has_cacheable_content() {
        let ttl = cache_ttl(req.cache_control.as_ref());
        let position = state.position();
        // Automatic caching is an independent final breakpoint. It is a no-op
        // only when the final cacheable block already has an explicit marker
        // with the same TTL; preflight rejects the different-TTL form.
        if !breakpoints
            .iter()
            .any(|breakpoint| breakpoint.position == position && breakpoint.ttl == ttl)
        {
            let warm_on_first_use =
                aws_b40_compat && cache_control_is_global(req.cache_control.as_ref());
            push_breakpoint(
                &state,
                &mut breakpoints,
                ttl,
                CacheBreakpointOptions {
                    readable: true,
                    warm_on_first_use,
                },
            );
        }
    }

    breakpoints.retain_mut(|breakpoint| {
        let Some(tokens) = calibrated_prefix_tokens_at(
            req,
            &state.tools,
            &state.system_segments,
            &state.content_segments,
            breakpoint.position,
            aws_b40_compat,
        ) else {
            tracing::error!(
                ?breakpoint.position,
                "cache breakpoint contained an invalid prefix position"
            );
            return false;
        };
        // A cache breakpoint is a prefix of this request. It therefore cannot
        // exceed the request total under any profile. Keeping the same bound
        // for AWS-B also prevents a cached prefix estimator from turning
        // discarded media bytes or an extrapolated calibration into billable
        // cache usage.
        breakpoint.tokens = tokens.min(total_input_tokens).max(0);
        true
    });

    let exact_ttl_plan = build_exact_cache_ttl_plan(
        req,
        total_input_tokens,
        aws_b40_compat,
        &state,
        &breakpoints,
    );

    CacheBuild {
        breakpoints,
        token_context: state.into_token_context(),
        exact_ttl_plan,
    }
}

fn empty_cache_build() -> CacheBuild {
    CacheBuild {
        breakpoints: Vec::new(),
        token_context: PrefixTokenContext {
            tools: Vec::new(),
            system_segments: Vec::new(),
            content_segments: Vec::new(),
        },
        exact_ttl_plan: ExactCacheTtlPlan::default(),
    }
}

#[derive(Clone)]
struct ForwardedPrefixSnapshot {
    keys: CacheKeyPair,
    segment_count: usize,
    cumulative_weight: i32,
}

struct RawForwardedCachePoint {
    ttl: CacheTtl,
    key: CacheKey,
    segment_count: usize,
    cumulative_weight: i32,
    candidates: Vec<ForwardedPrefixSnapshot>,
}

/// Build the AWS-B cache layout from the final Kiro conversation wire.
///
/// This is the single source of truth for fallback keys, warm commits and the
/// native exact-TTL plan. Calling the real converter here intentionally folds
/// in all model-visible transformations: prefill truncation, same-role merge,
/// dynamic system policy, forced-tool instructions, placeholder tools, media
/// promotion and the exact cachePoint positions actually sent upstream.
fn build_forwarded_cache_breakpoints(
    req: &MessagesRequest,
    total_input_tokens: i32,
) -> Option<CacheBuild> {
    let converted = super::converter::convert_request(req).ok()?;
    let system_history_len = converted.system_history_len;
    let conversation = converted.conversation_state;
    let mut key_state = PrefixKeyState::new(&req.model);
    let mut segments = Vec::<Value>::new();
    let mut snapshots = VecDeque::<ForwardedPrefixSnapshot>::new();
    let mut raw_points = Vec::<RawForwardedCachePoint>::new();
    let mut cumulative_weight = 0i32;

    let push_segment = |label: &str,
                        value: Value,
                        key_state: &mut PrefixKeyState,
                        segments: &mut Vec<Value>,
                        snapshots: &mut VecDeque<ForwardedPrefixSnapshot>,
                        cumulative_weight: &mut i32| {
        let canonical = canonical_json(&value);
        key_state.push(&format!("{label}:{canonical}"));
        *cumulative_weight =
            cumulative_weight.saturating_add(super::claude_tok::count_claude(&canonical).max(1));
        segments.push(value);
        if snapshots.len() == MAX_READ_CANDIDATES + 1 {
            snapshots.pop_front();
        }
        snapshots.push_back(ForwardedPrefixSnapshot {
            keys: key_state.snapshot(),
            segment_count: segments.len(),
            cumulative_weight: *cumulative_weight,
        });
    };

    let capture_point = |ttl: CacheTtl,
                         key_state: &PrefixKeyState,
                         segments: &[Value],
                         snapshots: &VecDeque<ForwardedPrefixSnapshot>,
                         cumulative_weight: i32,
                         raw_points: &mut Vec<RawForwardedCachePoint>| {
        if raw_points.len() >= MAX_CACHE_BREAKPOINTS || segments.is_empty() {
            return;
        }
        let key = key_state.snapshot().for_ttl(ttl);
        let candidates = snapshots
            .iter()
            .rev()
            .skip_while(|candidate| candidate.keys.for_ttl(ttl) == key)
            .take(MAX_READ_CANDIDATES)
            .cloned()
            .collect();
        raw_points.push(RawForwardedCachePoint {
            ttl,
            key,
            segment_count: segments.len(),
            cumulative_weight,
            candidates,
        });
    };

    if let Some(config) = rendered_thinking_and_effort_key(req) {
        push_segment(
            "model-config",
            Value::String(config),
            &mut key_state,
            &mut segments,
            &mut snapshots,
            &mut cumulative_weight,
        );
    }

    let current = conversation.current_message.user_input_message;
    for tool in &current.user_input_message_context.tools {
        if let Some(specification) = tool.tool_specification.as_ref() {
            push_segment(
                "tool",
                serde_json::to_value(specification).ok()?,
                &mut key_state,
                &mut segments,
                &mut snapshots,
                &mut cumulative_weight,
            );
        } else if let Some(cache_point) = tool.cache_point.as_ref() {
            capture_point(
                forwarded_cache_ttl(cache_point.ttl.as_deref()),
                &key_state,
                &segments,
                &snapshots,
                cumulative_weight,
                &mut raw_points,
            );
        }
    }

    let mut message_scope_salted = false;
    if system_history_len == 0 {
        key_state.push(if request_has_user_image(req) {
            "message-image-presence:true"
        } else {
            "message-image-presence:false"
        });
        message_scope_salted = true;
    }
    for (history_index, message) in conversation.history.iter().enumerate() {
        let (label, mut value, ttl) = match message {
            crate::kiro::model::requests::conversation::Message::User(message) => {
                let ttl = message
                    .user_input_message
                    .cache_point
                    .as_ref()
                    .map(|point| forwarded_cache_ttl(point.ttl.as_deref()));
                let mut value = serde_json::to_value(&message.user_input_message).ok()?;
                normalize_forwarded_user_value(&mut value);
                ("user", value, ttl)
            }
            crate::kiro::model::requests::conversation::Message::Assistant(message) => {
                let ttl = message
                    .assistant_response_message
                    .cache_point
                    .as_ref()
                    .map(|point| forwarded_cache_ttl(point.ttl.as_deref()));
                (
                    "assistant",
                    serde_json::to_value(&message.assistant_response_message).ok()?,
                    ttl,
                )
            }
        };
        remove_top_level_cache_point(&mut value);
        push_segment(
            label,
            value,
            &mut key_state,
            &mut segments,
            &mut snapshots,
            &mut cumulative_weight,
        );
        if let Some(ttl) = ttl {
            capture_point(
                ttl,
                &key_state,
                &segments,
                &snapshots,
                cumulative_weight,
                &mut raw_points,
            );
        }
        if !message_scope_salted && history_index + 1 == system_history_len {
            key_state.push(if request_has_user_image(req) {
                "message-image-presence:true"
            } else {
                "message-image-presence:false"
            });
            message_scope_salted = true;
        }
    }

    if !message_scope_salted {
        key_state.push(if request_has_user_image(req) {
            "message-image-presence:true"
        } else {
            "message-image-presence:false"
        });
    }

    let current_ttl = current
        .cache_point
        .as_ref()
        .map(|point| forwarded_cache_ttl(point.ttl.as_deref()));
    let mut current_value = serde_json::to_value(&current).ok()?;
    normalize_forwarded_user_value(&mut current_value);
    push_segment(
        "user",
        current_value,
        &mut key_state,
        &mut segments,
        &mut snapshots,
        &mut cumulative_weight,
    );
    if let Some(ttl) = current_ttl {
        capture_point(
            ttl,
            &key_state,
            &segments,
            &snapshots,
            cumulative_weight,
            &mut raw_points,
        );
    }

    let full_weight = cumulative_weight.max(1);
    let total_input_tokens = total_input_tokens.max(0);
    let scale = |weight: i32| -> i32 {
        if weight <= 0 || total_input_tokens <= 0 {
            return 0;
        }
        ((i64::from(weight) * i64::from(total_input_tokens) + i64::from(full_weight) / 2)
            / i64::from(full_weight))
        .clamp(0, i64::from(total_input_tokens)) as i32
    };

    let mut exact_ttl_plan = ExactCacheTtlPlan::default();
    let mut breakpoints = Vec::with_capacity(raw_points.len());
    for raw in raw_points {
        let tokens = scale(raw.cumulative_weight);
        if tokens <= 0 {
            continue;
        }
        let position = PrefixPosition {
            tool_count: 0,
            system_count: 0,
            content_count: raw.segment_count,
        };
        let read_candidates = raw
            .candidates
            .into_iter()
            .map(|candidate| CacheReadCandidate {
                keys: candidate.keys,
                position: PrefixPosition {
                    tool_count: 0,
                    system_count: 0,
                    content_count: candidate.segment_count,
                },
                tokens: scale(candidate.cumulative_weight),
            })
            .collect();
        breakpoints.push(CacheBreakpoint {
            tokens,
            ttl: raw.ttl,
            key: raw.key,
            position,
            readable: true,
            read_candidates,
            warm_on_first_use: false,
        });
        if exact_ttl_plan.len < MAX_CACHE_BREAKPOINTS {
            exact_ttl_plan.segments[exact_ttl_plan.len] = ExactCacheTtlSegment {
                end_tokens: tokens,
                one_hour: raw.ttl == CacheTtl::Ephemeral1h,
            };
            exact_ttl_plan.len += 1;
        }
    }

    Some(CacheBuild {
        breakpoints,
        token_context: PrefixTokenContext {
            tools: Vec::new(),
            system_segments: Vec::new(),
            content_segments: segments,
        },
        exact_ttl_plan,
    })
}

fn forwarded_cache_ttl(ttl: Option<&str>) -> CacheTtl {
    if ttl == Some("1h") {
        CacheTtl::Ephemeral1h
    } else {
        CacheTtl::Ephemeral5m
    }
}

fn remove_top_level_cache_point(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("cachePoint");
    }
}

fn normalize_forwarded_user_value(value: &mut Value) {
    remove_top_level_cache_point(value);
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let remove_context = object
        .get_mut("userInputMessageContext")
        .and_then(Value::as_object_mut)
        .is_some_and(|context| {
            // Tool specifications live before system/messages in the public
            // cache hierarchy and are already hashed separately above.
            context.remove("tools");
            context.is_empty()
        });
    if remove_context {
        object.remove("userInputMessageContext");
    }
}

/// 前缀 key 各块之间的分隔符（保持与历史实现一致的字节序列）。
const KEY_PART_SEPARATOR: &str = "\n---prefix-block---\n";

struct PrefixKeyState {
    ephemeral_5m: Sha256,
    ephemeral_1h: Sha256,
    part_count: usize,
}

/// Resolve the model namespace used by both the in-process and shared
/// prompt-cache registries.
///
/// Cache identity follows the model ID that `converter` actually puts on the
/// upstream wire, so public aliases for the same model reuse one prefix.  The
/// mapped value is retained byte-for-byte (including a distinct `-1m` route if
/// the converter exposes one) so different upstream models cannot share a
/// registry entry. Unsupported names deliberately keep their exact requested
/// bytes instead of being normalized together; normal request validation will
/// reject them, while direct/internal callers still fail closed into separate
/// namespaces.
fn cache_model_identity(model: &str) -> String {
    super::converter::map_model(model).unwrap_or_else(|| model.to_string())
}

impl PrefixKeyState {
    fn new(model: &str) -> Self {
        let model = cache_model_identity(model);
        Self {
            ephemeral_5m: seeded_cache_hasher(&model, CacheTtl::Ephemeral5m),
            ephemeral_1h: seeded_cache_hasher(&model, CacheTtl::Ephemeral1h),
            part_count: 0,
        }
    }

    fn push(&mut self, part: &str) {
        if self.part_count > 0 {
            self.ephemeral_5m.update(KEY_PART_SEPARATOR.as_bytes());
            self.ephemeral_1h.update(KEY_PART_SEPARATOR.as_bytes());
        }
        self.ephemeral_5m.update(part.as_bytes());
        self.ephemeral_1h.update(part.as_bytes());
        self.part_count += 1;
    }

    fn snapshot(&self) -> CacheKeyPair {
        CacheKeyPair {
            ephemeral_5m: finalize_cache_key(&self.ephemeral_5m),
            ephemeral_1h: finalize_cache_key(&self.ephemeral_1h),
        }
    }
}

struct PrefixState {
    tools: Vec<Tool>,
    system_segments: Vec<String>,
    content_segments: Vec<Value>,
    // Both hashers include the legacy model/TTL header. Appending each prefix
    // part once therefore produces the exact Redis keys used before c83492,
    // without ever retaining or rebuilding the joined prefix string.
    key_state: PrefixKeyState,
    // A breakpoint skips the current prefix and reads at most 20 predecessors.
    // Keeping 21 entries preserves that behavior while bounding request memory.
    read_candidates: VecDeque<CacheReadCandidate>,
}

impl PrefixState {
    fn new(model: &str) -> Self {
        Self {
            tools: Vec::new(),
            system_segments: Vec::new(),
            content_segments: Vec::new(),
            key_state: PrefixKeyState::new(model),
            read_candidates: VecDeque::with_capacity(MAX_READ_CANDIDATES + 1),
        }
    }

    fn has_cacheable_content(&self) -> bool {
        !self.tools.is_empty()
            || !self.system_segments.is_empty()
            || !self.content_segments.is_empty()
    }

    fn push_key_part(&mut self, part: &str) {
        self.key_state.push(part);
    }

    fn cache_keys(&self) -> CacheKeyPair {
        self.key_state.snapshot()
    }

    fn position(&self) -> PrefixPosition {
        PrefixPosition {
            tool_count: self.tools.len(),
            system_count: self.system_segments.len(),
            content_count: self.content_segments.len(),
        }
    }

    fn into_token_context(self) -> PrefixTokenContext {
        PrefixTokenContext {
            tools: self.tools,
            system_segments: self.system_segments,
            content_segments: self.content_segments,
        }
    }
}

fn collect_message_prefix(
    message: &Message,
    state: &mut PrefixState,
    breakpoints: &mut Vec<CacheBreakpoint>,
    aws_b40_compat: bool,
) {
    match &message.content {
        Value::String(text) => {
            let content = Value::String(text.clone());
            state.push_key_part(&format!("{}:{}", message.role, text));
            state.content_segments.push(content);
            remember_read_candidate(state);
        }
        Value::Array(items) => {
            for item in items {
                let mut item_without_cache = item.clone();
                let ttl = cache_ttl(item_without_cache.get("cache_control"));
                if let Some(obj) = item_without_cache.as_object_mut() {
                    obj.remove("cache_control");
                    // Kiro history has no field for an Anthropic thinking
                    // signature; only the thinking text reaches the model.
                    // Do not turn opaque response metadata into a false miss.
                    if message.role == "assistant"
                        && obj.get("type").and_then(Value::as_str) == Some("thinking")
                    {
                        obj.remove("signature");
                    }
                }
                state.push_key_part(&format!(
                    "{}:{}",
                    message.role,
                    canonical_json(&item_without_cache)
                ));
                state
                    .content_segments
                    .push(Value::Array(vec![item_without_cache]));
                remember_read_candidate(state);
                if has_direct_cache_control(item) {
                    let readable = aws_b40_compat;
                    let warm_on_first_use =
                        readable && cache_control_is_global(item.get("cache_control"));
                    push_breakpoint(
                        state,
                        breakpoints,
                        ttl,
                        CacheBreakpointOptions {
                            readable,
                            warm_on_first_use,
                        },
                    );
                }
            }
        }
        other => {
            state.push_key_part(&format!("{}:{}", message.role, canonical_json(other)));
            state.content_segments.push(other.clone());
            remember_read_candidate(state);
        }
    }
}

fn remember_read_candidate(state: &mut PrefixState) {
    let candidate = CacheReadCandidate {
        keys: state.cache_keys(),
        position: state.position(),
        tokens: 0,
    };
    if state.read_candidates.len() == MAX_READ_CANDIDATES + 1 {
        state.read_candidates.pop_front();
    }
    state.read_candidates.push_back(candidate);
}

fn strip_cache_control(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("cache_control");
            for child in map.values_mut() {
                strip_cache_control(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_cache_control(item);
            }
        }
        _ => {}
    }
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            let body = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(items) => {
            let body = items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn push_breakpoint(
    state: &PrefixState,
    breakpoints: &mut Vec<CacheBreakpoint>,
    ttl: CacheTtl,
    options: CacheBreakpointOptions,
) {
    // Normal request validation rejects a fifth explicit breakpoint. Keep this
    // hard bound here too because build_cache_breakpoints is shared by routes.
    if breakpoints.len() >= MAX_CACHE_BREAKPOINTS {
        return;
    }

    let current_key = state.cache_keys().for_ttl(ttl);
    let read_candidates = if options.readable {
        state
            .read_candidates
            .iter()
            .rev()
            .skip_while(|candidate| candidate.keys.for_ttl(ttl) == current_key)
            .take(MAX_READ_CANDIDATES)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    breakpoints.push(CacheBreakpoint {
        // Hydrated once after all prefix parts have been collected. Candidate
        // creation stays O(1) and never invokes the full tokenizer.
        tokens: 0,
        ttl,
        key: current_key,
        position: state.position(),
        readable: options.readable,
        read_candidates,
        warm_on_first_use: options.warm_on_first_use,
    });
}

fn calibrated_prefix_tokens_at(
    req: &MessagesRequest,
    tools: &[Tool],
    system_segments: &[String],
    content_segments: &[Value],
    position: PrefixPosition,
    aws_b40_compat: bool,
) -> Option<i32> {
    let tools = tools.get(..position.tool_count)?;
    let system_segments = system_segments.get(..position.system_count)?;
    let content_segments = content_segments.get(..position.content_count)?;
    #[cfg(test)]
    PREFIX_TOKENIZATION_CALLS.with(|calls| calls.set(calls.get() + 1));
    let base_tokens =
        super::compat::estimate_prefix_tokens(&req.model, system_segments, content_segments, tools);
    Some(if aws_b40_compat {
        super::bedrock::calibrated_cache_prefix_tokens(
            &req.model,
            base_tokens,
            system_segments,
            content_segments,
            tools,
        )
    } else {
        base_tokens
    })
}

fn has_direct_cache_control(v: &Value) -> bool {
    v.as_object()
        .and_then(|map| map.get("cache_control"))
        .is_some_and(|control| !control.is_null())
}

fn rendered_thinking_and_effort_key(req: &MessagesRequest) -> Option<String> {
    super::converter::forwarded_claude_effort(req).map(|effort| format!("effort={effort}"))
}

fn effective_forced_tool_choice_key(req: &MessagesRequest) -> Option<String> {
    let choice = req.tool_choice.as_ref()?;
    let tools = req.tools.as_ref()?;
    match choice.get("type").and_then(Value::as_str) {
        Some("any") if !tools.is_empty() => Some("any".to_string()),
        Some("tool") => {
            let name = choice.get("name").and_then(Value::as_str)?;
            tools
                .iter()
                .any(|tool| tool.name == name)
                .then(|| format!("tool:{}", canonical_json(&Value::String(name.to_string()))))
        }
        _ => None,
    }
}

fn request_has_user_image(req: &MessagesRequest) -> bool {
    req.messages
        .iter()
        .filter(|message| message.role == "user")
        .any(|message| value_contains_image(&message.content))
}

fn value_contains_image(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(value_contains_image),
        Value::Object(map) => {
            map.get("type").and_then(Value::as_str) == Some("image")
                || map.values().any(value_contains_image)
        }
        _ => false,
    }
}

fn cache_ttl(value: Option<&Value>) -> CacheTtl {
    let ttl = value
        .and_then(|v| v.get("ttl"))
        .and_then(|v| v.as_str())
        .unwrap_or("5m");
    if ttl == "1h" {
        CacheTtl::Ephemeral1h
    } else {
        CacheTtl::Ephemeral5m
    }
}

fn cache_control_is_global(value: Option<&Value>) -> bool {
    value
        .and_then(|control| control.get("scope"))
        .and_then(Value::as_str)
        == Some("global")
}

async fn cache_entry_match(
    req: &MessagesRequest,
    breakpoint: &CacheBreakpoint,
    token_context: &PrefixTokenContext,
    aws_b40_compat: bool,
    candidate_tokens: &mut HashMap<PrefixPosition, i32>,
) -> Option<CacheReadMatch> {
    if !breakpoint.readable {
        return None;
    }
    let redis_key = breakpoint.key.redis_key();
    let exact_hit = crate::cluster_cache::global().exists(&redis_key).await;
    tracing::debug!(
        model = %req.model,
        key = %redis_key,
        tokens = breakpoint.tokens,
        exact_hit,
        "checked prompt-cache registry entry"
    );
    if exact_hit {
        return Some(CacheReadMatch {
            tokens: breakpoint.tokens,
        });
    }

    for candidate in &breakpoint.read_candidates {
        let key = candidate.keys.for_ttl(breakpoint.ttl);
        let redis_key = key.redis_key();
        if !crate::cluster_cache::global().exists(&redis_key).await {
            continue;
        }

        // Tokenization is the expensive part. Candidate keys are cheap to
        // probe, so calculate the exact prefix tokens only for a real hit.
        let tokens = if candidate.tokens > 0 {
            candidate.tokens
        } else if let Some(tokens) = candidate_tokens.get(&candidate.position).copied() {
            tokens
        } else {
            let Some(tokens) = calibrated_prefix_tokens_at(
                req,
                &token_context.tools,
                &token_context.system_segments,
                &token_context.content_segments,
                candidate.position,
                aws_b40_compat,
            ) else {
                tracing::error!(
                    ?candidate.position,
                    "cache candidate contained an invalid prefix position"
                );
                continue;
            };
            candidate_tokens.insert(candidate.position, tokens);
            tokens
        };
        return Some(CacheReadMatch { tokens });
    }

    breakpoint.warm_on_first_use.then_some(CacheReadMatch {
        tokens: breakpoint.tokens,
    })
}

async fn register_cache_key(key: CacheKey, ttl: CacheTtl) {
    let redis_key = key.redis_key();
    tracing::debug!(key = %redis_key, ?ttl, "registering prompt-cache registry entry");
    crate::cluster_cache::global()
        .register(&redis_key, ttl.duration())
        .await;
}

fn seeded_cache_hasher(model: &str, ttl: CacheTtl) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update(b"\n");
    hasher.update(ttl.cache_key_label());
    hasher.update(b"\n");
    hasher
}

fn finalize_cache_key(hasher: &Sha256) -> CacheKey {
    CacheKey(hasher.clone().finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector_cache_prompt() -> String {
        let seed = concat!(
            "一句话概况下Python装饰器原理：装饰器本质是在不修改原函数定义的情况下，用高阶函数或可调用对象包装原函数，从而在调用前后注入额外逻辑。\n\n",
            "### 固定编程助手规则（长上下文缓存探针片段）\n",
            "你是一个专业的全栈编程助手，精通Python、JavaScript、Java、Go、C++等主流编程语言，熟悉前端、后端、数据库、云计算等全栈技术栈。\n",
            "Python核心知识点：变量定义、数据类型、运算符、流程控制、异常处理、函数定义、lambda、位置参数、关键字参数、默认参数、可变参数、返回值、作用域、闭包、装饰器、生成器、迭代器、上下文管理器。\n",
            "面向对象：类定义、对象实例化、实例属性、类属性、私有属性、实例方法、类方法、静态方法、继承、多继承、MRO、多态、封装、抽象类、魔法方法。\n",
            "JavaScript核心知识点：var、let、const、原始类型、引用类型、闭包、this、原型链、Promise、async/await、fetch、ES6模块、Set、Map、可选链、空值合并。\n",
            "数据库核心知识点：MySQL数据类型、DDL、DML、DQL、索引、事务、锁机制、执行计划、慢查询日志、PostgreSQL JSONB、MongoDB文档模型、聚合管道。\n",
            "缓存探针保持规则：这些文本是固定上下文的一部分。后续完全相同请求应命中 Claude prompt cache，并在 usage.cache_read_input_tokens 中体现。\n"
        );
        let mut prompt = String::new();
        let mut segment = 1;
        while prompt.chars().count() < 170_000 {
            prompt.push_str(&format!(
                "\n\n===== CACHE PROBE FIXED PROGRAMMING RULE SEGMENT {segment} =====\n{seed}"
            ));
            segment += 1;
        }
        prompt
    }

    fn detector_terminal_cache_request() -> (String, MessagesRequest) {
        let prompt = detector_cache_prompt();
        let request = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "metadata": {"user_id": "checkhub-cache-probe-session"},
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": prompt,
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }));
        (prompt, request)
    }

    #[tokio::test]
    async fn terminal_cache_usage_preserves_prefix_identity() {
        let (prompt, request) = detector_terminal_cache_request();
        let total = super::super::bedrock::calibrated_input_tokens(
            &request,
            super::super::compat::estimate_input_tokens(&request),
        );
        let build = build_cache_breakpoints(&request, total, true);
        let prefix_tokens = build.breakpoints.last().expect("cache breakpoint").tokens;
        let usage = compute_request_usage_breakdown_with_profile(total, &request, true).await;
        let cached = usage
            .cache_read_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);

        assert_eq!(prompt.chars().count(), 170_020);
        assert_eq!(prefix_tokens, total);
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(cached, total.saturating_sub(1));
        assert_eq!(usage.total(), total);
    }

    #[tokio::test]
    async fn long_request_without_cache_control_stays_flat() {
        let prompt = detector_cache_prompt();
        let request = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": prompt}]
        }));
        let total = super::super::bedrock::calibrated_input_tokens(
            &request,
            super::super::compat::estimate_input_tokens(&request),
        );
        let usage = compute_request_usage_breakdown_with_profile(total, &request, true).await;

        assert_eq!(usage, UsageBreakdown::flat(total));
        assert_eq!(usage.total(), total);
    }

    #[tokio::test]
    async fn nonterminal_public_breakpoint_does_not_claim_an_unforwarded_cache() {
        let prompt = detector_cache_prompt();
        let request = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": prompt,
                        "cache_control": {"type": "ephemeral"}
                    },
                    {
                        "type": "text",
                        "text": "Analyze the cached program and return a concrete implementation plan."
                    }
                ]
            }]
        }));
        let total = super::super::bedrock::calibrated_input_tokens(
            &request,
            super::super::compat::estimate_input_tokens(&request),
        );
        let usage = compute_request_usage_breakdown_with_profile(total, &request, true).await;

        assert_eq!(usage, UsageBreakdown::flat(total));
    }

    fn legacy_cache_key(model: &str, parts: &[String], ttl: CacheTtl) -> CacheKey {
        let mut hasher = Sha256::new();
        hasher.update(cache_model_identity(model).as_bytes());
        hasher.update(b"\n");
        hasher.update(format!("{ttl:?}").as_bytes());
        hasher.update(b"\n");
        hasher.update(parts.join(KEY_PART_SEPARATOR).as_bytes());
        CacheKey(hasher.finalize().into())
    }

    fn reset_prefix_tokenization_calls() {
        PREFIX_TOKENIZATION_CALLS.with(|calls| calls.set(0));
    }

    fn prefix_tokenization_calls() -> usize {
        PREFIX_TOKENIZATION_CALLS.with(std::cell::Cell::get)
    }

    #[test]
    fn rolling_cache_keys_match_legacy_redis_keys() {
        let model = "claude-opus-4-8";
        let parts = vec![
            "tool:{\"name\":\"search\"}".to_string(),
            "system:you are a helpful assistant".to_string(),
            "user:first question".to_string(),
            "assistant:first answer".to_string(),
            "user:second question".to_string(),
        ];

        let mut state = PrefixState::new(model);
        let mut prefix = Vec::new();
        for part in &parts {
            state.push_key_part(part);
            prefix.push(part.clone());
            let keys = state.cache_keys();
            assert_eq!(
                keys.ephemeral_5m,
                legacy_cache_key(model, &prefix, CacheTtl::Ephemeral5m)
            );
            assert_eq!(
                keys.ephemeral_1h,
                legacy_cache_key(model, &prefix, CacheTtl::Ephemeral1h)
            );
        }

        assert_eq!(
            state.cache_keys().ephemeral_5m.redis_key(),
            "krcc:f2be10850bce280622ea3be9ea061f1c6493718f31a4e00f1fd7b96ebb19bdc0"
        );
        assert_eq!(
            state.cache_keys().ephemeral_1h.redis_key(),
            "krcc:b7aa71573952fded3d1b39e8861ec990e5cc5bbc2aeb95b2536d0bebcbd99775"
        );
    }

    #[test]
    fn rolling_cache_keys_preserve_empty_and_special_parts() {
        let model = "claude-opus-4-8";
        let mut state = PrefixState::new(model);
        assert_eq!(
            state.cache_keys().ephemeral_5m.redis_key(),
            "krcc:508d538b114afd01a5a07b94201336991415f0bd8fbbf9f05fb64b169e801e7f"
        );
        assert_eq!(
            state.cache_keys().ephemeral_1h.redis_key(),
            "krcc:0ab227d62441c7299e07f180e714c732faf46d39aa6d11d20451632d4adff4a2"
        );

        let parts = ["", "x", KEY_PART_SEPARATOR, "中文\0suffix"];
        let mut prefix = Vec::new();
        for part in parts {
            state.push_key_part(part);
            prefix.push(part.to_string());
            let keys = state.cache_keys();
            assert_eq!(
                keys.ephemeral_5m,
                legacy_cache_key(model, &prefix, CacheTtl::Ephemeral5m)
            );
            assert_eq!(
                keys.ephemeral_1h,
                legacy_cache_key(model, &prefix, CacheTtl::Ephemeral1h)
            );
        }
    }

    #[test]
    fn different_prefixes_produce_different_cache_keys() {
        let mut a = PrefixState::new("claude-opus-4-8");
        a.push_key_part("user:hello");
        let mut b = PrefixState::new("claude-opus-4-8");
        b.push_key_part("user:hello!");
        assert_ne!(a.cache_keys().ephemeral_5m, b.cache_keys().ephemeral_5m);

        let mut c = PrefixState::new("claude-opus-4-8");
        c.push_key_part("user:hello");
        assert_eq!(a.cache_keys().ephemeral_5m, c.cache_keys().ephemeral_5m);
        assert_ne!(a.cache_keys().ephemeral_5m, a.cache_keys().ephemeral_1h);
    }

    #[test]
    fn unsupported_model_names_keep_separate_cache_namespaces() {
        let mut first = PrefixState::new("future-model-alpha");
        first.push_key_part("user:same prompt");
        let mut second = PrefixState::new("future-model-alpha-thinking");
        second.push_key_part("user:same prompt");

        assert!(super::super::converter::map_model("future-model-alpha").is_none());
        assert!(super::super::converter::map_model("future-model-alpha-thinking").is_none());
        assert_ne!(
            first.cache_keys().ephemeral_5m,
            second.cache_keys().ephemeral_5m,
            "unmapped names must not be guessed to be aliases of one model"
        );
    }

    #[test]
    fn read_candidate_window_keeps_current_plus_twenty_previous() {
        let mut state = PrefixState::new("claude-opus-4-8");
        for index in 1..=30 {
            state.push_key_part(&format!("user:{index}"));
            state
                .content_segments
                .push(Value::String(index.to_string()));
            remember_read_candidate(&mut state);
        }
        assert_eq!(state.read_candidates.len(), MAX_READ_CANDIDATES + 1);

        let mut breakpoints = Vec::new();
        push_breakpoint(
            &state,
            &mut breakpoints,
            CacheTtl::Ephemeral5m,
            CacheBreakpointOptions {
                readable: true,
                warm_on_first_use: false,
            },
        );
        let positions = breakpoints[0]
            .read_candidates
            .iter()
            .map(|candidate| candidate.position.content_count)
            .collect::<Vec<_>>();
        assert_eq!(positions, (10..=29).rev().collect::<Vec<_>>());
    }

    #[test]
    fn candidate_collection_does_not_retokenize_every_prefix() {
        let messages = (0..100)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("message {index}")
                })
            })
            .collect::<Vec<_>>();

        let without_cache = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "messages": messages
        }));
        reset_prefix_tokenization_calls();
        let build = build_cache_breakpoints(&without_cache, 100_000, true);
        assert!(build.breakpoints.is_empty());
        assert_eq!(prefix_tokenization_calls(), 0);

        let automatic = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "cache_control": {"type": "ephemeral"},
            "messages": without_cache.messages
        }));
        reset_prefix_tokenization_calls();
        let build = build_cache_breakpoints(&automatic, 100_000, true);
        assert_eq!(build.breakpoints.len(), 1);
        assert_eq!(
            prefix_tokenization_calls(),
            0,
            "converter-derived layouts reuse their precomputed canonical wire weights"
        );
    }

    /// 回归：cache_creation 是本请求的真子集，不得超过请求总量。
    ///
    /// 修复前 aws-b 上前缀走本地估算、总量走上游 context 上报，两套口径不一致时
    /// cache_creation 会凭空放大（实测约 +28%），撞上 1M 上下文钳制后被记成
    /// 999_999，单请求约 $13.8 直接进客户账单。
    #[tokio::test]
    async fn cache_creation_never_exceeds_request_total() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(500);
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{"type": "text", "text": text,
                        "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}]
        }));
        // 蓄意传入偏小的总量，模拟"前缀口径 > 总量口径"的真实情形
        for total in [1_i32, 64, 1_024, 5_000] {
            for aws_b40_compat in [true, false] {
                let u =
                    compute_request_usage_breakdown_with_profile(total, &req, aws_b40_compat).await;
                assert!(
                    u.cache_creation_input_tokens <= total,
                    "cache_creation {} 超过请求总量 {}（aws_b40_compat={}）",
                    u.cache_creation_input_tokens,
                    total,
                    aws_b40_compat
                );
                assert_eq!(
                    u.cache_creation_5m_input_tokens + u.cache_creation_1h_input_tokens,
                    u.cache_creation_input_tokens,
                    "5m + 1h 必须恒等于总量（aws_b40_compat={}）",
                    aws_b40_compat
                );
                assert!(
                    u.cache_creation_5m_input_tokens >= 0 && u.cache_creation_1h_input_tokens >= 0
                );
            }
        }
    }

    #[tokio::test]
    async fn cache_read_and_creation_share_the_current_request_budget() {
        let text = "shared-cache-budget-regression ".repeat(2_000);
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral", "scope": "global"}
            }],
            "messages": [{"role": "user", "content": "continue"}]
        }));

        for total in [1_i32, 64, 1_024, 5_000] {
            let usage = compute_request_usage_breakdown_with_profile(total, &req, true).await;
            assert_eq!(
                usage.total(),
                total,
                "cache parts must share one request-total budget"
            );
            assert!(
                usage
                    .cache_read_input_tokens
                    .saturating_add(usage.cache_creation_input_tokens)
                    <= total
            );
            assert_eq!(
                usage
                    .cache_creation_5m_input_tokens
                    .saturating_add(usage.cache_creation_1h_input_tokens),
                usage.cache_creation_input_tokens
            );
        }
    }

    /// 边缘情况：极小请求带 cache_control，不得报出巨额缓存写。
    #[tokio::test]
    async fn tiny_request_with_cache_control_reports_tiny_cache_creation() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{"type": "text", "text": "hi",
                        "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}]
        }));
        for aws_b40_compat in [true, false] {
            let u = compute_request_usage_breakdown_with_profile(8, &req, aws_b40_compat).await;
            assert!(u.cache_creation_input_tokens <= 8);
        }
    }

    /// 缩放函数本身的边界：零预算、超预算、比例保持、恒等式。
    #[test]
    fn clamp_cache_creation_edge_cases() {
        assert_eq!(
            clamp_cache_creation(100, 50, 1_000),
            (100, 50),
            "未超预算时原样返回"
        );
        assert_eq!(clamp_cache_creation(0, 0, 0), (0, 0), "全零安全");
        assert_eq!(
            clamp_cache_creation(500, 500, 0),
            (0, 0),
            "零预算不得保留缓存写入"
        );
        let (a, b) = clamp_cache_creation(800, 200, 100);
        assert_eq!(a + b, 100, "缩放后两档之和必须恰为预算");
        assert_eq!(b, 20, "按 1h 原占比 20% 缩放");
        let (a, b) = clamp_cache_creation(999_999, 0, 1_000);
        assert_eq!((a, b), (1_000, 0), "单档超限时全部归该档");
        let (a, b) = clamp_cache_creation(-5, -5, 100);
        assert_eq!((a, b), (0, 0), "负值归零");
    }

    #[test]
    fn cache_creation_zero_budget_is_empty_and_positive_budget_preserves_ratio() {
        assert_eq!(clamp_cache_creation(500, 500, 0), (0, 0));
        assert_eq!(clamp_cache_creation(100, 50, 1_000), (100, 50));
        assert_eq!(clamp_cache_creation(800, 200, 100), (80, 20));
        assert_eq!(clamp_cache_creation(999_999, 0, 1_000), (1_000, 0));
        assert_eq!(clamp_cache_creation(-5, -5, 100), (0, 0));
    }

    #[test]
    fn invalid_prefix_position_is_rejected_without_panicking() {
        let req = parse_request(serde_json::json!({}));
        assert_eq!(
            calibrated_prefix_tokens_at(
                &req,
                &[],
                &[],
                &[],
                PrefixPosition {
                    tool_count: 1,
                    system_count: 0,
                    content_count: 0,
                },
                true,
            ),
            None
        );
    }

    #[tokio::test]
    async fn more_than_four_breakpoints_fail_closed_without_tokenization() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "messages": [{
                "role": "user",
                "content": (0..5).map(|index| serde_json::json!({
                    "type": "text",
                    "text": format!("breakpoint {index}"),
                    "cache_control": {"type": "ephemeral"}
                })).collect::<Vec<_>>()
            }]
        }));

        reset_prefix_tokenization_calls();
        assert!(cache_plan_for_request(10_000, &req, true).await.is_none());
        assert_eq!(prefix_tokenization_calls(), 0);

        // Direct callers fail closed as well if they bypass HTTP preflight.
        let build = build_cache_breakpoints(&req, 10_000, true);
        assert!(build.breakpoints.is_empty());
        assert_eq!(prefix_tokenization_calls(), 0);
    }

    #[test]
    fn clamp_leaves_normal_usage_untouched() {
        let usage = UsageBreakdown {
            input_tokens: 12_000,
            cache_read_input_tokens: 32_295,
            cache_creation_input_tokens: 8_272,
            cache_creation_5m_input_tokens: 8_272,
            cache_creation_1h_input_tokens: 0,
        };
        assert_eq!(usage.clamp_for_model("claude-opus-4-8"), usage);
        assert_eq!(usage.clamp_for_model("claude-haiku-4-5"), usage);
    }

    #[test]
    fn clamp_holds_incident_scale_usage_instead_of_charging_the_window_limit() {
        // 2026-07-25 上游故障期间真实上报的 usage：三项分量各自都远超 1M 上下文窗口。
        let inflated = UsageBreakdown {
            input_tokens: 5_122_021,
            cache_read_input_tokens: 2_508_305,
            cache_creation_input_tokens: 13_078_753,
            cache_creation_5m_input_tokens: 182_611,
            cache_creation_1h_input_tokens: 0,
        };
        let clamped = inflated.clamp_for_model("claude-opus-4-8");
        assert_eq!(clamped, UsageBreakdown::flat(1));
    }

    #[test]
    fn clamp_holds_exact_incident_sentinel_even_when_it_fits_the_window() {
        let cases = [
            UsageBreakdown::flat(999_999),
            UsageBreakdown {
                input_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 999_999,
                cache_creation_5m_input_tokens: 999_999,
                cache_creation_1h_input_tokens: 0,
            },
            UsageBreakdown {
                input_tokens: 1,
                cache_read_input_tokens: 999_999,
                cache_creation_input_tokens: 0,
                cache_creation_5m_input_tokens: 0,
                cache_creation_1h_input_tokens: 0,
            },
            UsageBreakdown {
                input_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 999_998,
                cache_creation_5m_input_tokens: 999_998,
                cache_creation_1h_input_tokens: 0,
            },
        ];
        let models = [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-haiku-4-5",
        ];

        for model in models {
            for usage in cases {
                assert_eq!(
                    usage.clamp_for_model(model),
                    UsageBreakdown::flat(1),
                    "{model} exposed incident sentinel usage: {usage:?}"
                );
            }
        }
    }

    #[test]
    fn clamp_keeps_cache_creation_identity() {
        // 总量有值但分量全为 0：把总量整体记为 5m，恒等式必须成立。
        let usage = UsageBreakdown {
            input_tokens: 10,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 5_000,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let clamped = usage.clamp_for_model("claude-opus-4-8");
        assert_eq!(clamped.cache_creation_input_tokens, 5_000);
        assert_eq!(clamped.cache_creation_5m_input_tokens, 5_000);
    }

    #[test]
    fn clamp_respects_smaller_context_window() {
        let inflated = UsageBreakdown {
            input_tokens: 900_000,
            cache_read_input_tokens: 900_000,
            cache_creation_input_tokens: 900_000,
            cache_creation_5m_input_tokens: 900_000,
            cache_creation_1h_input_tokens: 0,
        };
        // haiku 走 200K 窗口。
        let clamped = inflated.clamp_for_model("claude-haiku-4-5");
        assert_eq!(clamped, UsageBreakdown::flat(1));
    }

    #[test]
    fn finalizer_holds_cached_estimate_without_authoritative_context() {
        let estimated = UsageBreakdown {
            input_tokens: 7,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 522_472,
            cache_creation_5m_input_tokens: 522_472,
            cache_creation_1h_input_tokens: 0,
        };

        let usage = finalize_request_usage(
            estimated,
            None,
            estimated.total(),
            &[],
            0,
            "claude-opus-4-8",
            true,
        );

        assert_eq!(
            usage,
            UsageBreakdown::flat(1),
            "a plausible in-window virtual-cache estimate is still not authoritative"
        );
    }

    #[test]
    fn finalizer_does_not_authorize_catalog_sized_cache_without_context_event() {
        let locally_calibrated_catalog = UsageBreakdown {
            input_tokens: 77,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 34_250,
            cache_creation_5m_input_tokens: 34_250,
            cache_creation_1h_input_tokens: 0,
        };

        let usage = finalize_request_usage(
            locally_calibrated_catalog,
            None,
            locally_calibrated_catalog.total(),
            &[],
            0,
            "claude-opus-4-8",
            true,
        );

        assert_eq!(
            usage,
            UsageBreakdown::flat(1),
            "a client-reproducible catalog shape cannot authorize upstream cache billing"
        );
    }

    #[test]
    fn finalizer_validates_each_continuation_without_clamping_legitimate_aggregate() {
        let initial = UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 699_900,
            cache_creation_5m_input_tokens: 699_900,
            cache_creation_1h_input_tokens: 0,
        };
        let usage = finalize_request_usage(
            initial,
            Some(700_000),
            initial.total(),
            &[600_000, 500_000],
            0,
            "claude-opus-4-8",
            true,
        );

        assert_eq!(usage.total(), 1_800_000);
        assert_eq!(usage.cache_creation_input_tokens, 699_900);

        let with_impossible_round = finalize_request_usage(
            initial,
            Some(700_000),
            initial.total(),
            &[1_200_000],
            0,
            "claude-opus-4-8",
            true,
        );
        assert_eq!(
            with_impossible_round.total(),
            700_001,
            "only the impossible continuation round is held"
        );

        let impossible_initial = finalize_request_usage(
            UsageBreakdown::flat(1_200_000),
            Some(1_200_000),
            1_200_000,
            &[100_000],
            0,
            "claude-opus-4-8",
            true,
        );
        assert_eq!(
            impossible_initial.total(),
            100_001,
            "an impossible first round is held without erasing a valid continuation"
        );
    }

    #[test]
    fn reconciliation_never_pours_context_delta_into_cached_prefix() {
        let initial = UsageBreakdown {
            input_tokens: 230,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 8272,
            cache_creation_5m_input_tokens: 8272,
            cache_creation_1h_input_tokens: 0,
        };
        let out = reconcile_initial_input(initial, 15_499, -17);

        assert_eq!(out.input_tokens, 213);
        assert_eq!(out.cache_creation_input_tokens, 8272);
        assert_eq!(out.cache_creation_5m_input_tokens, 8272);
        assert_eq!(out.total(), 8485);
    }

    #[test]
    fn reconciliation_preserves_exact_cache_kind_and_ttl_counts() {
        let initial = UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 300,
            cache_creation_input_tokens: 600,
            cache_creation_5m_input_tokens: 400,
            cache_creation_1h_input_tokens: 200,
        };
        let out = reconcile_initial_input(initial, 1900, 0);

        assert_eq!(out.input_tokens, 100);
        assert_eq!(out.cache_read_input_tokens, 300);
        assert_eq!(out.cache_creation_input_tokens, 600);
        assert_eq!(out.cache_creation_5m_input_tokens, 400);
        assert_eq!(out.cache_creation_1h_input_tokens, 200);
        assert_eq!(out.total(), 1000);
    }

    #[test]
    fn identical_prefix_is_stable_across_context_usage_jitter() {
        let initial = UsageBreakdown {
            input_tokens: 10,
            cache_read_input_tokens: 146_077,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };

        let observed = [146_396, 146_459, 146_665, 146_512]
            .map(|context_total| reconcile_initial_input(initial, context_total, 0));
        assert!(observed.iter().all(|usage| *usage == observed[0]));
        assert_eq!(observed[0].cache_read_input_tokens, 146_077);
    }

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
        // progress = (12000 - 4000) / (20000 - 4000) = 0.5
        // read hit ratio = 10% + (45% - 10%) * 0.5 = 27.5%
        // CC = floor(12000 * 0.15) = 1800
        // CR = floor((12000 - 1800) * 0.275) = 2805
        assert_eq!(b.cache_creation_input_tokens, 1800);
        assert_eq!(b.cache_read_input_tokens, 2805);
        assert_eq!(b.input_tokens, 7395);
    }

    #[test]
    fn large_context_starts_high_cache_read() {
        let b = compute_usage_breakdown(20_000, true);
        // 20k 是高缓存平滑过渡起点：15% creation，剩余部分 45% read。
        assert_eq!(b.cache_creation_input_tokens, 3000);
        assert_eq!(b.cache_read_input_tokens, 7650);
        assert_eq!(b.input_tokens, 9350);
    }

    #[test]
    fn strong_context_reaches_high_cache_read() {
        let b = compute_usage_breakdown(50_000, true);
        // 50k 达到强缓存起点：13% creation，剩余部分 80% read。
        assert_eq!(b.cache_creation_input_tokens, 6500);
        assert_eq!(b.cache_read_input_tokens, 34800);
        assert_eq!(b.input_tokens, 8700);
    }

    #[test]
    fn very_large_context_caps_cache_read_and_reduces_creation() {
        let b = compute_usage_breakdown(100_000, true);
        // 100k 后达到上限：10% creation，剩余部分 90% read。
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
    fn split_uses_progressive_read_hit_rates() {
        assert_eq!(read_hit_rate(split_virtual_cache(3999)), 0.0);
        assert!((read_hit_rate(split_virtual_cache(4000)) - 0.10).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(12_000)) - 0.275).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(20_000)) - 0.45).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(50_000)) - 0.80).abs() <= 0.01);
        assert!((read_hit_rate(split_virtual_cache(100_000)) - 0.90).abs() <= 0.01);
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
            10_000
        );
    }

    fn read_hit_rate(b: UsageBreakdown) -> f64 {
        let read_or_input = b.cache_read_input_tokens + b.input_tokens;
        if read_or_input == 0 {
            return 0.0;
        }
        b.cache_read_input_tokens as f64 / read_or_input as f64
    }

    /// 临时基准：观察 build_cache_breakpoints 随消息条数的扩展性。
    /// O(N²) 时耗时按 N² 增长；修好后应接近线性。
    #[test]
    #[ignore]
    fn bench_build_cache_breakpoints_scaling() {
        use std::time::Instant;
        let tools: Vec<serde_json::Value> = (0..12)
            .map(|i| {
                serde_json::json!({
                    "name": format!("tool_{i}"),
                    "description": "Does something useful. ".repeat(40),
                    "input_schema": {"type":"object","properties":{
                        "p0":{"type":"string","description":"param ".repeat(20)},
                        "p1":{"type":"string","description":"param ".repeat(20)},
                        "p2":{"type":"string","description":"param ".repeat(20)},
                        "p3":{"type":"string","description":"param ".repeat(20)}}}
                })
            })
            .collect();
        println!("\n  N_msgs   elapsed_ms   ms_per_msg");
        for n in [10usize, 20, 40, 80] {
            let msgs: Vec<serde_json::Value> = (0..n)
                .map(|i| {
                    serde_json::json!({
                        "role": if i % 2 == 0 { "user" } else { "assistant" },
                        "content": "Here is some code context. ".repeat(120)
                    })
                })
                .collect();
            let req = parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [{"type":"text","text":"You are helpful. ".repeat(800),
                            "cache_control":{"type":"ephemeral"}}],
                "tools": tools,
                "messages": msgs
            }));
            let t0 = Instant::now();
            let iters = 3;
            for _ in 0..iters {
                let bps = build_cache_breakpoints(&req, 100_000, true);
                std::hint::black_box(&bps);
            }
            let ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
            println!("  {:>6}   {:>10.2}   {:>10.4}", n, ms, ms / n as f64);
        }
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

    async fn commit_successful_request(
        total_input_tokens: i32,
        req: &MessagesRequest,
        aws_b40_compat: bool,
    ) {
        prepare_cache_commit(total_input_tokens, req, aws_b40_compat)
            .commit()
            .await;
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
    fn null_message_cache_control_is_absent_everywhere() {
        let req = parse_request(serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "plain question",
                    "cache_control": null
                }]
            }]
        }));

        assert!(!request_has_cache_control(&req));
        assert_eq!(request_cache_control_count(&req), 0);
        assert!(
            build_cache_breakpoints(&req, 10_000, true)
                .breakpoints
                .is_empty()
        );
    }

    fn automatic_cache_key(req: &MessagesRequest) -> CacheKey {
        let build = build_cache_breakpoints(req, 1_000_000, true);
        assert_eq!(build.breakpoints.len(), 1);
        build.breakpoints[0].key
    }

    fn system_cache_key_for_layout(model: &str, aws_b40_compat: bool) -> CacheKey {
        let req = parse_request(serde_json::json!({
            "model": model,
            "system": [{
                "type": "text",
                "text": "stable cache namespace test",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "same request"}]
        }));
        let build = build_cache_breakpoints(&req, 10_000, aws_b40_compat);
        assert_eq!(build.breakpoints.len(), 1);
        build.breakpoints[0].key
    }

    #[test]
    fn opus_5_aliases_share_canonical_registry_keys_in_both_layouts() {
        for aws_b40_compat in [false, true] {
            let canonical = system_cache_key_for_layout("claude-opus-5", aws_b40_compat);
            for alias in ["claude-opus-5-20260725", "claude-opus-5-thinking"] {
                assert_eq!(
                    canonical,
                    system_cache_key_for_layout(alias, aws_b40_compat),
                    "{alias} must share the canonical Opus 5 cache namespace"
                );
            }
        }
    }

    #[test]
    fn distinct_upstream_models_never_share_registry_keys() {
        for aws_b40_compat in [false, true] {
            assert_ne!(
                system_cache_key_for_layout("claude-opus-4-8", aws_b40_compat),
                system_cache_key_for_layout("claude-opus-5", aws_b40_compat),
                "Opus 4.8 and Opus 5 are different upstream models"
            );
        }
    }

    #[test]
    fn ignored_thinking_signature_does_not_change_forwarded_prefix_key() {
        let request_with = |signature: &str, thinking: &str| {
            parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "cache_control": {"type": "ephemeral"},
                "messages": [
                    {"role": "user", "content": "solve this"},
                    {"role": "assistant", "content": [{
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature
                    }, {"type": "text", "text": "partial"}]},
                    {"role": "user", "content": "continue"}
                ]
            }))
        };

        let first = request_with("opaque-signature-a", "same reasoning");
        let second = request_with("opaque-signature-b", "same reasoning");
        let changed = request_with("opaque-signature-b", "different reasoning");

        assert_eq!(automatic_cache_key(&first), automatic_cache_key(&second));
        assert_ne!(automatic_cache_key(&first), automatic_cache_key(&changed));
    }

    #[test]
    fn effective_effort_key_uses_official_high_default() {
        let request_with = |effort: Option<&str>| {
            let mut body = serde_json::json!({
                "model": "claude-opus-4-8",
                "cache_control": {"type": "ephemeral"},
                "thinking": {"type": "adaptive"},
                "messages": [{"role": "user", "content": "reason carefully"}]
            });
            if let Some(effort) = effort {
                body["output_config"] = serde_json::json!({"effort": effort});
            }
            parse_request(body)
        };

        let omitted = request_with(None);
        let explicit_high = request_with(Some("high"));
        let medium = request_with(Some("medium"));
        assert_eq!(
            automatic_cache_key(&omitted),
            automatic_cache_key(&explicit_high)
        );
        assert_ne!(automatic_cache_key(&omitted), automatic_cache_key(&medium));
    }

    #[test]
    fn trailing_assistant_prefill_does_not_change_forwarded_cache_identity() {
        let request_with_prefill = |prefill: &str| {
            parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "cache_control": {"type": "ephemeral"},
                "messages": [
                    {"role": "user", "content": "stable user turn"},
                    {"role": "assistant", "content": prefill}
                ]
            }))
        };

        let first = request_with_prefill("discarded prefill one");
        let second = request_with_prefill("a completely different discarded prefill");
        let without_prefill = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "cache_control": {"type": "ephemeral"},
            "messages": [{"role": "user", "content": "stable user turn"}]
        }));

        assert_eq!(automatic_cache_key(&first), automatic_cache_key(&second));
        assert_eq!(
            automatic_cache_key(&first),
            automatic_cache_key(&without_prefill)
        );

        let estimate = |request: &MessagesRequest| {
            let base = super::super::compat::estimate_input_tokens(request);
            super::super::bedrock::calibrated_input_tokens(request, base)
        };
        let first_total = estimate(&first);
        let second_total = estimate(&second);
        let without_prefill_total = estimate(&without_prefill);
        assert_eq!(first_total, second_total);
        assert_eq!(first_total, without_prefill_total);

        let first_build = build_cache_breakpoints(&first, first_total, true);
        let second_build = build_cache_breakpoints(&second, second_total, true);
        let without_build = build_cache_breakpoints(&without_prefill, without_prefill_total, true);
        assert_eq!(first_build.exact_ttl_plan, second_build.exact_ttl_plan);
        assert_eq!(first_build.exact_ttl_plan, without_build.exact_ttl_plan);
    }

    #[test]
    fn dynamic_system_policy_is_part_of_the_forwarded_system_key() {
        let request_with_user = |user: &str| {
            parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [{
                    "type": "text",
                    "text": "stable public system",
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": [{"role": "user", "content": user}]
            }))
        };

        let ordinary = request_with_user("Explain a hash map briefly.");
        let literal_code = request_with_user(
            "Write Rust fn kiro_cache_key(input: &str) and preserve the literal \"Kiro:\" exactly.",
        );
        assert!(!super::super::converter::preserves_private_product_code_content(&ordinary));
        assert!(super::super::converter::preserves_private_product_code_content(&literal_code));

        let ordinary_build = build_cache_breakpoints(&ordinary, 20_000, true);
        let literal_build = build_cache_breakpoints(&literal_code, 20_000, true);
        assert_eq!(ordinary_build.breakpoints.len(), 1);
        assert_eq!(literal_build.breakpoints.len(), 1);
        assert_ne!(
            ordinary_build.breakpoints[0].key, literal_build.breakpoints[0].key,
            "the system breakpoint must fingerprint the actual dynamic Kiro system wire"
        );
    }

    #[test]
    fn synthesized_placeholder_tools_are_part_of_forwarded_system_identity() {
        let request_with_tool = |name: &str| {
            parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [{
                    "type": "text",
                    "text": "stable public system",
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": [
                    {"role": "user", "content": "Use the historical tool."},
                    {"role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": "toolu_history",
                        "name": name,
                        "input": {"value": 1}
                    }]},
                    {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_history",
                        "content": "done"
                    }]}
                ]
            }))
        };

        let first = build_cache_breakpoints(&request_with_tool("history_tool_alpha"), 20_000, true);
        let second = build_cache_breakpoints(&request_with_tool("history_tool_beta"), 20_000, true);
        assert_eq!(first.breakpoints.len(), 1);
        assert_eq!(second.breakpoints.len(), 1);
        assert_ne!(
            first.breakpoints[0].key, second.breakpoints[0].key,
            "placeholder tools are sent before system/messages and must scope the cache key"
        );
    }

    #[test]
    fn image_presence_invalidates_message_keys_but_not_system_keys() {
        let request_with_image = |with_image: bool| {
            let mut final_content = vec![serde_json::json!({"type": "text", "text": "next"})];
            if with_image {
                final_content.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "iVBORw0KGgo="
                    }
                }));
            }
            parse_request(serde_json::json!({
                "model": "claude-opus-4-8",
                "system": [{
                    "type": "text",
                    "text": "stable system",
                    "cache_control": {"type": "ephemeral"}
                }],
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "early message prefix",
                        "cache_control": {"type": "ephemeral"}
                    }]
                }, {"role": "assistant", "content": "ack"}, {
                    "role": "user",
                    "content": final_content
                }]
            }))
        };

        let without = build_cache_breakpoints(&request_with_image(false), 100_000, true);
        let with = build_cache_breakpoints(&request_with_image(true), 100_000, true);
        assert_eq!(without.breakpoints.len(), 2);
        assert_eq!(with.breakpoints.len(), 2);
        assert_eq!(without.breakpoints[0].key, with.breakpoints[0].key);
        assert_ne!(without.breakpoints[1].key, with.breakpoints[1].key);
    }

    #[test]
    fn adaptive_request_with_system_breakpoint_has_cache_control() {
        // Adaptive thinking does not suppress an explicit system cache breakpoint.
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

    #[test]
    fn top_level_cache_mode_consumes_one_of_four_slots() {
        let req = parse_request(serde_json::json!({
            "cache_control": {"type": "ephemeral"},
            "system": [{
                "type": "text",
                "text": "system",
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "one", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}}
                ]
            }],
            "tools": [{
                "name": "calculator",
                "description": "math",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }]
        }));

        assert!(request_has_cache_control(&req));
        assert_eq!(request_cache_control_count(&req), 4);
        assert_eq!(request_cache_breakpoint_slot_count(&req), 5);
        assert!(ExactCacheTtlPlan::for_request(10_000, &req, true).is_empty());
    }

    #[test]
    fn nested_schema_fields_do_not_count_as_cache_breakpoints() {
        let req = parse_request(serde_json::json!({
            "tools": [{
                "name": "configure",
                "description": "configuration",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "cache_control": {"type": "string"},
                        "nested": {
                            "type": "object",
                            "properties": {
                                "cache_control": {"type": "boolean"}
                            }
                        }
                    }
                }
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": {"cache_control": "ordinary payload data"}
                }]
            }]
        }));

        assert_eq!(request_cache_control_count(&req), 0);
    }

    #[tokio::test]
    async fn system_breakpoint_transitions_from_creation_to_read() {
        let text = "stateful-system-cache-unique ".repeat(2_000);
        let req = parse_request(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "system": [{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            }]
        }));

        let first = compute_request_usage_breakdown(20_000, &req).await;
        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(first.cache_read_input_tokens, 0);
        assert_eq!(first.total(), 20_000);
        commit_successful_request(20_000, &req, false).await;

        let second = compute_request_usage_breakdown(20_000, &req).await;
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert!(second.cache_read_input_tokens > 0);
        assert_eq!(second.total(), 20_000);
    }

    #[tokio::test]
    async fn cache_registry_warms_only_after_successful_commit() {
        let req = parse_request(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "system": [{
                "type": "text",
                "text": "transactional-cache-commit-regression ".repeat(2_000),
                "cache_control": {"type": "ephemeral"}
            }]
        }));

        let first = compute_request_usage_breakdown(20_000, &req).await;
        let failed_retry = compute_request_usage_breakdown(20_000, &req).await;
        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(
            failed_retry, first,
            "planning alone must not warm cache state"
        );

        commit_successful_request(20_000, &req, false).await;
        let successful_retry = compute_request_usage_breakdown(20_000, &req).await;
        assert_eq!(successful_retry.cache_creation_input_tokens, 0);
        assert!(successful_retry.cache_read_input_tokens > 0);
    }

    #[test]
    fn exact_metadata_replaces_all_cache_buckets_using_native_ttl_plan() {
        let initial = UsageBreakdown {
            input_tokens: 11,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 100,
            cache_creation_5m_input_tokens: 30,
            cache_creation_1h_input_tokens: 70,
        };
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 1_025,
            output_tokens: 42,
            total_tokens: 1_074,
        };
        let mut plan = ExactCacheTtlPlan::default();
        plan.segments[0] = ExactCacheTtlSegment {
            end_tokens: 700,
            one_hour: true,
        };
        plan.segments[1] = ExactCacheTtlSegment {
            end_tokens: 1_025,
            one_hour: false,
        };
        plan.len = 2;

        let usage =
            UsageBreakdown::from_exact_token_usage_with_ttl_plan(initial, &exact, plan).unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 1_025);
        assert_eq!(usage.cache_creation_5m_input_tokens, 325);
        assert_eq!(usage.cache_creation_1h_input_tokens, 700);
        assert_eq!(usage.total(), 1_032);
    }

    #[test]
    fn native_internal_cache_is_flat_without_public_cache_intent() {
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: 1_000,
            cache_write_input_tokens: 25,
            output_tokens: 42,
            total_tokens: 1_074,
        };

        let usage = UsageBreakdown::from_exact_token_usage(
            UsageBreakdown::flat(exact.total_input_tokens()),
            &exact,
        )
        .expect("valid native token usage");

        assert_eq!(usage, UsageBreakdown::flat(1_032));
    }

    #[test]
    fn only_verified_opus_families_trust_native_cache_buckets() {
        for model in [
            "claude-opus-4-7",
            "claude-opus-4.8",
            "claude-opus-5-thinking",
        ] {
            assert!(
                native_cache_buckets_are_trusted(model),
                "{model} should keep its verified native cache split"
            );
        }

        for model in [
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-opus-4-6",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
        ] {
            assert!(
                !native_cache_buckets_are_trusted(model),
                "{model} should retain deterministic shared-cache accounting"
            );
        }
    }

    #[test]
    fn opus_47_reference_probe_keeps_public_17_token_envelope() {
        let initial = UsageBreakdown::flat(17);
        let exact = TokenUsage {
            uncached_input_tokens: 12,
            output_tokens: 8,
            total_tokens: 20,
            ..TokenUsage::default()
        };

        let native = reconcile_native_usage(
            "claude-opus-4-7",
            initial,
            &exact,
            ExactCacheTtlPlan::default(),
        )
        .expect("native usage is valid");

        assert_eq!(native.aggregate_input_tokens, 17);
        assert_eq!(native.public_cache_usage, Some(UsageBreakdown::flat(17)));
        assert_eq!(native.output_tokens, 8);
    }

    #[test]
    fn sonnet_native_repeated_write_cannot_overwrite_a_local_cache_read() {
        let local_hot = UsageBreakdown {
            input_tokens: 182,
            cache_read_input_tokens: 89_520,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let native_repeated_write = TokenUsage {
            uncached_input_tokens: 182,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 89_520,
            output_tokens: 72,
            total_tokens: 89_774,
        };
        let mut plan = ExactCacheTtlPlan::default();
        plan.segments[0] = ExactCacheTtlSegment {
            end_tokens: 89_520,
            one_hour: false,
        };
        plan.len = 1;

        for model in ["claude-sonnet-4-6", "claude-sonnet-5"] {
            let native = reconcile_native_usage(model, local_hot, &native_repeated_write, plan)
                .expect("native totals are valid");
            assert_eq!(native.aggregate_input_tokens, 89_702);
            assert_eq!(native.output_tokens, 72);
            assert_eq!(
                native.public_cache_usage, None,
                "{model}: the known repeated-write signal must not erase the local read"
            );
        }
    }

    #[test]
    fn verified_opus_keeps_authoritative_native_cache_buckets() {
        let initial = UsageBreakdown {
            input_tokens: 182,
            cache_read_input_tokens: 89_520,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let exact = TokenUsage {
            uncached_input_tokens: 182,
            cache_read_input_tokens: 89_520,
            cache_write_input_tokens: 0,
            output_tokens: 72,
            total_tokens: 89_774,
        };
        let mut plan = ExactCacheTtlPlan::default();
        plan.segments[0] = ExactCacheTtlSegment {
            end_tokens: 89_520,
            one_hour: false,
        };
        plan.len = 1;

        for model in ["claude-opus-4-7", "claude-opus-4-8", "claude-opus-5"] {
            let native = reconcile_native_usage(model, initial, &exact, plan)
                .expect("native totals are valid");
            assert_eq!(
                native.public_cache_usage,
                Some(UsageBreakdown {
                    input_tokens: 182,
                    cache_read_input_tokens: 89_520,
                    cache_creation_input_tokens: 0,
                    cache_creation_5m_input_tokens: 0,
                    cache_creation_1h_input_tokens: 0,
                }),
                "{model}: verified native reads remain authoritative"
            );
        }
    }

    #[test]
    fn exact_plan_ignores_public_breakpoint_not_forwarded_to_kiro() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "public local boundary ".repeat(2_000),
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }, {
                    "type": "text",
                    "text": "terminal uncached suffix"
                }]
            }]
        }));
        let build = build_cache_breakpoints(&req, 20_000, true);
        assert!(build.breakpoints.is_empty());
        assert!(
            build.exact_ttl_plan.is_empty(),
            "a non-terminal block has no native Kiro cachePoint"
        );

        let initial = UsageBreakdown {
            input_tokens: 10,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 10_000,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 10_000,
        };
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: 9_000,
            cache_write_input_tokens: 1_000,
            output_tokens: 2,
            total_tokens: 10_009,
        };
        let usage = UsageBreakdown::from_exact_token_usage_with_ttl_plan(
            initial,
            &exact,
            build.exact_ttl_plan,
        )
        .expect("valid exact metadata");
        assert_eq!(usage, UsageBreakdown::flat(10_007));
    }

    #[test]
    fn cache_commit_contains_only_points_present_on_the_kiro_wire() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": "forwarded system cache ".repeat(2_000),
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "unforwarded intra-message boundary ".repeat(2_000),
                    "cache_control": {"type": "ephemeral"}
                }, {
                    "type": "text",
                    "text": "terminal suffix without a marker"
                }]
            }]
        }));

        let commit = prepare_cache_commit(50_000, &req, true);
        assert_eq!(commit.entries.len(), 1);
        assert_eq!(commit.exact_ttl_plan.len, 1);
        assert!(commit.exact_ttl_plan.segments[0].one_hour);
    }

    #[test]
    fn exact_plan_mirrors_converter_system_and_history_terminal_rules() {
        let ignored = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": "early system",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }, {
                "type": "text",
                "text": "final system without marker"
            }],
            "messages": [
                {"role": "user", "content": [{
                    "type": "text", "text": "first merged user",
                    "cache_control": {"type": "ephemeral"}
                }]},
                {"role": "user", "content": "last merged user"},
                {"role": "assistant", "content": [{
                    "type": "text", "text": "first merged assistant",
                    "cache_control": {"type": "ephemeral"}
                }]},
                {"role": "assistant", "content": "last merged assistant"},
                {"role": "user", "content": "current"}
            ]
        }));
        let ignored_plan = ExactCacheTtlPlan::for_request(50_000, &ignored, true);
        assert!(
            ignored_plan.is_empty(),
            "only the final system item and final message in each merged role group survive conversion"
        );

        let forwarded = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{"type": "text", "text": "early system"}, {
                "type": "text",
                "text": "final system",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [
                {"role": "user", "content": "first merged user"},
                {"role": "user", "content": [{
                    "type": "text", "text": "last merged user",
                    "cache_control": {"type": "ephemeral"}
                }]},
                {"role": "assistant", "content": "assistant"},
                {"role": "user", "content": [{
                    "type": "text", "text": "current terminal",
                    "cache_control": {"type": "ephemeral"}
                }]}
            ]
        }));
        let forwarded_plan = ExactCacheTtlPlan::for_request(50_000, &forwarded, true);
        assert_eq!(forwarded_plan.len, 3);
        assert!(forwarded_plan.segments[0].one_hour);
        assert!(!forwarded_plan.segments[1].one_hour);
        assert!(!forwarded_plan.segments[2].one_hour);
    }

    #[test]
    fn exact_plan_uses_native_automatic_point_when_explicit_block_is_nonterminal() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "local explicit boundary ".repeat(2_000),
                    "cache_control": {"type": "ephemeral"}
                }, {"type": "text", "text": "terminal block"}]
            }]
        }));
        let build = build_cache_breakpoints(&req, 20_000, true);
        assert_eq!(build.breakpoints.len(), 1);
        assert_eq!(build.exact_ttl_plan.len, 1);
        assert!(
            build.exact_ttl_plan.segments[0].one_hour,
            "converter attaches the top-level automatic cachePoint to currentMessage"
        );
        assert_eq!(
            build.exact_ttl_plan.segments[0].end_tokens, build.breakpoints[0].tokens,
            "fallback and exact accounting share the same forwarded point"
        );
    }

    #[test]
    fn automatic_and_explicit_points_share_one_forwarded_layout() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "cache_control": {"type": "ephemeral"},
            "system": [{
                "type": "text",
                "text": "long-lived system prefix",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [{"role": "user", "content": "automatic current-message point"}]
        }));

        let build = build_cache_breakpoints(&req, 20_000, true);
        assert_eq!(build.breakpoints.len(), 2);
        assert_eq!(build.exact_ttl_plan.len, 2);
        assert_eq!(build.breakpoints[0].ttl, CacheTtl::Ephemeral1h);
        assert_eq!(build.breakpoints[1].ttl, CacheTtl::Ephemeral5m);
        assert!(build.exact_ttl_plan.segments[0].one_hour);
        assert!(!build.exact_ttl_plan.segments[1].one_hour);
        assert_eq!(
            build.exact_ttl_plan.segments[0].end_tokens,
            build.breakpoints[0].tokens
        );
        assert_eq!(
            build.exact_ttl_plan.segments[1].end_tokens,
            build.breakpoints[1].tokens
        );
    }

    #[test]
    fn exact_plan_keeps_each_forwarded_tool_cache_point() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "tools": [{
                "name": "first_tool",
                "description": "first",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }, {
                "name": "second_tool",
                "description": "second",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "use a tool"}]
        }));
        let plan = ExactCacheTtlPlan::for_request(20_000, &req, true);
        assert_eq!(plan.len, 2);
        assert!(plan.segments[0].one_hour);
        assert!(!plan.segments[1].one_hour);
        assert!(plan.segments[1].end_tokens >= plan.segments[0].end_tokens);
    }

    #[test]
    fn local_cache_minimum_is_model_specific_and_unknown_fails_closed() {
        assert_eq!(local_cache_min_tokens("claude-opus-5"), Some(512));
        assert_eq!(local_cache_min_tokens("claude-opus-4-8"), Some(1_024));
        assert_eq!(local_cache_min_tokens("claude-sonnet-5"), Some(1_024));
        assert_eq!(local_cache_min_tokens("claude-sonnet-4-6"), Some(1_024));
        assert_eq!(local_cache_min_tokens("claude-sonnet-4-5"), Some(1_024));
        assert_eq!(local_cache_min_tokens("claude-opus-4-7"), Some(2_048));
        assert_eq!(local_cache_min_tokens("claude-opus-4-6"), Some(4_096));
        assert_eq!(local_cache_min_tokens("claude-opus-4-5"), Some(4_096));
        assert_eq!(local_cache_min_tokens("claude-haiku-4-5"), Some(4_096));
        assert_eq!(
            local_cache_min_tokens("claude-opus-4-6-1m"),
            Some(4_096),
            "the 1m route inherits the Opus 4.6 family minimum"
        );
        assert_eq!(
            local_cache_min_tokens("claude-sonnet-4-6-1m"),
            Some(1_024),
            "the 1m route inherits the Sonnet 4.6 family minimum"
        );
        assert_eq!(local_cache_min_tokens("glm-5"), None);
    }

    #[tokio::test]
    async fn local_minimum_does_not_discard_native_exact_plan() {
        let request_for = |model: &str| {
            parse_request(serde_json::json!({
                "model": model,
                "system": [{
                    "type": "text",
                    "text": "cacheable platform prefix ".repeat(2_000),
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }]
            }))
        };

        let opus_48 = request_for("claude-opus-4-8");
        assert!(
            cache_plan_for_request(1_500, &opus_48, true)
                .await
                .is_some()
        );

        let opus_47 = request_for("claude-opus-4-7");
        assert!(
            cache_plan_for_request(1_500, &opus_47, true)
                .await
                .is_none()
        );

        let opus_46 = request_for("claude-opus-4-6");
        assert!(
            cache_plan_for_request(3_000, &opus_46, true)
                .await
                .is_none()
        );
        let commit = prepare_cache_commit(3_000, &opus_46, true);
        assert!(
            commit.entries.is_empty(),
            "sub-minimum local registry stays cold"
        );
        assert_eq!(
            commit.exact_ttl_plan.len, 1,
            "native metadata remains authoritative below the local gate"
        );

        let unknown = request_for("glm-5");
        assert!(
            cache_plan_for_request(50_000, &unknown, true)
                .await
                .is_none()
        );
        let commit = prepare_cache_commit(50_000, &unknown, true);
        assert!(
            commit.entries.is_empty(),
            "unknown models fail closed locally"
        );
        assert_eq!(
            commit.exact_ttl_plan.len, 1,
            "a real native cachePoint is not erased by the local model table"
        );
    }

    #[test]
    fn exact_cache_write_uses_request_ttl_when_local_plan_was_hot() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": "authoritative-one-hour-cache-prefix ".repeat(2_000),
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]
        }));
        let total = 20_000;
        let plan = ExactCacheTtlPlan::for_request(total, &req, true);
        assert_eq!(plan.len, 1);
        assert!(plan.segments[0].one_hour);

        // The local registry believed the whole prefix was hot, so the
        // initial creation split contains no TTL information at all.
        let initial = UsageBreakdown {
            input_tokens: 7,
            cache_read_input_tokens: 12_000,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 12_000,
            output_tokens: 4,
            total_tokens: 12_011,
        };

        let usage = UsageBreakdown::from_exact_token_usage_with_ttl_plan(initial, &exact, plan)
            .expect("valid native usage");
        assert_eq!(usage.cache_creation_input_tokens, 12_000);
        assert_eq!(usage.cache_creation_5m_input_tokens, 0);
        assert_eq!(usage.cache_creation_1h_input_tokens, 12_000);
        assert_eq!(usage.total(), 12_007);

        // The commit carries the same request-derived plan so handlers do not
        // have to rebuild or retokenize the request before moving the commit.
        let commit = prepare_cache_commit(total, &req, true);
        assert_eq!(commit.exact_ttl_plan(), plan);
    }

    #[test]
    fn exact_ttl_plan_survives_the_local_cache_minimum_gate() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": "small authoritative one-hour prefix ".repeat(20),
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]
        }));
        let local_total = 500;
        let plan = prepare_cache_commit(local_total, &req, true).exact_ttl_plan();
        assert_eq!(plan.len, 1);
        assert!(
            plan.segments[0].end_tokens
                < local_cache_min_tokens(&req.model).expect("known model minimum")
        );
        assert!(plan.segments[0].one_hour);

        let initial = UsageBreakdown::flat(local_total);
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 500,
            output_tokens: 0,
            total_tokens: 507,
        };
        let usage = UsageBreakdown::from_exact_token_usage_with_ttl_plan(initial, &exact, plan)
            .expect("native cache write is authoritative");

        assert_eq!(usage.cache_creation_input_tokens, 500);
        assert_eq!(usage.cache_creation_5m_input_tokens, 0);
        assert_eq!(usage.cache_creation_1h_input_tokens, 500);
    }

    #[test]
    fn exact_partial_write_follows_the_ttl_of_the_uncached_suffix() {
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": "durable-prefix ".repeat(2_000),
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "short-lived-suffix ".repeat(2_000),
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }));
        let total = 100_000;
        let plan = ExactCacheTtlPlan::for_request(total, &req, true);
        assert_eq!(plan.len, 2);
        assert!(plan.segments[0].one_hour);
        assert!(!plan.segments[1].one_hour);

        let durable_prefix = plan.segments[0].end_tokens;
        let complete_prefix = plan.segments[1].end_tokens;
        let short_lived_suffix = complete_prefix.saturating_sub(durable_prefix);
        assert!(durable_prefix >= local_cache_min_tokens(&req.model).expect("known model minimum"));
        assert!(short_lived_suffix > 0);
        // Exercise the native/local tokenizer scaling path as well as the TTL
        // boundary itself.
        let exact_read = durable_prefix.saturating_mul(2);
        let exact_write = short_lived_suffix.saturating_mul(2);
        let exact_cached_total = exact_read.saturating_add(exact_write);

        let initial = UsageBreakdown {
            input_tokens: 7,
            cache_read_input_tokens: complete_prefix,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let exact = TokenUsage {
            uncached_input_tokens: 7,
            cache_read_input_tokens: exact_read,
            cache_write_input_tokens: exact_write,
            output_tokens: 0,
            total_tokens: 7i32.saturating_add(exact_cached_total),
        };

        let usage = UsageBreakdown::from_exact_token_usage_with_ttl_plan(initial, &exact, plan)
            .expect("valid mixed-TTL native usage");
        assert_eq!(usage.cache_read_input_tokens, exact_read);
        assert_eq!(usage.cache_creation_input_tokens, exact_write);
        assert_eq!(usage.cache_creation_5m_input_tokens, exact_write);
        assert_eq!(usage.cache_creation_1h_input_tokens, 0);
        assert_eq!(usage.total(), 7i32.saturating_add(exact_cached_total));
    }

    #[test]
    fn malformed_exact_metadata_is_rejected() {
        let initial = UsageBreakdown::flat(10);
        let negative = TokenUsage {
            uncached_input_tokens: -1,
            ..TokenUsage::default()
        };
        assert!(UsageBreakdown::from_exact_token_usage(initial, &negative).is_none());

        let inconsistent_total = TokenUsage {
            uncached_input_tokens: 10,
            output_tokens: 5,
            total_tokens: 12,
            ..TokenUsage::default()
        };
        assert!(UsageBreakdown::from_exact_token_usage(initial, &inconsistent_total).is_none());
    }

    /// 回归：opus-4-6 / 4-7 的大前缀（>4096 tokens）必须和 4-8/5 一样正常缓存。
    ///
    /// 修复前 `cache_read_supported` 门禁让这两个老模型的大前缀既不登记也不读取，
    /// 于是每次请求都全量 cache_creation、cache_read 恒为 0（实测客户为此多付数倍）。
    /// pomoai 跨模型实测报告确认真实上游对 4-6/4-7 大前缀同样正常建缓存+读缓存，
    /// 该门禁纯属本地伪装，已移除。此测试锁定"所有模型行为与 4-8/5 一致"。
    #[tokio::test]
    async fn legacy_opus_large_prefix_caches_like_opus_48() {
        for model in ["claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8"] {
            let text = format!("legacy-opus-large-prefix-{model}-unique ").repeat(2_000);
            let req = parse_request(serde_json::json!({
                "model": model,
                "system": [{
                    "type": "text",
                    "text": text,
                    "cache_control": {"type": "ephemeral"}
                }]
            }));

            let first = compute_request_usage_breakdown(20_000, &req).await;
            assert!(
                first.cache_creation_input_tokens > 4_096,
                "{model}: first call must write the full large prefix, got {:?}",
                first
            );
            assert_eq!(
                first.cache_read_input_tokens, 0,
                "{model}: first call is cold"
            );
            commit_successful_request(20_000, &req, false).await;

            let second = compute_request_usage_breakdown(20_000, &req).await;
            assert_eq!(
                second.cache_creation_input_tokens, 0,
                "{model}: second call must not rewrite, got {:?}",
                second
            );
            assert!(
                second.cache_read_input_tokens > 4_096,
                "{model}: second call must read the full large prefix, got {:?}",
                second
            );
        }
    }

    /// 回归：缓存按模型隔离（pomoai 报告第一条结论）。同一段文本在两个模型上
    /// 各养各的缓存，切换模型第一次必定全量重建，不得跨模型命中。
    #[tokio::test]
    async fn cache_is_model_scoped_no_cross_model_hit() {
        let text = "model-scoped-isolation-shared-text ".repeat(2_000);
        let mk = |model: &str| {
            parse_request(serde_json::json!({
                "model": model,
                "system": [{
                    "type": "text",
                    "text": text.clone(),
                    "cache_control": {"type": "ephemeral"}
                }]
            }))
        };

        // 在 opus-4-6 上建立缓存。
        let warm_request = mk("claude-opus-4-6");
        let warm = compute_request_usage_breakdown(20_000, &warm_request).await;
        assert!(warm.cache_creation_input_tokens > 0);
        commit_successful_request(20_000, &warm_request, false).await;

        // 切到 opus-4-8：同一段文本，第一次必须全量重建（跨模型零命中）。
        let cross = compute_request_usage_breakdown(20_000, &mk("claude-opus-4-8")).await;
        assert_eq!(
            cross.cache_read_input_tokens, 0,
            "cross-model read leaked: {cross:?}"
        );
        assert!(cross.cache_creation_input_tokens > 0);

        // 回到 opus-4-6：自己的原缓存仍在，正常命中。
        let back = compute_request_usage_breakdown(20_000, &mk("claude-opus-4-6")).await;
        assert!(
            back.cache_read_input_tokens > 0,
            "same-model cache lost after other model wrote: {back:?}"
        );
    }

    #[tokio::test]
    async fn one_hour_breakpoint_uses_the_one_hour_creation_bucket() {
        let text = "stateful-one-hour-cache-unique ".repeat(2_000);
        let req = parse_request(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "system": [{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]
        }));

        let first = compute_request_usage_breakdown(20_000, &req).await;
        assert_eq!(first.cache_creation_5m_input_tokens, 0);
        assert!(first.cache_creation_1h_input_tokens > 0);
        assert_eq!(
            first.cache_creation_input_tokens,
            first.cache_creation_1h_input_tokens
        );
    }

    #[tokio::test]
    async fn message_breakpoints_transition_from_creation_to_read() {
        let text = "stateful-message-cache-unique ".repeat(2_000);
        let req = parse_request(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": text,
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }));

        let first = compute_request_usage_breakdown_with_profile(20_000, &req, true).await;
        commit_successful_request(20_000, &req, true).await;
        let second = compute_request_usage_breakdown_with_profile(20_000, &req, true).await;
        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(first.cache_read_input_tokens, 0);
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert!(second.cache_read_input_tokens > 0);
    }

    #[tokio::test]
    async fn sonnet_4_6_image_breakpoint_reads_same_image_and_changed_bytes_miss() {
        use base64::Engine;

        let image_data = |marker: u8| {
            let mut bytes = vec![0u8; 25];
            bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
            bytes[12..16].copy_from_slice(b"IHDR");
            bytes[16..20].copy_from_slice(&1_075u32.to_be_bytes());
            bytes[20..24].copy_from_slice(&1_520u32.to_be_bytes());
            bytes[24] = marker;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let request_with = |data: String| {
            parse_request(serde_json::json!({
                "model": "claude-sonnet-4-6",
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": data
                        },
                        "cache_control": {"type": "ephemeral"}
                    }]
                }]
            }))
        };
        let usage_for = |request: &MessagesRequest| {
            let base = super::super::compat::estimate_input_tokens(request);
            super::super::bedrock::calibrated_input_tokens(request, base)
        };

        let first_request = request_with(image_data(0x41));
        let first_total = usage_for(&first_request);
        let first =
            compute_request_usage_breakdown_with_profile(first_total, &first_request, true).await;
        assert_eq!(first.cache_read_input_tokens, 0);
        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(
            first.cache_creation_input_tokens,
            first
                .cache_creation_5m_input_tokens
                .saturating_add(first.cache_creation_1h_input_tokens)
        );
        assert_eq!(first.total(), first_total);
        commit_successful_request(first_total, &first_request, true).await;

        let second_total = usage_for(&first_request);
        let second =
            compute_request_usage_breakdown_with_profile(second_total, &first_request, true).await;
        assert!(second.cache_read_input_tokens > 0);
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert_eq!(
            second.cache_creation_input_tokens,
            second
                .cache_creation_5m_input_tokens
                .saturating_add(second.cache_creation_1h_input_tokens)
        );
        assert_eq!(second.total(), second_total);

        let changed_request = request_with(image_data(0x42));
        let changed_total = usage_for(&changed_request);
        let changed =
            compute_request_usage_breakdown_with_profile(changed_total, &changed_request, true)
                .await;
        assert_eq!(changed.cache_read_input_tokens, 0);
        assert!(changed.cache_creation_input_tokens > 0);
        assert_eq!(
            changed.cache_creation_input_tokens,
            changed
                .cache_creation_5m_input_tokens
                .saturating_add(changed.cache_creation_1h_input_tokens)
        );
        assert_eq!(changed.total(), changed_total);
    }

    #[tokio::test]
    async fn sonnet_4_6_text_message_breakpoint_transitions_to_read() {
        let request = parse_request(serde_json::json!({
            "model": "claude-sonnet-4-6",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "sonnet text breakpoint remains create only ".repeat(800),
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }));
        let total = super::super::bedrock::calibrated_input_tokens(
            &request,
            super::super::compat::estimate_input_tokens(&request),
        );

        let first = compute_request_usage_breakdown_with_profile(total, &request, true).await;
        commit_successful_request(total, &request, true).await;
        let second = compute_request_usage_breakdown_with_profile(total, &request, true).await;

        assert_eq!(first.cache_read_input_tokens, 0);
        assert!(first.cache_creation_input_tokens > 0);
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert!(second.cache_read_input_tokens > 0);
        assert_eq!(first.total(), total);
        assert_eq!(second.total(), total);
    }

    #[tokio::test]
    async fn bedrock_message_breakpoint_reads_the_previous_turn_prefix() {
        let tools = (0..8)
            .map(|index| {
                serde_json::json!({
                    "name": format!("history_tool_{index}"),
                    "description": "Stable cached tool contract. ".repeat(80),
                    "input_schema": {
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }
                })
            })
            .collect::<Vec<_>>();
        let anchor = "bedrock global cache anchor ".repeat(500);
        let suffix = "stable workspace context: value\n".repeat(500);
        let first_user = "inspect the cached project context carefully ".repeat(400);
        let first = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "tools": tools,
            "system": [
                {
                    "type": "text",
                    "text": anchor,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                },
                {"type": "text", "text": suffix}
            ],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": first_user,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }]
            }]
        }));
        let first_total = super::super::bedrock::calibrated_input_tokens(
            &first,
            super::super::compat::estimate_input_tokens(&first),
        );
        let first_usage =
            compute_request_usage_breakdown_with_profile(first_total, &first, true).await;
        let first_cached = first_usage
            .cache_read_input_tokens
            .saturating_add(first_usage.cache_creation_input_tokens);
        assert_eq!(first_usage.cache_read_input_tokens, 0);
        assert!(first_usage.cache_creation_input_tokens > 0);
        assert_eq!(
            first_usage.input_tokens,
            first_total.saturating_sub(first_cached)
        );
        assert!(first_usage.input_tokens > 0);
        commit_successful_request(first_total, &first, true).await;

        let second = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "tools": first.tools,
            "system": first.system,
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": first_user}]
                },
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_01history",
                        "name": "history_tool_0",
                        "input": {"value": "next"}
                    }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_01history",
                        "content": "completed",
                        "cache_control": {"type": "ephemeral", "ttl": "1h"}
                    }]
                }
            ]
        }));
        let second_total = super::super::bedrock::calibrated_input_tokens(
            &second,
            super::super::compat::estimate_input_tokens(&second),
        );
        let second_usage =
            compute_request_usage_breakdown_with_profile(second_total, &second, true).await;

        assert!(second_usage.cache_read_input_tokens <= first_cached);
        assert!(second_usage.cache_read_input_tokens > 0);
        assert!(second_usage.cache_read_input_tokens <= second_total);
        assert_eq!(
            second_usage.input_tokens,
            second_total
                .saturating_sub(second_usage.cache_read_input_tokens)
                .saturating_sub(second_usage.cache_creation_input_tokens)
        );
        assert!(second_usage.input_tokens > 0);
        assert_eq!(second_usage.total(), second_total);
    }

    #[tokio::test]
    async fn bedrock_cache_profile_calibrates_total_and_prefix_usage() {
        let anchor = (0..900)
            .map(|index| format!("stable cache anchor segment {index}: protocol parity datum."))
            .collect::<Vec<_>>()
            .join(" ");
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "system": [{
                "type": "text",
                "text": anchor,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "Reply exactly CACHE_OK."}]
        }));
        let base = super::super::compat::estimate_input_tokens(&req);
        let total = super::super::bedrock::calibrated_input_tokens(&req, base);
        let usage = compute_request_usage_breakdown_with_profile(total, &req, true).await;

        assert_eq!(total, 18_021);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert!(usage.input_tokens > 0);
        assert!(usage.cache_creation_input_tokens > 0);
        assert_eq!(
            usage.cache_creation_input_tokens,
            total.saturating_sub(usage.input_tokens)
        );
        assert_eq!(
            usage.cache_creation_5m_input_tokens,
            usage.cache_creation_input_tokens
        );
        assert_eq!(usage.cache_creation_1h_input_tokens, 0);
        assert_eq!(usage.total(), total);
    }

    #[tokio::test]
    async fn bedrock_cached_tools_do_not_leak_tool_framing_into_ordinary_input() {
        let tools = (0..28)
            .map(|index| {
                serde_json::json!({
                    "name": format!("cached_tool_{index}"),
                    "description": "A representative cached tool description. ".repeat(64),
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "alpha": {"type": "string"},
                            "beta": {"type": "integer"},
                            "mode": {"type": "string", "enum": ["one", "two", "three"]}
                        },
                        "required": ["alpha"],
                        "additionalProperties": false
                    }
                })
            })
            .collect::<Vec<_>>();
        let req = parse_request(serde_json::json!({
            "model": "claude-opus-4-8",
            "tools": tools,
            "system": [{
                "type": "text",
                "text": "cached tool protocol anchor ".repeat(1_200),
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [{"role": "user", "content": "1+1=?"}]
        }));
        let base = super::super::compat::estimate_input_tokens(&req);
        let total = super::super::bedrock::calibrated_input_tokens(&req, base);
        let usage = compute_request_usage_breakdown_with_profile(total, &req, true).await;

        assert!(
            usage.input_tokens <= 128,
            "unexpected uncached suffix: {usage:?}"
        );
        assert_eq!(usage.total(), total);
        assert!(
            usage.cache_creation_input_tokens + usage.cache_read_input_tokens
                >= total.saturating_sub(128)
        );
    }
}
