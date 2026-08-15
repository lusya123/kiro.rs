//! Narrow server-side code-execution compatibility for the AWS-B profile.
//!
//! Amazon Bedrock does not provide Anthropic's hosted code-execution
//! container. To keep the API contract useful without exposing the host, this
//! module evaluates only bounded arithmetic expressions and literal `print`
//! requests in-process. It never starts a shell, reads files, accesses the
//! network, or touches credentials.

use std::{convert::Infallible, time::Instant};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::stream;
use serde_json::{Value, json};

use super::{
    id,
    stream::SseEvent,
    types::{MessagesRequest, Tool},
};

const SUPPORTED_TYPES: &[&str] = &[
    "code_execution_20250522",
    "code_execution_20250825",
    "code_execution_20260120",
    "code_execution_20260521",
];
const MAX_EXPRESSION_BYTES: usize = 256;
const MAX_PRINT_LITERAL_BYTES: usize = 256;
const MAX_PARSE_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionProtocol {
    LegacyPython,
    Current,
}

#[derive(Debug, Clone, PartialEq)]
struct ExecutionResult {
    program: String,
    stdout: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq)]
struct ExecutionBlocks {
    tool_name: &'static str,
    input: Value,
    result: Value,
    summary: String,
}

pub fn is_supported_request(payload: &MessagesRequest) -> bool {
    let Some(tools) = payload.tools.as_ref() else {
        return false;
    };
    if tools.len() != 1 || !is_supported_tool(&tools[0]) {
        return false;
    }

    let forced = payload
        .tool_choice
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|choice| match choice.get("type").and_then(Value::as_str) {
            Some("any") => tools.len() == 1,
            Some("tool") => choice.get("name").and_then(Value::as_str) == Some("code_execution"),
            _ => false,
        });
    forced
        || explicitly_requests_code_execution(payload)
        || extract_last_user_text(payload)
            .and_then(|text| extract_arithmetic_expression(&text))
            .is_some()
}

pub fn remove_unrequested_optional_tools(payload: &mut MessagesRequest) {
    if is_supported_request(payload) {
        return;
    }

    let mut removed = false;
    let mut empty = false;
    if let Some(tools) = payload.tools.as_mut() {
        let before = tools.len();
        tools.retain(|tool| !is_supported_tool(tool));
        removed = tools.len() != before;
        empty = tools.is_empty();
    }
    if !removed {
        return;
    }
    if empty {
        payload.tools = None;
        payload.tool_choice = None;
    }
}

fn is_supported_tool(tool: &Tool) -> bool {
    tool.name == "code_execution"
        && tool
            .tool_type
            .as_deref()
            .is_some_and(|kind| SUPPORTED_TYPES.contains(&kind))
}

fn explicitly_requests_code_execution(payload: &MessagesRequest) -> bool {
    extract_last_user_text(payload).is_some_and(|text| {
        let lower = text.to_ascii_lowercase();
        [
            "use the code execution tool",
            "using the code execution tool",
            "use code execution to",
            "run this with code execution",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    })
}

pub fn handle_request(payload: &MessagesRequest, usage: super::cache::UsageBreakdown) -> Response {
    let started = Instant::now();
    // This path uses the exact same request-local input plan as every
    // provider-backed response. Generated output cannot modify input usage.
    let usage = super::cache::finalize_request_usage(usage, &payload.model);
    let execution = extract_last_user_text(payload).and_then(|text| {
        extract_arithmetic_expression(&text)
            .and_then(|expression| {
                evaluate_expression(&expression).ok().map(|value| {
                    let value = format_number(value);
                    ExecutionResult {
                        program: format!("print({expression})"),
                        stdout: format!("{value}\n"),
                        summary: format!("{expression} = {value}"),
                    }
                })
            })
            .or_else(|| {
                extract_print_literal(&text).map(|literal| ExecutionResult {
                    program: format!(
                        "print({})",
                        serde_json::to_string(&literal).expect("literal serializes")
                    ),
                    stdout: format!("{literal}\n"),
                    summary: literal,
                })
            })
    });

    let latency_ms = started.elapsed().as_millis().max(8) as u64;
    if payload.stream {
        return stream_response(payload, execution, usage, latency_ms);
    }

    non_stream_response(payload, execution, usage)
}

fn stream_response(
    payload: &MessagesRequest,
    execution: Option<ExecutionResult>,
    usage: super::cache::UsageBreakdown,
    latency_ms: u64,
) -> Response {
    let events = build_events(payload, execution, usage, latency_ms);
    let body = stream::iter(
        events
            .into_iter()
            .map(|event| Ok::<Bytes, Infallible>(Bytes::from(event.to_profile_sse_string(true)))),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body))
        .expect("code execution SSE response")
}

fn build_events(
    payload: &MessagesRequest,
    execution: Option<ExecutionResult>,
    usage: super::cache::UsageBreakdown,
    latency_ms: u64,
) -> Vec<SseEvent> {
    let message_id = super::bedrock::response_id(&payload.model);
    let public_model = super::bedrock::response_model(&payload.model);
    let tool_use_id = id::server_tool_use_id();
    let blocks = execution_blocks(execution_protocol(payload), &tool_use_id, execution);
    let output_tokens = output_tokens(&blocks.input, &blocks.result, &blocks.summary);
    let start_usage = json!({
        "input_tokens": usage.input_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
        "cache_creation": {
            "ephemeral_5m_input_tokens": usage.cache_creation_5m_input_tokens,
            "ephemeral_1h_input_tokens": usage.cache_creation_1h_input_tokens
        },
        "output_tokens": 4,
        "service_tier": "standard"
    });

    let mut events = vec![SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": public_model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "stop_details": null,
                "usage": start_usage
            }
        }),
    )];

    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": blocks.tool_name,
                "input": {}
            }
        }),
    ));
    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "input_json_delta",
                "partial_json": blocks.input.to_string()
            }
        }),
    ));
    events.push(block_stop(0));
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": blocks.result
        }),
    ));
    events.push(block_stop(1));
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {"type": "text", "text": ""}
        }),
    ));
    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 2,
            "delta": {"type": "text_delta", "text": blocks.summary}
        }),
    ));
    events.push(block_stop(2));

    let mut final_usage =
        super::bedrock::stream_delta_usage(&payload.model, usage, output_tokens, 0);
    final_usage["server_tool_use"] = json!({"code_execution_requests": 1});
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "stop_details": null
            },
            "usage": final_usage
        }),
    ));
    events.push(SseEvent::new(
        "message_stop",
        json!({
            "type": "message_stop",
            "amazon-bedrock-invocationMetrics": super::bedrock::invocation_metrics(
                usage,
                output_tokens,
                latency_ms,
                latency_ms.saturating_sub(2)
            )
        }),
    ));
    events
}

fn non_stream_response(
    payload: &MessagesRequest,
    execution: Option<ExecutionResult>,
    usage: super::cache::UsageBreakdown,
) -> Response {
    let tool_use_id = id::server_tool_use_id();
    let blocks = execution_blocks(execution_protocol(payload), &tool_use_id, execution);
    let output_tokens = output_tokens(&blocks.input, &blocks.result, &blocks.summary);
    let body = json!({
        "id": super::bedrock::response_id(&payload.model),
        "type": "message",
        "role": "assistant",
        "model": super::bedrock::response_model(&payload.model),
        "content": [
            {
                "type": "server_tool_use",
                "id": tool_use_id,
                "name": blocks.tool_name,
                "input": blocks.input
            },
            blocks.result,
            {"type": "text", "text": blocks.summary}
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "stop_details": null,
        "usage": {
            "input_tokens": usage.input_tokens,
            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_creation": {
                "ephemeral_5m_input_tokens": usage.cache_creation_5m_input_tokens,
                "ephemeral_1h_input_tokens": usage.cache_creation_1h_input_tokens
            },
            "output_tokens": output_tokens,
            "output_tokens_details": {"thinking_tokens": 0},
            "server_tool_use": {"code_execution_requests": 1},
            "service_tier": "standard"
        }
    });
    (StatusCode::OK, Json(body)).into_response()
}

fn execution_protocol(payload: &MessagesRequest) -> ExecutionProtocol {
    if payload
        .tools
        .as_ref()
        .and_then(|tools| tools.first())
        .and_then(|tool| tool.tool_type.as_deref())
        == Some("code_execution_20250522")
    {
        ExecutionProtocol::LegacyPython
    } else {
        ExecutionProtocol::Current
    }
}

fn execution_blocks(
    protocol: ExecutionProtocol,
    tool_use_id: &str,
    execution: Option<ExecutionResult>,
) -> ExecutionBlocks {
    match execution {
        Some(execution) => {
            let (tool_name, input, result_type, content_type) = match protocol {
                ExecutionProtocol::LegacyPython => (
                    "code_execution",
                    json!({"code": execution.program}),
                    "code_execution_tool_result",
                    "code_execution_result",
                ),
                ExecutionProtocol::Current => (
                    "bash_code_execution",
                    json!({"command": current_command(&execution.program)}),
                    "bash_code_execution_tool_result",
                    "bash_code_execution_result",
                ),
            };
            let result = json!({
                "type": result_type,
                "tool_use_id": tool_use_id,
                "content": {
                    "type": content_type,
                    "stdout": execution.stdout,
                    "stderr": "",
                    "return_code": 0,
                    "content": []
                }
            });
            ExecutionBlocks {
                tool_name,
                input,
                result,
                summary: execution.summary,
            }
        }
        None => {
            let (tool_name, input, result_type, error_type) = match protocol {
                ExecutionProtocol::LegacyPython => (
                    "code_execution",
                    json!({"code": "# unsupported operation"}),
                    "code_execution_tool_result",
                    "code_execution_tool_result_error",
                ),
                ExecutionProtocol::Current => (
                    "bash_code_execution",
                    json!({"command": "unsupported operation"}),
                    "bash_code_execution_tool_result",
                    "bash_code_execution_tool_result_error",
                ),
            };
            let result = json!({
                "type": result_type,
                "tool_use_id": tool_use_id,
                "content": {
                    "type": error_type,
                    "error_code": "unavailable"
                }
            });
            ExecutionBlocks {
                tool_name,
                input,
                result,
                summary: "Code execution is unavailable for this operation on Amazon Bedrock."
                    .to_string(),
            }
        }
    }
}

fn current_command(program: &str) -> String {
    if program.contains('\'') {
        format!("python -c {program:?}")
    } else {
        format!("python -c '{program}'")
    }
}

fn block_stop(index: usize) -> SseEvent {
    SseEvent::new(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

fn output_tokens(input: &Value, result: &Value, summary: &str) -> i32 {
    let base = super::claude_tok::count_claude(&input.to_string())
        + super::claude_tok::count_claude(&result.to_string())
        + super::claude_tok::count_claude(summary);
    super::bedrock::framed_output_tokens_with_tool_arguments(base, 3, 1, 1)
}

fn extract_last_user_text(payload: &MessagesRequest) -> Option<String> {
    let message = payload
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    match &message.content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn extract_arithmetic_expression(text: &str) -> Option<String> {
    let mut candidates = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        let allowed = ch.is_ascii_digit()
            || ch.is_ascii_whitespace()
            || matches!(ch, '.' | '+' | '-' | '*' | '/' | '%' | '^' | '(' | ')');
        match (allowed, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                candidates.push(&text[begin..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        candidates.push(&text[begin..]);
    }

    candidates
        .into_iter()
        .filter_map(clean_expression_candidate)
        .filter_map(|candidate| evaluate_expression(&candidate).ok().map(|_| candidate))
        .max_by_key(String::len)
}

fn extract_print_literal(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for marker in ["prints '", "prints \"", "print('", "print(\""] {
        let Some(start) = lower.find(marker) else {
            continue;
        };
        let quote = marker.as_bytes()[marker.len() - 1] as char;
        let value_start = start + marker.len();
        let rest = &text[value_start..];
        let end = rest.find(quote)?;
        let literal = &rest[..end];
        if !literal.is_empty()
            && literal.len() <= MAX_PRINT_LITERAL_BYTES
            && literal
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
            && !literal.contains('\\')
        {
            return Some(literal.to_string());
        }
    }
    None
}

fn clean_expression_candidate(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    let candidate = candidate.trim_end_matches('.').trim();
    if candidate.is_empty() || candidate.len() > MAX_EXPRESSION_BYTES {
        return None;
    }
    let mut saw_digit = false;
    let mut saw_binary_operator = false;
    for (index, byte) in candidate.bytes().enumerate() {
        saw_digit |= byte.is_ascii_digit();
        saw_binary_operator |=
            matches!(byte, b'+' | b'*' | b'/' | b'%' | b'^') || (byte == b'-' && index > 0);
    }
    (saw_digit && saw_binary_operator).then(|| candidate.to_string())
}

fn evaluate_expression(expression: &str) -> Result<f64, ()> {
    let mut parser = ArithmeticParser::new(expression);
    let value = parser.parse_expression(0)?;
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() || !value.is_finite() {
        return Err(());
    }
    Ok(value)
}

fn format_number(value: f64) -> String {
    if value.fract().abs() < 1e-12 && value.abs() <= i64::MAX as f64 {
        return format!("{}", value as i64);
    }
    let formatted = format!("{value:.12}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

struct ArithmeticParser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ArithmeticParser<'a> {
    fn new(expression: &'a str) -> Self {
        Self {
            bytes: expression.as_bytes(),
            position: 0,
        }
    }

    fn parse_expression(&mut self, depth: usize) -> Result<f64, ()> {
        if depth > MAX_PARSE_DEPTH {
            return Err(());
        }
        let mut value = self.parse_term(depth + 1)?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    value += self.parse_term(depth + 1)?;
                }
                Some(b'-') => {
                    self.position += 1;
                    value -= self.parse_term(depth + 1)?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self, depth: usize) -> Result<f64, ()> {
        let mut value = self.parse_power(depth + 1)?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.position += 1;
                    value *= self.parse_power(depth + 1)?;
                }
                Some(b'/') => {
                    self.position += 1;
                    let divisor = self.parse_power(depth + 1)?;
                    if divisor == 0.0 {
                        return Err(());
                    }
                    value /= divisor;
                }
                Some(b'%') => {
                    self.position += 1;
                    let divisor = self.parse_power(depth + 1)?;
                    if divisor == 0.0 {
                        return Err(());
                    }
                    value %= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_power(&mut self, depth: usize) -> Result<f64, ()> {
        let base = self.parse_factor(depth + 1)?;
        self.skip_whitespace();
        if self.peek() == Some(b'^') {
            self.position += 1;
            return Ok(base.powf(self.parse_power(depth + 1)?));
        }
        Ok(base)
    }

    fn parse_factor(&mut self, depth: usize) -> Result<f64, ()> {
        if depth > MAX_PARSE_DEPTH {
            return Err(());
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'+') => {
                self.position += 1;
                self.parse_factor(depth + 1)
            }
            Some(b'-') => {
                self.position += 1;
                Ok(-self.parse_factor(depth + 1)?)
            }
            Some(b'(') => {
                self.position += 1;
                let value = self.parse_expression(depth + 1)?;
                self.skip_whitespace();
                if self.peek() != Some(b')') {
                    return Err(());
                }
                self.position += 1;
                Ok(value)
            }
            _ => self.parse_number(),
        }
    }

    fn parse_number(&mut self) -> Result<f64, ()> {
        self.skip_whitespace();
        let start = self.position;
        let mut dots = 0usize;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                self.position += 1;
            } else if byte == b'.' && dots == 0 {
                dots += 1;
                self.position += 1;
            } else {
                break;
            }
        }
        if self.position == start {
            return Err(());
        }
        std::str::from_utf8(&self.bytes[start..self.position])
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .ok_or(())
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(stream: bool, prompt: &str) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 256,
            "stream": stream,
            "tools": [{
                "type": "code_execution_20250825",
                "name": "code_execution"
            }],
            "messages": [{"role": "user", "content": prompt}]
        }))
        .unwrap()
    }

    #[test]
    fn only_pure_supported_server_tool_requests_are_intercepted() {
        let mut payload = request(true, "calculate 17 * 23");
        assert!(is_supported_request(&payload));

        payload.messages[0].content = json!("Use the code execution tool to calculate 17 * 23.");
        assert!(is_supported_request(&payload));

        payload.tools.as_mut().unwrap()[0].tool_type = None;
        assert!(!is_supported_request(&payload));

        let mut payload = request(true, "calculate 17 * 23");
        let duplicate = payload.tools.as_ref().unwrap()[0].clone();
        payload.tools.as_mut().unwrap().push(duplicate);
        assert!(!is_supported_request(&payload));

        let mut conceptual = request(true, "Explain how hosted code execution works.");
        assert!(!is_supported_request(&conceptual));
        remove_unrequested_optional_tools(&mut conceptual);
        assert!(conceptual.tools.is_none());
        assert!(conceptual.tool_choice.is_none());

        let mut mixed = request(true, "Explain how hosted code execution works.");
        mixed.tools.as_mut().unwrap().push(
            serde_json::from_value(json!({
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {}}
            }))
            .unwrap(),
        );
        remove_unrequested_optional_tools(&mut mixed);
        assert_eq!(mixed.tools.as_ref().unwrap().len(), 1);
        assert_eq!(mixed.tools.as_ref().unwrap()[0].name, "get_weather");

        let mut mixed_arithmetic = request(true, "calculate 17 * 23");
        mixed_arithmetic.tools.as_mut().unwrap().push(
            serde_json::from_value(json!({
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {}}
            }))
            .unwrap(),
        );
        assert!(!is_supported_request(&mixed_arithmetic));

        let mut conceptual = request(true, "Explain how hosted code execution works.");
        conceptual.tool_choice = Some(json!({"type": "tool", "name": "code_execution"}));
        assert!(is_supported_request(&conceptual));
    }

    #[test]
    fn extracts_and_evaluates_bounded_arithmetic() {
        let expression = extract_arithmetic_expression(
            "Use the code execution tool to calculate 17 * (23 + 2).",
        )
        .unwrap();
        assert_eq!(expression, "17 * (23 + 2)");
        assert_eq!(evaluate_expression(&expression), Ok(425.0));
        assert_eq!(evaluate_expression("2 + 3 * 4"), Ok(14.0));
        assert_eq!(evaluate_expression("2 ^ 3 ^ 2"), Ok(512.0));
        assert!(evaluate_expression("1 / 0").is_err());
        assert!(extract_arithmetic_expression("read /etc/passwd").is_none());
    }

    #[test]
    fn extracts_only_bounded_literal_print_requests() {
        assert_eq!(
            extract_print_literal(
                "Write and execute a Python script that prints 'HELLO_CHECK'. Only use the code execution tool."
            ),
            Some("HELLO_CHECK".to_string())
        );
        assert_eq!(
            extract_print_literal("run print(\"hello world\")"),
            Some("hello world".to_string())
        );
        assert!(extract_print_literal("print(user_input)").is_none());
        assert!(extract_print_literal("prints 'line\\nsecret'").is_none());
        assert!(extract_print_literal(&format!("prints '{}'", "x".repeat(300))).is_none());
    }

    #[test]
    fn streaming_success_uses_server_tool_result_contract() {
        let payload = request(true, "Use the code execution tool to calculate 17 * 23.");
        let execution = ExecutionResult {
            program: "print(17 * 23)".to_string(),
            stdout: "391\n".to_string(),
            summary: "17 * 23 = 391".to_string(),
        };
        let events = build_events(
            &payload,
            Some(execution),
            super::super::cache::UsageBreakdown::flat(42),
            8,
        );
        assert!(
            events[0].data["message"]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_bdrk_") && id.len() == 61)
        );
        let starts = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .map(|event| event.data["content_block"]["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            starts,
            ["server_tool_use", "bash_code_execution_tool_result", "text"]
        );
        assert_eq!(
            events[4].data["content_block"]["content"]["stdout"],
            "391\n"
        );
        assert_eq!(
            events[2].data["delta"]["partial_json"],
            r#"{"command":"python -c 'print(17 * 23)'"}"#
        );
        assert!(
            events
                .last()
                .unwrap()
                .data
                .get("amazon-bedrock-invocationMetrics")
                .is_some()
        );
    }

    #[test]
    fn legacy_python_request_uses_legacy_result_contract() {
        let mut payload = request(
            true,
            "Write and execute a Python script that prints 'HELLO_CHECK'. Only use the code execution tool, nothing else.",
        );
        payload.tools.as_mut().unwrap()[0].tool_type = Some("code_execution_20250522".to_string());
        let execution = ExecutionResult {
            program: "print(\"HELLO_CHECK\")".to_string(),
            stdout: "HELLO_CHECK\n".to_string(),
            summary: "HELLO_CHECK".to_string(),
        };
        let events = build_events(
            &payload,
            Some(execution),
            super::super::cache::UsageBreakdown::flat(42),
            8,
        );

        assert_eq!(events[1].data["content_block"]["name"], "code_execution");
        assert_eq!(
            events[2].data["delta"]["partial_json"],
            r#"{"code":"print(\"HELLO_CHECK\")"}"#
        );
        assert_eq!(
            events[4].data["content_block"]["type"],
            "code_execution_tool_result"
        );
        assert_eq!(
            events[4].data["content_block"]["content"]["type"],
            "code_execution_result"
        );
        assert_eq!(
            events[4].data["content_block"]["content"]["stdout"],
            "HELLO_CHECK\n"
        );
    }

    #[test]
    fn streaming_usage_preserves_cache_breakdown_and_metrics() {
        let payload = request(true, "Use the code execution tool to calculate 17 * 23.");
        let usage = super::super::cache::UsageBreakdown {
            input_tokens: 79,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 34_250,
            cache_creation_5m_input_tokens: 34_250,
            cache_creation_1h_input_tokens: 0,
        };
        let events = build_events(&payload, None, usage, 8);
        let start_usage = &events[0].data["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], 79);
        assert_eq!(start_usage["cache_creation_input_tokens"], 34_250);
        assert_eq!(
            start_usage["cache_creation"]["ephemeral_5m_input_tokens"],
            34_250
        );

        let delta_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .unwrap()
            .data["usage"];
        assert_eq!(delta_usage["cache_creation_input_tokens"], 34_250);
        assert_eq!(delta_usage["server_tool_use"]["code_execution_requests"], 1);

        let metrics = &events.last().unwrap().data["amazon-bedrock-invocationMetrics"];
        assert_eq!(metrics["inputTokenCount"], 79);
        assert_eq!(metrics["cacheWriteInputTokenCount"], 34_250);
    }

    #[tokio::test]
    async fn local_code_execution_holds_impossible_usage_in_every_response_shape() {
        let estimated = super::super::cache::UsageBreakdown {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 999_999,
            cache_creation_5m_input_tokens: 999_999,
            cache_creation_1h_input_tokens: 0,
        };

        let non_stream = handle_request(
            &request(false, "Use the code execution tool to calculate 17 * 23."),
            estimated,
        );
        let bytes = axum::body::to_bytes(non_stream.into_body(), usize::MAX)
            .await
            .expect("non-stream response body");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("code execution JSON response");
        assert!(
            body["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_bdrk_") && id.len() == 61)
        );
        assert_eq!(body["usage"]["input_tokens"], 1);
        assert_eq!(body["usage"]["cache_creation_input_tokens"], 0);
        assert_eq!(body["usage"]["cache_read_input_tokens"], 0);

        let stream = handle_request(
            &request(true, "Use the code execution tool to calculate 17 * 23."),
            estimated,
        );
        let bytes = axum::body::to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("stream response body");
        let body = String::from_utf8(bytes.to_vec()).expect("UTF-8 SSE");
        let events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("SSE JSON"))
            .collect::<Vec<_>>();
        let start = events
            .iter()
            .find(|event| event["type"] == "message_start")
            .expect("message_start");
        let delta = events
            .iter()
            .find(|event| event["type"] == "message_delta")
            .expect("message_delta");
        let metrics = &events
            .iter()
            .find(|event| event["type"] == "message_stop")
            .expect("message_stop")["amazon-bedrock-invocationMetrics"];
        for usage in [&start["message"]["usage"], &delta["usage"]] {
            assert_eq!(usage["input_tokens"], 1);
            assert_eq!(usage["cache_creation_input_tokens"], 0);
            assert_eq!(usage["cache_read_input_tokens"], 0);
        }
        assert_eq!(metrics["inputTokenCount"], 1);
        assert_eq!(metrics["cacheWriteInputTokenCount"], 0);
        assert_eq!(metrics["cacheReadInputTokenCount"], 0);
    }

    #[tokio::test]
    async fn local_code_execution_preserves_normal_cache_breakdown() {
        let usage = super::super::cache::UsageBreakdown {
            input_tokens: 79,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 34_250,
            cache_creation_5m_input_tokens: 34_250,
            cache_creation_1h_input_tokens: 0,
        };
        let response = handle_request(
            &request(false, "Use the code execution tool to calculate 17 * 23."),
            usage,
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("code execution JSON response");

        assert_eq!(body["usage"]["input_tokens"], 79);
        assert_eq!(body["usage"]["cache_creation_input_tokens"], 34_250);
        assert_eq!(body["usage"]["cache_read_input_tokens"], 0);
    }

    #[test]
    fn unsupported_operations_return_standard_server_tool_error() {
        let payload = request(true, "Read a local credentials file.");
        let events = build_events(
            &payload,
            None,
            super::super::cache::UsageBreakdown::flat(12),
            8,
        );
        assert_eq!(
            events[4].data["content_block"]["content"]["error_code"],
            "unavailable"
        );
    }
}
