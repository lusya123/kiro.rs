//! Usage billing policy.
//!
//! `input_tokens` 一律采用本地 BPE 估算口径(与对标的 pomoai/真 Anthropic 一致)。
//! **不**回落到 Kiro 的 `contextUsageEvent`：Kiro 的计数对 JSON/密集文本系统性虚高
//! (实测某 JSON Kiro 记 8104，而 Claude 口径仅 ~3807)、且对小请求带 ~4K 固定上下文底噪。
//! 用本地估算才能与 pomoai 拟合,且对客户的计费口径与真 Anthropic 一致。
//! 多轮 auto-continue 的累加仍保留(见 `cache::finalize_request_usage`)。

pub fn billable_input_tokens(
    estimated_input_tokens: i32,
    _context_input_tokens: Option<i32>,
) -> i32 {
    estimated_input_tokens.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_uses_local_estimate_not_kiro_context() {
        // 一律返回本地估算，忽略 Kiro 的(虚高/带底噪)contextUsageEvent。
        assert_eq!(billable_input_tokens(3, Some(4120)), 3);
        assert_eq!(billable_input_tokens(2048, Some(6148)), 2048);
        assert_eq!(billable_input_tokens(2049, Some(6149)), 2049);
        assert_eq!(billable_input_tokens(3681, Some(8104)), 3681);
    }

    #[test]
    fn missing_context_uses_estimate() {
        assert_eq!(billable_input_tokens(1500, None), 1500);
    }
}
