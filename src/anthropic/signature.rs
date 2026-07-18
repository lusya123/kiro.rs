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

/// HMAC-SHA256 输出长度。
const MAC_LEN: usize = 32;

// AWS-B keeps the observed Bedrock signature shapes instead of exposing the
// Anthropic-shaped protobuf signature used by the AWS-P profile.
const AWS_B40_RAW_BYTES: usize = 198;
const AWS_B40_OPUS_46_RAW_BYTES: usize = 231;
const AWS_B40_SONNET_45_RAW_BYTES: &[usize] = &[309, 357];
const AWS_B40_HAIKU_45_RAW_BYTES: &[usize] = &[270, 285];
const AWS_B40_ADAPTIVE_RAW_BYTES: usize = 372;
const AWS_B_OPUS_48_THINKING_BLOB_BYTES: usize = 55;
const AWS_B_OPUS_48_ADAPTIVE_MIN_BLOB_BYTES: usize = 579;
const AWS_B_OPUS_48_ADAPTIVE_MAX_BLOB_BYTES: usize = 4_096;
const AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS: i32 = 10_000;
const AWS_B_OPUS_48_CACHE_CREATE_CONTEXT_DIVISOR: usize = 7;
const AWS_B_OPUS_48_CACHE_READ_CONTEXT_DIVISOR: usize = 12;
const AWS_B_OPUS_48_CACHE_CREATE_CONTEXT_MAX_BYTES: usize = 3_584;
const AWS_B_OPUS_48_CACHE_READ_CONTEXT_MAX_BYTES: usize = 2_048;

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
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(msg)
        .finalize();
    let outer = Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize();
    let mut out = [0u8; MAC_LEN];
    out.copy_from_slice(&outer);
    out
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    for chunk in v.chunks_mut(8) {
        let r = fastrand::u64(..).to_le_bytes();
        let k = chunk.len();
        chunk.copy_from_slice(&r[..k]);
    }
    v
}

fn generate_hmac_blob(raw_bytes: usize) -> String {
    debug_assert!(raw_bytes > MAC_LEN);
    let mut buf = rand_bytes(raw_bytes);
    let signed_len = buf.len() - MAC_LEN;
    let mac = hmac_sha256(signing_secret(), &buf[..signed_len]);
    buf[signed_len..].copy_from_slice(&mac);
    BASE64.encode(buf)
}

fn is_opus_4_8(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("opus-4-8") || lower.contains("opus-4.8")
}

fn rand_decimal_bytes(n: usize) -> Vec<u8> {
    (0..n)
        .map(|_| b'0' + fastrand::usize(0..10) as u8)
        .collect()
}

pub fn generate_aws_b40_signature_for_model(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    if is_opus_4_8(model) {
        return generate_aws_bedrock_opus_48_signature(AWS_B_OPUS_48_THINKING_BLOB_BYTES, false);
    }

    let raw_bytes = if lower.contains("opus-4-6") || lower.contains("opus-4.6") {
        AWS_B40_OPUS_46_RAW_BYTES
    } else if lower.contains("sonnet-4-5") || lower.contains("sonnet-4.5") {
        AWS_B40_SONNET_45_RAW_BYTES[fastrand::usize(..AWS_B40_SONNET_45_RAW_BYTES.len())]
    } else if lower.contains("haiku-4-5") || lower.contains("haiku-4.5") {
        AWS_B40_HAIKU_45_RAW_BYTES[fastrand::usize(..AWS_B40_HAIKU_45_RAW_BYTES.len())]
    } else {
        AWS_B40_RAW_BYTES
    };
    generate_hmac_blob(raw_bytes)
}

pub fn generate_aws_b40_adaptive_signature_for_model(
    model: &str,
    thinking_bytes: usize,
    context_tokens: i32,
    cache_read_input_tokens: i32,
) -> String {
    if !is_opus_4_8(model) {
        return generate_hmac_blob(AWS_B40_ADAPTIVE_RAW_BYTES);
    }

    if context_tokens < AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS {
        return generate_aws_bedrock_opus_48_signature(AWS_B_OPUS_48_THINKING_BLOB_BYTES, false);
    }

    // Exact POMO replays of the same 34k-token adaptive request expose two
    // stable Bedrock variants. A cache creation uses the 99-byte header and a
    // larger encrypted blob; a cache read uses the 113-byte header (field 11 is
    // a 12-digit cache stamp) and a smaller blob. The visible thinking summary
    // is much shorter than the encrypted payload, so context and cache state
    // are the reliable inputs here.
    let cache_read = cache_read_input_tokens > 0;
    let (context_divisor, context_max) = if cache_read {
        (
            AWS_B_OPUS_48_CACHE_READ_CONTEXT_DIVISOR,
            AWS_B_OPUS_48_CACHE_READ_CONTEXT_MAX_BYTES,
        )
    } else {
        (
            AWS_B_OPUS_48_CACHE_CREATE_CONTEXT_DIVISOR,
            AWS_B_OPUS_48_CACHE_CREATE_CONTEXT_MAX_BYTES,
        )
    };
    let context_boost = ((context_tokens - AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS) as usize
        / context_divisor)
        .min(context_max);
    let blob_bytes = thinking_bytes
        .saturating_mul(2)
        .saturating_add(context_boost)
        .saturating_add(fastrand::usize(..=320))
        .saturating_sub(160)
        .clamp(
            AWS_B_OPUS_48_ADAPTIVE_MIN_BLOB_BYTES,
            AWS_B_OPUS_48_ADAPTIVE_MAX_BLOB_BYTES,
        );
    generate_aws_bedrock_opus_48_signature(blob_bytes, cache_read)
}

/// 追加 protobuf 变长整数(varint)。
fn push_varint(buf: &mut Vec<u8>, mut v: usize) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf.push(b);
        if v == 0 {
            break;
        }
    }
}

fn push_varint_field(buf: &mut Vec<u8>, field: u8, value: usize) {
    buf.push(field << 3);
    push_varint(buf, value);
}

/// 追加一个 length-delimited 字段:`tag(field<<3|2)` + `varint(len)` + `content`。
fn push_len_field(buf: &mut Vec<u8>, field: u8, content: &[u8]) {
    buf.push((field << 3) | 2);
    push_varint(buf, content.len());
    buf.extend_from_slice(content);
}

/// Build the AWS Bedrock Opus 4.8 protobuf envelope observed on both enabled
/// and adaptive thinking responses. The final 32 bytes of field 5 remain a
/// local HMAC so signatures are still stateless and tamper-evident when they
/// are returned through a non-Bedrock validation path.
fn generate_aws_bedrock_opus_48_signature(
    thinking_blob_bytes: usize,
    include_large_context_stamp: bool,
) -> String {
    let thinking_blob_bytes = thinking_blob_bytes.max(MAC_LEN);

    let mut f1 = Vec::with_capacity(if include_large_context_stamp { 113 } else { 99 });
    push_varint_field(&mut f1, 1, 15);
    push_varint_field(&mut f1, 2, 1);
    push_varint_field(&mut f1, 3, 2);
    push_len_field(&mut f1, 5, &rand_bytes(64));
    push_len_field(&mut f1, 6, b"claude-quince");
    push_varint_field(&mut f1, 7, 0);
    push_len_field(&mut f1, 8, b"thinking");
    if include_large_context_stamp {
        push_len_field(&mut f1, 11, &rand_decimal_bytes(12));
    }

    let mut inner = Vec::with_capacity(f1.len() + thinking_blob_bytes + 96);
    push_len_field(&mut inner, 1, &f1);
    push_len_field(&mut inner, 2, &rand_bytes(12));
    push_len_field(&mut inner, 3, &rand_bytes(12));
    push_len_field(&mut inner, 4, &rand_bytes(48));
    push_len_field(&mut inner, 5, &rand_bytes(thinking_blob_bytes));

    let mut buf = Vec::with_capacity(inner.len() + 8);
    push_len_field(&mut buf, 2, &inner);
    push_varint_field(&mut buf, 3, 1);

    let mac_start = buf.len() - 2 - MAC_LEN;
    let mac = hmac_sha256(signing_secret(), &buf[..mac_start]);
    buf[mac_start..mac_start + MAC_LEN].copy_from_slice(&mac);
    BASE64.encode(buf)
}

/// 生成一个 thinking signature，**在字节结构上复刻真 Anthropic 的 protobuf 布局**:
///
/// ```text
/// field2 (len-delimited) {
///   field1 (99B): 08 0f 18 02 2a 40 [64B] [随机]   // 与真签名同构的内层头
///   field2 (12B) field3 (12B) field4 (48B)
///   field5 (大 blob, 长度浮动)                       // 承载随机体 + 末尾 32B HMAC
/// }
/// field3 (varint) = 1                                // 18 01 收尾,真签名恒有此字段
/// ```
///
/// 总长在 ~360–510 字节间随机浮动(真签名实测 364–503)。HMAC 位于整签名末尾 `18 01` 之前的
/// 32 字节(即 field5 内容尾部),验签时对其之前的全部字节重算比对。这样:客户/检测器解析出的
/// 结构、字段、长度分布都与真 Anthropic 一致;本服务仍能凭共享密钥无状态验真伪。
pub fn generate_signature() -> String {
    // 内层 field1:固定同构头 + 随机填充到 99 字节。
    let mut f1 = vec![0x08, 0x0f, 0x18, 0x02, 0x2a, 0x40];
    f1.extend(rand_bytes(64));
    f1.extend(rand_bytes(99 - f1.len()));
    f1.truncate(99);

    // field5 大 blob,长度浮动 → 总长浮动。末尾 32 字节稍后被 HMAC 覆盖。
    let f5_len = 175 + fastrand::usize(..=150);
    let f5 = rand_bytes(f5_len);

    // 组装 field2 的内容。
    let mut inner = Vec::new();
    push_len_field(&mut inner, 1, &f1);
    push_len_field(&mut inner, 2, &rand_bytes(12));
    push_len_field(&mut inner, 3, &rand_bytes(12));
    push_len_field(&mut inner, 4, &rand_bytes(48));
    push_len_field(&mut inner, 5, &f5);

    // 完整签名:field2 { inner } + field3 = 1。
    let mut buf = Vec::new();
    push_len_field(&mut buf, 2, &inner);
    buf.push(0x18);
    buf.push(0x01);

    // HMAC 覆盖 "MAC 与尾部 18 01 之前" 的全部;MAC 位于 [len-34, len-2)。
    let mac_start = buf.len() - 2 - MAC_LEN;
    let mac = hmac_sha256(signing_secret(), &buf[..mac_start]);
    buf[mac_start..mac_start + MAC_LEN].copy_from_slice(&mac);
    BASE64.encode(buf)
}

/// 校验签名是否由本服务（持同一共享密钥的任意容器）签发且未被篡改。**无状态**、跨容器/重启可验。
/// 兼容两种布局:新版(MAC 在尾部 `18 01` 之前的 32 字节)与旧版(MAC 恒为末尾 32 字节)。
pub fn verify_signature(signature: &str) -> bool {
    let Ok(buf) = BASE64.decode(signature) else {
        return false;
    };
    if buf.len() < MAC_LEN + 4 || buf.len() > 8192 {
        return false;
    }
    let secret = signing_secret();
    // 新版:签名以 `18 01`(field3=1)收尾,MAC 在其前 32 字节。
    if buf.len() >= MAC_LEN + 2 && buf[buf.len() - 2] == 0x18 && buf[buf.len() - 1] == 0x01 {
        let mac_start = buf.len() - 2 - MAC_LEN;
        let expected = hmac_sha256(secret, &buf[..mac_start]);
        if bool::from(
            expected
                .as_slice()
                .ct_eq(&buf[mac_start..mac_start + MAC_LEN]),
        ) {
            return true;
        }
    }
    // 旧版(向后兼容在途对话):MAC 恒为末尾 32 字节。
    let signed_len = buf.len() - MAC_LEN;
    let expected = hmac_sha256(secret, &buf[..signed_len]);
    expected.as_slice().ct_eq(&buf[signed_len..]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与真 Anthropic thinking 签名同构:顶层 `field2 { … }` + `field3=1`(以 `18 01` 收尾),
    /// 总长在真签名实测区间(364–503)附近浮动。
    fn parse_top_level(raw: &[u8]) -> (usize, usize) {
        // 返回 (field2 内容长度, 已消费到的偏移),仅解析顶层 field2 头。
        assert_eq!(raw[0], 0x12, "顶层应为 field2 (0x12)");
        let mut i = 1usize;
        let (len, adv) = {
            let mut v = 0usize;
            let mut sh = 0u32;
            let mut j = i;
            loop {
                let b = raw[j];
                j += 1;
                v |= ((b & 0x7f) as usize) << sh;
                if b & 0x80 == 0 {
                    break;
                }
                sh += 7;
            }
            (v, j - i)
        };
        i += adv;
        (len, i)
    }

    #[test]
    fn signature_matches_anthropic_structure() {
        for _ in 0..100 {
            let s = generate_signature();
            let raw = BASE64.decode(&s).expect("must decode");
            let n = raw.len();
            assert!((340..=520).contains(&n), "byte len out of range: {n}");
            // 以 field3=1 收尾。
            assert_eq!(&raw[n - 2..], &[0x18, 0x01], "应以 field3=1 收尾: {s}");
            // 顶层 field2 头合法,且其长度 + 头 + 2字节尾 == 总长。
            let (f2len, off) = parse_top_level(&raw);
            assert_eq!(off + f2len + 2, n, "field2 长度应与总长自洽");
            // 内层以真签名的 field1 头开始。
            assert_eq!(
                &raw[off..off + 8],
                &[0x0a, 0x63, 0x08, 0x0f, 0x18, 0x02, 0x2a, 0x40],
                "内层 field1 头应与真签名一致"
            );
        }
    }

    #[test]
    fn signature_length_varies_across_samples() {
        let mut lens = std::collections::HashSet::new();
        for _ in 0..50 {
            lens.insert(BASE64.decode(generate_signature()).unwrap().len());
        }
        assert!(
            lens.len() > 1,
            "signature byte-length should vary, got {lens:?}"
        );
    }

    #[test]
    fn signatures_never_repeat() {
        let a = generate_signature();
        let b = generate_signature();
        assert_ne!(a, b);
    }

    #[test]
    fn legacy_fixed_246_signature_still_verifies() {
        // 向后兼容：旧版恒 246 字节、MAC 在末尾 32 字节的签名（升级前在途对话回传）必须仍能验过。
        const LEGACY_HEAD: &[u8] = &[
            0x12, 0xf1, 0x01, 0x0a, 0x65, 0x08, 0x0f, 0x18, 0x02, 0x2a, 0x40,
        ];
        const LEGACY_LEN: usize = 246;
        let signed_len = LEGACY_LEN - MAC_LEN;
        let mut buf = vec![0u8; LEGACY_LEN];
        buf[..LEGACY_HEAD.len()].copy_from_slice(LEGACY_HEAD);
        for (i, b) in buf[LEGACY_HEAD.len()..signed_len].iter_mut().enumerate() {
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
        const HEAD: &[u8] = &[
            0x12, 0xf1, 0x01, 0x0a, 0x65, 0x08, 0x0f, 0x18, 0x02, 0x2a, 0x40,
        ];
        let total = 252usize;
        let signed_len = total - MAC_LEN;
        let mut buf = vec![0u8; total];
        buf[..HEAD.len()].copy_from_slice(HEAD);
        for (i, b) in buf[HEAD.len()..signed_len].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        let mac = hmac_sha256(signing_secret(), &buf[..signed_len]);
        buf[signed_len..].copy_from_slice(&mac);
        assert!(verify_signature(&BASE64.encode(buf)));
    }

    #[test]
    fn aws_b40_signatures_keep_observed_bedrock_lengths() {
        let cases: &[(&str, &[usize])] = &[
            ("claude-opus-4-8", &[241]),
            ("claude-opus-4-6", &[AWS_B40_OPUS_46_RAW_BYTES]),
            ("claude-sonnet-4-5", AWS_B40_SONNET_45_RAW_BYTES),
            ("claude-haiku-4-5", AWS_B40_HAIKU_45_RAW_BYTES),
            ("claude-opus-4-7", &[AWS_B40_RAW_BYTES]),
        ];

        for (model, expected_lengths) in cases {
            for _ in 0..20 {
                let raw = BASE64
                    .decode(generate_aws_b40_signature_for_model(model))
                    .expect("AWS-B signature must decode");
                assert!(
                    expected_lengths.contains(&raw.len()),
                    "unexpected AWS-B signature length for {model}: {}",
                    raw.len()
                );
                assert!(
                    verify_signature(&BASE64.encode(&raw)),
                    "AWS-B must accept its own {model} signature"
                );
            }
        }

        let opus_48 = BASE64
            .decode(generate_aws_b40_signature_for_model("claude-opus-4-8"))
            .expect("Opus 4.8 AWS-B signature must decode");
        assert_eq!(
            &opus_48[..13],
            &[
                0x12, 0xec, 0x01, 0x0a, 0x63, 0x08, 0x0f, 0x10, 0x01, 0x18, 0x02, 0x2a, 0x40,
            ]
        );
        assert!(opus_48.windows(13).any(|w| w == b"claude-quince"));
        assert!(opus_48.windows(8).any(|w| w == b"thinking"));
        assert_eq!(&opus_48[opus_48.len() - 2..], &[0x18, 0x01]);
        assert!(verify_signature(&BASE64.encode(&opus_48)));
    }

    #[test]
    fn aws_b40_opus_48_adaptive_signature_tracks_context_shape() {
        for _ in 0..20 {
            let low = generate_aws_b40_adaptive_signature_for_model("claude-opus-4-8", 303, 49, 0);
            let low_raw = BASE64.decode(&low).expect("low-context signature decodes");
            assert_eq!(low_raw.len(), 241);
            let (low_inner_len, low_off) = parse_top_level(&low_raw);
            assert_eq!(low_off + low_inner_len + 2, low_raw.len());
            assert_eq!(&low_raw[low_off..low_off + 4], &[0x0a, 0x63, 0x08, 0x0f]);
            assert!(verify_signature(&low));

            let cache_create =
                generate_aws_b40_adaptive_signature_for_model("claude-opus-4-8", 143, 35_000, 0);
            let cache_create_raw = BASE64
                .decode(&cache_create)
                .expect("cache-creation signature decodes");
            assert!(
                (3_884..=4_204).contains(&cache_create_raw.len()),
                "{}",
                cache_create_raw.len()
            );
            let (cache_create_inner_len, cache_create_off) = parse_top_level(&cache_create_raw);
            assert_eq!(
                cache_create_off + cache_create_inner_len + 2,
                cache_create_raw.len()
            );
            assert_eq!(
                &cache_create_raw[cache_create_off..cache_create_off + 4],
                &[0x0a, 0x63, 0x08, 0x0f]
            );
            assert!(verify_signature(&cache_create));

            let cache_read = generate_aws_b40_adaptive_signature_for_model(
                "claude-opus-4-8",
                143,
                35_000,
                34_250,
            );
            let cache_read_raw = BASE64
                .decode(&cache_read)
                .expect("large-context signature decodes");
            assert!(
                (2_375..=2_695).contains(&cache_read_raw.len()),
                "{}",
                cache_read_raw.len()
            );
            let (cache_read_inner_len, cache_read_off) = parse_top_level(&cache_read_raw);
            assert_eq!(
                cache_read_off + cache_read_inner_len + 2,
                cache_read_raw.len()
            );
            assert_eq!(
                &cache_read_raw[cache_read_off..cache_read_off + 4],
                &[0x0a, 0x71, 0x08, 0x0f]
            );
            assert!(cache_read_raw.windows(13).any(|w| w == b"claude-quince"));
            assert!(verify_signature(&cache_read));
        }

        let legacy_model = BASE64
            .decode(generate_aws_b40_adaptive_signature_for_model(
                "claude-opus-4-7",
                143,
                35_000,
                34_250,
            ))
            .expect("legacy adaptive signature decodes");
        assert_eq!(legacy_model.len(), AWS_B40_ADAPTIVE_RAW_BYTES);
        assert!(verify_signature(&BASE64.encode(legacy_model)));
    }
}
