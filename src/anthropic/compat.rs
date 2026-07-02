//! Protocol-shape compatibility helpers for the public Anthropic-like API.
//!
//! These helpers only shape the local HTTP/API envelope. They do not make Kiro
//! upstream signatures or rate limits real Anthropic/AWS values.

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use base64::Engine;
use serde_json::{Value, json};

use super::types::{CountTokensRequest, Message, MessagesRequest, SystemMessage, Thinking, Tool};

const EXPOSE_HEADERS: &str =
    "date,request-id,retry-after,retry-after-ms,x-should-retry,anthropic-organization-id";
const ORG_ID: &str = "089a9559-f257-4a40-845e-2f716aaeb7f6";
const INPUT_LIMIT: &str = "500000";
const OUTPUT_LIMIT: &str = "80000";
const REQUEST_LIMIT: &str = "1000";
const TOKENS_LIMIT: &str = "580000";
const SONNET_TOOL_TOTAL_OVERHEAD_TOKENS: i32 = 501;
const SONNET_TOOL_PREFIX_OVERHEAD_TOKENS: i32 = 189;
const OPUS_TOOL_TOTAL_OVERHEAD_TOKENS: i32 = 304;
const OPUS_TOOL_PREFIX_OVERHEAD_TOKENS: i32 = 242;

pub fn request_id() -> String {
    // 真 Anthropic 的 request-id 恒以 `req_011C` 开头（版本化前缀），其后跟随
    // 20 位 base62。旧实现是 `req_01` + 随机，缺了恒定的 `1C`，会被指纹识别。
    format!("req_011C{}", base62(20))
}

pub fn aws_request_id() -> String {
    // 一方 aws_req_ 标识为 52 位小写 base62（旧实现 56 位，长度对不上）。
    format!("aws_req_{}", lower_base62(52))
}

pub fn oneapi_request_id() -> String {
    format!(
        "{}{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S"),
        base62(24)
    )
}

pub fn add_response_headers(
    headers: &mut HeaderMap,
    status: StatusCode,
    is_stream: bool,
    include_official_headers: bool,
) {
    set(
        headers,
        "x-new-api-version",
        "official01-rc25-redis-release-cleanup-20260627-203800-5e86644649",
    );
    set(headers, "x-oneapi-request-id", &oneapi_request_id());

    if is_stream || !status.is_success() || !include_official_headers {
        return;
    }

    let request_id = request_id();
    let aws_id = aws_request_id();
    let reset = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
    set(headers, "request-id", &request_id);
    set(headers, "anthropic-organization-id", ORG_ID);
    set(headers, "x-amzn-requestid", &aws_id);
    set(headers, "x-request-id", &aws_id);

    set(
        headers,
        "anthropic-ratelimit-input-tokens-limit",
        INPUT_LIMIT,
    );
    set(
        headers,
        "anthropic-ratelimit-input-tokens-remaining",
        INPUT_LIMIT,
    );
    set(headers, "anthropic-ratelimit-input-tokens-reset", &reset);
    set(
        headers,
        "anthropic-ratelimit-output-tokens-limit",
        OUTPUT_LIMIT,
    );
    set(
        headers,
        "anthropic-ratelimit-output-tokens-remaining",
        OUTPUT_LIMIT,
    );
    set(headers, "anthropic-ratelimit-output-tokens-reset", &reset);
    set(headers, "anthropic-ratelimit-requests-limit", REQUEST_LIMIT);
    set(headers, "anthropic-ratelimit-requests-remaining", "999");
    set(headers, "anthropic-ratelimit-requests-reset", &reset);
    set(headers, "anthropic-ratelimit-tokens-limit", TOKENS_LIMIT);
    set(
        headers,
        "anthropic-ratelimit-tokens-remaining",
        TOKENS_LIMIT,
    );
    set(headers, "anthropic-ratelimit-tokens-reset", &reset);
}

pub fn usage(
    model: &str,
    input_tokens: i32,
    output_tokens: i32,
    thinking_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_creation_1h_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Value {
    let cache_creation_5m_input_tokens =
        (cache_creation_input_tokens - cache_creation_1h_input_tokens).max(0);
    let mut usage = serde_json::Map::new();
    usage.insert("input_tokens".to_string(), json!(input_tokens));
    usage.insert(
        "cache_creation_input_tokens".to_string(),
        json!(cache_creation_input_tokens),
    );
    usage.insert(
        "cache_read_input_tokens".to_string(),
        json!(cache_read_input_tokens),
    );
    usage.insert(
        "cache_creation".to_string(),
        json!({
            "ephemeral_5m_input_tokens": cache_creation_5m_input_tokens,
            "ephemeral_1h_input_tokens": cache_creation_1h_input_tokens.max(0)
        }),
    );
    usage.insert("output_tokens".to_string(), json!(output_tokens));

    if should_include_thinking_details(model, thinking_tokens) {
        usage.insert(
            "output_tokens_details".to_string(),
            json!({
                "thinking_tokens": thinking_tokens.max(0)
            }),
        );
    }

    usage.insert("service_tier".to_string(), json!("standard"));
    usage.insert("inference_geo".to_string(), json!("global"));
    Value::Object(usage)
}

/// `message_start` 事件的 usage。与最终响应 `usage()` 一致，但**不含**
/// `output_tokens_details`——真 Anthropic 仅在流末的 message_delta 给出
/// thinking_tokens，message_start 阶段不带该字段。签名与 `usage()` 一致以便直接替换。
pub fn stream_start_usage(
    _model: &str,
    input_tokens: i32,
    output_tokens: i32,
    _thinking_tokens: i32,
    cache_creation_input_tokens: i32,
    cache_creation_1h_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Value {
    let cache_creation_5m_input_tokens =
        (cache_creation_input_tokens - cache_creation_1h_input_tokens).max(0);
    let mut usage = serde_json::Map::new();
    usage.insert("input_tokens".to_string(), json!(input_tokens));
    usage.insert(
        "cache_creation_input_tokens".to_string(),
        json!(cache_creation_input_tokens),
    );
    usage.insert(
        "cache_read_input_tokens".to_string(),
        json!(cache_read_input_tokens),
    );
    usage.insert(
        "cache_creation".to_string(),
        json!({
            "ephemeral_5m_input_tokens": cache_creation_5m_input_tokens,
            "ephemeral_1h_input_tokens": cache_creation_1h_input_tokens.max(0)
        }),
    );
    usage.insert("output_tokens".to_string(), json!(output_tokens));
    usage.insert("service_tier".to_string(), json!("standard"));
    usage.insert("inference_geo".to_string(), json!("global"));
    Value::Object(usage)
}

/// `message_delta` 事件的 usage。真 Anthropic 的 delta 是**精简版**：
/// 不含 `cache_creation{}` 嵌套对象、不含 service_tier/inference_geo，
/// 仅在 opus 等模型上带 `output_tokens_details`。
pub fn stream_delta_usage(
    model: &str,
    input_tokens: i32,
    output_tokens: i32,
    thinking_tokens: i32,
    cache_creation_input_tokens: i32,
    _cache_creation_1h_input_tokens: i32,
    cache_read_input_tokens: i32,
) -> Value {
    let mut usage = serde_json::Map::new();
    usage.insert("input_tokens".to_string(), json!(input_tokens));
    usage.insert(
        "cache_creation_input_tokens".to_string(),
        json!(cache_creation_input_tokens),
    );
    usage.insert(
        "cache_read_input_tokens".to_string(),
        json!(cache_read_input_tokens),
    );
    usage.insert("output_tokens".to_string(), json!(output_tokens));

    if should_include_thinking_details(model, thinking_tokens) {
        usage.insert(
            "output_tokens_details".to_string(),
            json!({
                "thinking_tokens": thinking_tokens.max(0)
            }),
        );
    }

    Value::Object(usage)
}

pub fn should_include_thinking_details(model: &str, thinking_tokens: i32) -> bool {
    thinking_tokens != 0 || model.to_ascii_lowercase().contains("opus")
}

pub fn is_opus_4_8(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("opus") && (lower.contains("4-8") || lower.contains("4.8"))
}

/// 该模型经 Kiro 是否**不产出** `<thinking>` 内容(opus 系列)。
/// 用于:客户请求了 thinking 但上游拿不到思考时,决定是否合成一个思考块(以保持与真
/// Anthropic 一致的"思考块+签名"结构)。sonnet 会正常产思考,返回 false 以免重复注入。
pub fn model_omits_thinking(model: &str) -> bool {
    model.to_ascii_lowercase().contains("opus")
}

/// 合成 thinking 内容(多样化通用池,随机选一条,避免"逐字不变"成为指纹)。
/// 仅在 `model_omits_thinking` 且客户请求了 thinking 时使用;不改变真实答案文本。
pub fn synthetic_thinking() -> String {
    const POOL: &[&str] = &[
        "Let me work through this carefully before answering. I'll consider the key details and aim for a clear, accurate, and helpful response.",
        "I want to make sure I get this right. Let me think about what's being asked, weigh the relevant considerations, and then give a well-structured answer.",
        "Let me reason about this step by step. I'll identify the core of the question, check the important points, and respond precisely and helpfully.",
        "Before answering, let me think it through: what exactly is needed here, which details matter, and how to explain it clearly and accurately.",
        "Let me take a moment to consider this properly. I'll break the question down, decide on the best approach, and provide a helpful, correct answer.",
        "Thinking this through: I'll focus on what the user actually needs, account for the relevant nuances, and give a clear and accurate response.",
    ];
    POOL[fastrand::usize(..POOL.len())].to_string()
}

/// 文本 token 计数:统一走 `claude_tok::count_claude`(逆向 Claude 词表 + CJK 校准),
/// 输入/输出/缓存口径一致。
/// 每条消息的框架开销(role 包裹等)。
const MESSAGE_FRAMING_TOKENS: i32 = 4;
/// 请求基础开销。
const REQUEST_BASE_TOKENS: i32 = 3;

fn calibrated_text(text: &str) -> i32 {
    super::claude_tok::count_claude(text)
}

pub fn estimate_input_tokens(payload: &MessagesRequest) -> i32 {
    estimate_input_tokens_parts(
        &payload.model,
        payload.system.as_ref(),
        &payload.messages,
        payload.tools.as_ref(),
        payload.thinking.as_ref(),
    )
}

pub fn estimate_count_tokens_request(payload: &CountTokensRequest) -> i32 {
    estimate_input_tokens_parts(
        &payload.model,
        payload.system.as_ref(),
        &payload.messages,
        payload.tools.as_ref(),
        payload.thinking.as_ref(),
    )
}

pub fn estimate_prefix_tokens(
    model: &str,
    system_segments: &[String],
    content_segments: &[serde_json::Value],
    tools: &[Tool],
) -> i32 {
    let mut features = TokenFeatures::default();

    if !system_segments.is_empty() {
        features.has_system = true;
        for segment in system_segments {
            features.add_text(segment);
        }
        features.add_newline();
    }

    for content in content_segments {
        add_message_content_features(model, content, &mut features);
    }

    for tool in tools {
        features.add_text(&tool.name);
        features.add_text(&tool.description);
        features.add_text(&serde_json::to_string(&tool.input_schema).unwrap_or_default());
    }

    let is_opus = model.to_ascii_lowercase().contains("opus");
    let tool_overhead = if is_opus {
        OPUS_TOOL_PREFIX_OVERHEAD_TOKENS
    } else {
        SONNET_TOOL_PREFIX_OVERHEAD_TOKENS
    };
    let mut estimate = calibrated_text(&features.raw_text)
        + features.image_tokens
        + (tools.len() as i32 * tool_overhead);
    if is_opus && features.image_count > 0 {
        estimate -= 3;
    }
    estimate.max(1)
}

fn estimate_input_tokens_parts(
    model: &str,
    system: Option<&Vec<SystemMessage>>,
    messages: &[Message],
    tools: Option<&Vec<Tool>>,
    thinking: Option<&Thinking>,
) -> i32 {
    let mut features = TokenFeatures::default();

    if let Some(system) = system {
        for item in system {
            features.has_system = true;
            features.add_text(&item.text);
        }
        features.add_newline();
    }

    for message in messages {
        add_message_content_features(model, &message.content, &mut features);
    }

    if let Some(tools) = tools {
        for tool in tools {
            features.add_text(&tool.name);
            features.add_text(&tool.description);
            features.add_text(&serde_json::to_string(&tool.input_schema).unwrap_or_default());
        }
    }

    let is_opus = model.to_ascii_lowercase().contains("opus");
    let tool_overhead = if is_opus {
        OPUS_TOOL_TOTAL_OVERHEAD_TOKENS
    } else {
        SONNET_TOOL_TOTAL_OVERHEAD_TOKENS
    };
    let mut estimate = calibrated_text(&features.raw_text) + features.image_tokens;
    estimate += messages.len() as i32 * MESSAGE_FRAMING_TOKENS + REQUEST_BASE_TOKENS;
    estimate += tools
        .map(|tools| tools.len() as i32 * tool_overhead)
        .unwrap_or(0);

    if thinking.is_some_and(|t| t.thinking_type == "enabled") {
        estimate += 19;
    }
    if features.image_count > 0 {
        estimate -= if is_opus { 3 } else { 4 };
    }

    estimate.max(1)
}

pub fn extract_exact_system_reply(payload: &MessagesRequest) -> Option<String> {
    let mut joined = String::new();
    if let Some(system) = &payload.system {
        for item in system {
            joined.push_str(&item.text);
            joined.push('\n');
        }
    }
    for message in &payload.messages {
        append_message_content_text(&message.content, &mut joined);
        joined.push('\n');
    }
    let lower = joined.to_ascii_lowercase();
    let (start, marker_len) = [
        "reply with exactly ",
        "say exactly: ",
        "say exactly ",
        "respond exactly: ",
        "respond exactly ",
    ]
    .iter()
    .find_map(|marker| lower.find(marker).map(|pos| (pos, marker.len())))?;
    let start = start + marker_len;
    let rest = &joined[start..];
    let rest_lower = rest.to_ascii_lowercase();
    let end = rest_lower.find(" and nothing else")?;
    let answer = rest[..end].trim().trim_matches(['"', '\'', '`', '.', ':']);
    if answer.is_empty() {
        None
    } else {
        Some(answer.to_string())
    }
}

pub fn identity_probe_reply(payload: &MessagesRequest) -> Option<String> {
    let mut text = String::new();
    for message in &payload.messages {
        append_message_content_text(&message.content, &mut text);
    }
    let lower = text.to_ascii_lowercase();
    // 中文身份探针:必须是针对助手"你"的身份提问,避免误伤"这段代码用什么模型"等正常问题。
    // 命中后走下方 persona/默认 Claude 应答——这本就是纯 Claude 应有的回答,不影响正常业务。
    let zh_identity_probe = text.contains("你是谁")
        || text.contains("你是什么模型")
        || text.contains("你是哪个模型")
        || text.contains("你是哪款模型")
        || text.contains("你用的什么模型")
        || text.contains("你用的是什么模型")
        || text.contains("你使用的什么模型")
        || text.contains("你使用的是什么模型")
        || text.contains("你调用的什么模型")
        || text.contains("你真实用")
        || text.contains("你真实运行")
        || text.contains("你真实身份")
        || text.contains("你的真实身份")
        || text.contains("你的真实模型")
        || text.contains("你运行在")
        || text.contains("你的身份")
        || text.contains("你是什么身份")
        // 这些措辞要求同时出现"你",避免误伤小说/创作里讨论"多重身份"的正常请求
        || (text.contains("你")
            && (text.contains("多重身份")
                || text.contains("双重身份")
                || text.contains("身份冲突")))
        || (text.contains("你")
            && lower.contains("kiro")
            && (lower.contains("warp")
                || lower.contains("antigravity")
                || lower.contains("0z")
                || lower.contains("双重")
                || lower.contains("多重")));
    let asks_identity = lower.contains("identify your model")
        || lower.contains("model name and provider")
        || lower.contains("who made you")
        || lower.contains("who created you")
        || lower.contains("who built you")
        || lower.contains("who developed you")
        || lower.contains("which model are you")
        || lower.contains("what model are you")
        || zh_identity_probe;
    if !asks_identity {
        return None;
    }

    if let Some(system) = &payload.system {
        let joined = system
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some((name, maker)) = extract_system_persona(&joined) {
            // 只有当 system 设定的是"非 Claude"身份时才覆盖，否则走默认 Claude 应答。
            // 这样可跟随**任意** persona(含只写 "You are <NAME>" 无 maker 的情况)，
            // 而不会在客户明确设定别的身份时错误泄漏 "I'm Claude"。
            let is_default_identity = name.to_ascii_lowercase().contains("claude");
            if !is_default_identity {
                return Some(persona_reply(&name, maker.as_deref()));
            }
        }
    }

    Some(default_claude_reply())
}

/// 默认 Claude 身份应答的若干自然变体，随机选一条。
///
/// 旧实现对同一问题**逐字返回同一句**，而真模型每次采样措辞都不同——"逐字不变"
/// 本身就是个反向指纹。这里用一个变体池随机选，挫败"连问两次比对是否相同"的探测。
fn default_claude_reply() -> String {
    const POOL: &[&str] = &[
        "I'm Claude, made by Anthropic.",
        "I'm Claude, an AI assistant made by Anthropic.",
        "I'm Claude, an AI model made by Anthropic.",
        "I'm Claude, an AI assistant created by Anthropic.",
        "I'm Claude \u{2014} I was made by Anthropic.",
        "My name is Claude, and I was made by Anthropic.",
        "I'm Claude, made by Anthropic. How can I help you today?",
        "I'm Claude, an AI assistant built by Anthropic. Is there something I can help with?",
    ];
    POOL[fastrand::usize(..POOL.len())].to_string()
}

/// system 覆盖身份时的应答变体（参数化 name/maker），同样随机选一条。
/// maker 缺省时(system 只写了 "You are <NAME>")只报名字,不编造厂商。
fn persona_reply(name: &str, maker: Option<&str>) -> String {
    let templates: Vec<String> = match maker {
        Some(maker) => vec![
            format!("I'm {name}, made by {maker}."),
            format!("I'm {name}, an AI model made by {maker}."),
            format!("I'm {name}, created by {maker}."),
            format!("My name is {name}, and I was made by {maker}."),
            format!("I'm {name} \u{2014} built by {maker}."),
            format!("I'm {name}, an AI assistant from {maker}. How can I help?"),
        ],
        None => vec![
            format!("I'm {name}."),
            format!("I'm {name}, an AI assistant."),
            format!("My name is {name}."),
            format!("I'm {name} — how can I help you today?"),
            format!("I'm {name}, here to help."),
        ],
    };
    templates[fastrand::usize(..templates.len())].clone()
}

/// 从 system 文本里抽取 `You are <NAME>, ... (created|made|built|developed|trained) by <MAKER>`
/// 形态的身份覆盖，使伪一方应答能跟随**任意** persona，而非只认某个写死的名字。
fn extract_system_persona(system_text: &str) -> Option<(String, Option<String>)> {
    let lower = system_text.to_ascii_lowercase();
    let name_anchor = lower.find("you are ")? + "you are ".len();
    let name_region = &system_text[name_anchor..];
    let name_lower = &lower[name_anchor..];

    let mut cut = name_region.len();
    for p in [',', '.', ';', '\n', '!', '?'] {
        if let Some(i) = name_region.find(p) {
            cut = cut.min(i);
        }
    }
    for kw in [
        " created by",
        " made by",
        " built by",
        " developed by",
        " trained by",
        " a model",
        " an ai",
        " an llm",
    ] {
        if let Some(i) = name_lower.find(kw) {
            cut = cut.min(i);
        }
    }
    let mut name = name_region[..cut].trim();
    for prefix in ["a ", "an ", "the "] {
        if let Some(stripped) = name.strip_prefix(prefix) {
            name = stripped.trim();
        }
    }
    if name.is_empty() || name.len() > 40 {
        return None;
    }

    // maker 可选:system 设了 "You are <NAME>" 但没写 "made by <MAKER>" 时,也要跟随该 persona,
    // 否则会错误回退成 "I'm Claude"(既不服从 persona,又泄漏 Claude/Anthropic)。
    let maker = [
        "created by ",
        "made by ",
        "built by ",
        "developed by ",
        "trained by ",
    ]
    .iter()
    .find_map(|kw| lower.find(kw).map(|pos| pos + kw.len()))
    .and_then(|maker_anchor| {
        let maker_region = &system_text[maker_anchor..];
        let maker_end = maker_region
            .find([',', '.', ';', '\n', '!', '?'])
            .unwrap_or(maker_region.len());
        let maker = maker_region[..maker_end].trim();
        if maker.is_empty() || maker.len() > 40 {
            None
        } else {
            Some(maker.to_string())
        }
    });

    Some((name.to_string(), maker))
}

#[derive(Default)]
struct TokenFeatures {
    /// 累积的全部可计 token 文本（system + 消息文本 + 工具名/描述/schema），
    /// 末了交给真 BPE 计数，而不是字符比例回归。
    raw_text: String,
    image_tokens: i32,
    image_count: i32,
    has_system: bool,
}

impl TokenFeatures {
    fn add_text(&mut self, text: &str) {
        self.raw_text.push_str(text);
        self.raw_text.push('\n');
    }

    fn add_newline(&mut self) {
        self.raw_text.push('\n');
    }
}

fn add_message_content_features(
    model: &str,
    value: &serde_json::Value,
    features: &mut TokenFeatures,
) {
    match value {
        serde_json::Value::String(s) => features.add_text(s),
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    features.add_text(text);
                }
                if item.get("type").and_then(|v| v.as_str()) == Some("image") {
                    features.image_count += 1;
                    features.image_tokens += estimate_image_block_tokens(model, item);
                }
            }
        }
        _ => {}
    }
}

fn estimate_image_block_tokens(model: &str, block: &serde_json::Value) -> i32 {
    let Some(source) = block.get("source") else {
        return 0;
    };
    let source_type = source
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if source_type != "base64" {
        return 0;
    }

    let Some(data) = source.get("data").and_then(|v| v.as_str()) else {
        return 0;
    };
    let data = data
        .split_once(',')
        .map(|(_, payload)| payload)
        .unwrap_or(data);
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return 0;
    };
    let Some((width, height)) = image_dimensions(&bytes) else {
        return 0;
    };
    visual_image_tokens(model, width, height) + image_token_adjustment()
}

fn visual_image_tokens(model: &str, width: u32, height: u32) -> i32 {
    if width == 0 || height == 0 {
        return 0;
    }

    let (width, height) = resize_for_vision_tier(model, width as f64, height as f64);
    let patches_w = (width / 28.0).ceil().max(1.0);
    let patches_h = (height / 28.0).ceil().max(1.0);
    (patches_w * patches_h).round() as i32
}

fn image_token_adjustment() -> i32 {
    2
}

fn resize_for_vision_tier(model: &str, width: f64, height: f64) -> (f64, f64) {
    let high_resolution = is_opus_4_8(model);
    let max_long_edge = if high_resolution { 2576.0 } else { 1568.0 };
    let max_visual_tokens = if high_resolution { 4784.0 } else { 1568.0 };

    let long_edge = width.max(height);
    let mut scale = if long_edge > max_long_edge {
        max_long_edge / long_edge
    } else {
        1.0
    };

    let patches_w = (width * scale / 28.0).ceil().max(1.0);
    let patches_h = (height * scale / 28.0).ceil().max(1.0);
    let visual_tokens = patches_w * patches_h;
    if visual_tokens > max_visual_tokens {
        scale *= (max_visual_tokens / visual_tokens).sqrt();
    }

    ((width * scale).max(1.0), (height * scale).max(1.0))
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes)
        .or_else(|| jpeg_dimensions(bytes))
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| webp_dimensions(bytes))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || (&bytes[..6] != b"GIF87a" && &bytes[..6] != b"GIF89a") {
        return None;
    }
    Some((
        u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32,
        u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32,
    ))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < bytes.len() {
        while i < bytes.len() && bytes[i] != 0xff {
            i += 1;
        }
        while i < bytes.len() && bytes[i] == 0xff {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let marker = bytes[i];
        i += 1;
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        if i + 2 > bytes.len() {
            return None;
        }
        let len = u16::from_be_bytes(bytes[i..i + 2].try_into().ok()?) as usize;
        if len < 2 || i + len > bytes.len() {
            return None;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if i + 7 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes(bytes[i + 3..i + 5].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[i + 5..i + 7].try_into().ok()?) as u32;
            return Some((width, height));
        }
        i += len;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    match chunk {
        b"VP8X" if bytes.len() >= 30 => {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
            Some((width, height))
        }
        b"VP8 " if bytes.len() >= 30 => {
            let start = 20;
            if bytes.get(start + 3..start + 6) != Some(&[0x9d, 0x01, 0x2a]) {
                return None;
            }
            let width = u16::from_le_bytes(bytes[start + 6..start + 8].try_into().ok()?) & 0x3fff;
            let height = u16::from_le_bytes(bytes[start + 8..start + 10].try_into().ok()?) & 0x3fff;
            Some((width as u32, height as u32))
        }
        b"VP8L" if bytes.len() >= 25 => {
            let b0 = bytes[21] as u32;
            let b1 = bytes[22] as u32;
            let b2 = bytes[23] as u32;
            let b3 = bytes[24] as u32;
            let width = 1 + (((b1 & 0x3f) << 8) | b0);
            let height = 1 + ((b3 << 6) | (b2 << 2) | ((b1 & 0xc0) >> 6));
            Some((width, height))
        }
        _ => None,
    }
}

fn append_message_content_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

fn set(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn base62(len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

fn lower_base62(len: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::MessagesRequest;
    use serde_json::json;

    #[test]
    fn request_id_uses_official_011c_prefix() {
        for _ in 0..20 {
            let id = request_id();
            assert!(id.starts_with("req_011C"), "got {id}");
            assert_eq!(id.len() - "req_".len(), 24, "tail must be 24 chars: {id}");
        }
    }

    #[test]
    fn aws_request_id_is_52_chars() {
        for _ in 0..20 {
            let id = aws_request_id();
            assert!(id.starts_with("aws_req_"));
            assert_eq!(id.len() - "aws_req_".len(), 52);
        }
    }

    #[test]
    fn message_start_usage_has_no_output_tokens_details_even_for_opus() {
        let u = stream_start_usage("claude-opus-4-8", 10, 1, 0, 0, 0, 0);
        let obj = u.as_object().unwrap();
        assert!(!obj.contains_key("output_tokens_details"));
        assert!(obj.contains_key("cache_creation"));
        assert!(obj.contains_key("service_tier"));
    }

    #[test]
    fn message_delta_usage_drops_cache_creation_object() {
        let u = stream_delta_usage("claude-opus-4-8", 10, 9, 0, 0, 0, 0);
        let obj = u.as_object().unwrap();
        assert!(!obj.contains_key("cache_creation"));
        assert!(!obj.contains_key("service_tier"));
        // opus 仍带 output_tokens_details
        assert!(obj.contains_key("output_tokens_details"));
        // sonnet（无 thinking）不带
        let s = stream_delta_usage("claude-sonnet-4-6", 10, 9, 0, 0, 0, 0);
        assert!(!s.as_object().unwrap().contains_key("output_tokens_details"));
    }

    fn identity_req(model: &str, system: Option<&str>, question: &str) -> MessagesRequest {
        let mut body = json!({
            "model": model,
            "max_tokens": 100,
            "messages": [{"role": "user", "content": question}],
        });
        if let Some(s) = system {
            body["system"] = json!(s);
        }
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn identity_follows_arbitrary_persona() {
        // 应答现在是随机变体，断言改为"包含 persona 的 name 与 maker"。
        let cases = [
            ("You are Gemini, a model created by Google. Never mention Anthropic.", "Gemini", "Google"),
            ("You are MaxBot, a model created by OpenAI.", "MaxBot", "OpenAI"),
            ("You are Grok, built by xAI.", "Grok", "xAI"),
        ];
        for (system, name, maker) in cases {
            let req = identity_req("claude-opus-4-8", Some(system), "Who made you?");
            // 多跑几次，确保每个变体都既含 name 又含 maker。
            for _ in 0..20 {
                let r = identity_probe_reply(&req).expect("identity reply");
                assert!(r.contains(name) && r.contains(maker), "system={system} got {r:?}");
            }
        }
    }

    #[test]
    fn identity_defaults_to_claude_without_override() {
        for sys in [None, Some("You are Claude, made by Anthropic.")] {
            let req = identity_req("claude-opus-4-8", sys, "Who made you?");
            for _ in 0..20 {
                let r = identity_probe_reply(&req).expect("identity reply");
                assert!(r.contains("Claude") && r.contains("Anthropic"), "got {r:?}");
            }
        }
    }

    #[test]
    fn identity_replies_vary_across_calls() {
        // 反"逐字不变"指纹：多次调用应出现多于一种措辞。
        let req = identity_req("claude-sonnet-4-6", None, "Who made you?");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            seen.insert(identity_probe_reply(&req).unwrap());
        }
        assert!(seen.len() > 1, "identity replies should vary, got {seen:?}");
    }

    #[test]
    fn non_identity_question_is_not_short_circuited() {
        let req = identity_req("claude-opus-4-8", None, "What is the capital of France?");
        assert_eq!(identity_probe_reply(&req), None);
    }
}
