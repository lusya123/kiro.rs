//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// Native model options advertised by newer Kiro model catalogs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// Model-specific request options accepted at the Kiro request root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdditionalModelRequestFields {
    #[serde(rename = "output_config")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<KiroOutputConfig>,
}

/// Native reasoning effort accepted by supported Kiro models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KiroOutputConfig {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub effort: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {}
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
        assert!(request.additional_model_request_fields.is_none());
    }

    #[test]
    fn serializes_native_reasoning_effort_at_request_root() {
        let request: KiroRequest = serde_json::from_str(
            r#"{
                "conversationState": {
                    "conversationId": "conv-456",
                    "currentMessage": {
                        "userInputMessage": {
                            "content": "Test message",
                            "modelId": "claude-opus-4.8",
                            "userInputMessageContext": {}
                        }
                    }
                },
                "additionalModelRequestFields": {
                    "output_config": {"effort":"xhigh"}
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            request
                .additional_model_request_fields
                .as_ref()
                .and_then(|fields| fields.output_config.as_ref())
                .map(|config| config.effort.as_str()),
            Some("xhigh")
        );
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(
            wire["additionalModelRequestFields"]["output_config"]["effort"],
            "xhigh"
        );
    }
}
