use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::GlobalSettings;
use crate::error::AppError;
use crate::pty::PtySession;
use crate::runtime::ProcHandle;

/// 每实例的“长操作”锁：安装、版本切换、节点变更、快照恢复、删除等。
///
/// `runs` 只能防重复启动；它挡不住“安装还没完就点启动”“两个安装同时写 venv”。
/// 锁在进程内全局存在，RAII 释放，避免异常路径忘了解锁。
static OPS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub struct OpGuard {
    id: String,
}

impl OpGuard {
    pub fn acquire(id: &str, kind: &str) -> Result<Self, AppError> {
        let mut slot = OPS.lock();
        let map = slot.get_or_insert_with(HashMap::new);
        if let Some(current) = map.get(id) {
            return Err(AppError::new(
                "INSTANCE_BUSY",
                format!("这个实例正在{current}，等它结束再操作（当前请求：{kind}）"),
            ));
        }
        map.insert(id.to_string(), kind.to_string());
        Ok(Self { id: id.to_string() })
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if let Some(map) = OPS.lock().as_mut() {
            map.remove(&self.id);
        }
    }
}

/// 应用级共享状态。
pub struct AppState {
    pub settings: Mutex<GlobalSettings>,
    pub ptys: Mutex<HashMap<String, PtySession>>,
    /// 正在运行的实例进程（instance_id → 句柄）
    pub runs: Mutex<HashMap<String, Arc<ProcHandle>>>,
    /// 用户主动停止过的实例：自动重启必须让位于人为停止，退出应用/安装/同步也走这里。
    pub stop_requests: Mutex<HashSet<String>>,
    /// 每个实例的启动代次：旧监督线程发现代次变了就自行退出，避免多个线程抢着重启。
    pub run_epochs: Mutex<HashMap<String, u64>>,
    /// 应用正在退出：监督线程不能再拉起新进程。
    pub shutting_down: AtomicBool,
    /// 本次启动器会话里真正启动过的实例：用来区分「刚停下的日志」和「上次开启动器留下的旧日志」
    pub log_sessions: Mutex<HashSet<String>>,
    /// 局域网集群：Leader 服务 / Follower Agent 的句柄与错误
    pub cluster: Mutex<crate::cluster::Runtime>,
    /// 正在安装环境的实例 → 取消标志。安装循环每 500ms 检查一次，
    /// 用户点“取消”后终止当前子进程树。
    pub install_cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 正在切换/更新版本的实例 → 取消标志（目前作用于依赖重装阶段）。
    pub version_cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 源实例 → 正在进行的同步准备任务。准备线程轮询这个结构取取消标志，
    /// 界面也通过它拿到实时阶段、当前文件和日志。
    pub sync_prepares: Mutex<HashMap<String, Arc<crate::sync::SyncPrepare>>>,
}

impl AppState {
    pub fn new(settings: GlobalSettings) -> Self {
        Self {
            settings: Mutex::new(settings),
            ptys: Mutex::new(HashMap::new()),
            runs: Mutex::new(HashMap::new()),
            stop_requests: Mutex::new(HashSet::new()),
            run_epochs: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            log_sessions: Mutex::new(HashSet::new()),
            cluster: Mutex::new(crate::cluster::Runtime::default()),
            install_cancels: Mutex::new(HashMap::new()),
            version_cancels: Mutex::new(HashMap::new()),
            sync_prepares: Mutex::new(HashMap::new()),
        }
    }

    pub fn request_stop(&self, id: &str) {
        self.stop_requests.lock().insert(id.to_string());
    }

    pub fn clear_stop(&self, id: &str) {
        self.stop_requests.lock().remove(id);
    }

    pub fn stop_requested(&self, id: &str) -> bool {
        self.stop_requests.lock().contains(id)
    }

    pub fn next_run_epoch(&self, id: &str) -> u64 {
        let mut epochs = self.run_epochs.lock();
        let epoch = epochs.entry(id.to_string()).or_insert(0);
        *epoch = epoch.saturating_add(1);
        *epoch
    }

    pub fn run_epoch(&self, id: &str) -> Option<u64> {
        self.run_epochs.lock().get(id).copied()
    }

    /// UV 缓存清理期间是否有长操作在用缓存。安装 / 版本切换 / 同步准备都会走 uv；
    /// 自动清理遇到这些操作就跳过，等下一次检查，避免删掉正在下载的 wheel。
    pub fn uv_work_active(&self) -> bool {
        !self.install_cancels.lock().is_empty()
            || !self.version_cancels.lock().is_empty()
            || !self.sync_prepares.lock().is_empty()
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}
