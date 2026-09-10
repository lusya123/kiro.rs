//! Anthropic API 路由配置

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
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
    responses_compat::post_responses,
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
    // 保留本地结构化输出等兼容扩展；/v1 的 AWS-B Kiro 路由执行公共能力校验。
    // 流式事件会先缓冲；输入 usage 仍是同一本地快照。
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::Response;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn pomo_opus_validation_matches_across_messages_cc_and_chat() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for model in ["claude-opus-5", "claude-opus-4-8"] {
            for path in ["/v1/messages", "/cc/v1/messages", "/v1/chat/completions"] {
                for stream in [false, true] {
                    for (extra, detail) in [
                        (json!({"temperature":1.1}), "temperature: range: 0..1"),
                        (json!({"messages":[{"role":"invalid","content":"hello"}]}), "Unexpected role"),
                        (json!({"tools":[{"type":"advisor_20260301","name":"advisor"}]}), "tool type 'advisor_20260301' is not supported"),
                    ] {
                        let mut body = json!({"model":model,"stream":stream,"max_tokens":128,
                            "messages":[{"role":"user","content":"hello"}]});
                        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                        let response = client.post(format!("{base}{path}"))
                            .header("x-api-key","test-key").json(&body).send().await.unwrap();
                        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path} {model} {extra}");
                        assert!(response.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
                        let error: Value = response.json().await.unwrap();
                        let expected = if path.ends_with("completions") { "upstream_error" } else { "<nil>" };
                        assert_eq!(error["error"]["type"],expected,"{path}: {error}");
                        let message = error["error"]["message"].as_str().unwrap();
                        assert!(message.contains(detail),"{message}");
                        let operation = if stream { "InvokeModelWithResponseStream" } else { "InvokeModel" };
                        assert!(message.starts_with(operation),"{message}");
                    }
                }
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn aws_b_opus_messages_reject_fallbacks_before_upstream() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for model in ["claude-opus-5", "claude-opus-4-8"] {
            for path in ["/v1/messages", "/cc/v1/messages"] {
                for stream in [false, true] {
                    for beta in [false, true] {
                        for fallback in [
                            Some(json!("default")), Some(json!("invalid")), Some(json!([])),
                            Some(json!(["not-a-real-model"])), Some(json!({"model":"claude-opus-4-8"})),
                            Some(json!(false)), Some(Value::Null), None,
                        ] {
                            let requested = fallback.as_ref().is_some_and(|value| !value.is_null());
                            let mut body = json!({"model":model,"stream":stream,"max_tokens":128,
                                "messages":[{"role":"user","content":"hello"}]});
                            if let Some(value) = fallback { body["fallbacks"] = value; }
                            let mut request = client.post(format!("{base}{path}"))
                                .header("x-api-key", "test-key");
                            if beta { request = request.header("anthropic-beta", "server-side-fallback-2026-07-01"); }
                            let response = request.json(&body).send().await.unwrap();
                            if requested {
                                assert_eq!(response.status(), StatusCode::BAD_REQUEST,
                                    "{path} {model} stream={stream} beta={beta}: {body}");
                                assert!(response.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
                                let error: Value = response.json().await.unwrap();
                                assert_eq!(error["type"], "error");
                                assert_eq!(error["error"]["type"], "invalid_request_error");
                                assert_eq!(error["error"]["message"], "`fallbacks` is not supported on Amazon Bedrock");
                            } else {
                                // No fallback (including null) must still reach the
                                // provider, even with the beta header alone.
                                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE,
                                    "{path} {model} stream={stream} beta={beta}: {body}");
                            }
                        }
                    }
                }
            }
        }
        server.abort();
    }

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
        assert!(response.headers().get("server").is_none());
        assert!(response.headers().get("x-new-api-version").is_none());
        assert!(response.headers().get("x-oneapi-request-id").is_none());
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
        assert!(body.contains("\"id\":\"gpt-5.6\""));
        assert!(body.contains("gpt-5.6-sol"));
        assert!(body.contains("gpt-5.6-terra"));
        assert!(body.contains("gpt-5.6-luna"));

        let response = client
            .get(format!("{base}/v1/models?client_version=0.146.0"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("Codex model catalog request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.expect("Codex models JSON"),
            json!({"models": []})
        );

        let response = client
            .head(format!("{base}/v1/models"))
            .header("x-api-key", "test-key")
            .send()
            .await
            .expect("AWS-B HEAD models request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get("x-new-api-version").is_none());
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
        assert!(response.headers().get("x-new-api-version").is_none());

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
        assert!(response.headers().get("x-new-api-version").is_none());
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
        assert!(response.headers().get("x-new-api-version").is_none());
        let body: Value = response.json().await.expect("AWS-B malformed JSON body");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|message| message.starts_with("Invalid request: unexpected end"))
        );

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_message_entrypoints_share_opus_48_sampling_validation() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            for invalid in [json!({"temperature": 0.7}), json!({"top_k": 1})] {
                let mut body = json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "hello"}]
                });
                body.as_object_mut()
                    .unwrap()
                    .extend(invalid.as_object().unwrap().clone());

                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&body)
                    .send()
                    .await
                    .expect("invalid sampling request");
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}: {body}");
                let error: Value = response.json().await.expect("sampling error JSON");
                assert_eq!(error["error"]["type"], "invalid_request_error");
            }

            for compatible in [json!({}), json!({"temperature": 1, "top_p": 0.99})] {
                let mut body = json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "hello"}]
                });
                body.as_object_mut()
                    .unwrap()
                    .extend(compatible.as_object().unwrap().clone());

                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&body)
                    .send()
                    .await
                    .expect("compatible sampling request");
                assert_eq!(
                    response.status(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "{path}: compatible sampling must pass validation and reach the absent provider"
                );
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_message_entrypoints_reject_a_leading_system_role() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            for model in [
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
            ] {
                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&json!({
                        "model": model,
                        "max_tokens": 200,
                        "messages": [
                            {"role": "system", "content": "This system message is in the wrong location."},
                            {"role": "user", "content": "Reply in one sentence."}
                        ]
                    }))
                    .send()
                    .await
                    .expect("leading system role request");

                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{path}: {model}"
                );
                let body: Value = response.json().await.expect("validation error JSON");
                assert_eq!(body["type"], "error", "{path}: {model}");
                assert_eq!(
                    body["error"]["type"], "invalid_request_error",
                    "{path}: {model}"
                );
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_message_entrypoints_reject_modern_claude_assistant_prefill() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            for model in [
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
            ] {
                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&json!({
                        "model": model,
                        "max_tokens": 200,
                        "messages": [
                            {"role": "user", "content": "Return a JSON object containing only an ok field."},
                            {"role": "assistant", "content": "{"}
                        ]
                    }))
                    .send()
                    .await
                    .expect("assistant prefill request");

                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{path}: {model}"
                );
                let body: Value = response.json().await.expect("validation error JSON");
                assert_eq!(body["type"], "error", "{path}: {model}");
                assert_eq!(
                    body["error"]["type"], "invalid_request_error",
                    "{path}: {model}"
                );
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_message_entrypoints_reject_unknown_message_roles() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            for model in [
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
            ] {
                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&json!({
                        "model": model,
                        "max_tokens": 64,
                        "messages": [
                            {"role": "developer", "content": "This role is not valid here."},
                            {"role": "user", "content": "hello"}
                        ]
                    }))
                    .send()
                    .await
                    .expect("unknown role request");

                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{path}: {model}"
                );
                let body: Value = response.json().await.expect("validation error JSON");
                assert_eq!(body["type"], "error", "{path}: {model}");
                assert_eq!(
                    body["error"]["type"], if matches!(model,"claude-opus-5"|"claude-opus-4-8") { "<nil>" } else { "invalid_request_error" },
                    "{path}: {model}"
                );
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn external_message_entrypoints_ignore_malformed_thinking_signatures() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            let response = client
                .post(format!("{base}{path}"))
                .header("x-api-key", "test-key")
                .json(&json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "messages": [
                        {"role": "user", "content": "first"},
                        {"role": "assistant", "content": [
                            {"type": "thinking", "thinking": "test", "signature": "not-base64!!"},
                            {"type": "text", "text": "answer"}
                        ]},
                        {"role": "user", "content": "continue"}
                    ]
                }))
                .send()
                .await
                .expect("malformed signature request");

            // No provider is configured: 503 proves signature metadata did not block ingress.
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let body: Value = response.json().await.expect("validation error JSON");
            assert_eq!(body["type"], "error", "{path}");
            assert_eq!(body["error"]["type"], "service_unavailable", "{path}");
        }

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_message_entrypoints_reject_temperature_above_one_for_all_claude_models() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for path in ["/v1/messages", "/cc/v1/messages"] {
            for model in [
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
            ] {
                let response = client
                    .post(format!("{base}{path}"))
                    .header("x-api-key", "test-key")
                    .json(&json!({
                        "model": model,
                        "max_tokens": 64,
                        "temperature": 1.01,
                        "messages": [{"role": "user", "content": "hello"}]
                    }))
                    .send()
                    .await
                    .expect("out-of-range temperature request");

                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{path}: {model}"
                );
                let body: Value = response.json().await.expect("validation error JSON");
                assert_eq!(
                    body["error"]["type"], if matches!(model,"claude-opus-5"|"claude-opus-4-8") { "<nil>" } else { "invalid_request_error" },
                    "{path}: {model}: {body}"
                );
            }
        }

        server.abort();
    }

    #[tokio::test]
    async fn aws_b_manual_thinking_46_reaches_provider_after_budget_validation() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for path in ["/v1/messages", "/cc/v1/messages"] {
            for model in ["claude-sonnet-4-6", "claude-opus-4-6"] {
                for budget in [1024, 2048, 4096] {
                    let response = client.post(format!("{base}{path}"))
                        .header("x-api-key", "test-key")
                        .json(&json!({"model": model, "max_tokens": budget + 1024,
                            "thinking": {"type": "enabled", "budget_tokens": budget},
                            "messages": [{"role": "user", "content": "Calculate 19+23."}]}))
                        .send().await.unwrap();
                    // This router has no upstream provider. Reaching that boundary
                    // distinguishes accepted parameters from a premature local 400.
                    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path} {model} {budget}");
                }
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn schema_output_is_an_extension_of_the_cc_endpoint() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for path in ["/v1/messages", "/cc/v1/messages"] {
            let response = client.post(format!("{base}{path}"))
                .header("x-api-key", "test-key")
                .json(&json!({
                    "model": "claude-sonnet-4-6", "max_tokens": 512,
                    "messages": [{"role": "user", "content": "Return the requested JSON object."}],
                    "output_config": {"format": {"type": "json_schema", "schema": {
                        "type": "object", "properties": {"Kiro": {"type": "string"}},
                        "required": ["Kiro"], "additionalProperties": false
                    }}}
                })).send().await.unwrap();
            let expected = if path == "/v1/messages" {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            assert_eq!(response.status(), expected, "{path}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn aws_b_public_messages_rejects_unavailable_server_side_features() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        let cases = [
            (
                "server-side fallback",
                json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "fallbacks": "default",
                    "messages": [{"role": "user", "content": "hello"}]
                }),
            ),
            (
                "web search server tool",
                json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "tools": [{"type": "web_search_20250305", "name": "web_search"}],
                    "messages": [{"role": "user", "content": "search the web"}]
                }),
            ),
            (
                "advisor server tool",
                json!({
                    "model": "claude-sonnet-5",
                    "max_tokens": 64,
                    "tools": [{"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-5"}],
                    "messages": [{"role": "user", "content": "design a worker pool"}]
                }),
            ),
            (
                "code execution server tool",
                json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 64,
                    "tools": [{"type": "code_execution_20260521", "name": "code_execution"}],
                    "messages": [{"role": "user", "content": "Use code execution to calculate 17+25."}]
                }),
            ),
            (
                "manual enabled thinking",
                json!({
                    "model": "claude-opus-5",
                    "max_tokens": 2048,
                    "thinking": {"type": "enabled", "budget_tokens": 1024},
                    "messages": [{"role": "user", "content": "Calculate 17+25."}]
                }),
            ),
            (
                "URL image source",
                json!({
                    "model": "claude-opus-5",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": [
                        {"type": "image", "source": {"type": "url", "url": "https://example.com/image.png"}},
                        {"type": "text", "text": "Describe this image."}
                    ]}]
                }),
            ),
        ];

        for (case, body) in cases {
            let response = client
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", "test-key")
                .header("anthropic-beta", "server-side-fallback-2026-07-01")
                .json(&body)
                .send()
                .await
                .expect("unsupported public feature request");

            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}: {body}");
            let error: Value = response.json().await.expect("validation error JSON");
            assert_eq!(
                error["error"]["type"], if case != "server-side fallback" && body["model"] != "claude-sonnet-5" { "<nil>" } else { "invalid_request_error" },
                "{case}: {error}"
            );
        }

        server.abort();
    }

    #[tokio::test]
    async fn opus_public_capability_validation_is_independent_of_other_native_routes() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for hybrid in [false, true] {
            let native = hybrid.then(|| BedrockMantleProvider::for_test(
                "http://127.0.0.1:1/anthropic/v1/messages".to_string(),
                "unused-native-key", vec!["claude-sonnet-4-6".to_string()],
            ).unwrap());
            let app = create_router_with_native_bedrock("test-key", None, native, true, true);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            for model in ["claude-opus-5", "claude-opus-4-8"] {
                for stream in [false, true] {
                    for extra in [
                        json!({"fallbacks":"default"}),
                        json!({"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
                        json!({"tools":[{"type":"advisor_20260301","name":"advisor"}]}),
                        json!({"tools":[{"type":"code_execution_20260521","name":"code_execution"}]}),
                        json!({"max_tokens":2048,"thinking":{"type":"enabled","budget_tokens":1024}}),
                        json!({"output_config":{"format":{"type":"json_schema","schema":{
                            "type":"object","properties":{},"additionalProperties":false}}}}),
                        json!({"messages":[{"role":"user","content":[{"type":"image",
                            "source":{"type":"url","url":"https://example.com/image.png"}}]}]}),
                    ] {
                        let mut body = json!({"model":model,"max_tokens":256,"stream":stream,
                            "messages":[{"role":"user","content":"hello"}]});
                        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                        let response = client.post(format!("http://{addr}/v1/messages"))
                            .header("x-api-key","test-key").json(&body).send().await.unwrap();
                        assert_eq!(response.status(), StatusCode::BAD_REQUEST,
                            "hybrid={hybrid} model={model} stream={stream} extra={extra}");
                        let error: Value = response.json().await.unwrap();
                        assert_eq!(error["error"]["type"], if extra.get("fallbacks").is_some() { "invalid_request_error" } else { "<nil>" });
                    }
                }
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn aws_b_public_messages_keeps_normal_questions_and_client_tools_available() {
        let (base, server) = spawn_router(true).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client");

        for (case, body) in [
            (
                "ordinary coding question",
                json!({
                    "model": "claude-sonnet-4-6",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "Write a Rust function that adds two integers."}]
                }),
            ),
            (
                "client-defined code tool",
                json!({
                    "model": "claude-sonnet-4-6",
                    "max_tokens": 64,
                    "tools": [{
                        "name": "run_tests",
                        "description": "Run the project's tests",
                        "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}
                    }],
                    "messages": [{"role": "user", "content": "Use the client tool if tests are needed."}]
                }),
            ),
        ] {
            let response = client
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", "test-key")
                .json(&body)
                .send()
                .await
                .expect("normal public request");

            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{case}: request must pass validation and reach the absent provider"
            );
        }

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
        assert!(response.headers().get("x-new-api-version").is_none());
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
            .json(&json!({
                "model": "claude-opus-4-8",
                "max_tokens": 1024,
                "temperature": 0.7,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("native sampling request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), NATIVE_SSE);
        let (_, sampling_body) = captured.lock().expect("capture lock").take().unwrap();
        assert_eq!(sampling_body["temperature"], 0.7,
            "native validation belongs to the selected upstream");

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
                "temperature": 1,
                "custom_extension": {"keep": true},
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("native proxy request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-amzn-requestid"], "native-request-id");
        assert_eq!(response.headers()["x-native-rate-limit"], "preserved");
        assert!(response.headers().get(header::CONNECTION).is_none());
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
        assert_eq!(body["temperature"], 1);
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
            .expect("structured-output native request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.expect("native schema body"), NATIVE_SSE);
        let (_, schema_body) = captured.lock().expect("capture lock").take().unwrap();
        assert_eq!(schema_body["output_config"]["format"]["schema"], json!({"type": "object"}));

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
