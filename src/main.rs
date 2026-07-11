mod admin;
mod admin_ui;
mod anthropic;
mod cluster_cache;
mod common;
mod http_client;
mod kiro;
mod model;
mod tls_sidecar;
pub mod token;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::{IdeEndpoint, KiroEndpoint};
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 集群共享缓存(虚拟 prompt-cache 登记表跨容器共享,让一批容器对外像"一个统一号池")。
    // 约定用冷门端口 46379,可用环境变量 KIRO_CLUSTER_CACHE_ADDR 覆盖;设为 off/local/disabled 则纯本地。
    // 自举:连得上就当 client;连不上就抢占该端口起内嵌服务当 owner;都不行退本地(不影响请求)。
    let cluster_addr = std::env::var("KIRO_CLUSTER_CACHE_ADDR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1:46379".to_string());
    if matches!(cluster_addr.as_str(), "off" | "local" | "disabled") {
        tracing::info!("集群共享缓存: 已禁用(纯本地登记表)");
    } else {
        cluster_cache::init(&cluster_addr).await;
        tracing::info!(
            "集群共享缓存: {} (本容器角色={})",
            cluster_addr,
            cluster_cache::global().role()
        );
    }

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 初始化 TLS Sidecar（伪装 Chrome TLS 指纹，防止上游基于 JA3/JA4 封号）
    let _sidecar_handle = if config.tls_sidecar_enabled {
        // 选择 sidecar 上游代理：tlsSidecarProxyUrl > proxyUrl（向后兼容：自动迁移）
        let sidecar_upstream_proxy = config
            .tls_sidecar_proxy_url
            .clone()
            .or_else(|| config.proxy_url.clone());
        if config.tls_sidecar_proxy_url.is_none() && config.proxy_url.is_some() {
            tracing::info!("未设置 tlsSidecarProxyUrl，已自动沿用 proxyUrl 作为 sidecar 上游代理");
        }
        // 警告：sidecar 启用时，per-credential proxy 不再生效（reqwest 仅与 localhost 通讯）
        let has_per_credential_proxy = credentials_list.iter().any(|c| c.proxy_url.is_some());
        if has_per_credential_proxy {
            tracing::warn!(
                "检测到凭据级 proxyUrl 配置，但 TLS Sidecar 启用时仅 tlsSidecarProxyUrl 生效，凭据级 proxy 将被忽略"
            );
        }

        match tls_sidecar::find_binary(config.tls_sidecar_binary_path.as_deref()) {
            Some(binary_path) => {
                let sidecar_config = tls_sidecar::SidecarConfig {
                    port: config.tls_sidecar_port,
                    binary_path,
                };
                match tls_sidecar::SidecarManager::start(sidecar_config).await {
                    Ok(handle) => {
                        tls_sidecar::init_policy(
                            tls_sidecar::SidecarPolicy::enabled(config.tls_sidecar_port),
                            sidecar_upstream_proxy,
                        );
                        tracing::info!(
                            "TLS Sidecar 已启用：所有上游 HTTPS 经由 127.0.0.1:{} 转发（Chrome uTLS）",
                            config.tls_sidecar_port
                        );
                        Some(handle)
                    }
                    Err(e) => {
                        tracing::error!(
                            "TLS Sidecar 启动失败: {}（默认开启状态下视为致命错误，请检查二进制或在 config.json 中设置 tlsSidecarEnabled=false）",
                            e
                        );
                        std::process::exit(1);
                    }
                }
            }
            None => {
                tracing::error!(
                    "TLS Sidecar 二进制未找到。已搜索: {:?}。请确认镜像构建包含 sidecar，或在 config.json 中设置 tlsSidecarEnabled=false",
                    tls_sidecar::binary_search_hints()
                );
                std::process::exit(1);
            }
        }
    } else {
        tls_sidecar::init_policy(tls_sidecar::SidecarPolicy::disabled(), None);
        tracing::warn!("TLS Sidecar 已禁用，上游请求将使用原生 rustls 指纹，存在被识别封号的风险");
        None
    };

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);
    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        config.aws_b40_compat,
    );

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app
        } else {
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                endpoint_names.clone(),
                _sidecar_handle.clone(),
                Some(std::path::PathBuf::from(&config_path)),
            );
            let admin_state = admin::AdminState::new(admin_key, admin_service);
            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .route(
                    "/admin/",
                    axum::routing::get(|| async { axum::response::Redirect::temporary("/admin") }),
                )
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
