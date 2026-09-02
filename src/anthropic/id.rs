//! Anthropic-compatible public ID generation.

const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const NON_HEX_BASE62: &[u8] = b"GHIJKLMNOPQRSTUVWXYZghijklmnopqrstuvwxyz";

fn random_base62(len: usize) -> String {
    (0..len)
        .map(|_| BASE62[fastrand::usize(..BASE62.len())] as char)
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

fn pomo_bedrock_message_id() -> String {
    // POMO exposes the Bedrock route as a lowercase `msg_bdrk_` prefix plus
    // the same versioned Anthropic identifier shape used by `request-id`:
    // uppercase `011C`, followed by exactly 20 Base62 characters.
    format!("msg_bdrk_011C{}", random_base62(20))
}

pub fn message_id() -> String {
    anthropic_id("msg")
}

pub fn bedrock_message_id_for_model(model: &str) -> String {
    let mapped_model = super::converter::map_model(model);
    if mapped_model
        .as_deref()
        .is_some_and(|model_id| model_id.starts_with("claude-"))
    {
        pomo_bedrock_message_id()
    } else {
        message_id()
    }
}

pub fn server_tool_use_id() -> String {
    anthropic_id("srvtoolu")
}

/// 客户端可见的 tool_use ID(Anthropic 形态 `toolu_01…`)。
/// 工具 ID 继续沿用独立的 Anthropic 兼容规则；消息 ID 是否带 `msg_bdrk_`
/// 不改变工具调用 ID 的协议约定。
pub fn tool_use_id() -> String {
    anthropic_id("toolu")
}

#[cfg(test)]
mod tests {
    use super::{bedrock_message_id_for_model, message_id, server_tool_use_id};

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
    fn claude_bedrock_message_ids_match_current_pomo_shape() {
        for id in [
            bedrock_message_id_for_model("claude-opus-5"),
            bedrock_message_id_for_model("claude-sonnet-5"),
            bedrock_message_id_for_model("claude-opus-4-8"),
            bedrock_message_id_for_model("claude-opus-4-7"),
            bedrock_message_id_for_model("claude-sonnet-4-6"),
            bedrock_message_id_for_model("Opus 5"),
            bedrock_message_id_for_model("Sonnet 5"),
            bedrock_message_id_for_model("haiku"),
        ] {
            assert!(id.starts_with("msg_bdrk_011C"), "{id}");
            assert_eq!(id.len(), 33, "{id}");
            let suffix = &id["msg_bdrk_".len()..];
            assert_eq!(&suffix[..4], "011C", "{id}");
            assert_eq!(suffix[4..].len(), 20, "{id}");
            assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
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
    fn non_claude_message_ids_do_not_expose_transport_markers() {
        for model in ["glm-5", "minimax-m2.5", "deepseek-3.2", "qwen3-coder-next"] {
            let id = bedrock_message_id_for_model(model);
            assert_anthropic_id(&id, "msg");
            assert!(id.starts_with("msg_01"), "{model}: {id}");
            assert!(!id.to_ascii_lowercase().contains("bdrk"), "{model}: {id}");
            assert_eq!(id.len(), 28);
        }
    }

    #[test]
    fn server_tool_ids_match_anthropic_shape() {
        assert_anthropic_id(&server_tool_use_id(), "srvtoolu");
    }
}
