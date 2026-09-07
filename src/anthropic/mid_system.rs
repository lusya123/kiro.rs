//! Mid-conversation system messages for the legacy Kiro transport.
//!
//! Native Bedrock receives the original JSON. Kiro has only user/assistant
//! turns, so text reminders are appended to the preceding user turn, in place.
//! This is a compatibility rendering, not native system-role priority. Never
//! hoist reminders into the top-level system prompt: that changes past turns
//! and invalidates the entire cached prefix.

use super::types::Message;
use serde_json::{Value, json};

pub(super) fn supports_model(model: &str) -> bool {
    matches!(
        super::converter::map_model(model).as_deref(),
        Some("claude-opus-4.8" | "claude-opus-5")
    )
}

pub(super) fn validate_placement(messages: &[Message]) -> Result<(), String> {
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role != "system" {
            index += 1;
            continue;
        }
        let start = index;
        while index < messages.len() && messages[index].role == "system" {
            index += 1;
        }
        let follows_input = start > 0
            && (messages[start - 1].role == "user"
                || (messages[start - 1].role == "assistant"
                    && messages[start - 1]
                        .content
                        .as_array()
                        .and_then(|b| b.last())
                        .and_then(|b| b.get("type"))
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.ends_with("_tool_result") && t != "tool_result")));
        if !follows_input || (index < messages.len() && messages[index].role != "assistant") {
            return Err(format!(
                "messages.{start}: system messages must follow a user turn or server tool result and precede an assistant turn or end the conversation"
            ));
        }
    }
    Ok(())
}

/// Run before token accounting, signature checks and Kiro conversion so all
/// three see exactly the content that will be forwarded. Read message-level
/// extensions from raw JSON; the typed Message intentionally omits them.
pub(super) fn normalize_legacy(messages: &mut Vec<Message>, raw: &[u8]) -> Result<(), String> {
    if !messages.iter().any(|m| m.role == "system") {
        return Ok(());
    }
    validate_placement(messages)?;
    let raw: Value = serde_json::from_slice(raw)
        .map_err(|_| "Invalid JSON while processing system messages".to_string())?;
    let raw_messages = raw
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?;
    let last_user = messages.iter().rposition(|m| m.role == "user");
    let mut normalized: Vec<Message> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if message.role != "system" {
            normalized.push(message.clone());
            continue;
        }
        let original = &raw_messages[index];
        let scoped = match original.get("clear_at") {
            None => false,
            Some(Value::String(value)) if value == "never" => false,
            Some(Value::String(value)) if value == "next_user_message" => true,
            Some(_) => {
                return Err(format!(
                    "messages.{index}.clear_at: expected never or next_user_message"
                ));
            }
        };
        if original.get("output_config").is_some() {
            return Err(format!(
                "messages.{index}.output_config is not supported by the Kiro transport"
            ));
        }
        let blocks = match &message.content {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            Value::Array(blocks) => blocks.clone(),
            _ => {
                return Err(format!(
                    "messages.{index}.content: system content must be text"
                ));
            }
        };
        for (block_index, block) in blocks.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("text")
                || block.get("text").and_then(Value::as_str).is_none()
            {
                return Err(format!(
                    "messages.{index}.content.{block_index}: the Kiro transport supports text-only mid-conversation system messages"
                ));
            }
            if scoped && block.get("cache_control").is_some() {
                return Err(format!(
                    "messages.{index}.content.{block_index}: cache_control is not permitted on a turn-scoped system message"
                ));
            }
        }
        // Validate even expired reminders, but don't render their text.
        if scoped && last_user.is_some_and(|last| last > index) {
            continue;
        }
        let previous = normalized.last_mut().filter(|m| m.role == "user")
            .ok_or_else(|| format!("messages.{index}: system messages after server tool results require native Bedrock"))?;
        let mut content = match &previous.content {
            Value::String(text) => vec![json!({"type":"text", "text":text})],
            Value::Array(blocks) => blocks.clone(),
            _ => {
                return Err(format!(
                    "messages.{}: user content must be text or content blocks",
                    index - 1
                ));
            }
        };
        for mut block in blocks {
            let text = block["text"].as_str().unwrap();
            block["text"] = Value::String(format!("<system-reminder>\n{text}\n</system-reminder>"));
            content.push(block);
        }
        previous.content = Value::Array(content);
    }
    *messages = normalized;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::{converter::convert_request, types::MessagesRequest};

    fn request(messages: Value) -> (MessagesRequest, Vec<u8>) {
        let raw = serde_json::to_vec(&json!({"model":"claude-opus-5", "max_tokens":64,
            "system":"Stable system prefix", "messages":messages}))
        .unwrap();
        (serde_json::from_slice(&raw).unwrap(), raw)
    }

    #[test]
    fn mid_system_reaches_current_kiro_input_without_rewriting_system_prefix() {
        let (mut req, raw) = request(json!([
            {"role":"user","content":"你好"},
            {"role":"system","content":"Reply with short answers."},
            {"role":"system","content":[{"type":"text","text":"Use Chinese.","cache_control":{"type":"ephemeral"}}]}
        ]));
        normalize_legacy(&mut req.messages, &raw).unwrap();
        assert_eq!(req.system.as_ref().unwrap()[0].text, "Stable system prefix");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(
            req.messages[0].content[2]["cache_control"]["type"],
            "ephemeral"
        );
        let wire = convert_request(&req).unwrap().conversation_state;
        let text = wire.current_message.user_input_message.content;
        assert!(text.contains("你好"));
        assert!(text.contains("<system-reminder>\nReply with short answers."));
        assert!(text.find("Reply with short").unwrap() < text.find("Use Chinese").unwrap());
    }

    #[test]
    fn mid_system_keeps_tool_results_paired_and_in_current_turn() {
        let (mut req, raw) = request(json!([
            {"role":"user","content":"Read the file"},
            {"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"read_file","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"file data"}]},
            {"role":"system","content":"Also check the tests"}
        ]));
        normalize_legacy(&mut req.messages, &raw).unwrap();
        let wire = serde_json::to_value(convert_request(&req).unwrap().conversation_state).unwrap();
        let current = &wire["currentMessage"]["userInputMessage"];
        assert!(
            current["content"]
                .as_str()
                .unwrap()
                .contains("Also check the tests")
        );
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "tool_1"
        );
        assert!(wire["history"].to_string().contains("tool_1"));
    }

    #[test]
    fn mid_system_historical_reminders_stay_at_their_turn() {
        let (mut req, raw) = request(json!([
            {"role":"user","content":[{"type":"text","text":"earlier","cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant","content":"earlier reply"},
            {"role":"user","content":"middle"},
            {"role":"system","content":"later policy"},
            {"role":"assistant","content":"middle reply"},
            {"role":"user","content":"current"}
        ]));
        let prefix = req.messages[..2].to_vec();
        normalize_legacy(&mut req.messages, &raw).unwrap();
        assert_eq!(
            serde_json::to_value(&req.messages[..2]).unwrap(),
            serde_json::to_value(prefix).unwrap()
        );
        let wire = serde_json::to_value(convert_request(&req).unwrap().conversation_state).unwrap();
        assert!(wire["history"].to_string().contains("later policy"));
        assert!(!wire["currentMessage"].to_string().contains("later policy"));
    }

    #[test]
    fn mid_system_clear_at_expires_only_after_a_later_user() {
        let (mut req, raw) = request(json!([
            {"role":"user","content":"earlier"},
            {"role":"system","clear_at":"next_user_message","content":"expired reminder"},
            {"role":"system","clear_at":"never","content":"permanent reminder"},
            {"role":"assistant","content":"reply"},
            {"role":"user","content":"current"},
            {"role":"system","clear_at":"next_user_message","content":"active reminder"}
        ]));
        normalize_legacy(&mut req.messages, &raw).unwrap();
        let wire =
            serde_json::to_string(&convert_request(&req).unwrap().conversation_state).unwrap();
        assert!(!wire.contains("expired reminder"));
        assert!(wire.contains("permanent reminder"));
        assert!(wire.contains("active reminder"));
    }

    #[test]
    fn mid_system_rejects_invalid_placement_and_unsupported_blocks_without_mutation() {
        for messages in [
            json!([{"role":"system","content":"first"},{"role":"user","content":"hello"}]),
            json!([{"role":"user","content":"hello"},{"role":"system","content":"bad"},{"role":"user","content":"next"}]),
            json!([{"role":"user","content":"hello"},{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"read","input":{}}]},{"role":"system","content":"bad"}]),
            json!([{"role":"user","content":"hello"},{"role":"system","content":[{"type":"tool_removal","tool":{"type":"tool_reference","name":"read"}}]}]),
            json!([{"role":"user","content":"hello"},{"role":"system","content":[{"type":"image"}]}]),
            json!([{"role":"user","content":"hello"},{"role":"system","clear_at":"invalid","content":"bad"}]),
        ] {
            let (mut req, raw) = request(messages);
            let before = serde_json::to_value(&req.messages).unwrap();
            assert!(normalize_legacy(&mut req.messages, &raw).is_err());
            assert_eq!(serde_json::to_value(&req.messages).unwrap(), before);
        }
    }

    #[test]
    fn mid_system_plain_requests_are_unchanged() {
        let (mut req, raw) = request(json!([{"role":"user","content":"hello"}]));
        let before = serde_json::to_value(&req).unwrap();
        normalize_legacy(&mut req.messages, &raw).unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), before);
    }
}
