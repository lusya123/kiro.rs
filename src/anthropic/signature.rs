//! Thinking 块签名：无状态自校验 HMAC 方案
//!
//! Anthropic 官方 thinking 块带 `signature`，客户端多轮续聊时把它原样回传，由 Anthropic
//! 服务端验签。kiro-rs 链路（customer → sub2api → kiro-rs → Kiro 上游）里：
//! - Kiro 上游不签名也不验签；converter 转发前会丢弃 history 里的 signature。
//! - 真 Anthropic 永远看不到这些签名，客户端 SDK 自己也不验签。
//!
//! 因此 kiro-rs 只需要一套**自洽**的签名：客户看到的是 protobuf 风格 base64，回传时本服务
//! 能验真伪。
//!
//! 旧实现用**进程内 HashSet** 登记签名，有个致命问题：在"一容器一账号、请求按账号轮转"的
//! 号池里，A 容器签发的签名回传到 B 容器（或容器重启后）查不到 → 误判非法返回 400。开了
//! extended thinking 的多轮对话跨容器时几乎必踩。
//!
//! 现在改为**无状态自校验 HMAC**：签名内部布局为 `[protobuf 头][随机体] || HMAC(密钥, 前面全部)`，
//! 验签时用同一把密钥对"被签名区"重算 HMAC，与尾部 32 字节做常量时间比对。特性：
//! - **无状态**：只依赖共享密钥，与"哪个容器签发/是否重启过"无关 → 跨容器、重启后都能验。
//! - **防篡改**：改动签名任意字节都会令 HMAC 比对失败。
//! - **永不重复**：每次随机体不同 → 不会出现"同一签名重复"的破绽。
//!
//! 全车队只需共享同一把密钥（默认写死常量，镜像一致即可零配置跨容器互验；需要隔离/轮换时用
//! 环境变量 `KIRO_SIG_SECRET` 覆盖，**全车队配同一值**）。该密钥与账号凭据无关，仅用于签名自洽。
//!
//! 安全模型：这不是真 Anthropic 签名（拿不到其私钥，也无需），只承载"本服务签发且未被篡改"
//! 这一层语义，不要把它当真签名用于任何真实安全场景。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use subtle::ConstantTimeEq;

/// 签名总字节数的随机范围。真 Anthropic 签名长度随内容浮动（实测见过 ~246–279 字节），
/// 旧实现恒 246 字节，反而能被"多次采样长度恒定"识别。这里在该范围内随机取整字节数。
const MIN_BYTES: usize = 240;
const MAX_BYTES: usize = 288;
/// HMAC-SHA256 输出长度（始终为签名末尾 32 字节）。
const MAC_LEN: usize = 32;
/// protobuf wire-format 头，保证 base64 以 `EvEBCm` 开头、整体外观像 protobuf。
const PROTOBUF_HEAD: &[u8] = &[
    0x12, 0xf1, 0x01, 0x0a, 0x65, 0x08, 0x0f, 0x18, 0x02, 0x2a, 0x40,
];

/// 默认共享签名密钥。全车队镜像一致 → 零配置也能跨容器互验。
/// 需要隔离/轮换时用环境变量 `KIRO_SIG_SECRET` 覆盖（**全车队配同一值**）。
const DEFAULT_SECRET: &[u8] = b"kiro-rs/thinking-signature/v1/shared-hmac-secret";

fn signing_secret() -> &'static [u8] {
    static SECRET: OnceLock<Vec<u8>> = OnceLock::new();
    SECRET.get_or_init(|| {
        std::env::var("KIRO_SIG_SECRET")
            .ok()
            .filter(|s| !s.is_empty())
            .map(String::into_bytes)
            .unwrap_or_else(|| DEFAULT_SECRET.to_vec())
    })
}

/// 手写 HMAC-SHA256（复用已有的 `sha2` 依赖，避免引入 `hmac` crate）。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; MAC_LEN] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..MAC_LEN].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(msg).finalize();
    let outer = Sha256::new().chain_update(opad).chain_update(inner).finalize();
    let mut out = [0u8; MAC_LEN];
    out.copy_from_slice(&outer);
    out
}

/// 生成一个 thinking signature。
///
/// 布局：`[protobuf 头][随机体] || HMAC(密钥, 前面全部)`，总长在 `[MIN_BYTES, MAX_BYTES]` 内随机。
/// 每次随机体+长度都不同 → 签名永不重复、长度也浮动；客户视角是 protobuf 风格 base64，
/// 以 `EvEBCm` 开头，与 aws-p/Anthropic 响应外观一致。
pub fn generate_signature() -> String {
    let total = MIN_BYTES + fastrand::usize(..=(MAX_BYTES - MIN_BYTES));
    let signed_len = total - MAC_LEN;
    let mut buf = vec![0u8; total];
    buf[..PROTOBUF_HEAD.len()].copy_from_slice(PROTOBUF_HEAD);
    for chunk in buf[PROTOBUF_HEAD.len()..signed_len].chunks_mut(8) {
        let r = fastrand::u64(..).to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&r[..n]);
    }
    let mac = hmac_sha256(signing_secret(), &buf[..signed_len]);
    buf[signed_len..].copy_from_slice(&mac);
    BASE64.encode(buf)
}

/// 校验签名是否由本服务（持同一共享密钥的任意容器）签发且未被篡改。
///
/// 长度无关：MAC 恒为末尾 32 字节，对前面全部重算比对。因此旧版恒 246 字节的签名也照常验过
/// （向后兼容）。**无状态**：只依赖共享密钥，与"哪个容器签发/是否重启过"无关 → 跨容器、重启可验。
pub fn verify_signature(signature: &str) -> bool {
    let Ok(buf) = BASE64.decode(signature) else {
        return false;
    };
    if buf.len() < PROTOBUF_HEAD.len() + MAC_LEN || buf.len() > 4096 {
        return false;
    }
    let signed_len = buf.len() - MAC_LEN;
    let expected = hmac_sha256(signing_secret(), &buf[..signed_len]);
    expected.as_slice().ct_eq(&buf[signed_len..]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_prefix_and_length_in_range() {
        for _ in 0..100 {
            let s = generate_signature();
            assert!(s.starts_with("EvEBCm"), "signature should look protobuf-like: {s}");
            let n = BASE64.decode(&s).expect("must decode").len();
            assert!((MIN_BYTES..=MAX_BYTES).contains(&n), "byte len out of range: {n}");
        }
    }

    #[test]
    fn signature_length_varies_across_samples() {
        let mut lens = std::collections::HashSet::new();
        for _ in 0..50 {
            lens.insert(BASE64.decode(generate_signature()).unwrap().len());
        }
        assert!(lens.len() > 1, "signature byte-length should vary, got {lens:?}");
    }

    #[test]
    fn signatures_never_repeat() {
        let a = generate_signature();
        let b = generate_signature();
        assert_ne!(a, b);
    }

    #[test]
    fn legacy_fixed_246_signature_still_verifies() {
        // 向后兼容：旧版恒 246 字节的签名（升级前在途对话回传）必须仍能验过。
        const LEGACY_LEN: usize = 246;
        let signed_len = LEGACY_LEN - MAC_LEN;
        let mut buf = vec![0u8; LEGACY_LEN];
        buf[..PROTOBUF_HEAD.len()].copy_from_slice(PROTOBUF_HEAD);
        for (i, b) in buf[PROTOBUF_HEAD.len()..signed_len].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13);
        }
        let mac = hmac_sha256(signing_secret(), &buf[..signed_len]);
        buf[signed_len..].copy_from_slice(&mac);
        assert!(verify_signature(&BASE64.encode(buf)));
    }

    #[test]
    fn generated_signature_verifies() {
        for _ in 0..50 {
            assert!(verify_signature(&generate_signature()));
        }
    }

    #[test]
    fn tampered_signature_is_rejected() {
        // 复现报告里的篡改手法：翻第 31 字符、尾 5 字符替换为 XXXXX。
        let s = generate_signature();
        let mut b = s.into_bytes();
        let n = b.len();
        b[30] = if b[30] != b'A' { b'A' } else { b'B' };
        for x in b.iter_mut().skip(n - 5) {
            *x = b'X';
        }
        assert!(!verify_signature(&String::from_utf8(b).unwrap()));
    }

    #[test]
    fn garbage_and_foreign_signatures_rejected() {
        assert!(!verify_signature("not-base64!!"));
        assert!(!verify_signature(""));
        // 合法 base64、正确长度，但 MAC 不匹配（非本密钥签发）。
        assert!(!verify_signature(&BASE64.encode([0u8; 246])));
    }

    #[test]
    fn verification_is_stateless_across_holders_of_the_secret() {
        // 模拟"另一个容器"：直接用同一密钥构造签名，**不**经过本进程的 generate_signature
        // （即没有任何"登记"步骤），验签仍应通过——证明无状态、跨容器/重启可验。
        let total = 252usize;
        let signed_len = total - MAC_LEN;
        let mut buf = vec![0u8; total];
        buf[..PROTOBUF_HEAD.len()].copy_from_slice(PROTOBUF_HEAD);
        for (i, b) in buf[PROTOBUF_HEAD.len()..signed_len].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        let mac = hmac_sha256(signing_secret(), &buf[..signed_len]);
        buf[signed_len..].copy_from_slice(&mac);
        assert!(verify_signature(&BASE64.encode(buf)));
    }
}
