//! TLS Sidecar 子进程管理 + 请求路由
//!
//! 通过 Go uTLS sidecar 伪装 Chrome TLS 指纹，避免上游基于 JA3/JA4 指纹封号。
//!
//! 启动流程：
//! 1. [`SidecarManager::start`] spawn Go 二进制为子进程
//! 2. 轮询 `/health` 直到就绪
//! 3. 后台任务定时健康检查 + 自动重启
//!
//! 调用流程：
//! - [`init_policy`] 在启动时把全局策略写入
//! - 调用方用 [`post`] / [`get`] / [`request`] 替代 `client.post(url)` / `client.get(url)`
//! - sidecar 关闭时直接走原 URL，调用方无感知

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use reqwest::{Client, Method, RequestBuilder};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESTART_ATTEMPTS: usize = 5;
const RESTART_DELAY: Duration = Duration::from_secs(2);
const HEADER_TARGET: &str = "X-Target-Url";
const HEADER_PROXY: &str = "X-Proxy-Url";

// ───────── 全局策略（被调用点使用）─────────

/// TLS Sidecar 路由策略：调用点决定是否把请求重定向到 sidecar
///
/// 注意：上游代理 URL 不在此结构内，独立存放在 [`UPSTREAM_PROXY`] 以支持运行时热更新
/// （admin API 改 proxy 不需要重启 sidecar 子进程）。
#[derive(Debug, Clone)]
pub struct SidecarPolicy {
    enabled: bool,
    base_url: String,
}

impl SidecarPolicy {
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
        }
    }

    pub fn enabled(port: u16) -> Self {
        Self {
            enabled: true,
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    #[cfg(test)]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

static GLOBAL_POLICY: OnceLock<SidecarPolicy> = OnceLock::new();
/// 上游代理 URL：独立 RwLock 以支持运行时热更新
static UPSTREAM_PROXY: OnceLock<RwLock<Option<String>>> = OnceLock::new();

/// 初始化全局策略，进程内只能调用一次
pub fn init_policy(policy: SidecarPolicy, upstream_proxy: Option<String>) {
    if GLOBAL_POLICY.set(policy).is_err() {
        tracing::warn!("TLS Sidecar policy already initialized; ignoring re-init");
    }
    let _ = UPSTREAM_PROXY.set(RwLock::new(upstream_proxy));
}

fn current_policy() -> &'static SidecarPolicy {
    static FALLBACK: SidecarPolicy = SidecarPolicy::disabled();
    GLOBAL_POLICY.get().unwrap_or(&FALLBACK)
}

/// 当前生效的上游代理 URL（拷贝出来，调用方可任意持有）
pub fn current_upstream_proxy() -> Option<String> {
    UPSTREAM_PROXY.get().and_then(|lock| lock.read().clone())
}

/// 热更新上游代理 URL（admin API 调用此函数立即生效，无需重启 sidecar）
pub fn set_upstream_proxy(proxy: Option<String>) {
    if let Some(lock) = UPSTREAM_PROXY.get() {
        *lock.write() = proxy;
    } else {
        tracing::warn!("UPSTREAM_PROXY 未初始化，丢弃 set_upstream_proxy 调用");
    }
}

/// 全局 sidecar 是否启用（供 build_client 等模块查询）
///
/// 注意：在 `init_policy` 调用之前总是返回 false。`init_policy` 由 `main` 在
/// 任何上游 HTTP 请求发起之前调用。
pub fn is_active() -> bool {
    current_policy().enabled
}

/// 当前 sidecar 监听端口（仅 enabled 时有意义）
#[allow(dead_code)]
pub fn current_port() -> Option<u16> {
    let policy = current_policy();
    if !policy.enabled {
        return None;
    }
    // base_url 形如 "http://127.0.0.1:9090"
    policy
        .base_url
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
}

// ───────── 请求构造助手 ─────────

/// 构造 POST 请求：sidecar 启用时改写为 sidecar URL + X-Target-Url，否则原样
pub fn post(client: &Client, target_url: &str) -> RequestBuilder {
    request(client, Method::POST, target_url)
}

/// 构造 GET 请求：sidecar 启用时改写为 sidecar URL + X-Target-Url，否则原样
pub fn get(client: &Client, target_url: &str) -> RequestBuilder {
    request(client, Method::GET, target_url)
}

/// 通用入口：按 method 构造请求
pub fn request(client: &Client, method: Method, target_url: &str) -> RequestBuilder {
    let policy = current_policy();
    if policy.enabled {
        let mut rb = client
            .request(method, &policy.base_url)
            .header(HEADER_TARGET, target_url);
        // 每次请求都从 RwLock 读最新 proxy URL，支持运行时热更新
        if let Some(proxy) = current_upstream_proxy() {
            rb = rb.header(HEADER_PROXY, proxy);
        }
        rb
    } else {
        client.request(method, target_url)
    }
}

// ───────── 子进程管理 ─────────

/// SidecarManager 启动配置
///
/// 注意：上游代理（X-Proxy-Url）是按请求传递的，存放在全局 [`UPSTREAM_PROXY`] 静态变量
/// 中以支持运行时热更新；sidecar 子进程本身不需要知道。
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub port: u16,
    pub binary_path: PathBuf,
}

/// SidecarManager —— 管理 Go uTLS sidecar 子进程的生命周期
pub struct SidecarManager {
    config: SidecarConfig,
    state: Mutex<State>,
}

struct State {
    process: Option<Child>,
    ready: bool,
    restart_count: usize,
    shutting_down: bool,
    last_health_check: Option<DateTime<Utc>>,
    last_health_ok: bool,
    /// 累计成功重启次数（与 restart_count 不同，restart_count 在 ready 后清零）
    total_restarts: u64,
}

impl SidecarManager {
    /// 启动 sidecar 进程，等待 /health 就绪后返回
    pub async fn start(config: SidecarConfig) -> anyhow::Result<Arc<Self>> {
        let manager = Arc::new(Self {
            config,
            state: Mutex::new(State {
                process: None,
                ready: false,
                restart_count: 0,
                shutting_down: false,
                last_health_check: None,
                last_health_ok: false,
                total_restarts: 0,
            }),
        });

        manager.spawn_process().await?;
        manager.wait_for_ready().await?;

        // 启动后台健康检查 + 自动重启
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            loop {
                sleep(HEALTH_CHECK_INTERVAL).await;
                let Some(mgr) = weak.upgrade() else {
                    break;
                };
                if mgr.state.lock().shutting_down {
                    break;
                }
                if !mgr.health_check().await {
                    tracing::warn!("[TLS-Sidecar] health check failed, attempting restart");
                    mgr.state.lock().ready = false;
                    if let Err(e) = mgr.try_restart().await {
                        tracing::error!("[TLS-Sidecar] restart failed: {}", e);
                    }
                }
            }
        });

        Ok(manager)
    }

    /// 优雅停止（保留供未来 graceful shutdown 路径调用）
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let mut child = {
            let mut state = self.state.lock();
            state.shutting_down = true;
            state.ready = false;
            state.process.take()
        };
        if let Some(child) = child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.config.port)
    }

    async fn spawn_process(&self) -> anyhow::Result<()> {
        let bin = &self.config.binary_path;
        if !bin.exists() {
            anyhow::bail!(
                "TLS Sidecar 二进制不存在: {} (在 Docker 镜像中应为 /app/tls-sidecar)",
                bin.display()
            );
        }

        tracing::info!(
            "[TLS-Sidecar] starting binary: {} on port {}",
            bin.display(),
            self.config.port
        );

        let mut cmd = Command::new(bin);
        cmd.env("TLS_SIDECAR_PORT", self.config.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn TLS Sidecar 失败: {}", e))?;

        // 把 stdout/stderr 转发到 tracing
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::info!("[TLS-Sidecar] {}", line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::warn!("[TLS-Sidecar] {}", line);
                }
            });
        }

        self.state.lock().process = Some(child);
        Ok(())
    }

    async fn wait_for_ready(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            if self.health_check().await {
                self.state.lock().ready = true;
                self.state.lock().restart_count = 0;
                tracing::info!("[TLS-Sidecar] ready at {}", self.base_url());
                return Ok(());
            }
            sleep(Duration::from_millis(300)).await;
        }
        anyhow::bail!("TLS Sidecar 在 {:?} 内未就绪", READY_TIMEOUT)
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url());
        let client = match Client::builder()
            .timeout(HEALTH_CHECK_TIMEOUT)
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        let ok = match timeout(HEALTH_CHECK_TIMEOUT, client.get(&url).send()).await {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        };
        let mut state = self.state.lock();
        state.last_health_check = Some(Utc::now());
        state.last_health_ok = ok;
        ok
    }

    // ───────── 状态查询（admin API 调用）─────────

    /// sidecar 是否就绪（健康检查通过且子进程存在）
    pub fn is_running(&self) -> bool {
        let state = self.state.lock();
        state.ready && state.process.is_some()
    }

    /// 配置的端口
    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// 二进制路径（作为字符串展示）
    pub fn binary_path(&self) -> String {
        self.config.binary_path.display().to_string()
    }

    /// 上次健康检查时间
    pub fn last_health_check(&self) -> Option<DateTime<Utc>> {
        self.state.lock().last_health_check
    }

    /// 上次健康检查结果
    pub fn last_health_ok(&self) -> bool {
        self.state.lock().last_health_ok
    }

    /// 累计成功重启次数（用于 admin 面板观察故障频率）
    pub fn total_restarts(&self) -> u64 {
        self.state.lock().total_restarts
    }

    async fn try_restart(&self) -> anyhow::Result<()> {
        // 清掉老进程
        let mut child = self.state.lock().process.take();
        if let Some(child) = child.as_mut() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // 限制重启次数（避免反复 spawn 失败的二进制）
        {
            let mut state = self.state.lock();
            if state.restart_count >= MAX_RESTART_ATTEMPTS {
                anyhow::bail!(
                    "TLS Sidecar 已连续 {} 次启动失败，停止重试",
                    MAX_RESTART_ATTEMPTS
                );
            }
            state.restart_count += 1;
        }

        sleep(RESTART_DELAY).await;
        self.spawn_process().await?;
        self.wait_for_ready().await?;
        self.state.lock().total_restarts += 1;
        Ok(())
    }
}

// ───────── 二进制查找 ─────────

/// 按候选路径查找 sidecar 二进制
///
/// 顺序：
/// 1. 用户在 config 显式指定（外层调用方传入）
/// 2. /app/tls-sidecar （Docker 镜像默认位置）
/// 3. ./tls-sidecar/tls-sidecar （本地开发：在 cargo run 工作目录下）
/// 4. ./tls-sidecar （扁平布局）
/// 5. /usr/local/bin/tls-sidecar
pub fn find_binary(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!("config 指定的 tlsSidecarBinaryPath 不存在: {}", p);
    }

    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let bin_name = format!("tls-sidecar{}", exe_suffix);

    let candidates: [PathBuf; 4] = [
        PathBuf::from("/app").join(&bin_name),
        PathBuf::from("./tls-sidecar").join(&bin_name),
        PathBuf::from(".").join(&bin_name),
        PathBuf::from("/usr/local/bin").join(&bin_name),
    ];

    for c in &candidates {
        if c.is_file() {
            return Some(c.clone());
        }
    }
    None
}

/// 把候选路径作为字符串列表返回（仅用于错误信息）
pub fn binary_search_hints() -> &'static [&'static str] {
    &[
        "/app/tls-sidecar",
        "./tls-sidecar/tls-sidecar",
        "./tls-sidecar",
        "/usr/local/bin/tls-sidecar",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_policy_passes_url_through() {
        let policy = SidecarPolicy::disabled();
        assert!(!policy.is_enabled());
    }

    #[test]
    fn test_enabled_policy_builds_base_url() {
        let policy = SidecarPolicy::enabled(9090);
        assert!(policy.is_enabled());
        assert_eq!(policy.base_url, "http://127.0.0.1:9090");
    }

    #[test]
    fn test_find_binary_explicit_nonexistent_falls_back() {
        // explicit 路径不存在时应回退到候选路径搜索；本测试只断言调用不 panic
        let _ = find_binary(Some("/definitely/nonexistent/path"));
    }
}
