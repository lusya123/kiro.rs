//! 事件基础定义
//!
//! 定义事件类型枚举、trait 和统一事件结构

use crate::kiro::parser::error::{ParseError, ParseResult};
use crate::kiro::parser::frame::Frame;

/// 事件类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// 助手响应事件
    AssistantResponse,
    /// 原生 reasoning 事件
    ReasoningContent,
    /// 工具使用事件
    ToolUse,
    /// 计费事件
    Metering,
    /// 精确 token 计量事件
    Metadata,
    /// 上下文使用率事件
    ContextUsage,
    /// 上游拒绝当前会话状态；这是终止错误，不能当作未知事件忽略。
    InvalidState,
    /// 未知事件类型
    Unknown,
}

impl EventType {
    /// 从事件类型字符串解析
    pub fn from_str(s: &str) -> Self {
        match s {
            "assistantResponseEvent" => Self::AssistantResponse,
            "reasoningContentEvent" => Self::ReasoningContent,
            "toolUseEvent" => Self::ToolUse,
            "meteringEvent" => Self::Metering,
            "metadataEvent" => Self::Metadata,
            "contextUsageEvent" => Self::ContextUsage,
            "invalidStateEvent" => Self::InvalidState,
            _ => Self::Unknown,
        }
    }

    /// 转换为事件类型字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AssistantResponse => "assistantResponseEvent",
            Self::ReasoningContent => "reasoningContentEvent",
            Self::ToolUse => "toolUseEvent",
            Self::Metering => "meteringEvent",
            Self::Metadata => "metadataEvent",
            Self::ContextUsage => "contextUsageEvent",
            Self::InvalidState => "invalidStateEvent",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 事件 payload trait
///
/// 所有具体事件类型都需要实现此 trait
pub trait EventPayload: Sized {
    /// 从帧解析事件负载
    fn from_frame(frame: &Frame) -> ParseResult<Self>;
}

/// 统一事件枚举
///
/// 封装所有可能的事件类型
#[derive(Debug, Clone)]
pub enum Event {
    /// 助手响应
    AssistantResponse(super::AssistantResponseEvent),
    /// 模型原生 reasoning 响应
    ReasoningContent(super::ReasoningContentEvent),
    /// 工具使用
    ToolUse(super::ToolUseEvent),
    /// 计费
    Metering(super::MeteringEvent),
    /// Exact token accounting from newer Kiro runtimes.
    Metadata(super::MetadataEvent),
    /// 上下文使用率
    ContextUsage(super::ContextUsageEvent),
    /// 当前会话状态被上游拒绝。
    InvalidState(super::InvalidStateEvent),
    /// 未知事件（保留事件类型与原始 payload，便于新模型协议前向兼容）
    Unknown {
        event_type: String,
        payload: Vec<u8>,
    },
    /// 服务端错误
    Error {
        /// 错误代码
        error_code: String,
        /// 错误消息
        error_message: String,
    },
    /// 服务端异常
    Exception {
        /// 异常类型
        exception_type: String,
        /// 异常消息
        message: String,
    },
}

impl Event {
    /// 从帧解析事件
    pub fn from_frame(frame: Frame) -> ParseResult<Self> {
        let message_type = frame.message_type().unwrap_or("event");

        match message_type {
            "event" => Self::parse_event(frame),
            "error" => Self::parse_error(frame),
            "exception" => Self::parse_exception(frame),
            other => Err(ParseError::InvalidMessageType(other.to_string())),
        }
    }

    /// 解析事件类型消息
    fn parse_event(frame: Frame) -> ParseResult<Self> {
        let event_type_str = frame.event_type().unwrap_or("unknown");
        let event_type = EventType::from_str(event_type_str);

        match event_type {
            EventType::AssistantResponse => {
                let payload = super::AssistantResponseEvent::from_frame(&frame)?;
                Ok(Self::AssistantResponse(payload))
            }
            EventType::ReasoningContent => {
                let payload = super::ReasoningContentEvent::from_frame(&frame)?;
                Ok(Self::ReasoningContent(payload))
            }
            EventType::ToolUse => {
                let payload = super::ToolUseEvent::from_frame(&frame)?;
                Ok(Self::ToolUse(payload))
            }
            EventType::Metering => {
                let payload = super::MeteringEvent::from_frame(&frame)?;
                Ok(Self::Metering(payload))
            }
            EventType::Metadata => {
                let payload = super::MetadataEvent::from_frame(&frame)?;
                Ok(Self::Metadata(payload))
            }
            EventType::ContextUsage => {
                let payload = super::ContextUsageEvent::from_frame(&frame)?;
                Ok(Self::ContextUsage(payload))
            }
            EventType::InvalidState => {
                let payload = super::InvalidStateEvent::from_frame(&frame)?;
                Ok(Self::InvalidState(payload))
            }
            EventType::Unknown => Ok(Self::Unknown {
                event_type: event_type_str.to_string(),
                payload: frame.payload,
            }),
        }
    }

    /// 解析错误类型消息
    fn parse_error(frame: Frame) -> ParseResult<Self> {
        let error_code = frame
            .headers
            .error_code()
            .unwrap_or("UnknownError")
            .to_string();
        let error_message = frame.payload_as_str();

        Ok(Self::Error {
            error_code,
            error_message,
        })
    }

    /// 解析异常类型消息
    fn parse_exception(frame: Frame) -> ParseResult<Self> {
        let exception_type = frame
            .headers
            .exception_type()
            .unwrap_or("UnknownException")
            .to_string();
        let message = frame.payload_as_str();

        Ok(Self::Exception {
            exception_type,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::parser::header::{HeaderValue, Headers};

    #[test]
    fn test_event_type_from_str() {
        assert_eq!(
            EventType::from_str("assistantResponseEvent"),
            EventType::AssistantResponse
        );
        assert_eq!(
            EventType::from_str("reasoningContentEvent"),
            EventType::ReasoningContent
        );
        assert_eq!(EventType::from_str("toolUseEvent"), EventType::ToolUse);
        assert_eq!(EventType::from_str("meteringEvent"), EventType::Metering);
        assert_eq!(EventType::from_str("metadataEvent"), EventType::Metadata);
        assert_eq!(
            EventType::from_str("contextUsageEvent"),
            EventType::ContextUsage
        );
        assert_eq!(
            EventType::from_str("invalidStateEvent"),
            EventType::InvalidState
        );
        assert_eq!(EventType::from_str("unknown_type"), EventType::Unknown);
    }

    #[test]
    fn test_event_type_as_str() {
        assert_eq!(
            EventType::AssistantResponse.as_str(),
            "assistantResponseEvent"
        );
        assert_eq!(
            EventType::ReasoningContent.as_str(),
            "reasoningContentEvent"
        );
        assert_eq!(EventType::ToolUse.as_str(), "toolUseEvent");
        assert_eq!(EventType::Metering.as_str(), "meteringEvent");
        assert_eq!(EventType::Metadata.as_str(), "metadataEvent");
        assert_eq!(EventType::ContextUsage.as_str(), "contextUsageEvent");
        assert_eq!(EventType::InvalidState.as_str(), "invalidStateEvent");
    }

    #[test]
    fn metadata_event_preserves_stop_reason_without_token_usage() {
        let mut headers = Headers::new();
        headers.insert(
            ":message-type".to_string(),
            HeaderValue::String("event".to_string()),
        );
        headers.insert(
            ":event-type".to_string(),
            HeaderValue::String("metadataEvent".to_string()),
        );

        let event = Event::from_frame(Frame {
            headers,
            payload: br#"{"stopReason":"END_TURN","futureField":true}"#.to_vec(),
        })
        .expect("live metadata payload should parse");

        match event {
            Event::Metadata(metadata) => {
                assert_eq!(metadata.stop_reason.as_deref(), Some("END_TURN"));
                assert!(!metadata.token_usage.is_present());
            }
            other => panic!("expected metadata event, got {other:?}"),
        }
    }

    #[test]
    fn invalid_state_event_is_parsed_as_a_terminal_event() {
        let mut headers = Headers::new();
        headers.insert(
            ":message-type".to_string(),
            HeaderValue::String("event".to_string()),
        );
        headers.insert(
            ":event-type".to_string(),
            HeaderValue::String("invalidStateEvent".to_string()),
        );

        let event = Event::from_frame(Frame {
            headers,
            payload: br#"{"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD","message":"too large"}"#
                .to_vec(),
        })
        .expect("invalid state payload should parse");

        match event {
            Event::InvalidState(invalid) => {
                assert_eq!(invalid.reason, "CONTENT_LENGTH_EXCEEDS_THRESHOLD");
                assert_eq!(invalid.message, "too large");
            }
            other => panic!("expected invalid state event, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_preserves_type_and_payload() {
        let mut headers = Headers::new();
        headers.insert(
            ":message-type".to_string(),
            HeaderValue::String("event".to_string()),
        );
        headers.insert(
            ":event-type".to_string(),
            HeaderValue::String("thinkingMetadataEvent".to_string()),
        );
        let payload = br#"{"signature":"opaque","tokenCount":42}"#.to_vec();

        let event = Event::from_frame(Frame {
            headers,
            payload: payload.clone(),
        })
        .expect("unknown event should remain parseable");

        match event {
            Event::Unknown {
                event_type,
                payload: actual,
            } => {
                assert_eq!(event_type, "thinkingMetadataEvent");
                assert_eq!(actual, payload);
            }
            other => panic!("expected unknown event, got {other:?}"),
        }
    }
}
