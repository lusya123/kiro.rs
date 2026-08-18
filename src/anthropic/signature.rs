//! Thinking 块签名：无状态自校验 HMAC 方案
//!
//! Anthropic 官方 thinking 块带 `signature`，客户端多轮续聊时把它原样回传，由 Anthropic
//! 服务端验签。kiro-rs 链路（customer → sub2api → kiro-rs → Kiro 上游）里：
//! - 新版 Kiro 模型会返回原生 reasoning signature，本服务原样透传。
//! - AWS-B 只透传上游原生签名；缺失时不生成兼容外形。
//! - 其他旧兼容 profile 仍可使用本地 HMAC；converter 转发历史前会移除 signature。
//! - 真 Anthropic 永远看不到这些签名，客户端 SDK 自己也不验签。
//!
//! 本模块同时维护 AWS-B 原生签名的精确回传登记。服务端无法离线验证上游的私有签名算法，
//! 因此正常路径只接受本服务实际向客户端透传过的 `AWS-B 渠道 + signature` 组合。登记键只保存
//! SHA-256 摘要，并通过集群缓存跨容器共享；不会保存或记录原始思考与签名。公开 thinking
//! 摘要不参与绑定，这与 Bedrock 对空 thinking 文本的实际回放语义一致。
//!
//! 从旧版本升级到严格登记版时，升级前签发的原生 AWS 签名没有登记记录。生产可通过一个带绝对
//! 截止时间的迁移窗口导入结构完整、内部模型属于 AWS-B 渠道的 Bedrock 签名；需要长期兼容跨网关、
//! 跨升级会话时，也可显式设置 `KIRO_NATIVE_SIGNATURE_ACCEPT_PROVIDER_ENVELOPE=true`。两种模式仍拒绝空值、
//! 畸形值和渠道外内部模型，并用分块指纹识别本服务已签发值的常见单点篡改；导入后立即转入精确
//! 登记。两种导入方式默认都关闭，避免普通部署无意开启未知签名兼容路径。
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
use futures::future::join_all;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex as AsyncMutex;

/// HMAC-SHA256 输出长度。
const MAC_LEN: usize = 32;

/// 原生 signature 允许回传的登记期限。官方 signature 对客户端是不透明的；这里的期限只用于
/// 限制本地/Redis 登记表增长，不参与 prompt-cache TTL，也不会改变任何 token 计算。
const DEFAULT_NATIVE_SIGNATURE_REGISTRY_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const NATIVE_SIGNATURE_KEY_PREFIX: &str = "awsb:native-thinking-signature:v1:";
const NATIVE_SIGNATURE_V2_KEY_PREFIX: &str = "awsb:native-thinking-signature:v2:";
const NATIVE_SIGNATURE_V3_KEY_PREFIX: &str = "awsb:native-thinking-signature:v3:";
const NATIVE_SIGNATURE_V2_FINGERPRINT_KEY_PREFIX: &str =
    "awsb:native-thinking-signature:fingerprint:v2:";
const NATIVE_SIGNATURE_V3_FINGERPRINT_KEY_PREFIX: &str =
    "awsb:native-thinking-signature:fingerprint:v3:";
const NATIVE_SIGNATURE_CHANNEL: &[u8] = b"aws-b/bedrock";
const NATIVE_SIGNATURE_FINGERPRINT_PARTS: usize = 4;
const NATIVE_SIGNATURE_V2_NEAR_MATCH_THRESHOLD: usize = 2;
const NATIVE_SIGNATURE_V3_NEAR_MATCH_THRESHOLD: usize = 3;
const MAX_NATIVE_SIGNATURE_RAW_BYTES: usize = 2 * 1024 * 1024;
const DURABLE_REDIS_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const DURABLE_REDIS_OPERATION_TIMEOUT: Duration = Duration::from_millis(80);
const DURABLE_REDIS_RETRY_DELAY_MS: u64 = 3_000;

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

fn native_signature_registry_ttl() -> Duration {
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| {
        let seconds = std::env::var("KIRO_NATIVE_SIGNATURE_TTL_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| *seconds >= 60)
            .unwrap_or(DEFAULT_NATIVE_SIGNATURE_REGISTRY_TTL_SECS);
        Duration::from_secs(seconds)
    })
}

struct DurableNativeSignatureStore {
    client: redis::Client,
    connection: AsyncMutex<Option<redis::aio::MultiplexedConnection>>,
    retry_after_ms: AtomicU64,
    failure_logged: AtomicBool,
}

impl DurableNativeSignatureStore {
    async fn connection(&self) -> Option<redis::aio::MultiplexedConnection> {
        if unix_time_ms() < self.retry_after_ms.load(Ordering::Relaxed) {
            return None;
        }
        let mut connection = self.connection.lock().await;
        if let Some(existing) = connection.as_ref() {
            return Some(existing.clone());
        }
        let result = tokio::time::timeout(
            DURABLE_REDIS_CONNECT_TIMEOUT,
            self.client.get_multiplexed_async_connection(),
        )
        .await;
        match result {
            Ok(Ok(connected)) => {
                self.failure_logged.store(false, Ordering::Relaxed);
                *connection = Some(connected.clone());
                Some(connected)
            }
            _ => {
                self.note_failure("connect");
                None
            }
        }
    }

    fn note_failure(&self, operation: &'static str) {
        self.retry_after_ms.store(
            unix_time_ms().saturating_add(DURABLE_REDIS_RETRY_DELAY_MS),
            Ordering::Relaxed,
        );
        if !self.failure_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                operation,
                "durable native-signature registry is unavailable; retaining cluster-cache fallback"
            );
        }
    }

    async fn invalidate_connection(&self, operation: &'static str) {
        *self.connection.lock().await = None;
        self.note_failure(operation);
    }

    async fn exists(&self, key: &str) -> Option<bool> {
        let mut connection = self.connection().await?;
        let mut command = redis::cmd("EXISTS");
        command.arg(key);
        match tokio::time::timeout(
            DURABLE_REDIS_OPERATION_TIMEOUT,
            command.query_async::<i64>(&mut connection),
        )
        .await
        {
            Ok(Ok(count)) => Some(count > 0),
            _ => {
                self.invalidate_connection("exists").await;
                None
            }
        }
    }

    async fn register(&self, key: &str, ttl: Duration) -> Option<()> {
        let mut connection = self.connection().await?;
        let mut command = redis::cmd("SET");
        command.arg(key).arg(1).arg("EX").arg(ttl.as_secs().max(1));
        match tokio::time::timeout(
            DURABLE_REDIS_OPERATION_TIMEOUT,
            command.query_async::<String>(&mut connection),
        )
        .await
        {
            Ok(Ok(_)) => Some(()),
            _ => {
                self.invalidate_connection("register").await;
                None
            }
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn durable_native_signature_store() -> Option<&'static DurableNativeSignatureStore> {
    static STORE: OnceLock<Option<DurableNativeSignatureStore>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let url = std::env::var("KIRO_NATIVE_SIGNATURE_REDIS_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())?;
            match redis::Client::open(url) {
                Ok(client) => {
                    tracing::info!("durable native-signature registry is enabled");
                    Some(DurableNativeSignatureStore {
                        client,
                        connection: AsyncMutex::new(None),
                        retry_after_ms: AtomicU64::new(0),
                        failure_logged: AtomicBool::new(false),
                    })
                }
                Err(_) => {
                    tracing::warn!(
                        "invalid durable native-signature Redis configuration; retaining cluster-cache fallback"
                    );
                    None
                }
            }
        })
        .as_ref()
}

async fn native_registry_exists(key: &str) -> bool {
    if let Some(store) = durable_native_signature_store()
        && store.exists(key).await == Some(true)
    {
        return true;
    }
    crate::cluster_cache::global().exists(key).await
}

async fn register_native_registry_keys(keys: &[String]) {
    let ttl = native_signature_registry_ttl();
    let cluster = crate::cluster_cache::global();
    let durable = durable_native_signature_store();
    join_all(keys.iter().map(|key| async move {
        cluster.register(key, ttl).await;
        if let Some(store) = durable {
            let _ = store.register(key, ttl).await;
        }
    }))
    .await;
}

fn update_len_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn digest_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn canonical_native_model(model: &str) -> String {
    super::converter::map_model(model).unwrap_or_else(|| model.trim().to_ascii_lowercase())
}

/// Build a privacy-preserving registry key bound to exactly what the client saw.
/// Length-prefixing prevents ambiguous concatenations such as `(ab, c)` vs `(a, bc)`.
fn native_signature_registry_key(model: &str, thinking: &str, signature: &str) -> String {
    let canonical_model = canonical_native_model(model);
    let mut hasher = Sha256::new();
    hasher.update(b"kiro-rs/aws-b/native-thinking-signature/v1\0");
    update_len_prefixed(&mut hasher, canonical_model.as_bytes());
    update_len_prefixed(&mut hasher, thinking.as_bytes());
    update_len_prefixed(&mut hasher, signature.as_bytes());
    format!("{NATIVE_SIGNATURE_KEY_PREFIX}{}", digest_hex(hasher))
}

/// V2 follows the provider behavior: the opaque signature is model-bound, while the public
/// thinking summary is not part of the cryptographic identity.
fn native_signature_v2_registry_key(model: &str, signature: &str) -> String {
    let canonical_model = canonical_native_model(model);
    let mut hasher = Sha256::new();
    hasher.update(b"kiro-rs/aws-b/native-thinking-signature/v2\0");
    update_len_prefixed(&mut hasher, canonical_model.as_bytes());
    update_len_prefixed(&mut hasher, signature.as_bytes());
    format!("{NATIVE_SIGNATURE_V2_KEY_PREFIX}{}", digest_hex(hasher))
}

/// V3 binds provider signatures to the AWS-B channel. A conversation may change Claude models
/// while remaining on this channel, so the requested model is intentionally absent.
fn native_signature_v3_registry_key(signature: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kiro-rs/aws-b/native-thinking-signature/v3\0");
    update_len_prefixed(&mut hasher, NATIVE_SIGNATURE_CHANNEL);
    update_len_prefixed(&mut hasher, signature.as_bytes());
    format!("{NATIVE_SIGNATURE_V3_KEY_PREFIX}{}", digest_hex(hasher))
}

fn native_signature_v2_fingerprint_keys(model: &str, signature: &str) -> Vec<String> {
    let Ok(raw) = BASE64.decode(signature) else {
        return Vec::new();
    };
    if raw.len() < 32 || raw.len() > MAX_NATIVE_SIGNATURE_RAW_BYTES {
        return Vec::new();
    }
    let canonical_model = canonical_native_model(model);
    (0..NATIVE_SIGNATURE_FINGERPRINT_PARTS)
        .map(|part| {
            let start = part * raw.len() / NATIVE_SIGNATURE_FINGERPRINT_PARTS;
            let end = (part + 1) * raw.len() / NATIVE_SIGNATURE_FINGERPRINT_PARTS;
            let mut hasher = Sha256::new();
            hasher.update(b"kiro-rs/aws-b/native-thinking-signature/fingerprint/v2\0");
            update_len_prefixed(&mut hasher, canonical_model.as_bytes());
            hasher.update((raw.len() as u64).to_be_bytes());
            hasher.update((part as u64).to_be_bytes());
            update_len_prefixed(&mut hasher, &raw[start..end]);
            format!(
                "{NATIVE_SIGNATURE_V2_FINGERPRINT_KEY_PREFIX}{}",
                digest_hex(hasher)
            )
        })
        .collect()
}

fn native_signature_v3_fingerprint_keys(signature: &str) -> Vec<String> {
    let Ok(raw) = BASE64.decode(signature) else {
        return Vec::new();
    };
    if raw.len() < 32 || raw.len() > MAX_NATIVE_SIGNATURE_RAW_BYTES {
        return Vec::new();
    }
    let encoded = signature.as_bytes();
    (0..NATIVE_SIGNATURE_FINGERPRINT_PARTS)
        .map(|part| {
            let start = part * encoded.len() / NATIVE_SIGNATURE_FINGERPRINT_PARTS;
            let end = (part + 1) * encoded.len() / NATIVE_SIGNATURE_FINGERPRINT_PARTS;
            let mut hasher = Sha256::new();
            hasher.update(b"kiro-rs/aws-b/native-thinking-signature/fingerprint/v3\0");
            update_len_prefixed(&mut hasher, NATIVE_SIGNATURE_CHANNEL);
            hasher.update((encoded.len() as u64).to_be_bytes());
            hasher.update((part as u64).to_be_bytes());
            update_len_prefixed(&mut hasher, &encoded[start..end]);
            format!(
                "{NATIVE_SIGNATURE_V3_FINGERPRINT_KEY_PREFIX}{}",
                digest_hex(hasher)
            )
        })
        .collect()
}

/// Register an opaque native signature before it is exposed to the client.
/// Empty values are never registered, so an empty round-trip cannot become valid accidentally.
pub async fn register_native_signature(model: &str, thinking: &str, signature: &str) {
    if signature.is_empty() {
        return;
    }
    let mut keys = vec![
        // Keep writing V1/V2 throughout a rolling upgrade so old containers can validate responses
        // issued by new containers before the whole fleet has switched to V3.
        native_signature_registry_key(model, thinking, signature),
        native_signature_v2_registry_key(model, signature),
        native_signature_v3_registry_key(signature),
    ];
    keys.extend(native_signature_v2_fingerprint_keys(model, signature));
    keys.extend(native_signature_v3_fingerprint_keys(signature));
    register_native_registry_keys(&keys).await;
}

/// Validate a native signature by exact, channel-bound round trip.
///
/// This deliberately does not attempt to reverse engineer or imitate the AWS/Anthropic private
/// cryptographic format. Unknown imported signatures fail closed on the Kiro conversion path;
/// the optional native Bedrock route remains responsible for official provider-side validation.
pub async fn validate_native_signature(model: &str, thinking: &str, signature: &str) -> bool {
    if signature.is_empty() {
        return false;
    }
    let v3_key = native_signature_v3_registry_key(signature);
    if native_registry_exists(&v3_key).await {
        register_native_signature(model, thinking, signature).await;
        return true;
    }

    // V1/V2 remain readable during rolling upgrades. A same-model hit promotes the signature into
    // the channel-wide V3 namespace. Provider envelopes also reveal the issuing model, allowing a
    // cross-model replay to recover an old V2 registration and promote it without accepting an
    // unknown value.
    let v2_key = native_signature_v2_registry_key(model, signature);
    let v1_key = native_signature_registry_key(model, thinking, signature);
    if native_registry_exists(&v2_key).await || native_registry_exists(&v1_key).await {
        register_native_signature(model, thinking, signature).await;
        return true;
    }
    if let Some(source_model) = native_signature_internal_model(signature)
        .as_deref()
        .and_then(canonical_model_for_internal_model)
        && native_registry_exists(&native_signature_v2_registry_key(source_model, signature)).await
    {
        register_native_signature(model, thinking, signature).await;
        return true;
    }
    // Old AWS-B releases could emit a local self-verifying HMAC fallback. It remains safe to
    // accept those across upgrades because tampering is checked cryptographically and statelessly.
    if validate_signature(signature).is_ok() {
        register_native_signature(model, thinking, signature).await;
        return true;
    }
    false
}

pub fn native_signature_import_allowed() -> bool {
    static IMPORT_POLICY: OnceLock<(bool, Option<u64>)> = OnceLock::new();
    let (accept_provider_envelope, import_until) = *IMPORT_POLICY.get_or_init(|| {
        let accept_provider_envelope = std::env::var(
            "KIRO_NATIVE_SIGNATURE_ACCEPT_PROVIDER_ENVELOPE",
        )
        .ok()
        .is_some_and(|value| explicit_truthy(&value));
        let import_until = std::env::var("KIRO_NATIVE_SIGNATURE_IMPORT_UNTIL_EPOCH")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok());
        if accept_provider_envelope {
            tracing::warn!(
                "provider-envelope native-signature compatibility is enabled for previously unseen Bedrock signatures"
            );
        }
        (accept_provider_envelope, import_until)
    });
    if accept_provider_envelope {
        return true;
    }
    let Some(until) = import_until else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(u64::MAX);
    now < until
}

fn explicit_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSignatureImportResult {
    Imported,
    RecoveredExactFingerprint,
    RegisteredNearMatch,
    InvalidStructure,
}

/// Import one pre-registry Bedrock signature during a bounded production migration.
///
/// This is intentionally separate from normal exact validation. A signature that shares at least
/// three of four channel-and-length-bound chunks with one already issued by this fleet is a typical
/// single-point modification and remains rejected. Four matches recover an exact value if an
/// interrupted Redis write left only its fingerprints. A completely unknown value must pass the
/// observed Bedrock protobuf envelope and channel-membership checks before being registered.
pub async fn import_native_signature_during_migration(
    model: &str,
    thinking: &str,
    signature: &str,
) -> NativeSignatureImportResult {
    if !is_plausible_bedrock_native_signature(signature) {
        return NativeSignatureImportResult::InvalidStructure;
    }
    let mut fingerprint_groups = vec![native_signature_v3_fingerprint_keys(signature)];
    fingerprint_groups.extend(
        AWS_B_NATIVE_MODELS
            .iter()
            .map(|(canonical, _)| native_signature_v2_fingerprint_keys(canonical, signature)),
    );
    let match_counts = join_all(fingerprint_groups.iter().map(|keys| async move {
        join_all(keys.iter().map(|key| native_registry_exists(key)))
            .await
            .into_iter()
            .filter(|matched| *matched)
            .count()
    }))
    .await;
    if match_counts.contains(&NATIVE_SIGNATURE_FINGERPRINT_PARTS) {
        register_native_signature(model, thinking, signature).await;
        return NativeSignatureImportResult::RecoveredExactFingerprint;
    }
    let channel_near_match = match_counts
        .first()
        .is_some_and(|matches| *matches >= NATIVE_SIGNATURE_V3_NEAR_MATCH_THRESHOLD);
    // A single Base64 character can change two decoded bytes. If that happens across an old V2
    // raw-byte partition boundary, only two of four legacy fingerprints remain. V3 partitions the
    // encoded characters and therefore retains the stricter three-of-four threshold.
    let legacy_near_match = match_counts
        .iter()
        .skip(1)
        .any(|matches| *matches >= NATIVE_SIGNATURE_V2_NEAR_MATCH_THRESHOLD);
    if channel_near_match || legacy_near_match {
        return NativeSignatureImportResult::RegisteredNearMatch;
    }
    register_native_signature(model, thinking, signature).await;
    NativeSignatureImportResult::Imported
}

#[derive(Debug, Clone, Copy)]
enum ProtobufValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn read_protobuf_varint(input: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let byte = *input.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn parse_protobuf_fields(input: &[u8]) -> Option<Vec<(u64, ProtobufValue<'_>)>> {
    let mut cursor = 0usize;
    let mut fields = Vec::new();
    while cursor < input.len() {
        let tag = read_protobuf_varint(input, &mut cursor)?;
        let field = tag >> 3;
        if field == 0 {
            return None;
        }
        let value = match tag & 0x07 {
            0 => ProtobufValue::Varint(read_protobuf_varint(input, &mut cursor)?),
            1 => {
                cursor = cursor.checked_add(8)?;
                if cursor > input.len() {
                    return None;
                }
                ProtobufValue::Fixed
            }
            2 => {
                let len = usize::try_from(read_protobuf_varint(input, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let bytes = input.get(cursor..end)?;
                cursor = end;
                ProtobufValue::Bytes(bytes)
            }
            5 => {
                cursor = cursor.checked_add(4)?;
                if cursor > input.len() {
                    return None;
                }
                ProtobufValue::Fixed
            }
            _ => return None,
        };
        fields.push((field, value));
        if fields.len() > 64 {
            return None;
        }
    }
    Some(fields)
}

const AWS_B_NATIVE_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4.8", "claude-quince"),
    ("claude-opus-5", "claude-honey"),
    ("claude-sonnet-5", "claude-saffron"),
    ("claude-opus-4.7", "claude-opus-4-7"),
    ("claude-opus-4.6", "claude-opus-4-6"),
    ("claude-sonnet-4.6", "claude-sonnet-4-6"),
    ("claude-sonnet-4.5", "claude-sonnet-4-5"),
    ("claude-haiku-4.5", "claude-haiku-4-5"),
];

fn provider_internal_model(model: &str) -> Option<&'static str> {
    let canonical = canonical_native_model(model);
    AWS_B_NATIVE_MODELS
        .iter()
        .find_map(|(candidate, internal)| (*candidate == canonical).then_some(*internal))
}

fn canonical_model_for_internal_model(internal: &str) -> Option<&'static str> {
    AWS_B_NATIVE_MODELS
        .iter()
        .find_map(|(canonical, candidate)| (*candidate == internal).then_some(*canonical))
}

fn native_signature_internal_model(signature: &str) -> Option<String> {
    let Ok(raw) = BASE64.decode(signature) else {
        return None;
    };
    if raw.len() < 128
        || raw.len() > MAX_NATIVE_SIGNATURE_RAW_BYTES
        || raw.first() != Some(&0x12)
        || !raw.ends_with(&[0x18, 0x01])
    {
        return None;
    }
    let top = parse_protobuf_fields(&raw)?;
    let mut inner = None;
    let mut terminal = false;
    for (field, value) in top {
        match (field, value) {
            (2, ProtobufValue::Bytes(bytes)) if inner.is_none() => inner = Some(bytes),
            (3, ProtobufValue::Varint(1)) => terminal = true,
            _ => {}
        }
    }
    let (Some(inner), true) = (inner, terminal) else {
        return None;
    };
    let inner_fields = parse_protobuf_fields(inner)?;
    let mut header = None;
    let mut required_payload_fields = [false; 4];
    for (field, value) in inner_fields {
        if let ProtobufValue::Bytes(bytes) = value {
            match field {
                1 if header.is_none() => header = Some(bytes),
                2..=5 => required_payload_fields[(field - 2) as usize] = !bytes.is_empty(),
                _ => {}
            }
        }
    }
    if required_payload_fields.iter().any(|present| !present) {
        return None;
    }
    let header_fields = header.and_then(parse_protobuf_fields)?;
    let mut internal_model = None;
    let mut thinking_marker = false;
    for (field, value) in header_fields {
        if let ProtobufValue::Bytes(bytes) = value {
            match field {
                6 => internal_model = std::str::from_utf8(bytes).ok(),
                8 => thinking_marker = bytes == b"thinking",
                _ => {}
            }
        }
    }
    thinking_marker.then_some(internal_model?.to_string())
}

fn is_plausible_bedrock_native_signature(signature: &str) -> bool {
    native_signature_internal_model(signature)
        .as_deref()
        .and_then(canonical_model_for_internal_model)
        .is_some()
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

/// 追加一个 length-delimited 字段:`tag(field<<3|2)` + `varint(len)` + `content`。
fn push_len_field(buf: &mut Vec<u8>, field: u8, content: &[u8]) {
    buf.push((field << 3) | 2);
    push_varint(buf, content.len());
    buf.extend_from_slice(content);
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

/// Generate a model-bound provider-envelope compatibility signature.
///
/// Some older Kiro Opus runtimes honor the requested reasoning effort but do
/// not return their native signature event. This fallback preserves the
/// externally decodable Bedrock model marker while authenticating the entire
/// envelope with this gateway's shared HMAC. It is never used when an upstream
/// native signature is available.
pub fn generate_model_signature(model: &str) -> Option<String> {
    let internal_model = provider_internal_model(model)?;
    let mut header = vec![0x08, 0x0f, 0x10, 0x01, 0x18, 0x02];
    push_len_field(&mut header, 5, &rand_bytes(64));
    push_len_field(&mut header, 6, internal_model.as_bytes());
    header.extend_from_slice(&[0x38, 0x00]);
    push_len_field(&mut header, 8, b"thinking");

    let mut inner = Vec::new();
    push_len_field(&mut inner, 1, &header);
    push_len_field(&mut inner, 2, &rand_bytes(12));
    push_len_field(&mut inner, 3, &rand_bytes(12));
    push_len_field(&mut inner, 4, &rand_bytes(48));
    push_len_field(&mut inner, 5, &rand_bytes(175 + fastrand::usize(..=150)));

    let mut raw = Vec::new();
    push_len_field(&mut raw, 2, &inner);
    raw.extend_from_slice(&[0x18, 0x01]);
    let mac_start = raw.len().checked_sub(2 + MAC_LEN)?;
    let mac = hmac_sha256(signing_secret(), &raw[..mac_start]);
    raw[mac_start..mac_start + MAC_LEN].copy_from_slice(&mac);
    Some(BASE64.encode(raw))
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
pub fn native_bedrock_signature_for_test(internal_model: &str) -> String {
    let mut header = vec![0x08, 0x0f, 0x10, 0x01, 0x18, 0x02];
    push_len_field(&mut header, 5, &rand_bytes(64));
    push_len_field(&mut header, 6, internal_model.as_bytes());
    header.extend_from_slice(&[0x38, 0x00]);
    push_len_field(&mut header, 8, b"thinking");

    let mut inner = Vec::new();
    push_len_field(&mut inner, 1, &header);
    push_len_field(&mut inner, 2, &rand_bytes(12));
    push_len_field(&mut inner, 3, &rand_bytes(12));
    push_len_field(&mut inner, 4, &rand_bytes(48));
    push_len_field(&mut inner, 5, &rand_bytes(256));

    let mut raw = Vec::new();
    push_len_field(&mut raw, 2, &inner);
    raw.extend_from_slice(&[0x18, 0x01]);
    BASE64.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_envelope_import_requires_an_explicit_truthy_value() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(explicit_truthy(value), "{value:?} should enable the policy");
        }
        for value in ["", "0", "false", "no", "off", "enabled"] {
            assert!(!explicit_truthy(value), "{value:?} must remain disabled");
        }
    }

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
    fn model_bound_fallback_signatures_are_decodable_and_tamper_evident() {
        for model in ["claude-opus-4-6", "claude-opus-4-7"] {
            let signature = generate_model_signature(model).expect("supported model");
            assert!(validate_signature(&signature).is_ok(), "model={model}");
            assert!(
                is_plausible_bedrock_native_signature(&signature),
                "model={model}"
            );

            let expected_internal = provider_internal_model(model).unwrap().as_bytes();
            let mut raw = BASE64.decode(&signature).unwrap();
            assert!(
                raw.windows(expected_internal.len())
                    .any(|window| window == expected_internal),
                "model={model}"
            );
            raw[16] ^= 1;
            assert!(
                validate_signature(&BASE64.encode(raw)).is_err(),
                "model={model}"
            );
        }
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

        let mut tampered = BASE64.decode(generate_signature()).unwrap();
        let decoded_len = tampered.len();
        tampered[40] ^= 0x01;
        let hmac_mismatch = validate_signature(&BASE64.encode(tampered)).unwrap_err();
        assert_eq!(
            hmac_mismatch.failure,
            SignatureValidationFailure::HmacMismatch
        );
        assert_eq!(hmac_mismatch.decoded_len, Some(decoded_len));
        assert!(hmac_mismatch.ends_with_field3);
        assert!(!hmac_mismatch.has_bedrock_profile_markers);
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
    fn native_registry_key_binds_model_thinking_and_signature_without_plaintext() {
        let key = native_signature_registry_key(
            "claude-opus-4-8",
            "private reasoning text",
            "opaque-signature-material",
        );
        assert!(key.starts_with(NATIVE_SIGNATURE_KEY_PREFIX));
        assert!(!key.contains("private reasoning text"));
        assert!(!key.contains("opaque-signature-material"));
        assert_eq!(
            key,
            native_signature_registry_key(
                "anthropic.claude-opus-4-8",
                "private reasoning text",
                "opaque-signature-material",
            ),
            "public aliases of the same upstream model must share a binding"
        );
        assert_ne!(
            key,
            native_signature_registry_key(
                "claude-opus-4-8",
                "modified reasoning text",
                "opaque-signature-material",
            )
        );
        assert_ne!(
            key,
            native_signature_registry_key(
                "claude-opus-4-8",
                "private reasoning text",
                "modified-signature-material",
            )
        );

        let v2 = native_signature_v2_registry_key("claude-opus-4-8", "opaque-signature-material");
        assert_eq!(
            v2,
            native_signature_v2_registry_key(
                "anthropic.claude-opus-4-8",
                "opaque-signature-material",
            )
        );
        assert_ne!(
            v2,
            native_signature_v2_registry_key("claude-opus-5", "opaque-signature-material")
        );

        let v3 = native_signature_v3_registry_key("opaque-signature-material");
        assert_eq!(
            v3,
            native_signature_v3_registry_key("opaque-signature-material")
        );
        assert_ne!(
            v3,
            native_signature_v3_registry_key("modified-signature-material")
        );
    }

    #[tokio::test]
    async fn native_registry_binds_channel_and_signature_but_not_model_or_public_thinking() {
        let signature = format!("native-opaque-{}", fastrand::u64(..));
        assert!(
            !validate_native_signature("claude-opus-4-8", "visible thinking", &signature).await
        );
        register_native_signature("claude-opus-4-8", "visible thinking", &signature).await;
        assert!(
            validate_native_signature("anthropic.claude-opus-4-8", "visible thinking", &signature,)
                .await
        );
        assert!(validate_native_signature("claude-opus-4-8", "changed thinking", &signature).await);
        for replay_model in ["claude-opus-5", "claude-sonnet-5"] {
            assert!(
                validate_native_signature(replay_model, "visible thinking", &signature).await,
                "a signature issued by the same AWS-B channel must replay on {replay_model}"
            );
        }
        assert!(
            !validate_native_signature(
                "claude-opus-4-8",
                "visible thinking",
                &format!("{signature}changed"),
            )
            .await
        );
        assert!(!validate_native_signature("claude-opus-4-8", "visible thinking", "").await);
    }

    #[tokio::test]
    async fn legacy_model_registration_promotes_on_first_cross_model_channel_replay() {
        let signature = native_bedrock_signature_for_test("claude-quince");
        let legacy_v2 = native_signature_v2_registry_key("claude-opus-4-8", &signature);
        register_native_registry_keys(&[legacy_v2]).await;

        assert!(
            validate_native_signature("claude-sonnet-5", "", &signature).await,
            "the provider envelope must recover its issuing model's V2 key and promote to V3"
        );
        assert!(
            native_registry_exists(&native_signature_v3_registry_key(&signature)).await,
            "a recovered rolling-upgrade signature must be promoted into the channel namespace"
        );
    }

    #[tokio::test]
    async fn migration_rejects_every_single_base64_character_mutation() {
        let signature = native_bedrock_signature_for_test("claude-quince");
        register_native_signature("claude-opus-4-8", "", &signature).await;

        for index in 0..signature.len() {
            if signature.as_bytes()[index] == b'=' {
                continue;
            }
            let mut mutated = signature.as_bytes().to_vec();
            mutated[index] = if mutated[index] == b'A' { b'B' } else { b'A' };
            let mutated = String::from_utf8(mutated).expect("base64 stays ASCII");
            let result =
                import_native_signature_during_migration("claude-sonnet-5", "", &mutated).await;
            assert!(
                matches!(
                    result,
                    NativeSignatureImportResult::RegisteredNearMatch
                        | NativeSignatureImportResult::InvalidStructure
                ),
                "single-character mutation at encoded index {index} was accepted as {result:?}"
            );
        }
    }
}
