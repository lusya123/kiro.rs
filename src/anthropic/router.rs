//! Anthropic API 路由配置

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::json;

use crate::kiro::provider::KiroProvider;

use super::{
    handlers::{
        count_tokens, count_tokens_public, get_models, head_models, post_messages, post_messages_cc,
    },
    middleware::{AppState, auth_middleware, aws_b40_headers_middleware, cors_layer},
    native_bedrock::BedrockMantleProvider,
    openai_compat::post_chat_completions,
};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建 Anthropic API 路由
///
/// # 端点
/// - `GET /v1/models` - 获取可用模型列表
/// - `POST /v1/messages` - 创建消息（对话）
/// - `POST /v1/messages/count_tokens` - 计算 token 数量
///
/// # 认证
/// 所有 `/v1` 路径需要 API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
///
/// # 参数
/// - `api_key`: API 密钥，用于验证客户端请求
/// - `kiro_provider`: 可选的 KiroProvider，用于调用上游 API
///
/// 创建带有 KiroProvider 的 Anthropic API 路由
#[allow(dead_code)]
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    aws_b40_compat: bool,
) -> Router {
    create_router_with_native_bedrock(
        api_key,
        kiro_provider,
        None,
        extract_thinking,
        aws_b40_compat,
    )
}

pub fn create_router_with_native_bedrock(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    bedrock_mantle_provider: Option<BedrockMantleProvider>,
    extract_thinking: bool,
    aws_b40_compat: bool,
) -> Router {
    let native_bedrock_enabled = bedrock_mantle_provider.is_some();
    let mut state = AppState::new(api_key, extract_thinking, aws_b40_compat);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    if let Some(provider) = bedrock_mantle_provider {
        state = state.with_bedrock_mantle_provider(provider);
    }

    // 需要认证的 /v1 路由
    let count_tokens_route = if aws_b40_compat {
        if native_bedrock_enabled {
            post(count_tokens_public)
        } else {
            post(aws_b_count_tokens_not_found)
        }
    } else {
        post(count_tokens)
    };
    let v1_routes = Router::new()
        .route("/models", get(get_models).head(head_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", count_tokens_route)
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    let router = Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            aws_b40_headers_middleware,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state);

    if aws_b40_compat {
        router
    } else {
        router.layer(cors_layer())
    }
}

async fn aws_b_count_tokens_not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not Found" })))
}

async fn post_responses(State(state): State<AppState>) -> axum::response::Response {
    if !state.aws_b40_compat {
        return StatusCode::NOT_FOUND.into_response();
    }

    let request_id = super::middleware::aws_b40_oneapi_request_id();
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": format!("not implemented (request id: {request_id})"),
                "type": "new_api_error",
                "param": "",
                "code": "convert_request_failed"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::Response;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    async fn spawn_router(aws_b40_compat: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test router");
        let addr = listener.local_addr().expect("test router address");
        let app = create_router_with_provider("test-key", None, true, aws_b40_compat);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test router");
        });
        (format!("http://{addr}"), task)
    }

    #[tokio::test]
    async fn aws_b_router_preserves_models_auth_head_and_options_contract() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        let response = client
            .get(format!("{base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-B models request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["server"], "lyywafcdn");
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        let body = response.text().await.expect("AWS-B models body");
        assert!(body.starts_with("{\"data\":["));
        assert!(body.ends_with("],\"object\":\"list\",\"success\":true}"));
        assert!(body.contains("\"supported_endpoint_types\":[\"anthropic\",\"openai\"]"));
        assert!(body.contains("claude-opus-5"));
        assert!(body.contains("claude-sonnet-5"));

        let response = client
            .head(format!("{base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-B HEAD models request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");
        assert!(response.bytes().await.expect("HEAD body").is_empty());

        let response = client
            .request(reqwest::Method::OPTIONS, format!("{base}/v1/messages"))
            .send()
            .await
            .expect("AWS-B OPTIONS request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");

        let response = client
            .post(format!("{base}/v1/messages"))
            .json(&json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .expect("AWS-B unauthenticated request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");
        assert!(
            response
                .text()
                .await
                .expect("AWS-B auth error body")
                .contains("missing token")
        );

        let response = client
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "invalid-key")
            .json(&json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .expect("AWS-B invalid messages token");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            response
                .text()
                .await
                .expect("AWS-B invalid token body")
                .contains("无效的令牌")
        );

        let response = client
            .get(format!("{base}/v1/models"))
            .header("x-api-key", "invalid-key")
            .send()
            .await
            .expect("AWS-B invalid models token");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .json::<Value>()
                .await
                .expect("AWS-B invalid models body")["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("无效的令牌"))
        );

        let response = client
            .post(format!("{base}/v1/responses"))
            .header("x-api-key", "test-key")
            .json(&json!({"model": "claude-opus-4-8", "input": "hello"}))
            .send()
            .await
            .expect("AWS-B responses compatibility request");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .json::<Value>()
                .await
                .expect("AWS-B responses body")["error"]["code"],
            "convert_request_failed"
        );

        let response = client
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "test-key")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .expect("AWS-B malformed JSON request");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");
        let body: Value = response.json().await.expect("AWS-B malformed JSON body");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|message| message.starts_with("Invalid request: unexpected end"))
        );

        server.abort();
    }

    #[tokio::test]
    async fn profiles_share_token_engine_but_keep_distinct_model_catalogs() {
        let (aws_b_base, aws_b_server) = spawn_router(true).await;
        let (aws_p_base, aws_p_server) = spawn_router(false).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");
        let request = json!({
            "model": "claude-sonnet-4-6",
            "system": [{"type": "text", "text": "You are concise."}],
            "messages": [{"role": "user", "content": "你好, count these tokens."}]
        });

        let response = client
            .post(format!("{aws_b_base}/v1/messages/count_tokens"))
            .header("x-api-key", "test-key")
            .json(&request)
            .send()
            .await
            .expect("AWS-B public count_tokens");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-new-api-version"], "d47d4a8b");
        let body: Value = response
            .json()
            .await
            .expect("AWS-B public count_tokens body");
        assert_eq!(body, json!({ "error": "Not Found" }));

        let response = client
            .post(format!("{aws_b_base}/v1/messages/count_tokens"))
            .header("x-api-key", "test-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await
            .expect("AWS-B malformed public count_tokens");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let aws_b_count: Value = client
            .post(format!("{aws_b_base}/cc/v1/messages/count_tokens"))
            .header("x-api-key", "test-key")
            .json(&request)
            .send()
            .await
            .expect("AWS-B internal count_tokens")
            .json()
            .await
            .expect("AWS-B internal count_tokens body");
        let aws_p_count: Value = client
            .post(format!("{aws_p_base}/v1/messages/count_tokens"))
            .header("x-api-key", "test-key")
            .json(&request)
            .send()
            .await
            .expect("AWS-P count_tokens")
            .json()
            .await
            .expect("AWS-P count_tokens body");
        assert_eq!(aws_b_count, aws_p_count);
        assert!(aws_b_count["input_tokens"].as_i64().is_some_and(|n| n > 0));

        let response = client
            .get(format!("{aws_p_base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-P models request");
        assert_eq!(response.status(), StatusCode::OK);
        let models_body = response.text().await.expect("AWS-P models body");
        assert!(models_body.contains("claude-opus-5"));
        assert!(models_body.contains("claude-sonnet-5"));

        let response = client
            .head(format!("{aws_p_base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-P HEAD models request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert!(response.bytes().await.expect("AWS-P HEAD body").is_empty());

        aws_b_server.abort();
        aws_p_server.abort();
    }

    #[tokio::test]
    async fn native_bedrock_route_preserves_body_and_isolates_authentication() {
        const NATIVE_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        let captured = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
        let captured_for_handler = captured.clone();
        let upstream = Router::new()
            .route(
                "/anthropic/v1/messages",
                post(move |headers: HeaderMap, body: bytes::Bytes| {
                    let captured = captured_for_handler.clone();
                    async move {
                        let value: Value =
                            serde_json::from_slice(&body).expect("native request JSON");
                        *captured.lock().expect("capture lock") = Some((headers, value));
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .header("x-amzn-requestid", "native-request-id")
                            .header("x-native-rate-limit", "preserved")
                            .header(header::CONNECTION, "close")
                            .body(Body::from(NATIVE_SSE))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/anthropic/v1/messages/count_tokens",
                post(|| async { Json(json!({"input_tokens": 42})) }),
            );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind native upstream");
        let upstream_addr = upstream_listener.local_addr().expect("native address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream)
                .await
                .expect("serve native upstream");
        });

        let provider = BedrockMantleProvider::for_test(
            format!("http://{upstream_addr}/anthropic/v1/messages"),
            "native-secret",
            vec!["claude-opus-4-8".to_string()],
        )
        .expect("native provider");
        let app =
            create_router_with_native_bedrock("client-secret", None, Some(provider), true, true);
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind native proxy");
        let app_addr = app_listener.local_addr().expect("proxy address");
        let app_server = tokio::spawn(async move {
            axum::serve(app_listener, app)
                .await
                .expect("serve native proxy");
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("native test client");
        let response = client
            .post(format!("http://{app_addr}/v1/messages"))
            .bearer_auth("client-secret")
            .header("anthropic-version", "2023-06-01")
            .header(
                "anthropic-beta",
                "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14",
            )
            .json(&json!({
                "model": "claude-opus-4-8",
                "max_tokens": 1024,
                "stream": true,
                "temperature": 0.7,
                "custom_extension": {"keep": true},
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("native proxy request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-amzn-requestid"], "native-request-id");
        assert_eq!(response.headers()["x-native-rate-limit"], "preserved");
        assert_eq!(response.headers()[header::CONNECTION], "keep-alive");
        assert_eq!(response.text().await.expect("native body"), NATIVE_SSE);

        let (headers, body) = captured
            .lock()
            .expect("capture lock")
            .take()
            .expect("captured native request");
        assert_eq!(headers["x-api-key"], "native-secret");
        assert!(headers.get(header::AUTHORIZATION).is_none());
        assert_eq!(
            headers["anthropic-beta"],
            "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"
        );
        assert_eq!(body["model"], "anthropic.claude-opus-4-8");
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["custom_extension"]["keep"], true);

        let count_tokens: Value = client
            .post(format!("http://{app_addr}/v1/messages/count_tokens"))
            .header("x-api-key", "client-secret")
            .json(&json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("native count_tokens request")
            .json()
            .await
            .expect("native count_tokens body");
        assert_eq!(count_tokens["input_tokens"], 42);

        let response = client
            .post(format!("http://{app_addr}/v1/messages/count_tokens"))
            .header("x-api-key", "client-secret")
            .json(&json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("non-routed count_tokens request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = client
            .post(format!("http://{app_addr}/v1/messages/count_tokens"))
            .header("x-api-key", "client-secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body("{")
            .send()
            .await
            .expect("malformed count_tokens request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = client
            .post(format!("http://{app_addr}/v1/messages"))
            .header("x-api-key", "client-secret")
            .json(&json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}],
                "output_config": {
                    "effort": "high",
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object"}
                    }
                }
            }))
            .send()
            .await
            .expect("structured-output fallback request");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = client
            .post(format!("http://{app_addr}/v1/messages"))
            .header("x-api-key", "client-secret")
            .json(&json!({
                "model": "claude-sonnet-4-6",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("non-routed request");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        app_server.abort();
        upstream_server.abort();
    }
}
