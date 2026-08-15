//! 集群共享缓存登记表(自举式)。
//!
//! 目标:一台机器上部署很多 kiro-rs 容器时,让它们对外像"一个统一号池"——虚拟 prompt-cache
//! 的"某前缀谁先见过"这份登记表必须**跨容器共享**,否则每个容器各记各的,同一前缀在不同容器
//! 会反复报 cache_creation、几乎命不中 cache_read,一测就露(且客户被反复按创建价计费)。
//!
//! 机制:
//! 1. 启动时探测约定地址(默认 127.0.0.1:46379)有没有 Redis 协议服务在跑(PING)。
//! 2. 有 → 作为客户端连上它(可以是真 Redis,也可以是别的容器起的内嵌服务)。
//! 3. 没有 → 抢占 `bind` 该端口:抢到的容器就地启动**内嵌的 Redis 协议兼容迷你服务**
//!    (无需镜像装 redis-server);抢不到的(竞态输了)回退去连已经起来的那个。
//! 4. 暂时连不上 → 退回**本地内存**登记表保证请求继续，同时后台重新连接或竞争接管 owner；
//!    恢复后把仍有效的本地热键合并回共享服务，避免集群永久分裂。
//!
//! 高并发:客户端用 `redis` 的 MultiplexedConnection(单连接多路复用,几条连接扛上万并发);
//! 每个 API 请求只做 1~4 次极小操作(EXISTS/SET EX);所有操作带短超时,超时/故障即本地回退。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

const OP_TIMEOUT: Duration = Duration::from_millis(80);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

// 内嵌 RESP 服务的输入上限:防止本机任意 RESP 客户端用超大长度字段撑爆内存/触发进程 abort
// (owner 进程一挂,全机共享缓存就没了)。这些上限对正常缓存 key(几十字节)绰绰有余。
const MAX_ARGS: usize = 64; // 单条命令最多参数个数
const MAX_BULK_LEN: usize = 1 << 20; // 单个参数最大 1 MiB
const MAX_LINE_LEN: usize = 64 * 1024; // 单行(协议头)最大 64 KiB

// 熔断:共享后端一旦超时/出错,冷却 3s 内所有操作直接走本地,避免每请求 4+4 次 ×80ms 叠加延迟。
const BREAKER_COOLDOWN_MS: i64 = 3_000;

// 共享 owner 消失后，现有客户端必须自行恢复，不能永久退化成各进程独立缓存。
// 单 loopback 地址可以立即竞争 bind（内核保证只会有一个胜者）；多候选地址仍给 primary
// 约 6 秒恢复窗口，随后才允许后续候选接管，避免跨容器候选发生脑裂。
const RECOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const PRIMARY_RECOVERY_GRACE_ROUNDS: usize = 60;
const RECOVERY_REPLAY_BATCH: usize = 256;
const RECOVERY_REPLAY_TIMEOUT: Duration = Duration::from_secs(1);
// 一个进程发现旧 owner 连接已断时，不应立刻把共享未命中当成冷缓存返回。给重新选主、
// 重连及其他进程的本地热键回灌一个很短的窗口；这只发生在故障后的首批请求上。
const RECOVERY_READ_WAIT: Duration = Duration::from_millis(750);
const RECOVERY_READ_POLL: Duration = Duration::from_millis(20);

const ROLE_RECOVERING: u8 = 0;
const ROLE_OWNER: u8 = 1;
const ROLE_CLIENT: u8 = 2;

// 本地回退表只需低频回收过期项。若每次 register 都扫描整张表，连续回放 R 个
// thinking signature 时会产生 1 + 2 + ... + R 次访问，重新形成 O(R²)。
const LOCAL_STORE_CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 地址是否为回环(内嵌无鉴权服务只在回环上是安全的)。
fn is_loopback_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// 全局单例。未初始化时(如测试)退回纯本地。
static STORE: OnceLock<ClusterCache> = OnceLock::new();

pub fn global() -> &'static ClusterCache {
    STORE.get_or_init(ClusterCache::local)
}

/// 由 main 在启动时调用一次;失败也总能得到一个可用(本地)实例。
pub async fn init(addr: &str) {
    let cache = ClusterCache::bootstrap(addr).await;
    let _ = STORE.set(cache);
}

struct ManagedConnection {
    generation: u64,
    conn: redis::aio::MultiplexedConnection,
}

pub(crate) struct SharedBackend {
    candidates: Vec<String>,
    connection: RwLock<Option<ManagedConnection>>,
    next_generation: AtomicU64,
    recovering: AtomicBool,
    role: AtomicU8,
    breaker: AtomicI64,
}

impl SharedBackend {
    fn new(
        candidates: Vec<String>,
        connection: Option<redis::aio::MultiplexedConnection>,
        role: u8,
    ) -> Self {
        let managed = connection.map(|conn| ManagedConnection {
            generation: 1,
            conn,
        });
        Self {
            candidates,
            connection: RwLock::new(managed),
            next_generation: AtomicU64::new(2),
            recovering: AtomicBool::new(false),
            role: AtomicU8::new(role),
            breaker: AtomicI64::new(0),
        }
    }

    fn role(&self) -> &'static str {
        match self.role.load(Ordering::Relaxed) {
            ROLE_OWNER => "owner",
            ROLE_CLIENT => "client",
            _ => "recovering",
        }
    }

    fn cooling(&self) -> bool {
        self.breaker.load(Ordering::Relaxed) > now_ms()
    }

    fn trip(&self) {
        self.breaker
            .store(now_ms() + BREAKER_COOLDOWN_MS, Ordering::Relaxed);
    }

    async fn connection(&self) -> Option<(u64, redis::aio::MultiplexedConnection)> {
        self.connection
            .read()
            .await
            .as_ref()
            .map(|managed| (managed.generation, managed.conn.clone()))
    }

    async fn invalidate(&self, generation: u64) {
        let mut connection = self.connection.write().await;
        if connection
            .as_ref()
            .is_some_and(|managed| managed.generation == generation)
        {
            *connection = None;
            self.role.store(ROLE_RECOVERING, Ordering::Relaxed);
        }
    }

    async fn install(&self, conn: redis::aio::MultiplexedConnection, role: u8) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        *self.connection.write().await = Some(ManagedConnection { generation, conn });
        self.role.store(role, Ordering::Relaxed);
        self.breaker.store(0, Ordering::Relaxed);
    }

    async fn wait_for_connection(
        &self,
        deadline: tokio::time::Instant,
    ) -> Option<(u64, redis::aio::MultiplexedConnection)> {
        loop {
            if let Some(connection) = self.connection().await {
                return Some(connection);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            tokio::time::sleep(RECOVERY_READ_POLL.min(deadline - now)).await;
        }
    }

    fn schedule_recovery(self: &Arc<Self>, fallback: Arc<LocalStore>) {
        if self
            .recovering
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let backend = self.clone();
        tokio::spawn(async move {
            backend.role.store(ROLE_RECOVERING, Ordering::Relaxed);
            let mut rounds = 0usize;
            loop {
                for candidate in &backend.candidates {
                    if let Some(conn) = try_connect(candidate).await {
                        tracing::info!("集群缓存:故障后重新连接共享服务 {}", candidate);
                        backend.install(conn.clone(), ROLE_CLIENT).await;
                        replay_local_entries(conn, &fallback).await;
                        backend.recovering.store(false, Ordering::Release);
                        return;
                    }
                }

                // 单地址/loopback 部署由所有存活进程竞争同一个 bind，内核只允许一个新 owner。
                // 多候选部署也始终优先恢复 primary。
                if let Some(primary) = backend.candidates.first()
                    && let Some(conn) = try_own_candidate(primary).await
                {
                    tracing::warn!("集群缓存:原 owner 不可达，本进程已接管 {}", primary);
                    backend.install(conn.clone(), ROLE_OWNER).await;
                    replay_local_entries(conn, &fallback).await;
                    backend.recovering.store(false, Ordering::Release);
                    return;
                }

                // 只有 primary 持续不可达约 6 秒后，才允许后续候选接管。
                if rounds >= PRIMARY_RECOVERY_GRACE_ROUNDS {
                    for candidate in backend.candidates.iter().skip(1) {
                        if let Some(conn) = try_own_candidate(candidate).await {
                            tracing::warn!(
                                "集群缓存:primary 持续不可达，本进程已接管候选 {}",
                                candidate
                            );
                            backend.install(conn.clone(), ROLE_OWNER).await;
                            replay_local_entries(conn, &fallback).await;
                            backend.recovering.store(false, Ordering::Release);
                            return;
                        }
                    }
                }

                rounds = rounds.saturating_add(1);
                tokio::time::sleep(RECOVERY_RETRY_INTERVAL).await;
            }
        });
    }
}

pub enum ClusterCache {
    /// 共享:通过 redis 协议连接(真 Redis 或别的容器起的内嵌服务)。
    Shared {
        backend: Arc<SharedBackend>,
        /// 共享后端异常时的兜底本地表。
        fallback: Arc<LocalStore>,
    },
    /// 纯本地(无共享地址或全部失败)。
    Local(LocalStore),
}

impl ClusterCache {
    fn local() -> Self {
        ClusterCache::Local(LocalStore::new())
    }

    fn shared(conn: redis::aio::MultiplexedConnection, role: u8, candidates: Vec<String>) -> Self {
        ClusterCache::Shared {
            backend: Arc::new(SharedBackend::new(candidates, Some(conn), role)),
            fallback: Arc::new(LocalStore::new()),
        }
    }

    fn recovering(candidates: Vec<String>) -> Self {
        ClusterCache::Shared {
            backend: Arc::new(SharedBackend::new(candidates, None, ROLE_RECOVERING)),
            fallback: Arc::new(LocalStore::new()),
        }
    }

    pub fn role(&self) -> &'static str {
        match self {
            ClusterCache::Shared { backend, .. } => backend.role(),
            ClusterCache::Local(_) => "local",
        }
    }

    /// 自举:连得上就当 client;连不上就抢占端口起内嵌服务当 owner;都不行退本地。
    ///
    /// `addr` 支持**逗号分隔的多个候选**,用于 owner 故障转移(多容器集群无单点):
    /// - 任一候选可连 → 当 client(连上谁用谁);
    /// - 都连不上 → 按顺序尝试 `bind`。关键点:`bind("<容器名>:46379")` 只有当该名字
    ///   解析到**本机 IP** 时才成功,所以每个容器只能"抢占"与自己同名的候选 —— 天然
    ///   避免脑裂(别的容器名解析成远端 IP,bind 直接失败),同时允许列表里靠后的容器
    ///   在靠前的 owner 挂掉后接管。单地址(如默认 127.0.0.1:46379)= 只有一个候选,行为不变。
    async fn bootstrap(addr: &str) -> Self {
        let candidates: Vec<String> = addr
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if candidates.is_empty() {
            tracing::warn!("集群缓存:地址为空,退回本地");
            return ClusterCache::local();
        }

        // 1) 任一候选连得上 → client(primary owner 已在)
        for cand in &candidates {
            if let Some(conn) = try_connect(cand).await {
                tracing::info!("集群缓存:连接到已有共享服务 {}", cand);
                return ClusterCache::shared(conn, ROLE_CLIENT, candidates.clone());
            }
        }

        // 2) primary 抢占:只 bind **第一个**候选。名字==本机的容器立即成为 primary owner;
        //    其他容器 bind 远端名字会失败,落到下面的等待逻辑——**不会**在此各自称王(防脑裂)。
        if let Some(conn) = try_own_candidate(&candidates[0]).await {
            tracing::info!(
                "集群缓存:本容器成为 owner,内嵌共享服务已启动于 {}",
                candidates[0]
            );
            return ClusterCache::shared(conn, ROLE_OWNER, candidates);
        }

        // 3) 本容器不是 primary(或 primary 尚在启动)。**先给 primary 充足时间**(~6s)重试连接
        //    所有候选;这段等待是防脑裂的关键:failover 容器绝不在 primary 还可能上线时自立门户。
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            for cand in &candidates {
                if let Some(conn) = try_connect(cand).await {
                    tracing::info!("集群缓存:连接到 owner {}", cand);
                    return ClusterCache::shared(conn, ROLE_CLIENT, candidates.clone());
                }
            }
        }

        // 4) primary 确实持续不可达 → 由后续候选里"与自己同名"的容器接管成为 failover owner。
        for cand in candidates.iter().skip(1) {
            if let Some(conn) = try_own_candidate(cand).await {
                tracing::info!("集群缓存:本容器成为 failover owner {}", cand);
                return ClusterCache::shared(conn, ROLE_OWNER, candidates.clone());
            }
        }

        // 5) 收尾:再试一轮连接(可能刚有人接管)。仍失败时保留候选地址并在后台持续恢复；
        //    不能永久固定为本地缓存，否则启动时的一次网络抖动就会拆散整个集群。
        for cand in &candidates {
            if let Some(conn) = try_connect(cand).await {
                return ClusterCache::shared(conn, ROLE_CLIENT, candidates.clone());
            }
        }
        tracing::warn!(
            "集群缓存:暂时无法连接任何候选 {:?},先用本地回退并后台恢复",
            candidates
        );
        let cache = ClusterCache::recovering(candidates);
        if let ClusterCache::Shared { backend, fallback } = &cache {
            backend.schedule_recovery(fallback.clone());
        }
        cache
    }

    /// 只读:该前缀是否已登记(不写入)。用于 cache_plan 的"找最高已命中前缀"。
    pub async fn exists(&self, key: &str) -> bool {
        match self {
            ClusterCache::Local(s) => s.exists(key),
            ClusterCache::Shared { backend, fallback } => {
                if backend.cooling() {
                    backend.schedule_recovery(fallback.clone());
                    return exists_after_recovery(backend, fallback, key).await;
                }
                let Some((generation, conn)) = backend.connection().await else {
                    backend.schedule_recovery(fallback.clone());
                    return exists_after_recovery(backend, fallback, key).await;
                };
                match redis_exists(conn.clone(), key).await {
                    Some(true) => true,
                    Some(false) => {
                        // 新 owner 启动后共享表为空时，本进程的回退镜像仍可证明该键曾成功提交。
                        // 立即补写共享表，让后续其他容器也恢复命中。
                        if let Some(ttl) = fallback.live_ttl(key) {
                            if redis_register(conn, key, ttl).await.is_none() {
                                backend.trip();
                                backend.invalidate(generation).await;
                                backend.schedule_recovery(fallback.clone());
                            }
                            true
                        } else {
                            false
                        }
                    }
                    None => {
                        backend.trip();
                        backend.invalidate(generation).await;
                        backend.schedule_recovery(fallback.clone());
                        exists_after_recovery(backend, fallback, key).await
                    }
                }
            }
        }
    }

    /// 登记一个前缀(写入,带 TTL)。用于"首次创建缓存",以及命中时刷新 TTL。
    pub async fn register(&self, key: &str, ttl: Duration) {
        match self {
            ClusterCache::Local(s) => s.register(key, ttl),
            ClusterCache::Shared { backend, fallback } => {
                // Mirror successful shared writes locally as well. If the shared backend later
                // trips its breaker, this process can still validate entries it issued itself
                // instead of turning a transient Redis outage into a false cache/signature miss.
                fallback.register(key, ttl);
                if backend.cooling() {
                    backend.schedule_recovery(fallback.clone());
                    return;
                }
                let Some((generation, conn)) = backend.connection().await else {
                    backend.schedule_recovery(fallback.clone());
                    return;
                };
                if redis_register(conn, key, ttl).await.is_none() {
                    backend.trip();
                    backend.invalidate(generation).await;
                    backend.schedule_recovery(fallback.clone());
                }
            }
        }
    }
}

/// 共享连接故障后的同请求重读。先尊重本进程的热键镜像；若本地没有，则等待后台完成
/// 选主/重连，并在短窗口内反复确认共享表，让其他存活进程有机会把热键回灌进新 owner。
async fn exists_after_recovery(
    backend: &Arc<SharedBackend>,
    fallback: &Arc<LocalStore>,
    key: &str,
) -> bool {
    if fallback.exists(key) {
        return true;
    }

    let deadline = tokio::time::Instant::now() + RECOVERY_READ_WAIT;
    loop {
        let Some((generation, conn)) = backend.wait_for_connection(deadline).await else {
            return fallback.exists(key);
        };
        match redis_exists(conn.clone(), key).await {
            Some(true) => return true,
            Some(false) => {
                // 本进程可能在等待期间由另一个并发请求登记了该键；同步补进共享表。
                if let Some(ttl) = fallback.live_ttl(key) {
                    if redis_register(conn, key, ttl).await.is_some() {
                        return true;
                    }
                    backend.trip();
                    backend.invalidate(generation).await;
                    backend.schedule_recovery(fallback.clone());
                }
            }
            None => {
                backend.trip();
                backend.invalidate(generation).await;
                backend.schedule_recovery(fallback.clone());
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return fallback.exists(key);
        }
        tokio::time::sleep(RECOVERY_READ_POLL.min(deadline - now)).await;
    }
}

async fn redis_exists(mut conn: redis::aio::MultiplexedConnection, key: &str) -> Option<bool> {
    let mut cmd = redis::cmd("EXISTS");
    cmd.arg(key);
    match tokio::time::timeout(OP_TIMEOUT, cmd.query_async::<i64>(&mut conn)).await {
        Ok(Ok(n)) => Some(n > 0),
        _ => None,
    }
}

async fn redis_register(
    mut conn: redis::aio::MultiplexedConnection,
    key: &str,
    ttl: Duration,
) -> Option<()> {
    let mut cmd = redis::cmd("SET");
    cmd.arg(key).arg(1).arg("EX").arg(ttl.as_secs().max(1));
    match tokio::time::timeout(OP_TIMEOUT, cmd.query_async::<String>(&mut conn)).await {
        Ok(Ok(_)) => Some(()),
        _ => None,
    }
}

async fn replay_local_entries(mut conn: redis::aio::MultiplexedConnection, fallback: &LocalStore) {
    let entries = fallback.live_entries();
    if entries.is_empty() {
        return;
    }
    let mut replayed = 0usize;
    for chunk in entries.chunks(RECOVERY_REPLAY_BATCH) {
        let mut pipeline = redis::pipe();
        for (key, ttl) in chunk {
            pipeline
                .cmd("SET")
                .arg(key)
                .arg(1)
                .arg("EX")
                .arg(ttl.as_secs().max(1))
                .ignore();
        }
        match tokio::time::timeout(
            RECOVERY_REPLAY_TIMEOUT,
            pipeline.query_async::<()>(&mut conn),
        )
        .await
        {
            Ok(Ok(())) => replayed += chunk.len(),
            _ => {
                tracing::warn!(
                    replayed,
                    remaining = entries.len().saturating_sub(replayed),
                    "集群缓存:恢复后回放本地热键中断"
                );
                return;
            }
        }
    }
    tracing::info!(replayed, "集群缓存:已把本地回退热键合并回共享服务");
}

/// 尝试抢占一个候选地址并启动内嵌共享服务。`bind` 是跨进程选主原语：同一地址只会有
/// 一个胜者，其余进程下一轮会作为 client 连到胜者。
async fn try_own_candidate(candidate: &str) -> Option<redis::aio::MultiplexedConnection> {
    let listener = TcpListener::bind(candidate).await.ok()?;
    if !is_loopback_addr(candidate) {
        tracing::warn!(
            "集群缓存:即将在非回环地址 {} 启动内嵌服务(无鉴权的键值服务)。\
             请确保该地址仅在可信内网可达,勿暴露到公网。",
            candidate
        );
    }
    spawn_embedded_server(listener);
    for _ in 0..20 {
        if let Some(conn) = try_connect(candidate).await {
            return Some(conn);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tracing::warn!("集群缓存:{} 内嵌服务已起但自连失败", candidate);
    None
}

async fn try_connect(addr: &str) -> Option<redis::aio::MultiplexedConnection> {
    let url = format!("redis://{addr}/");
    let client = redis::Client::open(url).ok()?;
    let conn_fut = client.get_multiplexed_async_connection();
    let mut conn = tokio::time::timeout(CONNECT_TIMEOUT, conn_fut)
        .await
        .ok()?
        .ok()?;
    let ping = redis::cmd("PING");
    tokio::time::timeout(CONNECT_TIMEOUT, ping.query_async::<String>(&mut conn))
        .await
        .ok()?
        .ok()?;
    Some(conn)
}

// ===================== 本地内存登记表(回退用) =====================

struct LocalStoreState {
    entries: HashMap<String, Instant>,
    next_cleanup_at: Instant,
    #[cfg(test)]
    cleanup_scan_steps: usize,
}

pub struct LocalStore {
    state: Mutex<LocalStoreState>,
}

impl LocalStore {
    fn new() -> Self {
        let now = Instant::now();
        LocalStore {
            state: Mutex::new(LocalStoreState {
                entries: HashMap::new(),
                next_cleanup_at: now + LOCAL_STORE_CLEANUP_INTERVAL,
                #[cfg(test)]
                cleanup_scan_steps: 0,
            }),
        }
    }
    fn exists(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.entries.get(key).copied() {
            Some(expires_at) if expires_at > now => true,
            Some(_) => {
                // 查询命中的过期项立即删除；其余过期项由低频全表清理回收。
                state.entries.remove(key);
                false
            }
            None => false,
        }
    }
    fn register(&self, key: &str, ttl: Duration) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if now >= state.next_cleanup_at {
            #[cfg(test)]
            {
                state.cleanup_scan_steps =
                    state.cleanup_scan_steps.saturating_add(state.entries.len());
            }
            state.entries.retain(|_, expires_at| *expires_at > now);
            state.next_cleanup_at = now + LOCAL_STORE_CLEANUP_INTERVAL;
        }
        state.entries.insert(key.to_string(), now + ttl);
    }

    fn live_ttl(&self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.entries.get(key).copied() {
            Some(expires_at) if expires_at > now => Some(expires_at.duration_since(now)),
            Some(_) => {
                state.entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn live_entries(&self) -> Vec<(String, Duration)> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|_, expires_at| *expires_at > now);
        state
            .entries
            .iter()
            .map(|(key, expires_at)| (key.clone(), expires_at.duration_since(now)))
            .collect()
    }
}

// ===================== 内嵌 Redis 协议兼容迷你服务 =====================

type Store = std::sync::Arc<Mutex<HashMap<Vec<u8>, (Vec<u8>, Option<Instant>)>>>;

fn spawn_embedded_server(listener: TcpListener) {
    let store: Store = std::sync::Arc::new(Mutex::new(HashMap::new()));
    // 定期清理过期键
    {
        let store = store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let now = Instant::now();
                if let Ok(mut m) = store.lock() {
                    m.retain(|_, (_, exp)| exp.map(|e| e > now).unwrap_or(true));
                }
            }
        });
    }
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let store = store.clone();
                    tokio::spawn(async move {
                        let _ = serve_conn(sock, store).await;
                    });
                }
                Err(e) => {
                    tracing::warn!("集群缓存内嵌服务 accept 失败: {}", e);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });
}

async fn serve_conn(sock: TcpStream, store: Store) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    loop {
        let args = match read_command(&mut rd).await? {
            Some(a) if !a.is_empty() => a,
            Some(_) => continue,
            None => return Ok(()), // 连接关闭
        };
        let cmd = args[0].to_ascii_uppercase();
        let resp: Vec<u8> = match cmd.as_slice() {
            b"PING" => b"+PONG\r\n".to_vec(),
            b"SET" => handle_set(&args, &store),
            b"EXISTS" => handle_exists(&args, &store),
            b"EXPIRE" => handle_expire(&args, &store),
            b"GET" => handle_get(&args, &store),
            b"DEL" => handle_del(&args, &store),
            b"QUIT" => {
                let _ = wr.write_all(b"+OK\r\n").await;
                return Ok(());
            }
            // HELLO/CLIENT/COMMAND 等:回错误,让 redis-rs 回退 RESP2 并继续。
            _ => b"-ERR unknown command\r\n".to_vec(),
        };
        wr.write_all(&resp).await?;
    }
}

fn now_expiry(secs: Option<u64>) -> Option<Instant> {
    secs.map(|s| Instant::now() + Duration::from_secs(s.max(1)))
}

fn handle_set(args: &[Vec<u8>], store: &Store) -> Vec<u8> {
    // SET key val [NX] [EX secs]
    if args.len() < 3 {
        return b"-ERR wrong number of arguments\r\n".to_vec();
    }
    let key = args[1].clone();
    let val = args[2].clone();
    let mut nx = false;
    let mut ex: Option<u64> = None;
    let mut i = 3;
    while i < args.len() {
        match args[i].to_ascii_uppercase().as_slice() {
            b"NX" => {
                nx = true;
                i += 1;
            }
            b"EX" if i + 1 < args.len() => {
                ex = std::str::from_utf8(&args[i + 1])
                    .ok()
                    .and_then(|s| s.parse().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }
    let now = Instant::now();
    let mut m = store.lock().unwrap_or_else(|e| e.into_inner());
    let live = m
        .get(&key)
        .map(|(_, e)| e.map(|e| e > now).unwrap_or(true))
        .unwrap_or(false);
    if nx && live {
        return b"$-1\r\n".to_vec(); // NX 失败:已存在
    }
    m.insert(key, (val, now_expiry(ex)));
    b"+OK\r\n".to_vec()
}

fn handle_exists(args: &[Vec<u8>], store: &Store) -> Vec<u8> {
    if args.len() < 2 {
        return b"-ERR wrong number of arguments\r\n".to_vec();
    }
    let now = Instant::now();
    let m = store.lock().unwrap_or_else(|e| e.into_inner());
    let n = args[1..]
        .iter()
        .filter(|k| {
            m.get(*k)
                .map(|(_, e)| e.map(|e| e > now).unwrap_or(true))
                .unwrap_or(false)
        })
        .count();
    format!(":{n}\r\n").into_bytes()
}

fn handle_expire(args: &[Vec<u8>], store: &Store) -> Vec<u8> {
    if args.len() < 3 {
        return b"-ERR wrong number of arguments\r\n".to_vec();
    }
    let secs: Option<u64> = std::str::from_utf8(&args[2])
        .ok()
        .and_then(|s| s.parse().ok());
    let mut m = store.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = m.get_mut(&args[1]) {
        entry.1 = now_expiry(secs);
        b":1\r\n".to_vec()
    } else {
        b":0\r\n".to_vec()
    }
}

fn handle_get(args: &[Vec<u8>], store: &Store) -> Vec<u8> {
    if args.len() < 2 {
        return b"-ERR wrong number of arguments\r\n".to_vec();
    }
    let now = Instant::now();
    let m = store.lock().unwrap_or_else(|e| e.into_inner());
    match m.get(&args[1]) {
        Some((v, e)) if e.map(|e| e > now).unwrap_or(true) => {
            let mut out = format!("${}\r\n", v.len()).into_bytes();
            out.extend_from_slice(v);
            out.extend_from_slice(b"\r\n");
            out
        }
        _ => b"$-1\r\n".to_vec(),
    }
}

fn handle_del(args: &[Vec<u8>], store: &Store) -> Vec<u8> {
    let mut m = store.lock().unwrap_or_else(|e| e.into_inner());
    let n = args[1..].iter().filter(|k| m.remove(*k).is_some()).count();
    format!(":{n}\r\n").into_bytes()
}

/// 读取一条 RESP 命令(仅支持数组形态 `*N$len...`,redis-rs 都用这个)。返回 None 表示连接关闭。
async fn read_command<R: AsyncReadExt + Unpin>(
    rd: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<Vec<u8>>>> {
    let first = match read_line(rd).await? {
        Some(l) => l,
        None => return Ok(None),
    };
    if first.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if first[0] != b'*' {
        // 非数组(inline):按空格拆分
        let parts = first
            .split(|b| *b == b' ')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec())
            .collect();
        return Ok(Some(parts));
    }
    let n: usize = std::str::from_utf8(&first[1..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if n > MAX_ARGS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "too many args",
        ));
    }
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        let hdr = match read_line(rd).await? {
            Some(l) => l,
            None => return Ok(None),
        };
        if hdr.is_empty() || hdr[0] != b'$' {
            return Ok(Some(args));
        }
        let len: usize = std::str::from_utf8(&hdr[1..])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        // 上限保护:拒绝超大长度,防止 vec![0u8; len+2] 撑爆内存/整数溢出。
        if len > MAX_BULK_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bulk string too large",
            ));
        }
        let mut buf = vec![0u8; len + 2]; // 含结尾 \r\n
        rd.read_exact(&mut buf).await?;
        buf.truncate(len);
        args.push(buf);
    }
    Ok(Some(args))
}

/// 读取一行(到 \r\n),去掉行尾 \r\n。None=连接关闭。带长度上限,防止无换行的洪泛撑爆内存。
async fn read_line<R: AsyncReadExt + Unpin>(
    rd: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let b = match rd.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };
        if b == b'\n' {
            break;
        }
        if b != b'\r' {
            line.push(b);
            if line.len() > MAX_LINE_LEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
        }
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_server_roundtrip_and_sharing() {
        // 起内嵌服务,两个独立客户端(模拟两个容器)应共享状态。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_embedded_server(listener);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let c1 = ClusterCache::shared(
            try_connect(&addr).await.expect("connect1"),
            ROLE_OWNER,
            vec![addr.clone()],
        );
        let c2 = ClusterCache::shared(
            try_connect(&addr).await.expect("connect2"),
            ROLE_CLIENT,
            vec![addr.clone()],
        );
        let ttl = Duration::from_secs(300);
        // 容器1 登记 prefixA;容器2 应能看到(跨容器共享)
        assert!(!c1.exists("prefixA").await);
        c1.register("prefixA", ttl).await;
        assert!(c2.exists("prefixA").await, "跨容器应共享登记");
        // 容器2 登记 prefixB;容器1 应能看到
        assert!(!c2.exists("prefixB").await);
        c2.register("prefixB", ttl).await;
        assert!(c1.exists("prefixB").await, "跨容器应共享登记");
    }

    #[tokio::test]
    async fn bootstrap_multi_candidate_failover_to_reachable() {
        // 多候选:第一个不可达、第二个有服务 → 应跳过第一个,连上第二个当 client。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap().to_string();
        spawn_embedded_server(listener);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 占一个端口再立即释放,作为"不可达"候选(连接会被拒)。
        let dead_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = dead_listener.local_addr().unwrap().to_string();
        drop(dead_listener);

        let cache = ClusterCache::bootstrap(&format!("{dead},{live}")).await;
        assert_eq!(
            cache.role(),
            "client",
            "应跳过不可达候选、连上可达候选当 client"
        );
    }

    #[tokio::test]
    async fn embedded_server_survives_malicious_input() {
        // 回归测试:内嵌服务面对超大长度字段/超大数组数/超长行时必须不崩、不 OOM,且事后仍能服务。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_embedded_server(listener);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let attacks = [
            "*1\r\n$99999999999\r\n".to_string(), // 超大 bulk 长度(旧代码会 vec![0u8; 巨大] → abort)
            "*999999999\r\n".to_string(),         // 超大数组数
            format!("{}", "A".repeat(200_000)),   // 无换行的超长行洪泛
        ];
        for payload in attacks {
            if let Ok(mut s) = TcpStream::connect(&addr).await {
                let _ = s.write_all(payload.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 服务仍存活:新客户端能正常读写。
        let c = ClusterCache::shared(
            try_connect(&addr).await.expect("恶意输入后服务应仍存活"),
            ROLE_CLIENT,
            vec![addr.clone()],
        );
        let ttl = Duration::from_secs(300);
        assert!(!c.exists("k").await);
        c.register("k", ttl).await;
        assert!(c.exists("k").await);
    }

    #[test]
    fn is_loopback_detects_addrs() {
        assert!(is_loopback_addr("127.0.0.1:46379"));
        assert!(is_loopback_addr("localhost:46379"));
        assert!(!is_loopback_addr("0.0.0.0:46379"));
        assert!(!is_loopback_addr("192.168.1.10:46379"));
    }

    #[test]
    fn local_store_basic() {
        let s = LocalStore::new();
        let ttl = Duration::from_secs(300);
        assert!(!s.exists("k"));
        s.register("k", ttl);
        assert!(s.exists("k"));
    }

    #[test]
    fn local_store_register_is_constant_time_between_cleanup_ticks() {
        let s = LocalStore::new();
        let ttl = Duration::from_secs(300);
        for index in 0..4_096 {
            s.register(&format!("signature-{index}"), ttl);
        }

        let state = s.state.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.entries.len(), 4_096);
        assert_eq!(
            state.cleanup_scan_steps, 0,
            "a burst of registrations must not rescan the accumulated table"
        );
    }

    #[test]
    fn local_store_cleanup_is_low_frequency_and_expired_lookup_is_eager() {
        let s = LocalStore::new();
        let now = Instant::now();
        {
            let mut state = s.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.insert(
                "expired-on-cleanup".to_string(),
                now - Duration::from_secs(1),
            );
            state
                .entries
                .insert("live".to_string(), now + Duration::from_secs(300));
            state.next_cleanup_at = now;
        }

        s.register("new", Duration::from_secs(300));
        {
            let state = s.state.lock().unwrap_or_else(|e| e.into_inner());
            assert!(!state.entries.contains_key("expired-on-cleanup"));
            assert!(state.entries.contains_key("live"));
            assert!(state.entries.contains_key("new"));
            assert_eq!(state.cleanup_scan_steps, 2);
        }

        {
            let mut state = s.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.insert(
                "expired-on-lookup".to_string(),
                Instant::now() - Duration::from_secs(1),
            );
        }
        assert!(!s.exists("expired-on-lookup"));
        let state = s.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!state.entries.contains_key("expired-on-lookup"));
        assert_eq!(
            state.cleanup_scan_steps, 2,
            "registers inside the cleanup window must remain O(1)"
        );
    }

    #[test]
    fn local_store_live_entries_excludes_expired_and_preserves_ttl() {
        let s = LocalStore::new();
        s.register("live", Duration::from_secs(300));
        {
            let mut state = s.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entries.insert(
                "expired".to_string(),
                Instant::now() - Duration::from_secs(1),
            );
        }

        let entries = s.live_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "live");
        assert!(entries[0].1 > Duration::from_secs(0));
        assert!(entries[0].1 <= Duration::from_secs(300));
        assert!(
            !s.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .contains_key("expired")
        );
    }

    #[tokio::test]
    async fn recovering_cache_elects_owner_and_replays_local_entries() {
        // 模拟原 owner 已退出且端口已经释放：当前节点先在本地登记，再后台抢占端口。
        // 新 owner 就绪时，本地热键必须回灌到共享服务，其他节点才能继续命中。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);

        let cache = ClusterCache::recovering(vec![addr.clone()]);
        cache
            .register("registered-during-outage", Duration::from_secs(300))
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut fresh = None;
        while tokio::time::Instant::now() < deadline {
            if cache.role() == "owner" {
                fresh = try_connect(&addr).await;
                if fresh.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let conn = fresh.expect("应在后台接管已释放的共享缓存端口");
        assert_eq!(cache.role(), "owner");
        assert_eq!(
            redis_exists(conn.clone(), "registered-during-outage").await,
            Some(true),
            "接管后应把故障窗口内的本地登记回灌到共享服务"
        );

        cache
            .register("registered-after-recovery", Duration::from_secs(300))
            .await;
        assert_eq!(
            redis_exists(conn, "registered-after-recovery").await,
            Some(true),
            "恢复后的新登记应直接写入共享服务"
        );
    }

    #[tokio::test]
    async fn recovery_read_waits_for_another_nodes_replay() {
        // 模拟本节点刚连上空的新 owner，而另一个存活节点稍后才回灌热键。本节点应在
        // 同一次读取里等到该键出现，不能先向客户虚报一次 cache_creation。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_embedded_server(listener);
        tokio::time::sleep(Duration::from_millis(25)).await;

        let cache = ClusterCache::shared(
            try_connect(&addr).await.expect("connect cache client"),
            ROLE_CLIENT,
            vec![addr.clone()],
        );
        let replay_conn = try_connect(&addr).await.expect("connect replay client");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            assert_eq!(
                redis_register(replay_conn, "late-replayed-key", Duration::from_secs(300)).await,
                Some(())
            );
        });

        let (backend, fallback) = match &cache {
            ClusterCache::Shared { backend, fallback } => (backend, fallback),
            ClusterCache::Local(_) => unreachable!(),
        };
        assert!(
            exists_after_recovery(backend, fallback, "late-replayed-key").await,
            "应等待其他节点完成回灌后判为 cache_read"
        );
    }
}
