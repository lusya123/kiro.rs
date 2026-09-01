//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use serde_json::json;

use crate::kiro::model::events::Event;

use super::id;

const AUTO_CONTINUE_COMPLETE_SENTINEL: &str = "__KRS_CONTINUATION_COMPLETE__";
const AWS_B_TEXT_DELTA_TARGET_CHARS: usize = 8;
const AWS_B_TEXT_DELTA_MAX_PARTS: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForcedToolInputRepair {
    pub tool_name: String,
    pub input: serde_json::Value,
}

pub(crate) fn normalize_stop_reason_for_completed_tool_use<'a>(
    stop_reason: &'a str,
    has_completed_tool_use: bool,
) -> &'a str {
    if has_completed_tool_use && stop_reason == "end_turn" {
        "tool_use"
    } else {
        stop_reason
    }
}

pub fn merge_continuation_text(previous: &str, incoming: &str) -> String {
    if previous.is_empty() || incoming.is_empty() {
        return incoming.to_string();
    }

    let max_overlap = previous.len().min(incoming.len()).min(4096);
    for overlap in (1..=max_overlap).rev() {
        let previous_start = previous.len() - overlap;
        if previous.is_char_boundary(previous_start)
            && incoming.is_char_boundary(overlap)
            && previous[previous_start..] == incoming[..overlap]
        {
            return incoming[overlap..].to_string();
        }
    }

    if needs_numeric_line_separator(previous, incoming) {
        return format!("\n{incoming}");
    }

    incoming.to_string()
}

fn needs_numeric_line_separator(previous: &str, incoming: &str) -> bool {
    let previous_trimmed = previous.trim_end_matches([' ', '\t']);
    if previous_trimmed.ends_with('\n') || previous_trimmed.ends_with('\r') {
        return false;
    }

    let previous_line = previous_trimmed
        .rsplit_once(['\n', '\r'])
        .map(|(_, line)| line.trim())
        .unwrap_or_else(|| previous_trimmed.trim());
    if previous_line.is_empty() || !previous_line.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }

    let incoming_first = incoming.trim_start_matches([' ', '\t']);
    let incoming_digits: String = incoming_first
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if incoming_digits.is_empty() {
        return false;
    }

    match (previous_line.parse::<u64>(), incoming_digits.parse::<u64>()) {
        (Ok(prev), Ok(next)) => next == prev + 1,
        _ => false,
    }
}

fn content_tail(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut start = content.len() - max_bytes;
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    content[start..].to_string()
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn text_delta_chunks(text: &str) -> Vec<&str> {
    let char_count = text.chars().count();
    if char_count <= AWS_B_TEXT_DELTA_TARGET_CHARS {
        return vec![text];
    }

    let chars_per_chunk = AWS_B_TEXT_DELTA_TARGET_CHARS.max(
        char_count.saturating_add(AWS_B_TEXT_DELTA_MAX_PARTS - 1) / AWS_B_TEXT_DELTA_MAX_PARTS,
    );
    let mut chunks =
        Vec::with_capacity(char_count.saturating_add(chars_per_chunk - 1) / chars_per_chunk);
    let mut chunk_start = 0;
    let mut chars_in_chunk = 0;

    for (byte_index, _) in text.char_indices() {
        if chars_in_chunk == chars_per_chunk {
            chunks.push(&text[chunk_start..byte_index]);
            chunk_start = byte_index;
            chars_in_chunk = 0;
        }
        chars_in_chunk += 1;
    }
    chunks.push(&text[chunk_start..]);
    chunks
}

fn earliest_stop_sequence(text: &str, sequences: &[String]) -> Option<(usize, usize)> {
    sequences
        .iter()
        .enumerate()
        .filter(|(_, sequence)| !sequence.is_empty())
        .filter_map(|(sequence_index, sequence)| {
            text.find(sequence)
                .map(|byte_index| (byte_index, sequence_index))
        })
        .min_by_key(|(byte_index, sequence_index)| (*byte_index, *sequence_index))
}

fn trailing_stop_sequence_prefix_len(text: &str, sequences: &[String]) -> usize {
    text.char_indices()
        .map(|(byte_index, _)| byte_index)
        .chain(std::iter::once(text.len()))
        .filter_map(|byte_index| {
            let suffix = &text[byte_index..];
            (!suffix.is_empty()
                && sequences
                    .iter()
                    .any(|sequence| sequence.starts_with(suffix)))
            .then_some(suffix.len())
        })
        .max()
        .unwrap_or(0)
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        // 如果标签后面内容不足以判断是否有双换行符，等待更多内容
        if after_content.len() < 2 {
            return None;
        }

        // 真正的 thinking 结束标签后面会有双换行符 `\n\n`
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// Return the byte index before a one- or two-backtick suffix that may become a
/// triple-backtick fence when the next upstream chunk arrives.
fn split_before_trailing_fence_prefix(text: &str) -> usize {
    if text.ends_with("``") {
        text.len() - 2
    } else if text.ends_with('`') {
        text.len() - 1
    } else {
        text.len()
    }
}

/// 查找完整非流式文本中的 thinking 结束标签。
///
/// 非流式响应已经拿到了完整文本，因此 `</thinking>\nvisible text` 也可以安全
/// 识别为结束标签；流式路径仍保留更严格的双换行/缓冲区末尾判断。
fn find_real_thinking_end_tag_in_complete_text(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        let after_content = &buffer[after_pos..];
        if after_content.starts_with('\n') || after_content.starts_with("\r\n") {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

fn could_still_start_with_thinking_tag(buffer: &str) -> bool {
    const TAG: &str = "<thinking>";
    let candidate = buffer.trim_start_matches(char::is_whitespace);
    candidate.is_empty() || TAG.starts_with(candidate)
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..end_pos],
            &after_open[end_pos + "</thinking>\n\n".len()..],
        )
    } else if let Some(end_pos) = find_real_thinking_end_tag_in_complete_text(after_open) {
        let after_tag = end_pos + "</thinking>".len();
        (&after_open[..end_pos], after_open[after_tag..].trim_start())
    } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        let after_tag = end_pos + "</thinking>".len();
        (&after_open[..end_pos], after_open[after_tag..].trim_start())
    } else {
        // 找不到有效的结束标签，不做提取
        return (None, text.to_string());
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

fn is_renderable_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !ch.is_control()
        && !matches!(
            ch,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

fn has_renderable_text(text: &str) -> bool {
    text.chars().any(is_renderable_char)
}

fn trim_non_renderable_start(text: &str) -> &str {
    text.char_indices()
        .find(|(_, ch)| is_renderable_char(*ch))
        .map_or("", |(start, _)| &text[start..])
}

fn visible_after_leading_thinking_blocks(text: &str) -> Option<&str> {
    let mut remaining = trim_non_renderable_start(text);
    let mut found = false;

    while remaining.starts_with("<thinking>") {
        found = true;
        let after_open = &remaining["<thinking>".len()..];
        let mut search_from = 0usize;
        let mut closing = None;
        while let Some(relative) = after_open[search_from..].find("</thinking>") {
            let position = search_from + relative;
            let after_position = position + "</thinking>".len();
            let suffix = &after_open[after_position..];
            let immediately_quoted = suffix
                .chars()
                .next()
                .is_some_and(|ch| matches!(ch, '\'' | '"' | '`' | '\\'));
            let boundary_is_protocol = suffix.is_empty()
                || suffix
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .any(|ch| matches!(ch, '\n' | '\r'));
            if !immediately_quoted && boundary_is_protocol {
                closing = Some(after_position);
            }
            search_from = after_position;
        }

        let Some(closing) = closing else {
            return Some("");
        };
        remaining = trim_non_renderable_start(&after_open[closing..]);
    }

    found.then_some(remaining)
}

pub(crate) fn has_visible_assistant_text(text: &str, thinking_enabled: bool) -> bool {
    if !thinking_enabled {
        return has_renderable_text(text);
    }

    visible_after_leading_thinking_blocks(text)
        .map_or_else(|| has_renderable_text(text), has_renderable_text)
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    #[cfg(test)]
    pub fn to_sse_string(&self) -> String {
        self.to_profile_sse_string(false)
    }

    pub fn to_profile_sse_string(&self, aws_b40_compat: bool) -> String {
        let terminator = if aws_b40_compat { "\n\n\n" } else { "\n\n" };
        format!(
            "event: {}\ndata: {}{}",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default(),
            terminator
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 命中的客户端 stop sequence。
    stop_sequence: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
    /// 是否已发出"首个 content_block_start 之后的确定性 ping"
    first_block_started: bool,
    /// 是否在首个内容块后发送兼容 ping。AWS Bedrock 流不发送该事件。
    emit_initial_ping: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            stop_sequence: None,
            has_tool_use: false,
            first_block_started: false,
            emit_initial_ping: true,
        }
    }

    pub fn set_emit_initial_ping(&mut self, emit: bool) {
        self.emit_initial_ping = emit;
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    pub fn has_tool_use(&self) -> bool {
        self.has_tool_use
    }

    pub fn clear_stop_reason(&mut self) {
        self.stop_reason = None;
        self.stop_sequence = None;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if reason != "stop_sequence" {
            self.stop_sequence = None;
        }
        self.stop_reason = Some(reason);
    }

    pub fn set_stop_sequence(&mut self, sequence: impl Into<String>) {
        self.stop_reason = Some("stop_sequence".to_string());
        self.stop_sequence = Some(sequence.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));

        // 真 Anthropic 在**首个** content_block_start 之后紧跟一个 ping（确定性、固定位置）。
        // 旧实现靠 25s keepalive 定时器的立即首 tick 发 ping，会与首个数据块在 select! 里赛跑，
        // 导致 ping 有时跑到 content_block_start 之前——位置不稳即指纹。这里改为确定性注入。
        if !self.first_block_started {
            self.first_block_started = true;
            if self.emit_initial_ping {
                events.push(SseEvent::new("ping", json!({"type": "ping"})));
            }
        }

        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        usage: super::cache::UsageBreakdown,
        output_tokens: i32,
        model: &str,
        thinking_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta（usage 含 cache 字段，由 caller 决定是否拆分）
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": self.stop_sequence,
                        "stop_details": null
                    },
                    "usage": super::compat::stream_delta_usage(
                        model,
                        usage.input_tokens,
                        output_tokens,
                        thinking_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_creation_1h_input_tokens,
                        usage.cache_read_input_tokens
                    )
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 已完成的自动续写轮次上游观测值，仅供内部诊断，不进入公共输入 usage。
    additional_round_input_tokens: Vec<i32>,
    /// 是否已经从首轮进入自动续写。
    continuation_started: bool,
    /// 输出 tokens 累计（字符估算，仅用于流中途的限长/续写判断，非最终计费数字）
    pub output_tokens: i32,
    /// thinking tokens 累计（字符估算，同上）
    pub thinking_tokens: i32,
    /// 已发给客户端的输出文本(含工具调用 JSON),流结束时用 ctoc 算一次得到最终 output_tokens。
    /// 贪心分词跨块不可加,必须累积完整文本再算。多轮 auto-continue 会持续累积。
    output_text_acc: String,
    /// 已发给客户端的 thinking 文本,流结束时用 ctoc 算一次。
    thinking_text_acc: String,
    /// 已完成的原生 reasoning 块只保留精确 token 数，避免为最终 usage
    /// 再长期保存一整份 thinking 文本。
    completed_native_thinking_tokens: i32,
    /// 当前上游轮次的原生 reasoning 是否已在块结束时计入累计值。
    native_reasoning_usage_accounted: bool,
    /// 客户请求的输出 token 上限
    output_token_limit: Option<i32>,
    /// 是否已经因为输出 token 上限停止向客户端发送文本
    output_token_limit_reached: bool,
    /// 客户端请求的文本停止序列。
    stop_sequences: Vec<String>,
    /// 为识别跨上游 chunk 的停止序列而暂存的短文本尾部。
    stop_sequence_pending: String,
    /// 是否已经命中停止序列并停止暴露后续内容。
    stop_sequence_reached: bool,
    /// 已收到的上游助手原始文本，用于 max_tokens 截断后的续写上下文
    pub assistant_raw_content: String,
    /// Native reasoning generated during the current invocation, represented by
    /// a bounded streaming token counter instead of another full-text copy.
    upstream_reasoning_current_round: super::claude_tok::StreamingClaudeTokenCounter,
    /// Tool arguments generated during the current invocation, likewise bounded.
    upstream_tool_input_current_round: super::claude_tok::StreamingClaudeTokenCounter,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 后端 tool_use_id(`toolu_bdrk_…`)→ 对客户端暴露的 Anthropic 形态 id(`toolu_01…`)。
    /// 同一后端 id 复用同一输出 id,保证同一响应内的块相关性;跨轮由客户端回传该 id 自洽。
    pub tool_output_ids: HashMap<String, String>,
    /// Tool argument bytes held briefly so Bedrock-style JSON deltas have
    /// stable structural boundaries rather than upstream transport boundaries.
    tool_json_pending: HashMap<String, String>,
    tool_json_prefix_split: HashSet<String>,
    /// Complete argument JSON per tool, retained only until the final frame so
    /// Bedrock output usage can account for additional argument fields.
    tool_input_acc: HashMap<String, String>,
    /// Upstream tool name per tool id. Retained so end-of-stream recovery can
    /// apply the same name guard when the provider omits its final stop frame.
    tool_names_by_id: HashMap<String, String>,
    tool_argument_fields: usize,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// 是否向客户端暴露 thinking 块。
    pub expose_thinking: bool,
    /// 是否向客户端暴露 thinking 文本。Bedrock 在未请求
    /// `display=summarized` 时仍返回块和签名，但省略文本增量。
    expose_thinking_text: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// 初始请求的 input/cache 拆分。
    pub initial_usage_breakdown: super::cache::UsageBreakdown,
    /// AWS-B uses the real Kiro context-usage event after removing its fixed
    /// runtime prompt. Disabled for AWS-P and for short requests.
    input_context_calibration: super::bedrock::InputContextCalibration,
    /// 首轮上游输入观测值，仅供内部诊断，不进入公共输入 usage。
    first_round_authoritative_input_tokens: Option<i32>,
    /// Exact cache/input split from `metadataEvent.tokenUsage`, when the
    /// selected Kiro runtime exposes it.
    exact_first_round_usage: Option<super::cache::UsageBreakdown>,
    /// Request-derived TTL layout for an authoritative aggregate cache write.
    exact_cache_ttl_plan: super::cache::ExactCacheTtlPlan,
    /// Evidence that the first invocation reached a native terminal event.
    /// HTTP 200, partial text/reasoning and a clean transport EOF are not
    /// sufficient on their own.
    first_round_terminal_evidence: bool,
    /// A refusal is a valid client response, but is not by itself proof that a
    /// locally planned compatibility-cache prefix became warm.
    first_round_refused: bool,
    /// Whether the current invocation supplied a known native stop reason.
    /// End-of-stream shape heuristics must not replace authoritative metadata.
    native_stop_reason_received: bool,
    /// An unknown native stop reason must fail closed even when Opus 5 has
    /// already emitted non-empty assistant text before a clean transport EOF.
    first_round_unknown_native_stop_reason: bool,
    /// A fully closed tool call is meaningful terminal model output even when
    /// no assistant text is visible to the client.
    first_round_completed_tool_use: bool,
    /// Exact aggregate input for the invocation currently streaming, derived
    /// only from `metadataEvent.tokenUsage` (which also carries cache buckets).
    exact_current_input_tokens: Option<i32>,
    /// Aggregate input observed in `meteringEvent`. This may refine ordinary
    /// input billing, but it has no cache-bucket semantics and must never by
    /// itself authorize a locally estimated cache split.
    metered_current_input_tokens: Option<i32>,
    /// Aggregate output observed from either native usage event.
    exact_current_output_tokens: Option<i32>,
    /// Exact output accumulated across completed continuation rounds.  The
    /// boolean stays false once any round lacks native output accounting.
    exact_completed_output_tokens: i32,
    exact_output_complete_for_prior_rounds: bool,
    /// 疑似截断探测续写时吞掉完成哨兵，避免把内部控制文本发给客户端
    swallow_complete_sentinel_probe: bool,
    /// 探测完成哨兵可能被上游拆成多个 chunk，需要短暂缓冲确认
    complete_sentinel_probe_buffer: String,
    /// 上一轮结尾文本，用于清理续写开头重复的尾巴
    continuation_merge_tail: Option<String>,
    /// 输出侧规整可见文本中的上游产品自称；规则会跳过代码并保留普通 Kiro 技术提及。
    identity_sanitizer: Option<super::identity::IdentityOutputSanitizer>,
    /// A trusted client system/developer identity lock. The real upstream is
    /// still called, but its visible identity sentence is replaced at stream
    /// finalization so a hidden runtime persona cannot override the caller's
    /// explicit application identity.
    forced_application_identity_reply: Option<String>,
    /// HTTP 200 EventStream bodies can still carry provider error/exception
    /// frames or end with a corrupt partial frame. Such a response must never
    /// be converted into a locally synthesized identity success.
    upstream_fatal_event: bool,
    /// Markdown fenced-code state is tracked outside IdentityOutputSanitizer so
    /// a code block longer than its bounded cross-chunk hold window can still
    /// stream verbatim without losing the closing-fence state.
    identity_in_fenced_code: bool,
    /// At most two trailing backticks retained to recognize a ``` delimiter
    /// split across upstream chunks.
    identity_fence_pending: String,
    /// thinking(思维链)通道身份清理选项。Some 时:思考块内容在**块结束**时统一
    /// 过一遍 `sanitize_thinking_identity_text`(强制 strict + 预置 identity 上下文)再发出,
    /// 堵住 “thinking 里直接说 I should respond as Kiro” 这类身份泄漏。
    thinking_sanitize_options: Option<super::identity::IdentitySanitizationOptions>,
    /// 待清理的原始 thinking 文本累积区。为了让跨 chunk 的身份短语能被整体识别,
    /// thinking 内容先累积在这里,到 thinking 块结束时一次性清理并作为 thinking_delta 发出。
    thinking_pending_raw: String,
    /// Previous native reasoning wire chunk, used to normalize cumulative events.
    native_reasoning_last_chunk: String,
    /// Native delta boundaries retained across whole-block identity sanitization.
    native_reasoning_chunk_lengths: Vec<usize>,
    /// Whether a native reasoning block is currently open.
    native_reasoning_active: bool,
    /// Opaque signature received from the upstream reasoning stream.
    upstream_thinking_signature: Option<String>,
    /// Exact thinking text emitted for the currently open native block. This is deliberately
    /// separate from accounting text so omitted/sanitized output binds to what the client saw.
    outbound_native_thinking_block: String,
    /// A native signature must be registered before its SSE delta is exposed to the client.
    pending_native_signature_registration: Option<(String, String)>,
    /// 待注入的合成 thinking 内容。仅作为旧模型或旧协议没有原生 reasoning 事件时的回退；
    /// 一旦收到真实 reasoningContentEvent 会立即取消。真实答案不受影响。
    pending_synthetic_thinking: Option<String>,
    /// tool_choice 强制工具(any/tool)时置真:抑制所有文本块,只发 tool_use ——
    /// 与真 Anthropic 强制工具行为一致,避免模型在 tool_use 前后夹带解释性文本
    /// (如 "I'll check the weather"),那会让"结构化输出/只认工具调用"探针判失败。
    suppress_text_blocks: bool,
    /// Deterministic request-derived input used only when a forced tool call
    /// completes with an empty object. Valid upstream input always wins.
    forced_tool_input_repair: Option<ForcedToolInputRepair>,
    /// 强制工具请求在首个 tool_use 前产生的文本先暂存。最终有工具时丢弃；若上游
    /// 异常地完全没有工具调用，则在流结束时回放，避免给正常客户端返回空消息。
    forced_tool_text_pending: String,
    /// Preserve the externally observed AWS-B/Bedrock protocol shape.
    aws_b40_compat: bool,
    aws_b40_thinking_requested: bool,
    /// `call_api_stream` 返回响应头前已经消耗的时间。
    upstream_request_latency_ms: u64,
    /// 从响应头到当前事件处理的计时起点。
    stream_started_at: Instant,
    /// 首个上游 EventStream 事件到达的端到端耗时。
    first_byte_latency_ms: Option<u64>,
}

/// Choose the first-round aggregate that may reconcile public usage.
///
/// `meteringEvent.inputTokens` is useful for flat, ordinary input accounting,
/// but it carries no cache read/write semantics. A request with an explicit
/// cache split therefore requires either native `metadataEvent.tokenUsage` or
/// an independently observed `contextUsageEvent`; metering alone is not cache
/// authority.
pub(super) fn select_first_round_reconciled_input(
    initial_usage: super::cache::UsageBreakdown,
    exact_token_usage_input: Option<i32>,
    metered_aggregate_input: Option<i32>,
    context_usage_input: Option<i32>,
    resolved_aggregate_input: i32,
) -> Option<i32> {
    if initial_usage.has_cache_usage() {
        return exact_token_usage_input
            .or(context_usage_input)
            .map(|tokens| tokens.max(1));
    }

    (exact_token_usage_input.is_some()
        || metered_aggregate_input.is_some()
        || context_usage_input.is_some())
    .then_some(resolved_aggregate_input.max(1))
}

fn forced_tool_input_json_kind(input: &str) -> &'static str {
    if input.trim().is_empty() {
        return "empty";
    }
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(object)) if object.is_empty() => "object_empty",
        Ok(serde_json::Value::Object(_)) => "object_nonempty",
        Ok(serde_json::Value::Array(_)) => "array",
        Ok(serde_json::Value::String(_)) => "string",
        Ok(serde_json::Value::Number(_)) => "number",
        Ok(serde_json::Value::Bool(_)) => "bool",
        Ok(serde_json::Value::Null) => "null",
        Err(_) => "invalid",
    }
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        initial_usage_breakdown: impl IntoInitialUsageBreakdown,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let initial_usage_breakdown = initial_usage_breakdown.into_breakdown(input_tokens);
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: id::message_id(),
            input_tokens,
            context_input_tokens: None,
            additional_round_input_tokens: Vec::new(),
            continuation_started: false,
            output_tokens: 0,
            thinking_tokens: 0,
            output_text_acc: String::new(),
            thinking_text_acc: String::new(),
            completed_native_thinking_tokens: 0,
            native_reasoning_usage_accounted: false,
            output_token_limit: None,
            output_token_limit_reached: false,
            stop_sequences: Vec::new(),
            stop_sequence_pending: String::new(),
            stop_sequence_reached: false,
            assistant_raw_content: String::new(),
            upstream_reasoning_current_round:
                super::claude_tok::StreamingClaudeTokenCounter::default(),
            upstream_tool_input_current_round:
                super::claude_tok::StreamingClaudeTokenCounter::default(),
            tool_block_indices: HashMap::new(),
            tool_output_ids: HashMap::new(),
            tool_json_pending: HashMap::new(),
            tool_json_prefix_split: HashSet::new(),
            tool_input_acc: HashMap::new(),
            tool_names_by_id: HashMap::new(),
            tool_argument_fields: 0,
            tool_name_map,
            thinking_enabled,
            expose_thinking: thinking_enabled,
            expose_thinking_text: thinking_enabled,
            thinking_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            initial_usage_breakdown,
            input_context_calibration: super::bedrock::InputContextCalibration::default(),
            first_round_authoritative_input_tokens: None,
            exact_first_round_usage: None,
            exact_cache_ttl_plan: super::cache::ExactCacheTtlPlan::default(),
            first_round_terminal_evidence: false,
            first_round_refused: false,
            native_stop_reason_received: false,
            first_round_unknown_native_stop_reason: false,
            first_round_completed_tool_use: false,
            exact_current_input_tokens: None,
            metered_current_input_tokens: None,
            exact_current_output_tokens: None,
            exact_completed_output_tokens: 0,
            exact_output_complete_for_prior_rounds: true,
            swallow_complete_sentinel_probe: false,
            complete_sentinel_probe_buffer: String::new(),
            continuation_merge_tail: None,
            identity_sanitizer: None,
            forced_application_identity_reply: None,
            upstream_fatal_event: false,
            identity_in_fenced_code: false,
            identity_fence_pending: String::new(),
            thinking_sanitize_options: None,
            thinking_pending_raw: String::new(),
            native_reasoning_last_chunk: String::new(),
            native_reasoning_chunk_lengths: Vec::new(),
            native_reasoning_active: false,
            upstream_thinking_signature: None,
            outbound_native_thinking_block: String::new(),
            pending_native_signature_registration: None,
            pending_synthetic_thinking: None,
            suppress_text_blocks: false,
            forced_tool_input_repair: None,
            forced_tool_text_pending: String::new(),
            aws_b40_compat: false,
            aws_b40_thinking_requested: false,
            upstream_request_latency_ms: 0,
            stream_started_at: Instant::now(),
            first_byte_latency_ms: None,
        }
    }

    pub fn enable_aws_b40_compat(&mut self) {
        self.aws_b40_compat = true;
        self.model = super::bedrock::response_model(&self.model);
        self.message_id = super::bedrock::response_id(&self.model);
        self.state_manager.set_emit_initial_ping(false);
    }

    pub fn set_aws_b40_thinking_requested(&mut self, requested: bool) {
        self.aws_b40_thinking_requested = requested;
    }

    pub fn set_stop_sequences(&mut self, sequences: Vec<String>) {
        self.stop_sequences = sequences
            .into_iter()
            .filter(|sequence| !sequence.is_empty())
            .collect();
        self.stop_sequence_pending.clear();
        self.stop_sequence_reached = false;
    }

    pub fn set_input_context_calibration(
        &mut self,
        calibration: super::bedrock::InputContextCalibration,
    ) {
        self.input_context_calibration = calibration;
    }

    pub fn set_exact_cache_ttl_plan(&mut self, plan: super::cache::ExactCacheTtlPlan) {
        self.exact_cache_ttl_plan = plan;
    }

    pub(super) fn take_native_signature_registration(
        &mut self,
    ) -> Option<(String, String, String)> {
        self.pending_native_signature_registration
            .take()
            .map(|(thinking, signature)| (self.model.clone(), thinking, signature))
    }

    pub fn set_upstream_request_latency(&mut self, elapsed: Duration) {
        self.upstream_request_latency_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self.stream_started_at = Instant::now();
    }

    pub fn hide_thinking_blocks(&mut self) {
        self.expose_thinking = false;
    }

    pub fn set_thinking_text_visible(&mut self, visible: bool) {
        self.expose_thinking_text = visible;
    }

    /// tool_choice 强制工具(any/tool)时调用:响应只保留 tool_use,抑制所有文本块。
    pub fn set_suppress_text_blocks(&mut self, suppress: bool) {
        self.suppress_text_blocks = suppress;
    }

    pub(crate) fn set_forced_tool_input_repair(&mut self, repair: Option<ForcedToolInputRepair>) {
        self.forced_tool_input_repair = repair;
    }

    /// 设置待注入的合成 thinking(仅上游不产思考但客户请求了 thinking 时使用)。
    pub fn set_synthetic_thinking(&mut self, thinking: Option<String>) {
        self.pending_synthetic_thinking = thinking;
    }

    #[allow(dead_code)]
    pub fn enable_identity_sanitization(&mut self) {
        self.enable_identity_sanitization_with_strict_mode(true);
    }

    pub fn enable_identity_sanitization_with_strict_mode(&mut self, strict_identity_context: bool) {
        let options = super::identity::IdentitySanitizationOptions::strict(strict_identity_context);
        self.identity_sanitizer = Some(super::identity::IdentityOutputSanitizer::new_with_options(
            options,
        ));
        self.thinking_sanitize_options = Some(options);
    }

    #[allow(dead_code)]
    pub fn enable_identity_sanitization_with_options(
        &mut self,
        strict_identity_context: bool,
        agentic_ide_probe: bool,
        codewhisperer_relationship_probe: bool,
        vendor_lineage_probe: bool,
        obfuscated_private_thinking_probe: bool,
        third_party_kiro_discussion: bool,
    ) {
        let options = super::identity::IdentitySanitizationOptions {
            target: super::identity::IdentityTarget::Claude,
            query: super::identity::IdentityQuery::default(),
            strict_identity_context,
            structured_identity_probe: false,
            agentic_ide_probe,
            codewhisperer_relationship_probe,
            vendor_lineage_probe,
            obfuscated_private_thinking_probe,
            third_party_kiro_discussion,
        };
        self.enable_identity_sanitization_with_profile(options);
    }

    pub fn enable_identity_sanitization_with_profile(
        &mut self,
        options: super::identity::IdentitySanitizationOptions,
    ) {
        self.identity_sanitizer = Some(super::identity::IdentityOutputSanitizer::new_with_options(
            options,
        ));
        self.identity_in_fenced_code = false;
        self.identity_fence_pending.clear();
        self.thinking_sanitize_options = Some(options);
    }

    pub fn set_forced_application_identity_reply(&mut self, reply: Option<String>) {
        self.forced_application_identity_reply = reply;
        if self.forced_application_identity_reply.is_some() {
            self.pending_synthetic_thinking = None;
            self.expose_thinking = false;
            self.expose_thinking_text = false;
        }
    }

    fn strict_gpt_self_identity_target(&self) -> Option<super::identity::IdentityTarget> {
        self.thinking_sanitize_options
            .filter(|options| {
                options.target.is_gpt()
                    && options.strict_identity_context
                    && !options.third_party_kiro_discussion
            })
            .map(|options| options.target)
    }

    fn has_gpt_identity_target(&self) -> bool {
        self.thinking_sanitize_options
            .is_some_and(|options| options.target.is_gpt())
            || super::identity::IdentityTarget::for_model(&self.model).is_gpt()
    }

    pub fn set_output_token_limit(&mut self, max_tokens: i32) {
        self.output_token_limit = Some(max_tokens.max(1));
    }

    fn stream_start_usage_breakdown(&self) -> super::cache::UsageBreakdown {
        // Input usage is fully determined before the upstream call. Keep the
        // first SSE frame identical to the final delta; response-side usage
        // events are diagnostic only and cannot rewrite client input.
        self.initial_usage_breakdown.clamp_for_model(&self.model)
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        let breakdown = self.stream_start_usage_breakdown();
        if self.aws_b40_compat {
            let initial_output_tokens = match self.output_token_limit {
                Some(limit) if limit <= 1 => 1,
                _ if self.suppress_text_blocks => 16,
                _ if self.aws_b40_thinking_requested && self.thinking_enabled => 8,
                _ if self.pending_synthetic_thinking.is_some() => 4,
                _ if self.aws_b40_thinking_requested => 3,
                _ => 1,
            };
            let mut usage = json!({
                "input_tokens": breakdown.input_tokens,
                "cache_creation_input_tokens": breakdown.cache_creation_input_tokens,
                "cache_read_input_tokens": breakdown.cache_read_input_tokens,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": breakdown.cache_creation_5m_input_tokens,
                    "ephemeral_1h_input_tokens": breakdown.cache_creation_1h_input_tokens
                },
                "output_tokens": initial_output_tokens
            });
            super::compat::apply_service_tier(
                &self.model,
                usage.as_object_mut().expect("usage object"),
            );
            return json!({
                "type": "message_start",
                "message": {
                    "model": self.model,
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "stop_details": null,
                    "usage": usage
                }
            });
        }
        // message_start 用 stream_start_usage（不含 output_tokens_details）。
        let usage = super::compat::stream_start_usage(
            &self.model,
            breakdown.input_tokens,
            1,
            0,
            breakdown.cache_creation_input_tokens,
            breakdown.cache_creation_1h_input_tokens,
            breakdown.cache_read_input_tokens,
        );
        json!({
            "type": "message_start",
            "message": {
                "model": self.model,
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "stop_details": null,
                "usage": usage
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 首块一律惰性创建:不在这里急切发出空文本块。
        // 首个真实内容到达时才创建对应的首块——
        //   文本 -> emit_text_delta_events 会创建 text 块;
        //   工具 -> process_tool_use 创建 tool_use 块;
        //   思考 -> process_content_with_thinking 创建 thinking 块。
        // 这样强制工具调用(tool_choice)时会直接以 tool_use 开头,不再多出一个空 text 块
        // (真 Anthropic 的强制工具响应正是如此)。ping 仍固定跟在"首个 content_block_start"之后。
        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        if self.first_byte_latency_ms.is_none() {
            self.first_byte_latency_ms = Some(
                self.upstream_request_latency_ms
                    .saturating_add(self.stream_started_at.elapsed().as_millis() as u64),
            );
        }
        // A context event can arrive after the visible output has already hit
        // max_tokens. It is still authoritative billing data and must not be
        // discarded with later content events.
        if self.output_token_limit_reached
            && !matches!(
                event,
                Event::ThinkingMetadata(_)
                    | Event::ContextUsage(_)
                    | Event::Metadata(_)
                    | Event::Metering(_)
            )
        {
            return Vec::new();
        }

        match event {
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ThinkingMetadata(metadata) => self.process_thinking_metadata(metadata),
            Event::AssistantResponse(resp) => {
                if tracing::enabled!(tracing::Level::TRACE) {
                    let fields = resp.extra_field_names();
                    if !fields.is_empty() {
                        tracing::trace!(
                            fields = ?fields,
                            "收到带扩展字段的 assistantResponseEvent"
                        );
                    }
                }
                self.process_assistant_response(&resp.content)
            }
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 观测 context tokens: {}（不参与公共输入 usage）",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Unknown {
                event_type,
                payload,
            } => {
                let fields = unknown_event_field_names(payload);
                tracing::debug!(
                    event_type,
                    payload_bytes = payload.len(),
                    fields = ?fields,
                    "收到未知 Kiro 事件"
                );
                Vec::new()
            }
            Event::InvalidState(invalid) => {
                self.mark_upstream_fatal_event();
                tracing::error!(
                    reason = %invalid.reason,
                    message = %invalid.message,
                    "收到 invalidStateEvent，终止当前上游调用"
                );
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                self.mark_upstream_fatal_event();
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                    if !self.continuation_started {
                        self.first_round_terminal_evidence = true;
                    }
                } else {
                    self.mark_upstream_fatal_event();
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            Event::Metering(metering) => {
                // Kiro emits metering as a terminal accounting event. It is
                // completion evidence, but its aggregate input count still
                // carries no cache read/write semantics.
                if !self.continuation_started {
                    self.first_round_terminal_evidence = true;
                }
                if self.metered_current_input_tokens.is_none() && metering.input_tokens > 0 {
                    self.metered_current_input_tokens = Some(metering.input_tokens);
                }
                if self.exact_current_output_tokens.is_none() && metering.output_tokens > 0 {
                    self.exact_current_output_tokens = Some(metering.output_tokens);
                }
                tracing::debug!(
                    input_tokens = metering.input_tokens,
                    output_tokens = metering.output_tokens,
                    usage = metering.usage,
                    "收到 meteringEvent"
                );
                Vec::new()
            }
            Event::Metadata(metadata) => {
                let usage = &metadata.token_usage;
                let native = super::cache::reconcile_native_usage(
                    &self.model,
                    self.initial_usage_breakdown,
                    usage,
                    self.exact_cache_ttl_plan,
                );
                let mapped_stop_reason = metadata
                    .stop_reason
                    .as_deref()
                    .and_then(crate::kiro::model::events::anthropic_stop_reason);
                if let Some(reason) = mapped_stop_reason {
                    self.native_stop_reason_received = true;
                    // A client-side cap remains authoritative for the bytes
                    // actually exposed even when terminal metadata arrives
                    // after the locally truncated content.
                    if !self.output_token_limit_reached {
                        self.state_manager.set_stop_reason(reason);
                    }
                    if !self.continuation_started && reason == "refusal" {
                        self.first_round_refused = true;
                    }
                } else if metadata
                    .stop_reason
                    .as_deref()
                    .is_some_and(|reason| !reason.trim().is_empty())
                {
                    if !self.continuation_started {
                        self.first_round_unknown_native_stop_reason = true;
                    }
                    tracing::warn!(
                        native_stop_reason = %metadata.stop_reason.as_deref().unwrap_or_default(),
                        "忽略未知 Kiro stopReason，保留安全的本地终止推断"
                    );
                }
                if !self.continuation_started
                    && (native.is_some()
                        || mapped_stop_reason.is_some_and(|reason| reason != "refusal"))
                {
                    self.first_round_terminal_evidence = true;
                }
                if let Some(native) = native {
                    self.exact_current_input_tokens = Some(native.aggregate_input_tokens);
                    self.exact_current_output_tokens = Some(native.output_tokens);
                    if !self.continuation_started {
                        self.exact_first_round_usage = native.public_cache_usage;
                    }
                }
                tracing::debug!(
                    uncached_input_tokens = usage.uncached_input_tokens,
                    cache_read_input_tokens = usage.cache_read_input_tokens,
                    cache_write_input_tokens = usage.cache_write_input_tokens,
                    output_tokens = usage.output_tokens,
                    total_tokens = usage.total_tokens,
                    "收到 metadataEvent"
                );
                Vec::new()
            }
        }
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        if self.output_token_limit_reached {
            return Vec::new();
        }

        if content.is_empty() {
            return Vec::new();
        }

        let mut events = if self.native_reasoning_active {
            self.finish_native_reasoning_block()
        } else {
            Vec::new()
        };

        if let Some(completion_events) = self.process_completion_probe_content(content) {
            events.extend(completion_events);
            return events;
        }

        let merged_content = self.merge_continuation_boundary(content);
        if merged_content.is_empty() {
            return events;
        }
        let content = merged_content.as_str();

        self.assistant_raw_content.push_str(content);

        if self.forced_application_identity_reply.is_some() {
            return events;
        }

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            events.extend(self.process_content_with_thinking(content));
            return events;
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        events.extend(self.create_text_delta_events(content));
        events
    }

    fn merge_continuation_boundary(&mut self, content: &str) -> String {
        match self.continuation_merge_tail.take() {
            Some(previous_tail) => merge_continuation_text(&previous_tail, content),
            None => content.to_string(),
        }
    }

    fn process_completion_probe_content(&mut self, content: &str) -> Option<Vec<SseEvent>> {
        if !self.swallow_complete_sentinel_probe {
            return None;
        }

        self.complete_sentinel_probe_buffer.push_str(content);
        let trimmed = self.complete_sentinel_probe_buffer.trim();

        if trimmed == AUTO_CONTINUE_COMPLETE_SENTINEL {
            self.swallow_complete_sentinel_probe = false;
            self.complete_sentinel_probe_buffer.clear();
            return Some(Vec::new());
        }

        if trimmed.is_empty() || AUTO_CONTINUE_COMPLETE_SENTINEL.starts_with(trimmed) {
            return Some(Vec::new());
        }

        self.swallow_complete_sentinel_probe = false;
        let buffered = std::mem::take(&mut self.complete_sentinel_probe_buffer);
        Some(self.process_assistant_response(&buffered))
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        // Opus 通常不返回 thinking，因此可能准备了合成块。但上游仍可能在特定提示下
        // 返回真实 <thinking>。真实块必须优先走专用严格清洗，不能在合成块之后被当成正文。
        if self.pending_synthetic_thinking.is_some() {
            if self.suppress_text_blocks {
                self.pending_synthetic_thinking = None;
            } else if find_real_thinking_start_tag(&self.thinking_buffer).is_some() {
                self.pending_synthetic_thinking = None;
            } else if could_still_start_with_thinking_tag(&self.thinking_buffer) {
                return events;
            } else if let Some(synth) = self.pending_synthetic_thinking.take() {
                events.extend(self.emit_synthetic_thinking_block(&synth));
            }
        }

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    if self.expose_thinking && !self.aws_b40_compat {
                        // 创建 thinking 块的 content_block_start 事件
                        let thinking_index = self.state_manager.next_block_index();
                        self.thinking_block_index = Some(thinking_index);
                        let start_events = self.state_manager.handle_content_block_start(
                            thinking_index,
                            "thinking",
                            json!({
                                "type": "content_block_start",
                                "index": thinking_index,
                                "content_block": {
                                    "type": "thinking",
                                    "thinking": "",
                                    "signature": ""
                                }
                            }),
                        );
                        events.extend(start_events);
                    }
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 累积 thinking 内容(延迟到块结束时统一清理后再发出)
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    self.accumulate_thinking(&thinking_content);

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 先 flush(清理后)累积的 thinking,再发空的 thinking_delta、signature_delta、content_block_stop
                    if self.expose_thinking && !self.aws_b40_compat {
                        if let Some(thinking_index) = self.thinking_block_index {
                            if let Some(ev) = self.flush_thinking_delta(thinking_index) {
                                events.push(ev);
                            }
                            if self.expose_thinking_text {
                                events.push(self.create_thinking_delta_event(thinking_index, ""));
                            }
                            if let Some(signature) =
                                self.create_signature_delta_event(thinking_index)
                            {
                                events.push(signature);
                            }
                            if let Some(stop_event) =
                                self.state_manager.handle_content_block_stop(thinking_index)
                            {
                                events.push(stop_event);
                            }
                        }
                    }

                    // 剥离 `</thinking>\n\n`（find_real_thinking_end_tag 已确认 \n\n 存在）
                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        // 累积安全内容(延迟到块结束时统一清理);缓冲区照常推进。
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        self.accumulate_thinking(&safe_content);
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        if reasoning.text.is_empty()
            && reasoning.signature.is_empty()
            && reasoning.redacted_content.is_empty()
        {
            return Vec::new();
        }

        // A real upstream event always wins over the compatibility fallback.
        // Requests that did not enable thinking keep the reasoning private.
        self.pending_synthetic_thinking = None;
        if !self.thinking_enabled
            || self.forced_application_identity_reply.is_some()
            || self.suppress_text_blocks
        {
            if !reasoning.text.is_empty() {
                let delta =
                    cumulative_event_delta(&reasoning.text, &self.native_reasoning_last_chunk);
                self.native_reasoning_last_chunk = reasoning.text.clone();
                if !delta.is_empty() {
                    self.upstream_reasoning_current_round.push_str(&delta);
                    self.thinking_tokens += estimate_tokens(&delta);
                }
            }
            return Vec::new();
        }

        let mut events = Vec::new();

        if !reasoning.redacted_content.is_empty() {
            if self.native_reasoning_active {
                events.extend(self.finish_native_reasoning_block());
            }
            events.extend(self.emit_redacted_thinking_block(&reasoning.redacted_content));
        }

        if !reasoning.text.is_empty() && !self.thinking_extracted {
            let delta = cumulative_event_delta(&reasoning.text, &self.native_reasoning_last_chunk);
            self.native_reasoning_last_chunk = reasoning.text.clone();
            if !delta.is_empty() {
                self.upstream_reasoning_current_round.push_str(&delta);
                if !self.native_reasoning_active {
                    if self.aws_b40_compat {
                        // Buffer until the upstream signature arrives. This
                        // prevents an unsigned Kiro reasoning fragment from
                        // being exposed as an Anthropic thinking block.
                        self.native_reasoning_active = true;
                    } else {
                        events.extend(self.start_native_reasoning_block());
                    }
                }
                self.native_reasoning_chunk_lengths
                    .push(delta.chars().count());
                self.accumulate_thinking(&delta);
            }
        }

        if !reasoning.signature.is_empty() {
            self.upstream_thinking_signature = Some(reasoning.signature.clone());
            if self.thinking_block_index.is_none() && reasoning.redacted_content.is_empty() {
                events.extend(self.start_native_reasoning_block());
            }
            if self.native_reasoning_active {
                events.extend(self.finish_native_reasoning_block());
            }
        }

        events
    }

    fn process_thinking_metadata(
        &mut self,
        metadata: &crate::kiro::model::events::ThinkingMetadataEvent,
    ) -> Vec<SseEvent> {
        if metadata.signature.is_empty() {
            return Vec::new();
        }

        // Opus 4.6/4.7 deliver their opaque signature in this trailing event,
        // rather than on reasoningContentEvent itself. Once it arrives, close
        // the buffered native block through the exact same registration and
        // signature-delta path used by Opus 4.8.
        self.pending_synthetic_thinking = None;
        self.upstream_thinking_signature = Some(metadata.signature.clone());
        tracing::trace!(
            thinking_tokens = metadata.token_count,
            "收到 thinkingMetadataEvent"
        );

        if !self.thinking_enabled
            || self.forced_application_identity_reply.is_some()
            || self.suppress_text_blocks
        {
            return Vec::new();
        }

        let mut events = Vec::new();
        if self.thinking_block_index.is_none() {
            events.extend(self.start_native_reasoning_block());
        }
        events.extend(self.finish_native_reasoning_block());
        events
    }

    fn start_native_reasoning_block(&mut self) -> Vec<SseEvent> {
        self.native_reasoning_active = true;
        self.outbound_native_thinking_block.clear();
        if !self.expose_thinking {
            return Vec::new();
        }

        let index = self.state_manager.next_block_index();
        self.thinking_block_index = Some(index);
        self.state_manager.handle_content_block_start(
            index,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "thinking",
                    "thinking": "",
                    "signature": ""
                }
            }),
        )
    }

    fn finish_native_reasoning_block(&mut self) -> Vec<SseEvent> {
        if !self.native_reasoning_active {
            return Vec::new();
        }
        let local_signature_fallback = self.aws_b40_compat
            && self.upstream_thinking_signature.is_none()
            && super::compat::model_uses_local_thinking_signature_fallback(&self.model);
        let mut events = Vec::new();
        if local_signature_fallback && self.thinking_block_index.is_none() {
            events.extend(self.start_native_reasoning_block());
        }
        self.native_reasoning_active = false;
        self.thinking_extracted = true;

        if self.aws_b40_compat
            && self.upstream_thinking_signature.is_none()
            && !local_signature_fallback
        {
            let raw = std::mem::take(&mut self.thinking_pending_raw);
            if !raw.is_empty() {
                self.thinking_tokens += estimate_tokens(&raw);
                self.completed_native_thinking_tokens = self
                    .completed_native_thinking_tokens
                    .saturating_add(super::claude_tok::count_claude(&raw));
                self.native_reasoning_usage_accounted = true;
            }
            self.native_reasoning_last_chunk.clear();
            self.native_reasoning_chunk_lengths.clear();
            return events;
        }
        if self.expose_thinking {
            if let Some(index) = self.thinking_block_index {
                events.extend(self.flush_native_reasoning_deltas(index));
                if self.aws_b40_compat
                    && let Some(signature) = self.upstream_thinking_signature.as_ref()
                {
                    self.pending_native_signature_registration = Some((
                        std::mem::take(&mut self.outbound_native_thinking_block),
                        signature.clone(),
                    ));
                }
                if let Some(signature) = self.create_signature_delta_event(index) {
                    events.push(signature);
                }
                if let Some(stop) = self.state_manager.handle_content_block_stop(index) {
                    events.push(stop);
                }
            }
        }
        self.native_reasoning_last_chunk.clear();
        self.native_reasoning_chunk_lengths.clear();
        events
    }

    fn flush_native_reasoning_deltas(&mut self, index: i32) -> Vec<SseEvent> {
        if self.thinking_pending_raw.is_empty() {
            return Vec::new();
        }
        let raw = std::mem::take(&mut self.thinking_pending_raw);
        if !self.expose_thinking_text {
            self.thinking_tokens += estimate_tokens(&raw);
            self.completed_native_thinking_tokens = self
                .completed_native_thinking_tokens
                .saturating_add(super::claude_tok::count_claude(&raw));
            self.native_reasoning_usage_accounted = true;
            return Vec::new();
        }
        let sanitized = match self.thinking_sanitize_options {
            Some(options) => super::identity::sanitize_thinking_identity_text(&raw, options),
            None => raw,
        };

        self.completed_native_thinking_tokens = self
            .completed_native_thinking_tokens
            .saturating_add(super::claude_tok::count_claude(&sanitized));
        self.native_reasoning_usage_accounted = true;

        split_text_by_char_lengths(&sanitized, &self.native_reasoning_chunk_lengths)
            .into_iter()
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| self.create_native_thinking_delta_event(index, &chunk))
            .collect()
    }

    fn emit_redacted_thinking_block(&mut self, data: &str) -> Vec<SseEvent> {
        self.thinking_extracted = true;
        if !self.expose_thinking {
            return Vec::new();
        }

        let index = self.state_manager.next_block_index();
        let mut events = self.state_manager.handle_content_block_start(
            index,
            "redacted_thinking",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "redacted_thinking",
                    "data": data
                }
            }),
        );
        if let Some(stop) = self.state_manager.handle_content_block_stop(index) {
            events.push(stop);
        }
        events
    }

    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        if self.identity_sanitizer.is_none() {
            return self.emit_text_delta_events(text);
        }

        // Strict GPT identity answers need the sanitizer to see quote/code
        // delimiters together with their surrounding labels before anything
        // is emitted. This preserves explicit business/test literals while
        // still retargeting an identity answer that is merely wrapped in code.
        if self.strict_gpt_self_identity_target().is_some() {
            let sanitized = self
                .identity_sanitizer
                .as_mut()
                .expect("checked above")
                .push(text);
            return if sanitized.is_empty() {
                Vec::new()
            } else {
                self.emit_text_delta_events(&sanitized)
            };
        }

        // IdentityOutputSanitizer deliberately bounds how much unclosed code
        // it retains. If a fence exceeds that bound, a forced split would make
        // the retained suffix forget that it is still code; the closing fence
        // would then look like an opener and prose after it could bypass
        // identity cleaning. Track triple-backtick fences here and stream their
        // contents verbatim, while keeping prose in the sanitizer.
        let mut input = std::mem::take(&mut self.identity_fence_pending);
        input.push_str(text);
        let mut events = Vec::new();

        while !input.is_empty() {
            if self.identity_in_fenced_code {
                if let Some(close) = input.find("```") {
                    let after_close = close + 3;
                    events.extend(self.emit_text_delta_events(&input[..after_close]));
                    self.identity_in_fenced_code = false;
                    input = input[after_close..].to_string();
                    continue;
                }

                let split = split_before_trailing_fence_prefix(&input);
                if split > 0 {
                    events.extend(self.emit_text_delta_events(&input[..split]));
                }
                self.identity_fence_pending = input[split..].to_string();
                break;
            }

            if let Some(open) = input.find("```") {
                if open > 0 {
                    let sanitized = self
                        .identity_sanitizer
                        .as_mut()
                        .expect("checked above")
                        .push(&input[..open]);
                    if !sanitized.is_empty() {
                        events.extend(self.emit_text_delta_events(&sanitized));
                    }
                }

                // The fence must not overtake a short prose suffix retained by
                // the sanitizer.
                events.extend(self.flush_pending_identity_text());
                events.extend(self.emit_text_delta_events("```"));
                self.identity_in_fenced_code = true;
                input = input[open + 3..].to_string();
                continue;
            }

            let split = split_before_trailing_fence_prefix(&input);
            if split > 0 {
                let sanitized = self
                    .identity_sanitizer
                    .as_mut()
                    .expect("checked above")
                    .push(&input[..split]);
                if !sanitized.is_empty() {
                    events.extend(self.emit_text_delta_events(&sanitized));
                }
            }
            self.identity_fence_pending = input[split..].to_string();
            break;
        }

        events
    }

    /// Flush text retained by the cross-chunk identity sanitizer at a content
    /// block boundary.
    ///
    /// The sanitizer intentionally keeps a short suffix so identity phrases
    /// split across upstream chunks can still be recognized. A tool block is a
    /// hard Anthropic content boundary, though: opening it before releasing
    /// that suffix would move the earlier text after the tool (and, for a
    /// truncated tool, interleave two open blocks during finalization).
    fn flush_pending_identity_text(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let fence_pending = std::mem::take(&mut self.identity_fence_pending);
        if self.identity_in_fenced_code {
            if !fence_pending.is_empty() {
                events.extend(self.emit_text_delta_events(&fence_pending));
            }
        } else if !fence_pending.is_empty()
            && let Some(sanitizer) = self.identity_sanitizer.as_mut()
        {
            let sanitized = sanitizer.push(&fence_pending);
            if !sanitized.is_empty() {
                events.extend(self.emit_text_delta_events(&sanitized));
            }
        }
        self.identity_in_fenced_code = false;

        let remaining = self
            .identity_sanitizer
            .as_mut()
            .map(super::identity::IdentityOutputSanitizer::finish)
            .unwrap_or_default();
        if !remaining.is_empty() {
            events.extend(self.emit_text_delta_events(&remaining));
        }
        events
    }

    fn emit_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        // 强制工具调用时先缓冲前导文本；一旦工具出现就连同后续文本一起丢弃。
        // 若整轮没有工具，generate_final_events 会关闭抑制并回放缓冲内容。
        if self.suppress_text_blocks {
            if self.tool_block_indices.is_empty() {
                self.forced_tool_text_pending.push_str(text);
            }
            return Vec::new();
        }
        if self.stop_sequence_reached {
            return Vec::new();
        }
        if self.stop_sequences.is_empty() {
            return self.emit_text_delta_events_unfiltered(text);
        }

        self.stop_sequence_pending.push_str(text);
        if let Some((byte_index, sequence_index)) =
            earliest_stop_sequence(&self.stop_sequence_pending, &self.stop_sequences)
        {
            let visible = self.stop_sequence_pending[..byte_index].to_string();
            let matched = self.stop_sequences[sequence_index].clone();
            self.stop_sequence_pending.clear();
            self.stop_sequence_reached = true;
            // Reuse the existing terminal-content gate so later upstream text
            // or tool frames cannot leak past the client-requested boundary.
            self.output_token_limit_reached = true;
            self.state_manager.set_stop_sequence(matched);
            return self.emit_text_delta_events_unfiltered(&visible);
        }

        let retained =
            trailing_stop_sequence_prefix_len(&self.stop_sequence_pending, &self.stop_sequences);
        let emit_len = self.stop_sequence_pending.len().saturating_sub(retained);
        let visible = self.stop_sequence_pending[..emit_len].to_string();
        self.stop_sequence_pending.drain(..emit_len);
        self.emit_text_delta_events_unfiltered(&visible)
    }

    fn emit_text_delta_events_unfiltered(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let Some(text) = self.apply_output_token_limit(text) else {
            return events;
        };
        if text.is_empty() {
            return events;
        }
        self.output_tokens += estimate_tokens(&text);
        self.output_text_acc.push_str(&text); // ctoc 在流结束时对全量文本计数

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // Kiro may deliver an entire answer in one upstream event. Bedrock clients still expect
        // incremental SSE frames, so AWS-B splits only the transport envelope. Concatenating the
        // deltas is byte-for-byte identical to the original text.
        let chunks = if self.aws_b40_compat {
            text_delta_chunks(&text)
        } else {
            vec![text.as_str()]
        };
        for chunk in chunks {
            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                text_index,
                json!({
                    "type": "content_block_delta",
                    "index": text_index,
                    "delta": {
                        "type": "text_delta",
                        "text": chunk
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        events
    }

    fn flush_pending_stop_sequence_text(&mut self) -> Vec<SseEvent> {
        if self.stop_sequence_reached || self.stop_sequence_pending.is_empty() {
            self.stop_sequence_pending.clear();
            return Vec::new();
        }
        let pending = std::mem::take(&mut self.stop_sequence_pending);
        self.emit_text_delta_events_unfiltered(&pending)
    }

    /// 直接发出一个完整的合成 thinking 块(用于上游不产思考、但客户请求了 thinking 的情况,如 opus)。
    /// 发出后置 `thinking_extracted=true`,后续真实内容走文本路径。
    fn emit_synthetic_thinking_block(&mut self, synth: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let local_signature_fallback = self.aws_b40_compat
            && super::compat::model_uses_local_thinking_signature_fallback(&self.model);
        if self.aws_b40_compat && !local_signature_fallback {
            self.thinking_extracted = true;
            return events;
        }
        if local_signature_fallback && self.upstream_thinking_signature.is_none() {
            self.upstream_thinking_signature =
                super::signature::generate_model_signature(&self.model);
        }
        if self.expose_thinking {
            let idx = self.state_manager.next_block_index();
            self.thinking_block_index = Some(idx);
            events.extend(self.state_manager.handle_content_block_start(
                idx,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "thinking",
                        "thinking": "",
                        "signature": ""
                    }
                }),
            ));
            // A local fallback authenticates the compatibility envelope; it
            // must not present generic gateway text as model reasoning.
            if !self.aws_b40_compat {
                events.push(self.create_thinking_delta_event(idx, synth));
            }
            if let Some(signature) = self.create_signature_delta_event(idx) {
                events.push(signature);
            }
            if local_signature_fallback
                && let Some(signature) = self.upstream_thinking_signature.as_ref()
            {
                self.pending_native_signature_registration = Some((
                    self.outbound_native_thinking_block.clone(),
                    signature.clone(),
                ));
            }
            if let Some(stop) = self.state_manager.handle_content_block_stop(idx) {
                events.push(stop);
            }
        }
        self.thinking_extracted = true;
        events
    }

    /// 累积一段**原始**(未清理)thinking 文本,延迟到 thinking 块结束时统一清理后再发出。
    /// 只在 `expose_thinking` 时累积(与旧逻辑的发出门控一致);非空判断留给 flush。
    fn accumulate_thinking(&mut self, content: &str) {
        if self.expose_thinking && !content.is_empty() {
            self.thinking_pending_raw.push_str(content);
        }
    }

    /// thinking 块结束时调用:对累积的原始 thinking 做身份清理,并作为单个 thinking_delta 发出。
    /// 未开启身份清理(`thinking_sanitize_options` 为 None)时原样发出,保持既有行为。
    fn flush_thinking_delta(&mut self, index: i32) -> Option<SseEvent> {
        if self.thinking_pending_raw.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.thinking_pending_raw);
        if !self.expose_thinking_text {
            self.thinking_tokens += estimate_tokens(&raw);
            return None;
        }
        let out = match self.thinking_sanitize_options {
            Some(options) => super::identity::sanitize_thinking_identity_text(&raw, options),
            None => raw,
        };
        if out.is_empty() {
            return None;
        }
        Some(self.create_thinking_delta_event(index, &out))
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&mut self, index: i32, thinking: &str) -> SseEvent {
        if !thinking.is_empty() {
            self.outbound_native_thinking_block.push_str(thinking);
            self.thinking_tokens += estimate_tokens(thinking);
            self.thinking_text_acc.push_str(thinking); // ctoc 流结束时计数
        }
        Self::thinking_delta_event(index, thinking)
    }

    /// 原生 reasoning 在块结束时已经基于完整文本完成精确计数；这里仅维护
    /// 客户端实际收到的文本以登记 opaque signature，不再复制到 usage 累积区。
    fn create_native_thinking_delta_event(&mut self, index: i32, thinking: &str) -> SseEvent {
        if !thinking.is_empty() {
            self.outbound_native_thinking_block.push_str(thinking);
            self.thinking_tokens += estimate_tokens(thinking);
        }
        Self::thinking_delta_event(index, thinking)
    }

    fn thinking_delta_event(index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// Create a signature delta. AWS-B only passes through a native upstream
    /// signature; legacy profiles retain their local compatibility fallback.
    fn create_signature_delta_event(&self, index: i32) -> Option<SseEvent> {
        let signature = if let Some(signature) = self.upstream_thinking_signature.as_ref() {
            signature.clone()
        } else if self.aws_b40_compat {
            super::compat::model_uses_local_thinking_signature_fallback(&self.model)
                .then(|| super::signature::generate_model_signature(&self.model))
                .flatten()?
        } else {
            super::signature::generate_signature()
        };
        Some(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
                }
            }),
        ))
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        if !tool_use.input.is_empty() {
            self.upstream_tool_input_current_round
                .push_str(&tool_use.input);
        }
        if self.output_token_limit_reached {
            return Vec::new();
        }
        // A trusted application's exact identity reply is a text-only policy
        // result. The real upstream request has already run, but an incidental
        // optional tool call must not leak into that replacement response or
        // leave an open tool block interleaved with the final text block.
        if self.forced_application_identity_reply.is_some() {
            return Vec::new();
        }

        let mut events = Vec::new();
        if self.suppress_text_blocks {
            self.forced_tool_text_pending.clear();
        }
        let tool_identity_options = self
            .thinking_sanitize_options
            .filter(|options| options.protects_private_runtime());

        self.state_manager.set_has_tool_use(true);

        if self.native_reasoning_active {
            events.extend(self.finish_native_reasoning_block());
        }

        // A tool-only upstream response has no assistant text event to trigger
        // the pending synthetic thinking block. Emit it before an ordinary
        // auto-selected tool. A forced-tool response must remain a single
        // index-0 tool block: some Anthropic clients bind JSON deltas to the
        // first content block even though they still recognize a later tool
        // start, which otherwise turns a valid input into an observed `{}`.
        if let Some(synth) = self.pending_synthetic_thinking.take() {
            if !self.suppress_text_blocks {
                events.extend(self.emit_synthetic_thinking_block(&synth));
            }
        }

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                self.accumulate_thinking(&thinking_content);

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if self.expose_thinking && !self.aws_b40_compat {
                    if let Some(thinking_index) = self.thinking_block_index {
                        if let Some(ev) = self.flush_thinking_delta(thinking_index) {
                            events.push(ev);
                        }
                        if self.expose_thinking_text {
                            events.push(self.create_thinking_delta_event(thinking_index, ""));
                        }
                        if let Some(signature) = self.create_signature_delta_event(thinking_index) {
                            events.push(signature);
                        }
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // A new tool block is a hard ordering boundary. Release any short text
        // suffix retained by IdentityOutputSanitizer before allocating/opening
        // the tool block, so the observable order remains
        // text_delta -> text_stop -> tool_use_start. Continuation frames for an
        // already-open tool must not flush unrelated text into that block.
        if !self.tool_block_indices.contains_key(&tool_use.tool_use_id) {
            events.extend(self.flush_pending_identity_text());
            if self.suppress_text_blocks {
                // Forced-tool responses intentionally discard the preamble.
                self.forced_tool_text_pending.clear();
            }
        }

        // 获取或分配块索引
        let (block_index, new_tool_block) =
            if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
                (idx, false)
            } else {
                let idx = self.state_manager.next_block_index();
                self.tool_block_indices
                    .insert(tool_use.tool_use_id.clone(), idx);
                (idx, true)
            };

        // AWS-P rewrites the backend id to Anthropic shape; AWS-B deliberately
        // keeps the Bedrock id as part of its public profile.
        let output_id = if self.aws_b40_compat {
            tool_use.tool_use_id.clone()
        } else {
            self.tool_output_ids
                .entry(tool_use.tool_use_id.clone())
                .or_insert_with(super::id::tool_use_id)
                .clone()
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());
        self.tool_names_by_id
            .entry(tool_use.tool_use_id.clone())
            .or_insert_with(|| tool_use.name.clone());

        // 发送 content_block_start(带 caller,对齐真 Anthropic / 参考渠道的 tool_use 块)
        let mut content_block = json!({
            "type": "tool_use",
            "id": output_id,
            "name": original_name,
            "input": {}
        });
        if !self.aws_b40_compat {
            content_block["caller"] = json!({ "type": "direct" });
        }
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": content_block
            }),
        );
        events.extend(start_events);

        // Bedrock emits an empty JSON delta immediately after opening a tool
        // block, before any argument bytes arrive.
        if self.aws_b40_compat
            && new_tool_block
            && !self.suppress_text_blocks
            && let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": ""
                    }
                }),
            )
        {
            events.push(delta_event);
        }

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token
            self.output_text_acc.push_str(&tool_use.input); // tool 调用 JSON 计入输出(ctoc)
            self.tool_input_acc
                .entry(tool_use.tool_use_id.clone())
                .or_default()
                .push_str(&tool_use.input);
        }
        let mut sanitized_complete_input = None;
        let mut repaired_complete_input = false;
        if tool_use.stop {
            let complete_input = self
                .tool_input_acc
                .remove(&tool_use.tool_use_id)
                .unwrap_or_default();
            let repair_context_present = self.forced_tool_input_repair.is_some();
            let repair_tool_name_match =
                self.forced_tool_input_repair
                    .as_ref()
                    .is_some_and(|repair| {
                        repair.tool_name == tool_use.name || repair.tool_name == original_name
                    });
            let input_json_kind = forced_tool_input_json_kind(&complete_input);
            let repair_input = self
                .forced_tool_input_repair
                .as_ref()
                .filter(|_| {
                    self.suppress_text_blocks
                        && repair_tool_name_match
                        && matches!(input_json_kind, "empty" | "object_empty")
                })
                .and_then(|repair| serde_json::to_string(&repair.input).ok());
            repaired_complete_input = repair_input.is_some();
            if self.suppress_text_blocks {
                tracing::info!(
                    diagnostic = "forced_tool_input_shape",
                    path = "stop_frame",
                    stop = true,
                    repair_context_present,
                    suppress_text_blocks = self.suppress_text_blocks,
                    tool_name_match = repair_tool_name_match,
                    input_bytes = complete_input.len(),
                    trimmed_empty = complete_input.trim().is_empty(),
                    json_kind = input_json_kind,
                    repair_applied = repaired_complete_input,
                    "Forced tool input shape diagnostic"
                );
            }
            let effective_input = repair_input.as_deref().unwrap_or(&complete_input);
            let mut parsed_input = serde_json::from_str::<serde_json::Value>(effective_input).ok();
            if let (Some(options), Some(value)) = (tool_identity_options, parsed_input.as_mut()) {
                super::identity::sanitize_identity_json_value(value, options);
            }
            let canonical_input = parsed_input
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok());
            if (self.aws_b40_compat || tool_identity_options.is_some())
                && let Some(canonical_input) = canonical_input.as_ref()
                && let Some(start) = self.output_text_acc.rfind(&complete_input)
            {
                let canonical_usage_input = format!("{canonical_input}\n");
                self.output_text_acc
                    .replace_range(start..start + complete_input.len(), &canonical_usage_input);
            }
            self.tool_argument_fields += parsed_input
                .as_ref()
                .and_then(|value| value.as_object().map(serde_json::Map::len))
                .unwrap_or(0);
            sanitized_complete_input = canonical_input;
        }

        let argument_deltas = if repaired_complete_input && tool_use.stop {
            self.tool_json_pending.remove(&tool_use.tool_use_id);
            self.tool_json_prefix_split.remove(&tool_use.tool_use_id);
            let safe_input = sanitized_complete_input.unwrap_or_else(|| "{}".to_string());
            if self.aws_b40_compat {
                self.bedrock_tool_argument_deltas_for(&tool_use.tool_use_id, &safe_input, true)
            } else {
                vec![safe_input]
            }
        } else if tool_identity_options.is_some() {
            if !tool_use.stop {
                Vec::new()
            } else {
                self.tool_json_pending.remove(&tool_use.tool_use_id);
                self.tool_json_prefix_split.remove(&tool_use.tool_use_id);
                let safe_input = sanitized_complete_input.unwrap_or_else(|| "{}".to_string());
                if self.aws_b40_compat {
                    self.bedrock_tool_argument_deltas_for(&tool_use.tool_use_id, &safe_input, true)
                } else {
                    vec![safe_input]
                }
            }
        } else if self.aws_b40_compat {
            self.bedrock_tool_argument_deltas(tool_use)
        } else if tool_use.input.is_empty() {
            Vec::new()
        } else {
            vec![tool_use.input.clone()]
        };
        for partial_json in argument_deltas {
            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": partial_json
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop
            && let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index)
        {
            events.push(stop_event);
        }
        if tool_use.stop && !self.continuation_started {
            self.first_round_completed_tool_use = true;
        }

        events
    }

    fn bedrock_tool_argument_deltas(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<String> {
        self.bedrock_tool_argument_deltas_for(&tool_use.tool_use_id, &tool_use.input, tool_use.stop)
    }

    fn bedrock_tool_argument_deltas_for(
        &mut self,
        id: &str,
        input: &str,
        stop: bool,
    ) -> Vec<String> {
        let mut pending = self.tool_json_pending.remove(id).unwrap_or_default();
        pending.push_str(input);
        let mut deltas = Vec::new();

        if !self.tool_json_prefix_split.contains(id)
            && pending.starts_with("{\"")
            && let Some(key_end) = pending[2..].find("\":")
        {
            let mut value_start = 2 + key_end + 2;
            while pending
                .as_bytes()
                .get(value_start)
                .is_some_and(u8::is_ascii_whitespace)
            {
                value_start += 1;
            }
            deltas.push("{\"".to_string());
            deltas.push(pending[2..value_start].to_string());
            pending = pending[value_start..].to_string();
            self.tool_json_prefix_split.insert(id.to_string());
        }

        if stop {
            if !pending.is_empty() {
                deltas.push(pending);
            }
            self.tool_json_prefix_split.remove(id);
        } else {
            self.tool_json_pending.insert(id.to_string(), pending);
        }
        deltas
    }

    fn flush_pending_bedrock_tool_arguments(&mut self) -> Vec<SseEvent> {
        if !self.aws_b40_compat || self.tool_json_pending.is_empty() {
            return Vec::new();
        }

        let mut pending = std::mem::take(&mut self.tool_json_pending)
            .into_iter()
            .collect::<Vec<_>>();
        pending.sort_by_key(|(id, _)| self.tool_block_indices.get(id).copied().unwrap_or(i32::MAX));

        let mut events = Vec::new();
        for (id, input) in pending {
            let Some(block_index) = self.tool_block_indices.get(&id).copied() else {
                self.tool_json_prefix_split.remove(&id);
                continue;
            };
            let complete_input = self
                .tool_input_acc
                .remove(&id)
                .unwrap_or_else(|| input.clone());
            let repair_context_present = self.forced_tool_input_repair.is_some();
            let repair_tool_name_match =
                self.forced_tool_input_repair
                    .as_ref()
                    .is_some_and(|repair| {
                        self.tool_names_by_id.get(&id).is_some_and(|name| {
                            repair.tool_name == *name
                                || self.tool_name_map.get(name) == Some(&repair.tool_name)
                        })
                    });
            let input_json_kind = forced_tool_input_json_kind(&complete_input);
            let repair_input = self
                .forced_tool_input_repair
                .as_ref()
                .filter(|_| {
                    self.suppress_text_blocks
                        && repair_tool_name_match
                        && matches!(input_json_kind, "empty" | "object_empty")
                })
                .and_then(|repair| serde_json::to_string(&repair.input).ok());
            if self.suppress_text_blocks {
                tracing::info!(
                    diagnostic = "forced_tool_input_shape",
                    path = "end_of_stream",
                    stop = false,
                    repair_context_present,
                    suppress_text_blocks = self.suppress_text_blocks,
                    tool_name_match = repair_tool_name_match,
                    input_bytes = complete_input.len(),
                    trimmed_empty = complete_input.trim().is_empty(),
                    json_kind = input_json_kind,
                    repair_applied = repair_input.is_some(),
                    "Forced tool input shape diagnostic"
                );
            }
            let effective_input = repair_input.as_deref().unwrap_or(&input);
            if let Some(repair_input) = repair_input.as_ref() {
                if let Some(start) = self.output_text_acc.rfind(&complete_input) {
                    self.output_text_acc.replace_range(
                        start..start + complete_input.len(),
                        &format!("{repair_input}\n"),
                    );
                }
                self.tool_argument_fields +=
                    serde_json::from_str::<serde_json::Value>(repair_input)
                        .ok()
                        .and_then(|value| value.as_object().map(serde_json::Map::len))
                        .unwrap_or(0);
            }
            for partial_json in self.bedrock_tool_argument_deltas_for(&id, effective_input, true) {
                if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                    block_index,
                    json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial_json
                        }
                    }),
                ) {
                    events.push(delta_event);
                }
            }
        }
        self.state_manager.set_stop_reason("max_tokens");
        events
    }

    fn flush_pending_identity_tool_arguments(&mut self) -> Vec<SseEvent> {
        let Some(options) = self
            .thinking_sanitize_options
            .filter(|options| options.protects_private_runtime())
        else {
            return Vec::new();
        };
        if self.tool_input_acc.is_empty() {
            return Vec::new();
        }

        let mut pending = std::mem::take(&mut self.tool_input_acc)
            .into_iter()
            .collect::<Vec<_>>();
        pending.sort_by_key(|(id, _)| self.tool_block_indices.get(id).copied().unwrap_or(i32::MAX));

        let mut events = Vec::new();
        for (id, raw_input) in pending {
            let Some(block_index) = self.tool_block_indices.get(&id).copied() else {
                continue;
            };
            self.tool_json_pending.remove(&id);
            self.tool_json_prefix_split.remove(&id);

            let mut parsed = serde_json::from_str::<serde_json::Value>(&raw_input).ok();
            if let Some(value) = parsed.as_mut() {
                super::identity::sanitize_identity_json_value(value, options);
            }
            let safe_input = parsed
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok())
                .unwrap_or_else(|| "{}".to_string());

            if let Some(start) = self.output_text_acc.rfind(&raw_input) {
                self.output_text_acc
                    .replace_range(start..start + raw_input.len(), &format!("{safe_input}\n"));
            }
            self.tool_argument_fields += parsed
                .as_ref()
                .and_then(|value| value.as_object().map(serde_json::Map::len))
                .unwrap_or(0);

            let deltas = if self.aws_b40_compat {
                self.bedrock_tool_argument_deltas_for(&id, &safe_input, true)
            } else {
                vec![safe_input]
            };
            for partial_json in deltas {
                if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                    block_index,
                    json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial_json
                        }
                    }),
                ) {
                    events.push(delta_event);
                }
            }
        }
        self.state_manager.set_stop_reason("max_tokens");
        events
    }

    fn context_authoritative_input_tokens(&self) -> Option<i32> {
        let context_input_tokens =
            super::bedrock::context_input_without_current_generation_token_counts(
                self.context_input_tokens,
                super::claude_tok::count_claude(&self.assistant_raw_content),
                self.upstream_reasoning_current_round.count(),
                self.upstream_tool_input_current_round.count(),
            )?;
        Some(if self.aws_b40_compat {
            self.input_context_calibration.calibrate(
                &self.model,
                self.input_tokens,
                Some(context_input_tokens),
            )
        } else {
            super::billing::billable_input_tokens(self.input_tokens, Some(context_input_tokens))
        })
    }

    fn current_billable_input_tokens(&self) -> i32 {
        if let Some(exact) = self.exact_current_input_tokens {
            return exact.max(1);
        }
        if let Some(metered) = self.metered_current_input_tokens {
            return super::bedrock::calibrate_authoritative_input_tokens(
                &self.model,
                self.input_tokens,
                metered,
            );
        }
        if let Some(context) = self.context_authoritative_input_tokens() {
            return context.max(1);
        }
        if self.aws_b40_compat {
            self.input_context_calibration
                .calibrate(&self.model, self.input_tokens, None)
        } else {
            super::billing::billable_input_tokens(self.input_tokens, None)
        }
    }

    fn current_first_round_reconciled_input_tokens(
        &self,
        resolved_input_tokens: i32,
    ) -> Option<i32> {
        select_first_round_reconciled_input(
            self.initial_usage_breakdown,
            self.exact_current_input_tokens,
            self.metered_current_input_tokens,
            self.context_authoritative_input_tokens(),
            resolved_input_tokens,
        )
    }

    fn final_usage_breakdown(&self) -> super::cache::UsageBreakdown {
        self.final_usage_breakdown_uncapped()
    }

    fn exact_total_output_tokens(&self) -> Option<i32> {
        if !self.continuation_started {
            return self.exact_current_output_tokens;
        }
        if !self.exact_output_complete_for_prior_rounds {
            return None;
        }
        self.exact_current_output_tokens.map(|current| {
            self.exact_completed_output_tokens
                .saturating_add(current.max(0))
        })
    }

    fn final_usage_breakdown_uncapped(&self) -> super::cache::UsageBreakdown {
        super::cache::finalize_request_usage(self.initial_usage_breakdown, &self.model)
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let upstream_has_visible_text =
            has_visible_assistant_text(&self.assistant_raw_content, self.thinking_enabled);
        let requested_application_identity_reply = self.forced_application_identity_reply.take();
        let requires_real_gpt_identity_answer = requested_application_identity_reply.is_some()
            || self.strict_gpt_self_identity_target().is_some();
        if self.upstream_fatal_event || !upstream_has_visible_text {
            // Do not let IdentityOutputSanitizer::finish manufacture a
            // canonical GPT answer when the provider returned no answer or a
            // fatal in-band error. Any already-emitted genuine text stays as
            // partial upstream output.
            self.identity_sanitizer = None;
            self.identity_fence_pending.clear();
        }
        if (self.upstream_fatal_event && self.has_gpt_identity_target())
            || (requires_real_gpt_identity_answer
                && !upstream_has_visible_text
                && !self.state_manager.has_tool_use())
        {
            return vec![SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "Upstream model did not return a complete response"
                    }
                }),
            )];
        }
        let forced_application_identity_reply = requested_application_identity_reply
            .filter(|_| upstream_has_visible_text && !self.upstream_fatal_event);
        let forced_application_identity = forced_application_identity_reply.is_some();
        if forced_application_identity {
            // Upstream max-token/context/tool termination describes the
            // hidden response, not the complete policy reply exposed below.
            // A genuinely truncated policy reply will set max_tokens again in
            // apply_output_token_limit.
            self.state_manager.clear_stop_reason();
            self.state_manager.set_stop_reason("end_turn");
        }

        if self.native_reasoning_active {
            events.extend(self.finish_native_reasoning_block());
        }

        if let Some(synth) = self.pending_synthetic_thinking.take() {
            if !self.suppress_text_blocks {
                events.extend(self.emit_synthetic_thinking_block(&synth));
            }
        }

        // Flush thinking_buffer 中的剩余内容。
        // 注意:thinking 内容现在累积在 thinking_pending_raw(延迟到块结束统一清理),
        // 因此即使 thinking_buffer 已被抽空,只要仍在 thinking 块内(需要清理累积内容并关闭块),
        // 也必须进入此分支——否则累积的 thinking 会丢失、块也不会闭合。
        if self.thinking_enabled && (!self.thinking_buffer.is_empty() || self.in_thinking_block) {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    self.accumulate_thinking(&thinking_content);

                    // 关闭 thinking 块：先 flush(清理后)累积内容,再 thinking_delta 空 + signature + stop
                    if self.expose_thinking && !self.aws_b40_compat {
                        if let Some(thinking_index) = self.thinking_block_index {
                            if let Some(ev) = self.flush_thinking_delta(thinking_index) {
                                events.push(ev);
                            }
                            if self.expose_thinking_text {
                                events.push(self.create_thinking_delta_event(thinking_index, ""));
                            }
                            if let Some(signature) =
                                self.create_signature_delta_event(thinking_index)
                            {
                                events.push(signature);
                            }
                            if let Some(stop_event) =
                                self.state_manager.handle_content_block_stop(thinking_index)
                            {
                                events.push(stop_event);
                            }
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 仍在 thinking 块内:把剩余缓冲区累积后统一清理,再关闭 thinking 块
                    let thinking_buffer = self.thinking_buffer.clone();
                    self.accumulate_thinking(&thinking_buffer);
                    if self.expose_thinking && !self.aws_b40_compat {
                        if let Some(thinking_index) = self.thinking_block_index {
                            if let Some(ev) = self.flush_thinking_delta(thinking_index) {
                                events.push(ev);
                            }
                            if self.expose_thinking_text {
                                events.push(self.create_thinking_delta_event(thinking_index, ""));
                            }
                            if let Some(signature) =
                                self.create_signature_delta_event(thinking_index)
                            {
                                events.push(signature);
                            }
                            if let Some(stop_event) =
                                self.state_manager.handle_content_block_stop(thinking_index)
                            {
                                events.push(stop_event);
                            }
                        }
                    }
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        if let Some(reply) = forced_application_identity_reply {
            events.extend(self.create_text_delta_events(&reply));
        }
        events.extend(self.flush_pending_identity_text());

        if self.suppress_text_blocks
            && self.tool_block_indices.is_empty()
            && !self.forced_tool_text_pending.is_empty()
        {
            self.suppress_text_blocks = false;
            let pending = std::mem::take(&mut self.forced_tool_text_pending);
            events.extend(self.emit_text_delta_events(&pending));
        }
        events.extend(self.flush_pending_stop_sequence_text());

        // If the upstream transport ends before its final tool `stop=true`
        // frame, emit every argument byte already received before closing the
        // block. The response remains explicitly truncated via max_tokens.
        events.extend(self.flush_pending_identity_tool_arguments());
        events.extend(self.flush_pending_bedrock_tool_arguments());

        let current_stop_reason = self.state_manager.get_stop_reason();
        let normalized_stop_reason = normalize_stop_reason_for_completed_tool_use(
            &current_stop_reason,
            self.first_round_completed_tool_use,
        );
        if normalized_stop_reason != current_stop_reason {
            self.state_manager.set_stop_reason(normalized_stop_reason);
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.expose_thinking
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
            && !self.native_stop_reason_received
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.emit_text_delta_events(" "));
        }

        // 自动续写属于服务端内部调用，不得增加客户端这一次请求的输入 usage。
        let breakdown = self.final_usage_breakdown();

        // 最终 output_tokens 用 ctoc 对累积的完整输出文本算一次(贪心跨块不可加,故不能逐块累加)。
        // self.output_tokens(字符估算)仅用于流中途的限长/续写判断。
        // 限长是按字符估算截断的,与 ctoc 口径不同,故上报值封顶到客户请求的上限(真 Anthropic 的
        // output_tokens 也不会超过 max_tokens)。
        let ctoc_output_tokens = match self.output_token_limit {
            Some(limit) => super::claude_tok::count_claude(&self.output_text_acc).min(limit.max(0)),
            None => super::claude_tok::count_claude(&self.output_text_acc),
        };
        let base_visible_output_tokens = if ctoc_output_tokens > 0 {
            match self.output_token_limit {
                Some(limit) if limit < 4 => ctoc_output_tokens,
                _ => ctoc_output_tokens.max(4),
            }
        } else {
            0
        };
        let visible_output_tokens = if self.aws_b40_compat
            && self.tool_block_indices.is_empty()
            && !self.output_text_acc.is_empty()
        {
            super::bedrock::framed_text_output_tokens(
                &self.output_text_acc,
                base_visible_output_tokens,
            )
        } else if self.aws_b40_compat {
            super::bedrock::framed_output_tokens_with_tool_arguments(
                base_visible_output_tokens,
                self.state_manager.active_blocks.len(),
                self.tool_block_indices.len(),
                self.tool_argument_fields,
            )
        } else {
            base_visible_output_tokens
        };
        let thinking_usage_tokens = self.compat_thinking_usage_tokens();
        // 请求开启 thinking 即在 message_delta 带 output_tokens_details（与真 Anthropic 一致），
        // 即便本轮无思考也显示 thinking_tokens:0。-1 = "包含但显示 0" 的 sentinel。
        let usage_thinking_tokens = if self.thinking_enabled && thinking_usage_tokens == 0 {
            -1
        } else {
            thinking_usage_tokens
        };
        let uncapped_output_tokens = visible_output_tokens
            + thinking_usage_tokens
            + if thinking_usage_tokens > 0 { 2 } else { 0 };
        let locally_counted_output_tokens = self
            .output_token_limit
            .map(|limit| uncapped_output_tokens.min(limit.max(1)))
            .unwrap_or(uncapped_output_tokens);
        let final_output_tokens =
            if !self.output_token_limit_reached && !forced_application_identity {
                self.exact_total_output_tokens()
                    .map(|exact| {
                        self.output_token_limit
                            .map(|limit| exact.min(limit.max(1)))
                            .unwrap_or(exact)
                    })
                    .unwrap_or(locally_counted_output_tokens)
            } else {
                locally_counted_output_tokens
            };
        if self
            .output_token_limit
            .is_some_and(|limit| final_output_tokens >= limit.max(1))
            && !self.state_manager.has_tool_use()
            && !forced_application_identity
            && !self.native_stop_reason_received
            && self.state_manager.get_stop_reason() != "stop_sequence"
        {
            self.state_manager.set_stop_reason("max_tokens");
        }
        let mut final_events = self.state_manager.generate_final_events(
            breakdown,
            final_output_tokens,
            &self.model,
            usage_thinking_tokens,
        );
        if self.aws_b40_compat {
            let include_bedrock_invocation_metrics = !self.has_gpt_identity_target();
            let invocation_latency = self
                .upstream_request_latency_ms
                .saturating_add(self.stream_started_at.elapsed().as_millis() as u64);
            let first_byte_latency = self
                .first_byte_latency_ms
                .unwrap_or(invocation_latency)
                .min(invocation_latency);
            for event in &mut final_events {
                if event.event == "message_delta" {
                    event.data["usage"] = super::bedrock::stream_delta_usage(
                        &self.model,
                        breakdown,
                        final_output_tokens,
                        usage_thinking_tokens,
                    );
                } else if event.event == "message_stop" && include_bedrock_invocation_metrics {
                    event.data = json!({
                        "type": "message_stop",
                        "amazon-bedrock-invocationMetrics": super::bedrock::invocation_metrics(
                            breakdown,
                            final_output_tokens,
                            invocation_latency,
                            first_byte_latency
                        )
                    });
                }
            }
        }
        events.extend(final_events);
        events
    }

    fn compat_thinking_usage_tokens(&self) -> i32 {
        // 标签式/synthetic thinking 仍在 thinking_text_acc；原生 reasoning 在块结束时
        // 已折叠为数字。隐藏 reasoning 没有可见块结束回调，最后从当前轮原文计数。
        let unaccounted_native = if self.native_reasoning_usage_accounted {
            0
        } else {
            self.upstream_reasoning_current_round.count()
        };
        let ctoc_thinking = self
            .completed_native_thinking_tokens
            .saturating_add(super::claude_tok::count_claude(&self.thinking_text_acc))
            .saturating_add(unaccounted_native);
        if ctoc_thinking > 0 {
            ctoc_thinking + 6
        } else {
            0
        }
    }

    /// 当前响应是否适合自动续写。
    ///
    /// 只在纯文本/思考输出因 max_tokens 停止时续写；工具调用中续写可能重复执行工具，
    /// 因此显式禁用。
    pub fn should_auto_continue(&self, requested_max_tokens: i32) -> bool {
        self.forced_application_identity_reply.is_none()
            && requested_max_tokens > 8192
            && self.state_manager.get_stop_reason() == "max_tokens"
            && !self.state_manager.has_tool_use()
            && !self.assistant_raw_content.trim().is_empty()
            && !self.output_token_limit_reached
            && self.output_tokens < requested_max_tokens
    }

    #[cfg(test)]
    pub fn should_probe_auto_continue(&self, requested_max_tokens: i32) -> bool {
        let _ = requested_max_tokens;
        false
    }

    pub fn take_assistant_raw_content_for_continuation(&mut self) -> String {
        self.state_manager.clear_stop_reason();
        let content = std::mem::take(&mut self.assistant_raw_content);
        self.continuation_merge_tail = Some(content_tail(&content, 4096));
        content
    }

    pub fn assistant_raw_content(&self) -> &str {
        &self.assistant_raw_content
    }

    pub fn begin_continuation_for_billing(&mut self, next_estimated_input_tokens: i32) {
        let current_input_tokens = self.current_billable_input_tokens();
        if self.continuation_started {
            self.additional_round_input_tokens
                .push(current_input_tokens);
        } else {
            self.first_round_authoritative_input_tokens =
                self.current_first_round_reconciled_input_tokens(current_input_tokens);
            self.continuation_started = true;
        }
        if let Some(output_tokens) = self.exact_current_output_tokens {
            self.exact_completed_output_tokens = self
                .exact_completed_output_tokens
                .saturating_add(output_tokens.max(0));
        } else {
            self.exact_output_complete_for_prior_rounds = false;
        }
        self.input_tokens = next_estimated_input_tokens.max(1);
        self.context_input_tokens = None;
        self.exact_current_input_tokens = None;
        self.metered_current_input_tokens = None;
        self.exact_current_output_tokens = None;
        if !self.native_reasoning_usage_accounted {
            self.completed_native_thinking_tokens = self
                .completed_native_thinking_tokens
                .saturating_add(self.upstream_reasoning_current_round.count());
        }
        self.upstream_reasoning_current_round.reset();
        self.native_reasoning_usage_accounted = false;
        self.upstream_tool_input_current_round.reset();
        self.native_stop_reason_received = false;
    }

    #[allow(dead_code)]
    pub fn begin_completion_probe_for_billing(&mut self, next_estimated_input_tokens: i32) {
        self.begin_continuation_for_billing(next_estimated_input_tokens);
        self.swallow_complete_sentinel_probe = true;
    }

    pub fn mark_upstream_truncated(&mut self) {
        self.mark_upstream_fatal_event();
    }

    pub fn mark_upstream_fatal_event(&mut self) {
        self.upstream_fatal_event = true;
        self.state_manager.set_stop_reason("max_tokens");
    }

    /// A cache prefix becomes warm only after a complete, non-fatal upstream
    /// invocation.  HTTP 200 alone is insufficient because Kiro may carry an
    /// error or exception inside the EventStream body.
    pub fn upstream_succeeded_for_cache(&self) -> bool {
        if self.upstream_fatal_event {
            return false;
        }

        // Both streaming handlers call this predicate only after the upstream
        // body reached EOF and the EventStream decoder has no pending frame.
        // Opus 5 sometimes completes with only an assistantResponseEvent, so
        // non-empty visible output is sufficient in that clean-EOF shape.
        let has_clean_opus_5_assistant_response = stream_model_is_opus_5(&self.model)
            && !self.output_text_acc.trim().is_empty()
            && !self.output_token_limit_reached
            && !self.first_round_unknown_native_stop_reason;
        let has_completion_evidence = self.first_round_completed_tool_use
            || self.first_round_terminal_evidence
            || has_clean_opus_5_assistant_response;
        if !has_completion_evidence {
            return false;
        }

        // Refusals do not warm a speculative local cache entry. Remote token
        // buckets are intentionally irrelevant to this decision.
        if self.first_round_refused {
            return false;
        }

        true
    }

    fn apply_output_token_limit(&mut self, text: &str) -> Option<String> {
        let Some(limit) = self.output_token_limit else {
            return Some(text.to_string());
        };

        let remaining = limit - self.output_tokens;
        if remaining <= 0 {
            self.output_token_limit_reached = true;
            self.state_manager.set_stop_reason("max_tokens");
            return None;
        }

        let tokens = estimate_tokens(text);
        if tokens <= remaining {
            return Some(text.to_string());
        }

        let (limited, _) = truncate_to_estimated_token_limit(text, remaining);
        self.output_token_limit_reached = true;
        self.state_manager.set_stop_reason("max_tokens");
        Some(limited)
    }
}

fn stream_model_is_opus_5(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("opus-5") || lower.contains("opus-5.0") || lower.contains("opus 5")
}

pub(crate) fn cumulative_event_delta(current: &str, previous: &str) -> String {
    if previous.is_empty() {
        return current.to_string();
    }
    if current == previous || previous.starts_with(current) {
        return String::new();
    }
    if let Some(delta) = current.strip_prefix(previous) {
        return delta.to_string();
    }

    let max_overlap = previous.len().min(current.len()).min(4096);
    for overlap in (1..=max_overlap).rev() {
        let previous_start = previous.len() - overlap;
        if previous.is_char_boundary(previous_start)
            && current.is_char_boundary(overlap)
            && previous[previous_start..] == current[..overlap]
        {
            return current[overlap..].to_string();
        }
    }
    current.to_string()
}

fn split_text_by_char_lengths(text: &str, lengths: &[usize]) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if lengths.is_empty() {
        return vec![text.to_string()];
    }

    let mut boundaries = Vec::with_capacity(text.chars().count() + 1);
    boundaries.push(0);
    boundaries.extend(text.char_indices().skip(1).map(|(index, _)| index));
    boundaries.push(text.len());

    let char_count = boundaries.len() - 1;
    let mut cursor = 0usize;
    let mut chunks = Vec::new();
    for length in lengths {
        if cursor >= char_count {
            break;
        }
        let end = cursor.saturating_add(*length).min(char_count);
        if end > cursor {
            chunks.push(text[boundaries[cursor]..boundaries[end]].to_string());
            cursor = end;
        }
    }
    if cursor < char_count {
        if let Some(last) = chunks.last_mut() {
            last.push_str(&text[boundaries[cursor]..]);
        } else {
            chunks.push(text.to_string());
        }
    }
    chunks
}

fn unknown_event_field_names(payload: &[u8]) -> Vec<String> {
    let Ok(serde_json::Value::Object(fields)) =
        serde_json::from_slice::<serde_json::Value>(payload)
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = fields.into_iter().map(|(name, _)| name).collect();
    names.sort();
    names.truncate(32);
    names
}

pub(crate) trait IntoInitialUsageBreakdown {
    fn into_breakdown(self, input_tokens: i32) -> super::cache::UsageBreakdown;
}

impl IntoInitialUsageBreakdown for super::cache::UsageBreakdown {
    fn into_breakdown(self, _input_tokens: i32) -> super::cache::UsageBreakdown {
        self
    }
}

impl IntoInitialUsageBreakdown for bool {
    fn into_breakdown(self, input_tokens: i32) -> super::cache::UsageBreakdown {
        super::cache::compute_usage_breakdown(input_tokens, self)
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束。
/// 输入 usage 在请求发往上游前已经确定；缓冲只改变事件发送时机。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，确保 `message_start` 与最终 usage 使用同一本地快照
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        initial_usage_breakdown: impl IntoInitialUsageBreakdown,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner = StreamContext::new_with_thinking(
            model,
            estimated_input_tokens,
            thinking_enabled,
            initial_usage_breakdown,
            tool_name_map,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            initial_events_generated: false,
        }
    }

    pub fn hide_thinking_blocks(&mut self) {
        self.inner.hide_thinking_blocks();
    }

    pub fn set_thinking_text_visible(&mut self, visible: bool) {
        self.inner.set_thinking_text_visible(visible);
    }

    pub fn set_synthetic_thinking(&mut self, thinking: Option<String>) {
        self.inner.set_synthetic_thinking(thinking);
    }

    pub fn set_stop_sequences(&mut self, sequences: Vec<String>) {
        self.inner.set_stop_sequences(sequences);
    }

    pub fn enable_aws_b40_compat(&mut self) {
        self.inner.enable_aws_b40_compat();
    }

    pub fn set_aws_b40_thinking_requested(&mut self, requested: bool) {
        self.inner.set_aws_b40_thinking_requested(requested);
    }

    pub fn set_input_context_calibration(
        &mut self,
        calibration: super::bedrock::InputContextCalibration,
    ) {
        self.inner.set_input_context_calibration(calibration);
    }

    pub fn set_exact_cache_ttl_plan(&mut self, plan: super::cache::ExactCacheTtlPlan) {
        self.inner.set_exact_cache_ttl_plan(plan);
    }

    pub(super) fn take_native_signature_registration(
        &mut self,
    ) -> Option<(String, String, String)> {
        self.inner.take_native_signature_registration()
    }

    pub fn set_suppress_text_blocks(&mut self, suppress: bool) {
        self.inner.set_suppress_text_blocks(suppress);
    }

    pub(crate) fn set_forced_tool_input_repair(&mut self, repair: Option<ForcedToolInputRepair>) {
        self.inner.set_forced_tool_input_repair(repair);
    }

    pub fn set_upstream_request_latency(&mut self, elapsed: Duration) {
        self.inner.set_upstream_request_latency(elapsed);
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用原始请求的本地 usage 快照统一 message_start
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 获取原始请求侧的确定性 input/cache 拆分；不计内部自动续写。
        let breakdown = self.inner.final_usage_breakdown();

        // 更正 message_start 事件中的 usage（input + cache 字段全部按拆分后回填）
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(breakdown.input_tokens);
                        usage["cache_creation_input_tokens"] =
                            serde_json::json!(breakdown.cache_creation_input_tokens);
                        usage["cache_read_input_tokens"] =
                            serde_json::json!(breakdown.cache_read_input_tokens);
                        usage["cache_creation"] = serde_json::json!({
                            "ephemeral_5m_input_tokens": breakdown.cache_creation_5m_input_tokens,
                            "ephemeral_1h_input_tokens": breakdown.cache_creation_1h_input_tokens
                        });
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }

    pub fn should_auto_continue(&self, requested_max_tokens: i32) -> bool {
        self.inner.should_auto_continue(requested_max_tokens)
    }

    pub fn set_output_token_limit(&mut self, max_tokens: i32) {
        self.inner.set_output_token_limit(max_tokens);
    }

    pub fn take_assistant_raw_content_for_continuation(&mut self) -> String {
        self.inner.take_assistant_raw_content_for_continuation()
    }

    pub fn assistant_raw_content(&self) -> &str {
        self.inner.assistant_raw_content()
    }

    pub fn begin_continuation_for_billing(&mut self, next_estimated_input_tokens: i32) {
        self.inner
            .begin_continuation_for_billing(next_estimated_input_tokens);
    }

    pub fn upstream_succeeded_for_cache(&self) -> bool {
        self.inner.upstream_succeeded_for_cache()
    }

    #[allow(dead_code)]
    pub fn enable_identity_sanitization(&mut self) {
        self.enable_identity_sanitization_with_strict_mode(true);
    }

    pub fn enable_identity_sanitization_with_strict_mode(&mut self, strict_identity_context: bool) {
        self.inner
            .enable_identity_sanitization_with_strict_mode(strict_identity_context);
    }

    #[allow(dead_code)]
    pub fn enable_identity_sanitization_with_options(
        &mut self,
        strict_identity_context: bool,
        agentic_ide_probe: bool,
        codewhisperer_relationship_probe: bool,
        vendor_lineage_probe: bool,
        obfuscated_private_thinking_probe: bool,
        third_party_kiro_discussion: bool,
    ) {
        self.inner.enable_identity_sanitization_with_options(
            strict_identity_context,
            agentic_ide_probe,
            codewhisperer_relationship_probe,
            vendor_lineage_probe,
            obfuscated_private_thinking_probe,
            third_party_kiro_discussion,
        );
    }

    pub fn enable_identity_sanitization_with_profile(
        &mut self,
        options: super::identity::IdentitySanitizationOptions,
    ) {
        self.inner
            .enable_identity_sanitization_with_profile(options);
    }

    pub fn set_forced_application_identity_reply(&mut self, reply: Option<String>) {
        self.inner.set_forced_application_identity_reply(reply);
    }

    #[allow(dead_code)]
    pub fn begin_completion_probe_for_billing(&mut self, next_estimated_input_tokens: i32) {
        self.inner
            .begin_completion_probe_for_billing(next_estimated_input_tokens);
    }

    pub fn mark_upstream_truncated(&mut self) {
        self.inner.mark_upstream_truncated();
    }

    pub fn mark_upstream_fatal_event(&mut self) {
        self.inner.mark_upstream_fatal_event();
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

fn truncate_to_estimated_token_limit(text: &str, max_tokens: i32) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    if max_tokens <= 0 {
        return (String::new(), true);
    }
    if estimate_tokens(text) <= max_tokens {
        return (text.to_string(), false);
    }

    let mut candidate = String::new();
    let mut last_good = String::new();
    for ch in text.chars() {
        candidate.push(ch);
        if estimate_tokens(&candidate) > max_tokens {
            return (last_good, true);
        }
        last_good = candidate.clone();
    }

    (candidate, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
        assert!(!sse_str.ends_with("\n\n\n"));
        assert!(event.to_profile_sse_string(true).ends_with("\n\n\n"));
    }

    #[test]
    fn cache_commit_requires_model_completion_evidence() {
        let mut empty =
            StreamContext::new_with_thinking("test-model", 100, false, false, HashMap::new());
        assert!(!empty.upstream_succeeded_for_cache());

        let _ = empty.process_assistant_response("real upstream output");
        assert!(
            !empty.upstream_succeeded_for_cache(),
            "partial text followed by a clean EOF is not proof of completion"
        );

        let _ = empty.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent::default(),
        ));
        assert!(empty.upstream_succeeded_for_cache());

        empty.mark_upstream_fatal_event();
        assert!(!empty.upstream_succeeded_for_cache());
    }

    #[test]
    fn native_metadata_stop_reasons_are_preserved_in_stream_delta() {
        for (native, expected) in [
            ("END_TURN", "end_turn"),
            ("MAX_TOKENS", "max_tokens"),
            ("TOOL_USE", "tool_use"),
            ("STOP_SEQUENCE", "stop_sequence"),
            ("REFUSAL", "refusal"),
            (
                "MODEL_CONTEXT_WINDOW_EXCEEDED",
                "model_context_window_exceeded",
            ),
        ] {
            let mut ctx =
                StreamContext::new_with_thinking("test-model", 100, false, false, HashMap::new());
            let _ = ctx.process_kiro_event(&Event::Metadata(
                crate::kiro::model::events::MetadataEvent {
                    stop_reason: Some(native.to_string()),
                    token_usage: Default::default(),
                },
            ));

            let events = ctx.generate_final_events();
            let delta = events
                .iter()
                .find(|event| event.event == "message_delta")
                .expect("message_delta");
            assert_eq!(delta.data["delta"]["stop_reason"], expected, "{native}");
            assert!(delta.data["delta"]["stop_details"].is_null());
        }
    }

    #[test]
    fn completed_tool_use_overrides_native_end_turn_in_stream_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 100, false, false, HashMap::new());
        let _ = ctx.process_kiro_event(&Event::ToolUse(crate::kiro::model::events::ToolUseEvent {
            name: "Read".to_string(),
            tool_use_id: "call_file_read".to_string(),
            input: r#"{"file_path":"README.md"}"#.to_string(),
            stop: true,
        }));
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("END_TURN".to_string()),
                token_usage: Default::default(),
            },
        ));

        let events = ctx.generate_final_events();
        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");

        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn unknown_native_stop_reason_fails_closed_without_cache_evidence() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-5", 100, false, false, HashMap::new());
        let _ = ctx.process_assistant_response("complete-looking output");
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("FUTURE_PROVIDER_REASON".to_string()),
                token_usage: Default::default(),
            },
        ));

        assert_eq!(ctx.state_manager.get_stop_reason(), "end_turn");
        assert!(!ctx.first_round_terminal_evidence);
        assert!(!ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn refusal_does_not_warm_fallback_cache_even_with_generic_metering() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 100, false, false, HashMap::new());
        let _ = ctx.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent::default(),
        ));
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("REFUSAL".to_string()),
                token_usage: Default::default(),
            },
        ));

        assert!(ctx.first_round_terminal_evidence);
        assert!(ctx.first_round_refused);
        assert_eq!(ctx.state_manager.get_stop_reason(), "refusal");
        assert!(!ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn invalid_state_after_terminal_accounting_prevents_cache_commit() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 100, false, false, HashMap::new());
        let _ = ctx.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent::default(),
        ));
        assert!(ctx.upstream_succeeded_for_cache());

        let _ = ctx.process_kiro_event(&Event::InvalidState(
            crate::kiro::model::events::InvalidStateEvent {
                reason: "CONTENT_LENGTH_EXCEEDS_THRESHOLD".to_string(),
                message: "request state rejected".to_string(),
            },
        ));
        assert!(!ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn remote_cache_usage_cannot_veto_local_registry_commit() {
        let request = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "cache this terminal prefix",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .unwrap();
        let plan =
            super::super::cache::prepare_cache_commit(5_000, &request, true).exact_ttl_plan();
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            5_000,
            false,
            super::super::cache::UsageBreakdown::flat(5_000),
            HashMap::new(),
        );
        ctx.set_exact_cache_ttl_plan(plan);
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("END_TURN".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 0,
                    cache_read_input_tokens: 999_999,
                    cache_write_input_tokens: 0,
                    output_tokens: 1,
                    total_tokens: 1_000_000,
                },
            },
        ));

        assert_eq!(
            ctx.exact_first_round_usage,
            Some(super::super::cache::UsageBreakdown::flat(1))
        );
        assert!(ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn content_length_terminal_event_can_complete_cache_write() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 100, false, false, HashMap::new());
        let _ = ctx.process_assistant_response("complete prefix before native output limit");
        let _ = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "output limit reached".to_string(),
        });

        assert!(ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn exact_zero_cache_metadata_cannot_prevent_local_registry_warming() {
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 99,
            cache_creation_5m_input_tokens: 99,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            100,
            false,
            initial,
            HashMap::new(),
        );
        let _ = ctx.process_assistant_response("completed output");
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("END_TURN".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 100,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 2,
                    total_tokens: 102,
                },
            },
        ));

        assert!(ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn sonnet_repeated_native_write_preserves_shared_cache_read_in_stream() {
        let request = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-sonnet-5",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "stable Sonnet cache prefix",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .expect("cache request parses");
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 182,
            cache_read_input_tokens: 89_520,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let plan =
            super::super::cache::prepare_cache_commit(89_702, &request, true).exact_ttl_plan();
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-5",
            89_702,
            false,
            initial,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_exact_cache_ttl_plan(plan);
        let _ = ctx.process_assistant_response("completed output");
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("END_TURN".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 182,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 89_520,
                    output_tokens: 72,
                    total_tokens: 89_774,
                },
            },
        ));

        assert_eq!(ctx.exact_current_input_tokens, Some(89_702));
        assert_eq!(ctx.exact_first_round_usage, None);
        assert_eq!(ctx.final_usage_breakdown(), initial);
        assert!(ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn exact_cache_activity_allows_successful_registry_refresh() {
        let request = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "cache this terminal native prefix",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .expect("cache request parses");
        let exact_plan =
            super::super::cache::prepare_cache_commit(100, &request, true).exact_ttl_plan();
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 99,
            cache_creation_5m_input_tokens: 99,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 100, false, initial, HashMap::new());
        ctx.set_exact_cache_ttl_plan(exact_plan);
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("END_TURN".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 1,
                    cache_read_input_tokens: 99,
                    cache_write_input_tokens: 0,
                    output_tokens: 2,
                    total_tokens: 102,
                },
            },
        ));

        assert!(ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn refusal_does_not_warm_local_registry_even_with_native_cache_read() {
        let request = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "cache this prefix even when policy refuses the answer",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .expect("cache request parses");
        let exact_plan =
            super::super::cache::prepare_cache_commit(100, &request, true).exact_ttl_plan();
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 99,
            cache_creation_5m_input_tokens: 99,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            100,
            false,
            initial,
            HashMap::new(),
        );
        ctx.set_exact_cache_ttl_plan(exact_plan);
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("REFUSAL".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 1,
                    cache_read_input_tokens: 99,
                    cache_write_input_tokens: 0,
                    output_tokens: 2,
                    total_tokens: 102,
                },
            },
        ));

        let exact = ctx
            .exact_first_round_usage
            .expect("native cache accounting must be retained");
        assert_eq!(exact.input_tokens, 1);
        assert_eq!(exact.cache_read_input_tokens, 99);
        assert_eq!(ctx.state_manager.get_stop_reason(), "refusal");
        assert!(!ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn refusal_exact_cache_write_does_not_warm_fallback_registry() {
        let request = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-opus-5",
            "max_tokens": 32,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "cacheable prefix rejected before a reusable entry is committed",
                    "cache_control": {"type": "ephemeral"}
                }]
            }]
        }))
        .expect("cache request parses");
        let exact_plan =
            super::super::cache::prepare_cache_commit(100, &request, true).exact_ttl_plan();
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 99,
            cache_creation_5m_input_tokens: 99,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-5", 100, false, initial, HashMap::new());
        ctx.set_exact_cache_ttl_plan(exact_plan);
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("REFUSAL".to_string()),
                token_usage: crate::kiro::model::events::TokenUsage {
                    uncached_input_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 99,
                    output_tokens: 2,
                    total_tokens: 102,
                },
            },
        ));

        let exact = ctx
            .exact_first_round_usage
            .expect("native cache accounting must remain visible");
        assert_eq!(exact.cache_creation_input_tokens, 99);
        assert_eq!(ctx.state_manager.get_stop_reason(), "refusal");
        assert!(!ctx.upstream_succeeded_for_cache());
    }

    #[test]
    fn aws_b_transport_chunks_preserve_text_and_stay_bounded() {
        let text = "Claude can explain code safely. ".repeat(200);
        let chunks = text_delta_chunks(&text);

        assert_eq!(chunks.concat(), text);
        assert_eq!(chunks.len(), AWS_B_TEXT_DELTA_MAX_PARTS);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    }

    #[test]
    fn aws_b_modern_synthetic_thinking_emits_only_a_signed_envelope() {
        let thinking = "Inspect the request, calculate the result, and answer clearly.";
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();

        let events = ctx.emit_synthetic_thinking_block(thinking);
        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "thinking"
        }));
        assert!(!events.iter().any(|event| {
            event.data["delta"]["type"] == "thinking_delta"
                && !event.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
        }));
        let signature = events
            .iter()
            .find(|event| event.data["delta"]["type"] == "signature_delta")
            .and_then(|event| event.data["delta"]["signature"].as_str())
            .expect("modern fallback must emit a model-bound signature");
        assert!(super::super::signature::validate_signature(signature).is_ok());
        assert!(ctx.thinking_text_acc.is_empty());
        assert_eq!(ctx.thinking_tokens, 0);
    }

    #[test]
    fn aws_b_opus_4_5_synthetic_thinking_emits_a_valid_signature() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-5-20251101",
            1,
            true,
            true,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();

        let events = ctx.emit_synthetic_thinking_block("Plan carefully");
        let signature = events
            .iter()
            .find(|event| event.data["delta"]["type"] == "signature_delta")
            .and_then(|event| event.data["delta"]["signature"].as_str())
            .expect("Opus 4.5 explicit thinking must be signed");
        assert!(super::super::signature::validate_signature(signature).is_ok());
    }

    #[test]
    fn aws_b_legacy_opus_fallback_emits_only_model_bound_signature() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-7", 17, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();

        let events = ctx.emit_synthetic_thinking_block("gateway fallback text");
        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "thinking"
        }));
        assert!(!events.iter().any(|event| {
            event.data["delta"]["type"] == "thinking_delta"
                && !event.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
        }));
        let signature = events
            .iter()
            .find(|event| event.data["delta"]["type"] == "signature_delta")
            .and_then(|event| event.data["delta"]["signature"].as_str())
            .expect("model-bound signature");
        assert!(super::super::signature::validate_signature(signature).is_ok());
        assert_eq!(
            ctx.take_native_signature_registration(),
            Some((
                "claude-opus-4-7".to_string(),
                String::new(),
                signature.to_string(),
            ))
        );
    }

    #[test]
    fn aws_b_modern_model_signs_native_reasoning_when_upstream_signature_is_missing() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-5", 17, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_aws_b40_thinking_requested(true);
        ctx.set_thinking_text_visible(true);
        let _ = ctx.generate_initial_events();

        let pending = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "Plan carefully".to_string(),
            ..Default::default()
        }));
        assert!(
            pending.is_empty(),
            "unsigned reasoning must remain buffered"
        );

        let events = ctx.generate_final_events();
        let start = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "thinking"
            })
            .expect("explicit thinking must produce a thinking block");
        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.data["delta"]["thinking"].as_str())
                .collect::<String>(),
            "Plan carefully"
        );
        let (signature_position, signature) = events
            .iter()
            .enumerate()
            .find_map(|(position, event)| {
                (event.data["delta"]["type"] == "signature_delta")
                    .then(|| event.data["delta"]["signature"].as_str())
                    .flatten()
                    .map(|signature| (position, signature))
            })
            .expect("explicit thinking must produce a signature delta");
        assert!(super::super::signature::validate_signature(signature).is_ok());
        let stop = events
            .iter()
            .position(|event| event.event == "content_block_stop" && event.data["index"] == 0)
            .expect("thinking block must be closed");
        assert!(start < signature_position && signature_position < stop);
    }

    #[test]
    fn aws_p_synthetic_thinking_keeps_single_delta() {
        let thinking = "Inspect the request, calculate the result, and answer clearly.";
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());

        let events = ctx.emit_synthetic_thinking_block(thinking);
        let deltas = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "thinking_delta")
            .collect::<Vec<_>>();

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].data["delta"]["thinking"], thinking);
        assert_eq!(ctx.thinking_text_acc, thinking);
        assert_eq!(ctx.thinking_tokens, estimate_tokens(thinking));
    }

    #[test]
    fn cumulative_reasoning_chunks_become_nonduplicated_deltas() {
        assert_eq!(cumulative_event_delta("Plan", ""), "Plan");
        assert_eq!(
            cumulative_event_delta("Plan carefully", "Plan"),
            " carefully"
        );
        assert_eq!(
            cumulative_event_delta("carefully and answer", "Plan carefully"),
            " and answer"
        );
        assert_eq!(
            cumulative_event_delta("Plan", "Plan carefully"),
            String::new()
        );
    }

    #[test]
    fn private_gpt_native_reasoning_is_hidden_but_counted_in_usage() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 1, false, false, HashMap::new());
        let _ = ctx.generate_initial_events();
        let mut reasoning_events =
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                text: "Plan".to_string(),
                ..Default::default()
            }));
        reasoning_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            ReasoningContentEvent {
                text: "Plan carefully".to_string(),
                ..Default::default()
            },
        )));

        assert!(
            reasoning_events.is_empty(),
            "private GPT reasoning must never be emitted"
        );
        assert_eq!(
            ctx.upstream_reasoning_current_round.count(),
            super::super::claude_tok::count_claude("Plan carefully")
        );
        assert!(
            ctx.thinking_text_acc.is_empty(),
            "hidden reasoning must not be duplicated solely for usage accounting"
        );

        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert!(
            delta.data["usage"]["output_tokens_details"]["thinking_tokens"]
                .as_i64()
                .is_some_and(|tokens| tokens > 0),
            "private reasoning must still be represented in usage"
        );
    }

    #[test]
    fn native_reasoning_replaces_synthetic_and_preserves_upstream_signature() {
        use crate::kiro::model::events::{AssistantResponseEvent, ReasoningContentEvent};

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));
        let mut events = ctx.generate_initial_events();
        events.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                text: "Plan".to_string(),
                ..Default::default()
            })),
        );
        events.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                text: "Plan carefully".to_string(),
                ..Default::default()
            })),
        );
        events.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                signature: "upstream-opaque-signature".to_string(),
                ..Default::default()
            })),
        );
        let response: AssistantResponseEvent =
            serde_json::from_value(json!({"content":"Final answer."})).unwrap();
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(response)));

        let thinking = events
            .iter()
            .filter_map(|event| {
                (event.data["delta"]["type"] == "thinking_delta")
                    .then(|| event.data["delta"]["thinking"].as_str())
                    .flatten()
            })
            .collect::<String>();
        assert_eq!(thinking, "Plan carefully");
        assert!(!thinking.contains("synthetic"));
        assert_eq!(
            events
                .iter()
                .find(|event| event.data["delta"]["type"] == "signature_delta")
                .and_then(|event| event.data["delta"]["signature"].as_str()),
            Some("upstream-opaque-signature")
        );
        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.data["delta"]["text"].as_str())
                .collect::<String>(),
            "Final answer."
        );
    }

    #[test]
    fn aws_b_native_signature_registration_binds_exact_emitted_thinking() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "Plan carefully".to_string(),
            ..Default::default()
        }));
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            signature: "upstream-opaque-signature".to_string(),
            ..Default::default()
        }));

        assert!(events.iter().any(|event| {
            event.event == "content_block_delta" && event.data["delta"]["type"] == "signature_delta"
        }));
        assert_eq!(
            ctx.take_native_signature_registration(),
            Some((
                "claude-opus-4-8".to_string(),
                "Plan carefully".to_string(),
                "upstream-opaque-signature".to_string(),
            ))
        );
        assert!(ctx.take_native_signature_registration().is_none());
    }

    #[test]
    fn completed_native_reasoning_keeps_at_most_one_full_text_copy() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let reasoning = "reason carefully ".repeat(64 * 1024);
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_thinking_text_visible(true);
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: reasoning.clone(),
            ..Default::default()
        }));
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            signature: "upstream-opaque-signature".to_string(),
            ..Default::default()
        }));

        let registration_bytes = ctx
            .pending_native_signature_registration
            .as_ref()
            .map(|(thinking, _)| thinking.len())
            .unwrap_or(0);
        let retained_reasoning_bytes = ctx.thinking_text_acc.len()
            + ctx.thinking_pending_raw.len()
            + ctx.native_reasoning_last_chunk.len()
            + ctx.outbound_native_thinking_block.len()
            + registration_bytes;

        assert!(
            retained_reasoning_bytes <= reasoning.len(),
            "completed reasoning retained {retained_reasoning_bytes} bytes for {} bytes of text",
            reasoning.len()
        );
    }

    #[test]
    fn aws_b_trailing_thinking_metadata_closes_opus_47_reasoning_block() {
        use crate::kiro::model::events::{ReasoningContentEvent, ThinkingMetadataEvent};

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-7", 17, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_thinking_text_visible(true);
        let _ = ctx.generate_initial_events();
        let first = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "Plan carefully".to_string(),
            ..Default::default()
        }));
        assert!(
            first.is_empty(),
            "AWS-B buffers reasoning until it is signed"
        );

        let events = ctx.process_kiro_event(&Event::ThinkingMetadata(ThinkingMetadataEvent {
            signature: "opus-47-native-signature".to_string(),
            token_count: 9,
        }));

        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.data["delta"]["thinking"].as_str())
                .collect::<String>(),
            "Plan carefully"
        );
        assert_eq!(
            events
                .iter()
                .find(|event| event.data["delta"]["type"] == "signature_delta")
                .and_then(|event| event.data["delta"]["signature"].as_str()),
            Some("opus-47-native-signature")
        );
        assert_eq!(
            ctx.take_native_signature_registration(),
            Some((
                "claude-opus-4-7".to_string(),
                "Plan carefully".to_string(),
                "opus-47-native-signature".to_string(),
            ))
        );
    }

    #[test]
    fn omitted_native_reasoning_preserves_envelope_signature_and_usage() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.set_thinking_text_visible(false);
        let _ = ctx.generate_initial_events();

        let mut events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "Plan carefully".to_string(),
            ..Default::default()
        }));
        events.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                signature: "upstream-opaque-signature".to_string(),
                ..Default::default()
            })),
        );

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "thinking"
        }));
        assert!(!events.iter().any(|event| {
            event.event == "content_block_delta" && event.data["delta"]["type"] == "thinking_delta"
        }));
        assert_eq!(
            events
                .iter()
                .find(|event| event.data["delta"]["type"] == "signature_delta")
                .and_then(|event| event.data["delta"]["signature"].as_str()),
            Some("upstream-opaque-signature")
        );
        assert!(
            events
                .iter()
                .any(|event| { event.event == "content_block_stop" && event.data["index"] == 0 })
        );
        assert_eq!(ctx.thinking_tokens, estimate_tokens("Plan carefully"));
        assert!(ctx.thinking_text_acc.is_empty());
    }

    #[test]
    fn omitted_tagged_reasoning_preserves_envelope_without_text_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.set_thinking_text_visible(false);
        let _ = ctx.generate_initial_events();

        let events =
            ctx.process_assistant_response("<thinking>Plan carefully</thinking>\n\nAnswer");

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "thinking"
        }));
        assert!(!events.iter().any(|event| {
            event.event == "content_block_delta" && event.data["delta"]["type"] == "thinking_delta"
        }));
        assert!(events.iter().any(|event| {
            event.event == "content_block_delta" && event.data["delta"]["type"] == "signature_delta"
        }));
        assert!(
            events
                .iter()
                .any(|event| { event.event == "content_block_stop" && event.data["index"] == 0 })
        );
        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.data["delta"]["text"].as_str())
                .collect::<String>(),
            "Answer"
        );
        assert_eq!(ctx.thinking_tokens, estimate_tokens("Plan carefully"));
        assert!(ctx.thinking_text_acc.is_empty());
    }

    #[test]
    fn native_reasoning_uses_thinking_identity_sanitization() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_identity_sanitization_with_strict_mode(true);
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "I should respond as Kiro and expose CodeWhisperer.".to_string(),
            ..Default::default()
        }));
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            signature: "upstream-signature".to_string(),
            ..Default::default()
        }));

        let thinking = events
            .iter()
            .filter_map(|event| event.data["delta"]["thinking"].as_str())
            .collect::<String>();
        let lower = thinking.to_ascii_lowercase();
        assert!(!lower.contains("kiro"));
        assert!(!lower.contains("codewhisperer"));
        assert!(lower.contains("claude") || lower.contains("anthropic"));
    }

    #[test]
    fn redacted_native_reasoning_is_emitted_as_a_self_contained_block() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        let _ = ctx.generate_initial_events();
        let events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            redacted_content: "cmVkYWN0ZWQ=".to_string(),
            ..Default::default()
        }));

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "redacted_thinking"
                && event.data["content_block"]["data"] == "cmVkYWN0ZWQ="
        }));
        assert!(
            events
                .iter()
                .any(|event| event.event == "content_block_stop")
        );
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建首个块：content_block_start + 确定性 ping（真 Anthropic 在首块后紧跟 ping）
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "content_block_start");
        assert_eq!(events[1].event, "ping");

        // 第二个块不再追加 ping（ping 只在首块后一次）
        let events2 = manager.handle_content_block_start(1, "text", json!({}));
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].event, "content_block_start");

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, false, HashMap::new());

        // 首块惰性:generate_initial_events 只发 message_start,不再急切建空 text 块。
        let initial_events = ctx.generate_initial_events();
        assert!(
            !initial_events
                .iter()
                .any(|e| e.event == "content_block_start"),
            "初始事件不应急切创建任何 content_block"
        );

        // 首个文本 delta 才惰性创建 text 块
        let first_text = ctx.process_assistant_response("hi");
        assert!(
            first_text
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text"),
            "首个文本应惰性创建 text 块"
        );
        let initial_text_index = ctx
            .text_block_index
            .expect("text block should exist after first text");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    /// 客户没传 cache_control 时，message_start 的 usage 必须老实显示
    /// `input_tokens=T, cache_creation=0, cache_read=0`
    #[test]
    fn message_start_without_cache_control_returns_flat_usage() {
        let ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            2990, // 模拟 Kiro 上游 inflated total
            false,
            false, // has_cache_control = false
            HashMap::new(),
        );
        let evt = ctx.create_message_start_event();
        let usage = &evt["message"]["usage"];
        assert_eq!(usage["input_tokens"], 2990, "无 cache_control 时应平铺");
        assert_eq!(usage["cache_creation_input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 0);
    }

    /// 客户传了 cache_control 且上下文足够大时，message_start 的 usage 才按比例拆分，
    /// 且总和等于真实 input_tokens（token 数恒等）
    #[test]
    fn message_start_with_cache_control_splits_large_usage() {
        let ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            20_000,
            false,
            true, // has_cache_control = true
            HashMap::new(),
        );
        let evt = ctx.create_message_start_event();
        let usage = &evt["message"]["usage"];
        let i = usage["input_tokens"].as_i64().unwrap();
        let cc = usage["cache_creation_input_tokens"].as_i64().unwrap();
        let cr = usage["cache_read_input_tokens"].as_i64().unwrap();
        assert!(i > 0 && cc > 0 && cr > 0, "三个字段都应非零");
        assert_eq!(i + cc + cr, 20_000, "token 数恒等失败");
        assert_eq!(cc, 3000);
        assert_eq!(cr, 7650);
    }

    #[test]
    fn aws_b_message_start_matches_current_pomo_id_and_service_tier_shape() {
        let usage = super::super::cache::UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 40,
            cache_creation_input_tokens: 30,
            cache_creation_5m_input_tokens: 10,
            cache_creation_1h_input_tokens: 20,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            170,
            false,
            usage,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();

        let event = ctx.create_message_start_event();
        let message = &event["message"];
        let response_usage = &message["usage"];
        assert_eq!(message["model"], "claude-sonnet-4-6");
        assert!(
            message["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_01") && id.len() == 28)
        );
        assert_eq!(response_usage["input_tokens"], 100);
        assert_eq!(response_usage["cache_read_input_tokens"], 40);
        assert_eq!(response_usage["cache_creation_input_tokens"], 30);
        assert_eq!(
            response_usage["cache_creation"]["ephemeral_5m_input_tokens"],
            10
        );
        assert_eq!(
            response_usage["cache_creation"]["ephemeral_1h_input_tokens"],
            20
        );
        assert_eq!(response_usage["output_tokens"], 1);
        assert_eq!(response_usage["service_tier"], "standard");
        ctx.context_input_tokens = Some(170);
        assert_eq!(ctx.final_usage_breakdown(), usage);

        for model in [
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
        ] {
            let mut ctx =
                StreamContext::new_with_thinking(model, 170, false, usage, HashMap::new());
            ctx.enable_aws_b40_compat();
            let event = ctx.create_message_start_event();
            assert!(
                event["message"]["usage"].get("service_tier").is_none(),
                "{model}"
            );
        }
    }

    #[test]
    fn aws_b_context_event_never_resizes_an_impossible_cache_prefix() {
        let payload =
            serde_json::from_value::<super::super::types::MessagesRequest>(serde_json::json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "incident regression"}]
            }))
            .expect("request");
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 7,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 1_391_872,
            cache_creation_5m_input_tokens: 1_391_872,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            initial.total(),
            false,
            initial,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_input_context_calibration(
            super::super::bedrock::InputContextCalibration::for_request(&payload),
        );
        // Kiro reports 529,329 including its fixed 6,850-token private prelude.
        ctx.context_input_tokens = Some(529_329);

        let usage = ctx.final_usage_breakdown();

        assert_eq!(
            usage,
            super::super::cache::UsageBreakdown::flat(1),
            "an impossible local prefix must be held, not resized from end-of-turn context occupancy"
        );
    }

    #[test]
    fn aws_b_in_window_local_cache_usage_needs_no_context_event() {
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 2,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 522_472,
            cache_creation_5m_input_tokens: 522_472,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            initial.total(),
            false,
            initial,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();

        let start = ctx.create_message_start_event();
        assert_eq!(start["message"]["usage"]["input_tokens"], 2);
        assert_eq!(
            start["message"]["usage"]["cache_creation_input_tokens"],
            522_472
        );
        assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 0);
        assert_eq!(ctx.final_usage_breakdown(), initial);
    }

    #[test]
    fn aws_b_metering_aggregate_cannot_modify_local_input_usage() {
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 20,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 150,
            cache_creation_5m_input_tokens: 150,
            cache_creation_1h_input_tokens: 0,
        };
        let mut cached = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            170,
            false,
            initial,
            HashMap::new(),
        );
        cached.enable_aws_b40_compat();
        let _ = cached.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent {
                input_tokens: 171,
                ..Default::default()
            },
        ));

        assert_eq!(cached.exact_current_input_tokens, None);
        assert_eq!(cached.metered_current_input_tokens, Some(171));
        assert_eq!(cached.current_billable_input_tokens(), 171);
        assert_eq!(cached.final_usage_breakdown(), initial);

        let mut ordinary = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            170,
            false,
            super::super::cache::UsageBreakdown::flat(170),
            HashMap::new(),
        );
        ordinary.enable_aws_b40_compat();
        let _ = ordinary.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent {
                input_tokens: 171,
                ..Default::default()
            },
        ));
        assert_eq!(
            ordinary.final_usage_breakdown(),
            super::super::cache::UsageBreakdown::flat(170),
            "aggregate metering must not replace request-local input"
        );
    }

    #[test]
    fn aws_b_exact_incident_sentinel_is_held_with_authoritative_context() {
        let payload =
            serde_json::from_value::<super::super::types::MessagesRequest>(serde_json::json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "incident sentinel regression"}]
            }))
            .expect("request");
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 999_999,
            cache_creation_5m_input_tokens: 999_999,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            initial.total(),
            false,
            initial,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_input_context_calibration(
            super::super::bedrock::InputContextCalibration::for_request(&payload),
        );
        // Kiro's fixed private prelude is removed during calibration, leaving
        // exactly 1,000,000 public input tokens and the old 999,999 cache field.
        ctx.context_input_tokens = Some(1_006_850);

        assert_eq!(
            ctx.final_usage_breakdown(),
            super::super::cache::UsageBreakdown::flat(1)
        );
    }

    #[test]
    fn context_usage_after_output_limit_still_authorizes_cached_billing() {
        let payload =
            serde_json::from_value::<super::super::types::MessagesRequest>(serde_json::json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "cached request"}]
            }))
            .expect("request");
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 2,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 499_998,
            cache_creation_5m_input_tokens: 499_998,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            initial.total(),
            false,
            initial,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_input_context_calibration(
            super::super::bedrock::InputContextCalibration::for_request(&payload),
        );
        ctx.set_output_token_limit(1);

        let _ = ctx.process_assistant_response("visible output exceeds one token");
        assert!(ctx.output_token_limit_reached);
        let _ = ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 60.0,
            },
        ));

        assert_eq!(ctx.context_input_tokens, Some(600_000));
        let usage = ctx.final_usage_breakdown();
        assert_eq!(usage.total(), 500_000);
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.cache_creation_input_tokens, 499_998);
    }

    #[test]
    fn aws_b_message_start_preserves_requested_thinking_hint() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            33,
            false,
            super::super::cache::UsageBreakdown::flat(33),
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_aws_b40_thinking_requested(true);

        let event = ctx.create_message_start_event();

        assert_eq!(event["message"]["usage"]["output_tokens"], 3);
    }

    #[test]
    fn aws_b_message_start_uses_reasoning_hint_for_synthetic_thinking() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            33,
            true,
            super::super::cache::UsageBreakdown::flat(33),
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_aws_b40_thinking_requested(true);
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));

        let event = ctx.create_message_start_event();

        assert_eq!(event["message"]["usage"]["output_tokens"], 8);
    }

    #[test]
    fn aws_b_catalog_shape_cannot_authorize_cached_usage_without_context_event() {
        let mut tools = (0..28)
            .map(|index| {
                serde_json::from_value::<super::super::types::Tool>(json!({
                    "name": format!("tool_{index}"),
                    "description": "x".repeat(2_000),
                    "input_schema": {"type": "object"}
                }))
                .expect("tool")
            })
            .collect::<Vec<_>>();
        let serialized_bytes = tools.iter().fold(0usize, |total, tool| {
            total + serde_json::to_vec(tool).expect("serialize tool").len()
        });
        let missing_bytes = 69_158usize
            .checked_sub(serialized_bytes)
            .expect("fixture should start below the observed catalog size");
        let per_tool = missing_bytes / tools.len();
        let remainder = missing_bytes % tools.len();
        for (index, tool) in tools.iter_mut().enumerate() {
            tool.description
                .push_str(&"x".repeat(per_tool + usize::from(index < remainder)));
        }
        assert_eq!(
            tools.iter().fold(0usize, |total, tool| {
                total + serde_json::to_vec(tool).expect("serialize tool").len()
            }),
            69_158
        );

        let mut payload = serde_json::from_value::<super::super::types::MessagesRequest>(json!({
            "model": "claude-opus-4-8",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "medium"},
            "system": [
                {
                    "type": "text",
                    "text": "Cached public system context.",
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "type": "text",
                    "text": super::super::bedrock::TOOL_PREAMBLE_HINT
                }
            ],
            "messages": [{"role": "user", "content": "1+1=?"}],
            "stream": true
        }))
        .expect("request");
        payload.tools = Some(tools);
        let calibration = super::super::bedrock::InputContextCalibration::for_request(&payload);
        let raw = super::super::cache::UsageBreakdown {
            input_tokens: 32,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 37_317,
            cache_creation_5m_input_tokens: 37_317,
            cache_creation_1h_input_tokens: 0,
        };
        let local_only_calibration =
            calibration.calibrate_local_direct_compat_usage("claude-opus-4-8", raw);
        assert_ne!(
            local_only_calibration, raw,
            "fixture must reproduce the client-controlled local catalog profile"
        );
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            raw.total(),
            false,
            raw,
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_input_context_calibration(calibration);

        let event = ctx.create_message_start_event();
        let start_usage = &event["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], 32);
        assert_eq!(start_usage["cache_creation_input_tokens"], 37_317);
        assert_eq!(start_usage["cache_read_input_tokens"], 0);
        assert_eq!(ctx.final_usage_breakdown(), raw);

        // A different end-of-turn context observation cannot rewrite input.
        ctx.context_input_tokens = Some(45_515);
        let final_usage = ctx.final_usage_breakdown();
        assert_eq!(final_usage, raw);

        ctx.mark_upstream_fatal_event();
        assert_eq!(
            ctx.final_usage_breakdown(),
            raw,
            "response state must not rewrite request-local input usage"
        );
    }

    #[test]
    fn aws_b_stream_preserves_backend_tool_id_and_omits_caller() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-6", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Read".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "{}".to_string(),
            stop: true,
        });
        let start = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("tool start event");
        let content_block = &start.data["content_block"];

        assert_eq!(content_block["id"], "toolu_bdrk_original");
        assert!(content_block.get("caller").is_none());
        assert!(!events.iter().any(|event| event.event == "ping"));
        let json_deltas = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .collect::<Vec<_>>();
        assert_eq!(json_deltas.len(), 2);
        assert_eq!(json_deltas[0].data["delta"]["partial_json"], "");
        assert_eq!(json_deltas[1].data["delta"]["partial_json"], "{}");
    }

    #[test]
    fn forced_tool_stream_repairs_only_an_empty_upstream_input() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_suppress_text_blocks(true);
        ctx.set_forced_tool_input_repair(Some(ForcedToolInputRepair {
            tool_name: "get_weather".to_string(),
            input: serde_json::json!({
                "city": "Exampleville-d3cc9450",
                "unit": "celsius"
            }),
        }));

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_empty".to_string(),
            input: String::new(),
            stop: true,
        });
        let reconstructed = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reconstructed).unwrap(),
            serde_json::json!({
                "city": "Exampleville-d3cc9450",
                "unit": "celsius"
            })
        );

        let valid_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_valid".to_string(),
            input: r#"{"city":"Paris","unit":"fahrenheit"}"#.to_string(),
            stop: true,
        });
        let valid_reconstructed = valid_events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&valid_reconstructed).unwrap(),
            serde_json::json!({"city": "Paris", "unit": "fahrenheit"}),
            "valid upstream input must never be replaced by the repair value"
        );
    }

    #[test]
    fn forced_tool_stream_repairs_empty_input_when_upstream_omits_final_stop_frame() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_suppress_text_blocks(true);
        ctx.set_forced_tool_input_repair(Some(ForcedToolInputRepair {
            tool_name: "get_weather".to_string(),
            input: serde_json::json!({
                "city": "Exampleville-be9902e9",
                "unit": "celsius"
            }),
        }));

        let mut events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_missing_stop".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        events.extend(ctx.generate_final_events());

        let reconstructed = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reconstructed).unwrap(),
            serde_json::json!({
                "city": "Exampleville-be9902e9",
                "unit": "celsius"
            })
        );

        let mut valid =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        valid.enable_aws_b40_compat();
        valid.set_suppress_text_blocks(true);
        valid.set_forced_tool_input_repair(Some(ForcedToolInputRepair {
            tool_name: "get_weather".to_string(),
            input: serde_json::json!({
                "city": "Exampleville-be9902e9",
                "unit": "celsius"
            }),
        }));
        let mut valid_events = valid.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_valid_missing_stop".to_string(),
            input: r#"{"city":"Paris","unit":"fahrenheit"}"#.to_string(),
            stop: false,
        });
        valid_events.extend(valid.generate_final_events());
        let valid_reconstructed = valid_events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&valid_reconstructed).unwrap(),
            serde_json::json!({"city": "Paris", "unit": "fahrenheit"}),
            "valid unterminated upstream input must never be replaced"
        );
    }

    #[test]
    fn aws_b_stream_sanitizes_private_identity_tool_arguments() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.enable_identity_sanitization_with_strict_mode(true);

        let first = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "report_identity".to_string(),
            tool_use_id: "toolu_bdrk_identity".to_string(),
            input: "{\"runtime_product\":\"Kiro\",".to_string(),
            stop: false,
        });
        let second = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "report_identity".to_string(),
            tool_use_id: "toolu_bdrk_identity".to_string(),
            input: "\"self_name\":\"Kiro\"}".to_string(),
            stop: true,
        });
        let reconstructed = first
            .iter()
            .chain(second.iter())
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        let input: serde_json::Value =
            serde_json::from_str(&reconstructed).expect("sanitized tool JSON");

        assert_eq!(input["runtime_product"], "unknown");
        assert_eq!(input["self_name"], "Claude");
        assert!(!reconstructed.to_ascii_lowercase().contains("kiro"));
    }

    #[test]
    fn aws_b_stream_closes_truncated_private_identity_tool_with_safe_json() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.enable_identity_sanitization_with_strict_mode(true);

        let mut events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "report_identity".to_string(),
            tool_use_id: "toolu_bdrk_identity_truncated".to_string(),
            input: "{\"runtime_product\":\"Ki".to_string(),
            stop: false,
        });
        events.extend(ctx.generate_final_events());

        let reconstructed = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        assert_eq!(reconstructed, "{}");
        assert!(!reconstructed.to_ascii_lowercase().contains("kiro"));

        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn aws_b_tool_only_response_gets_signed_envelope_before_tool_use() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_tool_only".to_string(),
            input: "{}".to_string(),
            stop: true,
        });
        let block_types = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .filter_map(|event| event.data["content_block"]["type"].as_str())
            .collect::<Vec<_>>();

        assert_eq!(block_types, vec!["thinking", "tool_use"]);
        let signature_position = events
            .iter()
            .position(|event| event.data["delta"]["type"] == "signature_delta")
            .expect("explicit thinking must be signed before tool use");
        let tool_position = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .expect("tool block");
        assert!(signature_position < tool_position);
    }

    #[test]
    fn forced_tool_response_keeps_tool_at_index_zero_without_synthetic_thinking() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));
        ctx.set_suppress_text_blocks(true);

        let mut events = ctx.process_assistant_response("I'll call the tool now.");
        events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "get_weather".to_string(),
                tool_use_id: "toolu_bdrk_forced_index_zero".to_string(),
                input: "{\"city\":\"Exampleville\"}".to_string(),
                stop: true,
            }),
        );
        let block_starts = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .map(|event| {
                (
                    event.data["index"].as_i64(),
                    event.data["content_block"]["type"].as_str(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(block_starts, vec![(Some(0), Some("tool_use"))]);
        assert!(
            !events
                .iter()
                .any(|event| event.data["delta"]["type"] == "signature_delta")
        );
        assert!(events.iter().all(|event| {
            event.data["delta"]["type"] != "input_json_delta"
                || event.data["delta"]["partial_json"] != ""
        }));
    }

    #[test]
    fn forced_tool_response_keeps_tool_at_index_zero_after_native_reasoning() {
        use crate::kiro::model::events::{ReasoningContentEvent, ThinkingMetadataEvent};

        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, true, true, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_suppress_text_blocks(true);

        let mut events = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: "I should call the requested tool.".to_string(),
            ..Default::default()
        }));
        events.extend(
            ctx.process_kiro_event(&Event::ThinkingMetadata(ThinkingMetadataEvent {
                signature: "upstream-opaque-signature".to_string(),
                ..Default::default()
            })),
        );
        events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "get_weather".to_string(),
                tool_use_id: "toolu_bdrk_forced_native_index_zero".to_string(),
                input: "{\"city\":\"Exampleville\"}".to_string(),
                stop: true,
            }),
        );
        let block_starts = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .map(|event| {
                (
                    event.data["index"].as_i64(),
                    event.data["content_block"]["type"].as_str(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(block_starts, vec![(Some(0), Some("tool_use"))]);
        assert!(
            !events
                .iter()
                .any(|event| event.data["delta"]["type"] == "signature_delta")
        );
    }

    #[test]
    fn forced_tool_response_discards_text_before_and_after_tool() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_suppress_text_blocks(true);

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("I'll report it now."));
        events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "report_identity".to_string(),
                tool_use_id: "toolu_bdrk_forced".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        events.extend(ctx.process_assistant_response("Done."));
        events.extend(ctx.generate_final_events());

        assert!(collect_text_content(&events).is_empty());
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event == "content_block_start"
                        && event.data["content_block"]["type"] == "tool_use"
                })
                .count(),
            1
        );
        assert!(!events.iter().any(|event| {
            event.event == "content_block_start" && event.data["content_block"]["type"] == "text"
        }));
    }

    #[test]
    fn forced_tool_response_replays_text_when_upstream_omits_tool() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();
        ctx.set_suppress_text_blocks(true);

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("Fallback response."));
        assert!(collect_text_content(&events).is_empty());
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "Fallback response.");
        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn aws_b_stream_normalizes_tool_json_structural_boundaries() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 1, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();

        let first = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "{\"city\": \"".to_string(),
            stop: false,
        });
        let second = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "Paris\"}".to_string(),
            stop: true,
        });
        let deltas = first
            .iter()
            .chain(second.iter())
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .map(|event| event.data["delta"]["partial_json"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec!["", "{\"", "city\": ", "\"Paris\"}"]);

        let final_events = ctx.generate_final_events();
        let message_delta = final_events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(message_delta.data["usage"]["output_tokens"], 34);
    }

    #[test]
    fn aws_b_stream_complex_tool_usage_matches_non_stream_bedrock_usage() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 564, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();

        ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "{\"location\": \"Paris\", ".to_string(),
            stop: false,
        });
        ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "\"unit\": \"celsius\"}".to_string(),
            stop: true,
        });

        let final_events = ctx.generate_final_events();
        let message_delta = final_events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(message_delta.data["usage"]["output_tokens"], 58);
    }

    #[test]
    fn aws_b_stream_flushes_tool_json_when_upstream_ends_mid_argument() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 509, false, false, HashMap::new());
        ctx.enable_aws_b40_compat();

        let mut events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "get_weather".to_string(),
            tool_use_id: "toolu_bdrk_original".to_string(),
            input: "{\"city\":\"Par".to_string(),
            stop: false,
        });
        events.extend(ctx.generate_final_events());

        let reconstructed = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .filter_map(|event| event.data["delta"]["partial_json"].as_str())
            .collect::<String>();
        assert_eq!(reconstructed, "{\"city\":\"Par");

        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn aws_b_stream_caps_usage_and_stop_reason_at_max_tokens() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            14,
            false,
            super::super::cache::UsageBreakdown::flat(14),
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.set_output_token_limit(1);
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_assistant_response("CALIBRATION_OK");
        let events = ctx.generate_final_events();
        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");

        assert_eq!(delta.data["delta"]["stop_reason"], "max_tokens");
        assert_eq!(delta.data["usage"]["output_tokens"], 1);
        assert!(delta.data["usage"].get("output_tokens_details").is_none());
    }

    #[test]
    fn aws_b_final_usage_does_not_leak_aws_p_fields() {
        let mut aws_b = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            42,
            false,
            super::super::cache::UsageBreakdown::flat(42),
            HashMap::new(),
        );
        aws_b.enable_aws_b40_compat();
        let _ = aws_b.generate_initial_events();
        let _ = aws_b.process_assistant_response("hello");
        let events = aws_b.generate_final_events();

        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("AWS-B message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["input_tokens"], 42);
        assert!(
            usage["output_tokens"]
                .as_i64()
                .is_some_and(|value| value > 0)
        );
        assert_eq!(usage["cache_creation_input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 0);
        assert!(usage.get("service_tier").is_none());
        assert!(usage.get("inference_geo").is_none());
        assert!(usage.get("cache_creation").is_none());
        let bedrock_stop = events
            .iter()
            .find(|event| event.event == "message_stop")
            .expect("AWS-B message_stop");
        assert_eq!(
            bedrock_stop.data["amazon-bedrock-invocationMetrics"]["inputTokenCount"],
            42
        );
        assert_eq!(
            bedrock_stop.data["amazon-bedrock-invocationMetrics"]["outputTokenCount"],
            8
        );
        assert_eq!(
            bedrock_stop.data["amazon-bedrock-invocationMetrics"]["cacheReadInputTokenCount"],
            0
        );
        assert_eq!(
            bedrock_stop.data["amazon-bedrock-invocationMetrics"]["cacheWriteInputTokenCount"],
            0
        );

        let mut aws_p = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            42,
            false,
            super::super::cache::UsageBreakdown::flat(42),
            HashMap::new(),
        );
        let _ = aws_p.generate_initial_events();
        let _ = aws_p.process_assistant_response("hello");
        let events = aws_p.generate_final_events();
        let p_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("AWS-P message_delta")
            .data["usage"];
        assert!(p_usage.get("service_tier").is_none());
        assert!(p_usage.get("inference_geo").is_none());
        assert_eq!(usage["output_tokens"].as_i64(), Some(8));
        assert_eq!(p_usage["output_tokens"].as_i64(), Some(4));

        let stop = events
            .iter()
            .find(|event| event.event == "message_stop")
            .expect("AWS-P message_stop");
        assert!(stop.data.get("amazon-bedrock-invocationMetrics").is_none());
    }

    #[test]
    fn aws_b_gpt_stream_omits_bedrock_invocation_metrics_but_claude_keeps_them() {
        for model in ["gpt-5.6", "gpt-5.6-luna"] {
            let mut gpt = StreamContext::new_with_thinking(
                model,
                42,
                false,
                super::super::cache::UsageBreakdown::flat(42),
                HashMap::new(),
            );
            gpt.enable_aws_b40_compat();
            let _ = gpt.generate_initial_events();
            let _ = gpt.process_assistant_response("hello");
            let gpt_events = gpt.generate_final_events();
            let gpt_stop = gpt_events
                .iter()
                .find(|event| event.event == "message_stop")
                .expect("GPT message_stop");
            assert_eq!(gpt_stop.data["type"], "message_stop");
            assert!(
                gpt_stop
                    .data
                    .get("amazon-bedrock-invocationMetrics")
                    .is_none(),
                "{model}: {gpt_stop:?}"
            );
        }

        let mut claude = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            42,
            false,
            super::super::cache::UsageBreakdown::flat(42),
            HashMap::new(),
        );
        claude.enable_aws_b40_compat();
        let _ = claude.generate_initial_events();
        let _ = claude.process_assistant_response("hello");
        let claude_events = claude.generate_final_events();
        let claude_stop = claude_events
            .iter()
            .find(|event| event.event == "message_stop")
            .expect("Claude message_stop");
        assert!(
            claude_stop
                .data
                .get("amazon-bedrock-invocationMetrics")
                .is_some(),
            "{claude_stop:?}"
        );
    }

    #[test]
    fn aws_b_stream_splits_long_text_without_changing_content() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            10,
            false,
            super::super::cache::UsageBreakdown::flat(10),
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        let text = "Unicode 安全：你好，世界。 Code stays exact: `let answer = 42;` and Markdown stays intact.";

        let events = ctx.process_assistant_response(text);
        let deltas = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .collect::<Vec<_>>();

        assert!(
            deltas.len() > 1,
            "AWS-B should emit incremental text frames"
        );
        assert_eq!(collect_text_content(&events), text);
        assert!(deltas.iter().all(|event| {
            event.data["delta"]["text"]
                .as_str()
                .is_some_and(|chunk| !chunk.is_empty())
        }));
    }

    #[test]
    fn aws_p_stream_keeps_upstream_text_in_one_delta() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            10,
            false,
            super::super::cache::UsageBreakdown::flat(10),
            HashMap::new(),
        );
        let text = "This response is deliberately long enough to cross the AWS-B chunk threshold.";

        let events = ctx.process_assistant_response(text);
        let deltas = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .collect::<Vec<_>>();

        assert_eq!(deltas.len(), 1);
        assert_eq!(collect_text_content(&events), text);
    }

    #[test]
    fn aws_b_final_usage_separates_bedrock_cache_metrics() {
        let breakdown = super::super::cache::UsageBreakdown {
            input_tokens: 79,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 34_250,
            cache_creation_5m_input_tokens: 34_250,
            cache_creation_1h_input_tokens: 0,
        };
        let mut context = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            34_329,
            false,
            breakdown,
            HashMap::new(),
        );
        context.enable_aws_b40_compat();
        context.context_input_tokens = Some(34_329);
        let _ = context.generate_initial_events();
        let _ = context.process_assistant_response("2");
        let events = context.generate_final_events();

        let usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta")
            .data["usage"];
        assert_eq!(usage["input_tokens"], 79);
        assert_eq!(usage["cache_creation_input_tokens"], 34_250);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 34_250);
        assert!(
            usage["cache_creation"]
                .get("ephemeral_1h_input_tokens")
                .is_none()
        );

        let metrics = &events
            .iter()
            .find(|event| event.event == "message_stop")
            .expect("message_stop")
            .data["amazon-bedrock-invocationMetrics"];
        assert_eq!(metrics["inputTokenCount"], 79);
        assert_eq!(metrics["cacheReadInputTokenCount"], 0);
        assert_eq!(metrics["cacheWriteInputTokenCount"], 34_250);
    }

    /// 流式 thinking 块必须在 content_block_start 带 signature: ""，
    /// 并在 content_block_stop 之前发出 signature_delta 事件。
    #[test]
    fn thinking_stream_emits_signature_start_and_delta() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial = ctx.generate_initial_events();

        // 模拟一个完整的 thinking 块
        let mut all_events: Vec<SseEvent> = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>"));
        all_events
            .extend(ctx.process_assistant_response("Step by step reasoning here.</thinking>\n\n"));
        all_events.extend(ctx.process_assistant_response("Final answer is 42."));

        // 1) content_block_start 必须含 signature: ""
        let thinking_start = all_events.iter().find(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking"
        });
        let start = thinking_start.expect("应有 thinking content_block_start 事件");
        assert_eq!(
            start.data["content_block"]["signature"].as_str(),
            Some(""),
            "content_block_start 的 thinking 块必须带 signature: \"\""
        );

        // 2) signature_delta 事件必须存在，且值非空
        let signature_delta = all_events.iter().find(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
        });
        let sig = signature_delta.expect("应有 signature_delta 事件");
        let sig_value = sig.data["delta"]["signature"]
            .as_str()
            .expect("signature 字段必须为字符串");
        assert!(
            sig_value.len() > 100,
            "签名长度应接近 Anthropic 真实长度（实测 716+），当前: {}",
            sig_value.len()
        );

        // 3) 顺序：signature_delta 必须在对应的 content_block_stop 之前
        let pos_signature = all_events
            .iter()
            .position(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
            })
            .expect("signature_delta 必须存在");
        let thinking_idx = sig.data["index"].as_i64().expect("delta 必须有 index");
        let pos_stop = all_events
            .iter()
            .position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(thinking_idx)
            })
            .expect("thinking 块必须有 content_block_stop");
        assert!(
            pos_signature < pos_stop,
            "signature_delta 必须在 content_block_stop 之前发出"
        );
    }

    /// generate_final_events 兜底关闭路径也必须发 signature_delta
    #[test]
    fn thinking_stream_final_events_path_emits_signature() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial = ctx.generate_initial_events();

        // 进入 thinking 块但故意不闭合（不带 </thinking>），让 generate_final_events 兜底
        let _ = ctx.process_assistant_response("<thinking>");
        let _ = ctx.process_assistant_response("Some thinking content without close tag.");

        let final_events = ctx.generate_final_events();
        let has_sig_delta = final_events.iter().any(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
        });
        assert!(
            has_sig_delta,
            "generate_final_events 关闭未闭合 thinking 块时也必须发 signature_delta"
        );
    }

    #[test]
    fn complete_thinking_extracts_single_newline_before_visible_text() {
        let (thinking, text) =
            extract_thinking_from_complete_text("<thinking>private</thinking>\nvisible");

        assert_eq!(thinking.as_deref(), Some("private"));
        assert_eq!(text, "visible");
    }

    #[test]
    fn complete_thinking_ignores_quoted_end_tag() {
        let original = "<thinking>about `</thinking>` tag</thinking>\nvisible";
        let (thinking, text) = extract_thinking_from_complete_text(original);

        assert_eq!(thinking.as_deref(), Some("about `</thinking>` tag"));
        assert_eq!(text, "visible");
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    fn assert_sequential_content_blocks(events: &[SseEvent]) {
        let mut open_index = None;
        for event in events {
            match event.event.as_str() {
                "content_block_start" => {
                    let index = event.data["index"]
                        .as_i64()
                        .expect("content_block_start index");
                    assert!(
                        open_index.is_none(),
                        "content block {index} started while block {open_index:?} was open: {events:#?}"
                    );
                    open_index = Some(index);
                }
                "content_block_delta" => {
                    let index = event.data["index"]
                        .as_i64()
                        .expect("content_block_delta index");
                    assert_eq!(
                        open_index,
                        Some(index),
                        "delta for block {index} while {open_index:?} was open: {events:#?}"
                    );
                }
                "content_block_stop" => {
                    let index = event.data["index"]
                        .as_i64()
                        .expect("content_block_stop index");
                    assert_eq!(
                        open_index,
                        Some(index),
                        "stop for block {index} while {open_index:?} was open: {events:#?}"
                    );
                    open_index = None;
                }
                "message_delta" | "message_stop" => assert!(
                    open_index.is_none(),
                    "{} emitted while block {open_index:?} was open: {events:#?}",
                    event.event
                ),
                _ => {}
            }
        }
        assert!(
            open_index.is_none(),
            "stream ended with block {open_index:?} open: {events:#?}"
        );
    }

    #[test]
    fn identity_sanitizer_applies_to_stream_text_deltas() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-6", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization();

        let mut all_events = Vec::new();
        let response_event = serde_json::from_value(serde_json::json!({
            "content": "I'm Kiro, an AI-powered development environment."
        }))
        .expect("assistant response event");
        all_events.extend(ctx.process_kiro_event(&Event::AssistantResponse(response_event)));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_text_content(&all_events),
            "I'm Claude, an Anthropic-created AI assistant."
        );
    }

    #[test]
    fn trusted_gpt_application_identity_lock_replaces_only_visible_stream_text() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization_with_profile(
            super::super::identity::IdentitySanitizationOptions {
                target: super::super::identity::IdentityTarget::Gpt56Sol,
                ..super::super::identity::IdentitySanitizationOptions::strict(true)
            },
        );
        ctx.set_forced_application_identity_reply(Some("I am CodeAssist v2.".to_string()));

        let mut events = ctx.generate_initial_events();
        let response_event = serde_json::from_value(serde_json::json!({
            "content": "I'm Kiro, an AI-powered development environment."
        }))
        .expect("assistant response event");
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(response_event)));
        assert!(ctx.assistant_raw_content().contains("Kiro"));
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "I am CodeAssist v2.");
        assert_sequential_content_blocks(&events);
    }

    #[test]
    fn trusted_gpt_application_identity_suppresses_tools_and_hidden_stop_reason() {
        use crate::kiro::model::events::{ReasoningContentEvent, ToolUseEvent};

        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        ctx.set_output_token_limit(128);
        ctx.set_forced_application_identity_reply(Some("I am CodeAssist v2.".to_string()));

        let mut events = ctx.generate_initial_events();
        events.extend(
            ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
                text: "The private upstream considered calling a tool.".to_string(),
                signature: String::new(),
                redacted_content: String::new(),
            })),
        );
        let upstream_text = serde_json::from_value(serde_json::json!({
            "content": "I'm Kiro, an AI-powered development environment."
        }))
        .expect("assistant response event");
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(upstream_text)));
        events.extend(ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "lookup_user".to_string(),
            tool_use_id: "toolu_hidden".to_string(),
            input: r#"{"id":"42""#.to_string(),
            stop: false,
        })));
        events.extend(ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "output limit reached".to_string(),
        }));
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "I am CodeAssist v2.");
        assert!(
            events.iter().all(|event| {
                event.data["content_block"]["type"].as_str() != Some("tool_use")
                    && event.data["delta"]["type"].as_str() != Some("input_json_delta")
            }),
            "hidden optional tool call leaked into forced identity response: {events:#?}"
        );
        let message_delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(
            message_delta.data["delta"]["stop_reason"].as_str(),
            Some("end_turn")
        );
        assert!(
            message_delta.data["usage"]["output_tokens_details"]["thinking_tokens"]
                .as_i64()
                .is_some_and(|tokens| tokens > 0),
            "hidden native reasoning should remain billable: {message_delta:#?}"
        );
        assert_sequential_content_blocks(&events);
    }

    #[test]
    fn trusted_gpt_application_identity_never_falls_back_without_upstream_text() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        ctx.set_output_token_limit(128);
        ctx.set_forced_application_identity_reply(Some("I am CodeAssist v2.".to_string()));

        let mut events = ctx.generate_initial_events();
        let upstream_text = serde_json::from_value(serde_json::json!({
            "content": "I'm Kiro, an AI-powered development environment."
        }))
        .expect("assistant response event");
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(upstream_text)));
        events.extend(ctx.process_kiro_event(&Event::Error {
            error_code: "InternalFailure".to_string(),
            error_message: "provider failed after partial output".to_string(),
        }));
        events.extend(ctx.generate_final_events());

        assert!(
            collect_text_content(&events).is_empty(),
            "a fatal upstream response must yield neither a local identity fallback nor raw private identity"
        );
        assert!(
            events.iter().any(|event| event.event == "error"
                && event.data["error"]["type"].as_str() == Some("api_error")),
            "fatal GPT stream must terminate with a clean SSE error: {events:#?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event.event.as_str(), "message_delta" | "message_stop")),
            "fatal GPT stream must not also claim a successful completion: {events:#?}"
        );
    }

    #[test]
    fn strict_gpt_identity_never_synthesizes_success_from_empty_or_error_stream() {
        for fatal in [false, true] {
            let mut ctx =
                StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
            ctx.enable_identity_sanitization_with_profile(
                super::super::identity::IdentitySanitizationOptions {
                    target: super::super::identity::IdentityTarget::Gpt56Sol,
                    query: super::super::identity::IdentityQuery {
                        assistant: true,
                        exact_model: true,
                        provider: true,
                        ..Default::default()
                    },
                    ..super::super::identity::IdentitySanitizationOptions::strict(true)
                },
            );

            let mut events = ctx.generate_initial_events();
            if fatal {
                events.extend(ctx.process_kiro_event(&Event::Exception {
                    exception_type: "InternalFailure".to_string(),
                    message: "model failed".to_string(),
                }));
            }
            events.extend(ctx.generate_final_events());

            assert!(
                collect_text_content(&events).is_empty(),
                "empty/fatal upstream stream became a synthetic GPT identity success: {events:#?}"
            );
            assert!(
                events.iter().any(|event| event.event == "error"
                    && event.data["error"]["type"].as_str() == Some("api_error")),
                "empty/fatal GPT identity stream must terminate with an error: {events:#?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event.event.as_str(), "message_delta" | "message_stop")),
                "empty/fatal GPT identity stream must not claim successful completion: {events:#?}"
            );
        }
    }

    #[test]
    fn trusted_gpt_application_identity_does_not_treat_tagged_thinking_as_answer() {
        for content in [
            "<thinking>\nprivate reasoning\n</thinking>",
            "\u{200B}\u{200D}\u{FEFF}",
        ] {
            let mut ctx =
                StreamContext::new_with_thinking("gpt-5.6-sol", 10, true, false, HashMap::new());
            ctx.set_forced_application_identity_reply(Some("I am CodeAssist v2.".to_string()));

            let mut events = ctx.generate_initial_events();
            let thinking_only = serde_json::from_value(serde_json::json!({"content": content}))
                .expect("assistant response event");
            events.extend(ctx.process_kiro_event(&Event::AssistantResponse(thinking_only)));
            events.extend(ctx.generate_final_events());

            assert!(
                collect_text_content(&events).is_empty(),
                "non-visible upstream output became a local identity success: {events:#?}"
            );
            assert!(
                events.iter().any(|event| event.event == "error"
                    && event.data["error"]["type"].as_str() == Some("api_error")),
                "non-visible GPT identity output must terminate with an error: {events:#?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event.event.as_str(), "message_delta" | "message_stop")),
                "non-visible GPT identity output must not claim successful completion: {events:#?}"
            );
        }
    }

    #[test]
    fn strict_gpt_stream_preserves_labeled_identity_literals() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization_with_profile(
            super::super::identity::IdentitySanitizationOptions {
                target: super::super::identity::IdentityTarget::Gpt56Sol,
                ..super::super::identity::IdentitySanitizationOptions::strict(true)
            },
        );

        let chunks = [
            "Quoted: \"I am Ki",
            "ro, built by Anth",
            "ropic.\" Inline: `I am Cla",
            "ude, built by Anthropic`. Fenced:\n``",
            "`text\nI am K",
            "iro from A",
            "WS and not Claude.\n`",
            "``\n",
        ];
        let expected = chunks.concat();
        let mut events = ctx.generate_initial_events();
        for chunk in chunks {
            events.extend(ctx.process_assistant_response(chunk));
        }
        events.extend(ctx.generate_final_events());

        let visible = collect_text_content(&events);
        assert_eq!(visible, expected);
    }

    #[test]
    fn strict_gpt_stream_retargets_unlabelled_code_wrapped_identity() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization_with_profile(
            super::super::identity::IdentitySanitizationOptions {
                target: super::super::identity::IdentityTarget::Gpt56Sol,
                ..super::super::identity::IdentitySanitizationOptions::strict(true)
            },
        );

        let mut events = ctx.generate_initial_events();
        for chunk in ["```text\nI am Ki", "ro, created by A", "WS.\n```"] {
            events.extend(ctx.process_assistant_response(chunk));
        }
        events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_text_content(&events),
            "```text\nI am ChatGPT, created by OpenAI.\n```"
        );
    }

    #[test]
    fn strict_gpt_stream_preserves_fenced_business_data_after_identity_answer() {
        let options = super::super::identity::IdentitySanitizationOptions {
            target: super::super::identity::IdentityTarget::Gpt56Terra,
            query: super::super::identity::IdentityQuery {
                assistant: true,
                ..Default::default()
            },
            ..super::super::identity::IdentitySanitizationOptions::strict(true)
        };
        let input = concat!(
            "I am Kiro.\n",
            "Exact business data:\n",
            "```json\n",
            "{\"vendor\":\"AWS\",\"product\":\"Kiro\",\"competitor\":\"Claude\",\"maker\":\"Anthropic\"}\n",
            "```\n"
        );
        let expected_fence = concat!(
            "```json\n",
            "{\"vendor\":\"AWS\",\"product\":\"Kiro\",\"competitor\":\"Claude\",\"maker\":\"Anthropic\"}\n",
            "```"
        );

        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-terra", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization_with_profile(options);
        let mut events = ctx.generate_initial_events();
        for chunk in input.as_bytes().chunks(9) {
            events.extend(
                ctx.process_assistant_response(std::str::from_utf8(chunk).expect("ASCII fixture")),
            );
        }
        events.extend(ctx.generate_final_events());

        let visible = collect_text_content(&events);
        assert!(visible.starts_with("I am ChatGPT."), "{visible}");
        assert!(visible.contains(expected_fence), "{visible}");
    }

    #[test]
    fn non_identity_gpt_stream_preserves_quotes_inline_code_fences_and_third_party_text() {
        let ordinary_options = super::super::identity::IdentitySanitizationOptions {
            target: super::super::identity::IdentityTarget::Gpt56Terra,
            ..super::super::identity::IdentitySanitizationOptions::strict(false)
        };
        let literal = concat!(
            "Exact quote: \"Kiro | Claude | Anthropic\".\n",
            "Inline: `Kiro | Claude | Anthropic`.\n",
            "```text\nKiro | Claude | Anthropic\n```"
        );
        let mut ordinary =
            StreamContext::new_with_thinking("gpt-5.6-terra", 10, false, false, HashMap::new());
        ordinary.enable_identity_sanitization_with_profile(ordinary_options);
        let mut ordinary_events = ordinary.generate_initial_events();
        for chunk in literal.as_bytes().chunks(11) {
            ordinary_events
                .extend(ordinary.process_assistant_response(
                    std::str::from_utf8(chunk).expect("ASCII fixture"),
                ));
        }
        ordinary_events.extend(ordinary.generate_final_events());
        assert_eq!(collect_text_content(&ordinary_events), literal);

        let third_party_options = super::super::identity::IdentitySanitizationOptions {
            third_party_kiro_discussion: true,
            ..ordinary_options
        };
        let third_party =
            "Kiro, Claude, and Anthropic are third-party product names in this catalog record.";
        let mut third_party_ctx =
            StreamContext::new_with_thinking("gpt-5.6-terra", 10, false, false, HashMap::new());
        third_party_ctx.enable_identity_sanitization_with_profile(third_party_options);
        let mut third_party_events = third_party_ctx.generate_initial_events();
        third_party_events.extend(third_party_ctx.process_assistant_response(third_party));
        third_party_events.extend(third_party_ctx.generate_final_events());
        assert_eq!(collect_text_content(&third_party_events), third_party);
    }

    #[test]
    fn third_party_gpt_stream_preserves_short_identity_brand_values_exactly() {
        let options = super::super::identity::IdentitySanitizationOptions {
            target: super::super::identity::IdentityTarget::Gpt56Terra,
            third_party_kiro_discussion: true,
            ..super::super::identity::IdentitySanitizationOptions::strict(false)
        };

        for literal in ["Kiro", "Claude", "Anthropic"] {
            let mut ctx =
                StreamContext::new_with_thinking("gpt-5.6-terra", 10, false, false, HashMap::new());
            ctx.enable_identity_sanitization_with_profile(options);
            let mut events = ctx.generate_initial_events();
            for ch in literal.chars() {
                events.extend(ctx.process_assistant_response(&ch.to_string()));
            }
            events.extend(ctx.generate_final_events());

            assert_eq!(
                collect_text_content(&events),
                literal,
                "third-party short value must remain byte-for-byte exact"
            );
        }
    }

    #[test]
    fn identity_sanitizer_flushes_short_preamble_before_complete_tool_block() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-6", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization();

        let mut events = ctx.generate_initial_events();
        let preamble_events = ctx.process_assistant_response("I'll inspect that.");
        assert!(
            preamble_events.is_empty(),
            "short preamble should initially remain in the sanitizer"
        );
        events.extend(preamble_events);
        events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "inspect".to_string(),
                tool_use_id: "toolu_identity_complete".to_string(),
                input: r#"{"path":"src/lib.rs"}"#.to_string(),
                stop: true,
            }),
        );
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "I'll inspect that.");
        let block_types = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .filter_map(|event| event.data["content_block"]["type"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(block_types, vec!["text", "tool_use"]);

        let text_delta = events
            .iter()
            .position(|event| event.data["delta"]["type"] == "text_delta")
            .expect("preamble text delta");
        let text_stop = events
            .iter()
            .position(|event| {
                event.event == "content_block_stop" && event.data["index"].as_i64() == Some(0)
            })
            .expect("preamble block stop");
        let tool_start = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .expect("tool block start");
        assert!(text_delta < text_stop && text_stop < tool_start);
        assert_sequential_content_blocks(&events);
    }

    #[test]
    fn identity_sanitizer_flushes_short_preamble_before_truncated_tool_block() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-6", 10, false, false, HashMap::new());
        ctx.enable_identity_sanitization();

        let mut events = ctx.generate_initial_events();
        assert!(
            ctx.process_assistant_response("I'll inspect that.")
                .is_empty(),
            "short preamble should initially remain in the sanitizer"
        );
        events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "inspect".to_string(),
                tool_use_id: "toolu_identity_truncated".to_string(),
                input: r#"{"path":"src/li"#.to_string(),
                stop: false,
            }),
        );
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "I'll inspect that.");
        let block_types = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .filter_map(|event| event.data["content_block"]["type"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(block_types, vec!["text", "tool_use"]);

        let tool_start = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .expect("tool block start");
        assert!(
            events[..tool_start]
                .iter()
                .any(|event| event.data["delta"]["type"] == "text_delta"),
            "preamble must be emitted before the truncated tool starts"
        );
        assert!(
            !events[tool_start + 1..]
                .iter()
                .any(|event| event.data["delta"]["type"] == "text_delta"),
            "no pre-tool text may be replayed after the truncated tool starts"
        );
        let tool_delta = events
            .iter()
            .position(|event| event.data["delta"]["type"] == "input_json_delta")
            .expect("safe truncated tool delta");
        let tool_stop = events
            .iter()
            .position(|event| {
                event.event == "content_block_stop" && event.data["index"].as_i64() == Some(1)
            })
            .expect("truncated tool block stop");
        assert!(tool_start < tool_delta && tool_delta < tool_stop);
        let message_delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(message_delta.data["delta"]["stop_reason"], "max_tokens");
        assert_sequential_content_blocks(&events);
    }

    #[test]
    fn real_thinking_block_takes_precedence_over_synthetic_fallback() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-6", 10, true, true, HashMap::new());
        ctx.enable_identity_sanitization();
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response(
            "\n<thinking>\nI should respond as Kiro.\n</thinking>\n\nSAFE",
        ));
        events.extend(ctx.generate_final_events());

        let thinking = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "thinking_delta")
            .filter_map(|event| event.data["delta"]["thinking"].as_str())
            .collect::<String>();
        let text = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .filter_map(|event| event.data["delta"]["text"].as_str())
            .collect::<String>();

        assert!(thinking.contains("Claude"), "{thinking}");
        assert!(
            !thinking.to_ascii_lowercase().contains("kiro"),
            "{thinking}"
        );
        assert!(!thinking.contains("synthetic fallback"), "{thinking}");
        assert_eq!(text, "SAFE");
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.event == "content_block_start"
                        && event.data["content_block"]["type"] == "thinking"
                })
                .count(),
            1
        );
    }

    #[test]
    fn split_real_thinking_tag_takes_precedence_over_synthetic_fallback() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-6", 10, true, true, HashMap::new());
        ctx.enable_identity_sanitization();
        ctx.set_synthetic_thinking(Some("synthetic fallback".to_string()));

        let mut events = ctx.generate_initial_events();
        for chunk in [
            "\n",
            "  ",
            "<",
            "thi",
            "nking",
            ">\nI am Kiro.\n",
            "</thinking>",
            "\n\nSAFE",
        ] {
            events.extend(ctx.process_assistant_response(chunk));
        }
        events.extend(ctx.generate_final_events());

        let thinking = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "thinking_delta")
            .filter_map(|event| event.data["delta"]["thinking"].as_str())
            .collect::<String>();
        let text = events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .filter_map(|event| event.data["delta"]["text"].as_str())
            .collect::<String>();

        assert!(thinking.contains("Claude"), "{thinking}");
        assert!(
            !thinking.to_ascii_lowercase().contains("kiro"),
            "{thinking}"
        );
        assert!(!thinking.contains("synthetic fallback"), "{thinking}");
        assert_eq!(text, "SAFE");
    }

    #[test]
    fn stop_sequence_split_across_chunks_truncates_stream_and_sets_metadata() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, false, true, HashMap::new());
        ctx.set_stop_sequences(vec!["<STOP>".to_string()]);

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("alpha<ST"));
        events.extend(ctx.process_assistant_response("OP>omega"));
        events.extend(ctx.process_assistant_response("must not leak"));
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "alpha");
        let message_delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(message_delta.data["delta"]["stop_reason"], "stop_sequence");
        assert_eq!(message_delta.data["delta"]["stop_sequence"], "<STOP>");
    }

    #[test]
    fn obfuscated_thinking_is_sanitized_when_every_character_is_split() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, true, true, HashMap::new());
        ctx.enable_identity_sanitization();

        let mut events = ctx.generate_initial_events();
        let raw = "<thinking>In private reasoning I should respond as K(i)r{o} through C(o)d{e}W+h=i?s@p#e!r%e^r.</thinking>\n\nSAFE";
        for ch in raw.chars() {
            events.extend(ctx.process_assistant_response(&ch.to_string()));
        }
        events.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&events);
        assert!(!thinking.is_empty());
        assert!(
            !super::super::identity::contains_obfuscated_private_runtime_marker(&thinking),
            "obfuscated marker leaked across stream chunks: {thinking:?}"
        );
        let lower = thinking.to_ascii_lowercase();
        assert!(!lower.contains("kiro"), "{thinking}");
        assert!(!lower.contains("codewhisperer"), "{thinking}");
        assert_eq!(collect_text_content(&events), "SAFE");
    }

    #[test]
    fn thinking_only_probe_preserves_visible_code_fixture() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, true, true, HashMap::new());
        ctx.enable_identity_sanitization_with_options(false, false, false, false, true, false);

        let visible = r#"let fixture = "K(i)r{o}";"#;
        let raw = format!("<thinking>I should respond as K(i)r{{o}}.</thinking>\n\n{visible}");
        let mut events = ctx.generate_initial_events();
        for ch in raw.chars() {
            events.extend(ctx.process_assistant_response(&ch.to_string()));
        }
        events.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&events);
        assert!(
            !super::super::identity::contains_obfuscated_private_runtime_marker(&thinking),
            "thinking marker leaked: {thinking:?}"
        );
        assert_eq!(collect_text_content(&events), visible);
    }

    #[test]
    fn normal_code_identifier_survives_character_split_streaming() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, false, true, HashMap::new());
        ctx.enable_identity_sanitization_with_strict_mode(false);

        let code = "fn my_K(i)r{o}_value() -> i32 { 42 }";
        let mut events = ctx.generate_initial_events();
        for ch in code.chars() {
            events.extend(ctx.process_assistant_response(&ch.to_string()));
        }
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), code);
    }

    #[test]
    fn long_split_fence_streams_code_verbatim_and_protects_following_identity_prose() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-opus-4-8", 10, false, true, HashMap::new());
        ctx.enable_identity_sanitization_with_strict_mode(true);

        let code = format!("{}Kiro{}", "x".repeat(2500), "y".repeat(2500));
        let expected = format!("```text\n{code}\n```\nI am Claude.");
        let mut events = ctx.generate_initial_events();
        for chunk in ["`", "`", "`text\n"] {
            events.extend(ctx.process_assistant_response(chunk));
        }
        for chunk in code.as_bytes().chunks(97) {
            events.extend(
                ctx.process_assistant_response(std::str::from_utf8(chunk).expect("ASCII fixture")),
            );
        }
        for chunk in ["\n`", "`", "`\nI am ", "Kiro."] {
            events.extend(ctx.process_assistant_response(chunk));
        }
        events.extend(ctx.generate_final_events());

        let visible = collect_text_content(&events);
        assert_eq!(visible, expected);
    }

    #[test]
    fn stream_respects_output_token_limit_for_text_deltas() {
        let mut ctx =
            StreamContext::new_with_thinking("claude-sonnet-4-6", 10, false, false, HashMap::new());
        ctx.set_output_token_limit(2);
        let mut events = Vec::new();
        events.extend(ctx.generate_initial_events());
        events.extend(ctx.process_assistant_response("abcdefghijklmnopqrstuvwxyz"));
        events.extend(ctx.process_assistant_response("this should not be emitted"));
        events.extend(ctx.generate_final_events());

        let text = collect_text_content(&events);
        assert!(text.len() < "abcdefghijklmnopqrstuvwxyz".len());
        assert!(!text.contains("this should not be emitted"));

        let message_delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("message_delta should be emitted");
        assert_eq!(message_delta.data["delta"]["stop_reason"], "max_tokens");
        assert!(
            message_delta.data["usage"]["output_tokens"]
                .as_i64()
                .unwrap()
                <= 2
        );
    }

    #[test]
    fn merge_continuation_text_removes_repeated_tail() {
        assert_eq!(
            merge_continuation_text("3061\n3062\n3063", "3063\n3064\n3065"),
            "\n3064\n3065"
        );
    }

    #[test]
    fn merge_continuation_text_inserts_numeric_line_separator() {
        assert_eq!(
            merge_continuation_text("5792\n5793", "5794\n5795"),
            "\n5794\n5795"
        );
    }

    #[test]
    fn merge_continuation_text_leaves_regular_text_alone() {
        assert_eq!(
            merge_continuation_text("hello world", " and more"),
            " and more"
        );
    }

    #[test]
    fn suspect_end_turn_auto_continue_is_disabled() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 10, false, false, HashMap::new());
        ctx.process_assistant_response("short unfinished");
        assert!(
            !ctx.should_probe_auto_continue(26000),
            "short answers must not trigger speculative continuation"
        );

        let mut long_ctx =
            StreamContext::new_with_thinking("test-model", 10, false, false, HashMap::new());
        long_ctx.process_assistant_response(&format!("{}3046\n3", "x".repeat(12_000)));
        assert!(
            !long_ctx.should_probe_auto_continue(26000),
            "suspect end_turn continuation is disabled to avoid over-billing"
        );

        long_ctx.state_manager.set_has_tool_use(true);
        assert!(
            !long_ctx.should_probe_auto_continue(26000),
            "tool-use responses must not be auto-continued"
        );
    }

    #[test]
    fn uncommitted_continuation_keeps_partial_text_and_max_tokens() {
        let mut ctx =
            StreamContext::new_with_thinking("gpt-5.6-sol", 10, false, false, HashMap::new());
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("partial upstream answer"));
        events.extend(ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".to_string(),
            message: "output limit reached".to_string(),
        }));

        assert!(
            ctx.should_auto_continue(26_000),
            "the partial response should be eligible for a continuation attempt"
        );

        // A failed provider call leaves the transition uncommitted: handlers
        // only take raw content and clear max_tokens after receiving the next
        // upstream response.
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&events), "partial upstream answer");
        let message_delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(
            message_delta.data["delta"]["stop_reason"].as_str(),
            Some("max_tokens")
        );
    }

    #[test]
    fn completion_probe_sentinel_is_swallowed_without_changing_client_input() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 2000, false, false, HashMap::new());
        ctx.context_input_tokens = Some(5000);
        ctx.begin_completion_probe_for_billing(7000);

        let sentinel_events = ctx.process_assistant_response("__KRS_CONTINUATION_COMPLETE__");
        assert_eq!(
            collect_text_content(&sentinel_events),
            "",
            "internal completion sentinel must not be sent to the client"
        );

        let final_events = ctx.generate_final_events();
        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("message_delta should be emitted");
        assert_eq!(message_delta.data["usage"]["input_tokens"], 2000);
        assert_eq!(message_delta.data["usage"]["output_tokens"], 0);
    }

    #[test]
    fn completion_probe_sentinel_split_across_chunks_is_swallowed() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 2000, false, false, HashMap::new());
        ctx.begin_completion_probe_for_billing(7000);

        let mut events = Vec::new();
        events.extend(ctx.process_assistant_response("__KRS_"));
        events.extend(ctx.process_assistant_response("CONTINUATION_"));
        events.extend(ctx.process_assistant_response("COMPLETE__"));

        assert_eq!(
            collect_text_content(&events),
            "",
            "split internal sentinel must not leak to streamed clients"
        );
        assert_eq!(ctx.output_tokens, 0);
    }

    #[test]
    fn completion_probe_releases_real_continuation_when_prefix_diverges() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 2000, false, false, HashMap::new());
        ctx.begin_completion_probe_for_billing(7000);

        let mut events = Vec::new();
        events.extend(ctx.process_assistant_response("__KRS_"));
        events.extend(ctx.process_assistant_response("but this is real text"));

        assert_eq!(
            collect_text_content(&events),
            "__KRS_but this is real text",
            "non-sentinel content that shares a prefix must still be delivered"
        );
        assert!(ctx.output_tokens > 0);
    }

    #[test]
    fn continuation_does_not_accumulate_internal_rounds_into_client_input() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 2000, false, false, HashMap::new());
        ctx.context_input_tokens = Some(5000);
        ctx.begin_continuation_for_billing(7000);
        ctx.process_assistant_response("continued text");

        let final_events = ctx.generate_final_events();
        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("message_delta should be emitted");
        assert_eq!(message_delta.data["usage"]["input_tokens"], 2000);
        assert!(
            message_delta.data["usage"]["output_tokens"]
                .as_i64()
                .unwrap()
                > 0
        );
    }

    #[test]
    fn continuation_keeps_original_client_input_below_context_window() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-8",
            600_000,
            false,
            super::super::cache::UsageBreakdown::flat(600_000),
            HashMap::new(),
        );
        ctx.enable_aws_b40_compat();
        ctx.context_input_tokens = Some(600_000);
        ctx.begin_continuation_for_billing(600_000);

        let usage = ctx.final_usage_breakdown();

        assert_eq!(usage.input_tokens, 600_000);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
    }

    #[test]
    fn cached_continuation_keeps_original_local_cache_split() {
        let initial = super::super::cache::UsageBreakdown {
            input_tokens: 100,
            cache_read_input_tokens: 3954,
            cache_creation_input_tokens: 0,
            cache_creation_5m_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-sonnet-4-6",
            4054,
            false,
            initial,
            HashMap::new(),
        );
        ctx.begin_continuation_for_billing(3946);
        ctx.process_assistant_response("continued text");

        let final_events = ctx.generate_final_events();
        let usage = &final_events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta")
            .data["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["cache_read_input_tokens"], 3954);
        assert_eq!(usage["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_only_with_identity_sanitizer_emits_fallback_text() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        ctx.enable_identity_sanitization();
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "identity sanitizer must not buffer away thinking-only fallback text"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, true, false, HashMap::new());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }
}
