//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Method, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::types::ErrorResponse;

const AWS_B40_GATEWAY_VERSION: &str = "8840a103";

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥
    pub api_key: String,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 是否启用 AWS-B-40 外观兼容模式
    pub aws_b40_compat: bool,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(api_key: impl Into<String>, extract_thinking: bool, aws_b40_compat: bool) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            extract_thinking,
            aws_b40_compat,
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }
}

/// API Key 认证中间件
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let supplied_key = auth::extract_api_key(&request);
    match supplied_key.as_deref() {
        Some(key) if auth::constant_time_eq(key, &state.api_key) => next.run(request).await,
        _ => {
            if state.aws_b40_compat {
                let request_id = aws_b40_oneapi_request_id();
                if is_messages_path(&path) {
                    let (status, message) = if supplied_key.is_some() {
                        (StatusCode::FORBIDDEN, "无效的令牌")
                    } else {
                        (StatusCode::UNAUTHORIZED, "missing token")
                    };
                    let body =
                        format!("{{\"error\":\"{} (request id: {})\"}}", message, request_id);
                    let mut response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                        .body(Body::from(body))
                        .unwrap();
                    apply_aws_b40_headers(response.headers_mut(), &request_id);
                    return response;
                }

                let body = json!({
                    "error": {
                        "code": "",
                        "message": format!(
                            "{} (request id: {request_id})",
                            if supplied_key.is_some() { "无效的令牌" } else { "未提供令牌" }
                        ),
                        "type": "new_api_error"
                    }
                });
                let mut response = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
                apply_aws_b40_headers(response.headers_mut(), &request_id);
                return response;
            }

            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

/// AWS-B-40 响应头兼容层。
pub async fn aws_b40_headers_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    if state.aws_b40_compat && request.method() == Method::OPTIONS {
        let request_id = aws_b40_oneapi_request_id();
        let mut response =
            (StatusCode::NOT_FOUND, Json(json!({ "error": "Not Found" }))).into_response();
        apply_aws_b40_headers_with_version(
            response.headers_mut(),
            &request_id,
            AWS_B40_GATEWAY_VERSION,
        );
        return response;
    }

    let mut response = next.run(request).await;
    if state.aws_b40_compat {
        let messages_success = is_gateway_completion_path(&path) && response.status().is_success();
        let messages_stream_success = messages_success && is_stream_response(&response);
        let request_id = response
            .headers()
            .get("x-oneapi-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if messages_success && !messages_stream_success {
                    aws_b40_messages_success_request_id()
                } else {
                    aws_b40_oneapi_request_id()
                }
            });
        let version = aws_b40_version_for_response(&method, &path, &response);
        apply_aws_b40_headers_with_version(response.headers_mut(), &request_id, version);

        if messages_success && !messages_stream_success {
            apply_aws_b40_non_stream_success_headers(response.headers_mut());
        }
    } else {
        let include_official_headers = path.ends_with("/messages");
        let is_stream = is_stream_response(&response);
        let status = response.status();
        super::compat::add_response_headers(
            response.headers_mut(),
            status,
            is_stream,
            include_official_headers,
        );
    }
    response
}

pub fn aws_b40_oneapi_request_id() -> String {
    let now = chrono::Utc::now().format("%Y%m%d%H%M%S");
    format!("{now}{}{}", random_digits(9), random_base62(8))
}

fn aws_b40_messages_success_request_id() -> String {
    aws_b40_upstream_request_id()
}

pub(crate) fn aws_b40_upstream_request_id() -> String {
    let now = chrono::Utc::now().format("%Y%m%d%H%M%S");
    format!("{now}{}8268d9d6{}", random_digits(9), random_base62(8))
}

fn random_base62(len: usize) -> String {
    const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| BASE62[fastrand::usize(..BASE62.len())] as char)
        .collect()
}

fn random_digits(len: usize) -> String {
    (0..len)
        .map(|i| {
            let start = if i == 0 { 1 } else { 0 };
            char::from(b'0' + fastrand::u8(start..=9))
        })
        .collect()
}

pub(crate) fn apply_aws_b40_headers(headers: &mut header::HeaderMap, request_id: &str) {
    apply_aws_b40_headers_with_version(headers, request_id, AWS_B40_GATEWAY_VERSION);
}

pub(crate) fn apply_aws_b40_headers_with_version(
    headers: &mut header::HeaderMap,
    request_id: &str,
    version: &'static str,
) {
    headers.insert("x-new-api-version", HeaderValue::from_static(version));
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert("x-oneapi-request-id", value);
    }
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    headers.insert(header::SERVER, HeaderValue::from_static("lyywafcdn"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
}

fn apply_aws_b40_non_stream_success_headers(headers: &mut header::HeaderMap) {
    headers.insert(header::VIA, HeaderValue::from_static("1.1 Caddy"));
    headers.insert(
        header::ALT_SVC,
        HeaderValue::from_static("h3=\":443\"; ma=2592000"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
}

fn aws_b40_version_for_response(method: &Method, path: &str, response: &Response) -> &'static str {
    if *method == Method::OPTIONS || *method == Method::HEAD {
        return AWS_B40_GATEWAY_VERSION;
    }

    if is_gateway_completion_path(path) {
        if response.status().is_success() {
            if is_stream_response(response) {
                AWS_B40_GATEWAY_VERSION
            } else {
                "20260501R2"
            }
        } else {
            AWS_B40_GATEWAY_VERSION
        }
    } else {
        AWS_B40_GATEWAY_VERSION
    }
}

fn is_messages_path(path: &str) -> bool {
    path == "/messages" || path == "/v1/messages" || path == "/cc/v1/messages"
}

fn is_gateway_completion_path(path: &str) -> bool {
    is_messages_path(path) || path == "/chat/completions" || path == "/v1/chat/completions"
}

fn is_stream_response(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: StatusCode, content_type: &str) -> Response {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn aws_b_versions_distinguish_messages_and_streams() {
        let non_stream = response(StatusCode::OK, "application/json");
        assert_eq!(
            aws_b40_version_for_response(&Method::POST, "/v1/messages", &non_stream),
            "20260501R2"
        );

        let stream = response(StatusCode::OK, "text/event-stream");
        assert_eq!(
            aws_b40_version_for_response(&Method::POST, "/v1/messages", &stream),
            AWS_B40_GATEWAY_VERSION
        );

        let models = response(StatusCode::OK, "application/json");
        assert_eq!(
            aws_b40_version_for_response(&Method::GET, "/v1/models", &models),
            AWS_B40_GATEWAY_VERSION
        );

        let chat = response(StatusCode::OK, "application/json");
        assert_eq!(
            aws_b40_version_for_response(&Method::POST, "/v1/chat/completions", &chat),
            "20260501R2"
        );
    }

    #[test]
    fn aws_b_headers_keep_bedrock_gateway_identity() {
        let mut headers = header::HeaderMap::new();
        apply_aws_b40_headers(&mut headers, "request-123");

        assert_eq!(headers["server"], "lyywafcdn");
        assert_eq!(headers["x-oneapi-request-id"], "request-123");
        assert_eq!(headers["x-new-api-version"], AWS_B40_GATEWAY_VERSION);
        assert_eq!(headers["x-accel-buffering"], "no");
    }

    #[test]
    fn aws_b_non_stream_success_id_and_headers_match_gateway_shape() {
        let request_id = aws_b40_messages_success_request_id();
        assert_eq!(request_id.len(), 39);
        assert!(
            request_id[..23]
                .chars()
                .all(|character| character.is_ascii_digit())
        );
        assert_eq!(&request_id[23..31], "8268d9d6");
        assert!(
            request_id[31..]
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );

        let mut headers = header::HeaderMap::new();
        apply_aws_b40_non_stream_success_headers(&mut headers);
        assert_eq!(headers["via"], "1.1 Caddy");
        assert!(headers.get("vary").is_none());
        assert_eq!(headers["alt-svc"], "h3=\":443\"; ma=2592000");
        assert_eq!(
            headers["referrer-policy"],
            "strict-origin-when-cross-origin"
        );
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "SAMEORIGIN");
    }
}
