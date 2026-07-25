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

    /// 把 usage 钳制到物理可能的范围内，供所有对外出口在发出前调用。
    ///
    /// 单个请求的 input + cache_read + cache_creation 不可能超过模型上下文窗口。
    /// 一旦超过，只可能来自多轮累计、上游异常重连的重复计量，或本地估算把二进制
    /// 内容（例如 tool_result 里内嵌的 base64 截图）当作文本计数造成的放大。
    /// 下游网关按 usage 逐 token 计费，放大值会直接变成客户账单，因此必须在出口
    /// 处兜底：宁可少计，不可多计。
    ///
    /// 同时强制 `cache_creation == 5m + 1h`。二者失配时，下游会把 message_start
    /// 的分量和 message_delta 的总量混用（`compat::stream_delta_usage` 不带分量
    /// 子字段），得到远超真实值的缓存写入量。
    pub fn clamp_to_context_window(self, context_window_tokens: i32) -> Self {
        let limit = context_window_tokens.max(1);

        let mut cache_creation_5m = self.cache_creation_5m_input_tokens.max(0);
        let mut cache_creation_1h = self.cache_creation_1h_input_tokens.max(0);
        let mut cache_creation = self.cache_creation_input_tokens.max(0);
        if cache_creation_5m.saturating_add(cache_creation_1h) != cache_creation {
            if cache_creation_5m > 0 || cache_creation_1h > 0 {
                cache_creation = cache_creation_5m.saturating_add(cache_creation_1h);
            } else {
                cache_creation_5m = cache_creation;
            }
        }

        let mut cache_read = self.cache_read_input_tokens.max(0);
        let mut input = self.input_tokens.max(0);

        let within_limit = input
            .saturating_add(cache_read)
            .saturating_add(cache_creation)
            <= limit;
        if within_limit {
            return Self {
                input_tokens: input,
                cache_read_input_tokens: cache_read,
                cache_creation_input_tokens: cache_creation,
                cache_creation_5m_input_tokens: cache_creation_5m,
                cache_creation_1h_input_tokens: cache_creation_1h,
            };
        }

        // 超限：按可信度从高到低依次分配预算（缓存写入 → 缓存读取 → 普通 input），
        // 并始终给 input 留至少 1 个 token，与真 Anthropic 的 usage 形态一致。
        let mut remaining = limit;
        cache_creation = cache_creation.min(remaining.saturating_sub(1));
        cache_creation_1h = cache_creation_1h.min(cache_creation);
        cache_creation_5m = cache_creation - cache_creation_1h;
        remaining -= cache_creation;
        cache_read = cache_read.min(remaining.saturating_sub(1));
        remaining -= cache_read;
        input = input.clamp(1, remaining.max(1));

        tracing::warn!(
            limit,
            original_input = self.input_tokens,
            original_cache_read = self.cache_read_input_tokens,
            original_cache_creation = self.cache_creation_input_tokens,
            clamped_input = input,
            clamped_cache_read = cache_read,
            clamped_cache_creation = cache_creation,
            "usage 超过模型上下文窗口，已钳制后再上报（避免下游超额计费）"
        );

        Self {
            input_tokens: input,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            cache_creation_5m_input_tokens: cache_creation_5m,
            cache_creation_1h_input_tokens: cache_creation_1h,
        }
    }

    /// 按模型自身的上下文窗口钳制（opus-4.8 / sonnet-4.6 等为 1M，其余 200K）。
    pub fn clamp_for_model(self, model: &str) -> Self {
        self.clamp_to_context_window(super::converter::get_context_window_size(model))
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
    let Some(cache_plan) = cache_plan_for_request(total_input_tokens, req, aws_b40_compat).await
    else {
        return UsageBreakdown::flat(total_input_tokens);
    };

    let ordinary_input = if aws_b40_compat && cache_plan.terminal_message_breakpoint {
        2
    } else if aws_b40_compat {
        total_input_tokens
            .saturating_sub(cache_plan.cache_tokens)
            .max(1)
    } else {
        total_input_tokens - cache_plan.cache_tokens
    };
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
    terminal_message_breakpoint: bool,
}

async fn cache_plan_for_request(
    total_input_tokens: i32,
    req: &MessagesRequest,
    aws_b40_compat: bool,
) -> Option<CachePlan> {
    let mut breakpoints = build_cache_breakpoints(req, total_input_tokens, aws_b40_compat);
    breakpoints.retain(|b| b.tokens >= cache_min_tokens(&req.model));
    if breakpoints.is_empty() {
        return None;
    }

    breakpoints.sort_by_key(|b| b.tokens);
    breakpoints.truncate(4);

    let mut read_match: Option<CacheReadMatch> = None;
    for breakpoint in breakpoints.iter().rev() {
        if let Some(candidate) = cache_entry_match(&req.model, breakpoint).await
            && read_match
                .as_ref()
                .is_none_or(|current| candidate.tokens > current.tokens)
        {
            read_match = Some(candidate);
        }
    }

    let terminal_breakpoint = breakpoints.last()?;
    let max_cache_tokens = terminal_breakpoint.tokens;
    let terminal_message_breakpoint = terminal_breakpoint.message_breakpoint;
    let read_tokens = read_match
        .as_ref()
        .map(|candidate| candidate.tokens.min(max_cache_tokens))
        .unwrap_or(0);
    // 命中时刷新该前缀 TTL(对齐真 Anthropic 的"每次使用重置缓存有效期"),
    // 否则持续使用的前缀每到 5m/1h 就会冒出一次 cache_creation,破坏"统一号池"的一致观感。
    if let Some(candidate) = read_match {
        register_cache_material(&req.model, &candidate.key_material, candidate.ttl).await;
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
        cache_tokens: max_cache_tokens,
        cache_read_tokens: read_tokens,
        cache_creation_5m_tokens: creation_5m,
        cache_creation_1h_tokens: creation_1h,
        terminal_message_breakpoint,
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

#[derive(Clone)]
struct CacheReadCandidate {
    tokens: i32,
    key_material: String,
}

struct CacheReadMatch {
    tokens: i32,
    key_material: String,
    ttl: CacheTtl,
}

#[derive(Clone, Copy)]
struct CacheBreakpointOptions {
    readable: bool,
    warm_on_first_use: bool,
    message_breakpoint: bool,
}

struct CacheBreakpoint {
    tokens: i32,
    ttl: CacheTtl,
    key_material: String,
    readable: bool,
    read_candidates: Vec<CacheReadCandidate>,
    warm_on_first_use: bool,
    message_breakpoint: bool,
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
            state.push_key_part(&format!("tool:{}", canonical_json(&tool_key)));
            if tool.cache_control.is_some() {
                let warm_on_first_use =
                    aws_b40_compat && cache_control_is_global(tool.cache_control.as_ref());
                push_breakpoint(
                    req,
                    &state,
                    &mut breakpoints,
                    cache_ttl(tool.cache_control.as_ref()),
                    aws_b40_compat,
                    CacheBreakpointOptions {
                        readable: true,
                        warm_on_first_use,
                        message_breakpoint: false,
                    },
                );
            }
        }
    }

    if let Some(system) = &req.system {
        for item in system {
            state.system_segments.push(item.text.clone());
            state.push_key_part(&format!("system:{}", item.text));
            if item.cache_control.is_some() {
                let warm_on_first_use =
                    aws_b40_compat && cache_control_is_global(item.cache_control.as_ref());
                push_breakpoint(
                    req,
                    &state,
                    &mut breakpoints,
                    cache_ttl(item.cache_control.as_ref()),
                    aws_b40_compat,
                    CacheBreakpointOptions {
                        readable: true,
                        warm_on_first_use,
                        message_breakpoint: false,
                    },
                );
            }
        }
    }

    for message in &req.messages {
        collect_message_prefix(req, message, &mut state, &mut breakpoints, aws_b40_compat);
    }

    if req.cache_control.is_some() && breakpoints.is_empty() && state.has_cacheable_content() {
        let ttl = cache_ttl(req.cache_control.as_ref());
        let warm_on_first_use =
            aws_b40_compat && cache_control_is_global(req.cache_control.as_ref());
        push_breakpoint(
            req,
            &state,
            &mut breakpoints,
            ttl,
            aws_b40_compat,
            CacheBreakpointOptions {
                readable: true,
                warm_on_first_use,
                message_breakpoint: false,
            },
        );
    }

    for breakpoint in &mut breakpoints {
        breakpoint.tokens = if aws_b40_compat {
            breakpoint.tokens.max(0)
        } else {
            breakpoint.tokens.min(total_input_tokens).max(0)
        };
    }
    breakpoints
}

/// 前缀 key 各块之间的分隔符（保持与历史实现一致的字节序列）。
const KEY_PART_SEPARATOR: &str = "\n---prefix-block---\n";

#[derive(Default)]
struct PrefixState {
    tools: Vec<Tool>,
    system_segments: Vec<String>,
    content_segments: Vec<Value>,
    /// 前缀 key 的滚动摘要。
    ///
    /// 这里以前是 `key_parts: Vec<String>`：每追加一个块，
    /// `remember_read_candidate()` 都要 `key_parts.join(SEP)` 把整个前缀
    /// 重新拼成一个大 String。它对每条消息/每个 content block 调用一次，
    /// 于是 N 个块累计复制 1+2+…+N 份数据 —— O(N²)。长对话（几十条消息、
    /// 上百 KB）下这会吃掉大量 CPU，并且这些大 String 还全部留在
    /// `read_candidates` 里，导致进程 RSS 涨到 GB 级。
    ///
    /// `key_material` 只有两种用途：喂给 `cache_key()` 做 SHA-256，或与
    /// 另一个 key_material 做相等比较；从不按原文读取。因此这里改为增量
    /// 更新摘要——`key_material()` 的结果等价于 `sha256(parts.join(SEP))`，
    /// 唯一性与相等语义完全不变，但每个块只做一次 O(块大小) 的 update，
    /// 整体降到 O(总字节数) 单遍。
    key_digest: Sha256,
    key_part_count: usize,
    read_candidates: Vec<CacheReadCandidate>,
}

impl PrefixState {
    fn has_cacheable_content(&self) -> bool {
        !self.tools.is_empty()
            || !self.system_segments.is_empty()
            || !self.content_segments.is_empty()
    }

    /// 追加一个前缀块（等价于以前的 `key_parts.push(..)`）。
    fn push_key_part(&mut self, part: &str) {
        if self.key_part_count > 0 {
            self.key_digest.update(KEY_PART_SEPARATOR.as_bytes());
        }
        self.key_digest.update(part.as_bytes());
        self.key_part_count += 1;
    }

    /// 当前前缀的 key material（等价于以前的 `key_parts.join(SEP)`，
    /// 只是以定长摘要代替原文）。克隆 hasher 只复制固定大小的内部状态。
    fn key_material(&self) -> String {
        hex::encode(self.key_digest.clone().finalize())
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
            state.push_key_part(&format!("{}:{}", message.role, text));
            state.content_segments.push(content);
            remember_read_candidate(req, state, aws_b40_compat);
        }
        Value::Array(items) => {
            for item in items {
                let mut item_without_cache = item.clone();
                let ttl = cache_ttl(item_without_cache.get("cache_control"));
                if let Some(obj) = item_without_cache.as_object_mut() {
                    obj.remove("cache_control");
                }
                state.push_key_part(&format!(
                    "{}:{}",
                    message.role,
                    canonical_json(&item_without_cache)
                ));
                state
                    .content_segments
                    .push(Value::Array(vec![item_without_cache]));
                remember_read_candidate(req, state, aws_b40_compat);
                if has_direct_cache_control(item) {
                    let readable = aws_b40_compat && super::compat::is_opus_4_8(&req.model);
                    let warm_on_first_use =
                        readable && cache_control_is_global(item.get("cache_control"));
                    push_breakpoint(
                        req,
                        state,
                        breakpoints,
                        ttl,
                        aws_b40_compat,
                        CacheBreakpointOptions {
                            readable,
                            warm_on_first_use,
                            message_breakpoint: true,
                        },
                    );
                }
            }
        }
        other => {
            state.push_key_part(&format!("{}:{}", message.role, canonical_json(other)));
            state.content_segments.push(other.clone());
            remember_read_candidate(req, state, aws_b40_compat);
        }
    }
}

fn remember_read_candidate(req: &MessagesRequest, state: &mut PrefixState, aws_b40_compat: bool) {
    let tokens = calibrated_prefix_tokens(req, state, aws_b40_compat);
    state.read_candidates.push(CacheReadCandidate {
        tokens,
        key_material: state.key_material(),
    });
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
    aws_b40_compat: bool,
    options: CacheBreakpointOptions,
) {
    let tokens = calibrated_prefix_tokens(req, state, aws_b40_compat);
    let current_key = state.key_material();
    let read_candidates = if options.readable {
        state
            .read_candidates
            .iter()
            .rev()
            .skip_while(|candidate| candidate.key_material == current_key)
            .take(20)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    breakpoints.push(CacheBreakpoint {
        tokens,
        ttl,
        key_material: current_key,
        readable: options.readable,
        read_candidates,
        warm_on_first_use: options.warm_on_first_use,
        message_breakpoint: options.message_breakpoint,
    });
}

fn calibrated_prefix_tokens(
    req: &MessagesRequest,
    state: &PrefixState,
    aws_b40_compat: bool,
) -> i32 {
    let base_tokens = super::compat::estimate_prefix_tokens(
        &req.model,
        &state.system_segments,
        &state.content_segments,
        &state.tools,
    );
    if aws_b40_compat {
        super::bedrock::calibrated_cache_prefix_tokens(
            &req.model,
            base_tokens,
            &state.system_segments,
            &state.content_segments,
            &state.tools,
        )
    } else {
        base_tokens
    }
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

fn cache_control_is_global(value: Option<&Value>) -> bool {
    value
        .and_then(|control| control.get("scope"))
        .and_then(Value::as_str)
        == Some("global")
}

async fn cache_entry_match(model: &str, breakpoint: &CacheBreakpoint) -> Option<CacheReadMatch> {
    if !breakpoint.readable {
        return None;
    }
    if !cache_read_supported(model, breakpoint.tokens) {
        return None;
    }
    let key = cache_key(model, &breakpoint.key_material, breakpoint.ttl);
    if crate::cluster_cache::global().exists(&key).await {
        return Some(CacheReadMatch {
            tokens: breakpoint.tokens,
            key_material: breakpoint.key_material.clone(),
            ttl: breakpoint.ttl,
        });
    }

    for candidate in &breakpoint.read_candidates {
        if !cache_read_supported(model, candidate.tokens) {
            continue;
        }
        let key = cache_key(model, &candidate.key_material, breakpoint.ttl);
        if crate::cluster_cache::global().exists(&key).await {
            return Some(CacheReadMatch {
                tokens: candidate.tokens,
                key_material: candidate.key_material.clone(),
                ttl: breakpoint.ttl,
            });
        }
    }

    breakpoint.warm_on_first_use.then(|| CacheReadMatch {
        tokens: breakpoint.tokens,
        key_material: breakpoint.key_material.clone(),
        ttl: breakpoint.ttl,
    })
}

async fn register_cache_entry(model: &str, breakpoint: &CacheBreakpoint) {
    if !breakpoint.readable {
        return;
    }
    if !cache_read_supported(model, breakpoint.tokens) {
        return;
    }
    register_cache_material(model, &breakpoint.key_material, breakpoint.ttl).await;
}

async fn register_cache_material(model: &str, key_material: &str, ttl: CacheTtl) {
    let key = cache_key(model, key_material, ttl);
    crate::cluster_cache::global()
        .register(&key, ttl.duration())
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
    let legacy_opus = lower.contains("opus")
        && (lower.contains("4-6")
            || lower.contains("4.6")
            || lower.contains("4-7")
            || lower.contains("4.7"));
    !(legacy_opus && cache_tokens > 4_096)
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

    /// 前缀 key 的增量摘要必须与历史实现 `key_parts.join(SEP)` 的哈希逐字节等价。
    ///
    /// 历史实现每追加一个块都重新 join 整个前缀（O(N²)，长对话下 CPU 涨 7 倍、
    /// RSS 涨到 GB 级）。改成滚动摘要后语义必须保持不变——本测试锁定这一点，
    /// 同时防止有人改坏分隔符或改回逐块拼接。
    #[test]
    fn rolling_key_digest_matches_joined_prefix_hash() {
        let parts = vec![
            "tool:{\"name\":\"search\"}".to_string(),
            "system:you are a helpful assistant".to_string(),
            "user:first question".to_string(),
            "assistant:first answer".to_string(),
            "user:second question".to_string(),
        ];

        let mut state = PrefixState::default();
        for part in &parts {
            state.push_key_part(part);
        }

        let expected = {
            let joined = parts.join(KEY_PART_SEPARATOR);
            let mut hasher = Sha256::new();
            hasher.update(joined.as_bytes());
            hex::encode(hasher.finalize())
        };

        assert_eq!(state.key_material(), expected);
    }

    /// 不同的前缀必须得到不同的 key material（相等比较语义在第 748 行被依赖）。
    #[test]
    fn different_prefixes_produce_different_key_material() {
        let mut a = PrefixState::default();
        a.push_key_part("user:hello");
        let mut b = PrefixState::default();
        b.push_key_part("user:hello!");
        assert_ne!(a.key_material(), b.key_material());

        // 前缀相同时必须相等（增量读候选匹配依赖这个）
        let mut c = PrefixState::default();
        c.push_key_part("user:hello");
        assert_eq!(a.key_material(), c.key_material());
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
    fn clamp_caps_incident_scale_usage_to_context_window() {
        // 2026-07-25 上游故障期间真实上报的 usage：三项分量各自都远超 1M 上下文窗口。
        let inflated = UsageBreakdown {
            input_tokens: 5_122_021,
            cache_read_input_tokens: 2_508_305,
            cache_creation_input_tokens: 13_078_753,
            cache_creation_5m_input_tokens: 182_611,
            cache_creation_1h_input_tokens: 0,
        };
        let clamped = inflated.clamp_for_model("claude-opus-4-8");
        assert!(
            clamped.input_tokens
                + clamped.cache_read_input_tokens
                + clamped.cache_creation_input_tokens
                <= 1_000_000
        );
        assert!(clamped.input_tokens >= 1);
        // 分量失配时以 5m/1h 分量为准重建总量，避免下游混用两个事件的数值。
        assert_eq!(clamped.cache_creation_input_tokens, 182_611);
        assert_eq!(
            clamped.cache_creation_5m_input_tokens + clamped.cache_creation_1h_input_tokens,
            clamped.cache_creation_input_tokens
        );
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
        assert!(
            clamped.input_tokens
                + clamped.cache_read_input_tokens
                + clamped.cache_creation_input_tokens
                <= 200_000
        );
        assert!(clamped.input_tokens >= 1);
    }

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
    fn opus_48_supports_large_prompt_cache_reads() {
        assert!(cache_read_supported("claude-opus-4-8", 11_184));
        assert!(!cache_read_supported("claude-opus-4-7", 11_184));
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
            .map(|i| serde_json::json!({
                "name": format!("tool_{i}"),
                "description": "Does something useful. ".repeat(40),
                "input_schema": {"type":"object","properties":{
                    "p0":{"type":"string","description":"param ".repeat(20)},
                    "p1":{"type":"string","description":"param ".repeat(20)},
                    "p2":{"type":"string","description":"param ".repeat(20)},
                    "p3":{"type":"string","description":"param ".repeat(20)}}}
            }))
            .collect();
        println!("\n  N_msgs   elapsed_ms   ms_per_msg");
        for n in [10usize, 20, 40, 80] {
            let msgs: Vec<serde_json::Value> = (0..n)
                .map(|i| serde_json::json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": "Here is some code context. ".repeat(120)
                }))
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
                    "cache_control": {"type": "ephemeral", "ttl": "1h", "scope": "global"}
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
        assert!(first_usage.cache_read_input_tokens > 0);
        assert!(first_usage.cache_creation_input_tokens > 0);
        assert_eq!(first_usage.input_tokens, 2);

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

        assert_eq!(second_usage.cache_read_input_tokens, first_cached);
        assert!(second_usage.cache_creation_input_tokens > 0);
        assert_eq!(second_usage.input_tokens, 2);
        assert!(second_usage.cache_creation_input_tokens < first_usage.cache_creation_input_tokens);
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
