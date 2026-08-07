//! Anthropic-compatible public ID generation.

const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const NON_HEX_BASE62: &[u8] = b"GHIJKLMNOPQRSTUVWXYZghijklmnopqrstuvwxyz";
const LOWER_ALPHANUMERIC: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

fn random_base62(len: usize) -> String {
    (0..len)
        .map(|_| BASE62[fastrand::usize(..BASE62.len())] as char)
        .collect()
}

fn random_lower_alphanumeric(len: usize) -> String {
    (0..len)
        .map(|_| LOWER_ALPHANUMERIC[fastrand::usize(..LOWER_ALPHANUMERIC.len())] as char)
        .collect()
}

fn ensure_not_hex_suffix(mut suffix: String) -> String {
    if suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        suffix.pop();
        suffix.push(NON_HEX_BASE62[fastrand::usize(..NON_HEX_BASE62.len())] as char);
    }
    suffix
}

fn anthropic_id(prefix: &str) -> String {
    // Public Anthropic examples use 24-char Base62-looking suffixes commonly
    // beginning with "01" (for example: msg_01XFDUDYJgAACzvnptvVoYEL).
    // Avoid all-hex suffixes so responses do not look like UUID-derived IDs.
    let suffix = ensure_not_hex_suffix(format!("01{}", random_base62(22)));
    format!("{prefix}_{suffix}")
}

pub fn message_id() -> String {
    anthropic_id("msg")
}

fn anthropic_compatible_bedrock_message_id() -> String {
    // Keep the Bedrock marker while staying inside Anthropic's public
    // `msg_<18-40 alphanumeric chars>` compatibility contract.
    format!("msg_01bdrk{}", random_base62(18))
}

fn native_bedrock_message_id() -> String {
    // The current native Bedrock route sampled through the reference gateway
    // exposes a 52-character lowercase alphanumeric suffix after `msg_bdrk_`.
    format!("msg_bdrk_{}", random_lower_alphanumeric(52))
}

pub fn bedrock_message_id_for_model(model: &str) -> String {
    let mapped_model = super::converter::map_model(model);
    if mapped_model
        .as_deref()
        .is_some_and(|model_id| model_id.starts_with("gpt-"))
    {
        message_id()
    } else if mapped_model
        .as_deref()
        .is_some_and(|model_id| model_id.starts_with("claude-"))
    {
        native_bedrock_message_id()
    } else {
        anthropic_compatible_bedrock_message_id()
    }
}

pub fn server_tool_use_id() -> String {
    anthropic_id("srvtoolu")
}

/// 客户端可见的 tool_use ID(Anthropic 形态 `toolu_01…`)。
/// 后端返回的是 `toolu_bdrk_…`(Bedrock),会与我们已重写的 `msg_01…` 冲突暴露异源;
/// 统一重写成本函数生成的形态,与真 Anthropic / 参考渠道一致。
pub fn tool_use_id() -> String {
    anthropic_id("toolu")
}

#[cfg(test)]
mod tests {
    use super::{
        LOWER_ALPHANUMERIC, anthropic_compatible_bedrock_message_id, bedrock_message_id_for_model,
        message_id, native_bedrock_message_id, server_tool_use_id,
    };

    fn assert_anthropic_id(id: &str, prefix: &str) {
        let expected_prefix = format!("{prefix}_");
        assert!(id.starts_with(&expected_prefix));

        let suffix = &id[expected_prefix.len()..];
        assert_eq!(suffix.len(), 24);
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(
            !suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix should not look like UUID hex: {suffix}"
        );
    }

    #[test]
    fn message_ids_match_anthropic_shape() {
        assert_anthropic_id(&message_id(), "msg");
    }

    #[test]
    fn bedrock_message_ids_keep_marker_and_match_anthropic_shape() {
        let id = anthropic_compatible_bedrock_message_id();
        assert_anthropic_id(&id, "msg");
        assert!(id.starts_with("msg_01bdrk"));
        assert_eq!(id.len(), 28);
    }

    #[test]
    fn native_bedrock_message_ids_match_observed_gateway_shape() {
        for id in [
            native_bedrock_message_id(),
            bedrock_message_id_for_model("claude-opus-5"),
            bedrock_message_id_for_model("claude-sonnet-5"),
            bedrock_message_id_for_model("Opus 5"),
            bedrock_message_id_for_model("Sonnet 5"),
            bedrock_message_id_for_model("haiku"),
        ] {
            assert!(id.starts_with("msg_bdrk_"));
            assert_eq!(id.len(), 61);
            assert!(
                id["msg_bdrk_".len()..]
                    .bytes()
                    .all(|byte| LOWER_ALPHANUMERIC.contains(&byte))
            );
        }
    }

    #[test]
    fn gpt_message_ids_do_not_expose_bedrock_marker() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let id = bedrock_message_id_for_model(model);
            assert_anthropic_id(&id, "msg");
            assert!(!id.to_ascii_lowercase().contains("bdrk"), "{id}");
        }
    }

    #[test]
    fn native_bedrock_ids_are_limited_to_claude_models() {
        for model in ["glm-5", "minimax-m2.5", "deepseek-3.2", "qwen3-coder-next"] {
            let id = bedrock_message_id_for_model(model);
            assert_anthropic_id(&id, "msg");
            assert!(id.starts_with("msg_01bdrk"), "{model}: {id}");
            assert_eq!(id.len(), 28);
        }
    }

    #[test]
    fn server_tool_ids_match_anthropic_shape() {
        assert_anthropic_id(&server_tool_use_id(), "srvtoolu");
    }
}
