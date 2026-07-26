//! Thinking 块签名：无状态自校验 HMAC 方案
//!
//! Anthropic 官方 thinking 块带 `signature`，客户端多轮续聊时把它原样回传，由 Anthropic
//! 服务端验签。kiro-rs 链路（customer → sub2api → kiro-rs → Kiro 上游）里：
//! - 新版 Kiro 模型会返回原生 reasoning signature，本服务原样透传。
//! - 旧模型没有上游签名时才生成本地 HMAC 回退；converter 转发历史前仍会移除 signature。
//! - 真 Anthropic 永远看不到这些签名，客户端 SDK 自己也不验签。
//!
//! 因此本模块同时处理两类签名：本地回退签名可严格验 HMAC；上游签名无法使用本地密钥验算，
//! 但会按完整 protobuf/Bedrock 信封做严格结构校验，以支持客户端原样回传后继续对话。
//! 对本进程刚刚透传过的上游签名，还会保留一个有界、短期的紧凑指纹登记，用于识别客户端
//! 回传时发生的局部篡改。登记缺失不会导致拒绝，因此跨容器、重启后的合法历史仍然可用。
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
//! 安全模型：本地回退签名不是真 Anthropic 签名，只承载"本服务签发且未被篡改"这一层语义。
//! 原生上游签名保持不透明；两者都不是本服务的授权边界。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::OnceLock,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// HMAC-SHA256 输出长度。
const MAC_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureValidationFailure {
    InvalidBase64,
    InvalidLength,
    HmacMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureValidationDiagnostics {
    pub encoded_len: usize,
    pub decoded_len: Option<usize>,
    pub ends_with_field3: bool,
    pub has_bedrock_profile_markers: bool,
    pub failure: SignatureValidationFailure,
}

// AWS-B keeps the observed Bedrock signature shapes instead of exposing the
// Anthropic-shaped protobuf signature used by the AWS-P profile.
const AWS_B40_RAW_BYTES: usize = 198;
const AWS_B40_OPUS_46_RAW_BYTES: usize = 231;
const AWS_B40_SONNET_45_RAW_BYTES: &[usize] = &[309, 357];
const AWS_B40_HAIKU_45_RAW_BYTES: &[usize] = &[270, 285];
const AWS_B40_ADAPTIVE_RAW_BYTES: usize = 372;
const AWS_B_OPUS_48_THINKING_BLOB_BYTES: usize = 55;
const AWS_B_OPUS_48_ADAPTIVE_MIN_BLOB_BYTES: usize = 55;
const AWS_B_OPUS_48_ADAPTIVE_LOW_CONTEXT_MAX_BLOB_BYTES: usize = 256;
const AWS_B_OPUS_48_ADAPTIVE_MAX_BLOB_BYTES: usize = 3_200;
const AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS: i32 = 10_000;
const AWS_B_OPUS_48_CONTEXT_DIVISOR: usize = 10;
const AWS_B_OPUS_48_CONTEXT_MAX_BYTES: usize = 2_600;
const AWS_B_OPUS_48_CONTEXT_STAMP: &[u8; 12] = b"058264511794";
const EXTERNAL_ANTHROPIC_MIN_RAW_BYTES: usize = 340;
const EXTERNAL_ANTHROPIC_MAX_RAW_BYTES: usize = 520;
const UPSTREAM_BEDROCK_MIN_RAW_BYTES: usize = 196;
const UPSTREAM_BEDROCK_MAX_RAW_BYTES: usize = 2 * 1024 * 1024;
const ISSUED_SIGNATURE_CAPACITY: usize = 1_024;
const ISSUED_SIGNATURE_CHUNKS: usize = 8;
const ISSUED_SIGNATURE_RETENTION: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuedSignatureMatch {
    Exact,
    LocallyModified,
    Unknown,
}

#[derive(Debug)]
struct IssuedSignatureFingerprint {
    observed_at: Instant,
    decoded_len: usize,
    digest: [u8; 32],
    chunk_digests: [[u8; 32]; ISSUED_SIGNATURE_CHUNKS],
}

fn issued_signature_registry() -> &'static Mutex<VecDeque<IssuedSignatureFingerprint>> {
    static REGISTRY: OnceLock<Mutex<VecDeque<IssuedSignatureFingerprint>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(VecDeque::with_capacity(ISSUED_SIGNATURE_CAPACITY)))
}

fn issued_signature_fingerprint(signature: &str) -> Option<IssuedSignatureFingerprint> {
    let decoded = BASE64.decode(signature).ok()?;
    if decoded.is_empty() || decoded.len() > UPSTREAM_BEDROCK_MAX_RAW_BYTES {
        return None;
    }

    let digest = Sha256::digest(&decoded).into();
    let chunk_digests = std::array::from_fn(|index| {
        let start = decoded.len() * index / ISSUED_SIGNATURE_CHUNKS;
        let end = decoded.len() * (index + 1) / ISSUED_SIGNATURE_CHUNKS;
        Sha256::digest(&decoded[start..end]).into()
    });
    Some(IssuedSignatureFingerprint {
        observed_at: Instant::now(),
        decoded_len: decoded.len(),
        digest,
        chunk_digests,
    })
}

fn prune_issued_signatures(registry: &mut VecDeque<IssuedSignatureFingerprint>, now: Instant) {
    while registry.front().is_some_and(|entry| {
        now.saturating_duration_since(entry.observed_at) > ISSUED_SIGNATURE_RETENTION
    }) {
        registry.pop_front();
    }
}

pub fn register_issued_opaque_signature(signature: &str) {
    let Some(fingerprint) = issued_signature_fingerprint(signature) else {
        return;
    };
    let mut registry = issued_signature_registry().lock();
    prune_issued_signatures(&mut registry, fingerprint.observed_at);
    if registry
        .iter()
        .any(|entry| entry.digest == fingerprint.digest)
    {
        return;
    }
    registry.push_back(fingerprint);
    while registry.len() > ISSUED_SIGNATURE_CAPACITY {
        registry.pop_front();
    }
}

pub fn issued_opaque_signature_match(signature: &str) -> IssuedSignatureMatch {
    let Some(candidate) = issued_signature_fingerprint(signature) else {
        return IssuedSignatureMatch::Unknown;
    };
    let mut registry = issued_signature_registry().lock();
    prune_issued_signatures(&mut registry, candidate.observed_at);

    let mut locally_modified = false;
    for issued in registry
        .iter()
        .filter(|entry| entry.decoded_len == candidate.decoded_len)
    {
        if issued.digest == candidate.digest {
            return IssuedSignatureMatch::Exact;
        }
        let matching_chunks = issued
            .chunk_digests
            .iter()
            .zip(candidate.chunk_digests.iter())
            .filter(|(left, right)| left == right)
            .count();
        locally_modified |= matching_chunks + 1 >= ISSUED_SIGNATURE_CHUNKS;
    }

    if locally_modified {
        IssuedSignatureMatch::LocallyModified
    } else {
        IssuedSignatureMatch::Unknown
    }
}

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
    _cache_read_input_tokens: i32,
) -> String {
    if !is_opus_4_8(model) {
        return generate_hmac_blob(AWS_B40_ADAPTIVE_RAW_BYTES);
    }

    if context_tokens < AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS {
        let blob_bytes = thinking_bytes
            .saturating_add(fastrand::usize(..=160))
            .saturating_sub(80)
            .clamp(
                AWS_B_OPUS_48_ADAPTIVE_MIN_BLOB_BYTES,
                AWS_B_OPUS_48_ADAPTIVE_LOW_CONTEXT_MAX_BLOB_BYTES,
            );
        return generate_aws_bedrock_opus_48_signature(blob_bytes, false);
    }

    // Current Bedrock/POMO captures use the same stamped protobuf header for
    // both cache creation and cache reads. The encrypted thinking payload still
    // varies between generations, but its distribution does not flip merely
    // because the prompt cache changed state.
    let context_boost = ((context_tokens - AWS_B_OPUS_48_LARGE_CONTEXT_TOKENS) as usize
        / AWS_B_OPUS_48_CONTEXT_DIVISOR)
        .min(AWS_B_OPUS_48_CONTEXT_MAX_BYTES);
    let blob_bytes = thinking_bytes
        .saturating_mul(2)
        .saturating_add(context_boost)
        .saturating_add(fastrand::usize(..=480))
        .saturating_sub(240)
        .clamp(
            AWS_B_OPUS_48_ADAPTIVE_MIN_BLOB_BYTES,
            AWS_B_OPUS_48_ADAPTIVE_MAX_BLOB_BYTES,
        );
    generate_aws_bedrock_opus_48_signature(blob_bytes, true)
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

fn read_varint(buf: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0u32;
    let mut index = start;
    while index < buf.len() && shift < usize::BITS {
        let byte = buf[index];
        index += 1;
        value |= ((byte & 0x7f) as usize).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((value, index));
        }
        shift += 7;
    }
    None
}

fn take_len_field<'a>(buf: &'a [u8], cursor: &mut usize, field: u8) -> Option<&'a [u8]> {
    let expected_key = ((field as usize) << 3) | 2;
    let (key, after_key) = read_varint(buf, *cursor)?;
    if key != expected_key {
        return None;
    }
    let (len, start) = read_varint(buf, after_key)?;
    let end = start.checked_add(len)?;
    let value = buf.get(start..end)?;
    *cursor = end;
    Some(value)
}

fn has_external_signature_header(field_one: &[u8]) -> bool {
    if !(64..=128).contains(&field_one.len()) {
        return false;
    }

    let mut cursor = 0usize;
    let Some((field_one_key, after_field_one_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if field_one_key != 0x08 {
        return false;
    }
    let Some((profile, after_profile)) = read_varint(field_one, after_field_one_key) else {
        return false;
    };
    if !(8..=32).contains(&profile) {
        return false;
    }
    cursor = after_profile;

    // Some providers include field 2 = 1 here; others omit it.
    if read_varint(field_one, cursor).is_some_and(|(key, _)| key == 0x10) {
        let Some((_, after_key)) = read_varint(field_one, cursor) else {
            return false;
        };
        let Some((variant, after_variant)) = read_varint(field_one, after_key) else {
            return false;
        };
        if variant > 8 {
            return false;
        }
        cursor = after_variant;
    }

    let Some((thinking_key, after_thinking_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if thinking_key != 0x18 {
        return false;
    }
    let Some((thinking_variant, after_thinking_variant)) =
        read_varint(field_one, after_thinking_key)
    else {
        return false;
    };
    if thinking_variant != 2 {
        return false;
    }
    cursor = after_thinking_variant;

    take_len_field(field_one, &mut cursor, 5).is_some_and(|payload| payload.len() == 64)
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
        push_len_field(&mut f1, 11, AWS_B_OPUS_48_CONTEXT_STAMP);
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

/// Recognize the protobuf envelope used by Anthropic thinking signatures that
/// came from another provider. AWS-B cannot verify a foreign provider's MAC,
/// but accepting a well-formed opaque signature keeps imported conversations
/// usable; the converter drops the signature before calling Kiro upstream.
///
/// Bedrock-shaped signatures are deliberately excluded here. Those are issued
/// by this profile and must continue through the strict local HMAC check.
pub fn is_plausible_external_anthropic_signature(signature: &str) -> bool {
    let Ok(buf) = BASE64.decode(signature) else {
        return false;
    };
    if !(EXTERNAL_ANTHROPIC_MIN_RAW_BYTES..=EXTERNAL_ANTHROPIC_MAX_RAW_BYTES).contains(&buf.len())
        || !buf.ends_with(&[0x18, 0x01])
        || buf.first() != Some(&0x12)
        || buf
            .windows(b"claude-quince".len())
            .any(|window| window == b"claude-quince")
        || buf
            .windows(b"thinking".len())
            .any(|window| window == b"thinking")
    {
        return false;
    }

    let Some((outer_len, inner_start)) = read_varint(&buf, 1) else {
        return false;
    };
    let Some(inner_end) = inner_start.checked_add(outer_len) else {
        return false;
    };
    if inner_end.checked_add(2) != Some(buf.len()) {
        return false;
    }

    let inner = &buf[inner_start..inner_end];
    let mut cursor = 0usize;
    let Some(field_one) = take_len_field(inner, &mut cursor, 1) else {
        return false;
    };
    let Some(field_two) = take_len_field(inner, &mut cursor, 2) else {
        return false;
    };
    let Some(field_three) = take_len_field(inner, &mut cursor, 3) else {
        return false;
    };
    let Some(field_four) = take_len_field(inner, &mut cursor, 4) else {
        return false;
    };
    let Some(field_five) = take_len_field(inner, &mut cursor, 5) else {
        return false;
    };

    cursor == inner.len()
        && field_two.len() == 12
        && field_three.len() == 12
        && field_four.len() == 48
        && field_five.len() >= 128
        && has_external_signature_header(field_one)
}

/// Recognize the complete Bedrock reasoning envelope returned by newer Kiro
/// models. These signatures can be much larger than the legacy 8 KiB local
/// validation cap because field 5 carries encrypted reasoning content.
///
/// This is used only to keep an already-issued thinking block usable when a
/// client returns it as conversation history. The converter removes the opaque
/// signature before forwarding history to Kiro, so it is not an authorization
/// or trust boundary.
pub fn is_plausible_upstream_bedrock_signature(signature: &str) -> bool {
    let Ok(buf) = BASE64.decode(signature) else {
        return false;
    };
    if !(UPSTREAM_BEDROCK_MIN_RAW_BYTES..=UPSTREAM_BEDROCK_MAX_RAW_BYTES).contains(&buf.len())
        || !buf.ends_with(&[0x18, 0x01])
        || buf.first() != Some(&0x12)
    {
        return false;
    }

    let Some((outer_len, inner_start)) = read_varint(&buf, 1) else {
        return false;
    };
    let Some(inner_end) = inner_start.checked_add(outer_len) else {
        return false;
    };
    if inner_end.checked_add(2) != Some(buf.len()) {
        return false;
    }

    let inner = &buf[inner_start..inner_end];
    let mut cursor = 0usize;
    let Some(field_one) = take_len_field(inner, &mut cursor, 1) else {
        return false;
    };
    let Some(field_two) = take_len_field(inner, &mut cursor, 2) else {
        return false;
    };
    let Some(field_three) = take_len_field(inner, &mut cursor, 3) else {
        return false;
    };
    let Some(field_four) = take_len_field(inner, &mut cursor, 4) else {
        return false;
    };
    let Some(field_five) = take_len_field(inner, &mut cursor, 5) else {
        return false;
    };

    cursor == inner.len()
        && field_two.len() == 12
        && field_three.len() == 12
        && field_four.len() == 48
        && field_five.len() >= MAC_LEN
        && has_upstream_bedrock_signature_header(field_one)
}

fn has_upstream_bedrock_signature_header(field_one: &[u8]) -> bool {
    if !(96..=160).contains(&field_one.len()) {
        return false;
    }

    let mut cursor = 0usize;
    let Some((field_one_key, after_field_one_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if field_one_key != 0x08 {
        return false;
    }
    let Some((profile, after_profile)) = read_varint(field_one, after_field_one_key) else {
        return false;
    };
    // Current native Kiro/Bedrock reasoning uses profile 16. Locally generated
    // compatibility signatures use profile 15 and must continue through HMAC.
    if profile != 16 {
        return false;
    }
    cursor = after_profile;

    let Some((variant_key, after_variant_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if variant_key != 0x10 {
        return false;
    }
    let Some((variant, after_variant)) = read_varint(field_one, after_variant_key) else {
        return false;
    };
    if variant != 1 {
        return false;
    }
    cursor = after_variant;

    let Some((thinking_key, after_thinking_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if thinking_key != 0x18 {
        return false;
    }
    let Some((thinking_variant, after_thinking_variant)) =
        read_varint(field_one, after_thinking_key)
    else {
        return false;
    };
    if thinking_variant != 2 {
        return false;
    }
    cursor = after_thinking_variant;

    let Some(nonce) = take_len_field(field_one, &mut cursor, 5) else {
        return false;
    };
    let Some(model_family) = take_len_field(field_one, &mut cursor, 6) else {
        return false;
    };
    let Some((mode_key, after_mode_key)) = read_varint(field_one, cursor) else {
        return false;
    };
    if mode_key != 0x38 {
        return false;
    }
    let Some((mode, after_mode)) = read_varint(field_one, after_mode_key) else {
        return false;
    };
    cursor = after_mode;
    let Some(reasoning_kind) = take_len_field(field_one, &mut cursor, 8) else {
        return false;
    };

    if cursor < field_one.len() {
        let Some(context_stamp) = take_len_field(field_one, &mut cursor, 11) else {
            return false;
        };
        if context_stamp.len() != 12 || !context_stamp.iter().all(u8::is_ascii_digit) {
            return false;
        }
    }

    cursor == field_one.len()
        && nonce.len() == 64
        && model_family == b"claude-quince"
        && mode == 0
        && reasoning_kind == b"thinking"
}

/// 校验签名是否由本服务（持同一共享密钥的任意容器）签发且未被篡改。**无状态**、跨容器/重启可验。
/// 兼容两种布局:新版(MAC 在尾部 `18 01` 之前的 32 字节)与旧版(MAC 恒为末尾 32 字节)。
pub fn validate_signature(signature: &str) -> Result<(), SignatureValidationDiagnostics> {
    let encoded_len = signature.len();
    let Ok(buf) = BASE64.decode(signature) else {
        return Err(SignatureValidationDiagnostics {
            encoded_len,
            decoded_len: None,
            ends_with_field3: false,
            has_bedrock_profile_markers: false,
            failure: SignatureValidationFailure::InvalidBase64,
        });
    };

    let decoded_len = buf.len();
    let ends_with_field3 = buf.ends_with(&[0x18, 0x01]);
    let has_bedrock_profile_markers = buf
        .windows(b"claude-quince".len())
        .any(|window| window == b"claude-quince")
        && buf
            .windows(b"thinking".len())
            .any(|window| window == b"thinking");
    if buf.len() < MAC_LEN + 4 || buf.len() > 8192 {
        return Err(SignatureValidationDiagnostics {
            encoded_len,
            decoded_len: Some(decoded_len),
            ends_with_field3,
            has_bedrock_profile_markers,
            failure: SignatureValidationFailure::InvalidLength,
        });
    }
    let secret = signing_secret();
    // 新版:签名以 `18 01`(field3=1)收尾,MAC 在其前 32 字节。
    if ends_with_field3 {
        let mac_start = buf.len() - 2 - MAC_LEN;
        let expected = hmac_sha256(secret, &buf[..mac_start]);
        if bool::from(
            expected
                .as_slice()
                .ct_eq(&buf[mac_start..mac_start + MAC_LEN]),
        ) {
            return Ok(());
        }
    }
    // 旧版(向后兼容在途对话):MAC 恒为末尾 32 字节。
    let signed_len = buf.len() - MAC_LEN;
    let expected = hmac_sha256(secret, &buf[..signed_len]);
    if bool::from(expected.as_slice().ct_eq(&buf[signed_len..])) {
        return Ok(());
    }

    Err(SignatureValidationDiagnostics {
        encoded_len,
        decoded_len: Some(decoded_len),
        ends_with_field3,
        has_bedrock_profile_markers,
        failure: SignatureValidationFailure::HmacMismatch,
    })
}

#[cfg(test)]
pub fn verify_signature(signature: &str) -> bool {
    validate_signature(signature).is_ok()
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
    fn plausible_external_anthropic_envelope_is_distinct_from_local_hmac() {
        let mut foreign = BASE64.decode(generate_signature()).unwrap();
        let mac_start = foreign.len() - 2 - MAC_LEN;
        foreign[mac_start] ^= 0x01;
        let foreign = BASE64.encode(foreign);

        assert_eq!(
            validate_signature(&foreign).unwrap_err().failure,
            SignatureValidationFailure::HmacMismatch
        );
        assert!(is_plausible_external_anthropic_signature(&foreign));

        let mut bedrock = BASE64
            .decode(generate_aws_b40_signature_for_model("claude-opus-4-8"))
            .unwrap();
        bedrock[40] ^= 0x01;
        assert!(!is_plausible_external_anthropic_signature(
            &BASE64.encode(bedrock)
        ));
        assert!(!is_plausible_external_anthropic_signature("not-base64!!"));
    }

    #[test]
    fn accepts_cctest_external_signature_variant_without_accepting_bedrock_tampering() {
        let mut field_one = Vec::new();
        push_varint_field(&mut field_one, 1, 12);
        push_varint_field(&mut field_one, 3, 2);
        push_len_field(&mut field_one, 5, &[0x41; 64]);
        push_len_field(&mut field_one, 6, b"claude-opus-4-6");
        push_varint_field(&mut field_one, 7, 0);
        assert_eq!(field_one.len(), 89);

        let mut inner = Vec::new();
        push_len_field(&mut inner, 1, &field_one);
        push_len_field(&mut inner, 2, &[0x42; 12]);
        push_len_field(&mut inner, 3, &[0x43; 12]);
        push_len_field(&mut inner, 4, &[0x44; 48]);
        push_len_field(&mut inner, 5, &[0x45; 261]);

        let mut external = Vec::new();
        push_len_field(&mut external, 2, &inner);
        push_varint_field(&mut external, 3, 1);
        assert_eq!(external.len(), 438);
        let external = BASE64.encode(external);
        assert!(is_plausible_external_anthropic_signature(&external));
        assert_eq!(
            validate_signature(&external).unwrap_err().failure,
            SignatureValidationFailure::HmacMismatch
        );

        let mut malformed = BASE64.decode(&external).unwrap();
        malformed[3] = 0x0b;
        assert!(!is_plausible_external_anthropic_signature(
            &BASE64.encode(malformed)
        ));

        let mut bedrock = BASE64
            .decode(generate_aws_b40_signature_for_model("claude-opus-4-8"))
            .unwrap();
        bedrock[40] ^= 0x01;
        assert!(!is_plausible_external_anthropic_signature(
            &BASE64.encode(bedrock)
        ));
    }

    #[test]
    fn recognizes_large_native_bedrock_signature_without_weakening_local_hmac() {
        fn fixture(profile: usize) -> String {
            let mut field_one = Vec::new();
            push_varint_field(&mut field_one, 1, profile);
            push_varint_field(&mut field_one, 2, 1);
            push_varint_field(&mut field_one, 3, 2);
            push_len_field(&mut field_one, 5, &[0x41; 64]);
            push_len_field(&mut field_one, 6, b"claude-quince");
            push_varint_field(&mut field_one, 7, 0);
            push_len_field(&mut field_one, 8, b"thinking");
            push_len_field(&mut field_one, 11, b"058264511794");

            let mut inner = Vec::new();
            push_len_field(&mut inner, 1, &field_one);
            push_len_field(&mut inner, 2, &[0x42; 12]);
            push_len_field(&mut inner, 3, &[0x43; 12]);
            push_len_field(&mut inner, 4, &[0x44; 48]);
            push_len_field(&mut inner, 5, &vec![0x45; 18_966]);

            let mut envelope = Vec::new();
            push_len_field(&mut envelope, 2, &inner);
            push_varint_field(&mut envelope, 3, 1);
            BASE64.encode(envelope)
        }

        let upstream = fixture(16);
        assert!(is_plausible_upstream_bedrock_signature(&upstream));
        assert_eq!(
            validate_signature(&upstream).unwrap_err().failure,
            SignatureValidationFailure::InvalidLength
        );

        // Profile 15 is reserved for locally generated compatibility
        // signatures and must still require a valid local HMAC.
        assert!(!is_plausible_upstream_bedrock_signature(&fixture(15)));
        let local = generate_aws_b40_signature_for_model("claude-opus-4-8");
        assert!(!is_plausible_upstream_bedrock_signature(&local));
        assert!(!is_plausible_upstream_bedrock_signature("not-base64!!"));
    }

    #[test]
    fn validation_diagnostics_never_include_signature_material() {
        let invalid_base64 = validate_signature("not-base64!!").unwrap_err();
        assert_eq!(
            invalid_base64.failure,
            SignatureValidationFailure::InvalidBase64
        );
        assert_eq!(invalid_base64.encoded_len, 12);
        assert_eq!(invalid_base64.decoded_len, None);

        let invalid_length = validate_signature(&BASE64.encode([0u8; 8])).unwrap_err();
        assert_eq!(
            invalid_length.failure,
            SignatureValidationFailure::InvalidLength
        );
        assert_eq!(invalid_length.decoded_len, Some(8));
        assert!(!invalid_length.ends_with_field3);
        assert!(!invalid_length.has_bedrock_profile_markers);

        let mut tampered = BASE64
            .decode(generate_aws_b40_signature_for_model("claude-opus-4-8"))
            .unwrap();
        tampered[40] ^= 0x01;
        let hmac_mismatch = validate_signature(&BASE64.encode(tampered)).unwrap_err();
        assert_eq!(
            hmac_mismatch.failure,
            SignatureValidationFailure::HmacMismatch
        );
        assert_eq!(hmac_mismatch.decoded_len, Some(241));
        assert!(hmac_mismatch.ends_with_field3);
        assert!(hmac_mismatch.has_bedrock_profile_markers);
    }

    #[test]
    fn issued_signature_registry_detects_local_modification() {
        let issued =
            generate_aws_b40_adaptive_signature_for_model("claude-opus-4-8", 256, 34_749, 0);
        register_issued_opaque_signature(&issued);
        assert_eq!(
            issued_opaque_signature_match(&issued),
            IssuedSignatureMatch::Exact
        );

        let mut modified = BASE64.decode(&issued).unwrap();
        let midpoint = modified.len() / 2;
        modified[midpoint] ^= 0x01;
        assert_eq!(
            issued_opaque_signature_match(&BASE64.encode(modified)),
            IssuedSignatureMatch::LocallyModified
        );
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
    fn aws_b40_opus_48_adaptive_signature_tracks_current_bedrock_shape() {
        for _ in 0..20 {
            let low = generate_aws_b40_adaptive_signature_for_model("claude-opus-4-8", 303, 49, 0);
            let low_raw = BASE64.decode(&low).expect("low-context signature decodes");
            assert!((241..=443).contains(&low_raw.len()), "{}", low_raw.len());
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
                (2_740..=3_240).contains(&cache_create_raw.len()),
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
                &[0x0a, 0x71, 0x08, 0x0f]
            );
            assert!(
                cache_create_raw
                    .windows(12)
                    .any(|window| { window == AWS_B_OPUS_48_CONTEXT_STAMP.as_slice() })
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
                (2_740..=3_240).contains(&cache_read_raw.len()),
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
            assert!(
                cache_read_raw
                    .windows(12)
                    .any(|window| { window == AWS_B_OPUS_48_CONTEXT_STAMP.as_slice() })
            );
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
