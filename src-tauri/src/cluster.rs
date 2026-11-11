//! 局域网集群控制面：Leader 开 HTTP 服务，Follower 主动加入并拉任务/文件。
//!
//! 协议刻意保持很小，
//! - 加入：PBKDF2 口令派生 + 挑战应答，密码不上线，
//! - 成员：token 认证、心跳、上报实例状态；
//! - 任务：Leader 排队，Follower 拉取，进度回传；
//! - 数据：只读artifact/model 端点，路径必须在清单白名单里。
//!
//! HTTP 服务没有引入框架，直接基于`TcpListener`。请求体上限、读超时和
//! Range 都在这一层控制，避免同步大模型时把内存吃满。
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::config;
use crate::error::AppError;
use crate::state::AppState;
use crate::sync::{self, SyncJob, SyncManifest, SyncReport};

/// 2：同步请求只指定客户机，由目标机选择实例并确认（v1 是源端直接指定实例）。
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_PORT: u16 = 47821;
const TOKEN_HEADER: &str = "x-comfyfleet-token";
const JOIN_ITERATIONS: u32 = 210_000;
const MAX_BODY: usize = 64 * 1024 * 1024;
/// 未认证的 join/begin 会在内存里留下 pending 项；限制名称长度和项数，
/// 避免只发 begin 不完成加入把进程内存耗光。
const MAX_MEMBER_NAME: usize = 120;
const MAX_PENDING_JOINS: usize = 64;
const MAX_RATE_HOSTS: usize = 1024;
const STALE_JOB: Duration = Duration::from_secs(15 * 60);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 64;
/// 单个来源 IP 的连接上限。只看全局 64 条时，局域网里一台机器就能用
/// 慢速/半截请求把控制面占满；单机 16 条足够正常心跳和并发下载。
const MAX_CONNECTIONS_PER_IP: usize = 16;
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

struct ConnectionGuard;

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 和 ConnectionGuard 一起放在连接线程里；连接结束/ panic 都会归还本 IP 名额。
struct HostConnectionGuard {
    shared: Arc<LeaderShared>,
    host: IpAddr,
}

impl Drop for HostConnectionGuard {
    fn drop(&mut self) {
        let mut hosts = self.shared.active_hosts.lock();
        if let Some(count) = hosts.get_mut(&self.host) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                hosts.remove(&self.host);
            }
        }
    }
}

/// 预认证请求也会先读 body；给所有在途请求体设总量上限，
/// 避免 64 条连接 × 64MB 把桌面进程直接撑爆。
const MAX_INFLIGHT_BODY: usize = 128 * 1024 * 1024;
const MAX_JOIN_BODY: usize = 16 * 1024;
static INFLIGHT_BODY: AtomicUsize = AtomicUsize::new(0);

struct BodyBudget(usize);

impl BodyBudget {
    fn reserve(len: usize) -> Result<Self, AppError> {
        if len == 0 {
            return Ok(Self(0));
        }
        let prev = INFLIGHT_BODY.fetch_add(len, Ordering::Relaxed);
        if prev.saturating_add(len) > MAX_INFLIGHT_BODY {
            INFLIGHT_BODY.fetch_sub(len, Ordering::Relaxed);
            return Err(AppError::new("HTTP", "同时在途的请求体过多，稍后再试"));
        }
        Ok(Self(len))
    }
}

impl Drop for BodyBudget {
    fn drop(&mut self) {
        INFLIGHT_BODY.fetch_sub(self.0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- 持久化模型
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterConfig {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub port: u16,
    /// false（默认）只监听 127.0.0.1；显式勾选后才监听 0.0.0.0。
    pub allow_lan: bool,
    pub salt: String,
    /// PBKDF2 派生出的 challenge key；不是明文密码。
    pub join_key: String,
    pub iterations: u32,
    pub created_at: String,
    pub members: Vec<Member>,
    pub jobs: Vec<SyncJob>,
    /// 每个本机源实例最近一次成功同步时的快照，用来展示「自上次同步的变化」。
    #[serde(default)]
    pub last_syncs: HashMap<String, LastSync>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LastSync {
    pub at: String,
    pub generation_id: String,
    pub source: sync::InstanceState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub app_version: String,
    pub token_sha256: String,
    pub joined_at: String,
    pub last_seen: String,
    pub online: bool,
    pub model_store: String,
    /// 心跳上报的实例状态。不落盘：Leader 重启后等一次心跳即可恢复。
    #[serde(skip)]
    pub instances: Vec<sync::InstanceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberConfig {
    pub version: u32,
    pub cluster_id: String,
    pub cluster_name: String,
    pub leader_host: String,
    pub leader_port: u16,
    pub member_id: String,
    pub member_name: String,
    pub token: String,
    pub joined_at: String,
}

impl MemberConfig {
    pub fn base_url(&self) -> String {
        let host = self.leader_host.trim();
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        format!("http://{host}:{}", self.leader_port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterView {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub allow_lan: bool,
    pub created_at: String,
    pub addresses: Vec<String>,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberView {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub app_version: String,
    pub online: bool,
    pub last_seen: String,
    pub joined_at: String,
    pub model_store: String,
    pub instances: Vec<MemberInstanceView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberInstanceView {
    pub id: String,
    pub name: String,
    pub state: String,
    pub core_version: Option<String>,
    pub core_commit: Option<String>,
    pub python_version: String,
    pub node_count: usize,
    pub model_count: usize,
    pub model_bytes: u64,
    /// 计数是否已采集到。Leader 源实例依赖后台发布器，首次采集完成前为 false，
    /// 前端显示“—”而不是误报 0。
    #[serde(default)]
    pub counts_known: bool,
    pub disk_free: u64,
    pub last_applied: Option<sync::LastApplied>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberSelfView {
    pub cluster_id: String,
    pub cluster_name: String,
    pub leader_host: String,
    pub leader_port: u16,
    pub member_id: String,
    pub member_name: String,
    pub online: bool,
    pub last_seen: String,
    pub instances: Vec<MemberInstanceView>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClusterStatus {
    /// none | leader | member
    pub role: String,
    pub cluster: Option<ClusterView>,
    pub member: Option<MemberSelfView>,
    /// Follower 可见的Leader 本机源实例摘要，用于目标端自行比较差异。
    #[serde(default)]
    pub sources: Vec<MemberInstanceView>,
    pub jobs: Vec<SyncJob>,
    pub addresses: Vec<String>,
    pub server_running: bool,
    pub agent_running: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinBeginReq {
    pub member_name: String,
    pub hostname: String,
    pub platform: String,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinBeginResp {
    pub protocol: u32,
    pub cluster_id: String,
    pub cluster_name: String,
    pub salt: String,
    pub iterations: u32,
    pub nonce: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinReq {
    pub member_name: String,
    pub hostname: String,
    pub platform: String,
    pub app_version: String,
    pub nonce: String,
    pub proof: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinResp {
    pub protocol: u32,
    pub cluster_id: String,
    pub cluster_name: String,
    pub member_id: String,
    pub member_name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatReq {
    pub instances: Vec<sync::InstanceState>,
    pub model_store: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NextJobResp {
    pub job: Option<SyncJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ProgressReq {
    pub status: String,
    pub progress: u8,
    pub message: String,
    pub error: Option<String>,
    pub report: Option<SyncReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberStatusResp {
    pub cluster_name: String,
    pub jobs: Vec<SyncJob>,
    /// Leader 本机可作为同步源的实例；Follower 用它做“同步完成后自行校验”。
    #[serde(default)]
    pub sources: Vec<MemberInstanceView>,
}

/// 目标端发起只读差异比较：把自己实例的完整状态带结Leader：
/// Leader 采源实例状态后返回 diff，不创建任务、不写任何目标文件。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CompareReq {
    pub source_instance_id: String,
    pub target_instance_id: String,
    pub target: sync::InstanceState,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CompareResp {
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub target_instance_id: String,
    pub target_instance_name: String,
    pub source_running: bool,
    pub diff: sync::SyncDiff,
}

/// 目标端确认任务时带上自己选定的实例和完整状态。
/// Leader 在这一刻才计算目标差异，源端从此不再需要指定目标实例。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfirmJobReq {
    pub target_instance_id: String,
    pub target: sync::InstanceState,
    /// 目标机自己决定是否清理本机多出来的节点模型；源端不再代选。
    pub options: sync::SyncOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfirmJobResp {
    pub ok: bool,
    pub job: SyncJob,
}

/// 源端页面“自上次同步的变化”。from = 上次成功同步的快照，to = 当前源实例。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceChanges {
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub source_running: bool,
    pub has_baseline: bool,
    pub last_sync_at: Option<String>,
    pub last_sync_generation: Option<String>,
    pub diff: sync::SyncDiff,
}

// ---------------------------------------------------------------- 运行时
/// Follower 的状态由后台 Agent 缓存，`cluster_status` 只读它。
/// 之前状态命令会同步请求 Leader；Leader 没开时每次轮询都卡住 UI 线程。
#[derive(Debug, Clone, Default)]
pub struct FollowerCache {
    pub online: bool,
    pub jobs: Vec<SyncJob>,
    pub sources: Vec<MemberInstanceView>,
    pub error: Option<String>,
    pub checked_at: Option<String>,
}

#[derive(Default)]
pub struct Runtime {
    pub leader: Option<Arc<LeaderShared>>,
    pub agent: Option<AgentHandle>,
    pub error: Option<String>,
    pub follower: FollowerCache,
}

pub struct LeaderShared {
    pub config: Mutex<ClusterConfig>,
    pub pending: Mutex<HashMap<String, PendingJoin>>,
    pub attempts: Mutex<HashMap<String, (u32, Instant)>>,
    pub stop: AtomicBool,
    /// Leader 源实例的只读快照；由后台发布器定期刷新，避免把AppHandle 放进核心结构。
    pub source_states: Mutex<HashMap<String, (sync::InstanceState, bool)>>,
    pub publisher_started: AtomicBool,
    /// 每个任务最近一次落盘时间。目标端进度是秒级轮询，若每次都重写整份
    /// cluster.json（含所有任务日志），会持续放大磁盘写；只做有上限的节流。
    pub progress_saves: Mutex<HashMap<String, Instant>>,
    /// 每个来源 IP 的当前连接数，用于拒绝单机占满全局连接池。
    active_hosts: Mutex<HashMap<IpAddr, usize>>,
}

pub(crate) struct PendingJoin {
    member_name: String,
    expires: Instant,
}

pub struct AgentHandle {
    pub stop: Arc<AtomicBool>,
}

// ---------------------------------------------------------------- 路径

pub fn cluster_path() -> PathBuf {
    config::app_root().join("cluster.json")
}

pub fn member_path() -> PathBuf {
    config::app_root().join("member.json")
}

pub fn load_cluster() -> Option<ClusterConfig> {
    let raw = std::fs::read_to_string(cluster_path()).ok()?;
    let mut cfg: ClusterConfig = serde_json::from_str(&raw).ok()?;
    // 重启后所有成员先视为离线，等目标 Agent 心跳。
    for m in &mut cfg.members {
        m.online = false;
    }
    Some(cfg)
}

pub fn save_cluster(cfg: &ClusterConfig) -> Result<(), AppError> {
    config::atomic_write(
        &cluster_path(),
        serde_json::to_string_pretty(cfg)?.as_bytes(),
    )
}

pub fn load_member() -> Option<MemberConfig> {
    let raw = std::fs::read_to_string(member_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_member(cfg: &MemberConfig) -> Result<(), AppError> {
    config::atomic_write(
        &member_path(),
        serde_json::to_string_pretty(cfg)?.as_bytes(),
    )
}

// ---------------------------------------------------------------- 口令与token

fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        let id = uuid::Uuid::new_v4().simple().to_string();
        out.push_str(&id);
    }
    out.truncate(bytes * 2);
    out
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let mut h = Sha256::new();
        h.update(key);
        k[..32].copy_from_slice(&h.finalize());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let out = outer.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

fn pbkdf2(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password.as_bytes(), &block);
    let mut out = u;
    for _ in 1..iterations.max(1) {
        u = hmac_sha256(password.as_bytes(), &u);
        for i in 0..32 {
            out[i] ^= u[i];
        }
    }
    out
}

fn proof_for(join_key: &[u8], nonce: &str, member_name: &str) -> String {
    hex::encode(hmac_sha256(
        join_key,
        format!("{nonce}|{member_name}").as_bytes(),
    ))
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------- 本地地址

pub fn local_addresses() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                out.push(addr.ip().to_string());
            }
        }
    }
    out.push("127.0.0.1".to_string());
    out.sort();
    out.dedup();
    out
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

// ---------------------------------------------------------------- Leader 服务

pub fn create_cluster(
    name: &str,
    password: &str,
    port: u16,
    allow_lan: bool,
) -> Result<Arc<LeaderShared>, AppError> {
    if cluster_path().exists() {
        return Err(AppError::new("CLUSTER_EXISTS", "本机已经创建过集群"));
    }
    if member_path().exists() {
        return Err(AppError::new(
            "MEMBER_EXISTS",
            "本机已加入其他集群，先退出再加入/创建",
        ));
    }
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::new("CLUSTER_NAME", "集群名称不能为空"));
    }
    if password.chars().count() < 8 {
        return Err(AppError::new(
            "CLUSTER_PASSWORD",
            "集群密码至少 8 个字符（局域网内会用它派生加入密钥，",
        ));
    }
    if port < 1024 {
        return Err(AppError::new("CLUSTER_PORT", "端口需大于等于 1024"));
    }
    let salt = random_hex(16);
    let key = pbkdf2(
        password,
        &hex::decode(&salt).unwrap_or_default(),
        JOIN_ITERATIONS,
    );
    let cfg = ClusterConfig {
        version: PROTOCOL_VERSION,
        id: random_hex(8),
        name: name.to_string(),
        port,
        allow_lan,
        salt,
        join_key: hex::encode(key),
        iterations: JOIN_ITERATIONS,
        created_at: sync::now(),
        members: Vec::new(),
        jobs: Vec::new(),
        last_syncs: HashMap::new(),
    };
    save_cluster(&cfg)?;
    match start_server(cfg) {
        Ok(shared) => Ok(shared),
        Err(e) => {
            let _ = std::fs::remove_file(cluster_path());
            Err(e)
        }
    }
}

pub fn start_server(cfg: ClusterConfig) -> Result<Arc<LeaderShared>, AppError> {
    let mut port = cfg.port;
    let bind_host = if cfg.allow_lan {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let listener = TcpListener::bind((bind_host, port)).map_err(|e| {
        AppError::new(
            "CLUSTER_PORT",
            format!("集群端口 {port} 监听失败，{e}\n检查端口占用和 Windows 防火墙"),
        )
    })?;
    if port == 0 {
        if let Ok(addr) = listener.local_addr() {
            port = addr.port();
        }
    }
    let mut cfg = cfg;
    cfg.port = port;
    let _ = listener.set_nonblocking(true);
    let shared = Arc::new(LeaderShared {
        config: Mutex::new(cfg),
        pending: Mutex::new(HashMap::new()),
        attempts: Mutex::new(HashMap::new()),
        stop: AtomicBool::new(false),
        source_states: Mutex::new(HashMap::new()),
        publisher_started: AtomicBool::new(false),
        progress_saves: Mutex::new(HashMap::new()),
        active_hosts: Mutex::new(HashMap::new()),
    });
    let srv = Arc::clone(&shared);
    std::thread::spawn(move || {
        log::info!("集群服务已启动：{bind_host}:{port}");
        while !srv.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    // 无认证连接也走这里；必须在上限处直接拒绝，否则慢速/空闲
                    // 连接会无限堆线程，把控制面拖死。
                    if ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
                        log::warn!("集群连接数已达上限，拒绝 {addr}");
                        drop(stream);
                        continue;
                    }
                    let host = addr.ip();
                    let host_over_limit = {
                        let mut hosts = srv.active_hosts.lock();
                        let count = hosts.entry(host).or_insert(0);
                        if *count >= MAX_CONNECTIONS_PER_IP {
                            true
                        } else {
                            *count += 1;
                            false
                        }
                    };
                    if host_over_limit {
                        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
                        log::warn!("单个来源连接数已达上限，拒绝 {addr}");
                        drop(stream);
                        continue;
                    }
                    let shared = Arc::clone(&srv);
                    let host_guard = HostConnectionGuard {
                        shared: Arc::clone(&srv),
                        host,
                    };
                    std::thread::spawn(move || {
                        let _host_guard = host_guard;
                        let _guard = ConnectionGuard;
                        if let Err(e) = handle_connection(shared, stream, addr) {
                            log::warn!("集群请求处理失败（{addr}）：{}", e.message);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(80));
                }
                Err(e) => {
                    log::warn!("集群 accept 失败，{e}");
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
        log::info!("集群服务已停止");
    });
    Ok(shared)
}

pub fn stop_runtime(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if let Some(server) = &rt.leader {
        server.stop.store(true, Ordering::Relaxed);
    }
    if let Some(agent) = &rt.agent {
        agent.stop.store(true, Ordering::Relaxed);
    }
    rt.leader = None;
    rt.agent = None;
    rt.follower = FollowerCache::default();
}

pub fn remove_cluster() -> Result<(), AppError> {
    let _ = std::fs::remove_file(cluster_path());
    let _ = std::fs::remove_file(member_path());
    Ok(())
}

/// 删除集群前在本机校验 Leader 密码；密码不会离开这台机器。
pub fn verify_password(password: &str) -> Result<(), AppError> {
    let cfg = load_cluster().ok_or_else(|| AppError::new("CLUSTER_NONE", "本机不是集群 Leader"))?;
    if password.is_empty() {
        return Err(AppError::new("CLUSTER_PASSWORD", "集群密码错误"));
    }
    let salt = hex::decode(&cfg.salt)
        .map_err(|_| AppError::new("CLUSTER_CONFIG", "集群配置损坏，无法校验密码"))?;
    let expected = pbkdf2(password, &salt, cfg.iterations);
    if constant_eq(&hex::encode(expected), &cfg.join_key) {
        Ok(())
    } else {
        Err(AppError::new("CLUSTER_PASSWORD", "集群密码错误"))
    }
}

// ---------------------------------------------------------------- 请求处理

struct HttpReq {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    remote: String,
    _body_budget: BodyBudget,
}

fn read_request(stream: &mut TcpStream, remote: String) -> Result<HttpReq, AppError> {
    // Windows 一accept 出的 socket 可能继承 listener 的非阻塞状态；下载大模型时
    stream.set_nonblocking(false).ok();
    stream.set_nodelay(true).ok();
    // 请求头用短超时，避免未认证连接只发一半就永久占线程；收到头之后再放宽
    // 到请求体超时，给大模型上传留时间。
    stream.set_read_timeout(Some(HEADER_TIMEOUT)).ok();
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if buf.len() > 64 * 1024 {
            return Err(AppError::new("HTTP", "请求头过大"));
        }
        if let Some(pos) = find_bytes(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream
            .read(&mut tmp)
            .map_err(|e| AppError::new("HTTP", e.to_string()))?;
        if n == 0 {
            return Err(AppError::new("HTTP", "连接提前关闭"));
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((&target, ""));
    let path = percent_decode(raw_path);
    let mut query = HashMap::new();
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let body_cap = if path == "/api/v1/join/begin" || path == "/api/v1/join" {
        MAX_JOIN_BODY
    } else {
        MAX_BODY
    };
    if len > body_cap {
        return Err(AppError::new("HTTP", "请求体过大"));
    }
    let budget = BodyBudget::reserve(len)?;
    stream.set_read_timeout(Some(BODY_TIMEOUT)).ok();
    let mut body = buf[header_end..].to_vec();
    while body.len() < len {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| AppError::new("HTTP", e.to_string()))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(HttpReq {
        method,
        path,
        headers,
        body,
        remote,
        _body_budget: budget,
    })
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn percent_decode(raw: &str) -> String {
    // 必须始终在字节上解码：`&raw[i + 1..i + 3]` 会在 `%` 后跟多字节 UTF-8
    // 时切在字符中间，直接 panic（release 下 panic=abort 会杀掉整个应用）。
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn json_response<T: Serialize>(stream: &mut TcpStream, status: u16, value: &T) {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let _ = write_response(
        stream,
        status,
        "application/json; charset=utf-8",
        &[],
        &body,
    );
}

fn error_response(stream: &mut TcpStream, status: u16, code: &str, message: &str) {
    let body = serde_json::json!({"code": code, "message": message});
    json_response(stream, status, &body);
}

fn body_json<T: DeserializeOwned>(req: &HttpReq) -> Result<T, AppError> {
    serde_json::from_slice(&req.body)
        .map_err(|e| AppError::new("HTTP_BODY", format!("请求 JSON 无效，{e}")))
}

fn header<'a>(req: &'a HttpReq, name: &str) -> Option<&'a str> {
    req.headers.get(name).map(String::as_str)
}

fn handle_connection(
    shared: Arc<LeaderShared>,
    mut stream: TcpStream,
    addr: std::net::SocketAddr,
) -> Result<(), AppError> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
    let req = match read_request(&mut stream, addr.ip().to_string()) {
        Ok(r) => r,
        Err(e) => {
            error_response(&mut stream, 400, &e.code, &e.message);
            return Ok(());
        }
    };
    // 控制面只服务本应用的 Rust 客户端，不应该被浏览器页面调用。
    // 浏览器 POST 会带 Origin；拒绝它可以挡住 DNS rebinding / CSRF 探测。
    if req.headers.contains_key("origin") {
        error_response(
            &mut stream,
            403,
            "CLUSTER_BROWSER",
            "不接受浏览器来源的请求",
        );
        return Ok(());
    }
    if req.method == "POST"
        && !header(&req, "content-type")
            .map(|v| v.to_ascii_lowercase().starts_with("application/json"))
            .unwrap_or(false)
    {
        // 强制非简单请求类型，避免 text/plain 表单绕过预检。
        error_response(
            &mut stream,
            415,
            "CLUSTER_CONTENT",
            "请求必须是 application/json",
        );
        return Ok(());
    }
    log::debug!("集群 {} {}（{}）", req.method, req.path, req.remote);
    let result = route(&shared, &req, &mut stream);
    if let Err(e) = result {
        error_response(&mut stream, 500, &e.code, &e.message);
    }
    Ok(())
}

fn route(
    shared: &Arc<LeaderShared>,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    if req.method == "GET" && req.path == "/api/v1/ping" {
        let name = {
            let cfg = shared.config.lock();
            cfg.name.clone()
        };
        json_response(
            stream,
            200,
            &serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "cluster": name,
            }),
        );
        return Ok(());
    }
    if req.method == "POST" && req.path == "/api/v1/join/begin" {
        return join_begin(shared, req, stream);
    }
    if req.method == "POST" && req.path == "/api/v1/join" {
        return join_complete(shared, req, stream);
    }

    let Some(member) = authenticate(shared, req) else {
        error_response(stream, 401, "CLUSTER_AUTH", "缺少或无效的成员 token");
        return Ok(());
    };

    if req.method == "POST" && req.path == "/api/v1/member/heartbeat" {
        return heartbeat(shared, &member, req, stream);
    }
    if req.method == "GET" && req.path == "/api/v1/member/next" {
        return next_job(shared, &member, stream);
    }
    if req.method == "GET" && req.path == "/api/v1/member/status" {
        return handle_member_status(shared, &member, stream);
    }
    if req.method == "POST" && req.path == "/api/v1/member/compare" {
        return compare_source(shared, &member, req, stream);
    }
    if req.method == "POST" && req.path.starts_with("/api/v1/member/job/") {
        let rest = req.path.trim_start_matches("/api/v1/member/job/");
        if let Some(job_id) = rest.strip_suffix("/confirm") {
            return confirm_job(shared, &member, job_id, req, stream);
        }
        if let Some(job_id) = rest.strip_suffix("/reject") {
            return reject_job(shared, &member, job_id, stream);
        }
        if let Some(job_id) = rest.strip_suffix("/cancel") {
            return target_cancel_job(shared, &member, job_id, stream);
        }
        if let Some(job_id) = rest.strip_suffix("/retry") {
            return retry_job(shared, &member, job_id, stream);
        }
        if let Some(job_id) = rest.strip_suffix("/progress") {
            return progress(shared, &member, job_id, req, stream);
        }
    }
    if req.method == "GET" && req.path.starts_with("/api/v1/artifact/") {
        return serve_artifact(shared, &member, req, stream);
    }
    if req.method == "GET" && req.path.starts_with("/api/v1/model/") {
        return serve_model(shared, &member, req, stream);
    }
    if req.method == "POST" && req.path == "/api/v1/member/leave" {
        let mut cfg = shared.config.lock();
        cfg.members.retain(|m| m.id != member.id);
        save_cluster(&cfg)?;
        drop(cfg);
        json_response(stream, 200, &serde_json::json!({"ok": true}));
        return Ok(());
    }
    error_response(stream, 404, "HTTP_ROUTE", "未知接口");
    Ok(())
}

fn clean_member_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty()
        || name.chars().count() > MAX_MEMBER_NAME
        || name.chars().any(|c| c.is_control())
    {
        return None;
    }
    Some(name.to_string())
}

fn join_begin(
    shared: &Arc<LeaderShared>,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    if !allow_attempt(shared, &req.remote) {
        error_response(stream, 429, "CLUSTER_RATE", "尝试太频繁，稍后再试");
        return Ok(());
    }
    let body: JoinBeginReq = body_json(req)?;
    let Some(member_name) = clean_member_name(&body.member_name) else {
        error_response(
            stream,
            400,
            "CLUSTER_NAME",
            &format!("成员名称不能为空且最多 {MAX_MEMBER_NAME} 个字符"),
        );
        return Ok(());
    };
    let now = Instant::now();
    let nonce = random_hex(16);
    // 只读取集群元数据，锁内不做 socket 写；未认证请求不能阻塞控制面。
    let (cluster_id, cluster_name, salt, iterations) = {
        let cfg = shared.config.lock();
        (
            cfg.id.clone(),
            cfg.name.clone(),
            cfg.salt.clone(),
            cfg.iterations,
        )
    };
    {
        let mut pending = shared.pending.lock();
        pending.retain(|_, p| p.expires > now);
        if pending.len() >= MAX_PENDING_JOINS {
            // 不要把 socket 写操作留在 pending 锁内：慢客户端会拖住所有加入请求。
            drop(pending);
            error_response(
                stream,
                429,
                "CLUSTER_PENDING",
                "待确认的加入请求太多，稍后再试",
            );
            return Ok(());
        }
        pending.insert(
            nonce.clone(),
            PendingJoin {
                member_name,
                expires: now + Duration::from_secs(120),
            },
        );
    }
    json_response(
        stream,
        200,
        &JoinBeginResp {
            protocol: PROTOCOL_VERSION,
            cluster_id,
            cluster_name,
            salt,
            iterations,
            nonce,
            expires_in: 120,
        },
    );
    Ok(())
}

fn join_complete(
    shared: &Arc<LeaderShared>,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    if !allow_attempt(shared, &req.remote) {
        error_response(stream, 429, "CLUSTER_RATE", "尝试太频繁，稍后再试");
        return Ok(());
    }
    let body: JoinReq = body_json(req)?;
    let Some(member_name) = clean_member_name(&body.member_name) else {
        error_response(stream, 400, "CLUSTER_NAME", "成员名称不合法");
        return Ok(());
    };
    let pending = {
        let mut map = shared.pending.lock();
        let item = map.remove(&body.nonce);
        item.filter(|p| p.expires > Instant::now() && p.member_name == member_name)
    };
    let Some(_pending) = pending else {
        error_response(stream, 401, "CLUSTER_JOIN", "加入请求已过期或名称不匹配");
        return Ok(());
    };
    let key = {
        let cfg = shared.config.lock();
        // 配置损坏时必须 fail-closed：空 key 会让任何人都能算出 proof。
        let key = hex::decode(&cfg.join_key)
            .map_err(|_| AppError::new("CLUSTER_CONFIG", "集群加入密钥已损坏，请重建集群"))?;
        if key.len() != 32 {
            return Err(AppError::new(
                "CLUSTER_CONFIG",
                "集群加入密钥长度不合法，请重建集群",
            ));
        }
        key
    };
    let want = proof_for(&key, &body.nonce, &member_name);
    if !constant_eq(&want, &body.proof) {
        error_response(stream, 401, "CLUSTER_PASSWORD", "集群密码错误");
        return Ok(());
    }
    let token = random_hex(32);
    let member = Member {
        id: random_hex(8),
        name: member_name,
        hostname: sync::short_chars(&body.hostname, 64),
        platform: sync::short_chars(&body.platform, 64),
        app_version: sync::short_chars(&body.app_version, 32),
        token_sha256: token_hash(&token),
        joined_at: sync::now(),
        last_seen: sync::now(),
        online: true,
        model_store: String::new(),
        instances: Vec::new(),
    };
    let (cluster_id, cluster_name) = {
        let mut cfg = shared.config.lock();
        if cfg
            .members
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(&member.name))
        {
            drop(cfg);
            error_response(
                stream,
                409,
                "CLUSTER_MEMBER",
                "同名成员已存在，请换一个名称",
            );
            return Ok(());
        }
        cfg.members.push(member.clone());
        save_cluster(&cfg)?;
        (cfg.id.clone(), cfg.name.clone())
    };
    json_response(
        stream,
        200,
        &JoinResp {
            protocol: PROTOCOL_VERSION,
            cluster_id,
            cluster_name,
            member_id: member.id,
            member_name: member.name,
            token,
        },
    );
    Ok(())
}

fn allow_attempt(shared: &Arc<LeaderShared>, remote: &str) -> bool {
    let mut map = shared.attempts.lock();
    let now = Instant::now();
    if map.len() >= MAX_RATE_HOSTS && !map.contains_key(remote) {
        map.retain(|_, (_, at)| now.duration_since(*at) <= Duration::from_secs(60));
        if map.len() >= MAX_RATE_HOSTS {
            return false;
        }
    }
    let entry = map.entry(remote.to_string()).or_insert((0, now));
    if now.duration_since(entry.1) > Duration::from_secs(60) {
        *entry = (1, now);
        return true;
    }
    entry.0 += 1;
    entry.0 <= 12
}

fn authenticate(shared: &Arc<LeaderShared>, req: &HttpReq) -> Option<Member> {
    let token = header(req, TOKEN_HEADER)?;
    let hash = token_hash(token);
    let cfg = shared.config.lock();
    cfg.members
        .iter()
        .find(|m| constant_eq(&m.token_sha256, &hash))
        .cloned()
}

fn heartbeat(
    shared: &Arc<LeaderShared>,
    member: &Member,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let body: HeartbeatReq = body_json(req)?;
    if body.instances.len() > 256 || body.model_store.chars().count() > 4096 {
        error_response(stream, 400, "CLUSTER_HEARTBEAT", "心跳内容超出限制");
        return Ok(());
    }
    let mut cfg = shared.config.lock();
    let mut persist = false;
    if let Some(m) = cfg.members.iter_mut().find(|m| m.id == member.id) {
        // instances 本身不落盘（Member 上是 serde(skip)），online / last_seen
        // 重启后也会被 load_cluster 重置；只有 model_store 变化需要写盘。
        // 之前每次心跳都把整份 cluster.json（含全部任务日志）重写一遍，
        // 多台成员时会持续产生大量磁盘写。
        if m.model_store != body.model_store {
            m.model_store = body.model_store;
            persist = true;
        }
        m.online = true;
        m.last_seen = sync::now();
        m.instances = body.instances;
    }
    if persist {
        save_cluster(&cfg)?;
    }
    drop(cfg);
    json_response(stream, 200, &serde_json::json!({"ok": true}));
    Ok(())
}

fn next_job(
    shared: &Arc<LeaderShared>,
    member: &Member,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let mut cfg = shared.config.lock();
    let mut dirty = false;
    // 目标崩溃/断线：超过租期还没进度的任务重新排队。
    for job in cfg.jobs.iter_mut() {
        if job.target_member_id == member.id && job.status == sync::JOB_RUNNING {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&job.updated_at) {
                let age =
                    chrono::Local::now().signed_duration_since(t.with_timezone(&chrono::Local));
                if age.num_seconds() > STALE_JOB.as_secs() as i64 {
                    job.status = sync::JOB_PENDING.to_string();
                    job.started_at = None;
                    job.finished_at = None;
                    job.message = "目标端超时，任务重新排队".to_string();
                    job.logs
                        .push(sync::job_log("目标端超时没有上报进度，任务已重新排队"));
                    dirty = true;
                }
            }
        }
    }
    let job = cfg
        .jobs
        .iter_mut()
        .find(|j| j.target_member_id == member.id && j.status == sync::JOB_PENDING);
    let out = if let Some(job) = job {
        job.status = sync::JOB_RUNNING.to_string();
        job.started_at = Some(sync::now());
        job.finished_at = None;
        job.updated_at = sync::now();
        job.message = "目标端已领取任务".to_string();
        job.logs.push(sync::job_log("目标端已领取任务并开始执行"));
        dirty = true;
        Some(job.clone())
    } else {
        None
    };
    if dirty {
        save_cluster(&cfg)?;
    }
    drop(cfg);
    json_response(stream, 200, &NextJobResp { job: out });
    Ok(())
}

fn confirm_job(
    shared: &Arc<LeaderShared>,
    member: &Member,
    job_id: &str,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let body: ConfirmJobReq = body_json(req)?;
    if body.target_instance_id.trim().is_empty()
        || body.target.instance_id != body.target_instance_id
    {
        error_response(stream, 400, "SYNC_TARGET", "请选择本机要接收同步的实例");
        return Ok(());
    }
    if !member
        .instances
        .iter()
        .any(|i| i.instance_id == body.target_instance_id)
    {
        error_response(
            stream,
            409,
            "SYNC_TARGET",
            "这个实例还没有上报到 Leader，等心跳刷新后再确认",
        );
        return Ok(());
    }

    let mut cfg = shared.config.lock();
    let Some(job) = cfg
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id && j.target_member_id == member.id)
    else {
        drop(cfg);
        error_response(stream, 404, "CLUSTER_JOB", "任务不存在");
        return Ok(());
    };
    match job.status.as_str() {
        sync::JOB_AWAITING_CONFIRMATION => {}
        sync::JOB_PENDING | sync::JOB_RUNNING => {
            // 重复确认按幂等处理，避免目标端网络重试把任务变成错误。
            let out = job.clone();
            drop(cfg);
            json_response(stream, 200, &ConfirmJobResp { ok: true, job: out });
            return Ok(());
        }
        _ => {
            drop(cfg);
            error_response(stream, 409, "CLUSTER_JOB", "任务已经结束，不能再确认");
            return Ok(());
        }
    }

    let manifest = sync::load_manifest(&job.generation_id)?;
    job.scope = manifest.scope;
    let mut diff = sync::diff_states_with_scope(&manifest.source, &body.target, job.scope);
    diff.target_drift = false;
    if !diff.python.missing_wheels.is_empty() {
        let detail: Vec<String> = diff
            .python
            .missing_wheels
            .iter()
            .map(|w| {
                let reason = w
                    .reason
                    .as_deref()
                    .unwrap_or("没有可用的 wheel，也没有记录下载来源");
                format!("{}（{reason}）", w.name)
            })
            .collect();
        // 缺包不阻断整次同步：目标端会跳过这些包，完成后在报告里统一提示。
        sync::push_job_log(
            job,
            &format!(
                "警告：以下自定义包缺少可同步的 wheel，本次跳过：{}",
                detail.join("；")
            ),
        );
    }
    let compat: Vec<String> = diff
        .python
        .incompatible_wheels
        .iter()
        .filter(|w| w.available)
        .map(|w| {
            format!(
                "{}（{})",
                w.name,
                w.compat_warning.as_deref().unwrap_or("环境不兼容")
            )
        })
        .collect();
    if !compat.is_empty() {
        sync::push_job_log(
            job,
            &format!(
                "提示：以下自定义包与源环境不兼容，按“目标与源完全一致”策略仍会安装：{}",
                compat.join("；")
            ),
        );
    }
    let needed = sync::required_space(&diff, &manifest.totals);
    if body.target.disk_free > 0 && needed > body.target.disk_free {
        drop(cfg);
        error_response(
            stream,
            409,
            "SYNC_DISK",
            &format!(
                "目标磁盘空间不足：预计需要{}，目标只前{}",
                sync::human_bytes(needed),
                sync::human_bytes(body.target.disk_free)
            ),
        );
        return Ok(());
    }

    job.target_instance_id = body.target.instance_id.clone();
    job.target_instance_name = body.target.name.clone();
    // 清理开关由目标机点确认时决定；源端请求里的值不参与。
    job.options.remove_extra_nodes = body.options.remove_extra_nodes;
    job.options.remove_extra_models = body.options.remove_extra_models;
    // 目标机既然明确选择了这个实例，就按源端完整状态覆盖；不再用目标漂移拦截。
    job.options.allow_dirty_target = true;
    job.target_fingerprint = sync::state_fingerprint_scoped(&body.target, job.scope);
    job.diff = diff;
    job.target_online = true;
    job.status = sync::JOB_PENDING.to_string();
    job.accepted_at = Some(sync::now());
    job.started_at = None;
    job.finished_at = None;
    job.progress = 0;
    job.error = None;
    job.target_cancel_requested = false;
    job.message = "目标机已接受，等待执行".to_string();
    job.updated_at = sync::now();
    job.logs.push(sync::job_log(&format!(
        "目标机选择实例「{}」并接受同步",
        job.target_instance_name
    )));
    let cleanup_choice = if job.options.remove_extra_nodes || job.options.remove_extra_models {
        format!(
            "目标机选择清理：多出的节点{}，多出的模型{}",
            if job.options.remove_extra_nodes {
                "删除"
            } else {
                "保留"
            },
            if job.options.remove_extra_models {
                "删除"
            } else {
                "保留"
            }
        )
    } else {
        "目标机选择保留本机多出的节点和模型".to_string()
    };
    job.logs.push(sync::job_log(&cleanup_choice));
    job.logs.push(sync::job_log(&format!(
        "同步范围：本体{} · 节点 +{} ~{} · 模型 +{} ~{} / {}",
        if job.diff.core.changed {
            "变化"
        } else {
            "不变"
        },
        job.diff.nodes.added.len(),
        job.diff.nodes.changed.len(),
        job.diff.models.added.len(),
        job.diff.models.modified.len(),
        sync::human_bytes(job.diff.models.transfer_bytes)
    )));
    let out = job.clone();
    save_cluster(&cfg)?;
    drop(cfg);
    json_response(stream, 200, &ConfirmJobResp { ok: true, job: out });
    Ok(())
}

fn reject_job(
    shared: &Arc<LeaderShared>,
    member: &Member,
    job_id: &str,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let mut cfg = shared.config.lock();
    let Some(job) = cfg
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id && j.target_member_id == member.id)
    else {
        drop(cfg);
        error_response(stream, 404, "CLUSTER_JOB", "任务不存在");
        return Ok(());
    };
    if job.status == sync::JOB_AWAITING_CONFIRMATION {
        job.status = sync::JOB_CANCELLED.to_string();
        job.finished_at = Some(sync::now());
        job.message = "目标端拒绝同步".to_string();
        job.error = Some("目标端拒绝同步".to_string());
        job.updated_at = sync::now();
        job.logs.push(sync::job_log("目标端拒绝了本次同步"));
        save_cluster(&cfg)?;
        drop(cfg);
        json_response(stream, 200, &serde_json::json!({"ok": true}));
    } else if job.status == sync::JOB_CANCELLED {
        drop(cfg);
        json_response(stream, 200, &serde_json::json!({"ok": true}));
    } else {
        drop(cfg);
        error_response(stream, 409, "CLUSTER_JOB", "任务已开始，不能拒绝");
    }
    Ok(())
}

/// 目标端主动取消。它不是源端的终态取消：排队中的任务直接回到“等待接受”，
/// 执行中的任务只发停止请求，等 Agent 在安全点停下后回到“等待接受”。
fn target_cancel_job(
    shared: &Arc<LeaderShared>,
    member: &Member,
    job_id: &str,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let mut cfg = shared.config.lock();
    let Some(job) = cfg
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id && j.target_member_id == member.id)
    else {
        drop(cfg);
        error_response(stream, 404, "CLUSTER_JOB", "任务不存在");
        return Ok(());
    };
    if job.status == sync::JOB_CANCELLED || job.cancel_requested {
        drop(cfg);
        error_response(stream, 409, "SYNC_STOPPED", "源端已经停止本次同步");
        return Ok(());
    }
    match job.status.as_str() {
        sync::JOB_AWAITING_CONFIRMATION => {
            // 还没接受，目标端没有可取消的执行；按幂等返回。
            drop(cfg);
            json_response(stream, 200, &serde_json::json!({ "ok": true }));
            return Ok(());
        }
        sync::JOB_PENDING => {
            job.status = sync::JOB_AWAITING_CONFIRMATION.to_string();
            job.target_cancel_requested = false;
            job.target_cancelled_at = Some(sync::now());
            job.target_cancel_count = job.target_cancel_count.saturating_add(1);
            job.started_at = None;
            job.finished_at = None;
            job.progress = 0;
            job.error = None;
            job.message = "目标端取消，可再次接受".to_string();
            job.updated_at = sync::now();
            job.logs.push(sync::job_log(
                "目标端取消了同步，任务回到等待接受；源端仍可停止本次任务",
            ));
        }
        sync::JOB_RUNNING => {
            if job.target_cancel_requested {
                drop(cfg);
                json_response(stream, 200, &serde_json::json!({ "ok": true }));
                return Ok(());
            }
            job.target_cancel_requested = true;
            job.target_cancelled_at = Some(sync::now());
            job.target_cancel_count = job.target_cancel_count.saturating_add(1);
            job.message = "目标端请求取消，正在安全停止".to_string();
            job.updated_at = sync::now();
            job.logs.push(sync::job_log(
                "目标端请求取消；停止后可再次接受，除非源端停止本次任务",
            ));
        }
        _ => {
            drop(cfg);
            error_response(stream, 409, "CLUSTER_JOB", "任务已经结束，不能取消");
            return Ok(());
        }
    }
    save_cluster(&cfg)?;
    drop(cfg);
    json_response(stream, 200, &serde_json::json!({ "ok": true }));
    Ok(())
}

/// 目标端在失败后请求重试：只把任务放回“等待接受”，由目标端重新确认并领取。
/// 这样会重新抓取目标状态、重新计算差异，不会拿失败前的旧 diff 直接覆盖。
fn retry_job(
    shared: &Arc<LeaderShared>,
    member: &Member,
    job_id: &str,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let mut cfg = shared.config.lock();
    let Some(job) = cfg
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id && j.target_member_id == member.id)
    else {
        drop(cfg);
        error_response(stream, 404, "CLUSTER_JOB", "任务不存在");
        return Ok(());
    };
    if job.cancel_requested {
        drop(cfg);
        error_response(stream, 409, "SYNC_STOPPED", "源端已经停止本次同步");
        return Ok(());
    }
    if job.status != sync::JOB_FAILED && job.status != sync::JOB_PARTIAL {
        drop(cfg);
        error_response(
            stream,
            409,
            "CLUSTER_JOB",
            "只有失败或部分完成的同步可以重试",
        );
        return Ok(());
    }
    job.status = sync::JOB_AWAITING_CONFIRMATION.to_string();
    job.accepted_at = None;
    job.started_at = None;
    job.finished_at = None;
    job.progress = 0;
    job.error = None;
    job.report = None;
    job.target_cancel_requested = false;
    job.target_cancelled_at = None;
    job.target_fingerprint.clear();
    job.message = "目标端请求重试，等待再次接受".to_string();
    job.updated_at = sync::now();
    job.logs
        .push(sync::job_log("目标端请求重试；重新确认后会重新计算差异"));
    save_cluster(&cfg)?;
    drop(cfg);
    json_response(stream, 200, &serde_json::json!({ "ok": true }));
    Ok(())
}

fn progress(
    shared: &Arc<LeaderShared>,
    member: &Member,
    job_id: &str,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let body: ProgressReq = body_json(req)?;
    if body.message.chars().count() > 8192
        || body
            .error
            .as_deref()
            .is_some_and(|e| e.chars().count() > 8192)
    {
        error_response(stream, 400, "CLUSTER_JOB", "进度消息过长");
        return Ok(());
    }
    let mut cfg = shared.config.lock();
    let Some(job) = cfg
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id && j.target_member_id == member.id)
    else {
        drop(cfg);
        error_response(stream, 404, "CLUSTER_JOB", "任务不存在");
        return Ok(());
    };
    // 只接受协议内的状态，且不能把「等待接受」直接改成成功/运行：
    // 目标端必须先确认并领取任务，否则它可以伪造“同步完成”。
    let known = [
        sync::JOB_AWAITING_CONFIRMATION,
        sync::JOB_PENDING,
        sync::JOB_RUNNING,
        sync::JOB_SUCCESS,
        sync::JOB_PARTIAL,
        sync::JOB_FAILED,
        sync::JOB_CANCELLED,
    ];
    if !known.contains(&body.status.as_str()) {
        drop(cfg);
        error_response(stream, 400, "CLUSTER_JOB", "未知的任务状态");
        return Ok(());
    }
    if job.status == sync::JOB_AWAITING_CONFIRMATION
        && (body.status == sync::JOB_SUCCESS
            || body.status == sync::JOB_PARTIAL
            || body.status == sync::JOB_RUNNING)
    {
        drop(cfg);
        error_response(stream, 409, "CLUSTER_JOB", "任务还没有被目标端接受");
        return Ok(());
    }
    if matches!(
        job.status.as_str(),
        sync::JOB_SUCCESS | sync::JOB_PARTIAL | sync::JOB_FAILED
    ) && body.status != job.status
    {
        drop(cfg);
        error_response(stream, 409, "CLUSTER_JOB", "任务已经结束，不能再改状态");
        return Ok(());
    }
    // 取消后目标端仍可能已经完成了落盘：如果它抽success，接受success，避免    // leader 显示“已取消”但目标实际已全部应用。failed 则保留cancelled。
    if job.status == sync::JOB_CANCELLED
        && body.status != sync::JOB_CANCELLED
        && body.status != sync::JOB_SUCCESS
    {
        drop(cfg);
        json_response(
            stream,
            200,
            &serde_json::json!({"ok": true, "cancelled": true}),
        );
        return Ok(());
    }
    // 目标端主动停止：任务回到等待接受，不算终态。源端若已经停止，则保持 cancelled。
    if body.status == sync::JOB_AWAITING_CONFIRMATION {
        if job.cancel_requested || job.status == sync::JOB_CANCELLED {
            drop(cfg);
            json_response(
                stream,
                200,
                &serde_json::json!({"ok": true, "cancelled": true}),
            );
            return Ok(());
        }
        job.status = sync::JOB_AWAITING_CONFIRMATION.to_string();
        job.target_cancel_requested = false;
        if job.target_cancelled_at.is_none() {
            job.target_cancelled_at = Some(sync::now());
        }
        job.started_at = None;
        job.finished_at = None;
        job.progress = 0;
        job.error = None;
        job.report = None;
        sync::push_job_log(job, &body.message);
        job.message = "目标端已取消，可再次接受".to_string();
        job.updated_at = sync::now();
        job.cancel_requested = false;
        save_cluster(&cfg)?;
        drop(cfg);
        json_response(
            stream,
            200,
            &serde_json::json!({"ok": true, "awaiting": true}),
        );
        return Ok(());
    }
    let old_status = job.status.clone();
    job.status = body.status.clone();
    job.progress = body.progress.min(100);
    if job.started_at.is_none() && body.status == sync::JOB_RUNNING {
        job.started_at = Some(sync::now());
    }
    if body.status == sync::JOB_SUCCESS
        || body.status == sync::JOB_PARTIAL
        || body.status == sync::JOB_FAILED
        || body.status == sync::JOB_CANCELLED
    {
        job.finished_at = Some(sync::now());
        job.target_cancel_requested = false;
    }
    sync::push_job_log(job, &body.message);
    job.message = body.message;
    job.updated_at = sync::now();
    if body.status != sync::JOB_RUNNING && body.status != sync::JOB_PENDING {
        job.cancel_requested = false;
    }
    // 先记下是否带了错误/报告，下面 move 进 job 后还要用它决定是否立即落盘。
    let has_error = body.error.is_some();
    let has_report = body.report.is_some();
    if has_error {
        job.error = body.error;
    }
    if has_report {
        job.report = body.report;
    }
    // 目标端每秒都可能上报 running 进度；整份 cluster.json 包含全部任务日志，
    // 不能每来一条就重写。状态切换、错误/报告等里程碑立即写，普通进度最多 15 秒写一次。
    let should_persist = old_status != sync::JOB_RUNNING
        || body.status != sync::JOB_RUNNING
        || has_error
        || has_report
        || shared
            .progress_saves
            .lock()
            .get(job_id)
            .map(|at| at.elapsed() >= Duration::from_secs(15))
            .unwrap_or(true);
    let success_generation = (body.status == sync::JOB_SUCCESS && old_status != sync::JOB_SUCCESS)
        .then(|| job.generation_id.clone());
    if let Some(generation) = success_generation {
        // 记录源端成功同步时的快照。刷新页面后仍能展示「自上次同步的变化」。
        // 和下面的 job 状态一起只落盘一次，避免终态写两遍整份配置。
        if let Ok(manifest) = sync::load_manifest(&generation) {
            cfg.last_syncs.insert(
                manifest.source_instance_id.clone(),
                LastSync {
                    at: sync::now(),
                    generation_id: generation,
                    source: manifest.source,
                },
            );
        }
    }
    if should_persist {
        save_cluster(&cfg)?;
        let mut saves = shared.progress_saves.lock();
        if matches!(
            body.status.as_str(),
            sync::JOB_SUCCESS | sync::JOB_PARTIAL | sync::JOB_FAILED | sync::JOB_CANCELLED
        ) {
            saves.remove(job_id);
        } else {
            saves.insert(job_id.to_string(), Instant::now());
        }
    }
    drop(cfg);
    json_response(stream, 200, &serde_json::json!({"ok": true}));
    Ok(())
}

fn handle_member_status(
    shared: &Arc<LeaderShared>,
    member: &Member,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    // 源实例计数直接读后台发布器缓存，避免每次成员轮询都扫描模型库。
    let sources = {
        let states = shared.source_states.lock();
        local_instance_views(Some(&states))
    };
    // 复制完后立刻释放锁：状态响应包含完整任务日志，慢客户端不能把整个
    // 控制面（心跳、领取任务）卡在配置锁上。
    let (cluster_name, jobs) = {
        let cfg = shared.config.lock();
        (
            cfg.name.clone(),
            cfg.jobs
                .iter()
                .filter(|j| j.target_member_id == member.id)
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    json_response(
        stream,
        200,
        &MemberStatusResp {
            cluster_name,
            jobs,
            sources,
        },
    );
    Ok(())
}

/// 目标端只读比较：Leader 采源实例状态，与目标端上报的本地状态做 diff。
/// 不建任务、不拍快照、不落盘，可随时重复调用，适合同步完成后自行校验。
fn compare_source(
    shared: &Arc<LeaderShared>,
    _member: &Member,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let body: CompareReq = match body_json(req) {
        Ok(v) => v,
        Err(e) => {
            error_response(stream, 400, &e.code, &e.message);
            return Ok(());
        }
    };
    let (source, source_running) = match shared
        .source_states
        .lock()
        .get(&body.source_instance_id)
        .cloned()
    {
        Some(v) => v,
        None => {
            error_response(
                stream,
                404,
                "SYNC_SOURCE",
                "源端实例状态尚未发布，请在源端刷新同步页后重试",
            );
            return Ok(());
        }
    };
    let diff = sync::diff_states(&source, &body.target);
    json_response(
        stream,
        200,
        &CompareResp {
            source_instance_id: source.instance_id.clone(),
            source_instance_name: source.name.clone(),
            target_instance_id: body.target.instance_id.clone(),
            target_instance_name: body.target.name.clone(),
            source_running,
            diff,
        },
    );
    Ok(())
}

fn member_owns_generation(shared: &Arc<LeaderShared>, member_id: &str, generation: &str) -> bool {
    shared
        .config
        .lock()
        .jobs
        .iter()
        .any(|job| job.generation_id == generation && job.target_member_id == member_id)
}

fn serve_artifact(
    shared: &Arc<LeaderShared>,
    member: &Member,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let rest = req.path.trim_start_matches("/api/v1/artifact/");
    let Some((generation, rel)) = rest.split_once('/') else {
        error_response(stream, 404, "SYNC_ARTIFACT", "无效的产物路径");
        return Ok(());
    };
    // 路由层已经对 path 做过一次 percent-decode；这里不能再解一次，
    // 否则 `%252e%252e` 会被还原成 `..`，和安全校验的假设不一致。
    let generation = generation.to_string();
    let rel = rel.to_string();
    if generation.is_empty() || rel.is_empty() || !sync::safe_generation_id(&generation) {
        error_response(stream, 404, "SYNC_ARTIFACT", "无效的产物路径");
        return Ok(());
    }
    // 清单里的白名单字符串本身也必须做词法校验：即使 manifest 被手工改坏，
    // 也不能让 `..\` / 盘符路径逃出代次目录。这是白名单之外的纵深防御。
    if !sync::safe_relative_path(&rel) {
        error_response(stream, 404, "SYNC_ARTIFACT", "无效的产物路径");
        return Ok(());
    }
    // 只允许目标成员读取自己任务的代次，避免任意成员遍历 leader 的历史制品。
    if !member_owns_generation(shared, &member.id, &generation) {
        error_response(stream, 404, "SYNC_ARTIFACT", "产物不存在");
        return Ok(());
    }
    if rel == "manifest.json" {
        let path = sync::generation_dir(&generation).join("manifest.json");
        serve_file(stream, &path, req, "application/json; charset=utf-8")?;
        return Ok(());
    }
    let manifest =
        sync::load_manifest(&generation).map_err(|e| AppError::new("SYNC_ARTIFACT", e.message))?;
    let allowed = artifact_allowed(&manifest, &rel);
    if !allowed {
        error_response(stream, 403, "SYNC_ARTIFACT", "该文件不在同步清单里");
        return Ok(());
    }
    let path = sync::generation_dir(&generation).join(rel.replace('/', "\\"));
    serve_file(stream, &path, req, "application/octet-stream")?;
    Ok(())
}

fn artifact_allowed(manifest: &SyncManifest, rel: &str) -> bool {
    if manifest.source.core.full_archive.as_deref() == Some(rel)
        || manifest.source.core.patch.as_deref() == Some(rel)
        || manifest.source.core.untracked_archive.as_deref() == Some(rel)
    {
        return true;
    }
    if manifest
        .source
        .nodes
        .iter()
        .any(|n| n.archive.as_deref() == Some(rel))
    {
        return true;
    }
    if manifest
        .source
        .python
        .custom_wheels
        .iter()
        .any(|w| w.archive.as_deref() == Some(rel))
    {
        return true;
    }
    false
}

fn serve_model(
    shared: &Arc<LeaderShared>,
    member: &Member,
    req: &HttpReq,
    stream: &mut TcpStream,
) -> Result<(), AppError> {
    let rest = req.path.trim_start_matches("/api/v1/model/");
    let Some((generation, file_id)) = rest.split_once('/') else {
        error_response(stream, 404, "SYNC_MODEL", "无效的模型路径");
        return Ok(());
    };
    let generation = generation.to_string();
    let file_id = file_id.to_string();
    if !sync::safe_generation_id(&generation) {
        error_response(stream, 404, "SYNC_MODEL", "无效的产物路径");
        return Ok(());
    }
    if !member_owns_generation(shared, &member.id, &generation) {
        error_response(stream, 404, "SYNC_MODEL", "产物不存在");
        return Ok(());
    }
    let manifest =
        sync::load_manifest(&generation).map_err(|e| AppError::new("SYNC_MODEL", e.message))?;
    let Some(file) = manifest.source.models.iter().find(|m| m.file_id == file_id) else {
        error_response(stream, 404, "SYNC_MODEL", "模型不在清单里");
        return Ok(());
    };
    let Some(path) = file.source_path.as_deref() else {
        error_response(stream, 404, "SYNC_MODEL", "模型源路径为空");
        return Ok(());
    };
    serve_file(stream, Path::new(path), req, "application/octet-stream")?;
    Ok(())
}

fn serve_file(
    stream: &mut TcpStream,
    path: &Path,
    req: &HttpReq,
    content_type: &str,
) -> Result<(), AppError> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            error_response(stream, 404, "SYNC_FILE", "文件不存在");
            return Ok(());
        }
    };
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    let range = header(req, "range").and_then(parse_range);
    if let Some((start, end)) = range {
        if start >= total {
            let _ = write_response(
                stream,
                416,
                "text/plain",
                &[("Content-Range", format!("bytes */{total}"))],
                b"",
            );
            return Ok(());
        }
        let end = end.min(total.saturating_sub(1));
        let len = end.saturating_sub(start) + 1;
        file.seek(SeekFrom::Start(start)).ok();
        let mut head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes())?;
        head.clear();
        copy_exact(&mut file, stream, len)?;
        stream.flush()?;
        return Ok(());
    }
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    head.clear();
    copy_exact(&mut file, stream, total)?;
    stream.flush()?;
    Ok(())
}

/// 显式指Content-Length 拷贝。不要用 `std::io::copy` + `Take`：在 Windows
/// 上某些socket 写满时它会提前结束，客户端收到"body closed before all bytes"。
fn copy_exact(
    file: &mut std::fs::File,
    stream: &mut TcpStream,
    mut left: u64,
) -> std::io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    while left > 0 {
        let want = (buf.len() as u64).min(left) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "source file ended before Content-Length",
            ));
        }
        stream.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let raw = value.strip_prefix("bytes=")?;
    let (start, end) = raw.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = if end.trim().is_empty() {
        u64::MAX
    } else {
        end.trim().parse::<u64>().ok()?
    };
    (end >= start).then_some((start, end))
}

// ---------------------------------------------------------------- Follower 客户端
fn http_agent() -> ureq::Agent {
    http_agent_with_read(Duration::from_secs(60))
}

fn http_agent_with_read(read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read)
        // 集群客户端会把 x-comfyfleet-token 放在自定义头里；ureq 的默认重定向
        // 只清理 Authorization/Cookie，跨主机时会把这个头一起带走。Leader 的
        // 地址来自配置，不能让一次 3xx 把成员令牌送到任意主机。
        .redirects(0)
        .build()
}

fn http_error(path: &str, e: ureq::Error) -> AppError {
    match e {
        ureq::Error::Status(code, resp) => {
            // 对端可能是任意地址：只读前 4KB，错误里也不回显整段 HTML/响应体。
            let mut body = String::new();
            let _ = resp.into_reader().take(4096).read_to_string(&mut body);
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "远端返回错误".into());
            let msg = msg.trim();
            let msg = if msg.chars().count() > 512 {
                format!("{}…", msg.chars().take(512).collect::<String>())
            } else {
                msg.to_string()
            };
            AppError::new("CLUSTER_HTTP", format!("{path} 返回 {code}：{msg}"))
        }
        other => AppError::new("CLUSTER_HTTP", format!("{path} 请求失败，{other}")),
    }
}

fn post_json<T: Serialize, R: DeserializeOwned>(
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &T,
) -> Result<R, AppError> {
    let mut req = http_agent().post(&format!("{base}{path}"));
    if let Some(token) = token {
        req = req.set(TOKEN_HEADER, token);
    }
    let resp = req.send_json(body).map_err(|e| http_error(path, e))?;
    resp.into_json()
        .map_err(|e| AppError::new("CLUSTER_HTTP", format!("{path} 返回无效 JSON：{e}")))
}

fn get_json<R: DeserializeOwned>(base: &str, path: &str, token: &str) -> Result<R, AppError> {
    get_json_timeout(base, path, token, Duration::from_secs(60))
}

fn get_json_timeout<R: DeserializeOwned>(
    base: &str,
    path: &str,
    token: &str,
    timeout: Duration,
) -> Result<R, AppError> {
    let resp = http_agent_with_read(timeout)
        .get(&format!("{base}{path}"))
        .set(TOKEN_HEADER, token)
        .call()
        .map_err(|e| http_error(path, e))?;
    resp.into_json()
        .map_err(|e| AppError::new("CLUSTER_HTTP", format!("{path} 返回无效 JSON：{e}")))
}

fn normalize_host(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

pub fn join_cluster(
    host: &str,
    port: u16,
    password: &str,
    member_name: &str,
) -> Result<MemberConfig, AppError> {
    if cluster_path().exists() {
        return Err(AppError::new(
            "CLUSTER_EXISTS",
            "本机是Leader，不能同时加入另一个集群",
        ));
    }
    if member_path().exists() {
        return Err(AppError::new(
            "MEMBER_EXISTS",
            "本机已加入集群，先退出再加入",
        ));
    }
    let host = normalize_host(host);
    if host.is_empty() {
        return Err(AppError::new("CLUSTER_HOST", "Leader 地址不能为空"));
    }
    if password.is_empty() {
        return Err(AppError::new("CLUSTER_PASSWORD", "密码不能为空"));
    }
    if port < 1024 {
        return Err(AppError::new("CLUSTER_PORT", "端口需大于等于 1024"));
    }
    let name = if member_name.trim().is_empty() {
        hostname()
    } else {
        member_name.trim().to_string()
    };
    let base = format!(
        "http://{}:{}",
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.clone()
        },
        port
    );
    let begin: JoinBeginResp = post_json(
        &base,
        "/api/v1/join/begin",
        None,
        &JoinBeginReq {
            member_name: name.clone(),
            hostname: hostname(),
            platform: platform(),
            app_version: option_env!("CARGO_PKG_VERSION")
                .unwrap_or("0.1.0")
                .to_string(),
        },
    )?;
    if begin.protocol != PROTOCOL_VERSION {
        return Err(AppError::new(
            "CLUSTER_PROTOCOL",
            format!(
                "协议版本不一致（Leader {} / 本机 {}），请把两台启动器升级到同一版本",
                begin.protocol, PROTOCOL_VERSION
            ),
        ));
    }
    let salt = hex::decode(&begin.salt)
        .map_err(|_| AppError::new("CLUSTER_JOIN", "Leader 返回了无效 salt"))?;
    let key = pbkdf2(password, &salt, begin.iterations);
    let proof = proof_for(&key, &begin.nonce, &name);
    let resp: JoinResp = post_json(
        &base,
        "/api/v1/join",
        None,
        &JoinReq {
            member_name: name.clone(),
            hostname: hostname(),
            platform: platform(),
            app_version: option_env!("CARGO_PKG_VERSION")
                .unwrap_or("0.1.0")
                .to_string(),
            nonce: begin.nonce,
            proof,
        },
    )?;
    let cfg = MemberConfig {
        version: PROTOCOL_VERSION,
        cluster_id: resp.cluster_id,
        cluster_name: resp.cluster_name,
        leader_host: host,
        leader_port: port,
        member_id: resp.member_id,
        member_name: resp.member_name,
        token: resp.token,
        joined_at: sync::now(),
    };
    save_member(&cfg)?;
    Ok(cfg)
}

// ---------------------------------------------------------------- 视图

fn instance_summary(state: &sync::InstanceState) -> MemberInstanceView {
    MemberInstanceView {
        id: state.instance_id.clone(),
        name: state.name.clone(),
        state: state.state.clone(),
        core_version: state.core.version.clone(),
        core_commit: state.core.commit.clone(),
        python_version: state.python.version.clone(),
        node_count: state.nodes.len(),
        model_count: state.models.len(),
        model_bytes: state.models.iter().map(|m| m.size).sum(),
        counts_known: true,
        disk_free: state.disk_free,
        last_applied: state.last_applied.clone(),
    }
}

fn member_is_online(m: &Member) -> bool {
    chrono::DateTime::parse_from_rfc3339(&m.last_seen)
        .map(|dt| {
            chrono::Local::now()
                .signed_duration_since(dt.with_timezone(&chrono::Local))
                .num_seconds()
                <= 45
        })
        .unwrap_or(false)
}

fn member_view(m: &Member) -> MemberView {
    MemberView {
        id: m.id.clone(),
        name: m.name.clone(),
        hostname: m.hostname.clone(),
        platform: m.platform.clone(),
        app_version: m.app_version.clone(),
        online: member_is_online(m),
        last_seen: m.last_seen.clone(),
        joined_at: m.joined_at.clone(),
        model_store: m.model_store.clone(),
        instances: m.instances.iter().map(instance_summary).collect(),
    }
}

fn leader_view(cfg: &ClusterConfig) -> ClusterView {
    // 只监听回环时不要把局域网 IP 展示成可加入地址，否则成员会白试。
    let addresses = if cfg.allow_lan {
        local_addresses()
            .into_iter()
            .map(|ip| format!("{ip}:{}", cfg.port))
            .collect()
    } else {
        vec![format!("127.0.0.1:{}", cfg.port)]
    };
    ClusterView {
        id: cfg.id.clone(),
        name: cfg.name.clone(),
        port: cfg.port,
        allow_lan: cfg.allow_lan,
        created_at: cfg.created_at.clone(),
        addresses,
        members: cfg.members.iter().map(member_view).collect(),
    }
}

/// Follower 自己的状态页不重新扫描模型库（可能几十GB），只展示实例登记信息。
/// 目标实例的完整清单由 Agent 心跳上报给 Leader；只有 Leader 侧才有。
///
/// `cached` 是 Leader 后台发布器已采集好的源实例状态：节点/模型计数从这里取，
/// 避免每次状态轮询都全量扫描模型库。缓存尚未就绪时把 `counts_known` 置 false。
fn local_instance_views(
    cached: Option<&HashMap<String, (sync::InstanceState, bool)>>,
) -> Vec<MemberInstanceView> {
    let mut out = Vec::new();
    if let Ok(list) = crate::commands::instance::instance_list() {
        for inst in list {
            if inst.is_remote() {
                continue;
            }
            let state = cached
                .and_then(|map| map.get(&inst.id))
                .map(|(state, _)| state);
            out.push(local_instance_view(&inst, state));
        }
    }
    out
}

fn local_instance_view(
    inst: &config::InstanceConfig,
    state: Option<&sync::InstanceState>,
) -> MemberInstanceView {
    MemberInstanceView {
        id: inst.id.clone(),
        name: inst.name.clone(),
        state: format!("{:?}", inst.state).to_lowercase(),
        core_version: inst.comfy.version.clone(),
        core_commit: (!inst.comfy.commit.is_empty()).then(|| inst.comfy.commit.clone()),
        python_version: inst.python_version.clone(),
        node_count: state.map_or(0, |s| s.nodes.len()),
        model_count: state.map_or(0, |s| s.models.len()),
        model_bytes: state.map_or(0, |s| s.models.iter().map(|m| m.size).sum()),
        counts_known: state.is_some(),
        disk_free: sync::free_space(std::path::Path::new(&inst.root_dir)),
        last_applied: sync::last_applied(&inst.id),
    }
}

pub fn leader_status(app: &tauri::AppHandle) -> ClusterStatus {
    let state = app.state::<AppState>();
    let rt = state.cluster.lock();
    let Some(shared) = &rt.leader else {
        if let Some(cfg) = load_cluster() {
            let view = leader_view(&cfg);
            return ClusterStatus {
                role: "leader".to_string(),
                addresses: view.addresses.clone(),
                cluster: Some(view),
                sources: local_instance_views(None),
                jobs: cfg.jobs.clone(),
                server_running: false,
                error: rt.error.clone(),
                ..Default::default()
            };
        }
        return ClusterStatus {
            role: "none".to_string(),
            error: rt.error.clone(),
            ..Default::default()
        };
    };
    let sources = {
        let states = shared.source_states.lock();
        local_instance_views(Some(&states))
    };
    let cfg = shared.config.lock();
    let mut jobs = cfg.jobs.clone();
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    jobs.truncate(40);
    let view = leader_view(&cfg);
    ClusterStatus {
        role: "leader".to_string(),
        addresses: view.addresses.clone(),
        cluster: Some(view),
        member: None,
        sources,
        jobs,
        server_running: true,
        agent_running: false,
        error: rt.error.clone(),
    }
}

pub fn member_status(app: &tauri::AppHandle) -> ClusterStatus {
    let state = app.state::<AppState>();
    // 只在锁内拷缓存，绝不能在这里发网络请求：这是同步命令，请求会卡住界面。
    let (cache, agent_running, rt_error) = {
        let rt = state.cluster.lock();
        (rt.follower.clone(), rt.agent.is_some(), rt.error.clone())
    };
    let Some(member) = load_member() else {
        return ClusterStatus {
            role: "none".to_string(),
            error: rt_error,
            ..Default::default()
        };
    };
    let mut jobs = cache.jobs;
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    jobs.truncate(40);
    let error = cache.error.or(rt_error);
    ClusterStatus {
        role: "member".to_string(),
        cluster: None,
        member: Some(MemberSelfView {
            cluster_id: member.cluster_id.clone(),
            cluster_name: member.cluster_name.clone(),
            leader_host: member.leader_host.clone(),
            leader_port: member.leader_port,
            member_id: member.member_id.clone(),
            member_name: member.member_name.clone(),
            online: cache.online,
            last_seen: cache.checked_at.unwrap_or_else(sync::now),
            instances: local_instance_views(None),
        }),
        sources: cache.sources,
        jobs,
        addresses: Vec::new(),
        server_running: false,
        agent_running,
        error,
    }
}

pub fn status(app: &tauri::AppHandle) -> ClusterStatus {
    if cluster_path().exists() {
        leader_status(app)
    } else if member_path().exists() {
        member_status(app)
    } else {
        let state = app.state::<AppState>();
        let rt = state.cluster.lock();
        ClusterStatus {
            role: "none".to_string(),
            error: rt.error.clone(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------- Agent

pub fn start_agent(app: tauri::AppHandle) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if rt.agent.is_some() {
        return Ok(());
    }
    let stop = Arc::new(AtomicBool::new(false));
    rt.agent = Some(AgentHandle {
        stop: Arc::clone(&stop),
    });
    rt.error = None;
    drop(rt);
    std::thread::spawn(move || agent_loop(app, stop));
    Ok(())
}

fn collect_local_states(app: &tauri::AppHandle) -> Vec<sync::InstanceState> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().clone();
    let mut out = Vec::new();
    if let Ok(list) = crate::commands::instance::instance_list() {
        for inst in list {
            if inst.is_remote() {
                continue;
            }
            out.push(sync::capture_target_state(&inst, &settings));
        }
    }
    out
}

/// Agent 线程更新 Follower 缓存；状态命令只读，不再同步打网络。
/// 目标端执行任务时同步刷新本机缓存，目标UI 才能像源端一样实时看到控制台日志。
fn remember_progress(
    app: &tauri::AppHandle,
    id: &str,
    status: &str,
    progress: u8,
    message: &str,
    error: Option<String>,
    report: Option<SyncReport>,
) {
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    let Some(job) = rt.follower.jobs.iter_mut().find(|j| j.id == id) else {
        return;
    };
    let terminal = status == sync::JOB_SUCCESS
        || status == sync::JOB_PARTIAL
        || status == sync::JOB_FAILED
        || status == sync::JOB_CANCELLED;
    let now = sync::now();
    if job.started_at.is_none() && (status == sync::JOB_RUNNING || terminal) {
        job.started_at = Some(now.clone());
    }
    if terminal && job.finished_at.is_none() {
        job.finished_at = Some(now.clone());
    }
    job.status = status.to_string();
    job.progress = progress.min(100);
    job.message = message.to_string();
    job.error = error;
    if report.is_some() {
        job.report = report;
    }
    job.updated_at = now;
    sync::push_job_log(job, message);
}

fn remember_follower(
    app: &tauri::AppHandle,
    online: bool,
    error: Option<String>,
    sources: Option<Vec<MemberInstanceView>>,
    jobs: Option<Vec<SyncJob>>,
) {
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    rt.follower.online = online;
    rt.follower.checked_at = Some(sync::now());
    if let Some(message) = error {
        rt.follower.error = Some(message);
    } else {
        rt.follower.error = None;
    }
    if let Some(sources) = sources {
        rt.follower.sources = sources;
    }
    if let Some(jobs) = jobs {
        rt.follower.jobs = jobs;
    }
    if online {
        rt.error = None;
    }
}

fn agent_loop(app: tauri::AppHandle, stop: Arc<AtomicBool>) {
    log::info!("集群 Agent 已启动");
    let mut states = Vec::new();
    let mut last_refresh: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(member) = load_member() else {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };
        let base = member.base_url();
        let refresh = last_refresh
            .map(|t| t.elapsed() > Duration::from_secs(15))
            .unwrap_or(true);
        if refresh {
            states = collect_local_states(&app);
            last_refresh = Some(Instant::now());
        }
        let store = states
            .first()
            .map(|s| s.model_store_path.clone())
            .unwrap_or_default();
        let heartbeat = post_json::<_, serde_json::Value>(
            &base,
            "/api/v1/member/heartbeat",
            Some(&member.token),
            &HeartbeatReq {
                instances: states.clone(),
                model_store: store,
            },
        );
        if let Err(e) = heartbeat {
            log::warn!("集群心跳失败，{}", e.message);
            let state = app.state::<AppState>();
            state.cluster.lock().error = Some(e.message.clone());
            remember_follower(&app, false, Some(e.message), None, None);
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        remember_follower(&app, true, None, None, None);
        // 顺带刷新一次任务列表；失败只记缓存错误，不影响心跳在线状态。
        match get_json_timeout::<MemberStatusResp>(
            &base,
            "/api/v1/member/status",
            &member.token,
            Duration::from_secs(3),
        ) {
            Ok(resp) => remember_follower(&app, true, None, Some(resp.sources), Some(resp.jobs)),
            Err(e) => remember_follower(&app, true, Some(e.message), None, None),
        }
        let next: NextJobResp = match get_json(&base, "/api/v1/member/next", &member.token) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("拉取同步任务失败，{}", e.message);
                remember_follower(&app, false, Some(e.message), None, None);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        if let Some(job) = next.job {
            log::info!(
                "领取同步任务 {}：{} → {}",
                job.id,
                job.source_instance_name,
                job.target_instance_name
            );
            let manifest: Result<SyncManifest, AppError> = get_json(
                &base,
                &format!("/api/v1/artifact/{}/manifest.json", job.generation_id),
                &member.token,
            );
            match manifest {
                Ok(manifest) => {
                    let job_for_progress = job.clone();
                    let base_for_progress = base.clone();
                    let token_for_progress = member.token.clone();
                    let app_for_progress = app.clone();
                    let mut progress = move |pct: u8, message: &str| {
                        let status = if pct >= 100 {
                            sync::JOB_SUCCESS
                        } else {
                            sync::JOB_RUNNING
                        };
                        remember_progress(
                            &app_for_progress,
                            &job_for_progress.id,
                            status,
                            pct,
                            message,
                            None,
                            None,
                        );
                        let body = ProgressReq {
                            status: status.to_string(),
                            progress: pct,
                            message: message.to_string(),
                            error: None,
                            report: None,
                        };
                        let path = format!("/api/v1/member/job/{}/progress", job_for_progress.id);
                        if let Err(e) = post_json::<_, serde_json::Value>(
                            &base_for_progress,
                            &path,
                            Some(&token_for_progress),
                            &body,
                        ) {
                            log::warn!("上报同步进度失败，{}", e.message);
                        }
                    };
                    // 轮询 Leader 上这个任务的状态：源端取消是终态；目标端取消只是
                    // 暂停，Agent 停下后回到等待接受，源端仍可再取消。
                    let target_cancel = Arc::new(AtomicBool::new(false));
                    let source_cancel = Arc::new(AtomicBool::new(false));
                    let pre_write_cancel = Arc::new(AtomicBool::new(false));
                    let target_cancel_seen = Arc::new(AtomicBool::new(false));
                    let monitor_done = Arc::new(AtomicBool::new(false));
                    {
                        let target_cancel = target_cancel.clone();
                        let source_cancel = source_cancel.clone();
                        let pre_write_cancel = pre_write_cancel.clone();
                        let target_cancel_seen = target_cancel_seen.clone();
                        let done = monitor_done.clone();
                        let base = base.clone();
                        let token = member.token.clone();
                        let job_id = job.id.clone();
                        std::thread::spawn(move || {
                            while !done.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_secs(2));
                                if done.load(Ordering::Relaxed) {
                                    break;
                                }
                                let Ok(status) = get_json_timeout::<MemberStatusResp>(
                                    &base,
                                    "/api/v1/member/status",
                                    &token,
                                    Duration::from_secs(3),
                                ) else {
                                    continue;
                                };
                                let Some(remote) = status.jobs.iter().find(|j| j.id == job_id)
                                else {
                                    continue;
                                };
                                if remote.status == sync::JOB_CANCELLED || remote.cancel_requested {
                                    source_cancel.store(true, Ordering::Relaxed);
                                    pre_write_cancel.store(true, Ordering::Relaxed);
                                }
                                if remote.target_cancel_requested {
                                    target_cancel.store(true, Ordering::Relaxed);
                                    target_cancel_seen.store(true, Ordering::Relaxed);
                                    pre_write_cancel.store(true, Ordering::Relaxed);
                                }
                                if source_cancel.load(Ordering::Relaxed)
                                    && target_cancel.load(Ordering::Relaxed)
                                {
                                    break;
                                }
                            }
                        });
                    }
                    let result = {
                        let state = app.state::<AppState>();
                        sync::apply_job(
                            &app,
                            state.inner(),
                            &job,
                            &manifest,
                            &base,
                            &member.token,
                            &target_cancel,
                            &source_cancel,
                            &pre_write_cancel,
                            &mut progress,
                        )
                    };
                    monitor_done.store(true, Ordering::Relaxed);
                    let stopped_by_source = source_cancel.load(Ordering::Relaxed);
                    let stopped_by_target = target_cancel_seen.load(Ordering::Relaxed);
                    let (status, error, report) = match result {
                        Ok(report) => (
                            if report.partial {
                                sync::JOB_PARTIAL.to_string()
                            } else {
                                sync::JOB_SUCCESS.to_string()
                            },
                            None,
                            Some(report),
                        ),
                        Err(e) => {
                            if e.code == "SYNC_CANCELLED" {
                                log::info!("同步任务 {} 已取消：{}", job.id, e.message);
                                if stopped_by_source {
                                    (sync::JOB_CANCELLED.to_string(), None, None)
                                } else if stopped_by_target {
                                    (sync::JOB_AWAITING_CONFIRMATION.to_string(), None, None)
                                } else {
                                    (sync::JOB_CANCELLED.to_string(), None, None)
                                }
                            } else {
                                log::warn!("同步任务 {} 失败，{}", job.id, e.message);
                                (sync::JOB_FAILED.to_string(), Some(e.message), None)
                            }
                        }
                    };
                    let message = match &report {
                        Some(r) => format!(
                            "{}：模型{} 个，节点 {} 个，警告 {} 条{}",
                            if r.partial {
                                "同步部分完成"
                            } else {
                                "同步完成"
                            },
                            r.models,
                            r.nodes,
                            r.warnings.len(),
                            if r.partial {
                                format!("；目标环境有 {} 项未完成", r.issues.len())
                            } else {
                                String::new()
                            }
                        ),
                        None if status == sync::JOB_AWAITING_CONFIRMATION => {
                            "目标端已取消，停止完成；可再次接受".to_string()
                        }
                        None if status == sync::JOB_CANCELLED => "同步已取消".to_string(),
                        None => "同步失败".to_string(),
                    };
                    remember_progress(
                        &app,
                        &job.id,
                        &status,
                        if report.is_some() { 100 } else { 0 },
                        &message,
                        error.clone(),
                        report.clone(),
                    );
                    let body = ProgressReq {
                        status,
                        progress: if report.is_some() { 100 } else { 0 },
                        message,
                        error,
                        report,
                    };
                    let path = format!("/api/v1/member/job/{}/progress", job.id);
                    let _ =
                        post_json::<_, serde_json::Value>(&base, &path, Some(&member.token), &body);
                    // 目标端也会留下 inbox/backups；任务结束后按同一保留次数回收。
                    let cache_keep = app.state::<AppState>().settings.lock().sync.sync_cache_keep;
                    std::thread::spawn(move || {
                        let pruned = sync::prune_sync_cache(cache_keep);
                        if pruned.removed > 0 {
                            log::info!(
                                "目标端清理同步缓存 {} 组，释放 {}",
                                pruned.removed,
                                sync::human_bytes(pruned.freed_bytes)
                            );
                        }
                    });
                    states = collect_local_states(&app);
                    last_refresh = Some(Instant::now());
                }
                Err(e) => {
                    let body = ProgressReq {
                        status: sync::JOB_FAILED.to_string(),
                        progress: 0,
                        message: "读取同步清单失败".to_string(),
                        error: Some(e.message),
                        report: None,
                    };
                    let path = format!("/api/v1/member/job/{}/progress", job.id);
                    let _ =
                        post_json::<_, serde_json::Value>(&base, &path, Some(&member.token), &body);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    log::info!("集群 Agent 已停止");
}

/// 启动时恢复：Leader 服务 / Follower Agent。
pub fn autostart(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if cluster_path().exists() {
        match load_cluster() {
            Some(cfg) => match start_server(cfg) {
                Ok(shared) => {
                    start_source_publisher(&shared, app.clone());
                    rt.leader = Some(shared);
                    rt.error = None;
                }
                Err(e) => rt.error = Some(e.message),
            },
            None => rt.error = Some("cluster.json 损坏，无法启动集群服务".to_string()),
        }
        return;
    }
    drop(rt);
    if member_path().exists() {
        if let Err(e) = start_agent(app.clone()) {
            app.state::<AppState>().cluster.lock().error = Some(e.message);
        }
    }
}

// ---------------------------------------------------------------- 命令层辅助
pub fn retry_server(app: &tauri::AppHandle) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    {
        let rt = state.cluster.lock();
        if rt.leader.is_some() {
            return Ok(());
        }
    }
    let cfg = load_cluster()
        .ok_or_else(|| AppError::new("CLUSTER_NONE", "本机没有集群配置，无法启动集群服务"))?;
    let shared = start_server(cfg)?;
    start_source_publisher(&shared, app.clone());
    let mut rt = state.cluster.lock();
    rt.leader = Some(shared);
    rt.error = None;
    Ok(())
}

pub fn leader_shared(app: &tauri::AppHandle) -> Option<Arc<LeaderShared>> {
    app.state::<AppState>().cluster.lock().leader.clone()
}

/// Leader 侧后台发布源实例快照，供目标端只读比较。
/// 不在核心结构里保存AppHandle，避免测试二进制被Tauri 对话框运行时拖进来。
pub fn start_source_publisher(shared: &Arc<LeaderShared>, app: tauri::AppHandle) {
    if shared.publisher_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        while !shared.stop.load(Ordering::Relaxed) {
            publish_source_states(&shared, &app);
            for _ in 0..150 {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

fn publish_source_states(shared: &Arc<LeaderShared>, app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let instances = match crate::commands::instance::instance_list() {
        Ok(v) => v,
        Err(_) => return,
    };
    let settings = state.settings.lock().clone();
    let git = crate::git::resolve(app, &settings).ok();
    let mut map = HashMap::new();
    for inst in instances.iter().filter(|i| !i.is_remote()) {
        let source = sync::capture_instance_state(
            inst,
            &settings,
            git.as_ref().map(|g| Path::new(&g.path)),
            true,
        );
        let running = matches!(
            inst.state,
            config::InstanceState::Running | config::InstanceState::Starting
        );
        map.insert(inst.id.clone(), (source, running));
    }
    *shared.source_states.lock() = map;
}

/// 源端页面：当前源实例相对「最近一次成功同步快照」的变化。
/// 这里刻意不与任何目标机器比较：目标机收到请求后自行选实例，源端只负责展示本次要带过去的内容。
pub fn source_changes(app: &tauri::AppHandle, source_id: &str) -> Result<SourceChanges, AppError> {
    if source_id.trim().is_empty() {
        return Err(AppError::new("CLUSTER_SOURCE", "先选择本机源实例"));
    }
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let (current, running) = shared
        .source_states
        .lock()
        .get(source_id)
        .cloned()
        .ok_or_else(|| {
            AppError::new(
                "SYNC_SOURCE_STATE",
                "源实例状态正在采集，请稍后点刷新；如果一直这样，确认实例目录可读",
            )
        })?;
    let cfg = shared.config.lock();
    let baseline = cfg.last_syncs.get(source_id);
    let mut diff = if let Some(last) = baseline {
        sync::diff_states(&last.source, &current)
    } else {
        sync::diff_states(&sync::InstanceState::default(), &current)
    };
    // 这是源端自身「上次 → 本次」的差异，不涉及目标漂移。
    diff.target_drift = false;
    diff.target_last_source = None;
    Ok(SourceChanges {
        source_instance_id: current.instance_id.clone(),
        source_instance_name: current.name.clone(),
        source_running: running,
        has_baseline: baseline.is_some(),
        last_sync_at: baseline.map(|b| b.at.clone()),
        last_sync_generation: baseline.map(|b| b.generation_id.clone()),
        diff,
    })
}

/// 目标端：把本机目标实例状态发给 Leader，只读比较某个源实例。
pub fn compare_with_leader(
    app: &tauri::AppHandle,
    source_instance_id: &str,
    target_instance_id: &str,
) -> Result<CompareResp, AppError> {
    if source_instance_id.trim().is_empty() {
        return Err(AppError::new("CLUSTER_SOURCE", "先选择源端实例"));
    }
    let member =
        load_member().ok_or_else(|| AppError::new("CLUSTER_MEMBER", "本机不是集群成员"))?;
    let inst = crate::commands::instance::load_instance(target_instance_id)?;
    if inst.is_remote() {
        return Err(AppError::new(
            "SYNC_TARGET_REMOTE",
            "云端实例没有本地环境，不能作为比较目标",
        ));
    }
    let settings = app.state::<AppState>().settings.lock().clone();
    let target = sync::capture_target_state(&inst, &settings);
    let req = CompareReq {
        source_instance_id: source_instance_id.to_string(),
        target_instance_id: target.instance_id.clone(),
        target,
    };
    post_json(
        &member.base_url(),
        "/api/v1/member/compare",
        Some(&member.token),
        &req,
    )
}

/// 源端发起任务时读取客户机信息（不要求目标实例已知）。
pub fn leader_member(app: &tauri::AppHandle, member_id: &str) -> Option<Member> {
    let shared = leader_shared(app)?;
    let cfg = shared.config.lock();
    let mut member = cfg.members.iter().find(|m| m.id == member_id)?.clone();
    member.online = member_is_online(&member);
    Some(member)
}

pub fn member_target(
    app: &tauri::AppHandle,
    member_id: &str,
    instance_id: &str,
) -> Option<(Member, sync::InstanceState)> {
    let shared = leader_shared(app)?;
    let cfg = shared.config.lock();
    let mut member = cfg.members.iter().find(|m| m.id == member_id)?.clone();
    member.online = member_is_online(&member);
    let instance = member
        .instances
        .iter()
        .find(|i| i.instance_id == instance_id)?
        .clone();
    Some((member, instance))
}

pub fn add_job(app: &tauri::AppHandle, job: SyncJob) -> Result<(), AppError> {
    add_jobs(app, vec![job])
}

/// 批量创建任务：先整体校验目标是否忙碌，再一次性落盘，避免只成功一半。
pub fn add_jobs(app: &tauri::AppHandle, jobs: Vec<SyncJob>) -> Result<(), AppError> {
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let mut cfg = shared.config.lock();
    merge_jobs(&mut cfg, jobs)?;
    save_cluster(&cfg)
}

/// 任务入队的纯逻辑：先整体校验，再一次性extend，避免批量只成功一半。
fn merge_jobs(cfg: &mut ClusterConfig, jobs: Vec<SyncJob>) -> Result<(), AppError> {
    if jobs.is_empty() {
        return Ok(());
    }
    let mut seen = std::collections::HashSet::new();
    for job in &jobs {
        if !seen.insert(job.target_member_id.clone()) {
            return Err(AppError::new(
                "SYNC_BUSY",
                format!("客户机。{}」在本次任务里重复", job.target_member_name),
            ));
        }
        if cfg.jobs.iter().any(|j| {
            j.target_member_id == job.target_member_id
                && (j.status == sync::JOB_AWAITING_CONFIRMATION
                    || j.status == sync::JOB_PENDING
                    || j.status == sync::JOB_RUNNING)
        }) {
            return Err(AppError::new(
                "SYNC_BUSY",
                format!(
                    "客户机。{}」已有同步任务在排队/执行，等它完成或取消后再发",
                    job.target_member_name
                ),
            ));
        }
    }
    cfg.jobs.extend(jobs);
    if cfg.jobs.len() > 80 {
        let remove = cfg.jobs.len() - 80;
        cfg.jobs.drain(0..remove);
    }
    Ok(())
}
pub fn jobs(app: &tauri::AppHandle) -> Vec<SyncJob> {
    leader_shared(app)
        .map(|s| s.config.lock().jobs.clone())
        .unwrap_or_default()
}

/// 手动删除一条已经结束的同步历史。等待接受、排队、执行中的任务不能删，
/// 避免把目标机正在使用的任务记录和载荷引用弄丢。
pub fn delete_job(app: &tauri::AppHandle, id: &str) -> Result<(), AppError> {
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let mut cfg = shared.config.lock();
    remove_terminal_job(&mut cfg, id)?;
    save_cluster(&cfg)
}

fn remove_terminal_job(cfg: &mut ClusterConfig, id: &str) -> Result<bool, AppError> {
    let Some(job) = cfg.jobs.iter().find(|job| job.id == id) else {
        return Ok(false);
    };
    if matches!(
        job.status.as_str(),
        sync::JOB_AWAITING_CONFIRMATION | sync::JOB_PENDING | sync::JOB_RUNNING
    ) {
        return Err(AppError::new(
            "SYNC_ACTIVE",
            "任务还在等待接受、排队或执行中，结束或取消后才能删除",
        ));
    }
    cfg.jobs.retain(|job| job.id != id);
    Ok(true)
}

pub fn drop_member(app: &tauri::AppHandle, member_id: &str) -> Result<(), AppError> {
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let mut cfg = shared.config.lock();
    cfg.members.retain(|m| m.id != member_id);
    save_cluster(&cfg)
}

pub fn leave_cluster() -> Result<(), AppError> {
    let Some(member) = load_member() else {
        return Ok(());
    };
    let base = member.base_url();
    let _ = post_json::<_, serde_json::Value>(
        &base,
        "/api/v1/member/leave",
        Some(&member.token),
        &serde_json::json!({}),
    );
    let _ = std::fs::remove_file(member_path());
    Ok(())
}

pub fn preview_data(
    app: &tauri::AppHandle,
    source_id: &str,
    member_id: &str,
    target_id: &str,
) -> Result<(ClusterConfig, Member, sync::InstanceState), AppError> {
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let cfg = shared.config.lock().clone();
    let mut member = cfg
        .members
        .iter()
        .find(|m| m.id == member_id)
        .cloned()
        .ok_or_else(|| AppError::new("CLUSTER_MEMBER", "cluster member not found"))?;
    member.online = member_is_online(&member);
    let target = member
        .instances
        .iter()
        .find(|i| i.instance_id == target_id)
        .cloned()
        .ok_or_else(|| {
            AppError::new(
                "SYNC_TARGET",
                "目标实例没有上报状态：等目标端在线并刷新后再试",
            )
        })?;
    let _ = source_id;
    Ok((cfg, member, target))
}

pub fn job_by_id(app: &tauri::AppHandle, id: &str) -> Option<SyncJob> {
    jobs(app).into_iter().find(|j| j.id == id)
}

/// 目标端只缓存 Leader 上发给自己的任务，需要单独查 follower 缓存。
pub fn member_job_by_id(app: &tauri::AppHandle, id: &str) -> Option<SyncJob> {
    let state = app.state::<AppState>();
    let rt = state.cluster.lock();
    rt.follower.jobs.iter().find(|job| job.id == id).cloned()
}

/// 目标端确认：把本机选中的实例和完整状态发给 Leader，由 Leader 在确认这一刻计算差异。
/// 真正执行仍由 Agent 在下一轮拉取pending 任务完成。
pub fn confirm_member_job(
    app: &tauri::AppHandle,
    id: &str,
    target_instance_id: &str,
    target: sync::InstanceState,
    options: sync::SyncOptions,
) -> Result<(), AppError> {
    let member =
        load_member().ok_or_else(|| AppError::new("CLUSTER_MEMBER", "本机不是集群成员"))?;
    let path = format!("/api/v1/member/job/{id}/confirm");
    let resp: ConfirmJobResp = post_json(
        &member.base_url(),
        &path,
        Some(&member.token),
        &ConfirmJobReq {
            target_instance_id: target_instance_id.to_string(),
            target,
            options,
        },
    )?;
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if let Some(job) = rt.follower.jobs.iter_mut().find(|job| job.id == id) {
        *job = resp.job;
    }
    Ok(())
}

/// 目标端拒绝等待中的任务；Leader 会把任务标记为cancelled，源端不会开始。
pub fn reject_member_job(app: &tauri::AppHandle, id: &str) -> Result<(), AppError> {
    let member =
        load_member().ok_or_else(|| AppError::new("CLUSTER_MEMBER", "本机不是集群成员"))?;
    let path = format!("/api/v1/member/job/{id}/reject");
    post_json::<_, serde_json::Value>(
        &member.base_url(),
        &path,
        Some(&member.token),
        &serde_json::json!({}),
    )?;
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if let Some(job) = rt.follower.jobs.iter_mut().find(|job| job.id == id) {
        job.status = sync::JOB_CANCELLED.to_string();
        job.finished_at = Some(sync::now());
        job.message = "目标端拒绝同步".to_string();
        job.error = Some("目标端拒绝同步".to_string());
        job.logs.push(sync::job_log("你拒绝了本次同步"));
        job.updated_at = sync::now();
    }
    Ok(())
}

/// 源端准备前先检查目标机是否已有任务，避免白扫描、白打包一整个代次后
/// 才在入队时失败。
pub fn member_active_job_name(app: &tauri::AppHandle, member_id: &str) -> Option<String> {
    let shared = leader_shared(app)?;
    let cfg = shared.config.lock();
    cfg.jobs
        .iter()
        .find(|job| {
            job.target_member_id == member_id
                && (job.status == sync::JOB_AWAITING_CONFIRMATION
                    || job.status == sync::JOB_PENDING
                    || job.status == sync::JOB_RUNNING)
        })
        .map(|job| job.target_member_name.clone())
}

/// 目标端取消当前同步。和 `reject_member_job` 不同：这里不是终态拒绝，
/// Leader 会把任务放回“等待接受”，目标端之后还能再次接受；只有源端取消才终结。
pub fn cancel_member_job(app: &tauri::AppHandle, id: &str) -> Result<(), AppError> {
    let member =
        load_member().ok_or_else(|| AppError::new("CLUSTER_MEMBER", "本机不是集群成员"))?;
    let path = format!("/api/v1/member/job/{id}/cancel");
    post_json::<_, serde_json::Value>(
        &member.base_url(),
        &path,
        Some(&member.token),
        &serde_json::json!({}),
    )?;
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if let Some(job) = rt.follower.jobs.iter_mut().find(|job| job.id == id) {
        if job.status == sync::JOB_PENDING {
            job.status = sync::JOB_AWAITING_CONFIRMATION.to_string();
            job.progress = 0;
            job.finished_at = None;
            job.message = "目标端取消，可再次接受".to_string();
        } else if job.status == sync::JOB_RUNNING {
            job.target_cancel_requested = true;
            job.message = "已请求取消，正在安全停止".to_string();
        }
        job.target_cancelled_at = Some(sync::now());
        job.target_cancel_count = job.target_cancel_count.saturating_add(1);
        job.logs
            .push(sync::job_log("你取消了同步；源端未停止时可以再次接受"));
        job.updated_at = sync::now();
    }
    Ok(())
}

/// 目标端请求重试失败的任务：Leader 先把任务放回等待接受，随后调用方会重新
/// 抓取目标状态并确认，确保按当前目标重新计算差异。
pub fn retry_member_job(app: &tauri::AppHandle, id: &str) -> Result<(), AppError> {
    let member =
        load_member().ok_or_else(|| AppError::new("CLUSTER_MEMBER", "本机不是集群成员"))?;
    let path = format!("/api/v1/member/job/{id}/retry");
    post_json::<_, serde_json::Value>(
        &member.base_url(),
        &path,
        Some(&member.token),
        &serde_json::json!({}),
    )?;
    let state = app.state::<AppState>();
    let mut rt = state.cluster.lock();
    if let Some(job) = rt.follower.jobs.iter_mut().find(|job| job.id == id) {
        job.status = sync::JOB_AWAITING_CONFIRMATION.to_string();
        job.accepted_at = None;
        job.started_at = None;
        job.finished_at = None;
        job.progress = 0;
        job.error = None;
        job.report = None;
        job.target_cancel_requested = false;
        job.target_cancelled_at = None;
        job.target_fingerprint.clear();
        job.message = "目标端请求重试，等待再次接受".to_string();
        job.updated_at = sync::now();
        job.logs
            .push(sync::job_log("目标端请求重试；重新确认后会重新计算差异"));
    }
    Ok(())
}

pub fn cancel_job(app: &tauri::AppHandle, id: &str) -> Result<(), AppError> {
    let shared = leader_shared(app)
        .ok_or_else(|| AppError::new("CLUSTER_OFFLINE", "本机不是集群 Leader"))?;
    let mut cfg = shared.config.lock();
    if let Some(job) = cfg.jobs.iter_mut().find(|j| j.id == id) {
        if job.status == sync::JOB_AWAITING_CONFIRMATION || job.status == sync::JOB_PENDING {
            job.status = sync::JOB_CANCELLED.to_string();
            job.finished_at = Some(sync::now());
            job.target_cancel_requested = false;
            job.updated_at = sync::now();
            job.message = "用户取消".to_string();
            job.logs
                .push(sync::job_log("源端取消了任务；目标端不能再接受"));
            save_cluster(&cfg)?;
        } else if job.status == sync::JOB_RUNNING {
            // 已经开跑：只发取消请求。目标端在安全点停止；如果已经进入应用阶段，
            job.cancel_requested = true;
            job.target_cancel_requested = false;
            job.updated_at = sync::now();
            job.message = "已请求取消：目标端正在安全停止，已完成的应用会保留".to_string();
            job.logs
                .push(sync::job_log("源端请求取消任务；这是终态取消"));
            save_cluster(&cfg)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::InstanceState;
    use std::sync::Mutex as StdMutex;

    /// join/confirm 测试都会写同一个cluster.json，串行执行避免互相覆盖。
    static CLUSTER_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn loopback_cluster_only_advertises_localhost() {
        let cfg = ClusterConfig {
            port: 47821,
            allow_lan: false,
            ..Default::default()
        };
        let view = leader_view(&cfg);
        assert_eq!(view.addresses, vec!["127.0.0.1:47821".to_string()]);
        assert!(!view.allow_lan);
    }

    #[test]
    fn join_member_name_is_bounded_and_clean() {
        assert_eq!(clean_member_name("  node-1  ").as_deref(), Some("node-1"));
        assert!(clean_member_name("").is_none());
        assert!(clean_member_name("a\nb").is_none());
        assert!(clean_member_name(&"x".repeat(MAX_MEMBER_NAME)).is_some());
        assert!(clean_member_name(&"x".repeat(MAX_MEMBER_NAME + 1)).is_none());
    }

    fn test_server(password: &str) -> (Arc<LeaderShared>, String) {
        let salt = [7u8; 16];
        let key = pbkdf2(password, &salt, 1_000);
        let cfg = ClusterConfig {
            version: PROTOCOL_VERSION,
            id: "testcluster".to_string(),
            name: "测试集群".to_string(),
            port: 0,
            allow_lan: false,
            salt: hex::encode(salt),
            join_key: hex::encode(key),
            iterations: 1_000,
            created_at: sync::now(),
            members: Vec::new(),
            jobs: Vec::new(),
            last_syncs: HashMap::new(),
        };
        let shared = start_server(cfg).expect("start test server");
        let port = shared.config.lock().port;
        (shared, format!("http://127.0.0.1:{port}"))
    }

    #[test]
    fn join_handshake_issues_token_and_auth_works() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("test-password");
        let begin: JoinBeginResp = post_json(
            &base,
            "/api/v1/join/begin",
            None,
            &JoinBeginReq {
                member_name: "目标机".to_string(),
                hostname: "TARGET".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
            },
        )
        .expect("join begin");
        assert_eq!(begin.protocol, PROTOCOL_VERSION);
        let key = pbkdf2(
            "test-password",
            &hex::decode(&begin.salt).unwrap(),
            begin.iterations,
        );
        let proof = proof_for(&key, &begin.nonce, "目标机");
        let joined: JoinResp = post_json(
            &base,
            "/api/v1/join",
            None,
            &JoinReq {
                member_name: "目标机".to_string(),
                hostname: "TARGET".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
                nonce: begin.nonce,
                proof,
            },
        )
        .expect("join complete");
        assert_eq!(joined.member_name, "目标机");
        assert!(!joined.token.is_empty());

        let heartbeat: serde_json::Value = post_json(
            &base,
            "/api/v1/member/heartbeat",
            Some(&joined.token),
            &HeartbeatReq {
                instances: vec![InstanceState {
                    instance_id: "inst-a".to_string(),
                    name: "A".to_string(),
                    ..Default::default()
                }],
                model_store: "D:\\models".to_string(),
            },
        )
        .expect("heartbeat");
        assert_eq!(heartbeat["ok"], true);

        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &joined.token).expect("next job");
        assert!(next.job.is_none());
        {
            let cfg = shared.config.lock();
            let member = cfg
                .members
                .iter()
                .find(|m| m.id == joined.member_id)
                .unwrap();
            assert!(member.online);
            assert_eq!(member.instances.len(), 1);
        }
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn compare_endpoint_uses_published_source_state() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("test-password");
        let joined = join_test_member(&base, "比较目标机");
        let source = InstanceState {
            instance_id: "source-a".to_string(),
            name: "源A".to_string(),
            ..Default::default()
        };
        shared
            .source_states
            .lock()
            .insert("source-a".to_string(), (source, false));
        let target = InstanceState {
            instance_id: "target-a".to_string(),
            name: "目标 A".to_string(),
            ..Default::default()
        };
        let resp: CompareResp = post_json(
            &base,
            "/api/v1/member/compare",
            Some(&joined.token),
            &CompareReq {
                source_instance_id: "source-a".to_string(),
                target_instance_id: "target-a".to_string(),
                target,
            },
        )
        .expect("compare");
        assert_eq!(resp.source_instance_name, "源A");
        assert_eq!(resp.target_instance_name, "目标 A");
        assert!(!resp.source_running);
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn batch_job_merge_is_all_or_nothing() {
        let mut cfg = ClusterConfig::default();
        cfg.jobs.push(SyncJob {
            target_member_id: "busy".to_string(),
            target_member_name: "忙碌客户机".to_string(),
            status: sync::JOB_PENDING.to_string(),
            ..Default::default()
        });
        let before = cfg.jobs.len();
        let jobs = vec![
            SyncJob {
                target_member_id: "free".to_string(),
                target_member_name: "空闲客户机".to_string(),
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                ..Default::default()
            },
            SyncJob {
                target_member_id: "busy".to_string(),
                target_member_name: "忙碌客户机".to_string(),
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                ..Default::default()
            },
        ];
        assert!(merge_jobs(&mut cfg, jobs).is_err());
        assert_eq!(cfg.jobs.len(), before, "失败时不能只入队一部分");
    }

    #[test]
    fn only_finished_jobs_can_be_removed_from_history() {
        let mut cfg = ClusterConfig::default();
        cfg.jobs.push(SyncJob {
            id: "done".to_string(),
            status: sync::JOB_SUCCESS.to_string(),
            ..Default::default()
        });
        cfg.jobs.push(SyncJob {
            id: "running".to_string(),
            status: sync::JOB_RUNNING.to_string(),
            ..Default::default()
        });
        assert!(remove_terminal_job(&mut cfg, "done").unwrap());
        assert!(cfg.jobs.iter().all(|job| job.id != "done"));
        let err = remove_terminal_job(&mut cfg, "running").unwrap_err();
        assert_eq!(err.code, "SYNC_ACTIVE");
        assert!(cfg.jobs.iter().any(|job| job.id == "running"));
        assert!(!remove_terminal_job(&mut cfg, "missing").unwrap());
    }

    #[test]
    fn stale_heartbeat_is_offline() {
        let mut member = Member::default();
        member.last_seen = sync::now();
        assert!(member_is_online(&member));
        member.last_seen = "2020-01-01T00:00:00+08:00".to_string();
        assert!(!member_is_online(&member));
        member.last_seen = "not-a-date".to_string();
        assert!(!member_is_online(&member));
    }

    #[test]
    fn instance_summary_marks_counts_as_known() {
        let state = InstanceState {
            instance_id: "i1".to_string(),
            name: "实例".to_string(),
            nodes: vec![Default::default(), Default::default()],
            models: vec![
                sync::SyncModelFile {
                    size: 10,
                    ..Default::default()
                },
                sync::SyncModelFile {
                    size: 32,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let view = instance_summary(&state);
        assert_eq!(view.node_count, 2);
        assert_eq!(view.model_count, 2);
        assert_eq!(view.model_bytes, 42);
        assert!(view.counts_known);

        let json = serde_json::to_value(&view).expect("serialize view");
        assert_eq!(json["countsKnown"], true);
        // 旧版 Leader 没有这个字段，默认按“未采集”处理。
        let legacy: MemberInstanceView =
            serde_json::from_value(serde_json::json!({"id": "x"})).expect("legacy view");
        assert!(!legacy.counts_known);
    }

    fn test_state(id: &str, name: &str) -> sync::InstanceState {
        sync::InstanceState {
            instance_id: id.to_string(),
            name: name.to_string(),
            root_dir: format!(r"D:\test\{id}"),
            app_version: "0.1.0".to_string(),
            state: "stopped".to_string(),
            disk_free: 10 * 1024 * 1024 * 1024,
            ..Default::default()
        }
    }

    fn write_confirm_manifest(generation: &str, source: sync::InstanceState) {
        std::fs::create_dir_all(sync::generation_dir(generation)).unwrap();
        sync::save_manifest(&SyncManifest {
            format: 2,
            generation_id: generation.to_string(),
            created_at: sync::now(),
            source_instance_id: source.instance_id.clone(),
            source_instance_name: source.name.clone(),
            source,
            ..Default::default()
        })
        .unwrap();
    }

    fn join_test_member(base: &str, name: &str) -> JoinResp {
        let begin: JoinBeginResp = post_json(
            base,
            "/api/v1/join/begin",
            None,
            &JoinBeginReq {
                member_name: name.to_string(),
                hostname: "TARGET".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
            },
        )
        .expect("join begin");
        let key = pbkdf2(
            "test-password",
            &hex::decode(&begin.salt).unwrap(),
            begin.iterations,
        );
        let proof = proof_for(&key, &begin.nonce, name);
        post_json(
            base,
            "/api/v1/join",
            None,
            &JoinReq {
                member_name: name.to_string(),
                hostname: "TARGET".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
                nonce: begin.nonce,
                proof,
            },
        )
        .expect("join complete")
    }

    #[test]
    fn member_must_confirm_before_job_is_claimed() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("test-password");
        let member = join_test_member(&base, "目标机");
        let target = test_state("inst-a", "目标 A");
        write_confirm_manifest("gen-confirm", test_state("source-a", "源实例"));
        {
            let mut cfg = shared.config.lock();
            if let Some(m) = cfg.members.iter_mut().find(|m| m.id == member.member_id) {
                m.instances = vec![target.clone()];
            }
            cfg.jobs.push(SyncJob {
                id: "job-confirm".to_string(),
                generation_id: "gen-confirm".to_string(),
                target_member_id: member.member_id.clone(),
                target_member_name: "目标机".to_string(),
                target_instance_id: "inst-a".to_string(),
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                created_at: sync::now(),
                updated_at: sync::now(),
                message: "等待目标端确认".to_string(),
                ..Default::default()
            });
            save_cluster(&cfg).unwrap();
        }
        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &member.token).expect("next before confirm");
        assert!(next.job.is_none(), "未确认任务不能被目标端领取");

        let _: ConfirmJobResp = post_json(
            &base,
            "/api/v1/member/job/job-confirm/confirm",
            Some(&member.token),
            &ConfirmJobReq {
                target_instance_id: "inst-a".to_string(),
                target,
                options: sync::SyncOptions {
                    remove_extra_nodes: true,
                    remove_extra_models: false,
                    allow_dirty_target: false,
                },
            },
        )
        .expect("confirm");
        {
            let cfg = shared.config.lock();
            let confirmed = cfg.jobs.iter().find(|j| j.id == "job-confirm").unwrap();
            assert_eq!(confirmed.status, sync::JOB_PENDING);
            assert_eq!(confirmed.target_instance_id, "inst-a");
            assert_eq!(confirmed.target_instance_name, "目标 A");
            assert!(
                confirmed.options.allow_dirty_target,
                "目标端确认后必须允许覆盖本地漂移"
            );
            assert!(
                confirmed.options.remove_extra_nodes,
                "清理节点的开关来自目标机确认"
            );
            assert!(!confirmed.options.remove_extra_models);
            assert!(confirmed.diff.target_drift == false);
        }
        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &member.token).expect("next after confirm");
        let job = next.job.expect("confirmed job should be claimed");
        assert_eq!(job.id, "job-confirm");
        assert_eq!(job.status, sync::JOB_RUNNING);

        {
            let mut cfg = shared.config.lock();
            cfg.jobs.push(SyncJob {
                id: "job-reject".to_string(),
                target_member_id: member.member_id.clone(),
                target_instance_id: "inst-a".to_string(),
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                created_at: sync::now(),
                updated_at: sync::now(),
                ..Default::default()
            });
            save_cluster(&cfg).unwrap();
        }
        let _: serde_json::Value = post_json(
            &base,
            "/api/v1/member/job/job-reject/reject",
            Some(&member.token),
            &serde_json::json!({}),
        )
        .expect("reject");
        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &member.token).expect("next after reject");
        assert!(next.job.is_none(), "拒绝任务不能被领取");
        let cfg = shared.config.lock();
        let rejected = cfg.jobs.iter().find(|j| j.id == "job-reject").unwrap();
        assert_eq!(rejected.status, sync::JOB_CANCELLED);
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn target_cancel_returns_to_awaiting_and_can_accept_again() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("test-password");
        let member = join_test_member(&base, "可重试目标机");
        let target = test_state("inst-retry", "重试目标");
        write_confirm_manifest("gen-retry", test_state("source-retry", "源实例"));
        {
            let mut cfg = shared.config.lock();
            if let Some(m) = cfg.members.iter_mut().find(|m| m.id == member.member_id) {
                m.instances = vec![target.clone()];
            }
            cfg.jobs.push(SyncJob {
                id: "job-retry".to_string(),
                generation_id: "gen-retry".to_string(),
                target_member_id: member.member_id.clone(),
                target_member_name: "可重试目标机".to_string(),
                target_instance_id: "inst-retry".to_string(),
                target_instance_name: "重试目标".to_string(),
                status: sync::JOB_PENDING.to_string(),
                created_at: sync::now(),
                accepted_at: Some(sync::now()),
                updated_at: sync::now(),
                ..Default::default()
            });
            save_cluster(&cfg).unwrap();
        }

        // 排队阶段目标端取消：直接回到等待接受，任务不是终态。
        let _: serde_json::Value = post_json(
            &base,
            "/api/v1/member/job/job-retry/cancel",
            Some(&member.token),
            &serde_json::json!({}),
        )
        .expect("target cancel pending");
        {
            let cfg = shared.config.lock();
            let job = cfg.jobs.iter().find(|j| j.id == "job-retry").unwrap();
            assert_eq!(job.status, sync::JOB_AWAITING_CONFIRMATION);
            assert_eq!(job.target_cancel_count, 1);
            assert!(job.target_cancelled_at.is_some());
        }

        // 再次接受：立刻回到 pending，可以重新排队。
        let _: ConfirmJobResp = post_json(
            &base,
            "/api/v1/member/job/job-retry/confirm",
            Some(&member.token),
            &ConfirmJobReq {
                target_instance_id: "inst-retry".to_string(),
                target: target.clone(),
                options: sync::SyncOptions::default(),
            },
        )
        .expect("re-accept");
        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &member.token).expect("claim after re-accept");
        let running = next.job.expect("re-accepted job should be claimable");
        assert_eq!(running.id, "job-retry");
        assert_eq!(running.status, sync::JOB_RUNNING);

        // 执行中取消：只发停止请求；Agent 停下后上报 awaiting。
        let _: serde_json::Value = post_json(
            &base,
            "/api/v1/member/job/job-retry/cancel",
            Some(&member.token),
            &serde_json::json!({}),
        )
        .expect("target cancel running");
        {
            let cfg = shared.config.lock();
            let job = cfg.jobs.iter().find(|j| j.id == "job-retry").unwrap();
            assert_eq!(job.status, sync::JOB_RUNNING);
            assert!(job.target_cancel_requested);
            assert_eq!(job.target_cancel_count, 2);
        }
        let _: serde_json::Value = post_json(
            &base,
            "/api/v1/member/job/job-retry/progress",
            Some(&member.token),
            &ProgressReq {
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                progress: 0,
                message: "目标端已取消".to_string(),
                error: None,
                report: None,
            },
        )
        .expect("agent reports paused");
        {
            let cfg = shared.config.lock();
            let job = cfg.jobs.iter().find(|j| j.id == "job-retry").unwrap();
            assert_eq!(job.status, sync::JOB_AWAITING_CONFIRMATION);
            assert!(!job.target_cancel_requested);
            assert!(job.started_at.is_none());
            assert!(job.finished_at.is_none());
        }

        // 源端取消是终态：目标端不能再取消或接受。
        {
            let mut cfg = shared.config.lock();
            let job = cfg.jobs.iter_mut().find(|j| j.id == "job-retry").unwrap();
            job.status = sync::JOB_CANCELLED.to_string();
            save_cluster(&cfg).unwrap();
        }
        let cancelled: Result<serde_json::Value, AppError> = post_json(
            &base,
            "/api/v1/member/job/job-retry/cancel",
            Some(&member.token),
            &serde_json::json!({}),
        );
        assert!(cancelled.is_err(), "源端停止后目标端不能再取消");
        let reaccept: Result<ConfirmJobResp, AppError> = post_json(
            &base,
            "/api/v1/member/job/job-retry/confirm",
            Some(&member.token),
            &ConfirmJobReq {
                target_instance_id: "inst-retry".to_string(),
                target,
                options: sync::SyncOptions::default(),
            },
        );
        assert!(reaccept.is_err(), "源端停止后目标端不能再接受");
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn failed_job_can_be_retried_by_target() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("test-password");
        let member = join_test_member(&base, "重试目标机");
        let target = test_state("inst-retry-failed", "重试目标");
        write_confirm_manifest(
            "gen-retry-failed",
            test_state("source-retry-failed", "源实例"),
        );
        {
            let mut cfg = shared.config.lock();
            if let Some(m) = cfg.members.iter_mut().find(|m| m.id == member.member_id) {
                m.instances = vec![target.clone()];
            }
            cfg.jobs.push(SyncJob {
                id: "job-retry-failed".to_string(),
                generation_id: "gen-retry-failed".to_string(),
                target_member_id: member.member_id.clone(),
                target_member_name: "重试目标机".to_string(),
                target_instance_id: "inst-retry-failed".to_string(),
                target_instance_name: "重试目标".to_string(),
                status: sync::JOB_FAILED.to_string(),
                created_at: sync::now(),
                finished_at: Some(sync::now()),
                updated_at: sync::now(),
                progress: 55,
                message: "安装普通依赖失败".to_string(),
                error: Some("包缺失".to_string()),
                report: Some(sync::SyncReport::default()),
                ..Default::default()
            });
            save_cluster(&cfg).unwrap();
        }

        let _: serde_json::Value = post_json(
            &base,
            "/api/v1/member/job/job-retry-failed/retry",
            Some(&member.token),
            &serde_json::json!({}),
        )
        .expect("target retry failed job");
        {
            let cfg = shared.config.lock();
            let job = cfg
                .jobs
                .iter()
                .find(|j| j.id == "job-retry-failed")
                .unwrap();
            assert_eq!(job.status, sync::JOB_AWAITING_CONFIRMATION);
            assert_eq!(job.progress, 0);
            assert!(job.error.is_none());
            assert!(job.report.is_none());
            assert!(job.started_at.is_none());
            assert!(job.finished_at.is_none());
        }

        // 重试后必须重新确认，才能再次领取；旧 diff 不会直接复用。
        let _: ConfirmJobResp = post_json(
            &base,
            "/api/v1/member/job/job-retry-failed/confirm",
            Some(&member.token),
            &ConfirmJobReq {
                target_instance_id: "inst-retry-failed".to_string(),
                target,
                options: sync::SyncOptions::default(),
            },
        )
        .expect("re-confirm after retry");
        let next: NextJobResp =
            get_json(&base, "/api/v1/member/next", &member.token).expect("claim retried job");
        assert_eq!(
            next.job.expect("retried job should be claimable").status,
            sync::JOB_RUNNING
        );

        // 运行中的任务不能再走“重试”，必须先结束或失败。
        let again: Result<serde_json::Value, AppError> = post_json(
            &base,
            "/api/v1/member/job/job-retry-failed/retry",
            Some(&member.token),
            &serde_json::json!({}),
        );
        assert!(again.is_err(), "running job cannot be retried");
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn wrong_password_does_not_issue_token() {
        let _guard = CLUSTER_TEST_LOCK.lock().unwrap();
        let (shared, base) = test_server("right-password");
        let begin: JoinBeginResp = post_json(
            &base,
            "/api/v1/join/begin",
            None,
            &JoinBeginReq {
                member_name: "bad".to_string(),
                hostname: "BAD".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
            },
        )
        .expect("begin");
        let key = pbkdf2(
            "wrong-password",
            &hex::decode(&begin.salt).unwrap(),
            begin.iterations,
        );
        let result: Result<JoinResp, AppError> = post_json(
            &base,
            "/api/v1/join",
            None,
            &JoinReq {
                member_name: "bad".to_string(),
                hostname: "BAD".to_string(),
                platform: "windows-x86_64".to_string(),
                app_version: "0.1.0".to_string(),
                nonce: begin.nonce,
                proof: proof_for(&key, "x", "bad"),
            },
        );
        assert!(result.is_err());
        shared.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn range_and_percent_helpers() {
        assert_eq!(parse_range("bytes=10-19"), Some((10, 19)));
        assert_eq!(parse_range("bytes=10-"), Some((10, u64::MAX)));
        assert_eq!(parse_range("bytes=20-10"), None);
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        // 回归：`%` 后直接跟多字节 UTF-8 时，旧实现会切在字符中间 panic。
        assert_eq!(percent_decode("/%中"), "/%中");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        assert_eq!(percent_decode("%A"), "%A");
    }
}
