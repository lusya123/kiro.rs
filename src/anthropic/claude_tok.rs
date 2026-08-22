//! Claude token 计数(逆向 Claude 词表 + 贪心最长匹配)。
//!
//! 背景:Anthropic 不公开 Claude 3+ 的分词器。社区项目 `ctoc`(github.com/rohangpta/ctoc)
//! 通过 `count_tokens` API "三明治计数"逆向出了 Claude 的词表;实测整体 ~3% 误差、代码 -2%、
//! JSON ~0%,远优于 cl100k(代码 -16%)。本模块内嵌其 `verified` 词表(38,360 个 token,
//! 长度前缀二进制 ~318KB),用与 ctoc 一致的**贪心字节级最长匹配**计数。
//!
//! 内存/速度:用 HashSet + 按首字节限长的最长匹配(非 256 叉数组 trie,后者每节点 2KB、
//! 几十万节点会吃数百 MB)。常驻内存 ~2MB,3.4 万字符约 1ms 量级。
//!
//! 词表来源是逆向数据(仓库无明确 license),仅用于本地 usage 估算。

use std::collections::HashSet;
use std::sync::OnceLock;

/// 长度前缀二进制:重复的 [u16 LE 长度][该长度的 token 字节]。由 `verified` 词表生成。
static VOCAB_BIN: &[u8] = include_bytes!("claude_vocab.bin");

struct Vocab {
    tokens: HashSet<Box<[u8]>>,
    /// 每个首字节对应的"最长 token 字节数",用于收紧贪心匹配的尝试范围。
    max_len_by_first: [usize; 256],
    max_token_len: usize,
}

fn vocab() -> &'static Vocab {
    static V: OnceLock<Vocab> = OnceLock::new();
    V.get_or_init(|| {
        let mut tokens = HashSet::new();
        let mut max_len_by_first = [0usize; 256];
        let mut i = 0usize;
        while i + 2 <= VOCAB_BIN.len() {
            let len = u16::from_le_bytes([VOCAB_BIN[i], VOCAB_BIN[i + 1]]) as usize;
            i += 2;
            if i + len > VOCAB_BIN.len() {
                break;
            }
            let tok = &VOCAB_BIN[i..i + len];
            i += len;
            if let Some(&first) = tok.first() {
                if len > max_len_by_first[first as usize] {
                    max_len_by_first[first as usize] = len;
                }
            }
            tokens.insert(tok.to_vec().into_boxed_slice());
        }
        Vocab {
            tokens,
            max_len_by_first,
            max_token_len: max_len_by_first.iter().copied().max().unwrap_or(1),
        }
    })
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF       // 平假名/片假名
        | 0x3400..=0x4DBF     // CJK 扩展 A
        | 0x4E00..=0x9FFF     // CJK 统一表意
        | 0xAC00..=0xD7AF     // 谚文
        | 0xF900..=0xFAFF     // CJK 兼容表意
        | 0x20000..=0x2A6DF   // CJK 扩展 B
    )
}

/// 规范的"Claude token 数":贪心 ctoc 计数 + CJK 校准。
///
/// ctoc 对 ASCII(英文/JSON/代码)已是 Claude 口径(系数 1.0),但对 CJK 系统性偏高 ~8.5%。
/// 按 CJK 字符占比线性混合一个收缩系数(纯 ASCII→1.0,纯 CJK→0.92)。纯 ASCII 走快路径。
/// 输入/输出/思考/缓存的 token 计数都应走本函数,保证口径统一。
pub fn count_claude(text: &str) -> i32 {
    let raw = count_tokens(text);
    let (cjk, total) = text.chars().fold((0usize, 0usize), |(cjk, total), ch| {
        (cjk + usize::from(is_cjk(ch)), total + 1)
    });
    calibrate_cjk_count(raw, cjk, total)
}

fn calibrate_cjk_count(raw: i32, cjk: usize, total: usize) -> i32 {
    if raw <= 0 {
        return 0;
    }
    if cjk == 0 {
        return raw; // 纯 ASCII:ctoc 已准
    }
    let frac = cjk as f64 / total as f64;
    let factor = 1.0 + (0.92 - 1.0) * frac;
    ((raw as f64) * factor).round().max(1.0) as i32
}

/// 与 `count_claude` 完全相同的流式计数器。只保留不超过词表最长 token
/// 的边界尾巴，因此上游流越长也不会线性占用内存。
#[derive(Clone, Debug, Default)]
pub struct StreamingClaudeTokenCounter {
    stable_raw_tokens: i32,
    pending: Vec<u8>,
    cjk_chars: usize,
    total_chars: usize,
}

impl StreamingClaudeTokenCounter {
    pub fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.total_chars += 1;
            if is_cjk(ch) {
                self.cjk_chars += 1;
            }
        }
        let vocab = vocab();
        // Feed bounded chunks so a one-shot multi-megabyte prompt cannot make
        // the boundary Vec retain the prompt's former capacity forever.
        for chunk in text.as_bytes().chunks(512) {
            self.pending.extend_from_slice(chunk);
            let mut consumed = 0usize;
            while self.pending.len().saturating_sub(consumed) > vocab.max_token_len {
                let token_len = greedy_token_len(&self.pending, consumed, vocab);
                consumed += token_len;
                self.stable_raw_tokens = self.stable_raw_tokens.saturating_add(1);
            }
            if consumed > 0 {
                self.pending.drain(..consumed);
            }
        }
    }

    pub fn count(&self) -> i32 {
        let raw = self
            .stable_raw_tokens
            .saturating_add(count_token_bytes(&self.pending));
        calibrate_cjk_count(raw, self.cjk_chars, self.total_chars)
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.pending.capacity()
    }

    pub fn reset(&mut self) {
        self.stable_raw_tokens = 0;
        self.pending.clear();
        self.cjk_chars = 0;
        self.total_chars = 0;
    }
}

/// Truncate text without exceeding a Claude token budget.
///
/// This walks the same greedy vocabulary as `count_tokens` and stops as soon
/// as the raw token budget is exhausted, so an incident-sized input does not
/// need to be fully tokenized or copied before it can be bounded. The raw
/// count is an upper bound for the CJK-calibrated public count.
pub fn truncate_to_claude_tokens(text: &str, max_tokens: i32) -> String {
    if text.is_empty() || max_tokens <= 0 {
        return String::new();
    }

    let bytes = text.as_bytes();
    let vocab = vocab();
    let mut count = 0i32;
    let mut end = 0usize;
    while end < bytes.len() && count < max_tokens {
        end += greedy_token_len(bytes, end, vocab);
        count += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    while end > 0 && count_claude(&text[..end]) > max_tokens {
        end = text[..end]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }
    text[..end].to_string()
}

fn greedy_token_len(bytes: &[u8], pos: usize, vocab: &Vocab) -> usize {
    let cap = vocab.max_len_by_first[bytes[pos] as usize];
    let hi = (pos + cap).min(bytes.len());
    let mut end = hi;
    while end > pos {
        if vocab.tokens.contains(&bytes[pos..end]) {
            return end - pos;
        }
        end -= 1;
    }
    1
}

/// 贪心字节级最长匹配的 token 数(与 ctoc.cc 一致:未命中的字节按 1 token 兜底)。
pub fn count_tokens(text: &str) -> i32 {
    count_token_bytes(text.as_bytes())
}

fn count_token_bytes(bytes: &[u8]) -> i32 {
    if bytes.is_empty() {
        return 0;
    }
    let v = vocab();
    let mut count = 0i32;
    let mut pos = 0usize;
    while pos < bytes.len() {
        pos += greedy_token_len(bytes, pos, v);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_vocab() {
        assert!(
            vocab().tokens.len() > 30_000,
            "vocab should load ~38k tokens"
        );
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn counts_are_reasonable() {
        // 纯英文应接近 ~1.3 token/词;贪心结果应稳定且 >0。
        let n = count_tokens("The quick brown fox jumps over the lazy dog.");
        assert!((8..=14).contains(&n), "got {n}");
        // 中文每字通常 1~2 token。
        let z = count_tokens("人工智能正在改变世界");
        assert!(z >= 5, "got {z}");
    }

    #[test]
    fn greedy_prefers_longer_tokens() {
        // 重复空格等会被合并成更长的 token,token 数应远小于字节数。
        let spaces = " ".repeat(64);
        assert!(count_tokens(&spaces) < 20, "runs should merge");
    }

    #[test]
    fn streaming_counter_matches_full_counter_at_every_character_boundary() {
        let text = "Plan carefully：先检查 JSON，再输出 tool_result ✅ with  multiple   spaces.";
        let expected = count_claude(text);
        let boundaries = text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();

        for &split in &boundaries {
            let mut counter = StreamingClaudeTokenCounter::default();
            counter.push_str(&text[..split]);
            counter.push_str(&text[split..]);
            assert_eq!(counter.count(), expected, "split at byte {split}");
        }
    }

    #[test]
    fn streaming_counter_can_be_reset_between_upstream_rounds() {
        let mut counter = StreamingClaudeTokenCounter::default();
        counter.push_str("first round reasoning");
        assert_eq!(counter.count(), count_claude("first round reasoning"));

        counter.reset();
        counter.push_str("第二轮");

        assert_eq!(counter.count(), count_claude("第二轮"));
    }

    #[test]
    fn streaming_counter_retains_only_a_bounded_token_boundary() {
        let text = "reasoning-token ".repeat(16 * 1024);
        let mut counter = StreamingClaudeTokenCounter::default();

        counter.push_str(&text);

        assert_eq!(counter.count(), count_claude(&text));
        assert!(
            counter.pending.len() <= vocab().max_token_len,
            "streaming token accounting must not retain output-sized text"
        );
    }
}
