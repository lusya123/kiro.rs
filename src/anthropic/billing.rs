//! Usage billing policy.
//!
//! Kiro's `contextUsageEvent` includes a fixed service-side context floor of about
//! 4K tokens for even tiny prompts. Returning that floor directly makes short
//! user inputs look heavily overcharged, while it is much less visible on large
//! requests. For short requests we therefore expose the client request estimate;
//! once the request itself is substantial, we switch back to Kiro context usage.

/// Below this estimated client-request size, ignore Kiro's fixed context floor
/// when reporting billable input tokens.
pub const SHORT_INPUT_BILLING_THRESHOLD: i32 = 512;

pub fn billable_input_tokens(
    estimated_input_tokens: i32,
    context_input_tokens: Option<i32>,
) -> i32 {
    let estimated = estimated_input_tokens.max(1);

    if estimated <= SHORT_INPUT_BILLING_THRESHOLD {
        return estimated;
    }

    context_input_tokens.unwrap_or(estimated).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_requests_ignore_kiro_context_floor() {
        assert_eq!(billable_input_tokens(3, Some(4120)), 3);
        assert_eq!(billable_input_tokens(512, Some(4612)), 512);
    }

    #[test]
    fn large_requests_use_kiro_context_usage() {
        assert_eq!(billable_input_tokens(513, Some(4613)), 4613);
    }

    #[test]
    fn missing_context_uses_estimate() {
        assert_eq!(billable_input_tokens(1500, None), 1500);
    }
}
