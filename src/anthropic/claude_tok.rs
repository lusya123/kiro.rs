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
    if raw <= 0 {
        return 0;
    }
    let mut cjk = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        total += 1;
        if is_cjk(c) {
            cjk += 1;
        }
    }
    if cjk == 0 {
        return raw; // 纯 ASCII:ctoc 已准
    }
    let frac = cjk as f64 / total as f64;
    let factor = 1.0 + (0.92 - 1.0) * frac;
    ((raw as f64) * factor).round().max(1.0) as i32
}

/// 贪心字节级最长匹配的 token 数(与 ctoc.cc 一致:未命中的字节按 1 token 兜底)。
pub fn count_tokens(text: &str) -> i32 {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return 0;
    }
    let v = vocab();
    let mut count = 0i32;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let cap = v.max_len_by_first[bytes[pos] as usize];
        let hi = (pos + cap).min(bytes.len());
        let mut matched = 0usize;
        let mut end = hi;
        while end > pos {
            if v.tokens.contains(&bytes[pos..end]) {
                matched = end - pos;
                break;
            }
            end -= 1;
        }
        pos += if matched == 0 { 1 } else { matched };
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_vocab() {
        assert!(vocab().tokens.len() > 30_000, "vocab should load ~38k tokens");
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
}
