//! Thinking 块伪签名生成
//!
//! Anthropic 官方 thinking 块带 `signature` 字段（服务端 Ed25519 签名）用于
//! 客户后续把 thinking 历史传回 Anthropic API 时校验完整性。
//!
//! kiro-rs 中转链路（customer → sub2api → kiro-rs → Kiro 上游）中：
//! - Kiro 上游不签名也不验签
//! - 客户传回的 history thinking 永远经过 kiro-rs，签名值不会被真 Anthropic 验
//!
//! 因此：本模块生成与 Anthropic 真实签名**长度/字符集相似**的伪签名，
//! 让客户 SDK 看到 `signature` 字段非空即可。Round-trip 时 converter.rs
//! 把 history 的 signature 字段直接丢弃，不影响转给 Kiro 的请求。
//!
//! 安全模型：伪签名永远是随机 base64 字符串，**不**承载任何真实校验语义。
//! 不要把它当真签名用于任何安全场景。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// Anthropic 真实签名长度区间（实测 716~788 字符），目标长度 720。
/// 720 字符 base64 ≈ 540 字节原始数据。
const RAW_BYTES: usize = 540;

/// 生成一个伪 thinking signature。
///
/// 每次调用都返回不同的随机值。客户视角看：是个~720字符的 base64 字符串，
/// 与 Anthropic 真实签名外观一致。
pub fn generate_fake_signature() -> String {
    let mut buf = [0u8; RAW_BYTES];
    for chunk in buf.chunks_mut(8) {
        let r = fastrand::u64(..).to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&r[..n]);
    }
    BASE64.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_has_expected_length_range() {
        for _ in 0..50 {
            let s = generate_fake_signature();
            // 540 bytes base64 → 720 chars (含 padding)
            assert!(
                s.len() >= 700 && s.len() <= 740,
                "signature length out of range: {}",
                s.len()
            );
        }
    }

    #[test]
    fn signature_is_valid_base64() {
        let s = generate_fake_signature();
        let decoded = BASE64.decode(&s).expect("must decode as base64");
        assert_eq!(decoded.len(), RAW_BYTES);
    }

    #[test]
    fn signatures_are_unique() {
        let a = generate_fake_signature();
        let b = generate_fake_signature();
        assert_ne!(a, b);
    }
}
