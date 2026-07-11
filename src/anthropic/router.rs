//! Anthropic API 路由配置

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::kiro::provider::KiroProvider;

use super::{
    handlers::{count_tokens, get_models, head_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, aws_b40_headers_middleware, cors_layer},
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
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    aws_b40_compat: bool,
) -> Router {
    let mut state = AppState::new(api_key, extract_thinking, aws_b40_compat);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models).head(head_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
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

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            aws_b40_headers_middleware,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use serde_json::{Value, json};

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
    async fn aws_b_router_preserves_models_auth_head_and_cors_contract() {
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
        assert_eq!(response.headers()["x-new-api-version"], "83c64fa5");
        let body = response.text().await.expect("AWS-B models body");
        assert!(body.contains("\"first_id\":\"claude-haiku-4-5\""));
        assert!(!body.contains("claude-sonnet-5"));

        let response = client
            .head(format!("{base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-B HEAD models request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-new-api-version"], "0b8be5cf");
        assert!(response.bytes().await.expect("HEAD body").is_empty());

        let response = client
            .request(reqwest::Method::OPTIONS, format!("{base}/v1/messages"))
            .send()
            .await
            .expect("AWS-B OPTIONS request");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");

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
        assert_eq!(response.headers()["x-new-api-version"], "0b8be5cf");
        assert!(
            response
                .text()
                .await
                .expect("AWS-B auth error body")
                .contains("missing token")
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

        let aws_b_count: Value = client
            .post(format!("{aws_b_base}/v1/messages/count_tokens"))
            .header("x-api-key", "test-key")
            .json(&request)
            .send()
            .await
            .expect("AWS-B count_tokens")
            .json()
            .await
            .expect("AWS-B count_tokens body");
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
        assert!(
            response
                .text()
                .await
                .expect("AWS-P models body")
                .contains("claude-sonnet-5")
        );

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
}
