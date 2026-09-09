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

// POMO's time-ordered IDs decode as `01` + Base58(UUIDv7 bytes), not
// `011C` + random Base62. The apparent 011Ceq... prefix changes with time.
fn base58_128(mut value: u128) -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut encoded = [b'1'; 22];
    for digit in encoded.iter_mut().rev() {
        *digit = ALPHABET[(value % 58) as usize];
        value /= 58;
    }
    String::from_utf8(encoded.to_vec()).expect("ASCII Base58 alphabet")
}

fn pomo_bedrock_message_id() -> String {
    format!("msg_bdrk_01{}", base58_128(uuid::Uuid::now_v7().as_u128()))
}

/// Normalize Converse IDs to the POMO tool envelope. Native Bedrock IDs are
/// already compatible and remain opaque. A deterministic mapping keeps all
/// fragments consistent without a process-local lookup or a TTL. Kiro history
/// is stateless: assistant tool uses and client results carry this same ID.
pub fn bedrock_tool_use_id(upstream: &str) -> String {
    if !upstream.starts_with("tooluse_") {
        return upstream.to_owned();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(upstream.as_bytes());
    let bytes: [u8; 16] = digest[..16].try_into().expect("128-bit digest prefix");
    format!("toolu_bdrk_01{}", base58_128(u128::from_be_bytes(bytes)))
}

fn pomo_base32_message_id() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bytes = [
        fastrand::u128(..).to_be_bytes(),
        fastrand::u128(..).to_be_bytes(),
    ]
    .concat();
    let mut output = String::from("msg_bdrk_");
    let mut bits = 0u16;
    let mut count = 0;
    for byte in bytes {
        bits = (bits << 8) | u16::from(byte);
        count += 8;
        while count >= 5 {
            count -= 5;
            output.push(ALPHABET[((bits >> count) & 31) as usize] as char);
        }
    }
    if count > 0 {
        output.push(ALPHABET[((bits << (5 - count)) & 31) as usize] as char);
    }
    output
}

pub fn message_id() -> String {
    anthropic_id("msg")
}

pub fn bedrock_message_id_for_model(model: &str) -> String {
    let mapped_model = super::converter::map_model(model);
    if mapped_model
        .as_deref()
        .is_some_and(|id| id.starts_with("claude-haiku-") || id == "claude-opus-5")
    {
        // Fresh Opus 5 reference: 28/28 message IDs (plain and streaming)
        // use the same 256-bit Base32 envelope previously observed on Haiku.
        return pomo_base32_message_id();
    }
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
    fn pomo_message_ids_decode_to_current_ordered_uuid_v7() {
        let decode = |id: &str| {
            let alphabet = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
            let suffix = id.strip_prefix("msg_bdrk_01").unwrap();
            assert_eq!(suffix.len(), 22);
            let mut value = 0u128;
            for byte in suffix.bytes() {
                let digit = alphabet
                    .iter()
                    .position(|&c| c == byte)
                    .expect("Base58 digit");
                value = value
                    .checked_mul(58)
                    .unwrap()
                    .checked_add(digit as u128)
                    .unwrap();
            }
            value
        };
        // Captured POMO response at 2026-09-08T05:18:36.951Z.
        assert_eq!(
            decode("msg_bdrk_011CeqQaFreWDrY7uDPP1193"),
            0x01a07f7433977758818be1d730e5e9f2
        );
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ids: Vec<_> = (0..256)
            .map(|_| decode(&bedrock_message_id_for_model("claude-sonnet-4-6")))
            .collect();
        let end = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        for &id in &ids {
            assert_eq!((id >> 76) & 15, 7, "UUID version");
            assert_eq!((id >> 62) & 3, 2, "UUID variant");
            assert!(
                (start..=end).contains(&(id >> 80)),
                "message timestamp must match generation"
            );
        }
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn message_ids_match_anthropic_shape() {
        assert_anthropic_id(&message_id(), "msg");
    }

    #[test]
    fn claude_bedrock_message_ids_match_current_pomo_shape() {
        for id in [
            bedrock_message_id_for_model("claude-sonnet-5"),
            bedrock_message_id_for_model("claude-opus-4-8"),
            bedrock_message_id_for_model("claude-opus-4-7"),
            bedrock_message_id_for_model("claude-sonnet-4-6"),
            bedrock_message_id_for_model("Sonnet 5"),
        ] {
            assert!(id.starts_with("msg_bdrk_01"), "{id}");
            assert_eq!(id.len(), 33, "{id}");
            let suffix = &id["msg_bdrk_".len()..];
            assert_eq!(&suffix[..2], "01", "{id}");
            assert_eq!(suffix[2..].len(), 22, "{id}");
            assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
        }
    }

    #[test]
    fn pomo_haiku_message_ids_use_base32_256_bit_shape() {
        let id = bedrock_message_id_for_model("claude-haiku-4-5-20251001");
        let suffix = id.strip_prefix("msg_bdrk_").unwrap();
        assert_eq!(suffix.len(), 52);
        assert!(
            suffix
                .bytes()
                .all(|c| b"abcdefghijklmnopqrstuvwxyz234567".contains(&c))
        );
        assert!(
            b"aq".contains(&suffix.as_bytes()[51]),
            "last Base32 digit has four padding bits"
        );
    }

    #[test]
    fn pomo_opus5_message_ids_use_base32_256_bit_shape() {
        for model in ["claude-opus-5", "Opus 5"] {
            let ids: std::collections::HashSet<_> = (0..64)
                .map(|_| bedrock_message_id_for_model(model)).collect();
            assert_eq!(ids.len(), 64);
            for id in ids {
                let suffix = id.strip_prefix("msg_bdrk_").unwrap();
                assert_eq!(suffix.len(), 52);
                assert!(suffix.bytes().all(|c| b"abcdefghijklmnopqrstuvwxyz234567".contains(&c)));
                assert!(b"aq".contains(&suffix.as_bytes()[51]));
            }
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
    fn converse_mapping_is_stable_distinct_and_preserves_native_ids() {
        let first = super::bedrock_tool_use_id("tooluse_yiulrCVHZ5MVJa3AdVcgtf");
        assert_eq!(
            first,
            super::bedrock_tool_use_id("tooluse_yiulrCVHZ5MVJa3AdVcgtf")
        );
        assert_ne!(
            first,
            super::bedrock_tool_use_id("tooluse_YQOVJCCthHn2vS4r1UzalJ")
        );
        assert_eq!(
            super::bedrock_tool_use_id("toolu_bdrk_01DsPRT9aR6qWUjq8S2vJDbb"),
            "toolu_bdrk_01DsPRT9aR6qWUjq8S2vJDbb"
        );
    }

    #[test]
    fn server_tool_ids_match_anthropic_shape() {
        assert_anthropic_id(&server_tool_use_id(), "srvtoolu");
    }
}
