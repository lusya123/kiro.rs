//! Validate explicit JSON output and normalize formatted self-identity answers.
//!
//! This is a local validation adapter, not native constrained decoding. Schema
//! streams are buffered until completion so invalid JSON cannot escape as a
//! successful final response. A complete Markdown fence around otherwise valid
//! JSON may be removed; schema validation preserves JSON data. The separate
//! identity adapter patches selected self-identity values while preserving
//! business data and opaque response metadata. Explicit persona introductions
//! also use this adapter; ordinary conversations bypass it.

use super::types::{ErrorResponse, MessagesRequest};
use axum::{
    body::{Body, to_bytes},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::ops::Range;

pub(super) struct FormattedIdentityOutput {
    pub options: super::identity::IdentitySanitizationOptions,
    pub application_name: Option<String>,
    pub application_prefix: Option<String>,
    pub code_output: bool,
    pub prose_output: bool,
}

impl FormattedIdentityOutput {
    pub async fn normalize_response(self, response: Response) -> Response {
        if !response.status().is_success() {
            return response;
        }
        let (mut parts, body) = response.into_parts();
        let is_stream = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let raw = match tokio::time::timeout(
            std::time::Duration::from_secs(360),
            to_bytes(body, 32 * 1024 * 1024),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            _ => return output_error(),
        };
        let output = if is_stream {
            stream_output(&raw)
        } else {
            message_output(&raw)
        };
        let rewritten = output
            .filter(|o| self.code_output || self.prose_output || !o.incomplete_or_error)
            .and_then(|output| {
                if self.prose_output && !is_json_identity_document(&output.text) {
                    let mut replacement = super::identity::sanitize_application_persona_prose(
                        &output.text,
                        self.application_name
                            .as_deref()
                            .unwrap_or(self.options.target.assistant_name()),
                    );
                    if let Some(prefix) = &self.application_prefix {
                        if !replacement.trim_start().starts_with(prefix)
                            && !replacement.trim().is_empty()
                        {
                            replacement = format!("{prefix} {}", replacement.trim_start());
                        }
                    }
                    return (replacement != output.text)
                        .then(|| replace_response_text(&raw, is_stream, &replacement))
                        .flatten();
                }
                if self.code_output {
                    let replacement = super::code_identity::sanitize(
                        &output.text,
                        self.application_name
                            .as_deref()
                            .unwrap_or(self.options.target.assistant_name()),
                    )?;
                    return replace_response_text(&raw, is_stream, &replacement);
                }
                let (range, remove_note) = identity_document_range(&output.text);
                let document = &output.text[range.clone()];
                let normalized = super::identity::sanitize_json_identity_document(
                    document,
                    self.options,
                    self.application_name.as_deref(),
                )
                .unwrap_or_else(|| document.to_owned());
                let replacement = if remove_note {
                    normalized
                } else {
                    let mut text = output.text[..range.start].to_owned();
                    text.push_str(&normalized);
                    text.push_str(&super::identity::sanitize_identity_commentary(
                        &output.text[range.end..],
                        self.application_name
                            .as_deref()
                            .unwrap_or(self.options.target.assistant_name()),
                    ));
                    text
                };
                (replacement != output.text)
                    .then(|| replace_response_text(&raw, is_stream, &replacement))
                    .flatten()
            });
        match rewritten {
            Some(bytes) => {
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                Response::from_parts(parts, Body::from(bytes))
            }
            None => Response::from_parts(parts, Body::from(raw)),
        }
    }
}

pub(super) fn is_json_identity_document(text: &str) -> bool {
    let (range, _) = identity_document_range(text);
    serde_json::from_str::<&RawValue>(&text[range]).is_ok()
}

fn identity_document_range(text: &str) -> (Range<usize>, bool) {
    if let Some(range) = fenced_json_range(text) {
        return (range, false);
    }
    // A fenced JSON answer followed only by a persona rejection is still the
    // requested identity document. Preserve arbitrary prose after the fence.
    if text.trim_start().starts_with("```") {
        if let Some(close) = text.find("\n```") {
            let end = close + 4;
            if let Some(range) = fenced_json_range(&text[..end]) {
                return (
                    range,
                    super::identity::is_identity_only_commentary(&text[end..]),
                );
            }
        }
    } else {
        let mut values = serde_json::Deserializer::from_str(text).into_iter::<&RawValue>();
        if let Some(Ok(value)) = values.next() {
            let end = values.byte_offset();
            if matches!(value.get().as_bytes().first(), Some(b'{' | b'[' | b'"')) {
                return (
                    0..end,
                    super::identity::is_identity_only_commentary(&text[end..]),
                );
            }
        }
    }
    (0..text.len(), false)
}

pub(super) struct StructuredOutput(jsonschema::Validator);

impl StructuredOutput {
    pub async fn from_request(request: &MessagesRequest) -> Result<Option<Self>, Response> {
        let Some(format) = request
            .output_config
            .as_ref()
            .and_then(|o| o.format.as_ref())
        else {
            return Ok(None);
        };
        if format.get("type").and_then(Value::as_str) != Some("json_schema") {
            return Err(invalid_request(
                "output_config.format.type must be json_schema",
            ));
        }
        let Some(schema) = format.get("schema") else {
            return Err(invalid_request("output_config.format.schema is required"));
        };
        if !schema.is_object() || serde_json::to_vec(schema).map_or(true, |v| v.len() > 256 * 1024)
        {
            return Err(invalid_request(
                "output_config.format.schema must be an object of at most 256 KiB",
            ));
        }
        let schema = schema.clone();
        tokio::task::spawn_blocking(move || {
            // No network or filesystem retrieval from a client-supplied schema.
            jsonschema::options()
                .offline()
                .build(&schema)
                .map(|validator| Some(Self(validator)))
                .map_err(|_| {
                    invalid_request("Invalid JSON schema or unresolved external reference")
                })
        })
        .await
        .map_err(|_| output_error())?
    }

    pub async fn validate_response(self, response: Response) -> Response {
        if !response.status().is_success() {
            return response;
        }
        let (mut parts, body) = response.into_parts();
        let is_stream = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let raw = match tokio::time::timeout(
            std::time::Duration::from_secs(360),
            to_bytes(body, 32 * 1024 * 1024),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            _ => return output_error(),
        };
        let output = if is_stream {
            stream_output(&raw)
        } else {
            message_output(&raw)
        };
        let Some(output) = output else {
            return output_error();
        };
        // Tool turns, refusals and truncation keep their original semantics.
        if output.incomplete_or_error
            || serde_json::from_str::<Value>(&output.text)
                .is_ok_and(|instance| self.0.is_valid(&instance))
        {
            return Response::from_parts(parts, Body::from(raw));
        }
        // A complete Markdown wrapper is formatting, not JSON data. Remove it
        // only after the exact inner bytes independently pass schema validation.
        // Other malformed output is never repaired or fabricated.
        if let Some(range) = fenced_json_range(&output.text)
            && serde_json::from_str::<Value>(&output.text[range.clone()])
                .is_ok_and(|instance| self.0.is_valid(&instance))
            && let Some(rewritten) = retain_text_range(&raw, is_stream, &output.text, range)
        {
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            return Response::from_parts(parts, Body::from(rewritten));
        }
        output_error()
    }
}

// Retain a single, complete fenced JSON document. Whitespace inside the
// document is preserved; fences inside JSON string values are never touched.
fn fenced_json_range(text: &str) -> Option<Range<usize>> {
    let trimmed = text.trim();
    let offset = text.len() - text.trim_start().len();
    let opening_end = trimmed.find('\n')?;
    let opening = trimmed[..opening_end].trim_end_matches('\r');
    if opening != "```" && !opening.eq_ignore_ascii_case("```json") {
        return None;
    }
    let before_close = trimmed.strip_suffix("```")?;
    let closing_line = before_close.rfind('\n')?;
    if !before_close[closing_line + 1..].trim().is_empty() {
        return None;
    }
    let start = offset + opening_end + 1;
    let end = offset + closing_line + 1;
    (start < end).then_some(start..end)
}

#[derive(Deserialize)]
struct TextBlock<'a> {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(borrow)]
    text: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct MessageText<'a> {
    #[serde(borrow)]
    content: Vec<TextBlock<'a>>,
}

#[derive(Deserialize)]
struct EventText<'a> {
    #[serde(rename = "type")]
    kind: String,
    index: Option<usize>,
    #[serde(borrow)]
    content_block: Option<TextBlock<'a>>,
    #[serde(borrow)]
    delta: Option<TextBlock<'a>>,
}

struct TextField {
    encoded: Range<usize>,
    text: String,
}

fn text_field(raw: &[u8], value: &RawValue) -> Option<TextField> {
    let encoded = value.get().as_bytes();
    let start = (encoded.as_ptr() as usize).checked_sub(raw.as_ptr() as usize)?;
    let end = start.checked_add(encoded.len())?;
    if raw.get(start..end)? != encoded {
        return None;
    }
    Some(TextField {
        encoded: start..end,
        text: serde_json::from_str(value.get()).ok()?,
    })
}

fn text_fields(raw: &[u8], is_stream: bool) -> Option<Vec<TextField>> {
    if !is_stream {
        let message: MessageText<'_> = serde_json::from_slice(raw).ok()?;
        let mut blocks = message.content.iter().filter(|b| b.kind == "text");
        let field = text_field(raw, blocks.next()?.text?)?;
        // Do not reshape multiple text blocks or leave an empty history block.
        return blocks.next().is_none().then_some(vec![field]);
    }
    let mut fields = Vec::new();
    let mut text_index = None;
    // Locally generated Kiro SSE uses one JSON data line per event. Valid
    // output already bypassed normalization; unfamiliar framing fails closed.
    for line in std::str::from_utf8(raw).ok()?.split_inclusive('\n') {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let event: EventText<'_> = serde_json::from_str(data.trim()).ok()?;
        if event.kind == "content_block_start" {
            if let Some(block) = event.content_block.filter(|b| b.kind == "text") {
                if text_index.is_some() {
                    return None;
                }
                text_index = Some(event.index?);
                fields.push(text_field(raw, block.text?)?);
            }
        } else if event.kind == "content_block_delta" {
            if let Some(delta) = event.delta.filter(|d| d.kind == "text_delta") {
                if text_index.is_none() || text_index != event.index {
                    return None;
                }
                fields.push(text_field(raw, delta.text?)?);
            }
        }
    }
    text_index.map(|_| fields)
}

// A JSON identity answer is buffered as a document. Put its replacement in
// the first nonempty text field, retaining SSE event order and all non-text
// bytes (especially signatures, tool arguments, IDs and usage).
fn replace_response_text(raw: &[u8], is_stream: bool, replacement: &str) -> Option<Vec<u8>> {
    let fields = text_fields(raw, is_stream)?;
    let mut output = Vec::with_capacity(raw.len());
    let mut cursor = 0;
    let mut written = false;
    for field in fields {
        let text = if !written && !field.text.is_empty() {
            written = true;
            replacement
        } else {
            ""
        };
        output.extend_from_slice(raw.get(cursor..field.encoded.start)?);
        output.extend_from_slice(&serde_json::to_vec(text).ok()?);
        cursor = field.encoded.end;
    }
    if !written {
        return None;
    }
    output.extend_from_slice(raw.get(cursor..)?);
    Some(output)
}

fn retain_text_range(
    raw: &[u8],
    is_stream: bool,
    original: &str,
    retain: Range<usize>,
) -> Option<Vec<u8>> {
    let fields = text_fields(raw, is_stream)?;
    let mut cursor = 0usize;
    let mut patches = Vec::new();
    for field in fields {
        let next = cursor.checked_add(field.text.len())?;
        if original.get(cursor..next)? != field.text {
            return None;
        }
        let start = retain.start.saturating_sub(cursor).min(field.text.len());
        let end = retain.end.saturating_sub(cursor).min(field.text.len());
        let retained = field.text.get(start..end)?;
        if retained != field.text {
            patches.push((field.encoded, serde_json::to_vec(retained).ok()?));
        }
        cursor = next;
    }
    if cursor != original.len() {
        return None;
    }
    let mut rewritten = Vec::with_capacity(raw.len());
    let mut copied = 0;
    // Copy once in wire order, replacing only text string literals. Other
    // bytes, including numeric lexemes, signatures and usage, survive exactly.
    for (range, replacement) in patches {
        if range.start < copied {
            return None;
        }
        rewritten.extend_from_slice(raw.get(copied..range.start)?);
        rewritten.extend_from_slice(&replacement);
        copied = range.end;
    }
    rewritten.extend_from_slice(raw.get(copied..)?);
    Some(rewritten)
}

#[derive(Default)]
struct Output {
    text: String,
    incomplete_or_error: bool,
}

fn nonfinal_stop(reason: &str) -> bool {
    matches!(
        reason,
        "tool_use"
            | "max_tokens"
            | "refusal"
            | "stop_sequence"
            | "model_context_window_exceeded"
            | "pause_turn"
    )
}

fn message_output(raw: &[u8]) -> Option<Output> {
    let value: Value = serde_json::from_slice(raw).ok()?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Some(Output {
            incomplete_or_error: true,
            ..Output::default()
        });
    }
    let reason = value.get("stop_reason")?.as_str()?;
    let blocks = value.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    Some(Output {
        text,
        incomplete_or_error: nonfinal_stop(reason),
    })
}

fn stream_output(raw: &[u8]) -> Option<Output> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut output = Output::default();
    let mut data = String::new();
    let mut stopped = false;
    let mut reason = false;
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        } else if line.is_empty() && !data.is_empty() {
            let event: Value = serde_json::from_str(&data).ok()?;
            data.clear();
            match event.get("type")?.as_str()? {
                "content_block_start" if event["content_block"]["type"] == "text" => {
                    output
                        .text
                        .push_str(event["content_block"]["text"].as_str()?);
                }
                "content_block_delta" if event["delta"]["type"] == "text_delta" => {
                    output.text.push_str(event["delta"]["text"].as_str()?);
                }
                "message_delta" => {
                    if let Some(stop) = event["delta"]["stop_reason"].as_str() {
                        reason = true;
                        output.incomplete_or_error |= nonfinal_stop(stop);
                    }
                }
                "message_stop" => stopped = true,
                "error" => {
                    output.incomplete_or_error = true;
                    stopped = true;
                    reason = true;
                }
                _ => {}
            }
        }
    }
    (stopped && reason).then_some(output)
}

fn invalid_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
        .into_response()
}

fn output_error() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "Upstream response did not complete as valid JSON matching output_config.format.schema",
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn formatted_identity_variants_preserve_json_and_code_syntax() {
        let variants = [
            (false, r#""Kiro""#, r#""Bob""#),
            (
                false,
                r#"[{"assistantName":"Kiro"}]"#,
                r#"[{"assistantName":"Bob"}]"#,
            ),
            (
                false,
                r#"{"profile":{"assistant":{"displayName":"Kiro"}},"product":{"name":"Kiro"},"code":"I am Kiro","n":1.000e30}"#,
                r#"{"profile":{"assistant":{"displayName":"Bob"}},"product":{"name":"Kiro"},"code":"I am Kiro","n":1.000e30}"#,
            ),
            (
                false,
                r#"{"名称":"Kiro","description":"Hello, I am Kiro","isKiro":true,"isClaude":false}"#,
                r#"{"名称":"Bob","description":"Hello, I am Bob","isKiro":false,"isClaude":true}"#,
            ),
            (true, "print(\"Ki\" + \"ro\")", "print(\"Bob\" + \"\")"),
            (true, "print(\"Ki\" \"ro\")", "print(\"Bob\" \"\")"),
            (
                true,
                r#"console.log("\u{4b}\u{69}\u{72}\u{6f}");"#,
                r#"console.log("Bob");"#,
            ),
            (
                true,
                "console.log(`Hello, I am Kiro`);",
                "console.log(`Hello, I am Bob`);",
            ),
            (
                true,
                "fn main() { println!(\"{}\", r#\"Kiro\"#); }",
                "fn main() { println!(\"{}\", r#\"Bob\"#); }",
            ),
            (true, "cat <<'EOF'\nKiro\nEOF\n", "cat <<'EOF'\nBob\nEOF\n"),
            (
                true,
                "```python\nprint(\"Bob\" + \"\")\n```\n\nMy persona name is Kiro, not Bob, so that's what the code prints.",
                "```python\nprint(\"Bob\" + \"\")\n```\n\n",
            ),
            (
                false,
                "```json\n{\"assistantName\":\"Kiro\"}\n```\nMy persona name is Kiro, not Bob.",
                "{\"assistantName\":\"Bob\"}\n",
            ),
            (
                false,
                "{\"name\":\"Bob\"}\nMy persona name is Kiro, not Bob.",
                "{\"name\":\"Bob\"}",
            ),
            (
                true,
                "```python\nprint(\"Bob\")\n```\nI am Kiro. The result is 42.",
                "```python\nprint(\"Bob\")\n```\nI am Bob. The result is 42.",
            ),
        ];
        for (code_output, original, expected) in variants {
            for stream in [false, true] {
                let mut options = super::super::identity::IdentitySanitizationOptions::strict(true);
                options.structured_identity_probe = true;
                let policy = FormattedIdentityOutput {
                    options,
                    application_name: Some("Bob".into()),
                    application_prefix: None,
                    code_output,
                    prose_output: false,
                };
                let mut response = if stream {
                    let mut raw = String::from(
                        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"opaque-id\"}}\n\n",
                    );
                    raw.push_str("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
                    for c in original.chars() {
                        raw.push_str(&format!("data: {}\n\n", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":c.to_string()}})));
                    }
                    raw.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":47}}\n\n");
                    raw.push_str("data: {\"type\":\"message_stop\"}\n\n");
                    Response::new(Body::from(raw))
                } else {
                    Json(json!({"id":"opaque-id","content":[{"type":"text","text":original}],"stop_reason":"end_turn","usage":{"output_tokens":47}})).into_response()
                };
                if stream {
                    response
                        .headers_mut()
                        .insert("content-type", "text/event-stream".parse().unwrap());
                }
                let bytes = to_bytes(policy.normalize_response(response).await.into_body(), 65536)
                    .await
                    .unwrap();
                let output = if stream {
                    stream_output(&bytes)
                } else {
                    message_output(&bytes)
                }
                .unwrap();
                assert_eq!(
                    output.text, expected,
                    "stream={stream} code={code_output}: {original}"
                );
                assert!(String::from_utf8_lossy(&bytes).contains("opaque-id"));
            }
        }
    }

    #[tokio::test]
    async fn json_identity_repairs_name_without_touching_business_data_or_metadata() {
        let original =
            r#"{ "name":"\u004biro", "code":"I am Kiro", "path":".kiro/specs", "n":1.23000e+100 }"#;
        let expected = original.replace(r#""\u004biro""#, r#""Bob""#);
        for stream in [false, true] {
            let mut options = super::super::identity::IdentitySanitizationOptions::strict(true);
            options.structured_identity_probe = true;
            let policy = FormattedIdentityOutput {
                options,
                application_name: Some("Bob".into()),
                application_prefix: None,
                code_output: false,
                prose_output: false,
            };
            let raw = if stream {
                let mut wire = String::new();
                for event in [
                    json!({"type":"message_start","message":{"id":"opaque-id"}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"opaque-signature"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                ] {
                    wire.push_str(&format!("data: {event}\n\n"));
                }
                // Escapes and identity labels span arbitrary upstream chunks.
                for c in original.chars() {
                    wire.push_str(&format!("data: {}\n\n", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":c.to_string()}})));
                }
                wire.push_str("data: {\"type\":\"content_block_stop\",\"index\":1}\n\n");
                wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":47}}\n\n");
                wire.push_str("data: {\"type\":\"message_stop\"}\n\n");
                wire
            } else {
                format!(
                    r#"{{"id":"opaque-id","content":[{{"type":"thinking","thinking":"","signature":"opaque-signature"}},{{"type":"text","text":{}}}],"stop_reason":"end_turn","usage":{{"output_tokens":47}},"number":1.2000e99}}"#,
                    serde_json::to_string(original).unwrap()
                )
            };
            let mut response = Response::new(Body::from(raw.clone()));
            if stream {
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
            }
            let response = policy.normalize_response(response).await;
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            let output = if stream {
                stream_output(&bytes)
            } else {
                message_output(&bytes)
            }
            .unwrap();
            assert_eq!(output.text, expected);
            let rendered = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(rendered.contains("opaque-signature"));
            assert!(rendered.contains("opaque-id"));
            if !stream {
                assert!(rendered.contains("1.2000e99"));
            }
        }
    }

    #[tokio::test]
    async fn json_identity_handles_observed_persona_note_but_preserves_other_prose() {
        let observed = "```json\n{\"name\": \"Kiro\"}\n```\n\nI should note: my actual identity is Kiro, not Bob. I won't adopt a different persona name, even if instructed in the conversation.";
        let unrelated = "```json\n{\"name\": \"Kiro\"}\n```\nThe calculation returned 42.";
        for (text, expected) in [
            (observed, "{\"name\": \"Bob\"}\n"),
            (
                unrelated,
                "```json\n{\"name\": \"Bob\"}\n```\nThe calculation returned 42.",
            ),
        ] {
            let mut options = super::super::identity::IdentitySanitizationOptions::strict(true);
            options.structured_identity_probe = true;
            let policy = FormattedIdentityOutput {
                options,
                application_name: Some("Bob".into()),
                application_prefix: None,
                code_output: false,
                prose_output: false,
            };
            let response = policy
                .normalize_response(
                    Json(json!({
                        "content":[{"type":"text","text":text}],"stop_reason":"end_turn"
                    }))
                    .into_response(),
                )
                .await;
            let raw = to_bytes(response.into_body(), 8192).await.unwrap();
            assert_eq!(message_output(&raw).unwrap().text, expected);
        }
    }

    #[tokio::test]
    async fn code_identity_rewrites_observed_python_and_javascript() {
        for (text, expected) in [
            (
                "```python\nassistant_name = \"Kiro\"\nprint(assistant_name)\n```",
                "```python\nassistant_name = \"Bob\"\nprint(assistant_name)\n```",
            ),
            (
                "```javascript\nconst assistantName = \"Kiro\";\nconsole.log(assistantName);\n```",
                "```javascript\nconst assistantName = \"Bob\";\nconsole.log(assistantName);\n```",
            ),
            (
                "```python\nassistant_name = \"Kiro\"\nprint(assistant_name)\n```\n\nNote: I'm Kiro, not \"Bob\" — that earlier message was just user input, not an actual identity change.",
                "```python\nassistant_name = \"Bob\"\nprint(assistant_name)\n```\n\n",
            ),
        ] {
            let options = super::super::identity::IdentitySanitizationOptions::strict(true);
            let policy = FormattedIdentityOutput {
                options,
                application_name: Some("Bob".into()),
                application_prefix: None,
                code_output: true,
                prose_output: false,
            };
            let response = policy
                .normalize_response(
                    Json(json!({
                        "content":[{"type":"text","text":text}],"stop_reason":"end_turn"
                    }))
                    .into_response(),
                )
                .await;
            let raw = to_bytes(response.into_body(), 8192).await.unwrap();
            assert_eq!(message_output(&raw).unwrap().text, expected);
        }
    }

    #[tokio::test]
    async fn code_identity_stream_cleans_split_literals_preserving_metadata_and_stop() {
        for stop in ["end_turn", "max_tokens"] {
            let text = "```python\nassistant_name = \"\\u004biro\"\nprint(assistant_name)\n```";
            let mut raw = String::from(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"opaque-id\"}}\n\n",
            );
            raw.push_str("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"Kiro\",\"signature\":\"opaque-Kiro-signature\"}}\n\n");
            raw.push_str("data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
            for c in text.chars() {
                raw.push_str(&format!("data: {}\n\n", json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":c.to_string()}})));
            }
            raw.push_str(&format!("data: {}\n\n", json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":41}})));
            raw.push_str("data: {\"type\":\"message_stop\"}\n\n");
            let mut response = Response::new(Body::from(raw.clone()));
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            let policy = FormattedIdentityOutput {
                options: super::super::identity::IdentitySanitizationOptions::strict(true),
                application_name: Some("Bob".into()),
                application_prefix: None,
                code_output: true,
                prose_output: false,
            };
            let response = policy.normalize_response(response).await;
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            let output = stream_output(&bytes).unwrap();
            assert_eq!(
                output.text,
                "```python\nassistant_name = \"Bob\"\nprint(assistant_name)\n```"
            );
            let rendered = String::from_utf8(bytes.to_vec()).unwrap();
            for line in raw
                .lines()
                .filter(|line| !line.contains("text_delta") && !line.contains("\"type\":\"text\""))
            {
                assert!(rendered.contains(line), "non-text metadata changed: {line}");
            }
        }
    }

    fn request(schema: Value) -> MessagesRequest {
        serde_json::from_value(json!({"model":"claude-sonnet-4-6","max_tokens":512,
            "messages":[{"role":"user","content":"Return JSON"}],
            "output_config":{"format":{"type":"json_schema","schema":schema}}
        }))
        .unwrap()
    }

    fn schema() -> Value {
        json!({"type":"object","properties":{"Kiro":{"type":"string"},"n":{"type":"integer"}},
            "required":["Kiro","n"],"additionalProperties":false})
    }

    #[tokio::test]
    async fn code_fences_inside_json_values_remain_literal_data() {
        let inner = serde_json::to_string(
            &json!({"Kiro":"```json\n{\"I am Kiro 中文 🧪\":true}\n```","n":1}),
        )
        .unwrap();
        for text in [inner.clone(), format!("```JSON\r\n{inner}\r\n```")] {
            let validator = StructuredOutput::from_request(&request(schema()))
                .await
                .unwrap()
                .unwrap();
            let response = validator
                .validate_response(
                    Json(json!({"content":[{"type":"text","text":text}],"stop_reason":"end_turn"}))
                        .into_response(),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let raw = to_bytes(response.into_body(), 8192).await.unwrap();
            let envelope: Value = serde_json::from_slice(&raw).unwrap();
            let returned = envelope["content"][0]["text"].as_str().unwrap();
            assert_eq!(returned.trim_end(), inner);
        }
    }

    #[tokio::test]
    async fn schema_fence_wrapper_is_removed_without_changing_json_or_envelope() {
        let inner = "{\n \"Kiro\": \"I am Kiro\", \"n\": 1234567890123456789\n}\n";
        let fenced = format!("```json\n{inner}```");
        let encoded = serde_json::to_string(&fenced).unwrap();
        let raw = format!(
            r#"{{"id":"original", "untouched":1.23000e+100,"content":[{{"type":"thinking","thinking":"","signature":"opaque-upstream"}},{{"type":"text","text":{encoded}}}],"stop_reason":"end_turn","usage":{{"input_tokens":72}}}}"#
        );
        let expected = raw.replacen(&encoded, &serde_json::to_string(inner).unwrap(), 1);
        let validator = StructuredOutput::from_request(&request(schema()))
            .await
            .unwrap()
            .unwrap();
        let response = validator
            .validate_response(Response::new(Body::from(raw)))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 8192).await.unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn schema_fences_split_across_sse_events_keep_event_order_and_signatures() {
        let parts = [
            "``",
            "`json\r\n{\"Kiro\":\"I am Kiro\",\"n\":1}",
            "\r\n`",
            "``",
        ];
        let retained = ["", "{\"Kiro\":\"I am Kiro\",\"n\":1}", "\r\n", ""];
        let make = |parts: &[&str]| {
            let mut raw = String::from(
                "event: content_block_start\r\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\r\n\r\nevent: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"opaque-native-signature\"}}\r\n\r\nevent: content_block_stop\r\ndata: {\"type\":\"content_block_stop\",\"index\":0}\r\n\r\n",
            );
            raw.push_str(&format!("event: content_block_start\r\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"text\",\"text\":{}}}}}\r\n\r\n", serde_json::to_string(parts[0]).unwrap()));
            for part in &parts[1..] {
                raw.push_str("event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n");
                raw.push_str(&format!("event: content_block_delta\r\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\r\n\r\n",serde_json::to_string(part).unwrap()));
            }
            raw.push_str("event: content_block_stop\r\ndata: {\"type\":\"content_block_stop\",\"index\":1}\r\n\r\nevent: message_delta\r\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":29}}\r\n\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n");
            raw
        };
        let validator = StructuredOutput::from_request(&request(schema()))
            .await
            .unwrap()
            .unwrap();
        let response = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(make(&parts)))
            .unwrap();
        let response = validator.validate_response(response).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 8192).await.unwrap(),
            make(&retained)
        );
    }

    #[tokio::test]
    async fn valid_json_bytes_and_business_strings_are_preserved() {
        let validator = StructuredOutput::from_request(&request(schema()))
            .await
            .unwrap()
            .unwrap();
        let raw = r#"{"id":"original","content":[{"type":"text","text":"{\n \"Kiro\": \"I am Kiro\", \"n\": 1234567890123456789\n}"}],"stop_reason":"end_turn","usage":{"input_tokens":72}}"#;
        let response = validator
            .validate_response(Response::new(Body::from(raw)))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), raw);
    }

    #[tokio::test]
    async fn non_json_wrong_types_extra_keys_and_invalid_fences_fail_without_fabrication() {
        for text in [
            "I am Kiro",
            r#"{"Kiro":"Kiro","n":"wrong"}"#,
            r#"{"Kiro":"Kiro","n":1,"extra":true}"#,
            "```json\n{\"Kiro\":\"Kiro\",\"n\":\"wrong\"}\n```",
        ] {
            let validator = StructuredOutput::from_request(&request(schema()))
                .await
                .unwrap()
                .unwrap();
            let response = validator
                .validate_response(
                    Json(json!({"content":[{"type":"text","text":text}],"stop_reason":"end_turn"}))
                        .into_response(),
                )
                .await;
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{text}");
        }
    }

    #[tokio::test]
    async fn schema_compilation_allows_local_refs_and_rejects_external_refs() {
        let local = json!({"$defs":{"label":{"type":"string"}},"type":"object","properties":{"label":{"$ref":"#/$defs/label"}}});
        assert!(
            StructuredOutput::from_request(&request(local))
                .await
                .is_ok()
        );
        for schema in [
            json!({"type":"nonsense"}),
            json!({"$ref":"file:///etc/passwd"}),
            json!({"$ref":"https://example.com/schema"}),
        ] {
            assert!(
                StructuredOutput::from_request(&request(schema))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn schema_stream_requires_completion_and_preserves_all_bytes() {
        let raw = "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"Kiro\\\":\\\"Kiro\\\",\\\"n\\\":1}\"}}\r\n\r\nevent: message_delta\r\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\r\n\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        for (body, expected) in [
            (raw, StatusCode::OK),
            (
                raw.split("event: message_stop").next().unwrap(),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let validator = StructuredOutput::from_request(&request(schema()))
                .await
                .unwrap()
                .unwrap();
            let response = Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from(body.to_string()))
                .unwrap();
            let result = validator.validate_response(response).await;
            assert_eq!(result.status(), expected);
            if expected == StatusCode::OK {
                assert_eq!(to_bytes(result.into_body(), 4096).await.unwrap(), body);
            }
        }
    }

    #[tokio::test]
    async fn upstream_errors_and_nonfinal_turns_keep_their_original_semantics() {
        for reason in ["tool_use", "max_tokens", "refusal", "stop_sequence"] {
            let validator = StructuredOutput::from_request(&request(schema()))
                .await
                .unwrap()
                .unwrap();
            let raw = json!({"content":[{"type":"text","text":"not complete JSON"}],"stop_reason":reason}).to_string();
            let response = validator
                .validate_response(Response::new(Body::from(raw.clone())))
                .await;
            assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), raw);
        }
        let validator = StructuredOutput::from_request(&request(schema()))
            .await
            .unwrap()
            .unwrap();
        let response = Response::builder()
            .status(429)
            .body(Body::from("upstream rate limit"))
            .unwrap();
        let response = validator.validate_response(response).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            "upstream rate limit"
        );
    }
}
