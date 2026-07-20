//! Optional native Amazon Bedrock Messages API transport.
//!
//! Kiro's EventStream does not contain Bedrock-issued thinking signatures. When
//! this provider is explicitly enabled, selected models are sent to Bedrock
//! Mantle and the response body is relayed unchanged, preserving native
//! thinking, signatures, usage, caching, and streaming behavior.

use std::collections::HashSet;

use anyhow::Context;
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use serde_json::Value;

use crate::{
    common::auth,
    http_client::{self, ProxyConfig},
    model::config::Config,
};

use super::types::ErrorResponse;

const API_KEY_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
const REQUEST_TIMEOUT_SECS: u64 = 360;

/// Native Bedrock Mantle transport for an explicit set of public model aliases.
pub struct BedrockMantleProvider {
    client: reqwest::Client,
    messages_endpoint: String,
    count_tokens_endpoint: String,
    api_key: String,
    routed_models: HashSet<String>,
}

impl BedrockMantleProvider {
    pub fn from_config(
        config: &Config,
        proxy: Option<&ProxyConfig>,
    ) -> anyhow::Result<Option<Self>> {
        if !config.bedrock_mantle_enabled {
            return Ok(None);
        }

        validate_region(&config.bedrock_mantle_region)?;
        if config.bedrock_mantle_models.is_empty() {
            anyhow::bail!("bedrockMantleModels must contain at least one model when enabled");
        }

        let api_key = std::env::var(API_KEY_ENV)
            .with_context(|| format!("{API_KEY_ENV} is required when bedrockMantleEnabled=true"))?;
        if !auth::is_valid_header_secret(&api_key) {
            anyhow::bail!("{API_KEY_ENV} must be non-empty visible ASCII");
        }

        let messages_endpoint = format!(
            "https://bedrock-mantle.{}.api.aws/anthropic/v1/messages",
            config.bedrock_mantle_region
        );
        let client =
            http_client::build_direct_client(proxy, REQUEST_TIMEOUT_SECS, config.tls_backend)?;

        Self::from_parts(
            client,
            messages_endpoint,
            api_key,
            config.bedrock_mantle_models.clone(),
        )
        .map(Some)
    }

    fn from_parts(
        client: reqwest::Client,
        messages_endpoint: String,
        api_key: String,
        models: Vec<String>,
    ) -> anyhow::Result<Self> {
        let mut routed_models = HashSet::new();
        for model in models {
            let model = model.trim();
            if model.is_empty() {
                anyhow::bail!("bedrockMantleModels cannot contain a blank model");
            }
            routed_models.insert(model.to_ascii_lowercase());
            routed_models.insert(bedrock_model_id(model).to_ascii_lowercase());
        }

        Ok(Self {
            client,
            count_tokens_endpoint: format!("{messages_endpoint}/count_tokens"),
            messages_endpoint,
            api_key,
            routed_models,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        messages_endpoint: String,
        api_key: &str,
        models: Vec<String>,
    ) -> anyhow::Result<Self> {
        let client =
            http_client::build_direct_client(None, 30, crate::model::config::TlsBackend::Rustls)?;
        Self::from_parts(client, messages_endpoint, api_key.to_string(), models)
    }

    pub fn should_route(&self, model: &str) -> bool {
        let model = model.trim().to_ascii_lowercase();
        self.routed_models.contains(&model)
            || self
                .routed_models
                .contains(&bedrock_model_id(&model).to_ascii_lowercase())
    }

    pub async fn proxy_messages(&self, incoming_headers: &HeaderMap, raw_body: Bytes) -> Response {
        self.proxy(
            &self.messages_endpoint,
            "messages",
            incoming_headers,
            raw_body,
        )
        .await
    }

    pub async fn proxy_count_tokens(
        &self,
        incoming_headers: &HeaderMap,
        raw_body: Bytes,
    ) -> Response {
        self.proxy(
            &self.count_tokens_endpoint,
            "count_tokens",
            incoming_headers,
            raw_body,
        )
        .await
    }

    async fn proxy(
        &self,
        endpoint: &str,
        operation: &str,
        incoming_headers: &HeaderMap,
        raw_body: Bytes,
    ) -> Response {
        let body = match rewrite_model(&raw_body) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(error = %error, "Bedrock Mantle request rewrite failed");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "invalid_request_error",
                        "Invalid Messages API request body",
                    )),
                )
                    .into_response();
            }
        };

        let mut request = self
            .client
            .post(endpoint)
            .header("x-api-key", &self.api_key)
            .header(header::CONTENT_TYPE, "application/json");

        for name in [
            header::ACCEPT.as_str(),
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
        ] {
            if let Some(value) = incoming_headers.get(name) {
                request = request.header(name, value);
            }
        }
        if incoming_headers.get("anthropic-version").is_none() {
            request = request.header("anthropic-version", "2023-06-01");
        }

        let upstream = match request.body(body).send().await {
            Ok(response) => response,
            Err(error) => {
                tracing::error!(error = %error, "Bedrock Mantle request failed");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(
                        "api_error",
                        "Amazon Bedrock upstream request failed",
                    )),
                )
                    .into_response();
            }
        };

        let status = upstream.status();
        let upstream_headers = upstream.headers().clone();
        let mut response = Response::builder().status(status);
        for (name, value) in &upstream_headers {
            if !is_hop_by_hop_response_header(name.as_str()) {
                response = response.header(name, value);
            }
        }
        if upstream_headers.get(header::CONTENT_TYPE).is_none() {
            response = response.header(header::CONTENT_TYPE, "application/json");
        }

        tracing::info!(
            status = status.as_u16(),
            operation,
            endpoint,
            "Bedrock Mantle response relayed"
        );
        response
            .body(Body::from_stream(upstream.bytes_stream()))
            .expect("static Bedrock Mantle response headers are valid")
    }
}

fn is_hop_by_hop_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn validate_region(region: &str) -> anyhow::Result<()> {
    let bytes = region.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 64
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
    if !valid {
        anyhow::bail!("bedrockMantleRegion is not a valid AWS Region name");
    }
    Ok(())
}

fn bedrock_model_id(model: &str) -> String {
    let model = model.trim();
    if model.to_ascii_lowercase().starts_with("anthropic.") {
        model.to_string()
    } else if model.to_ascii_lowercase().starts_with("claude-") {
        format!("anthropic.{model}")
    } else {
        model.to_string()
    }
}

fn rewrite_model(raw_body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(raw_body)?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .context("missing string model")?;
    body["model"] = Value::String(bedrock_model_id(model));
    Ok(serde_json::to_vec(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_alias_is_mapped_without_dropping_unknown_fields() {
        let raw = br#"{"model":"claude-opus-4-8","temperature":0.7,"messages":[]}"#;
        let rewritten: Value = serde_json::from_slice(&rewrite_model(raw).unwrap()).unwrap();
        assert_eq!(rewritten["model"], "anthropic.claude-opus-4-8");
        assert_eq!(rewritten["temperature"], 0.7);
    }

    #[test]
    fn region_validation_rejects_endpoint_injection() {
        assert!(validate_region("us-east-1").is_ok());
        assert!(validate_region("us-east-1.example.com").is_err());
        assert!(validate_region("../us-east-1").is_err());
        assert!(validate_region("").is_err());
    }

    #[test]
    fn configured_alias_and_native_id_both_route() {
        let provider = BedrockMantleProvider::for_test(
            "http://127.0.0.1:1/anthropic/v1/messages".to_string(),
            "test-key",
            vec!["claude-opus-4-8".to_string()],
        )
        .unwrap();
        assert!(provider.should_route("claude-opus-4-8"));
        assert!(provider.should_route("anthropic.claude-opus-4-8"));
        assert!(!provider.should_route("claude-sonnet-4-6"));
    }

    #[test]
    fn response_header_filter_keeps_end_to_end_metadata() {
        assert!(!is_hop_by_hop_response_header("retry-after"));
        assert!(!is_hop_by_hop_response_header("x-amzn-requestid"));
        assert!(is_hop_by_hop_response_header("transfer-encoding"));
        assert!(is_hop_by_hop_response_header("content-length"));
    }
}
