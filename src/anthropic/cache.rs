//! Prompt cache usage 兼容策略。
//!
//! Kiro 上游不支持 Anthropic/AWS prompt caching，但客户端会依赖 usage 里的
//! cache 字段计费。本模块按 aws-p 实测行为维护一个本进程内的前缀缓存：
//!
//! - 无 `cache_control`：所有 token 都计入普通 `input_tokens`。
//! - 显式 system/tool breakpoint 或顶层 automatic cache：首轮写入 creation，
//!   5 分钟或 1 小时 TTL 内重复前缀进入 read。
//! - 显式 message content breakpoint：aws-p 实测会写 creation，但后续不读命中；
//!   本地保持同样行为。
//! - `input_tokens` 只展示最后一个 cache breakpoint 后面的非缓存部分。
//! - `cache_creation.ephemeral_5m_input_tokens` 和 `ephemeral_1h_input_tokens`
//!   按每个 breakpoint 的 TTL 分拆。

use crate::anthropic::types::{Message, MessagesRequest, Tool};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

const CACHE_MIN_TOKENS: i32 = 1_024;
// 缓存登记表已迁移到 `crate::cluster_cache`(跨容器共享 + 本地回退)。

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
            .is_some_and(|map| map.contains_key("cache_control")),
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
    let Some(cache_plan) =
        cache_plan_for_request(total_input_tokens, req, aws_b40_compat).await
    else {
        return UsageBreakdown::flat(total_input_tokens);
    };

    let ordinary_input = total_input_tokens - cache_plan.cache_tokens;
    UsageBreakdown {
        input_tokens: ordinary_input,
        cache_read_input_tokens: cache_plan.cache_read_tokens,
        cache_creation_input_tokens: cache_plan.cache_creation_5m_tokens
            + cache_plan.cache_creation_1h_tokens,
        cache_creation_5m_input_tokens: cache_plan.cache_creation_5m_tokens,
        cache_creation_1h_input_tokens: cache_plan.cache_creation_1h_tokens,
    }
}

pub fn with_additional_input(
    initial: UsageBreakdown,
    initial_total_input_tokens: i32,
    final_total_input_tokens: i32,
) -> UsageBreakdown {
    // 初始拆分已经覆盖首轮总量；自动续写产生的后续轮次没有 cache breakpoint，
    // 因此只把新增 input 累加到普通 input，已有 cache_read/cache_creation 保持不变。
    // final 来自本地估算累加（见 billing::billable_input_tokens），不会混入 Kiro 的固定底噪。
    let extra = (final_total_input_tokens - initial_total_input_tokens).max(0);
    UsageBreakdown {
        input_tokens: initial.input_tokens + extra,
        ..initial
    }
}

/// Reconcile the first-round cache split after a profile obtains a more
/// accurate total from the upstream context-usage event. Continuation rounds
/// are handled separately by `with_additional_input` and remain ordinary input.
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
        .clamp(1, calibrated_total_input_tokens);
    let calibrated_cached = calibrated_total_input_tokens.saturating_sub(ordinary_input);
    let cache_read = ((calibrated_cached as i64 * initial.cache_read_input_tokens as i64)
        / initial_cached as i64) as i32;
    let cache_creation = calibrated_cached.saturating_sub(cache_read);
    let initial_creation = initial.cache_creation_input_tokens;
    let cache_creation_1h = if initial_creation > 0 {
        ((cache_creation as i64 * initial.cache_creation_1h_input_tokens as i64)
            / initial_creation as i64) as i32
    } else {
        0
    };
    let cache_creation_5m = cache_creation.saturating_sub(cache_creation_1h);

    UsageBreakdown {
        input_tokens: ordinary_input,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        cache_creation_5m_input_tokens: cache_creation_5m,
        cache_creation_1h_input_tokens: cache_creation_1h,
    }
}

struct CachePlan {
    cache_tokens: i32,
    cache_read_tokens: i32,
    cache_creation_5m_tokens: i32,
    cache_creation_1h_tokens: i32,
}

async fn cache_plan_for_request(
    total_input_tokens: i32,
    req: &MessagesRequest,
    aws_b40_compat: bool,
) -> Option<CachePlan> {
    let mut breakpoints =
        build_cache_breakpoints(req, total_input_tokens, aws_b40_compat);
    breakpoints.retain(|b| b.tokens >= cache_min_tokens(&req.model));
    if breakpoints.is_empty() {
        return None;
    }

    breakpoints.sort_by_key(|b| b.tokens);
    breakpoints.truncate(4);

    let mut read_index: Option<usize> = None;
    for (idx, breakpoint) in breakpoints.iter().enumerate().rev() {
        if cache_entry_exists(&req.model, breakpoint).await {
            read_index = Some(idx);
            break;
        }
    }

    let read_tokens = read_index.map(|idx| breakpoints[idx].tokens).unwrap_or(0);
    // 命中时刷新该前缀 TTL(对齐真 Anthropic 的"每次使用重置缓存有效期"),
    // 否则持续使用的前缀每到 5m/1h 就会冒出一次 cache_creation,破坏"统一号池"的一致观感。
    if let Some(idx) = read_index {
        register_cache_entry(&req.model, &breakpoints[idx]).await;
    }
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
        register_cache_entry(&req.model, breakpoint).await;
        previous = breakpoint.tokens;
    }

    Some(CachePlan {
        cache_tokens: breakpoints.last()?.tokens,
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
}

struct CacheBreakpoint {
    tokens: i32,
    ttl: CacheTtl,
    key_material: String,
    readable: bool,
}

fn build_cache_breakpoints(
    req: &MessagesRequest,
    total_input_tokens: i32,
    aws_b40_compat: bool,
) -> Vec<CacheBreakpoint> {
    let mut state = PrefixState::default();
    let mut breakpoints = Vec::new();

    if let Some(tools) = &req.tools {
        for tool in tools {
            state.tools.push(tool.clone());
            let mut tool_key = serde_json::to_value(tool).unwrap_or(Value::Null);
            strip_cache_control(&mut tool_key);
            state
                .key_parts
                .push(format!("tool:{}", canonical_json(&tool_key)));
            if tool.cache_control.is_some() {
                push_breakpoint(
                    req,
                    &state,
                    &mut breakpoints,
                    cache_ttl(tool.cache_control.as_ref()),
                    true,
                    aws_b40_compat,
                );
            }
        }
    }

    if let Some(system) = &req.system {
        for item in system {
            state.system_segments.push(item.text.clone());
            state.key_parts.push(format!("system:{}", item.text));
            if item.cache_control.is_some() {
                push_breakpoint(
                    req,
                    &state,
                    &mut breakpoints,
                    cache_ttl(item.cache_control.as_ref()),
                    true,
                    aws_b40_compat,
                );
            }
        }
    }

    for message in &req.messages {
        collect_message_prefix(
            req,
            message,
            &mut state,
            &mut breakpoints,
            aws_b40_compat,
        );
    }

    if req.cache_control.is_some() && breakpoints.is_empty() && state.has_cacheable_content() {
        let ttl = cache_ttl(req.cache_control.as_ref());
        push_breakpoint(
            req,
            &state,
            &mut breakpoints,
            ttl,
            true,
            aws_b40_compat,
        );
    }

    for breakpoint in &mut breakpoints {
        breakpoint.tokens = breakpoint.tokens.min(total_input_tokens).max(0);
    }
    breakpoints
}

#[derive(Default)]
struct PrefixState {
    tools: Vec<Tool>,
    system_segments: Vec<String>,
    content_segments: Vec<Value>,
    key_parts: Vec<String>,
}

impl PrefixState {
    fn has_cacheable_content(&self) -> bool {
        !self.tools.is_empty()
            || !self.system_segments.is_empty()
            || !self.content_segments.is_empty()
    }
}

fn collect_message_prefix(
    req: &MessagesRequest,
    message: &Message,
    state: &mut PrefixState,
    breakpoints: &mut Vec<CacheBreakpoint>,
    aws_b40_compat: bool,
) {
    match &message.content {
        Value::String(text) => {
            let content = Value::String(text.clone());
            state.key_parts.push(format!("{}:{}", message.role, text));
            state.content_segments.push(content);
        }
        Value::Array(items) => {
            for item in items {
                let mut item_without_cache = item.clone();
                let ttl = cache_ttl(item_without_cache.get("cache_control"));
                if let Some(obj) = item_without_cache.as_object_mut() {
                    obj.remove("cache_control");
                }
                state.key_parts.push(format!(
                    "{}:{}",
                    message.role,
                    canonical_json(&item_without_cache)
                ));
                state
                    .content_segments
                    .push(Value::Array(vec![item_without_cache]));
                if has_direct_cache_control(item) {
                    push_breakpoint(
                        req,
                        state,
                        breakpoints,
                        ttl,
                        false,
                        aws_b40_compat,
                    );
                }
            }
        }
        other => {
            state
                .key_parts
                .push(format!("{}:{}", message.role, canonical_json(other)));
            state.content_segments.push(other.clone());
        }
    }
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
    req: &MessagesRequest,
    state: &PrefixState,
    breakpoints: &mut Vec<CacheBreakpoint>,
    ttl: CacheTtl,
    readable: bool,
    aws_b40_compat: bool,
) {
    let base_tokens = super::compat::estimate_prefix_tokens(
        &req.model,
        &state.system_segments,
        &state.content_segments,
        &state.tools,
    );
    let tokens = if aws_b40_compat {
        super::bedrock::calibrated_cache_prefix_tokens(
            base_tokens,
            &state.system_segments,
            &state.content_segments,
            !state.tools.is_empty(),
        )
    } else {
        base_tokens
    };
    breakpoints.push(CacheBreakpoint {
        tokens,
        ttl,
        key_material: state.key_parts.join("\n---prefix-block---\n"),
        readable,
    });
}

fn has_direct_cache_control(v: &Value) -> bool {
    v.as_object()
        .map(|map| map.contains_key("cache_control"))
        .unwrap_or(false)
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

async fn cache_entry_exists(model: &str, breakpoint: &CacheBreakpoint) -> bool {
    if !breakpoint.readable {
        return false;
    }
    if !cache_read_supported(model, breakpoint.tokens) {
        return false;
    }
    let key = cache_key(model, &breakpoint.key_material, breakpoint.ttl);
    crate::cluster_cache::global().exists(&key).await
}

async fn register_cache_entry(model: &str, breakpoint: &CacheBreakpoint) {
    if !breakpoint.readable {
        return;
    }
    if !cache_read_supported(model, breakpoint.tokens) {
        return;
    }
    let key = cache_key(model, &breakpoint.key_material, breakpoint.ttl);
    crate::cluster_cache::global()
        .register(&key, breakpoint.ttl.duration())
        .await;
}

fn cache_min_tokens(model: &str) -> i32 {
    let lower = model.to_ascii_lowercase();
    if lower.contains("opus") && (lower.contains("4-6") || lower.contains("4.6")) {
        4_096
    } else if lower.contains("opus") && (lower.contains("4-7") || lower.contains("4.7")) {
        2_048
    } else {
        CACHE_MIN_TOKENS
    }
}

fn cache_read_supported(model: &str, cache_tokens: i32) -> bool {
    let lower = model.to_ascii_lowercase();
    !(lower.contains("opus") && cache_tokens > 4_096)
}

fn cache_key(model: &str, key_material: &str, ttl: CacheTtl) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update(b"\n");
    hasher.update(format!("{ttl:?}").as_bytes());
    hasher.update(b"\n");
    hasher.update(key_material.as_bytes());
    // 命名空间前缀:即使与别的应用共享同一个真 Redis,也不会与其 key 冲突,
    // 且便于批量识别/清理(SCAN krcc:* )。
    format!("krcc:{}", hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_additional_input_preserves_cache_and_bills_continuation_rounds() {
        // 缓存命中：input=100, cache_read=3954, total=4054。
        let cached = UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 3954,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        // final=8000 表示本地估算累计了后续轮次；新增的 3946 只进入普通 input。
        let out = with_additional_input(cached, 4054, 8000);
        assert_eq!(out.input_tokens, 4046);
        assert_eq!(out.cache_read_input_tokens, 3954);
        assert_eq!(out.total(), 8000, "input+cr+cc 必须覆盖所有上游轮次");
    }

    #[test]
    fn with_additional_input_accumulates_flat_multiround() {
        // 无缓存：多轮累计的额外 input 叠加进来（成本回收）。final 来自本地估算累加。
        let flat = UsageBreakdown::flat(2000);
        let out = with_additional_input(flat, 2000, 9000);
        assert_eq!(out.input_tokens, 9000);
        assert_eq!(out.cache_read_input_tokens, 0);
        assert_eq!(out.cache_creation_input_tokens, 0);
    }

    #[test]
    fn reconciles_profile_delta_into_cached_prefix() {
        let initial = UsageBreakdown {
            input_tokens: 230,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 8272,
            cache_creation_5m_input_tokens: 8272,
            cache_creation_1h_input_tokens: 0,
        };
        let out = reconcile_initial_input(initial, 15_499, -17);

        assert_eq!(out.input_tokens, 213);
        assert_eq!(out.cache_creation_input_tokens, 15_286);
        assert_eq!(out.cache_creation_5m_input_tokens, 15_286);
        assert_eq!(out.total(), 15_499);
    }

    #[test]
    fn reconciliation_preserves_cache_kind_and_ttl_ratios() {
        let initial = UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 300,
            cache_creation_input_tokens: 600,
            cache_creation_5m_input_tokens: 400,
            cache_creation_1h_input_tokens: 200,
        };
        let out = reconcile_initial_input(initial, 1900, 0);

        assert_eq!(out.input_tokens, 100);
        assert_eq!(out.cache_read_input_tokens, 600);
        assert_eq!(out.cache_creation_input_tokens, 1200);
        assert_eq!(out.cache_creation_5m_input_tokens, 800);
        assert_eq!(out.cache_creation_1h_input_tokens, 400);
        assert_eq!(out.total(), 1900);
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

    #[test]
    fn top_level_cache_mode_does_not_consume_a_bedrock_breakpoint() {
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

        let second = compute_request_usage_breakdown(20_000, &req).await;
        assert_eq!(second.cache_creation_input_tokens, 0);
        assert!(second.cache_read_input_tokens > 0);
        assert_eq!(second.total(), 20_000);
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
    async fn message_breakpoints_create_but_do_not_report_read_hits() {
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

        let first = compute_request_usage_breakdown(20_000, &req).await;
        let second = compute_request_usage_breakdown(20_000, &req).await;
        assert!(first.cache_creation_input_tokens > 0);
        assert!(second.cache_creation_input_tokens > 0);
        assert_eq!(first.cache_read_input_tokens, 0);
        assert_eq!(second.cache_read_input_tokens, 0);
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
        assert_eq!(usage.input_tokens, 18);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 18_003);
        assert_eq!(usage.cache_creation_5m_input_tokens, 18_003);
        assert_eq!(usage.cache_creation_1h_input_tokens, 0);
        assert_eq!(usage.total(), total);
    }
}
