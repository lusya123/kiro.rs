//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use bytes::Bytes;
use reqwest::{Client, StatusCode};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TlsBackend;
use crate::tls_sidecar;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
const MAX_TOTAL_RETRIES: usize = 9;

/// A non-retryable HTTP rejection returned by the Kiro upstream.
///
/// Keep the status typed until the Anthropic handler builds its response. A
/// plain `anyhow!("400 ...")` loses this information and previously caused
/// client request errors to be exposed as gateway failures.
#[derive(Debug)]
pub(crate) struct UpstreamHttpError {
    status: StatusCode,
    body: String,
    api_type: &'static str,
}

#[cfg(test)]
mod tests {
    use super::KiroProvider;

    #[test]
    fn explicit_model_hint_does_not_require_reparsing_the_request_body() {
        let model = KiroProvider::model_for_request(Some("claude-opus-4.8"), b"not-json");
        assert_eq!(model.as_deref(), Some("claude-opus-4.8"));
    }
}

impl UpstreamHttpError {
    pub(crate) fn new(status: StatusCode, body: String, api_type: &'static str) -> Self {
        Self {
            status,
            body,
            api_type,
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Display for UpstreamHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} API 请求失败: {} {}",
            self.api_type, self.status, self.body
        )
    }
}

impl std::error::Error for UpstreamHttpError {}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 已完成 profileArn 探测的凭据，避免 Builder ID 每次请求都重复查询。
    profile_resolution_attempted: Mutex<HashSet<u64>>,
}

impl KiroProvider {
    fn model_for_request(model_hint: Option<&str>, request_body: &[u8]) -> Option<String> {
        model_hint.map(str::to_owned).or_else(|| {
            std::str::from_utf8(request_body)
                .ok()
                .and_then(Self::extract_model_from_request)
        })
    }

    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client =
            build_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
            profile_resolution_attempted: Mutex::new(HashSet::new()),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    async fn ensure_profile_arn(&self, ctx: &mut crate::kiro::token_manager::CallContext) {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        if ctx.credentials.is_api_key_credential() {
            return;
        }
        let needs_resolution = ctx
            .credentials
            .profile_arn
            .as_deref()
            .is_none_or(is_placeholder_profile_arn);
        if !needs_resolution || self.profile_resolution_attempted.lock().contains(&ctx.id) {
            return;
        }

        match self
            .token_manager
            .resolve_profile_arn_for(ctx.id, &ctx.token)
            .await
        {
            Ok(Some(arn)) => {
                ctx.credentials.profile_arn = Some(arn);
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Ok(None) => {
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Err(error) => {
                tracing::warn!(
                    "凭据 #{} 解析真实 profileArn 失败（本次使用兼容值）: {}",
                    ctx.id,
                    error
                );
            }
        }
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    pub async fn call_api(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(Bytes::copy_from_slice(request_body.as_bytes()), false, None)
            .await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(Bytes::copy_from_slice(request_body.as_bytes()), true, None)
            .await
    }

    pub async fn call_api_for_model(
        &self,
        request_body: Bytes,
        model: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(request_body, false, Some(model))
            .await
    }

    pub async fn call_api_stream_for_model(
        &self,
        request_body: Bytes,
        model: &str,
    ) -> anyhow::Result<reqwest::Response> {
        self.call_api_with_retry(request_body, true, Some(model))
            .await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let mut ctx = match self.token_manager.acquire_context(None).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            self.ensure_profile_arn(&mut ctx).await;

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let client = self.client_for(&ctx.credentials)?;
            let base = tls_sidecar::post(&client, &url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            let mut response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };
            // 内部诊断头不能继续进入 MCP 响应处理链。
            let _ = tls_sidecar::take_response_timing(&mut response);

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: Bytes,
        is_stream: bool,
        model_hint: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut avoid_credential_id: Option<u64> = None;
        let api_type = if is_stream { "流式" } else { "非流式" };
        let call_started = Instant::now();

        // 尝试从请求体中提取模型信息
        let model = Self::model_for_request(model_hint, &request_body);

        for attempt in 0..max_retries {
            // 获取调用上下文（绑定 index、credentials、token）
            let context = if let Some(avoided_id) = avoid_credential_id.take() {
                self.token_manager
                    .acquire_context_avoiding(model.as_deref(), avoided_id)
                    .await
            } else {
                self.token_manager.acquire_context(model.as_deref()).await
            };
            let mut ctx = match context {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            self.ensure_profile_arn(&mut ctx).await;

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(request_body.clone(), &rctx);

            let client = self.client_for(&ctx.credentials)?;
            let base = tls_sidecar::post(&client, &url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_api(base, &rctx);
            let attempt_started = Instant::now();

            let mut response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        upstream_model_id = model.as_deref().unwrap_or("unknown"),
                        api_type = api_type,
                        attempt = attempt + 1,
                        retry_count = attempt,
                        attempt_headers_ms = attempt_started.elapsed().as_millis() as u64,
                        call_elapsed_ms = call_started.elapsed().as_millis() as u64,
                        error = %e,
                        "API 请求发送失败（尝试 {}/{}）",
                        attempt + 1,
                        max_retries
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let attempt_headers_us = attempt_started.elapsed().as_micros() as u64;
            let sidecar_timing = tls_sidecar::take_response_timing(&mut response);
            let sidecar_metrics_present = sidecar_timing.is_some();
            let sidecar_request_id = sidecar_timing
                .and_then(|timing| timing.request_id)
                .unwrap_or_default();
            let sidecar_connection_reused = sidecar_timing
                .and_then(|timing| timing.connection_reused)
                .unwrap_or(false);
            let sidecar_reconnected = sidecar_timing
                .and_then(|timing| timing.reconnected)
                .unwrap_or(false);
            let sidecar_network_dial_us = sidecar_timing
                .and_then(|timing| timing.network_dial_us)
                .unwrap_or_default();
            let sidecar_tls_handshake_us = sidecar_timing
                .and_then(|timing| timing.tls_handshake_us)
                .unwrap_or_default();
            let sidecar_upstream_headers_us = sidecar_timing
                .and_then(|timing| timing.upstream_headers_us)
                .unwrap_or_default();
            let rust_sidecar_overhead_us = sidecar_timing
                .and_then(|timing| timing.upstream_headers_us)
                .map(|sidecar_us| attempt_headers_us.saturating_sub(sidecar_us))
                .unwrap_or_default();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                tracing::info!(
                    upstream_model_id = model.as_deref().unwrap_or("unknown"),
                    api_type = api_type,
                    attempt = attempt + 1,
                    retry_count = attempt,
                    upstream_headers_ms = attempt_headers_us / 1_000,
                    call_headers_ms = call_started.elapsed().as_millis() as u64,
                    sidecar_metrics_present = sidecar_metrics_present,
                    sidecar_request_id = sidecar_request_id,
                    sidecar_connection_reused = sidecar_connection_reused,
                    sidecar_reconnected = sidecar_reconnected,
                    sidecar_network_dial_us = sidecar_network_dial_us,
                    sidecar_tls_handshake_us = sidecar_tls_handshake_us,
                    sidecar_upstream_headers_us = sidecar_upstream_headers_us,
                    rust_sidecar_overhead_us = rust_sidecar_overhead_us,
                    status = %status,
                    "Kiro 上游请求成功"
                );
                return Ok(response);
            }

            tracing::warn!(
                upstream_model_id = model.as_deref().unwrap_or("unknown"),
                api_type = api_type,
                attempt = attempt + 1,
                retry_count = attempt,
                attempt_headers_ms = attempt_headers_us / 1_000,
                call_elapsed_ms = call_started.elapsed().as_millis() as u64,
                sidecar_metrics_present = sidecar_metrics_present,
                sidecar_request_id = sidecar_request_id,
                sidecar_connection_reused = sidecar_connection_reused,
                sidecar_reconnected = sidecar_reconnected,
                sidecar_network_dial_us = sidecar_network_dial_us,
                sidecar_tls_handshake_us = sidecar_tls_handshake_us,
                sidecar_upstream_headers_us = sidecar_upstream_headers_us,
                rust_sidecar_overhead_us = rust_sidecar_overhead_us,
                status = %status,
                "Kiro 上游请求返回非成功状态"
            );

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                return Err(UpstreamHttpError::new(status, body, api_type).into());
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用凭据。
            // 429 通常是凭据级限流，因此下一次尝试优先避开当前凭据；408/5xx
            // 仍保持原凭据，避免网络或全局上游故障引发无意义切换。
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if status == StatusCode::TOO_MANY_REQUESTS {
                    avoid_credential_id = Some(ctx.id);
                }
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}
