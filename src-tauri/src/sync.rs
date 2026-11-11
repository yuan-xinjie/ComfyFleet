//! 局域网集群同步：代次清单、四组差异、载荷打包与目标端应用。
//!
//! 这里只放"可测试的同步本体"和文件环境操作；HTTP 控制面在 `cluster.rs`：
//! Tauri 命令在`commands/cluster.rs`。两者都通过本模块的 `InstanceState` /
//! `SyncManifest` 交换数据，避免把网络协议和业务逻辑搅在一起。
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::commands::instance::{load_instance, save_instance};
use crate::commands::nodes::scan_nodes;
use crate::config::{self, GlobalSettings, InstanceConfig, InstanceState as RunState, ServerOpts};
use crate::error::AppError;
use crate::proc;
use crate::snapshot;

pub const SYNC_FORMAT: u32 = 2;
pub const JOB_AWAITING_CONFIRMATION: &str = "awaiting_confirmation";
pub const JOB_PENDING: &str = "pending";
pub const JOB_RUNNING: &str = "running";
pub const JOB_SUCCESS: &str = "success";
/// 同步完成，但有包被跳过（缺 wheel / 与源环境不兼容），目标环境不完整。
pub const JOB_PARTIAL: &str = "partial";
pub const JOB_FAILED: &str = "failed";
pub const JOB_CANCELLED: &str = "cancelled";

const STORE_MARKER: &str = ".comfyfleet-model-store.json";

// ---------------------------------------------------------------- 数据模型

/// 本次同步包含哪几组内容。源端准备、目标端差异计算和落盘都以它为准：
/// 未勾选的组不会被扫描、打包、比较或写入。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncScope {
    pub comfyui: bool,
    pub python: bool,
    pub nodes: bool,
    pub models: bool,
}

impl Default for SyncScope {
    fn default() -> Self {
        Self::all()
    }
}

impl SyncScope {
    pub fn all() -> Self {
        Self {
            comfyui: true,
            python: true,
            nodes: true,
            models: true,
        }
    }

    pub fn any(self) -> bool {
        self.comfyui || self.python || self.nodes || self.models
    }

    pub fn summary(self) -> String {
        let mut parts = Vec::new();
        if self.comfyui {
            parts.push("ComfyUI");
        }
        if self.nodes {
            parts.push("节点");
        }
        if self.python {
            parts.push("Python");
        }
        if self.models {
            parts.push("模型");
        }
        if parts.is_empty() {
            "无".to_string()
        } else {
            parts.join("、")
        }
    }
}

/// 源端准备阶段的实时快照。准备在后台线程执行，界面通过这个结构轮询细节。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncPrepareView {
    pub source_instance_id: String,
    pub phase: String,
    pub percent: u8,
    pub message: String,
    pub detail: String,
    pub logs: Vec<String>,
    pub started_at: String,
    pub updated_at: String,
    pub done: bool,
    pub cancelled: bool,
    pub cancelling: bool,
    pub error: Option<String>,
    pub generation_id: Option<String>,
    pub jobs: usize,
    pub files: usize,
    pub bytes: u64,
}

/// 准备线程与界面共享的控制块。`cancel` 是协作式取消标志，耗时循环会频繁检查。
pub struct SyncPrepare {
    cancel: AtomicBool,
    inner: Mutex<SyncPrepareView>,
}

impl SyncPrepare {
    pub fn new(source_instance_id: &str) -> Self {
        let now = now();
        Self {
            cancel: AtomicBool::new(false),
            inner: Mutex::new(SyncPrepareView {
                source_instance_id: source_instance_id.to_string(),
                phase: "提交准备".to_string(),
                percent: 0,
                message: "正在建立准备任务…".to_string(),
                detail: String::new(),
                started_at: now.clone(),
                updated_at: now,
                ..Default::default()
            }),
        }
    }

    pub fn view(&self) -> SyncPrepareView {
        self.inner.lock().clone()
    }

    pub fn is_done(&self) -> bool {
        self.inner.lock().done
    }

    pub fn check(&self) -> Result<(), AppError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(AppError::new("SYNC_CANCELLED", "准备已取消"))
        } else {
            Ok(())
        }
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let mut view = self.inner.lock();
        if !view.done {
            view.cancelling = true;
            view.message = "正在停止准备，并清理临时文件…".to_string();
            view.updated_at = now();
        }
    }

    pub fn set_generation(&self, generation_id: &str) {
        let mut view = self.inner.lock();
        view.generation_id = Some(generation_id.to_string());
        view.updated_at = now();
    }

    pub fn generation_id(&self) -> Option<String> {
        self.inner.lock().generation_id.clone()
    }

    pub fn update(
        &self,
        phase: &str,
        percent: u8,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let message = message.into();
        let detail = detail.into();
        let mut view = self.inner.lock();
        if view.done {
            return;
        }
        let changed_message = view.message != message || view.phase != phase;
        view.phase = phase.to_string();
        view.percent = view.percent.max(percent.min(100));
        view.message = message;
        view.detail = detail;
        view.updated_at = now();
        if changed_message {
            let line = format!("{} {}", view.phase, view.message);
            push_prepare_log(&mut view, &line);
        }
    }

    pub fn note(&self, detail: impl Into<String>) {
        let mut view = self.inner.lock();
        if view.done {
            return;
        }
        view.detail = detail.into();
        view.updated_at = now();
    }

    pub fn add_files(&self, files: usize, bytes: u64) {
        let mut view = self.inner.lock();
        if view.done {
            return;
        }
        view.files = view.files.saturating_add(files);
        view.bytes = view.bytes.saturating_add(bytes);
        view.updated_at = now();
    }

    pub fn finish(&self, jobs: usize, generation_id: &str) {
        let mut view = self.inner.lock();
        view.done = true;
        view.cancelling = false;
        view.phase = "准备完成".to_string();
        view.percent = 100;
        view.message = format!("准备完成，已创建 {jobs} 条同步任务");
        view.detail = "同步包已保留，目标机接下来会收到确认请求".to_string();
        view.generation_id = Some(generation_id.to_string());
        view.jobs = jobs;
        view.updated_at = now();
        push_prepare_log(&mut view, "准备完成");
    }

    pub fn fail(&self, error: impl Into<String>) {
        let error = error.into();
        let mut view = self.inner.lock();
        view.done = true;
        view.cancelling = false;
        view.phase = "准备失败".to_string();
        view.message = error.clone();
        view.error = Some(error);
        view.detail = String::new();
        view.updated_at = now();
        push_prepare_log(&mut view, "准备失败");
    }

    pub fn mark_cancelled(&self, cleanup_error: Option<String>) {
        let mut view = self.inner.lock();
        view.done = true;
        view.cancelled = true;
        view.cancelling = false;
        view.phase = "已取消".to_string();
        view.percent = view.percent.min(99);
        view.message = "准备已取消".to_string();
        view.detail = match cleanup_error {
            Some(err) => format!("临时文件清理失败：{err}"),
            None => "临时文件已清理".to_string(),
        };
        view.error = None;
        let log = match &view.detail {
            detail if detail.starts_with("临时文件清理失败") => {
                format!("准备已取消；{detail}")
            }
            _ => "准备已取消，临时文件已清理".to_string(),
        };
        view.updated_at = now();
        push_prepare_log(&mut view, &log);
    }
}

fn push_prepare_log(view: &mut SyncPrepareView, message: &str) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    // 复制/校验/下载 Python 包是高频进度，原地替换最后一行，避免把控制台刷满。
    if let Some(last) = view.logs.last_mut() {
        if is_transient_progress(log_message(last)) {
            *last = job_log(message);
            return;
        }
    }
    view.logs.push(job_log(message));
    if view.logs.len() > 300 {
        let remove = view.logs.len() - 300;
        view.logs.drain(0..remove);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncCoreState {
    pub channel: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub version: Option<String>,
    pub commit: Option<String>,
    pub origin_url: Option<String>,
    pub dirty: bool,
    pub worktree_hash: String,
    /// 源端工作区修改的二进制patch（代次目录里的相对名，
    pub patch: Option<String>,
    pub untracked_archive: Option<String>,
    /// 源端没有 git 时的完整工作区包
    pub full_archive: Option<String>,
    pub full_archive_sha256: Option<String>,
    pub patch_sha256: Option<String>,
    pub untracked_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomWheel {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub sha256: String,
    pub size: u64,
    pub tags: Vec<String>,
    /// local-wheel / local-archive / remote-url / editable / local-dir / vcs / missing
    pub source: String,
    /// 代次目录里的相对路径（env/wheels/xxx.whl 成env/projects/xxx.zip：
    pub archive: Option<String>,
    pub available: bool,
    pub missing_reason: Option<String>,
    /// 与源客户端环境（Python/ABI/平台/torch/CUDA）不兼容。
    /// 同步目标是“目标与源完全一致”，所以这里只记录警告，不再据此跳过安装。
    #[serde(default)]
    pub incompatible: bool,
    /// 不兼容的具体原因，供界面/日志提示；有制品时仍会照常同步。
    #[serde(default)]
    pub compat_warning: Option<String>,
    /// 生成代次时定位原始文件用；VCS 包是仓库 URL，目标端按固定commit 安装。
    #[serde(default)]
    pub source_ref: Option<String>,
    /// VCS 类型（目前支持 git）和固定提交；`source` 为 `vcs` 时使用。
    #[serde(default)]
    pub vcs: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

/// 目标端拿不到文件、装不上的自定义依赖（缺 wheel / 目录或仓库不可用）。
/// 同步和快照恢复前先把它列给用户：补文件后重试，或确认跳过。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingWheel {
    pub name: String,
    pub version: String,
    pub filename: String,
    /// local-wheel / local-archive / local-dir / vcs / missing
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncPythonState {
    /// 完整版本，如 3.13.6
    pub version: String,
    /// 实例登记的短版本，如 3.13
    pub short_version: String,
    pub abi: String,
    pub platform: String,
    pub packages: BTreeMap<String, String>,
    pub torch: Option<String>,
    pub torch_index_url: Option<String>,
    pub custom_wheels: Vec<CustomWheel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncNodeState {
    pub name: String,
    pub enabled: bool,
    /// git / manual
    pub kind: String,
    pub url: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
    pub has_local_commits: bool,
    pub worktree_hash: String,
    /// 代次目录里的相对路径（nodes/xxx.zip：
    pub archive: Option<String>,
    pub archive_sha256: Option<String>,
    pub archive_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncModelFile {
    /// 代次内稳定ID；目标状态为空
    pub file_id: String,
    /// checkpoints / loras / ...
    pub kind: String,
    /// 相对模型库根目录，含类型前缀，如 checkpoints/SDXL/a.safetensors
    pub relative_path: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    /// 仅源端清单里有；目标端不可用这个字段访问任意路径
    #[serde(default)]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LastApplied {
    pub generation_id: String,
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub applied_at: String,
    pub snapshot_id: Option<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceState {
    pub instance_id: String,
    pub name: String,
    pub root_dir: String,
    pub app_version: String,
    /// 启动参数页的服务选项：监听/ 端口 / CORS / 自定义参数。
    #[serde(default)]
    pub server: ServerOpts,
    /// 目标端实例登记状态（running / stopped / ...）；源端也填。
    pub state: String,
    pub model_store_id: String,
    pub core: SyncCoreState,
    pub python: SyncPythonState,
    pub nodes: Vec<SyncNodeState>,
    pub models: Vec<SyncModelFile>,
    pub model_store_path: String,
    /// 实例所在磁盘可用字节（目标端用于预览执行前空间检查）。
    pub disk_free: u64,
    pub last_applied: Option<LastApplied>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncTotals {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncManifest {
    pub format: u32,
    pub generation_id: String,
    pub created_at: String,
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub source: InstanceState,
    pub snapshot: Option<snapshot::Snapshot>,
    pub totals: SyncTotals,
    /// 本次代次包含的内容组；旧代次没有这个字段时按“全部”处理。
    #[serde(default)]
    pub scope: SyncScope,
    /// 源端准备这份代次时的内容指纹。源实例和同步范围都没变时，后续任务
    /// 直接复用同一个代次，不再重新拍快照、打包节点/本体。
    #[serde(default)]
    pub prepared_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncOptions {
    pub remove_extra_nodes: bool,
    pub remove_extra_models: bool,
    pub allow_dirty_target: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncIssue {
    /// missing / incompatible / install
    pub kind: String,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncReport {
    pub pre_snapshot: Option<String>,
    pub post_snapshot: Option<String>,
    pub core: String,
    pub python: String,
    pub nodes: usize,
    pub models: usize,
    pub warnings: Vec<String>,
    /// true 表示有 wheel 未安装或缺失，目标环境与源端并不完全一致；任务仍会完成。
    pub partial: bool,
    pub issues: Vec<SyncIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncJob {
    pub id: String,
    pub generation_id: String,
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub target_member_id: String,
    #[serde(default)]
    pub target_member_name: String,
    pub target_instance_id: String,
    pub target_instance_name: String,
    pub status: String,
    pub created_at: String,
    /// 目标端接受任务的时间；重新接受时覆盖为最近一次。
    #[serde(default)]
    pub accepted_at: Option<String>,
    /// 目标端真正领取任务的时间；等待确认排队时为空。
    #[serde(default)]
    pub started_at: Option<String>,
    /// 任务进入终态的时间；取消失败/成功都会写入。
    #[serde(default)]
    pub finished_at: Option<String>,
    pub updated_at: String,
    pub progress: u8,
    pub message: String,
    pub error: Option<String>,
    pub options: SyncOptions,
    /// 本次只同步哪些组；目标端确认和落盘都按它执行。
    #[serde(default)]
    pub scope: SyncScope,
    pub diff: SyncDiff,
    pub target_fingerprint: String,
    pub target_online: bool,
    /// Leader 已收到取消请求，但目标端可能已经把文件落盘；目标端会在安全点停止，
    /// 或者安全地完成本次应用。只有pending 任务才会立刻标记为cancelled。
    #[serde(default)]
    pub cancel_requested: bool,
    /// 目标端请求暂停/取消当前执行。和源端 `cancel_requested` 不同：目标端取消
    /// 只是回到“等待接受”，任务仍可由目标端重新接受；源端取消才是终态。
    #[serde(default)]
    pub target_cancel_requested: bool,
    /// 目标端最近一次取消的时间；用于源端展示接受/取消回执。
    #[serde(default)]
    pub target_cancelled_at: Option<String>,
    /// 目标端取消次数；重新接受不清零，方便判断是否反复中断。
    #[serde(default)]
    pub target_cancel_count: u32,
    pub report: Option<SyncReport>,
    /// 目标端源端共用的事件日志，按时间顺序追加；前端以可收起控制台展示。
    #[serde(default)]
    pub logs: Vec<String>,
}

// ---------------------------------------------------------------- 差异模型

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CoreDiff {
    pub changed: bool,
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub from_channel: String,
    pub to_channel: String,
    pub source_dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WheelDelta {
    pub name: String,
    pub from: String,
    pub to: String,
    pub sha256: Option<String>,
    pub available: bool,
    pub reason: Option<String>,
    #[serde(default)]
    pub incompatible: bool,
    #[serde(default)]
    pub compat_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PythonDiff {
    pub from_version: String,
    pub to_version: String,
    pub from_torch: Option<String>,
    pub to_torch: Option<String>,
    pub packages_added: Vec<snapshot::PkgDelta>,
    pub packages_removed: Vec<snapshot::PkgDelta>,
    pub packages_changed: Vec<snapshot::PkgDelta>,
    pub wheels_added: Vec<WheelDelta>,
    pub wheels_removed: Vec<WheelDelta>,
    pub wheels_changed: Vec<WheelDelta>,
    pub missing_wheels: Vec<WheelDelta>,
    pub incompatible_wheels: Vec<WheelDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeChange {
    pub name: String,
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub from_enabled: bool,
    pub to_enabled: bool,
    pub content_changed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeDiff {
    pub added: Vec<SyncNodeState>,
    pub removed: Vec<SyncNodeState>,
    pub changed: Vec<NodeChange>,
    pub target_extra: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelChange {
    pub kind: String,
    pub relative_path: String,
    pub size: u64,
    pub from_size: u64,
    pub from_hash: Option<String>,
    pub to_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelDiff {
    pub added: Vec<ModelChange>,
    pub removed: Vec<ModelChange>,
    pub modified: Vec<ModelChange>,
    pub transfer_bytes: u64,
    pub reusable_bytes: u64,
    pub unchanged: usize,
}

/// 启动参数页的四项：监听地址 / 端口 / CORS / 自定义参数。
/// from = 目标当前值，to = 源端值，和其他差异组保持一致。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LaunchDiff {
    pub changed: bool,
    pub from_listen: String,
    pub to_listen: String,
    pub from_port: u16,
    pub to_port: u16,
    pub from_cors_origin: Option<String>,
    pub to_cors_origin: Option<String>,
    /// 可跨机器同步的普通参数；目录类参数不在其中。
    pub from_extra_args: Vec<String>,
    pub to_extra_args: Vec<String>,
    /// 目录类参数：写的是本机路径，必须保留目标端自己的值。
    pub from_local_extra_args: Vec<String>,
    pub to_local_extra_args: Vec<String>,
}

/// 这些启动参数的值是本机路径。两台机器盘符目录布局不同，绝不能直接复制。
/// 帧`=` 的写法（`--models-directory=D:\\models`）也会按参数名识别。
const LOCAL_PATH_FLAGS: &[&str] = &[
    "--models-directory",
    "--input-directory",
    "--output-directory",
    "--base-directory",
    "--temp-directory",
    "--user-directory",
    "--front-end-root",
    "--extra-model-paths-config",
    "--model-path",
    "--model-paths",
    "--input-path",
    "--output-path",
    "--database-url",
    "--log-file",
];

fn line_has_local_path_arg(line: &str) -> bool {
    line.split_whitespace().any(|token| {
        if !token.starts_with("--") {
            return false;
        }
        let flag = token.split('=').next().unwrap_or(token);
        LOCAL_PATH_FLAGS.contains(&flag)
    })
}

/// 拆成（本机路径参数，可移植参数）。一整行里只要出现目录参数就整行保留在目标端，
/// 避免把和路径写在同一行的普通参数也带过去。
pub fn split_launch_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut local = Vec::new();
    let mut portable = Vec::new();
    for arg in args {
        if line_has_local_path_arg(arg) {
            local.push(arg.clone());
        } else {
            portable.push(arg.clone());
        }
    }
    (local, portable)
}

/// 生成目标端的新启动配置：源端负责可移植参数，目标端保留自己的目录参数。
pub fn merge_launch_server(source: &ServerOpts, target: &ServerOpts) -> ServerOpts {
    let (_, mut source_portable) = split_launch_args(&source.extra_args);
    let (target_local, _) = split_launch_args(&target.extra_args);
    source_portable.extend(target_local);
    let mut merged = source.clone();
    merged.extra_args = source_portable;
    merged
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncDiff {
    pub core: CoreDiff,
    pub launch: LaunchDiff,
    pub python: PythonDiff,
    pub nodes: NodeDiff,
    pub models: ModelDiff,
    pub target_drift: bool,
    pub target_last_source: Option<String>,
    pub warnings: Vec<String>,
    pub needs_apply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncPreview {
    pub generation_id: String,
    pub source_instance_id: String,
    pub source_instance_name: String,
    pub target_member_id: String,
    pub target_instance_id: String,
    pub target_instance_name: String,
    pub target_online: bool,
    pub target_last_applied: Option<LastApplied>,
    pub target_fingerprint: String,
    /// 目标实例根目录所在磁盘的可用字节，预览开始前用来拦截空间不足。
    pub target_disk_free: u64,
    pub diff: SyncDiff,
    pub totals: SyncTotals,
}

// ---------------------------------------------------------------- 通用工具

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

/// 同步任务日志行：前端控制台直接逐行显示。
pub fn job_log(message: &str) -> String {
    format!(
        "{}  {}",
        chrono::Local::now().format("%H:%M:%S"),
        message.trim()
    )
}

/// 这些消息是“当前正在做什么”，每秒会刷新很多次。写进日志时只保留最后
/// 一条原地更新，否则控制台会被下载/安装进度刷满。
pub(crate) fn is_transient_progress(message: &str) -> bool {
    let m = message.trim_start();
    let download = m.starts_with("下载 ") && !m.contains(" 完成（") && !m.contains(" 完成(");
    let model_download = m.starts_with("下载模型 ") && !m.contains(" 完成：");
    download
        || model_download
        || m.starts_with("创建环境：")
        || m.starts_with("安装依赖：")
        || m.starts_with("安装 Git 包 ")
        || m.starts_with("安装项目 ")
        || m.starts_with("安装本地项目 ")
        || m.starts_with("安装自定义包 ")
        || m.starts_with("安装 Git 自建包 ")
        || m.starts_with("准备 Python 包")
        || m.starts_with("复制 Python 包")
        || m.starts_with("校验 Python 包")
        || m.starts_with("下载远程 Python 包")
        || m.starts_with("打包 Python 包")
}

fn log_message(log: &str) -> &str {
    // job_log 的格式是 "HH:MM:SS  message"；兼容旧数据里的 "[time] message"。
    log.split_once("  ")
        .map(|(_, rest)| rest)
        .or_else(|| log.split_once("] ").map(|(_, rest)| rest))
        .unwrap_or(log)
}

/// 统一的日志追加入口：遇到临时进度就替换上一条临时进度，而不是追加新行。
/// 临时进度后面出现完成/失败等里程碑时，也顺手替换掉那条进度行。
pub(crate) fn push_job_log(job: &mut SyncJob, message: &str) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    if let Some(last) = job.logs.last_mut() {
        if is_transient_progress(log_message(last)) {
            *last = job_log(message);
            return;
        }
    }
    job.logs.push(job_log(message));
    if job.logs.len() > 600 {
        let remove = job.logs.len() - 600;
        job.logs.drain(0..remove);
    }
}

pub fn sync_root() -> PathBuf {
    config::app_root().join("sync")
}

pub fn generations_dir() -> PathBuf {
    sync_root().join("generations")
}

pub fn inbox_dir() -> PathBuf {
    sync_root().join("inbox")
}

pub fn backups_dir() -> PathBuf {
    sync_root().join("backups")
}

pub fn generation_dir(id: &str) -> PathBuf {
    generations_dir().join(id)
}

/// 同步缓存占用：设置页用它显示当前 `data\sync` 的大小。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCacheInfo {
    pub total_bytes: u64,
    pub generation_bytes: u64,
    pub generation_count: usize,
    pub inbox_bytes: u64,
    pub inbox_count: usize,
    pub backup_bytes: u64,
    pub backup_count: usize,
    pub node_cache_bytes: u64,
    pub node_cache_count: usize,
    pub hash_cache_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCachePrune {
    pub removed: usize,
    pub freed_bytes: u64,
}

fn tree_usage(path: &Path) -> u64 {
    if !path.is_dir() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

fn group_usage(path: &Path) -> (u64, usize) {
    let Ok(entries) = fs::read_dir(path) else {
        return (0, 0);
    };
    let mut bytes = 0u64;
    let mut count = 0usize;
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        count += 1;
        bytes = bytes.saturating_add(tree_usage(&entry.path()));
    }
    (bytes, count)
}

fn node_cache_usage(path: &Path) -> (u64, usize) {
    let Ok(entries) = fs::read_dir(path) else {
        return (0, 0);
    };
    let mut bytes = 0u64;
    let mut count = 0usize;
    for entry in entries.filter_map(Result::ok) {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        count += 1;
        bytes = bytes.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
    }
    (bytes, count)
}

fn sync_cache_info_at(root: &Path) -> SyncCacheInfo {
    if !root.is_dir() {
        return SyncCacheInfo::default();
    }
    let (generation_bytes, generation_count) = group_usage(&root.join("generations"));
    let (inbox_bytes, inbox_count) = group_usage(&root.join("inbox"));
    let (backup_bytes, backup_count) = group_usage(&root.join("backups"));
    let (node_cache_bytes, node_cache_count) = node_cache_usage(&root.join("node-cache"));
    let hash_cache_bytes = ["hash-cache.json", "hash-cache.json.bak"]
        .iter()
        .filter_map(|name| fs::metadata(root.join(name)).ok())
        .map(|meta| meta.len())
        .sum();
    SyncCacheInfo {
        total_bytes: tree_usage(&root),
        generation_bytes,
        generation_count,
        inbox_bytes,
        inbox_count,
        backup_bytes,
        backup_count,
        node_cache_bytes,
        node_cache_count,
        hash_cache_bytes,
    }
}

pub fn sync_cache_info() -> SyncCacheInfo {
    sync_cache_info_at(&sync_root())
}

fn cache_dir_time(path: &Path) -> std::time::SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

fn prune_cache_dirs(
    root: &Path,
    keep: usize,
    protected: &BTreeSet<String>,
    by_name: bool,
) -> SyncCachePrune {
    let Ok(entries) = fs::read_dir(root) else {
        return SyncCachePrune::default();
    };
    let mut dirs: Vec<(String, PathBuf, std::time::SystemTime)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            Some((name, entry.path(), cache_dir_time(&entry.path())))
        })
        .collect();
    dirs.sort_by(|a, b| {
        if by_name {
            b.0.cmp(&a.0)
        } else {
            b.2.cmp(&a.2).then_with(|| b.0.cmp(&a.0))
        }
    });
    let mut report = SyncCachePrune::default();
    let mut kept = 0usize;
    for (name, path, _) in dirs {
        if protected.contains(&name) {
            continue;
        }
        if kept < keep {
            kept += 1;
            continue;
        }
        let bytes = tree_usage(&path);
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                report.removed += 1;
                report.freed_bytes = report.freed_bytes.saturating_add(bytes);
            }
            Err(e) => log::warn!("清理同步缓存 {} 失败：{e}", path.display()),
        }
    }
    report
}

fn active_sync_ids() -> (BTreeSet<String>, BTreeSet<String>) {
    let Some(cfg) = crate::cluster::load_cluster() else {
        return (BTreeSet::new(), BTreeSet::new());
    };
    let mut generations = BTreeSet::new();
    let mut jobs = BTreeSet::new();
    for job in &cfg.jobs {
        if matches!(
            job.status.as_str(),
            JOB_AWAITING_CONFIRMATION | JOB_PENDING | JOB_RUNNING
        ) {
            generations.insert(job.generation_id.clone());
            jobs.insert(job.id.clone());
        }
    }
    (generations, jobs)
}

/// 只保留最近 `keep` 组同步缓存。
///
/// 代次按时间戳目录名排序（不依赖文件时间）；收件箱/备份按目录修改时间排序。
/// 正在等待确认、排队或运行中的任务永远不删，避免把目标机正在拉取的产物提前回收。
pub fn prune_sync_cache(keep: u32) -> SyncCachePrune {
    let keep = (keep.max(1) as usize).min(100);
    let (active_generations, active_jobs) = active_sync_ids();
    let mut total = SyncCachePrune::default();
    for report in [
        prune_cache_dirs(&generations_dir(), keep, &active_generations, true),
        prune_cache_dirs(&inbox_dir(), keep, &active_jobs, false),
        prune_cache_dirs(&backups_dir(), keep, &active_jobs, false),
    ] {
        total.removed += report.removed;
        total.freed_bytes = total.freed_bytes.saturating_add(report.freed_bytes);
    }
    total
}

pub fn target_state_path() -> PathBuf {
    sync_root().join("target-state.json")
}

/// 按字符截断，绝不能在 UTF-8 字符中间切。
pub(crate) fn short_chars(value: &str, n: usize) -> String {
    value.chars().take(n).collect()
}

fn windows_reserved_stem(stem: &str) -> bool {
    matches!(
        stem,
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}

/// Windows 文件名语义检查：保留设备名、结尾点/空格、非法字符都会让路径
/// 被系统重解释。模型/压缩包路径不能只防 `..`，否则 `NUL`、`a.`、`a:ads`
/// 在 Windows 上会指向和清单字符串不同的对象。
fn windows_component_ok(name: &str) -> bool {
    if name.is_empty() || name.ends_with('.') || name.ends_with(' ') {
        return false;
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_lowercase();
    !windows_reserved_stem(&stem)
}

pub fn safe_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 120 && !name.starts_with('.') && windows_component_ok(name)
}

pub(crate) fn safe_generation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// PEP 503 规范名允许的字符。用于把远端清单写进 requirements 前做白名单。
fn safe_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 只允许 PEP 440 里常见的版本字符；换行、空白、`@`、URL 一律拒绝。
fn safe_package_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 120
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | '!' | '*'))
}

/// 目标端安装普通依赖时使用的索引顺序：torch 专用索引 → 用户 PyPI 镜像 →
/// 官方 PyPI 兜底。镜像缺包（例如刚拆出的 ComfyUI media-assets）时仍能安装。
fn install_indexes(torch: Option<&str>, mirror: Option<&str>) -> Vec<String> {
    let mut indexes: Vec<String> = Vec::new();
    for candidate in [torch, mirror, Some("https://pypi.org/simple")]
        .into_iter()
        .flatten()
    {
        let index = candidate.trim().trim_end_matches('/').to_string();
        if !index.is_empty() && !indexes.iter().any(|item| item == &index) {
            indexes.push(index);
        }
    }
    indexes
}

/// 生成目标端 requirements。自定义 wheel / Git / 本地目录包由 custom_wheels
/// 单独安装，不能因为它们的版本号为空或来自 URL 就先被普通包校验拦下。
fn requirement_lines(
    packages: &BTreeMap<String, String>,
    custom_wheels: &[CustomWheel],
) -> Result<Vec<String>, AppError> {
    let custom_names: BTreeSet<String> = custom_wheels
        .iter()
        .map(|w| canonical_dist_name(&w.name))
        .collect();
    let mut lines = Vec::new();
    for (name, version) in packages {
        if custom_names.contains(&canonical_dist_name(name)) {
            continue;
        }
        // 从 wheel 文件名/旧清单来的版本可能仍是 `%2B`；PEP 440 的本地版本
        // 分隔符是 `+`，必须还原后再写进 requirements。
        let version = percent_decode(version);
        if version.trim().is_empty() {
            // 没有版本号无法写成 `name==version`。通常是直接 URL / 本地目录
            // 安装的包；如果它没有进入 custom_wheels，只能跳过，不能让整个
            // 同步因为一条无法表达的行失败。
            log::warn!("同步跳过没有版本号的 Python 包：{name}");
            continue;
        }
        if !safe_package_name(name) || !safe_package_version(&version) {
            return Err(AppError::new(
                "SYNC_REQUIREMENTS",
                format!("远端的 Python 包名/版本不合法，拒绝写入 requirements：{name}=={version}"),
            ));
        }
        lines.push(format!("{name}=={version}"));
    }
    lines.sort();
    Ok(lines)
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> Result<String, AppError> {
    sha256_file_with_progress(path, |_, _| Ok(()))
}

fn sha256_file_with_progress<F>(path: &Path, mut on_read: F) -> Result<String, AppError>
where
    F: FnMut(u64, u64) -> Result<(), AppError>,
{
    let mut f = File::open(path)
        .map_err(|e| AppError::new("SYNC_READ", format!("读取 {} 失败，{e}", path.display())))?;
    let total = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut read_bytes: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(|e| {
            AppError::new("SYNC_READ", format!("读取 {} 失败，{e}", path.display()))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read_bytes = read_bytes.saturating_add(n as u64);
        on_read(read_bytes, total)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 模型/节点哈希缓存：键 = 绝对路径 + 大小 + 修改时间。
///
/// 大模型全量哈希很贵；缓存让第二次同步只做 stat。缓存本身落盘，启动器重启后仍然有效。
fn hash_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let path = sync_root().join("hash-cache.json");
        let map = fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        Mutex::new(map)
    })
}

fn file_cache_key(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 统一分隔符：同一个文件可能由 WalkDir 给出 `\`，也可能由清单的相对路径
    // 拼成 `/`；不统一会导致哈希缓存明明有记录却反复重算。
    let path_key = path.to_string_lossy().replace('\\', "/").to_lowercase();
    Some(format!("{}|{}|{}", path_key, meta.len(), modified))
}

pub fn sha256_file_cached(path: &Path) -> Result<String, AppError> {
    sha256_file_cached_with_progress(path, |_, _| Ok(()))
}

/// 目录一致性哈希用的文件哈希：文本文件先去掉 CR，再算 SHA-256。
///
/// Windows 上 Git 的 autocrlf / patch 落盘可能把同一段文本写成 CRLF 或 LF；
/// 原始字节哈希会把“内容相同、换行风格不同”误判成核心有改动，导致每次同步都
/// 重复应用补丁，目标工作区还会因为再次 apply 失败。这里只对目录一致性哈希做
/// 规范化；下载载荷的校验仍使用原始字节的 `sha256_file_cached`。
fn sha256_file_consistency_cached(path: &Path) -> Result<String, AppError> {
    let Some(raw_key) = file_cache_key(path) else {
        return sha256_file_consistency(path);
    };
    let key = format!("{raw_key}|lf");
    if let Some(hit) = hash_cache().lock().get(&key) {
        return Ok(hit.clone());
    }
    let hash = sha256_file_consistency(path)?;
    hash_cache().lock().insert(key, hash.clone());
    Ok(hash)
}

fn sha256_file_consistency(path: &Path) -> Result<String, AppError> {
    let data = fs::read(path)
        .map_err(|e| AppError::new("SYNC_READ", format!("读取 {} 失败，{e}", path.display())))?;
    let mut hasher = Sha256::new();
    // 含 NUL 的文件按二进制处理，避免把 CR 字节从图片/权重里删掉。
    if data.contains(&0) || !data.contains(&b'\r') {
        hasher.update(&data);
    } else {
        for chunk in data.split(|b| *b == b'\r') {
            hasher.update(chunk);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_file_cached_with_progress<F>(path: &Path, on_read: F) -> Result<String, AppError>
where
    F: FnMut(u64, u64) -> Result<(), AppError>,
{
    let Some(key) = file_cache_key(path) else {
        return sha256_file_with_progress(path, on_read);
    };
    if let Some(hit) = hash_cache().lock().get(&key) {
        return Ok(hit.clone());
    }
    let hash = sha256_file_with_progress(path, on_read)?;
    hash_cache().lock().insert(key, hash.clone());
    Ok(hash)
}

/// 下载时已经算过哈希，直接记入缓存，避免同步后再次读取整个文件。
fn remember_file_hash(path: &Path, hash: &str) {
    if let Some(key) = file_cache_key(path) {
        hash_cache().lock().insert(key, hash.to_string());
    }
}

pub fn flush_hash_cache() {
    let map = hash_cache().lock().clone();
    if let Ok(text) = serde_json::to_string(&map) {
        let _ = fs::create_dir_all(sync_root());
        let _ = config::atomic_write(&sync_root().join("hash-cache.json"), text.as_bytes());
    }
}

fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git" | "__pycache__" | ".venv" | "venv" | "node_modules" | ".cache" | ".pytest_cache"
    )
}

fn is_skipped_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".pyc")
        || lower.ends_with(".pyo")
        || lower == ".ds_store"
        || lower.ends_with(".part")
        || lower.ends_with(".tmp")
}

/// ComfyUI 本体同步只看代码/配置，不把custom_nodes、模型、输入输出算进核心状态；
/// 否则目标端模型库不同会永远判定「核心有差异」。
const CORE_EXCLUDE: &[&str] = &[
    "custom_nodes",
    "models",
    "input",
    "output",
    "temp",
    "user",
    "logs",
    ".venv",
    // 完整核心同步的归档不含 .git；清理目标目录时必须保留它，
    // 否则目标实例会永久失去 git 历史，快照恢复也会因 NO_GIT 全部失败。
    ".git",
    "extra_model_paths.yaml",
];

/// 目录内容哈希：不依赖 mtime，按相对路径排序后逐文件哈希。用于校验同步是否一致。
pub fn hash_tree(root: &Path, include_git: bool) -> String {
    hash_tree_ex(root, include_git, &[])
}

fn hash_tree_ex(root: &Path, include_git: bool, exclude_top: &[&str]) -> String {
    let mut files: Vec<PathBuf> = Vec::new();
    if !root.exists() {
        return String::new();
    }
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if entry.depth() == 1 && exclude_top.contains(&name.as_ref()) {
                return false;
            }
            if is_skipped_dir(&name) && !(include_git && name == ".git") {
                return false;
            }
            true
        });
    for entry in walker.filter_map(Result::ok) {
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if is_skipped_file(&name) {
            continue;
        }
        files.push(entry.into_path());
    }
    files.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    let mut hasher = Sha256::new();
    for path in files {
        let rel = path.strip_prefix(root).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        // 用路径大小+修改时间缓存单文件哈希：心跳比15 秒采集状态时可stat：        // 不重复读取ComfyUI/节点的大文件。
        match sha256_file_consistency_cached(&path) {
            Ok(hash) => hasher.update(hash.as_bytes()),
            Err(_) => hasher.update(b"<unreadable>"),
        }
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn hash_core_tree(root: &Path) -> String {
    hash_tree_ex(root, false, CORE_EXCLUDE)
}

pub(crate) fn zip_dir(
    root: &Path,
    dest: &Path,
    include_git: bool,
) -> Result<(u64, String), AppError> {
    zip_dir_with(root, dest, include_git, &[], None, "")
}

fn zip_dir_ex(
    root: &Path,
    dest: &Path,
    include_git: bool,
    exclude_top: &[&str],
) -> Result<(u64, String), AppError> {
    zip_dir_with(root, dest, include_git, exclude_top, None, "")
}

fn zip_file_options(path: &Path) -> zip::write::SimpleFileOptions {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // 权重、图片、视频和压缩包本身已经压不动，再 Deflate 只会白烧 CPU。
    let already_compressed = matches!(
        ext.as_str(),
        "safetensors"
            | "ckpt"
            | "pt"
            | "pth"
            | "bin"
            | "onnx"
            | "zip"
            | "7z"
            | "gz"
            | "bz2"
            | "xz"
            | "zst"
            | "jpg"
            | "jpeg"
            | "png"
            | "webp"
            | "mp4"
            | "webm"
            | "mov"
    );
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    if already_compressed {
        options.compression_method(zip::CompressionMethod::Stored)
    } else {
        options
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(1))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NodeCacheMeta {
    name: String,
    kind: String,
    worktree_hash: String,
    include_git: bool,
    commit: String,
    url: String,
    has_local_commits: bool,
    size: u64,
    sha256: String,
}

fn node_cache_paths(source_id: &str, name: &str) -> (PathBuf, PathBuf) {
    let key = hex::encode(Sha256::digest(format!("{source_id}\0{name}").as_bytes()));
    let dir = sync_root().join("node-cache");
    (
        dir.join(format!("{key}.zip")),
        dir.join(format!("{key}.json")),
    )
}

fn reuse_node_archive(
    source_id: &str,
    node: &SyncNodeState,
    include_git: bool,
    dest: &Path,
) -> Option<(u64, String)> {
    if node.worktree_hash.is_empty() {
        return None;
    }
    let (archive, meta_path) = node_cache_paths(source_id, &node.name);
    let raw = fs::read_to_string(&meta_path).ok()?;
    let meta: NodeCacheMeta = serde_json::from_str(&raw).ok()?;
    if meta.name != node.name
        || meta.kind != node.kind
        || meta.worktree_hash != node.worktree_hash
        || meta.include_git != include_git
        || meta.commit != node.commit.clone().unwrap_or_default()
        || meta.url != node.url.clone().unwrap_or_default()
        || meta.has_local_commits != node.has_local_commits
        || fs::metadata(&archive).ok()?.len() != meta.size
    {
        return None;
    }
    let _ = fs::remove_file(dest);
    if fs::hard_link(&archive, dest).is_err() && fs::copy(&archive, dest).is_err() {
        return None;
    }
    Some((meta.size, meta.sha256))
}

fn save_node_archive(
    source_id: &str,
    node: &SyncNodeState,
    include_git: bool,
    dest: &Path,
    size: u64,
    sha256: &str,
) {
    let (archive, meta_path) = node_cache_paths(source_id, &node.name);
    let Some(parent) = archive.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = archive.with_extension("zip.tmp");
    let _ = fs::remove_file(&temp);
    let linked = fs::hard_link(dest, &temp).is_ok() || fs::copy(dest, &temp).is_ok();
    if !linked {
        let _ = fs::remove_file(&temp);
        return;
    }
    let _ = fs::remove_file(&archive);
    if fs::rename(&temp, &archive).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    let meta = NodeCacheMeta {
        name: node.name.clone(),
        kind: node.kind.clone(),
        worktree_hash: node.worktree_hash.clone(),
        include_git,
        commit: node.commit.clone().unwrap_or_default(),
        url: node.url.clone().unwrap_or_default(),
        has_local_commits: node.has_local_commits,
        size,
        sha256: sha256.to_string(),
    };
    if let Ok(text) = serde_json::to_vec(&meta) {
        let _ = config::atomic_write(&meta_path, &text);
    }
}

fn zip_dir_with(
    root: &Path,
    dest: &Path,
    include_git: bool,
    exclude_top: &[&str],
    prep: Option<&SyncPrepare>,
    label: &str,
) -> Result<(u64, String), AppError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(dest)
        .map_err(|e| AppError::new("SYNC_ZIP", format!("创建 {} 失败，{e}", dest.display())))?;
    let mut zip = zip::ZipWriter::new(file);
    let dir_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    let mut count = 0usize;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.path() == dest {
                return false;
            }
            if !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if entry.depth() == 1 && exclude_top.contains(&name.as_ref()) {
                return false;
            }
            if is_skipped_dir(&name) && !(include_git && name == ".git") {
                return false;
            }
            true
        });
    for entry in walker.filter_map(Result::ok) {
        if let Some(context) = prep {
            context.check()?;
        }
        let path = entry.path();
        if entry.depth() == 0 {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_text = rel.to_string_lossy().replace('\\', "/");
        if rel_text.is_empty() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if entry.file_type().is_dir() {
            zip.add_directory(format!("{rel_text}/"), dir_options)
                .map_err(|e| AppError::new("SYNC_ZIP", e.to_string()))?;
            continue;
        }
        if is_skipped_file(name) || name == STORE_MARKER {
            continue;
        }
        if let Some(context) = prep {
            context.note(format!("压缩 {label}：{rel_text}"));
        }
        zip.start_file(rel_text.clone(), zip_file_options(path))
            .map_err(|e| AppError::new("SYNC_ZIP", e.to_string()))?;
        let mut source = File::open(path)?;
        let total = source.metadata().map(|m| m.len()).unwrap_or(0);
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut read_bytes: u64 = 0;
        loop {
            if let Some(context) = prep {
                context.check()?;
            }
            let n = source.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
            read_bytes = read_bytes.saturating_add(n as u64);
            if let Some(context) = prep {
                context.note(format!(
                    "压缩 {label}：{}（{}/{}）",
                    rel_text,
                    human_bytes(read_bytes),
                    human_bytes(total)
                ));
            }
        }
        count += 1;
    }
    zip.finish()
        .map_err(|e| AppError::new("SYNC_ZIP", e.to_string()))?;
    let size = fs::metadata(dest)?.len();
    let hash = sha256_file_for_prepare(dest, prep, &format!("校验 {label}"))?;
    log::debug!(
        "打包 {} →{}：{count} 文件，{size} 字节，",
        root.display(),
        dest.display()
    );
    Ok((size, hash))
}

/// 传输/校验进度节流器：每 400ms 采样一次速度，避免每个 4 MiB 块都刷新 UI。
struct SpeedMeter {
    last_at: std::time::Instant,
    last_bytes: u64,
}

impl SpeedMeter {
    fn new() -> Self {
        Self {
            last_at: std::time::Instant::now(),
            last_bytes: 0,
        }
    }

    fn sample(&mut self, done: u64, total: u64, force: bool) -> Option<(Option<u8>, f64)> {
        let elapsed = self.last_at.elapsed();
        if !force && elapsed < std::time::Duration::from_millis(400) {
            return None;
        }
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = done.saturating_sub(self.last_bytes) as f64 / secs;
        self.last_at = std::time::Instant::now();
        self.last_bytes = done;
        let percent = if total > 0 {
            Some(((done.saturating_mul(100) / total).min(100)) as u8)
        } else {
            None
        };
        Some((percent, rate))
    }
}

fn transfer_progress_message(label: &str, percent: Option<u8>, rate: f64) -> String {
    match percent {
        Some(percent) => format!("{label}：{percent}% · {}", human_rate(rate)),
        None => format!("{label}：{}", human_rate(rate)),
    }
}

fn copy_file_with_progress(
    src: &Path,
    dest: &Path,
    prep: Option<&SyncPrepare>,
    label: &str,
) -> Result<(), AppError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut source = File::open(src)?;
    let total = source.metadata().map(|m| m.len()).unwrap_or(0);
    let mut target = File::create(dest)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut copied: u64 = 0;
    let mut meter = SpeedMeter::new();
    if let Some(context) = prep {
        context.check()?;
        context.update(
            "准备 Python 包",
            80,
            transfer_progress_message(label, Some(0), 0.0),
            format!("0/{}", human_bytes(total)),
        );
    }
    loop {
        if let Some(context) = prep {
            context.check()?;
        }
        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }
        target.write_all(&buf[..n])?;
        copied = copied.saturating_add(n as u64);
        if let Some(context) = prep {
            if let Some((percent, rate)) = meter.sample(copied, total, copied >= total) {
                context.update(
                    "准备 Python 包",
                    80,
                    transfer_progress_message(label, percent, rate),
                    format!("{}/{}", human_bytes(copied), human_bytes(total)),
                );
            }
        }
    }
    target.flush()?;
    Ok(())
}

fn sha256_file_for_prepare(
    path: &Path,
    prep: Option<&SyncPrepare>,
    label: &str,
) -> Result<String, AppError> {
    match prep {
        // Python 包/轮子需要一行内联进度；节点和 ComfyUI 归档仍走 detail，
        // 避免把“校验节点”误标成“准备 Python 包”并刷满控制台。
        Some(context) if label.contains("Python") => {
            let mut meter = SpeedMeter::new();
            sha256_file_with_progress(path, |read, total| {
                context.check()?;
                if let Some((percent, rate)) = meter.sample(read, total, read >= total) {
                    context.update(
                        "准备 Python 包",
                        80,
                        transfer_progress_message(label, percent, rate),
                        format!("{}/{}", human_bytes(read), human_bytes(total)),
                    );
                }
                Ok(())
            })
        }
        Some(context) => sha256_file_with_progress(path, |read, total| {
            context.check()?;
            context.note(format!(
                "{label}：{}/{}",
                human_bytes(read),
                human_bytes(total)
            ));
            Ok(())
        }),
        None => sha256_file(path),
    }
}

/// 安全解压：拒绝绝对路径、`..`、符号链接和超大解压总量。
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), AppError> {
    const MAX_ENTRIES: usize = 200_000;
    const MAX_UNZIPPED: u64 = 64 * 1024 * 1024 * 1024;
    let file = File::open(zip_path).map_err(|e| {
        AppError::new(
            "SYNC_UNZIP",
            format!("打开 {} 失败，{e}", zip_path.display()),
        )
    })?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("SYNC_UNZIP", format!("读取 zip 失败，{e}")))?;
    if archive.len() > MAX_ENTRIES {
        return Err(AppError::new("SYNC_UNZIP", "压缩包条目过多，拒绝解压"));
    }
    fs::create_dir_all(dest)?;
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::new("SYNC_UNZIP", e.to_string()))?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(AppError::new("SYNC_UNZIP", "压缩包包含越界路径，已拒绝"));
        };
        if !safe_zip_entry_path(&rel) {
            return Err(AppError::new(
                "SYNC_UNZIP",
                "压缩包包含 Windows 保留名/ADS/非法字符，已拒绝",
            ));
        }
        if entry
            .unix_mode()
            .map(|mode| mode & 0o170000 == 0o120000)
            .unwrap_or(false)
        {
            return Err(AppError::new("SYNC_UNZIP", "压缩包含符号链接，已拒绝"));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| AppError::new("SYNC_UNZIP", "解压尺寸溢出"))?;
        if total > MAX_UNZIPPED {
            return Err(AppError::new("SYNC_UNZIP", "解压后总量过大，拒绝解压"));
        }
        let out = dest.join(rel);
        if !out.starts_with(dest) {
            return Err(AppError::new("SYNC_UNZIP", "压缩包路径越界，已拒绝"));
        }
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 实例状态采集
// Windows 上查询目录所在卷的可用空间；失败返回 0（不阻止，仅提示）。
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lp_directory_name: *const u16,
        lp_free_bytes_available_to_caller: *mut u64,
        lp_total_number_of_bytes: *mut u64,
        lp_total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent.to_path_buf(),
            _ => break,
        }
    }
    let mut wide: Vec<u16> = probe.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        free
    } else {
        0
    }
}

#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> u64 {
    0
}

fn app_version() -> String {
    option_env!("CARGO_PKG_VERSION")
        .unwrap_or("0.1.0")
        .to_string()
}

fn instance_state_text(state: &RunState) -> String {
    match state {
        RunState::Uninitialized => "uninitialized",
        RunState::Installing => "installing",
        RunState::Ready => "ready",
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::Stopping => "stopping",
        RunState::Stopped => "stopped",
        RunState::Updating => "updating",
        RunState::Error => "error",
    }
    .to_string()
}

fn git_run(git: &Path, dir: &Path, args: &[&str]) -> Option<proc::CmdOutput> {
    proc::run(git, args, Some(dir), &proc::git_env()).ok()
}

fn git_origin(git: &Path, dir: &Path) -> Option<String> {
    let out = git_run(git, dir, &["remote", "get-url", "origin"])?;
    if !out.ok() {
        return None;
    }
    let text = out.stdout.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn git_dirty(git: &Path, dir: &Path) -> bool {
    git_run(git, dir, &["status", "--porcelain"])
        .filter(|o| o.ok())
        .map(|o| !o.stdout.trim().is_empty())
        .unwrap_or(false)
}

/// ComfyUI 本体只关心已跟踪文件的修改；`custom_nodes` 等未跟踪目录由节点同步单独负责。
fn git_dirty_tracked(git: &Path, dir: &Path) -> bool {
    git_run(git, dir, &["status", "--porcelain", "--untracked-files=no"])
        .filter(|o| o.ok())
        .map(|o| !o.stdout.trim().is_empty())
        .unwrap_or(false)
}

fn git_has_local_commits(git: &Path, dir: &Path) -> bool {
    let out = git_run(
        git,
        dir,
        &["rev-list", "--count", "HEAD", "--not", "--remotes"],
    );
    out.filter(|o| o.ok())
        .and_then(|o| o.stdout.trim().parse::<u64>().ok())
        .map(|n| n > 0)
        .unwrap_or(false)
}

pub fn core_state(inst: &InstanceConfig, git: Option<&Path>) -> SyncCoreState {
    let comfy = Path::new(&inst.root_dir).join("ComfyUI");
    let commit = snapshot::head_comfy(&inst.root_dir)
        .or_else(|| (!inst.comfy.commit.is_empty()).then(|| inst.comfy.commit.clone()));
    let has_git = comfy.join(".git").exists();
    let (origin, dirty, _local_commits) = match (git, has_git) {
        (Some(git), true) => (
            git_origin(git, &comfy),
            git_dirty_tracked(git, &comfy),
            git_has_local_commits(git, &comfy),
        ),
        _ => (None, false, false),
    };
    SyncCoreState {
        channel: inst.comfy.channel.clone(),
        reference: inst.comfy.reference.clone(),
        version: inst.comfy.version.clone(),
        commit,
        origin_url: origin,
        dirty,
        worktree_hash: hash_core_tree(&comfy),
        patch: None,
        untracked_archive: None,
        full_archive: None,
        full_archive_sha256: None,
        patch_sha256: None,
        untracked_sha256: None,
    }
}

/// 源目标都是 Windows 启动器，但 wheel 仍可能来自别的 Python ABI。只做保守检查：
/// pure wheel (`none/any`) 放行；带 CPython 标签的必须命中目标 major.minor。
/// 源实例是导入的旧环境时，`engine_detail` 可能为空，拿不到 PyTorch 索引。
/// 但 `torch==2.14.1+cu130` 的 local label 已经足够推出官方索引，
/// 否则 uv 会先命中 PyPI 镜像里的 CPU 版 torch，然后报“找不到 +cu130”。
fn torch_local_tag(python: &SyncPythonState) -> Option<&str> {
    for version in [
        python.packages.get("torch").map(String::as_str),
        python.torch.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let Some(local) = version.split_once('+').map(|(_, local)| local) else {
            continue;
        };
        let tag = local.trim();
        if tag == "cpu" || tag.starts_with("cu") || tag.starts_with("rocm") {
            return Some(tag);
        }
    }
    None
}

fn torch_index_fallback(python: &SyncPythonState) -> Option<String> {
    torch_local_tag(python).map(|tag| format!("https://download.pytorch.org/whl/{tag}/"))
}

/// 引擎元数据里的 `torchIndexUrl` 有时和实际安装的 torch 不一致：例如源端
/// 实际是 `torch==2.10.0+cu130`，登记却写成 `/whl/cpu/`。这种索引下永远
/// 找不到 `+cu130`，必须让 local label 推出的索引优先。
fn torch_index_for(python: &SyncPythonState) -> Option<String> {
    let explicit = python
        .torch_index_url
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let Some(tag) = torch_local_tag(python) else {
        return explicit.or_else(|| torch_index_fallback(python));
    };
    let Some(fallback) = torch_index_fallback(python) else {
        return explicit;
    };
    match explicit {
        Some(url) if !url.to_lowercase().contains(&tag.to_lowercase()) => Some(fallback),
        Some(url) => Some(url),
        None => Some(fallback),
    }
}

pub(crate) fn custom_wheel_tags(filename: &str) -> Vec<String> {
    let stem = filename.trim_end_matches(".whl");
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 3 {
        parts[parts.len() - 3..]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    }
}

fn cp_short(version: &str) -> String {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("cp{major}{minor}"),
        _ => String::new(),
    }
}

fn normalized_platform(platform: &str) -> String {
    platform.trim().to_ascii_lowercase().replace('-', "_")
}

fn source_cuda(python: &SyncPythonState) -> Option<String> {
    let local = python
        .torch
        .as_deref()?
        .split_once('+')
        .map(|(_, local)| local)?;
    for part in local.split(['.', '-']) {
        if let Some(rest) = part.strip_prefix("cu") {
            let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
    }
    None
}

/// 判断 wheel 是否兼容**源客户端环境**。
///
/// 目标端的 Python / torch 会被同步成源端同一套，因此判断基准必须是源端
/// 实例的状态，而不是目标当前环境。返回 `None` 表示兼容或无法判断。
fn wheel_compat_reason(wheel: &CustomWheel, python: &SyncPythonState) -> Option<String> {
    let filename = wheel.filename.as_str();
    if !filename.to_ascii_lowercase().ends_with(".whl") {
        return None;
    }
    let tags = crate::accel::parse_wheel_name(filename)?;
    let env = crate::accel::EnvReport {
        python: python.version.clone(),
        abi: cp_short(&python.version),
        platform: normalized_platform(&python.platform),
        torch: python.torch.clone(),
        torch_cuda: source_cuda(python),
        cxx11: None,
        torchvision: None,
        torchaudio: None,
        torchcodec: None,
    };
    if !env.platform.is_empty() && !crate::accel::platform_ok(&tags, &env) {
        return Some(format!(
            "轮子平台 {} 与源客户端 {} 不匹配",
            tags.platform, env.platform
        ));
    }
    if !crate::accel::python_ok(&tags, &env) {
        return Some(format!(
            "轮子 Python 标签 {} 与源客户端 Python {} 不兼容",
            tags.python, python.version
        ));
    }
    if !crate::accel::torch_ok(&tags, &env) {
        return Some(match tags.torch.as_deref() {
            Some(tag) => format!(
                "轮子要求 torch {tag}，源客户端是 {}",
                python.torch.as_deref().unwrap_or("未安装 torch")
            ),
            None => "轮子 torch 标签与源客户端不兼容".to_string(),
        });
    }
    if !crate::accel::cuda_ok(&tags, &env) {
        return Some(match tags.cuda.as_deref() {
            Some(tag) => format!(
                "轮子要求 CUDA {tag}，源客户端是 {}",
                env.torch_cuda.as_deref().unwrap_or("未检测到 CUDA")
            ),
            None => "轮子 CUDA 标签与源客户端不兼容".to_string(),
        });
    }
    None
}

/// 在构建源端 Python 状态时记录不兼容 wheel。
///
/// 同步目标是让目标实例与源实例完全一致，因此兼容性只作为提示，不能
/// 把已安装的 wheel 从制品列表里剔除。否则目标端会永久少一个包，反而
/// 破坏“一模一样”的语义。
fn mark_incompatible_wheels(state: &mut SyncPythonState) {
    let snapshot = state.clone();
    for wheel in &mut state.custom_wheels {
        if !wheel.available || wheel.incompatible {
            continue;
        }
        if let Some(reason) = wheel_compat_reason(wheel, &snapshot) {
            wheel.incompatible = true;
            wheel.compat_warning = Some(reason);
        }
    }
}

/// Python 侧元数据：完整版本、ABI、平台，以及每个 dist 的PEP 610 `direct_url`。
fn python_meta(python: &Path) -> Option<serde_json::Value> {
    const SCRIPT: &str = r#"
import sys, sysconfig, json, platform
import importlib.metadata as md
out = {
  "version": platform.python_version(),
  "abi": sysconfig.get_config_var("SOABI") or "",
  "platform": sysconfig.get_platform(),
  "packages": [],
}
for d in md.distributions():
    try:
        name = d.metadata.get("Name") or ""
        version = d.version or ""
        direct = d.read_text("direct_url.json")
        direct = json.loads(direct) if direct else None
        out["packages"].append({"name": name, "version": version, "directUrl": direct})
    except Exception as e:
        out["packages"].append({"name": "", "version": "", "error": str(e)})
print(json.dumps(out))
"#;
    let out = proc::run(
        python,
        &["-c", SCRIPT],
        Some(python.parent().unwrap_or(Path::new("."))),
        &BTreeMap::new(),
    )
    .ok()?;
    if !out.ok() {
        log::warn!("读取 Python 元数据失败：{}", out.detail());
        return None;
    }
    serde_json::from_str(out.stdout.trim()).ok()
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file:///")?;
    let decoded = percent_decode(raw);
    Some(PathBuf::from(decoded.replace('/', "\\")))
}

pub(crate) fn percent_decode(raw: &str) -> String {
    // 必须始终在字节上解码：`&raw[i + 1..i + 3]` 会在 `%` 后跟多字节 UTF-8
    // 时切在字符中间，直接 panic。
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

fn package_direct_url(pkg: &serde_json::Value) -> Option<&serde_json::Value> {
    pkg.get("directUrl").filter(|v| !v.is_null())
}

/// PEP 503 规范化：包名里的 `-` / `_` / `.` 等价，且大小写不敏感。
/// 目标端安装后 direct_url 里的 dist 名可能从 `demo-pkg` 写成 `demo_pkg`：
/// 不能因此把它当成“新增一个、删除一个”。
pub(crate) fn canonical_dist_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separated = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !out.is_empty() && !separated {
                out.push('-');
            }
            separated = true;
        } else {
            out.push(c.to_ascii_lowercase());
            separated = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// PEP 610 的archive_info 里可能保存了 wheel 内容哈希，即使原始文件已被清理，
/// 也不应该因此把“同一个wheel”误报为变化。
fn direct_url_sha256(direct: &serde_json::Value) -> String {
    let info = direct.get("archive_info");
    if let Some(hash) = info
        .and_then(|v| v.get("hashes"))
        .and_then(|v| v.get("sha256"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
    {
        return hash.to_ascii_lowercase();
    }
    info.and_then(|v| v.get("hash"))
        .and_then(|v| v.as_str())
        .and_then(|v| v.strip_prefix("sha256="))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default()
}

fn custom_wheels_from_meta(
    meta: &serde_json::Value,
    wheels_dir: Option<&Path>,
) -> Vec<CustomWheel> {
    let mut out = Vec::new();
    let Some(pkgs) = meta.get("packages").and_then(|v| v.as_array()) else {
        return out;
    };
    for pkg in pkgs {
        let Some(direct) = package_direct_url(pkg) else {
            continue;
        };
        let name = pkg
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let version = pkg
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let url = direct.get("url").and_then(|v| v.as_str()).unwrap_or("");
        // 加速库的社区轮子常被改名后从本地安装；原始下载 URL 记在这里。
        // 同步时优先把 URL 交给目标端，不再把本地缓存文件打进同步包。
        let origin_url = direct
            .get("comfyfleet_origin")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
        let dir_info = direct.get("dir_info");
        let editable = dir_info
            .and_then(|v| v.get("editable"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut wheel = CustomWheel {
            name,
            version,
            filename: String::new(),
            sha256: direct_url_sha256(direct),
            size: 0,
            tags: Vec::new(),
            source: "missing".to_string(),
            archive: None,
            available: false,
            missing_reason: None,
            incompatible: false,
            compat_warning: None,
            source_ref: None,
            vcs: None,
            commit: None,
        };
        // PEP 610 的 VCS 安装（git+https/ssh）：URL 是仓库地址，不是可下载文件。
        // 以前把它当 remote-url 去下载，落成 `ffmpy.git` 这种文件再交给 uv，必然失败。
        if let Some(vcs_info) = direct.get("vcs_info") {
            let vcs = vcs_info
                .get("vcs")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let commit = vcs_info
                .get("commit_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_string();
            wheel.vcs = Some(vcs.clone());
            wheel.commit = Some(commit.clone());
            wheel.source_ref = Some(url.to_string());
            wheel.filename = format!("{}.git", wheel.name);
            wheel.source = "vcs".to_string();
            let local_repo = url.starts_with("file:");
            wheel.available = vcs == "git" && !url.is_empty() && !local_repo;
            wheel.missing_reason = if !wheel.available {
                Some(if local_repo {
                    "Git 仓库位于源端本机，目标端无法访问；请改用可访问的仓库地址或提供 wheel"
                        .to_string()
                } else {
                    format!("不支持的 VCS 来源，{vcs} {}", redact_url(url))
                })
            } else if commit.is_empty() {
                Some("Git 仓库未记录固定commit，将安装默认分支".to_string())
            } else {
                Some(format!(
                    "Git 仓库安装，固定commit {}",
                    short_chars(&commit, 12)
                ))
            };
            // Use URL+commit as a stable fingerprint.
            wheel.sha256 =
                sha256_bytes(format!("vcs\0{vcs}\0{url}\0{commit}\0{}", wheel.version).as_bytes());
            out.push(wheel);
            continue;
        }
        if dir_info.is_some() {
            wheel.source = if editable {
                "editable".to_string()
            } else {
                "local-dir".to_string()
            };
            wheel.filename = format!("{}.zip", wheel.name);
            if let Some(path) = file_url_to_path(url) {
                if path.is_dir() {
                    wheel.available = true;
                    wheel.source_ref = Some(path.to_string_lossy().to_string());
                    wheel.missing_reason = Some(if editable {
                        "可编辑安装，将打包源码目录".to_string()
                    } else {
                        "本地目录安装，将打包源码目录".to_string()
                    });
                } else {
                    wheel.missing_reason = Some(if editable {
                        "可编辑安装的源目录已不存在".to_string()
                    } else {
                        "本地目录安装的源目录已不存在".to_string()
                    });
                }
            } else {
                wheel.missing_reason = Some(if editable {
                    "可编辑安装来源不是本地目录".to_string()
                } else {
                    "本地目录安装来源不是本地目录".to_string()
                });
            }
            out.push(wheel);
            continue;
        }
        if let Some(path) = file_url_to_path(url) {
            // 启动器从 URL 下载后再本地安装的 wheel，会同时记录本地文件和原始 URL。
            // 同步仍按 URL 交给目标端，不把本地缓存文件打进同步包。
            if let Some(origin) = origin_url {
                wheel.filename = remote_filename(origin);
                wheel.source = "remote-url".to_string();
                wheel.available = true;
                wheel.source_ref = Some(origin.to_string());
                wheel.tags = custom_wheel_tags(&wheel.filename);
                wheel.missing_reason =
                    Some("安装来源是远程 URL，目标端直接按 URL 安装".to_string());
                out.push(wheel);
                continue;
            }
            let proposed = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            // 本地 wheel：只在用户指定的 wheel 库里找同名文件（其次按包名+版本）。
            // 原始安装路径不参与同步——用户可能早就把下载的 whl 删了。
            let found = wheels_dir.and_then(|dir| {
                crate::wheelhouse::find(dir, &wheel.name, &wheel.version, Some(&proposed))
            });
            if let Some(found) = found {
                let filename = found
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&proposed)
                    .to_string();
                wheel.tags = custom_wheel_tags(&filename);
                wheel.source = if filename.to_ascii_lowercase().ends_with(".whl") {
                    "local-wheel".to_string()
                } else {
                    "local-archive".to_string()
                };
                wheel.filename = filename.clone();
                wheel.archive = Some(format!("env/wheels/{filename}"));
                wheel.source_ref = Some(found.to_string_lossy().to_string());
                wheel.size = std::fs::metadata(&found).map(|m| m.len()).unwrap_or(0);
                wheel.available = true;
                wheel.missing_reason =
                    Some("使用 wheel 库中的文件（原始安装路径不参与同步）".to_string());
            } else if let Some(origin) = origin_url {
                // 本地 wheel 库没有，但安装时记录了来源：让 prepare 重新下载并规范化。
                wheel.filename = remote_filename(origin);
                wheel.source = "remote-url".to_string();
                wheel.available = true;
                wheel.source_ref = Some(origin.to_string());
                wheel.tags = custom_wheel_tags(&wheel.filename);
                wheel.missing_reason =
                    Some("wheel 库里没有副本，按安装时记录的远程来源重新下载".to_string());
            } else {
                let reason = match wheels_dir {
                    Some(dir) => format!(
                        "wheel 库里没有这个文件，请把 {proposed} 放进 {} 后重新扫描",
                        dir.display()
                    ),
                    None => {
                        "没有可用的“本地 Wheel 库”目录：在设置里选一个存在的目录，把本地安装的 wheel 放进去"
                            .to_string()
                    }
                };
                wheel.filename = proposed;
                wheel.source = "missing".to_string();
                wheel.missing_reason = Some(reason);
            }
        } else if url.starts_with("http://") || url.starts_with("https://") {
            wheel.source = "remote-url".to_string();
            wheel.filename = remote_filename(url);
            wheel.available = true;
            wheel.source_ref = Some(url.to_string());
            wheel.missing_reason = Some("远程 URL，目标端直接下载安装".to_string());
            wheel.tags = custom_wheel_tags(&wheel.filename);
        } else {
            wheel.missing_reason = Some(format!("无法识别的安装来源：{}", redact_url(url)));
        }
        out.push(wheel);
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// 源实例里缺文件的自定义依赖：目标端装不上，同步前要让用户补齐或确认跳过。
pub fn missing_custom_wheels(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
) -> Vec<MissingWheel> {
    python_state(inst, settings)
        .custom_wheels
        .into_iter()
        .filter(|wheel| !wheel.available)
        .map(|wheel| MissingWheel {
            name: wheel.name,
            version: wheel.version,
            filename: wheel.filename,
            source: wheel.source,
            reason: wheel
                .missing_reason
                .unwrap_or_else(|| "缺少可同步的文件".to_string()),
        })
        .collect()
}

fn python_state(inst: &InstanceConfig, settings: &GlobalSettings) -> SyncPythonState {
    let python = crate::runtime::resolve_python(inst).map(|p| p.python);
    let mut state = SyncPythonState {
        version: inst.python_version.clone(),
        short_version: inst.python_version.clone(),
        abi: String::new(),
        platform: String::new(),
        packages: BTreeMap::new(),
        torch: None,
        torch_index_url: inst
            .engine_detail
            .as_ref()
            .map(|d| d.torch_index_url.clone())
            .filter(|s| !s.trim().is_empty()),
        custom_wheels: Vec::new(),
    };
    if let Some(python) = python.as_deref() {
        if let Some(meta) = python_meta(python) {
            if let Some(v) = meta.get("version").and_then(|v| v.as_str()) {
                state.version = v.to_string();
                state.short_version = v.split('.').take(2).collect::<Vec<_>>().join(".");
            }
            state.abi = meta
                .get("abi")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            state.platform = meta
                .get("platform")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 本地 wheel 只从用户指定的库目录取，原始安装路径不参与同步。
            let wheels_dir = crate::wheelhouse::dir(settings);
            state.custom_wheels = custom_wheels_from_meta(&meta, wheels_dir.as_deref());
        }
    }
    // 包清单统一走快照的 freeze 逻辑，但不哈希节点目录（心跳比15 秒一次）。
    let (packages, _note) = snapshot::package_versions(inst);
    state.packages = packages;
    state.torch = state.packages.get("torch").cloned();
    mark_incompatible_wheels(&mut state);
    state
}

fn node_state(inst: &InstanceConfig, git: Option<&Path>) -> Vec<SyncNodeState> {
    node_state_with(inst, git, None).unwrap_or_default()
}

fn node_state_with(
    inst: &InstanceConfig,
    git: Option<&Path>,
    prep: Option<&SyncPrepare>,
) -> Result<Vec<SyncNodeState>, AppError> {
    let dir = Path::new(&inst.root_dir)
        .join("ComfyUI")
        .join("custom_nodes");
    let Ok(items) = scan_nodes(&dir) else {
        return Ok(Vec::new());
    };
    let total = items.len();
    let mut out = Vec::new();
    // 节点扫描只保留一行：message 固定为总范围，detail 随当前节点刷新。
    // SyncPrepare::update 只在 message/phase 变化时追加日志，因此不会刷屏。
    if let Some(context) = prep {
        if total > 0 {
            context.update(
                "扫描节点",
                5,
                format!("节点 1-{total}/{total}"),
                "逐个检查节点的 git 状态与内容哈希",
            );
        }
    }
    for item in items {
        if let Some(context) = prep {
            context.check()?;
            if total > 0 {
                context.update(
                    "扫描节点",
                    5,
                    format!("节点 1-{total}/{total}"),
                    format!("检查 {} 的 git 状态与内容哈希", item.name),
                );
            }
        }
        let node_dir = if item.enabled {
            dir.join(&item.name)
        } else {
            dir.join(format!("{}.disabled", item.name))
        };
        let has_git = node_dir.join(".git").exists();
        let (origin, dirty, local_commits) = match (git, has_git) {
            (Some(git), true) => (
                git_origin(git, &node_dir),
                git_dirty(git, &node_dir),
                git_has_local_commits(git, &node_dir),
            ),
            _ => (None, false, false),
        };
        out.push(SyncNodeState {
            name: item.name,
            enabled: item.enabled,
            kind: if has_git {
                "git".into()
            } else {
                "manual".into()
            },
            // 清单会通过网络传给目标机并落盘，URL 里的凭据必须先脱敏。
            url: item.url.or(origin).map(|url| redact_url(&url)),
            commit: item.commit,
            dirty,
            has_local_commits: local_commits,
            worktree_hash: hash_tree(&node_dir, false),
            archive: None,
            archive_sha256: None,
            archive_size: 0,
        });
    }
    Ok(out)
}

/// 同步模型的接收根目录。整包模型需要“无类型”的根目录；优先用户明确设为
/// “下载落点”的共享目录，其次第一个启用的共享根目录，最后才是实例自己的 models。
/// 类型目录只参与扫描比对，不能作为整包写入目标。
fn model_store(inst: &InstanceConfig, settings: &GlobalSettings) -> PathBuf {
    if inst.storage.use_shared_models {
        let mut fallback: Option<&str> = None;
        for dir in &settings.storage.shared_models {
            let path = dir.path.trim();
            if path.is_empty()
                || dir.type_name.is_some()
                || crate::storage::shared_disabled(inst, path)
            {
                continue;
            }
            if dir.is_default {
                return PathBuf::from(path);
            }
            if fallback.is_none() {
                fallback = Some(path);
            }
        }
        if let Some(path) = fallback {
            return PathBuf::from(path);
        }
    }
    Path::new(&inst.root_dir).join("ComfyUI").join("models")
}

fn store_id(path: &Path) -> String {
    let marker = path.join(STORE_MARKER);
    if let Ok(raw) = fs::read_to_string(&marker) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                // 旧版本的 `path-*` ID 是路径哈希，不能证明物理目录相同；
                // 读到它时顺便迁移成随机 ID，避免以后继续误判。
                if !id.is_empty() && !id.starts_with("path-") {
                    return id.to_string();
                }
            }
        }
    }
    // 不能用路径哈希当模型库 ID：源端和目标端都用 `D:\models_test` 时，
    // 两台机器会得到同一个 ID，从而被误判成“同一物理目录”而跳过传输。
    // 每个真实目录首次登记时生成随机 ID；同一 NAS 目录共享同一份 marker，
    // 因此仍能识别为同一个库。
    let id = format!("store-{}", uuid::Uuid::new_v4().simple());
    if fs::create_dir_all(path).is_ok() {
        let text = serde_json::json!({"id": id, "createdAt": now()});
        if let Ok(raw) = serde_json::to_string_pretty(&text) {
            let _ = fs::write(&marker, raw);
        }
    }
    id
}

/// 只有真正的随机 marker ID 才能证明两边是同一个物理目录。旧版本用路径
/// 哈希生成的 `path-` ID 无法区分“同一台机器的同一个目录”和“两台机器刚好
/// 同路径”，一律按不同库处理，宁可多传一次也不能漏模型。
fn same_model_store(a: &str, b: &str) -> bool {
    !a.is_empty() && a == b && !a.starts_with("path-")
}

fn model_entries(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    prep: Option<&SyncPrepare>,
) -> Result<Vec<(String, String, PathBuf, u64)>, AppError> {
    let plan = crate::storage::plan(inst, settings);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for dir in &plan.models {
        if !dir.exists {
            continue;
        }
        let base = Path::new(&dir.path);
        let fixed_type = dir
            .type_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        for entry in WalkDir::new(base)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if let Some(context) = prep {
                context.check()?;
                context.note(format!("枚举模型目录：{}", path.display()));
            }
            let rel = match path.strip_prefix(base) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let parts: Vec<String> = rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().to_string()),
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                continue;
            }
            let (kind, inner) = match fixed_type {
                Some(kind) => (kind.to_string(), parts.join("/")),
                None => {
                    if parts.len() < 2 {
                        continue;
                    }
                    (parts[0].clone(), parts[1..].join("/"))
                }
            };
            let file_name = parts.last().cloned().unwrap_or_default();
            if file_name == STORE_MARKER || is_skipped_file(&file_name) {
                continue;
            }
            let relative = format!("{kind}/{inner}");
            let key = relative.to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((kind, relative, path.to_path_buf(), size));
        }
    }
    Ok(out)
}

fn capture_models(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
) -> (Vec<SyncModelFile>, String) {
    capture_models_scoped(inst, settings, None).unwrap_or_else(|_| {
        (
            Vec::new(),
            model_store(inst, settings).to_string_lossy().to_string(),
        )
    })
}

/// 扫描模型时逐文件上报进度；大文件哈希按块检查取消，避免“点了停止还要等整块读完”。
fn capture_models_scoped(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    prep: Option<&SyncPrepare>,
) -> Result<(Vec<SyncModelFile>, String), AppError> {
    let store = model_store(inst, settings);
    let _ = store_id(&store);
    if let Some(context) = prep {
        context.update("扫描模型", 13, "枚举模型目录", "统计文件数量与体积");
    }
    let entries = model_entries(inst, settings, prep)?;
    let total_bytes: u64 = entries.iter().map(|(_, _, _, size)| *size).sum();
    let total_count = entries.len();
    // 模型扫描同样只保留一行总范围；message 保持不变，进度写在 detail。
    let scan_label = if total_count > 0 {
        format!("模型 1-{total_count}/{total_count}")
    } else {
        "模型 0/0".to_string()
    };
    if let Some(context) = prep {
        context.update(
            "扫描模型",
            14,
            scan_label.clone(),
            format!(
                "找到 {total_count} 个模型文件，合计 {}",
                human_bytes(total_bytes)
            ),
        );
    }
    let mut files = Vec::with_capacity(total_count);
    let mut done_bytes: u64 = 0;
    for (index, (kind, relative_path, path, size)) in entries.into_iter().enumerate() {
        if let Some(context) = prep {
            context.check()?;
            let percent = if total_bytes > 0 {
                15 + ((done_bytes.saturating_mul(20) / total_bytes) as u8).min(20)
            } else {
                15
            };
            context.update(
                "扫描模型",
                percent,
                scan_label.clone(),
                format!("准备读取 {}", relative_path),
            );
        }
        let sha256 = match prep {
            Some(context) => match sha256_file_cached_with_progress(&path, |read, total| {
                context.check()?;
                context.note(format!(
                    "读取模型 {}/{}：{}（{}/{}）",
                    index + 1,
                    total_count.max(1),
                    relative_path,
                    human_bytes(read),
                    human_bytes(total)
                ));
                Ok(())
            }) {
                Ok(hash) => Some(hash),
                Err(error) if error.code == "SYNC_CANCELLED" => return Err(error),
                Err(_) => None,
            },
            None => sha256_file_cached(&path).ok(),
        };
        files.push(SyncModelFile {
            file_id: format!("m{:05}", index + 1),
            kind,
            relative_path,
            size,
            sha256,
            source_path: Some(path.to_string_lossy().to_string()),
        });
        done_bytes = done_bytes.saturating_add(size);
        if let Some(context) = prep {
            context.add_files(1, size);
            let percent = if total_bytes > 0 {
                15 + ((done_bytes.saturating_mul(20) / total_bytes) as u8).min(20)
            } else {
                15
            };
            context.update(
                "扫描模型",
                percent,
                scan_label.clone(),
                format!(
                    "已扫描 {}/{} 个模型，累计 {}",
                    index + 1,
                    total_count,
                    human_bytes(done_bytes)
                ),
            );
        }
    }
    Ok((files, store.to_string_lossy().to_string()))
}

pub fn last_applied(instance_id: &str) -> Option<LastApplied> {
    read_last_applied(instance_id)
}

fn read_last_applied(instance_id: &str) -> Option<LastApplied> {
    let raw = fs::read_to_string(target_state_path()).ok()?;
    let map: HashMap<String, LastApplied> = serde_json::from_str(&raw).ok()?;
    map.get(instance_id).cloned()
}

fn write_last_applied(instance_id: &str, value: &LastApplied) -> Result<(), AppError> {
    let path = target_state_path();
    let mut map: HashMap<String, LastApplied> = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    map.insert(instance_id.to_string(), value.clone());
    let text = serde_json::to_string_pretty(&map)?;
    config::atomic_write(&path, text.as_bytes())
}

/// 采集一个实例的完整可同步状态。`stage=false` 用于目标端上报和差异计算。
pub fn capture_instance_state(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    git: Option<&Path>,
    with_models: bool,
) -> InstanceState {
    let (nodes, core, python) = (
        node_state(inst, git),
        core_state(inst, git),
        python_state(inst, settings),
    );
    let (models, store) = if with_models {
        capture_models(inst, settings)
    } else {
        (
            Vec::new(),
            model_store(inst, settings).to_string_lossy().to_string(),
        )
    };
    let store_id = store_id(Path::new(&store));
    InstanceState {
        instance_id: inst.id.clone(),
        name: inst.name.clone(),
        root_dir: inst.root_dir.clone(),
        app_version: app_version(),
        server: inst.server.clone(),
        state: instance_state_text(&inst.state),
        model_store_id: store_id,
        core,
        python,
        nodes,
        models,
        model_store_path: store,
        disk_free: free_space(Path::new(&inst.root_dir)),
        last_applied: read_last_applied(&inst.id),
    }
}

/// 源端准备专用：只采集勾选的内容组。未勾选的组不扫描，尤其不会为了同步节点而
/// 去哈希整个模型库。准备线程可以取消，所以这里返回 Result。
pub fn capture_instance_state_scoped(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    git: Option<&Path>,
    scope: SyncScope,
    prep: Option<&SyncPrepare>,
) -> Result<InstanceState, AppError> {
    if let Some(context) = prep {
        context.check()?;
        context.update("扫描源实例", 3, "读取实例状态", "读取 ComfyUI 与实例登记");
    }
    let core = if scope.comfyui {
        let core = core_state(inst, git);
        if let Some(context) = prep {
            context.check()?;
            context.note(format!(
                "ComfyUI：{}，commit {}",
                core.version.as_deref().unwrap_or("未登记版本"),
                core.commit
                    .as_deref()
                    .map(|c| short_chars(c, 10))
                    .unwrap_or_else(|| "—".to_string())
            ));
        }
        core
    } else {
        SyncCoreState::default()
    };

    let nodes = if scope.nodes {
        if let Some(context) = prep {
            context.check()?;
            context.update("扫描节点", 5, "读取自定义节点", "列出 custom_nodes");
        }
        node_state_with(inst, git, prep)?
    } else {
        Vec::new()
    };

    let python = if scope.python {
        if let Some(context) = prep {
            context.check()?;
            context.update("扫描 Python", 12, "读取 Python 环境", "读取解释器与包清单");
        }
        let python = python_state(inst, settings);
        if let Some(context) = prep {
            context.note(format!(
                "Python {}，已登记 {} 个包，{} 个自建 wheel",
                if python.version.is_empty() {
                    "未检测到"
                } else {
                    python.version.as_str()
                },
                python.packages.len(),
                python.custom_wheels.len()
            ));
        }
        python
    } else {
        SyncPythonState::default()
    };

    let (models, store) = if scope.models {
        capture_models_scoped(inst, settings, prep)?
    } else {
        (
            Vec::new(),
            model_store(inst, settings).to_string_lossy().to_string(),
        )
    };
    let store_id = if scope.models {
        store_id(Path::new(&store))
    } else {
        String::new()
    };
    Ok(InstanceState {
        instance_id: inst.id.clone(),
        name: inst.name.clone(),
        root_dir: inst.root_dir.clone(),
        app_version: app_version(),
        server: inst.server.clone(),
        state: instance_state_text(&inst.state),
        model_store_id: store_id,
        core,
        python,
        nodes,
        models,
        model_store_path: store,
        disk_free: free_space(Path::new(&inst.root_dir)),
        last_applied: read_last_applied(&inst.id),
    })
}

/// 给目标端/预览用的轻量状态，模型只记路径、大小、已有缓存哈希。
pub fn capture_target_state(inst: &InstanceConfig, settings: &GlobalSettings) -> InstanceState {
    let state = capture_instance_state(inst, settings, None, true);
    state
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec.max(0.0).round() as u64))
}

/// 估算目标落盘需要的空间：载草+ 修改模型的旧文件备份 + 64 MiB 余量。
/// 这是保守值；真正写盘前还会再查一次目标剩余空间。
/// 目标端是否必须显式确认才能覆盖。两组条件都要看，
/// - 预览时记录的指纹 vs 当前指纹（预览后又改了），
/// - lastApplied 指纹 vs 当前指纹（预览前就已经改过，只是预览把改动算进去了）。
pub fn target_drift_requires_confirmation(
    preview_fingerprint: &str,
    current_fingerprint: &str,
    last_applied: Option<&LastApplied>,
    allow_dirty_target: bool,
) -> bool {
    if allow_dirty_target {
        return false;
    }
    let changed_since_preview =
        !preview_fingerprint.is_empty() && preview_fingerprint != current_fingerprint;
    let changed_since_last = last_applied
        .map(|last| last.fingerprint != current_fingerprint)
        .unwrap_or(false);
    changed_since_preview || changed_since_last
}

pub fn required_space(diff: &SyncDiff, totals: &SyncTotals) -> u64 {
    let old_models: u64 = diff.models.modified.iter().map(|m| m.from_size).sum();
    totals
        .bytes
        .saturating_add(diff.models.transfer_bytes)
        .saturating_add(old_models)
        .saturating_add(64 * 1024 * 1024)
}

/// 指纹算法一旦改变（例如从整文件内容改成单文件哈希组合），历史lastApplied
/// 就不能再直接比较；带版本前缀能避免升级后误报/漏报目标漂移。
const FINGERPRINT_VERSION: &str = "v3:";

pub fn state_fingerprint(state: &InstanceState) -> String {
    state_fingerprint_scoped(state, SyncScope::all())
}

/// 只指纹本次勾选的内容组。未勾选组即使目标机后来发生变化，也不应阻止本次
/// 只同步 ComfyUI / 节点 / Python / 模型中的某几项。
///
/// 全选时保持旧格式不变，避免升级后把历史 lastApplied 全部判成目标漂移；
/// 部分选择时加 scope 前缀，防止不同选择范围之间发生指纹碰撞。
pub fn state_fingerprint_scoped(state: &InstanceState, scope: SyncScope) -> String {
    let all = scope == SyncScope::all();
    let mut text = String::new();
    if !all {
        text.push_str(&format!(
            "scope:{}{}{}{}|",
            u8::from(scope.comfyui),
            u8::from(scope.nodes),
            u8::from(scope.python),
            u8::from(scope.models)
        ));
    }
    if scope.comfyui {
        text.push_str(&state.core.commit.clone().unwrap_or_default());
        text.push('|');
        text.push_str(&state.core.worktree_hash);
        text.push('|');
        text.push_str(&state.server.listen);
        text.push(':');
        text.push_str(&state.server.port.to_string());
        text.push('|');
        text.push_str(state.server.cors_origin.as_deref().unwrap_or(""));
        text.push('|');
        for arg in &state.server.extra_args {
            text.push_str(arg);
            text.push(';');
        }
        text.push('|');
    }
    if scope.python {
        text.push_str(&state.python.version);
        text.push('|');
        for (k, v) in &state.python.packages {
            text.push_str(k);
            text.push('=');
            text.push_str(v);
            text.push(';');
        }
        text.push('|');
    }
    if scope.nodes {
        for node in &state.nodes {
            text.push_str(&node.name);
            text.push(':');
            text.push_str(&node.worktree_hash);
            text.push(':');
            text.push_str(if node.enabled { "1" } else { "0" });
            text.push(';');
        }
        text.push('|');
    }
    if scope.models {
        for model in &state.models {
            text.push_str(&model.relative_path);
            text.push(':');
            text.push_str(&model.size.to_string());
            text.push(':');
            text.push_str(model.sha256.as_deref().unwrap_or(""));
            text.push(';');
        }
    }
    format!("{FINGERPRINT_VERSION}{}", sha256_bytes(text.as_bytes()))
}

/// 源端“准备数据”是否可复用的指纹。和 `InstanceState` 指纹分开：
/// - 清除运行状态、磁盘剩余空间、上次同步记录等每次都会变的值；
/// - 纳入同步范围、镜像设置和自定义 wheel 内容；
/// - 源实例路径或名称变化也重新准备，避免复用指向旧目录的代次。
pub fn preparation_fingerprint(
    state: &InstanceState,
    scope: SyncScope,
    settings: &GlobalSettings,
) -> String {
    let mut normalized = state.clone();
    normalized.state.clear();
    normalized.disk_free = 0;
    normalized.last_applied = None;
    // 只按本次勾选的模块识别内容：例如只同步节点时，模型文件变化不应该
    // 让节点代次失效；节点自身是否变化已经包含在 nodes 的 worktreeHash 里。
    if !scope.comfyui {
        normalized.core = Default::default();
        normalized.server = Default::default();
    }
    if !scope.python {
        normalized.python = Default::default();
    }
    if !scope.nodes {
        normalized.nodes.clear();
    }
    if !scope.models {
        normalized.models.clear();
        normalized.model_store_id.clear();
        normalized.model_store_path.clear();
    }
    let payload = serde_json::json!({
        "scope": scope,
        "source": normalized,
        "mirrors": &settings.mirrors,
        "keepOldVenvDays": settings.sync.keep_old_venv_days,
    });
    let text = serde_json::to_string(&payload).unwrap_or_default();
    format!("sp1:{}", sha256_bytes(text.as_bytes()))
}

fn manifest_artifacts_exist(manifest: &SyncManifest) -> bool {
    let dir = generation_dir(&manifest.generation_id);
    let exists = |rel: &str| dir.join(rel).is_file();
    if let Some(path) = manifest.source.core.full_archive.as_deref() {
        if !exists(path) {
            return false;
        }
    }
    if let Some(path) = manifest.source.core.patch.as_deref() {
        if !exists(path) {
            return false;
        }
    }
    if let Some(path) = manifest.source.core.untracked_archive.as_deref() {
        if !exists(path) {
            return false;
        }
    }
    for node in &manifest.source.nodes {
        if let Some(path) = node.archive.as_deref() {
            if !exists(path) {
                return false;
            }
        }
    }
    for wheel in &manifest.source.python.custom_wheels {
        if let Some(path) = wheel.archive.as_deref() {
            if !exists(path) {
                return false;
            }
        }
    }
    true
}

/// 查找一份已经准备好的代次。只复用“源实例 + 同步范围 + 内容指纹”完全一致
/// 的代次；代次里的载荷文件必须还在，避免缓存清理后复用空壳。
pub fn find_prepared_generation(
    source_instance_id: &str,
    scope: SyncScope,
    fingerprint: &str,
) -> Option<SyncManifest> {
    if fingerprint.trim().is_empty() {
        return None;
    }
    let mut best: Option<SyncManifest> = None;
    for entry in fs::read_dir(generations_dir()).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !safe_generation_id(&name) {
            continue;
        }
        let Ok(manifest) = load_manifest(&name) else {
            continue;
        };
        if manifest.format != SYNC_FORMAT
            || manifest.prepared_fingerprint != fingerprint
            || manifest.scope != scope
            || manifest.source_instance_id != source_instance_id
            || !manifest_artifacts_exist(&manifest)
        {
            continue;
        }
        let newer = best
            .as_ref()
            .map(|current| manifest.created_at > current.created_at)
            .unwrap_or(true);
        if newer {
            best = Some(manifest);
        }
    }
    best
}

// ---------------------------------------------------------------- 四组差异

fn wheel_delta(w: &CustomWheel) -> WheelDelta {
    WheelDelta {
        name: w.name.clone(),
        from: String::new(),
        to: w.version.clone(),
        sha256: (!w.sha256.is_empty()).then(|| w.sha256.clone()),
        available: w.available,
        reason: w.missing_reason.clone(),
        incompatible: w.incompatible,
        compat_warning: w.compat_warning.clone(),
    }
}

/// 两个 wheel 是否代表同一份安装内容。
///
/// 不能直接比`source` / `source_ref`：目标端安装后`direct_url.json` 指向代次目录，
/// 原始 wheel 被清理后来源会变成missing；这只说明“文件不再可取”，
/// 不代表包内容变了。优先比内容哈希，其次比 VCS 固定提交；
/// 最后才用文件名作为没有哈希时的保守判据。
fn wheel_content_same(source: &CustomWheel, target: &CustomWheel) -> bool {
    if canonical_dist_name(&source.name) != canonical_dist_name(&target.name)
        || source.version != target.version
    {
        return false;
    }
    // 一侧有内容哈希、另一侧没有（常见于目标端原始 wheel 已被清理）时，    // 没有证据证明内容变了；宁可暂不报差异，也不要制造“没动过却变了”的噪音。
    if source.sha256.is_empty() != target.sha256.is_empty() {
        return true;
    }
    if !source.sha256.is_empty() && !target.sha256.is_empty() {
        return source.sha256.eq_ignore_ascii_case(&target.sha256);
    }
    if source.source == "vcs" || target.source == "vcs" {
        if source.vcs != target.vcs {
            return false;
        }
        if let (Some(a), Some(b)) = (source.commit.as_deref(), target.commit.as_deref()) {
            if !a.is_empty() && !b.is_empty() {
                return a.eq_ignore_ascii_case(b);
            }
        }
        return source.source_ref == target.source_ref;
    }
    if !source.filename.is_empty() && !target.filename.is_empty() {
        return source.filename.eq_ignore_ascii_case(&target.filename);
    }
    // 一侧没有内容哈希、也没有文件名可查时，没有证据说明内容变了；
    // No content hash or filename evidence: treat as unchanged.
    true
}

pub fn diff_states(source: &InstanceState, target: &InstanceState) -> SyncDiff {
    diff_states_with_scope(source, target, SyncScope::all())
}

/// 两个包版本是否算同一个版本。
///
/// `uv pip freeze` 对直接 URL 包会从 wheel 文件名取版本；同一个 wheel 的
/// 本地构建标签可能只在一边出现（例如源端 `flash-attn==2.8.3`、目标端
/// `2.8.3+cu130...`，但 `pip list` 两边都是 2.8.3）。非 torch 保护包在
/// “只有一边带 +local”时按上游版本比较；两边都带不同的 +local 仍算不同
/// 构建，避免把 cu128 / cu130 混为一谈。torch 全家继续严格比较。
///
/// 空版本代表采集不到元数据（旧目标 / 直接 URL 包偶发），非保护包按
/// “未知”处理，不制造假变更；torch 全家仍报差异，避免掩盖真实构建问题。
fn package_versions_match(name: &str, left: &str, right: &str) -> bool {
    let left = percent_decode(left.trim());
    let right = percent_decode(right.trim());
    if left == right {
        return true;
    }
    if left.is_empty() || right.is_empty() {
        return !snapshot::is_protected_package(name);
    }
    if snapshot::is_protected_package(name) {
        return false;
    }
    let (left_base, left_local) = match left.split_once('+') {
        Some((base, local)) => (base, Some(local)),
        None => (left.as_str(), None),
    };
    let (right_base, right_local) = match right.split_once('+') {
        Some((base, local)) => (base, Some(local)),
        None => (right.as_str(), None),
    };
    if left_local.is_some() && right_local.is_some() {
        return false;
    }
    left_base == right_base
}

/// 同步后校验只看 ComfyUI 版本号，不再要求整个工作区哈希一致。
/// 同版本允许存在本地文件差异；版本缺失时才退回 commit 比较。
fn core_versions_match(left: &SyncCoreState, right: &SyncCoreState) -> bool {
    let version = |core: &SyncCoreState| {
        core.version
            .as_deref()
            .unwrap_or("")
            .trim()
            .trim_start_matches('v')
            .trim_start_matches('V')
            .to_string()
    };
    let left_version = version(left);
    let right_version = version(right);
    if !left_version.is_empty() && !right_version.is_empty() {
        return left_version.eq_ignore_ascii_case(&right_version);
    }
    left.commit == right.commit
}

/// 只比较本次勾选的内容组。未勾选组保持默认空差异，目标端确认时也就不会去写它。
pub fn diff_states_with_scope(
    source: &InstanceState,
    target: &InstanceState,
    scope: SyncScope,
) -> SyncDiff {
    let mut out = SyncDiff::default();

    // ---- ComfyUI 本体
    if scope.comfyui {
        out.core = CoreDiff {
            changed: source.core.commit != target.core.commit
                || source.core.channel != target.core.channel
                || source.core.version != target.core.version
                || source.core.worktree_hash != target.core.worktree_hash,
            from_commit: target.core.commit.clone(),
            to_commit: source.core.commit.clone(),
            from_version: target.core.version.clone(),
            to_version: source.core.version.clone(),
            from_channel: target.core.channel.clone(),
            to_channel: source.core.channel.clone(),
            source_dirty: source.core.dirty,
        };
        if out.core.source_dirty {
            out.warnings.push(
            "源端 ComfyUI 有本地修改：会作为补丁同步；同步后目标也是dirty 状态，后续快照恢复可能要求先清理工作区"
                .to_string(),
        );
        }

        // ---- 启动参数（监听/ 端口 / CORS / 自定义参数）
        // 目录类参数的值是本机路径，只参与展示和保留，不参与“是否变化”的比较。
        let (target_local_args, target_portable_args) =
            split_launch_args(&target.server.extra_args);
        let (source_local_args, source_portable_args) =
            split_launch_args(&source.server.extra_args);
        out.launch = LaunchDiff {
            changed: source.server.listen != target.server.listen
                || source.server.port != target.server.port
                || source.server.cors_origin != target.server.cors_origin
                || source_portable_args != target_portable_args,
            from_listen: target.server.listen.clone(),
            to_listen: source.server.listen.clone(),
            from_port: target.server.port,
            to_port: source.server.port,
            from_cors_origin: target.server.cors_origin.clone(),
            to_cors_origin: source.server.cors_origin.clone(),
            from_extra_args: target_portable_args,
            to_extra_args: source_portable_args,
            from_local_extra_args: target_local_args,
            to_local_extra_args: source_local_args,
        };
        if target.state == "running" {
            out.warnings
                .push("目标实例正在运行：开始同步时会先停止实".to_string());
        }
        if target.state == "starting" || target.state == "installing" || target.state == "updating"
        {
            out.warnings
                .push("目标实例正在启动/安装：建议等它稳定后再同步".to_string());
        }
    }

    // ---- Python
    if scope.python {
        let source_pkgs = &source.python.packages;
        let target_pkgs = &target.python.packages;
        for (name, version) in source_pkgs {
            match target_pkgs.get(name) {
                None => out.python.packages_added.push(snapshot::PkgDelta {
                    name: name.clone(),
                    from: String::new(),
                    to: version.clone(),
                }),
                Some(prev) if !package_versions_match(name, prev, version) => {
                    out.python.packages_changed.push(snapshot::PkgDelta {
                        name: name.clone(),
                        from: prev.clone(),
                        to: version.clone(),
                    })
                }
                _ => {}
            }
        }
        for (name, version) in target_pkgs {
            if !source_pkgs.contains_key(name) {
                out.python.packages_removed.push(snapshot::PkgDelta {
                    name: name.clone(),
                    from: version.clone(),
                    to: String::new(),
                });
            }
        }
        out.python.from_version = target.python.version.clone();
        out.python.to_version = source.python.version.clone();
        out.python.from_torch = target.python.torch.clone();
        out.python.to_torch = source.python.torch.clone();

        let source_wheels: BTreeMap<String, &CustomWheel> = source
            .python
            .custom_wheels
            .iter()
            .map(|w| (canonical_dist_name(&w.name), w))
            .collect();
        let target_wheels: BTreeMap<String, &CustomWheel> = target
            .python
            .custom_wheels
            .iter()
            .map(|w| (canonical_dist_name(&w.name), w))
            .collect();
        for (name, wheel) in &source_wheels {
            if wheel.incompatible {
                out.python.incompatible_wheels.push(wheel_delta(wheel));
            }
            if !wheel.available {
                let mut delta = wheel_delta(wheel);
                delta.available = false;
                out.python.missing_wheels.push(delta.clone());
                out.warnings.push(format!(
                    "自定义包 {} {}：{}",
                    wheel.name,
                    if wheel.incompatible {
                        "与源客户端环境不兼容"
                    } else {
                        "没有可用的 wheel"
                    },
                    wheel
                        .missing_reason
                        .clone()
                        .unwrap_or_else(|| "请手动提供文件".to_string())
                ));
            } else if wheel.incompatible {
                out.warnings.push(format!(
                    "自定义包 {} 与源客户端环境不兼容（{}）；按完全一致策略仍会安装",
                    wheel.name,
                    wheel
                        .compat_warning
                        .clone()
                        .unwrap_or_else(|| "环境标签不匹配".to_string())
                ));
            }
            match target_wheels.get(name) {
                None => out.python.wheels_added.push(wheel_delta(wheel)),
                Some(prev) => {
                    if !wheel_content_same(wheel, prev) {
                        let mut delta = wheel_delta(wheel);
                        delta.from = prev.version.clone();
                        out.python.wheels_changed.push(delta);
                    }
                }
            }
        }
        for (name, wheel) in &target_wheels {
            if !source_wheels.contains_key(name) {
                let mut delta = wheel_delta(wheel);
                delta.from = wheel.version.clone();
                delta.to = String::new();
                out.python.wheels_removed.push(delta);
            }
        }
    }

    // ---- 节点
    if scope.nodes {
        let source_nodes: BTreeMap<String, &SyncNodeState> =
            source.nodes.iter().map(|n| (n.name.clone(), n)).collect();
        let target_nodes: BTreeMap<String, &SyncNodeState> =
            target.nodes.iter().map(|n| (n.name.clone(), n)).collect();
        for (name, node) in &source_nodes {
            match target_nodes.get(name) {
                None => out.nodes.added.push((*node).clone()),
                Some(prev) => {
                    let content_changed = prev.worktree_hash != node.worktree_hash;
                    let commit_changed = prev.commit != node.commit;
                    let enabled_changed = prev.enabled != node.enabled;
                    let url_changed = prev.url != node.url;
                    if content_changed || commit_changed || enabled_changed || url_changed {
                        let mut reason = Vec::new();
                        if commit_changed {
                            reason.push("commit");
                        }
                        if content_changed {
                            reason.push(if node.kind == "manual" {
                                "手工"
                            } else if node.dirty {
                                "本地修改"
                            } else if node.url.is_none() {
                                "私有/无远端"
                            } else {
                                "内容"
                            });
                        }
                        if enabled_changed {
                            reason.push("启用性");
                        }
                        if url_changed {
                            reason.push("来源地址");
                        }
                        out.nodes.changed.push(NodeChange {
                            name: (*name).clone(),
                            from_commit: prev.commit.clone(),
                            to_commit: node.commit.clone(),
                            from_enabled: prev.enabled,
                            to_enabled: node.enabled,
                            content_changed,
                            reason: reason.join(" / "),
                        });
                    }
                }
            }
        }
        for (name, node) in &target_nodes {
            if !source_nodes.contains_key(name) {
                out.nodes.removed.push((*node).clone());
            }
        }
        out.nodes.target_extra = out.nodes.removed.len();
    }

    // ---- 模型
    if scope.models {
        let source_models: BTreeMap<String, &SyncModelFile> = source
            .models
            .iter()
            .map(|m| (m.relative_path.to_lowercase(), m))
            .collect();
        let target_models: BTreeMap<String, &SyncModelFile> = target
            .models
            .iter()
            .map(|m| (m.relative_path.to_lowercase(), m))
            .collect();
        for (key, model) in &source_models {
            match target_models.get(key) {
                None => {
                    out.models.added.push(ModelChange {
                        kind: model.kind.clone(),
                        relative_path: model.relative_path.clone(),
                        size: model.size,
                        from_size: 0,
                        from_hash: None,
                        to_hash: model.sha256.clone(),
                    });
                    out.models.transfer_bytes += model.size;
                }
                Some(prev) => {
                    let hash_differs = match (&prev.sha256, &model.sha256) {
                        (Some(a), Some(b)) => a != b,
                        _ => false,
                    };
                    if prev.size != model.size || hash_differs {
                        out.models.modified.push(ModelChange {
                            kind: model.kind.clone(),
                            relative_path: model.relative_path.clone(),
                            size: model.size,
                            from_size: prev.size,
                            from_hash: prev.sha256.clone(),
                            to_hash: model.sha256.clone(),
                        });
                        out.models.transfer_bytes += model.size;
                    } else {
                        out.models.unchanged += 1;
                        out.models.reusable_bytes += model.size;
                    }
                }
            }
        }
        for (key, model) in &target_models {
            if !source_models.contains_key(key) {
                out.models.removed.push(ModelChange {
                    kind: model.kind.clone(),
                    relative_path: model.relative_path.clone(),
                    size: model.size,
                    from_size: model.size,
                    from_hash: model.sha256.clone(),
                    to_hash: None,
                });
            }
        }
    }

    // 模型库是“同一物理共享目录”时，路径相同、哈希相同会自然命中 unchanged：    // 如果双方 marker id 一致，额外提示可以零拷贝。
    if scope.models && same_model_store(&source.model_store_id, &target.model_store_id) {
        out.warnings
            .push("源端与目标端模型库是同一个物理目录：模型文件无需传输，只做清单核对".to_string());
        out.models.transfer_bytes = 0;
    }

    // ---- 目标漂移
    if let Some(last) = &target.last_applied {
        out.target_last_source = Some(last.source_instance_name.clone());
        if !last.fingerprint.is_empty() {
            if last.fingerprint.starts_with(FINGERPRINT_VERSION) {
                out.target_drift = last.fingerprint != state_fingerprint_scoped(target, scope);
            } else {
                out.target_drift = true;
                out.warnings.push(
                    "目标上次同步来自旧版本启动器，指纹算法已升级；本次需要确认一次目标状态"
                        .to_string(),
                );
            }
        }
    }
    if out.target_drift {
        out.warnings
            .push("目标实例自上次同步后有本地改动：继续会覆盖这些改动，请确认或先回滚".to_string());
    }
    if scope.python
        && out.python.from_version != out.python.to_version
        && !out.python.to_version.is_empty()
    {
        out.warnings.push(format!(
            "Python 版本变化，{} → {}，目标端会新建 venv（旧环境保留备份）",
            out.python.from_version, out.python.to_version
        ));
    }
    if scope.python && !out.python.missing_wheels.is_empty() {
        out.warnings
            .push("存在缺少文件的自定义包：本次会跳过这些包继续同步，结束后统一提示".to_string());
    }

    out.needs_apply = out.core.changed
        || out.launch.changed
        || !out.python.packages_added.is_empty()
        || !out.python.packages_removed.is_empty()
        || !out.python.packages_changed.is_empty()
        || out.python.from_version != out.python.to_version
        || !out.python.wheels_added.is_empty()
        || !out.python.wheels_changed.is_empty()
        || !out.python.wheels_removed.is_empty()
        || !out.nodes.added.is_empty()
        || !out.nodes.changed.is_empty()
        || (!out.nodes.removed.is_empty())
        || !out.models.added.is_empty()
        || !out.models.modified.is_empty()
        || !out.models.removed.is_empty();
    out
}

// ---------------------------------------------------------------- 代次生成

fn remote_filename(url: &str) -> String {
    let raw = url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    // URL 里的 `+` 常被编码成 `%2B`；直接当成本地文件名会让 uv 把版本解析成
    // `0.4.5%2Bcu130`，必须在落盘/安装前还原。`decoded_file_name` 还会
    // 剥掉解码后可能出现的路径分隔符，避免 `%2F` 变成目录穿越。
    decoded_file_name(raw)
}

/// 把 PEP 610 的 VCS URL + commit 变成 uv 能识别的 `git+URL@commit`。
fn vcs_install_spec(url: &str, commit: &str) -> Result<String, AppError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(AppError::new("SYNC_PIP", "Git 自定义包缺少仓库地址"));
    }
    if url.starts_with("file://") || url.starts_with("file:") {
        return Err(AppError::new(
            "SYNC_PIP",
            "Git 自定义包位于源端本机路径，目标端无法访问；请改用可访问的仓库地址或提供 wheel",
        ));
    }
    let (base, fragment) = match url.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (url, None),
    };
    let mut spec = if base.starts_with("git+") {
        base.to_string()
    } else {
        format!("git+{base}")
    };
    if !commit.trim().is_empty() {
        spec.push('@');
        spec.push_str(commit.trim());
    }
    if let Some(fragment) = fragment {
        spec.push('#');
        spec.push_str(fragment);
    }
    Ok(spec)
}

/// uv 只接受这些扩展名的本地包。源端历史数据远程 URL 可能把文件名存成 `cstr`
/// 这种没有扩展名的形式，必须先还原成可识别的名字再交给 uv。
const ARCHIVE_EXTS: &[&str] = &[
    ".whl",
    ".tar.gz",
    ".tar.bz2",
    ".tar.lz",
    ".tar.lzma",
    ".tar.xz",
    ".tar.zst",
    ".tar",
    ".tbz",
    ".tgz",
    ".tlz",
    ".txz",
    ".zip",
];

fn supported_archive_ext(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    ARCHIVE_EXTS
        .iter()
        .copied()
        .find(|ext| lower.ends_with(ext))
}

fn zip_contains_wheel(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        if entry.name().ends_with(".dist-info/WHEEL") {
            return true;
        }
    }
    false
}

fn read_zip_text(path: &Path, suffix: &str) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        if entry.name().ends_with(suffix) {
            let mut text = String::new();
            entry.read_to_string(&mut text).ok()?;
            return Some(text);
        }
    }
    None
}

/// 当原始文件名不是合法 wheel 名时，从包内的`.dist-info/WHEEL|METADATA`
/// 还原一个uv 接受的`<name>-<version>-<tags>.whl`。
fn wheel_name_from_zip(path: &Path, fallback_name: &str, fallback_version: &str) -> Option<String> {
    let wheel = read_zip_text(path, ".dist-info/WHEEL")?;
    let metadata = read_zip_text(path, ".dist-info/METADATA").unwrap_or_default();
    let field = |text: &str, key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key).map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let name = field(&metadata, "Name:").unwrap_or_else(|| fallback_name.to_string());
    let version = field(&metadata, "Version:").unwrap_or_else(|| fallback_version.to_string());
    if name.trim().is_empty() || version.trim().is_empty() {
        return None;
    }
    let tags: Vec<String> = wheel
        .lines()
        .filter_map(|line| line.strip_prefix("Tag:").map(str::trim))
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect();
    if tags.is_empty() {
        return None;
    }
    // wheel 文件名里的发行名统一用下划线，版本保持原样（可带 +cu130 之类本地标签）。
    let dist = name
        .trim()
        .chars()
        .map(|c| {
            if c == '-' || c == '.' || c == ' ' {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    Some(format!("{dist}-{}-{}.whl", version.trim(), tags.join(".")))
}

fn sniff_archive_ext(path: &Path) -> Option<&'static str> {
    let mut file = File::open(path).ok()?;
    let mut buf = [0u8; 512];
    let n = file.read(&mut buf).ok()?;
    let bytes = &buf[..n];
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return Some(if zip_contains_wheel(path) {
            ".whl"
        } else {
            ".zip"
        });
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Some(".tar.gz");
    }
    if bytes.starts_with(b"BZh") {
        return Some(".tar.bz2");
    }
    if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        return Some(".tar.xz");
    }
    if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Some(".tar.zst");
    }
    if bytes.starts_with(&[0x5d, 0x00, 0x00]) {
        return Some(".tar.lzma");
    }
    if bytes.len() >= 262 && &bytes[257..262] == b"ustar" {
        return Some(".tar");
    }
    None
}

fn decoded_file_name(raw: &str) -> String {
    let decoded = percent_decode(raw);
    Path::new(&decoded)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 把任意本地包文件名规范成 uv 支持的扩展名；原文件名已合法时原样返回。
fn normalize_archive_filename(
    path: &Path,
    proposed: &str,
    fallback_name: &str,
    fallback_version: &str,
) -> Result<String, AppError> {
    // 旧代次/远程 URL 可能把 `+` 存成 `%2B`。先还原再判断扩展名，
    // 否则会生成 `...0.4.5%2Bcu130...whl`，uv 直接拒绝。
    let proposed = decoded_file_name(proposed);
    let fallback = {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("custom-package");
        decoded_file_name(name)
    };
    let original = if proposed.is_empty() {
        fallback
    } else {
        proposed
    };
    if supported_archive_ext(&original).is_some() {
        return Ok(original);
    }
    match sniff_archive_ext(path) {
        Some(".whl") => {
            wheel_name_from_zip(path, fallback_name, fallback_version).ok_or_else(|| {
                AppError::new(
                    "SYNC_WHEEL_FORMAT",
                    format!("文件 {original} 是 wheel，但包内缺少可解析的 WHEEL/METADATA"),
                )
            })
        }
        Some(ext) => {
            let stem = original.trim_end_matches('.').trim();
            let stem = if stem.is_empty() {
                "custom-package"
            } else {
                stem
            };
            Ok(format!("{stem}{ext}"))
        }
        None => Err(AppError::new(
            "SYNC_WHEEL_FORMAT",
            format!(
                "无法识别自定义包 {original} 的格式；只支持 wheel 或源码包（.whl/.zip/.tar.gz 等）"
            ),
        )),
    }
}

/// 目标端旧代次可能仍带无扩展名的制品；安装前落到一个扩展名正确的临时副本。
fn ensure_install_archive(
    local: &Path,
    filename: &str,
    name: &str,
    version: &str,
) -> Result<PathBuf, AppError> {
    let local_name = local
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_string();
    let proposed = if local_name.is_empty() {
        filename
    } else {
        &local_name
    };
    let normalized = normalize_archive_filename(local, proposed, name, version)?;
    if normalized == local_name {
        return Ok(local.to_path_buf());
    }
    let dest = local.with_file_name(normalized);
    fs::copy(local, &dest)?;
    Ok(dest)
}

/// 把节点工作区/核心补丁/自定义轮子落到代次目录，并把 manifest 写入磁盘。
pub fn stage_generation(
    source: &mut InstanceState,
    snapshot: Option<snapshot::Snapshot>,
    git: Option<&Path>,
) -> Result<SyncManifest, AppError> {
    stage_generation_scoped(source, snapshot, git, SyncScope::all(), None)
}

pub(crate) fn stage_generation_scoped(
    source: &mut InstanceState,
    snapshot: Option<snapshot::Snapshot>,
    git: Option<&Path>,
    scope: SyncScope,
    prep: Option<&SyncPrepare>,
) -> Result<SyncManifest, AppError> {
    let id = format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
    );
    let dir = generation_dir(&id);
    fs::create_dir_all(&dir)?;
    if let Some(context) = prep {
        context.set_generation(&id);
        context.update(
            "创建同步代次",
            38,
            "创建代次目录",
            format!("准备包目录 {}", dir.display()),
        );
        context.check()?;
    }
    let mut totals = SyncTotals::default();
    let inst_root = PathBuf::from(&source.root_dir);

    // 节点：全量打包工作区。这样私有仓库、本地修改、手工节点都能精确同步，
    // 不需要目标端访问 Git/GitHub。但 .git 仅在有本地提交时包含，避免无谓膨胀。
    if scope.nodes {
        let nodes_dir = dir.join("nodes");
        fs::create_dir_all(&nodes_dir)?;
        let custom_nodes = inst_root.join("ComfyUI").join("custom_nodes");
        let source_id = source.instance_id.clone();
        let node_total = source.nodes.len();
        for (index, node) in source.nodes.iter_mut().enumerate() {
            if let Some(context) = prep {
                context.check()?;
                context.update(
                    "打包节点",
                    40 + ((index * 24 / node_total.max(1)) as u8),
                    format!("节点 {}/{}：{}", index + 1, node_total, node.name),
                    "校验节点目录",
                );
            }
            if !safe_name(&node.name) {
                return Err(AppError::new(
                    "SYNC_NODE_NAME",
                    format!("节点名不安全，不能同步：{}", node.name),
                ));
            }
            let node_path = if node.enabled {
                custom_nodes.join(&node.name)
            } else {
                custom_nodes.join(format!("{}.disabled", node.name))
            };
            if !node_path.is_dir() {
                continue;
            }
            // 所有git 节点都带 `.git`：目标端才能读到同一个commit/remote：        // 否则下一次预览会把「目标变成manual」误判成节点变更。
            let include_git = node.kind == "git" || node.has_local_commits;
            let archive_name = format!("{}.zip", node.name);
            let dest = nodes_dir.join(&archive_name);
            let (size, hash) =
                if let Some(hit) = reuse_node_archive(&source_id, node, include_git, &dest) {
                    if let Some(context) = prep {
                        context.note(format!("复用节点缓存：{}", node.name));
                    }
                    hit
                } else {
                    let packed = zip_dir_with(
                        &node_path,
                        &dest,
                        include_git,
                        &[],
                        prep,
                        &format!("节点 {}", node.name),
                    )?;
                    save_node_archive(&source_id, node, include_git, &dest, packed.0, &packed.1);
                    packed
                };
            node.archive = Some(format!("nodes/{archive_name}"));
            node.archive_sha256 = Some(hash);
            node.archive_size = size;
            totals.files += 1;
            totals.bytes += size;
            if let Some(context) = prep {
                context.add_files(1, size);
            }
        }
    } else if let Some(context) = prep {
        context.update("打包节点", 40, "跳过自定义节点", "本次未选择节点");
    }

    // ComfyUI 本体：干净 git 不打包，让目标 clone；dirty / 没有 git 时打包。
    if scope.comfyui {
        if let Some(context) = prep {
            context.check()?;
            context.update(
                "打包 ComfyUI",
                66,
                "检查 ComfyUI 本体",
                "读取 git 状态与远端",
            );
        }
        let comfy = inst_root.join("ComfyUI");
        let core_dir = dir.join("core");
        fs::create_dir_all(&core_dir)?;
        let has_git = comfy.join(".git").exists();
        // 只有官方公开远端的干净 git 源才让目标clone/fetch；私有fork/本地远端
        // 目标可能根本访问不到，直接打完整核心包（仍排除节点模型/用户目录）。
        let public_origin = source
            .core
            .origin_url
            .as_deref()
            .map(|url| {
                let url = url.to_ascii_lowercase();
                url.contains("github.com/comfyanonymous/comfyui")
                    && !url.contains("github.com/comfyanonymous/comfyui/")
            })
            .unwrap_or(false);
        let needs_full_core = !has_git || source.core.commit.is_none() || !public_origin;
        if needs_full_core {
            let dest = core_dir.join("full.zip");
            let (size, hash) =
                zip_dir_with(&comfy, &dest, false, CORE_EXCLUDE, prep, "ComfyUI 本体")?;
            source.core.full_archive = Some("core/full.zip".to_string());
            source.core.full_archive_sha256 = Some(hash);
            totals.files += 1;
            totals.bytes += size;
            if let Some(context) = prep {
                context.add_files(1, size);
            }
        } else if source.core.dirty {
            if let Some(context) = prep {
                context.check()?;
                context.update(
                    "打包 ComfyUI",
                    68,
                    "生成本地修改补丁",
                    "执行 git diff --binary HEAD",
                );
            }
            let git = git.ok_or_else(|| {
                AppError::new(
                    "SYNC_GIT",
                    "源端 ComfyUI 有本地修改，但找不到 git 可执行文件",
                )
            })?;
            let out = proc::run(
                git,
                &["diff", "--binary", "HEAD"],
                Some(&comfy),
                &proc::git_env(),
            )
            .map_err(|e| AppError::new("SYNC_GIT", e.message))?;
            if !out.ok() {
                return Err(AppError::new(
                    "SYNC_GIT",
                    format!("生成 ComfyUI 补丁失败，{}", out.detail()),
                ));
            }
            if !out.stdout.is_empty() {
                let patch = core_dir.join("worktree.patch");
                fs::write(&patch, &out.stdout)?;
                source.core.patch = Some("core/worktree.patch".to_string());
                source.core.patch_sha256 = Some(sha256_bytes(out.stdout.as_bytes()));
                totals.files += 1;
                totals.bytes += out.stdout.len() as u64;
            }

            // 未跟踪文件单独打包；已跟踪修改在 patch 里。
            let untracked_out = proc::run(
                git,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                Some(&comfy),
                &proc::git_env(),
            )
            .map_err(|e| AppError::new("SYNC_GIT", e.message))?;
            if untracked_out.ok() {
                let list: Vec<PathBuf> = untracked_out
                    .stdout
                    .split('\0')
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
                    .filter(|rel| {
                        let text = rel.to_string_lossy().replace('\\', "/").to_lowercase();
                        !CORE_EXCLUDE
                            .iter()
                            .any(|top| text == *top || text.starts_with(&format!("{top}/")))
                    })
                    .collect();
                if !list.is_empty() {
                    let dest = core_dir.join("untracked.zip");
                    let file = File::create(&dest)?;
                    let mut zip = zip::ZipWriter::new(file);
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated);
                    for rel in list {
                        if let Some(context) = prep {
                            context.check()?;
                            context.note(format!(
                                "打包 ComfyUI 未跟踪文件：{}",
                                rel.to_string_lossy().replace('\\', "/")
                            ));
                        }
                        let path = comfy.join(&rel);
                        if path.is_file() {
                            zip.start_file(rel.to_string_lossy().replace('\\', "/"), options)
                                .map_err(|e| AppError::new("SYNC_ZIP", e.to_string()))?;
                            let mut f = File::open(&path)?;
                            std::io::copy(&mut f, &mut zip)?;
                            totals.files += 1;
                        }
                    }
                    zip.finish()
                        .map_err(|e| AppError::new("SYNC_ZIP", e.to_string()))?;
                    source.core.untracked_archive = Some("core/untracked.zip".to_string());
                    source.core.untracked_sha256 = Some(sha256_file(&dest)?);
                    totals.bytes += fs::metadata(&dest)?.len();
                }
            }
        }
    } else if let Some(context) = prep {
        context.update(
            "打包 ComfyUI",
            66,
            "跳过 ComfyUI 本体",
            "本次未选择 ComfyUI",
        );
    }

    // 自定义wheel：本地复制，远程下载，可编辑目录打包。
    if scope.python {
        let wheels_dir = dir.join("env").join("wheels");
        let projects_dir = dir.join("env").join("projects");
        fs::create_dir_all(&wheels_dir)?;
        fs::create_dir_all(&projects_dir)?;
        let wheel_total = source.python.custom_wheels.len();
        for (wheel_index, wheel) in source.python.custom_wheels.iter_mut().enumerate() {
            if let Some(context) = prep {
                context.check()?;
                let wheel_percent =
                    ((wheel_index + 1).saturating_mul(100) / wheel_total.max(1)) as u8;
                context.update(
                    "准备 Python 包",
                    80 + ((wheel_index * 14 / wheel_total.max(1)) as u8),
                    format!(
                        "Python 包 {}/{}：{} · {wheel_percent}%",
                        wheel_index + 1,
                        wheel_total,
                        wheel.name
                    ),
                    format!("来源 {}", redact_url(&wheel.source)),
                );
            }
            if wheel.source == "local-wheel" || wheel.source == "local-archive" {
                // 状态扫描已经把库里的文件定位到 source_ref；这里只确认它还在。
                let src = wheel
                    .source_ref
                    .as_deref()
                    .map(PathBuf::from)
                    .filter(|path| path.is_file());
                let Some(src) = src else {
                    wheel.available = false;
                    wheel.archive = None;
                    wheel.missing_reason = Some(
                        "wheel 库里的文件已不在（可能被移走或改名）；请放回去后重新扫描"
                            .to_string(),
                    );
                    continue;
                };
                let proposed = if wheel.filename.is_empty() {
                    src.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("custom-package.whl")
                        .to_string()
                } else {
                    wheel.filename.clone()
                };
                wheel.filename =
                    normalize_archive_filename(&src, &proposed, &wheel.name, &wheel.version)?;
                let dest = wheels_dir.join(&wheel.filename);
                copy_file_with_progress(
                    &src,
                    &dest,
                    prep,
                    &format!("复制 Python 包 {}", wheel.name),
                )?;
                wheel.sha256 = sha256_file_for_prepare(
                    &dest,
                    prep,
                    &format!("校验 Python 包 {}", wheel.name),
                )?;
                wheel.size = fs::metadata(&dest)?.len();
                wheel.archive = Some(format!("env/wheels/{}", wheel.filename));
                wheel.available = true;
                wheel.missing_reason = None;
                totals.files += 1;
                totals.bytes += wheel.size;
            } else if wheel.source == "editable" || wheel.source == "local-dir" {
                if let Some(src) = wheel.source_ref.as_deref().map(PathBuf::from) {
                    if src.is_dir() {
                        let dest = projects_dir.join(format!("{}.zip", wheel.name));
                        let (size, hash) = zip_dir_with(
                            &src,
                            &dest,
                            false,
                            &[],
                            prep,
                            &format!("Python 项目 {}", wheel.name),
                        )?;
                        wheel.filename = format!("{}.zip", wheel.name);
                        wheel.sha256 = hash;
                        wheel.size = size;
                        wheel.archive = Some(format!("env/projects/{}.zip", wheel.name));
                        wheel.available = true;
                        wheel.missing_reason = None;
                        totals.files += 1;
                        totals.bytes += size;
                    } else {
                        wheel.available = false;
                        wheel.missing_reason = Some(if wheel.source == "local-dir" {
                            "本地目录安装的源目录已不存在".to_string()
                        } else {
                            "可编辑安装的源目录已不存在".to_string()
                        });
                    }
                }
            } else if wheel.source == "vcs" {
                wheel.archive = None;
                wheel.available = wheel.vcs.as_deref() == Some("git")
                    && wheel
                        .source_ref
                        .as_deref()
                        .is_some_and(|url| !url.is_empty());
                if wheel.available {
                    let commit = wheel.commit.as_deref().unwrap_or("");
                    wheel.missing_reason = Some(if commit.is_empty() {
                        "Git 仓库安装（默认分支）".to_string()
                    } else {
                        format!("Git 仓库安装，固定commit {}", short_chars(commit, 12))
                    });
                } else {
                    wheel.missing_reason = Some("Git 仓库地址成VCS 类型不可用".to_string());
                }
            } else if wheel.source == "remote-url" {
                // 远程 URL 包不预下载：目标端直接按 URL 安装。这样同步包更小，
                // 也避免源端把一个文件名/METADATA 不匹配的轮子改造成本地文件。
                if let Some(url) = wheel.source_ref.as_deref() {
                    if wheel.filename.is_empty() {
                        wheel.filename = remote_filename(url);
                    }
                    wheel.size = 0;
                    wheel.archive = None;
                    wheel.available = url.starts_with("http://") || url.starts_with("https://");
                    wheel.missing_reason = Some(if wheel.available {
                        "远程 URL，目标端直接下载安装".to_string()
                    } else {
                        format!("无法识别的远程来源：{}", redact_url(url))
                    });
                } else {
                    wheel.available = false;
                    wheel.missing_reason = Some("远程 URL 记录为空".to_string());
                }
            }
        }
    } else if let Some(context) = prep {
        context.update(
            "准备 Python 包",
            80,
            "跳过 Python 环境",
            "本次未选择 Python",
        );
    }

    if let Some(context) = prep {
        context.check()?;
        context.update("写入同步清单", 96, "写入 manifest", "统计同步包文件与体积");
    }
    let mut manifest = SyncManifest {
        format: SYNC_FORMAT,
        generation_id: id.clone(),
        created_at: now(),
        source_instance_id: source.instance_id.clone(),
        source_instance_name: source.name.clone(),
        source: source.clone(),
        snapshot,
        totals,
        scope,
        prepared_fingerprint: String::new(),
    };
    save_manifest(&manifest)?;
    manifest.source = source.clone();
    save_manifest(&manifest)?;
    flush_hash_cache();
    Ok(manifest)
}

pub fn load_manifest(generation_id: &str) -> Result<SyncManifest, AppError> {
    if !safe_generation_id(generation_id) {
        return Err(AppError::new("SYNC_GENERATION", "代次 ID 不合法"));
    }
    let path = generation_dir(generation_id).join("manifest.json");
    let raw = fs::read_to_string(&path)
        .map_err(|e| AppError::new("SYNC_GENERATION", format!("读不到同步代次：{e}")))?;
    serde_json::from_str(&raw)
        .map_err(|e| AppError::new("SYNC_GENERATION", format!("同步代次损坏，{e}")))
}

pub fn save_manifest(manifest: &SyncManifest) -> Result<(), AppError> {
    let path = generation_dir(&manifest.generation_id).join("manifest.json");
    config::atomic_write(&path, serde_json::to_string_pretty(manifest)?.as_bytes())
}

// ---------------------------------------------------------------- 目标端应用
fn encode_path(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 清单里的相对路径必须以 `/` 分隔且不能逃出 inbox。
///
/// 反斜杠也要拒绝：Windows 上 `..\\..\\x` 与 `../../x` 等价，而 Unix 上
/// 反斜杠只是普通字符，只靠 `Path::components` 会出现平台差异。
pub(crate) fn safe_relative_path(rel: &str) -> bool {
    let path = Path::new(rel);
    !rel.trim().is_empty()
        && !rel.contains('\0')
        && !rel.contains('\\')
        && !rel.contains(':')
        && !path.is_absolute()
        && path.components().all(|c| match c {
            Component::Normal(part) => windows_component_ok(&part.to_string_lossy()),
            _ => false,
        })
}

/// 解压路径额外允许 zip 常见的 `./` 前缀，但仍拒绝 Windows 保留名、
/// 结尾点/空格、冒号 ADS 和反斜杠；`enclosed_name` 已负责拒绝 `..`/绝对路径。
fn safe_zip_entry_path(path: &Path) -> bool {
    let mut has_component = false;
    path.components().all(|c| match c {
        Component::Normal(part) => {
            has_component = true;
            windows_component_ok(&part.to_string_lossy())
        }
        Component::CurDir => true,
        _ => false,
    }) && has_component
}

/// 把清单里的相对 archive 路径安全地拼到 inbox 下。
///
/// 只做词法校验，不依赖目标文件是否存在；调用方仍应在解压/删除前确认
/// 目标位于预期子目录内（例如 `inbox/projects`）。
fn inbox_path(inbox: &Path, archive: &str) -> Result<PathBuf, AppError> {
    if !safe_relative_path(archive) {
        return Err(AppError::new(
            "SYNC_PATH",
            format!("同步清单包含不安全的相对路径：{archive}"),
        ));
    }
    Ok(inbox.join(archive))
}

/// 找到代次内产物的期望 SHA-256；节点、核心、wheel/可编辑项目都可能带产物。
fn artifact_sha(manifest: &SyncManifest, archive: &str) -> Option<String> {
    manifest
        .source
        .nodes
        .iter()
        .find(|n| n.archive.as_deref() == Some(archive))
        .and_then(|n| n.archive_sha256.as_deref())
        .map(str::to_string)
        .or_else(|| core_artifact_sha(&manifest.source.core, archive).map(str::to_string))
        .or_else(|| {
            manifest
                .source
                .python
                .custom_wheels
                .iter()
                .find(|w| w.archive.as_deref() == Some(archive))
                .map(|w| w.sha256.clone())
                .filter(|sha| !sha.is_empty())
        })
}

fn core_artifact_sha<'a>(core: &'a SyncCoreState, archive: &str) -> Option<&'a str> {
    if core.full_archive.as_deref() == Some(archive) {
        core.full_archive_sha256.as_deref()
    } else if core.patch.as_deref() == Some(archive) {
        core.patch_sha256.as_deref()
    } else if core.untracked_archive.as_deref() == Some(archive) {
        core.untracked_sha256.as_deref()
    } else {
        None
    }
}

/// 下载并校验到 `dest`。`on_bytes` 用于 UI 的实时进度；内部已节流，
/// 不会每 64KB 都回调一次。
#[derive(Clone)]
struct ArtifactJob {
    index: usize,
    url: String,
    local: PathBuf,
    sha: Option<String>,
    label: String,
    slot_start: u8,
    slot_end: u8,
}

enum ArtifactEvent {
    Progress(usize, u64, u64),
    Done(usize, Result<(), AppError>),
}

/// 单个载荷下载失败自动重试；`.part` 已保留时下一次会从断点续传。
fn download_artifact_with_retry(
    job: &ArtifactJob,
    leader: &str,
    token: &str,
    cancel: &AtomicBool,
    tx: &std::sync::mpsc::Sender<ArtifactEvent>,
) -> Result<(), AppError> {
    let mut attempt = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(AppError::new("SYNC_CANCELLED", "同步已取消，下载停止"));
        }
        let result = download_file_with_progress(
            leader,
            token,
            &job.url,
            &job.local,
            job.sha.as_deref(),
            Some(cancel),
            Some(&mut |written, total| {
                let _ = tx.send(ArtifactEvent::Progress(job.index, written, total));
            }),
        );
        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.code == "SYNC_CANCELLED" => return Err(e),
            Err(e) if attempt < 2 => {
                log::warn!(
                    "载荷 {} 第 {} 次下载失败，准备重试：{}",
                    job.label,
                    attempt + 1,
                    e.message
                );
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(400 * attempt as u64));
            }
            Err(e) => return Err(e),
        }
    }
}

/// 模型等单文件下载的重试包装；断点续传由底层 `.part` 负责。
fn download_file_with_retry(
    leader: &str,
    token: &str,
    url_path: &str,
    dest: &Path,
    expected_sha: Option<&str>,
    cancel: Option<&AtomicBool>,
    mut on_bytes: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), AppError> {
    let mut attempt = 0u32;
    // 通过稳定的桥接闭包转发回调；直接在循环里 as_deref_mut 会把内部引用
    // 借到整个重试循环，触发 E0499。
    let mut bridge = |written: u64, total: u64| {
        if let Some(cb) = on_bytes.as_deref_mut() {
            cb(written, total);
        }
    };
    loop {
        let result = download_file_with_progress(
            leader,
            token,
            url_path,
            dest,
            expected_sha,
            cancel,
            Some(&mut bridge),
        );
        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.code == "SYNC_CANCELLED" => return Err(e),
            Err(e) if attempt < 2 => {
                log::warn!(
                    "下载 {} 第 {} 次失败，准备重试：{}",
                    url_path,
                    attempt + 1,
                    e.message
                );
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(400 * attempt as u64));
            }
            Err(e) => return Err(e),
        }
    }
}

fn download_file_with_progress(
    leader: &str,
    token: &str,
    url_path: &str,
    dest: &Path,
    expected_sha: Option<&str>,
    cancel: Option<&AtomicBool>,
    mut on_bytes: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), AppError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("bin")
    ));
    let offset = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(300))
        // 请求带成员令牌，不能跟随 3xx 到别的主机（ureq 只自动清理
        // Authorization/Cookie，不会清理自定义的 x-comfyfleet-token）。
        .redirects(0)
        .build();
    let mut req = agent
        .get(&format!("{leader}{url_path}"))
        .set("x-comfyfleet-token", token);
    if offset > 0 {
        req = req.set("Range", &format!("bytes={offset}-"));
    }
    let response = match req.call() {
        Ok(r) => r,
        Err(e) => {
            return Err(AppError::new(
                "SYNC_DOWNLOAD",
                format!("下载 {} 失败，{e}", url_path),
            ))
        }
    };
    const MAX_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024 * 1024;
    let status = response.status();
    if status == 416 {
        let _ = fs::remove_file(&part);
        return Err(AppError::new("SYNC_DOWNLOAD", "续传范围无效，已丢弃分片"));
    }
    if status != 200 && status != 206 {
        return Err(AppError::new(
            "SYNC_DOWNLOAD",
            format!("下载 {url_path} 返回了意外的 HTTP 状态 {status}"),
        ));
    }
    let mut append = offset > 0 && status == 206;
    if append {
        // 服务端必须从请求的 offset 开始返回，否则会把错误内容续到 .part 尾部。
        let range = response.header("Content-Range").unwrap_or("");
        let start = range
            .strip_prefix("bytes ")
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.parse::<u64>().ok());
        if start != Some(offset) {
            let _ = fs::remove_file(&part);
            return Err(AppError::new(
                "SYNC_DOWNLOAD",
                format!("断点续传响应不匹配（期望从 {offset} 开始，实际 {range}）"),
            ));
        }
    }
    if offset > 0 && status == 200 {
        // 服务端不支持 Range，从头写，不能追加。
        append = false;
    }
    let declared = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let total = declared.saturating_add(if append { offset } else { 0 });
    if total > MAX_ARTIFACT_BYTES {
        return Err(AppError::new("SYNC_DOWNLOAD", "产物超过允许的最大体积"));
    }
    let mut file = if append {
        fs::OpenOptions::new().append(true).open(&part)?
    } else {
        File::create(&part)?
    };
    let mut written = if append { offset } else { 0 };
    let mut reader = response.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut last_report = std::time::Instant::now() - std::time::Duration::from_secs(1);
    loop {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            return Err(AppError::new(
                "SYNC_CANCELLED",
                "同步已取消，下载停止（已下载的分片保留）",
            ));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        written = written.saturating_add(n as u64);
        if written > MAX_ARTIFACT_BYTES {
            let _ = fs::remove_file(&part);
            return Err(AppError::new(
                "SYNC_DOWNLOAD",
                "产物超过允许的最大体积，已删除分片",
            ));
        }
        file.write_all(&buf[..n])?;
        if last_report.elapsed() >= std::time::Duration::from_millis(500) {
            if let Some(cb) = on_bytes.as_deref_mut() {
                cb(written, total);
            }
            last_report = std::time::Instant::now();
        }
    }
    file.sync_all()?;
    drop(file);
    if let Some(cb) = on_bytes.as_deref_mut() {
        cb(written, total);
    }
    let mut verified = None;
    if let Some(want) = expected_sha {
        let got = sha256_file(&part)?;
        if !got.eq_ignore_ascii_case(want) {
            let _ = fs::remove_file(&part);
            return Err(AppError::new(
                "SYNC_HASH",
                format!(
                    "文件校验失败，{}（期望 {want}，实际 {got}）",
                    dest.display()
                ),
            ));
        }
        verified = Some(got);
    }
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&part, dest)
        .map_err(|e| AppError::new("SYNC_DOWNLOAD", format!("落盘失败，{e}")))?;
    // 下载阶段已经算过 SHA-256；写入缓存，后续“同步后快照/检查”直接命中，
    // 不再把刚下载的大模型重新读一遍。
    if let Some(hash) = verified {
        remember_file_hash(dest, &hash);
    }
    Ok(())
}

fn uv_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let cache = config::shared_dir().join("uv-cache");
    let _ = fs::create_dir_all(&cache);
    env.insert(
        "UV_CACHE_DIR".to_string(),
        cache.to_string_lossy().to_string(),
    );
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    // 活动日志是纯文本，关掉 ANSI 颜色，避免日志里夹带转义序列。
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("CLICOLOR".to_string(), "0".to_string());
    env
}

/// 安装类命令要边跑边把 uv 的输出送到活动日志；只在结束时给一行
/// “安装失败”看不到到底卡在哪个包。
/// 目标端取消时要能立即停掉耗时的 venv/pip 安装；`proc` 已有可取消的流式
/// 执行，这里统一把进程被杀映射成同步取消，而不是普通安装失败。
fn run_uv_stream_cancellable(
    uv: &Path,
    args: &[String],
    cwd: &Path,
    on_line: &mut dyn FnMut(&str),
    cancel: &AtomicBool,
) -> Result<proc::CmdOutput, AppError> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    proc::run_stream_cancellable(uv, &refs, Some(cwd), &uv_env(), on_line, cancel).map_err(|e| {
        if e.code == "CMD_CANCELLED" {
            AppError::new("SYNC_CANCELLED", "目标端取消，已停止安装")
        } else {
            e
        }
    })
}

/// uv 会严格校验 wheel 文件名与包内 METADATA 的版本；社区改过名的轮子
/// （例如文件名去掉构建日期）会被 uv 直接拒装。pip 不做这项校验，失败后
/// 用目标 venv 里的 pip 再试一次，避免整次同步卡在一个可安装的轮子上。
fn run_pip_stream_cancellable(
    python: &Path,
    args: &[String],
    cwd: &Path,
    on_line: &mut dyn FnMut(&str),
    cancel: &AtomicBool,
) -> Result<proc::CmdOutput, AppError> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut env = uv_env();
    env.insert("PIP_PROGRESS_BAR".to_string(), "on".to_string());
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
    env.insert("PIP_NO_INPUT".to_string(), "1".to_string());
    proc::run_stream_cancellable(python, &refs, Some(cwd), &env, on_line, cancel).map_err(|e| {
        if e.code == "CMD_CANCELLED" {
            AppError::new("SYNC_CANCELLED", "目标端取消，已停止安装")
        } else {
            e
        }
    })
}

/// 构造 pip 兜底安装参数。`--force-reinstall` 保证目标上已有的同名旧版本
/// 会被源端这个轮子替换；`--no-deps` 与 uv 路径保持一致。
fn pip_wheel_install_args(spec: &str) -> Vec<String> {
    vec![
        "-m".to_string(),
        "pip".to_string(),
        "install".to_string(),
        "--no-deps".to_string(),
        "--force-reinstall".to_string(),
        "--disable-pip-version-check".to_string(),
        "--no-input".to_string(),
        spec.to_string(),
    ]
}

/// uv/pip 输出的最后一行通常才是真正的失败原因，前面是 Resolved/Prepared 之类的摘要。
fn last_detail_line(detail: &str) -> &str {
    detail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("安装失败")
}

/// 把 uv 的安装输出节流成活动日志。uv 的进度条会用 `\r` 高频刷新，
/// 这里限制到约 3 行/秒，并截断超长行，避免把任务日志写成几百 KB。
fn report_install_line(
    on_progress: &mut dyn FnMut(u8, &str),
    pct: u8,
    prefix: &str,
    line: &str,
    last: &mut std::time::Instant,
) {
    if last.elapsed() < std::time::Duration::from_millis(300) {
        return;
    }
    let mut clean = String::new();
    for c in line.trim().chars() {
        if c.is_control() {
            continue;
        }
        clean.push(c);
    }
    if clean.is_empty() {
        return;
    }
    if clean.chars().count() > 220 {
        clean = clean.chars().take(220).collect::<String>() + "…";
    }
    on_progress(pct, &format!("{prefix}{clean}"));
    *last = std::time::Instant::now();
}

fn python_exe(venv: &Path) -> PathBuf {
    venv.join("Scripts").join("python.exe")
}

fn git_sha(value: &str) -> bool {
    (4..=40).contains(&value.len()) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// 同步清单来自网络，不能把里面的字段直接当 git 选项：`--upload-pack=...`
/// 这类值会让 git 执行本机命令。仓库地址只放行非选项、无控制字符的字符串。
fn safe_git_url(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.starts_with('-') && !value.chars().any(char::is_control)
}

/// 日志、活动记录和界面里展示 URL 时，隐去 userinfo 和常见凭据参数。
///
/// 私有仓库常用 `https://user:token@host/...`，这些字符串会被写进活动日志、
/// 同步清单并显示在控制台；原样保存等于把访问令牌留在磁盘上。这里只做展示
/// 用脱敏，真正执行 git/下载的调用仍使用原始 URL。
pub fn redact_url(raw: &str) -> String {
    let text = raw.trim();
    let Some((scheme, rest)) = text.split_once("://") else {
        // git@host:path 这种 scp 形式没有密码字段，保持原样。
        return text.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let mut out = String::with_capacity(text.len());
    out.push_str(scheme);
    out.push_str("://");
    if let Some((_, host)) = authority.rsplit_once('@') {
        out.push_str("***@");
        out.push_str(host);
    } else {
        out.push_str(authority);
    }
    let tail = &rest[authority_end..];
    // 某些镜像把令牌放在查询参数里（?token=...&signature=...）。
    let (before_frag, fragment) = tail
        .split_once('#')
        .map(|(a, b)| (a, Some(b)))
        .unwrap_or((tail, None));
    if let Some((path, query)) = before_frag.split_once('?') {
        out.push_str(path);
        out.push('?');
        let mut first = true;
        for pair in query.split('&') {
            if !first {
                out.push('&');
            }
            first = false;
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let lower = key.to_ascii_lowercase();
            if lower.contains("token")
                || lower.contains("secret")
                || lower.contains("password")
                || lower.contains("auth")
                || lower.contains("signature")
                || lower.contains("credential")
                || lower.contains("sig")
            {
                out.push_str(key);
                out.push_str("=***");
            } else {
                out.push_str(key);
                if !value.is_empty() {
                    out.push('=');
                    out.push_str(value);
                }
            }
        }
    } else {
        out.push_str(before_frag);
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn checkout_commit(git: &Path, dir: &Path, commit: &str) -> Result<(), String> {
    if !git_sha(commit) {
        return Err("commit 不是合法的 git SHA".to_string());
    }
    let run = |args: &[&str]| -> Result<proc::CmdOutput, AppError> {
        proc::run(git, args, Some(dir), &proc::git_env())
    };
    let mut out = run(&["checkout", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        return Ok(());
    }
    let _ = run(&["fetch", "origin", "--tags", "--quiet"]);
    out = run(&["checkout", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        return Ok(());
    }
    let _ = run(&["fetch", "origin", commit, "--depth", "1", "--quiet"]);
    out = run(&["checkout", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        Ok(())
    } else {
        Err(out.detail())
    }
}

/// 完整核心包同步时，源端没有的文件也要从目标核心目录清掉，否则目标会残留
/// 旧版本模块。custom_nodes/models/input/output 等用户目录由 CORE_EXCLUDE 保留。
fn clear_core_except(comfy: &Path) -> Result<(), AppError> {
    if !comfy.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(comfy)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if CORE_EXCLUDE.iter().any(|keep| name == *keep) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn dir_non_empty(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// 重建 venv 时的回滚点：旧 venv 同盘临时改名，成功后按设置保留几天
/// （默认 7 天）供手动回退，到期后在下一次同步时清理，0 天则立即删除。
/// 新环境创建装包失败时 Drop 会删掉半成品并把旧 venv 改回来。
struct VenvSwap {
    venv: PathBuf,
    old: Option<PathBuf>,
    keep_days: u32,
    committed: bool,
}

impl VenvSwap {
    fn new(venv: PathBuf, old: Option<PathBuf>, keep_days: u32) -> Self {
        Self {
            venv,
            old,
            keep_days,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
        let Some(old) = self.old.take() else {
            return;
        };
        if !old.exists() {
            return;
        }
        if self.keep_days == 0 {
            if let Err(e) = fs::remove_dir_all(&old) {
                log::warn!("清理旧 venv 临时目录失败，{}：{e}", old.display());
            }
            return;
        }
        let dest = self.venv.with_file_name(format!(
            ".venv-comfyfleet-old-{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        match fs::rename(&old, &dest) {
            Ok(()) => {
                log::info!("旧 venv 已保留 {} 天：{}", self.keep_days, dest.display());
                prune_old_venvs(&self.venv, self.keep_days);
            }
            Err(e) => log::warn!(
                "保留旧 venv 失败，{} -> {}：{e}",
                old.display(),
                dest.display()
            ),
        }
    }
}

/// 清理实例目录下过期的 venv 回滚目录。名字前缀限定得很死，不会碰用户自己的目录。
fn prune_old_venvs(venv: &Path, keep_days: u32) {
    let Some(parent) = venv.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let max_age = std::time::Duration::from_secs(u64::from(keep_days) * 86_400);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(".venv-comfyfleet-old-") && !name.starts_with(".venv-comfyfleet-swap-")
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        // 旧环境本身的 mtime 可能比保留期还老，不能拿它判断“保留了几天”。        // 优先按改名时写进目录名的时间戳算；临时swap 目录没有时间戳，退回mtime。
        let age = name
            .strip_prefix(".venv-comfyfleet-old-")
            .and_then(|rest| rest.get(0..15))
            .and_then(|stamp| chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S").ok())
            .and_then(|stamp| stamp.and_local_timezone(chrono::Local).single())
            .map(|stamp| {
                chrono::Local::now()
                    .signed_duration_since(stamp)
                    .to_std()
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| {
                meta.modified()
                    .ok()
                    .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
                    .unwrap_or_default()
            });
        if age <= max_age {
            continue;
        }
        if let Err(e) = fs::remove_dir_all(entry.path()) {
            log::warn!("清理过期旧 venv 失败，{}：{e}", entry.path().display());
        }
    }
}

impl Drop for VenvSwap {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.venv.exists() {
            if let Err(e) = fs::remove_dir_all(&self.venv) {
                log::warn!("回滚 venv 时删除半成品失败，{}：{e}", self.venv.display());
            }
        }
        if let Some(old) = self.old.take() {
            if old.exists() {
                if let Err(e) = fs::rename(&old, &self.venv) {
                    log::warn!("回滚 venv 时恢复旧环境失败，{}：{e}", old.display());
                }
            }
        }
    }
}

/// 同步失败时的核心兜底：只要 apply_core 已经生成了备份 zip，drop 就把
/// ComfyUI 代码层恢复回去。custom_nodes/models 不在备份里，不会被误删。
/// 同步失败时把实例从 Updating 恢复为 Stopped；成功路径已经自己写成 Stopped。
struct SyncApplyingGuard {
    id: String,
}

impl SyncApplyingGuard {
    fn new(id: String) -> Self {
        Self { id }
    }
}

impl Drop for SyncApplyingGuard {
    fn drop(&mut self) {
        let Ok(mut cfg) = load_instance(&self.id) else {
            return;
        };
        if matches!(cfg.state, RunState::Updating | RunState::Installing) {
            cfg.state = RunState::Stopped;
            if let Err(e) = save_instance(&cfg) {
                log::warn!("同步失败后恢复实例状态失败：{}", e.message);
            }
        }
    }
}

struct CoreRollback {
    comfy: PathBuf,
    backup_dir: PathBuf,
    armed: bool,
}

impl CoreRollback {
    fn new(inst: &InstanceConfig, job: &SyncJob, armed: bool) -> Self {
        Self {
            comfy: Path::new(&inst.root_dir).join("ComfyUI"),
            backup_dir: backups_dir().join(&job.id),
            armed,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CoreRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for name in [
            "ComfyUI-core-before.zip",
            "ComfyUI-core-before-git-init.zip",
        ] {
            let backup = self.backup_dir.join(name);
            if backup.is_file() {
                match extract_zip(&backup, &self.comfy) {
                    Ok(()) => log::warn!("同步失败，已生{} 回滚 ComfyUI 核心", backup.display()),
                    Err(e) => log::error!(
                        "同步失败且核心回滚失败：{}（备份{}：",
                        e.message,
                        backup.display()
                    ),
                }
                break;
            }
        }
    }
}

fn apply_core(
    job: &SyncJob,
    manifest: &SyncManifest,
    inst: &InstanceConfig,
    git: &Path,
    inbox: &Path,
) -> Result<(), AppError> {
    let comfy = Path::new(&inst.root_dir).join("ComfyUI");
    let core = &manifest.source.core;
    if let Some(archive) = &core.full_archive {
        let local = inbox_path(inbox, archive)?;
        if !local.is_file() {
            return Err(AppError::new("SYNC_CORE", "完整 ComfyUI 包没有下载成功"));
        }
        // 完整包只覆盖代码/配置，不动 custom_nodes/models/input/output；
        // 先打一份核心目录备份，解压失败还能人工恢复。
        if comfy.is_dir() {
            let backup_zip = backups_dir().join(&job.id).join("ComfyUI-core-before.zip");
            if let Some(parent) = backup_zip.parent() {
                fs::create_dir_all(parent)?;
            }
            zip_dir_ex(&comfy, &backup_zip, false, CORE_EXCLUDE)?;
        }
        clear_core_except(&comfy)?;
        extract_zip(&local, &comfy)?;
        if !comfy.join("main.py").is_file() {
            return Err(AppError::new(
                "SYNC_CORE",
                "完整 ComfyUI 包解压后缺少 main.py，拒绝把目标标记为同步成功",
            ));
        }
        return Ok(());
    }
    let Some(commit) = core.commit.as_deref().filter(|c| !c.is_empty()) else {
        return Err(AppError::new(
            "SYNC_CORE",
            "源端没有记录 ComfyUI commit，无法按版本同步",
        ));
    };
    if !git_sha(commit) {
        return Err(AppError::new(
            "SYNC_CORE",
            "源端 ComfyUI commit 格式不合法，拒绝执行 git 操作",
        ));
    }
    // 指commit 同步同样要留一份核心代码备份；失败时CoreRollback 才能恢复。
    if comfy.is_dir() {
        let backup_zip = backups_dir().join(&job.id).join("ComfyUI-core-before.zip");
        if !backup_zip.is_file() {
            if let Some(parent) = backup_zip.parent() {
                fs::create_dir_all(parent)?;
            }
            zip_dir_ex(&comfy, &backup_zip, false, CORE_EXCLUDE)?;
        }
    }
    if !comfy.join(".git").exists() {
        let origin = core.origin_url.as_deref().ok_or_else(|| {
            AppError::new(
                "SYNC_CORE",
                "目标没有 ComfyUI 仓库，源端也没有记录 origin URL",
            )
        })?;
        if !safe_git_url(origin) {
            return Err(AppError::new(
                "SYNC_CORE",
                "源端 origin URL 不合法，拒绝传给 git",
            ));
        }
        if let Some(parent) = comfy.parent() {
            fs::create_dir_all(parent)?;
        }
        if comfy.exists() && dir_non_empty(&comfy) {
            // 启动器早期版本手工安装可能没有 .git。先备份核心目录，再就地
            // init + fetch + reset --hard；这样 custom_nodes、models 不会被清掉。
            let backup_zip = backups_dir()
                .join(&job.id)
                .join("ComfyUI-core-before-git-init.zip");
            if let Some(parent) = backup_zip.parent() {
                fs::create_dir_all(parent)?;
            }
            zip_dir_ex(&comfy, &backup_zip, false, CORE_EXCLUDE)?;
            let run_git = |args: &[&str]| -> Result<(), AppError> {
                let out = proc::run(git, args, Some(&comfy), &proc::git_env())
                    .map_err(|e| AppError::new("SYNC_CORE", e.message))?;
                if out.ok() {
                    Ok(())
                } else {
                    Err(AppError::new(
                        "SYNC_CORE",
                        format!("git {} 失败，{}", args.join(" "), out.detail()),
                    ))
                }
            };
            run_git(&[
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "protocol.file.allow=never",
                "init",
                "--quiet",
            ])?;
            run_git(&[
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "protocol.file.allow=never",
                "remote",
                "add",
                "--",
                "origin",
                origin,
            ])?;
            run_git(&[
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "protocol.file.allow=never",
                "fetch",
                "origin",
                "--tags",
                "--quiet",
            ])?;
            run_git(&["reset", "--hard", commit])?;
        } else {
            if comfy.exists() {
                let _ = fs::remove_dir(&comfy);
            }
            let out = proc::run(
                git,
                &[
                    "-c",
                    "protocol.ext.allow=never",
                    "-c",
                    "protocol.file.allow=never",
                    "clone",
                    "--quiet",
                    "--",
                    origin,
                    &comfy.to_string_lossy(),
                ],
                None,
                &proc::git_env(),
            )
            .map_err(|e| AppError::new("SYNC_CORE", e.message))?;
            if !out.ok() {
                return Err(AppError::new(
                    "SYNC_CORE",
                    format!("clone ComfyUI 失败，{}", out.detail()),
                ));
            }
        }
    }
    let dirty = proc::run(
        git,
        &["status", "--porcelain", "--untracked-files=no"],
        Some(&comfy),
        &proc::git_env(),
    )
    .ok()
    .filter(|o| o.ok())
    .map(|o| !o.stdout.trim().is_empty())
    .unwrap_or(false);
    if dirty {
        return Err(AppError::new(
            "SYNC_CORE",
            "目标 ComfyUI 工作区有未提交改动：请先提交/清理后再同步，避免覆盖",
        ));
    }
    checkout_commit(git, &comfy, commit)
        .map_err(|e| AppError::new("SYNC_CORE", format!("切换 ComfyUI 提交失败，{e}")))?;
    // commit 是同步代次的第一身份；checkout 后必须再确认 HEAD，不能只信退出码。
    let head = proc::run(git, &["rev-parse", "HEAD"], Some(&comfy), &proc::git_env())
        .map_err(|e| AppError::new("SYNC_CORE", e.message))?;
    if !head.ok() || !head.stdout.trim().eq_ignore_ascii_case(commit) {
        return Err(AppError::new(
            "SYNC_CORE",
            format!(
                "目标 ComfyUI HEAD 与源 commit 不一致：期望 {commit}，实际{}",
                head.stdout.trim()
            ),
        ));
    }
    if let Some(patch) = &core.patch {
        let local = inbox_path(inbox, patch)?;
        let out = proc::run(
            git,
            &["apply", "--binary", &local.to_string_lossy()],
            Some(&comfy),
            &proc::git_env(),
        )
        .map_err(|e| AppError::new("SYNC_CORE", e.message))?;
        if !out.ok() {
            return Err(AppError::new(
                "SYNC_CORE",
                format!("应用源端 ComfyUI 补丁失败，{}", out.detail()),
            ));
        }
    }
    if let Some(archive) = &core.untracked_archive {
        let local = inbox_path(inbox, archive)?;
        extract_zip(&local, &comfy)?;
    }
    Ok(())
}

fn backup_path(job: &SyncJob, name: &str) -> PathBuf {
    backups_dir().join(&job.id).join("nodes").join(name)
}

/// 把目录文件移到备份位置。Windows 上备份目录可能和实例不在同一个盘，
/// `fs::rename` 会直接报 os error 17（跨驱动器），所以必须退化成
/// “复制完整内容成功后再删除源”，不能只对文件 `fs::copy`。
fn move_to_backup(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if to.exists() {
        if to.is_dir() {
            fs::remove_dir_all(to)?;
        } else {
            fs::remove_file(to)?;
        }
    }
    let copied = if from.is_dir() {
        copy_dir_recursive(from, to)
    } else {
        fs::copy(from, to).map(|_| ())
    };
    if let Err(e) = copied {
        let _ = if to.is_dir() {
            fs::remove_dir_all(to)
        } else {
            fs::remove_file(to)
        };
        return Err(e);
    }
    if from.is_dir() {
        fs::remove_dir_all(from)
    } else {
        fs::remove_file(from)
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.metadata()?.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn apply_nodes(
    job: &SyncJob,
    manifest: &SyncManifest,
    inst: &InstanceConfig,
    options: &SyncOptions,
    inbox: &Path,
) -> Result<(), AppError> {
    let comfy = Path::new(&inst.root_dir).join("ComfyUI");
    let custom = comfy.join("custom_nodes");
    fs::create_dir_all(&custom)?;
    let target_now = node_state(inst, None);
    let source_names: BTreeSet<String> = manifest
        .source
        .nodes
        .iter()
        .map(|n| n.name.clone())
        .collect();
    let mut needed: BTreeSet<String> = job
        .diff
        .nodes
        .added
        .iter()
        .map(|n| n.name.clone())
        .collect();
    needed.extend(job.diff.nodes.changed.iter().map(|n| n.name.clone()));

    if options.remove_extra_nodes {
        for stale in &target_now {
            if source_names.contains(&stale.name) {
                continue;
            }
            let from = if stale.enabled {
                custom.join(&stale.name)
            } else {
                custom.join(format!("{}.disabled", stale.name))
            };
            if from.exists() {
                let to = backup_path(job, &format!("{}.removed", stale.name));
                let _ = fs::create_dir_all(to.parent().unwrap_or(Path::new(".")));
                move_to_backup(&from, &to).map_err(|e| {
                    AppError::new("SYNC_NODES", format!("移除节点 {} 失败，{e}", stale.name))
                })?;
            }
        }
    }

    for node in &manifest.source.nodes {
        if !needed.contains(&node.name) {
            continue;
        }
        if !safe_name(&node.name) {
            return Err(AppError::new("SYNC_NODES", "节点名不安全"));
        }
        let Some(archive) = &node.archive else {
            continue;
        };
        let local = inbox_path(inbox, archive)?;
        if !local.is_file() {
            return Err(AppError::new(
                "SYNC_NODES",
                format!("节点 {} 的压缩包没有下载成功", node.name),
            ));
        }
        let dest = if node.enabled {
            custom.join(&node.name)
        } else {
            custom.join(format!("{}.disabled", node.name))
        };
        // 启用态切换时同名目录的另一份（foo 一foo.disabled）必须移走，
        // 否则目标会同时存在两份，ComfyUI 可能加载错版本。
        let counterpart = if node.enabled {
            custom.join(format!("{}.disabled", node.name))
        } else {
            custom.join(&node.name)
        };
        if counterpart.exists() {
            let backup = backup_path(job, &format!("{}.state", node.name));
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            if backup.exists() {
                let _ = fs::remove_dir_all(&backup);
            }
            move_to_backup(&counterpart, &backup).map_err(|e| {
                AppError::new(
                    "SYNC_NODES",
                    format!("切换节点 {} 启用态失败：{e}", node.name),
                )
            })?;
        }
        let temp = custom.join(format!(
            ".comfyfleet-sync-{}-{}",
            job.id.chars().take(8).collect::<String>(),
            node.name
        ));
        if temp.exists() {
            let _ = fs::remove_dir_all(&temp);
        }
        extract_zip(&local, &temp)?;
        if dest.exists() {
            let backup = backup_path(job, &node.name);
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            if backup.exists() {
                let _ = fs::remove_dir_all(&backup);
            }
            move_to_backup(&dest, &backup).map_err(|e| {
                AppError::new(
                    "SYNC_NODES",
                    format!("备份旧节点{} 失败（可能被占用）：{e}", node.name),
                )
            })?;
        }
        fs::rename(&temp, &dest).map_err(|e| {
            AppError::new("SYNC_NODES", format!("安装节点 {} 失败，{e}", node.name))
        })?;
    }
    Ok(())
}

/// 把同步清单里的 `kind/inner` 映射到某个模型目录里的真实文件路径。
/// `type_name` 非空的目录本身已经是类型目录，不能再拼一遍 `kind`。
fn model_path_in(dir: &crate::storage::DirEntry, relative: &str) -> Option<PathBuf> {
    if !dir.exists {
        return None;
    }
    if let Some(kind) = dir
        .type_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let rest = relative.strip_prefix(kind)?.strip_prefix('/')?;
        Some(Path::new(&dir.path).join(rest))
    } else {
        Some(Path::new(&dir.path).join(relative))
    }
}

fn apply_models(
    job: &SyncJob,
    diff: &SyncDiff,
    manifest: &SyncManifest,
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    options: &SyncOptions,
    leader: &str,
    token: &str,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<usize, AppError> {
    const MODEL_PCT_START: u64 = 84;
    const MODEL_PCT_SPAN: u64 = 5;
    let transferring = !diff.models.added.is_empty() || !diff.models.modified.is_empty();
    let removing = options.remove_extra_models && !diff.models.removed.is_empty();
    if transferring || removing {
        let store = model_store(inst, settings);
        fs::create_dir_all(&store)?;
        let plan = crate::storage::plan(inst, settings);
        let source_store = &manifest.source.model_store_id;
        let target_store = store_id(&store);
        let same_store = same_model_store(source_store, &target_store);
        let by_path: BTreeMap<String, &SyncModelFile> = manifest
            .source
            .models
            .iter()
            .map(|m| (m.relative_path.to_lowercase(), m))
            .collect();
        // marker id 相同只说明“可能”是同一个物理库；目录被整份复制过时 marker 也会
        // 一起被复制，不能据此跳过传输。只有目标文件确实存在且大小一致才复用。
        let added_keys: BTreeSet<String> = diff
            .models
            .added
            .iter()
            .map(|m| m.relative_path.to_lowercase())
            .collect();
        let changes: Vec<&ModelChange> = diff
            .models
            .added
            .iter()
            .chain(diff.models.modified.iter())
            .collect();
        let total_bytes = changes
            .iter()
            .filter_map(|c| by_path.get(&c.relative_path.to_lowercase()))
            .map(|m| m.size)
            .sum::<u64>()
            .max(diff.models.transfer_bytes);
        let change_count = changes.len();
        let mut done = 0usize;
        let mut done_bytes = 0u64;
        // 第一个模型真正rename 到目标库之后，取消不再有效；否则多个模型中只传了
        // 前几个就中断，会留下半套模型。模型下载阶段仍可安全取消（只会留下 .part）。
        let mut committed = false;
        for (index, change) in changes.into_iter().enumerate() {
            if !safe_relative_path(&change.relative_path) {
                return Err(AppError::new(
                    "SYNC_MODEL_PATH",
                    format!("模型相对路径不安全，拒绝写入，{}", change.relative_path),
                ));
            }
            if !committed && cancel.load(Ordering::Relaxed) {
                return Err(AppError::new("SYNC_CANCELLED", "同步已取消，模型传输停止"));
            }
            let Some(model) = by_path.get(&change.relative_path.to_lowercase()) else {
                continue;
            };
            if !safe_relative_path(&model.relative_path) {
                return Err(AppError::new("SYNC_MODEL_PATH", "模型路径越界，已拒绝"));
            }
            let relative = Path::new(&model.relative_path);
            // 目标同一个模型可能位于实例自己的 models、附加目录或共享库；更新时优先
            // 覆盖它实际所在的那一份，否则会写进共享库、旧文件却仍被扫描到。
            let existing = plan
                .models
                .iter()
                .filter_map(|dir| model_path_in(dir, &model.relative_path))
                .find(|path| path.is_file());
            let dest = existing.unwrap_or_else(|| store.join(relative));
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if same_store
                && added_keys.contains(&change.relative_path.to_lowercase())
                && fs::metadata(&dest)
                    .map(|m| m.len() == model.size)
                    .unwrap_or(false)
            {
                continue;
            }
            // 先把旧模型备份好，再直接下载到模型库的 .part。这样成功时只需
            // 一次 rename，不再经过 inbox 多复制一遍整个模型。
            if dest.is_file() {
                let backup = backups_dir().join(&job.id).join("models").join(relative);
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)?;
                }
                on_progress(
                    MODEL_PCT_START as u8,
                    &format!(
                        "备份旧模型 {}/{}：{}",
                        index + 1,
                        change_count,
                        model.relative_path
                    ),
                );
                fs::copy(&dest, &backup).map_err(|e| {
                    AppError::new(
                        "SYNC_MODEL_BACKUP",
                        format!("备份目标模型 {} 失败，{e}", dest.display()),
                    )
                })?;
            }
            let url = format!(
                "/api/v1/model/{}/{}",
                encode_path(&manifest.generation_id),
                encode_path(&model.file_id)
            );
            let base_bytes = done_bytes;
            let label = model.relative_path.clone();
            let mut speed_at = std::time::Instant::now();
            let mut speed_bytes = 0u64;
            let mut speed_text = String::new();
            let mut speed_first = true;
            download_file_with_retry(
                leader,
                token,
                &url,
                &dest,
                model.sha256.as_deref(),
                if committed { None } else { Some(cancel) },
                Some(&mut |written, file_total| {
                    let overall = base_bytes.saturating_add(written);
                    let pct = if total_bytes > 0 {
                        (MODEL_PCT_START + overall.min(total_bytes) * MODEL_PCT_SPAN / total_bytes)
                            as u8
                    } else {
                        MODEL_PCT_START as u8
                    };
                    let file_pct = if file_total > 0 {
                        (written.saturating_mul(100) / file_total).min(100)
                    } else {
                        0
                    };
                    let size = if file_total > 0 {
                        format!("{}/{}", human_bytes(written), human_bytes(file_total))
                    } else {
                        human_bytes(written)
                    };
                    // 续传时第一次回调的 written 含已存在的分片，不能拿它当本轮速度。
                    if speed_first {
                        speed_first = false;
                        speed_at = std::time::Instant::now();
                        speed_bytes = written;
                    } else {
                        let elapsed = speed_at.elapsed().as_secs_f64();
                        if elapsed >= 0.4 {
                            let rate = written.saturating_sub(speed_bytes) as f64 / elapsed;
                            speed_text = format!(" · {}", human_rate(rate));
                            speed_at = std::time::Instant::now();
                            speed_bytes = written;
                        }
                    }
                    on_progress(
                        pct,
                        &format!(
                            "下载模型 {}/{}：{}：{file_pct}% · {size}{speed_text}",
                            index + 1,
                            change_count,
                            label
                        ),
                    );
                }),
            )?;
            let actual_size = fs::metadata(&dest).map(|m| m.len()).unwrap_or(model.size);
            done_bytes = done_bytes.saturating_add(actual_size);
            let pct = if total_bytes > 0 {
                (MODEL_PCT_START + done_bytes.min(total_bytes) * MODEL_PCT_SPAN / total_bytes) as u8
            } else {
                MODEL_PCT_START as u8
            };
            on_progress(
                pct,
                &format!(
                    "模型 {}/{} 完成：{}（{}）",
                    index + 1,
                    change_count,
                    label,
                    human_bytes(actual_size)
                ),
            );
            done += 1;
            committed = true;
        }
        if options.remove_extra_models {
            for removed in &diff.models.removed {
                let relative = Path::new(&removed.relative_path);
                if relative.is_absolute()
                    || relative
                        .components()
                        .any(|c| matches!(c, Component::ParentDir))
                {
                    continue;
                }
                // 同一相对路径可能出现在多个生效模型目录里（实例自带的 models、
                // 附加目录、共享库）。清理必须删掉实际存在的那一份，而不是只删默认库。
                for target in plan
                    .models
                    .iter()
                    .filter_map(|dir| model_path_in(dir, &removed.relative_path))
                    .filter(|path| path.is_file())
                {
                    let backup = backups_dir().join(&job.id).join("models").join(relative);
                    if let Some(parent) = backup.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    move_to_backup(&target, &backup).map_err(|e| {
                        AppError::new(
                            "SYNC_MODELS",
                            format!("删除目标多余模型时备份失败：{}：{e}：", target.display()),
                        )
                    })?;
                }
            }
        }
        flush_hash_cache();
        return Ok(done);
    }
    Ok(0)
}

fn current_venv(inst: &InstanceConfig) -> PathBuf {
    Path::new(&inst.root_dir).join(".venv")
}

/// 目标端应用一个完整任务。调用方负责提供当前 leader URL 和token。
#[allow(clippy::too_many_arguments)]
pub fn apply_job(
    app: &tauri::AppHandle,
    state: &crate::state::AppState,
    job: &SyncJob,
    manifest: &SyncManifest,
    leader: &str,
    token: &str,
    cancel: &AtomicBool,
    source_cancel: &AtomicBool,
    pre_write_cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<SyncReport, AppError> {
    // 同步应用会长时间改写核心/venv/节点/模型；必须和启动、安装、切版本、删除
    // 使用同一把每实例操作锁。否则同步中途启动/删除会留下半应用目录或幽灵实例。
    let _op = crate::state::OpGuard::acquire(&job.target_instance_id, "同步应用")?;
    let scope = job.scope;
    // 目标端取消可以回到“等待再次接受”，因此在模块边界允许打断；源端取消是
    // 终态，仍遵守原来的安全点：一旦开始落盘就让本次应用收尾，避免留下半同步。
    let check_cancel = |allow_source: bool| -> Result<(), AppError> {
        if cancel.load(Ordering::Relaxed) {
            Err(AppError::new("SYNC_CANCELLED", "目标端取消，等待再次接受"))
        } else if allow_source && source_cancel.load(Ordering::Relaxed) {
            Err(AppError::new("SYNC_CANCELLED", "任务已被源端取消"))
        } else {
            Ok(())
        }
    };
    // 一旦开始落盘（模型重命名、Python/本体/节点应用），源端取消就不能再中途
    // 打断；目标端取消仍可在下一个模块边界停止并重新接受。
    let mut cancel_allowed = true;
    let job_started = std::time::Instant::now();
    check_cancel(cancel_allowed)?;
    let mut inst = load_instance(&job.target_instance_id)?;
    if inst.is_remote() {
        return Err(AppError::new(
            "SYNC_REMOTE",
            "云端实例不能作为局域网同步目标",
        ));
    }
    let settings = state.settings.lock().clone();
    let root = PathBuf::from(&inst.root_dir);
    if root.to_string_lossy().is_empty() {
        return Err(AppError::new("SYNC_ROOT", "目标实例目录为空"));
    }
    let inbox = inbox_dir().join(&job.id);
    fs::create_dir_all(&inbox)?;
    let mut rollback = CoreRollback::new(&inst, job, job.diff.core.changed);
    on_progress(2, "停止目标实例");

    // 目标端有本地漂移且预览已经提示；这里要求用户确认述allow_dirty_target：    // 否则拒绝覆盖（避免把 B 影A 的垃圾桶）。
    let before = capture_instance_state_scoped(&inst, &settings, None, scope, None)?;
    let current_fingerprint = state_fingerprint_scoped(&before, scope);
    if target_drift_requires_confirmation(
        &job.target_fingerprint,
        &current_fingerprint,
        before.last_applied.as_ref(),
        job.options.allow_dirty_target,
    ) {
        return Err(AppError::new(
            "SYNC_TARGET_DRIFT",
            "目标实例自上次同步后有本地改动；请重新预览并勾选「允许覆盖目标本地改动。",
        ));
    }
    let needed = required_space(&job.diff, &manifest.totals);
    if before.disk_free > 0 && needed > before.disk_free {
        return Err(AppError::new(
            "SYNC_DISK",
            format!(
                "目标磁盘空间不足：预计需要{}，目标只前{}",
                human_bytes(needed),
                human_bytes(before.disk_free)
            ),
        ));
    }

    crate::commands::run::stop_instance_unlocked(state, &job.target_instance_id)?;
    // 同步期间标记为 Updating：启动/安装/删除都会拒绝；失败路径由 guard 恢复 Stopped。
    inst.state = RunState::Updating;
    save_instance(&inst)?;
    let _state_guard = SyncApplyingGuard::new(job.target_instance_id.clone());
    let git_info = crate::git::resolve(app, &settings)?;
    let git = PathBuf::from(&git_info.path);

    on_progress(4, "拍同步前快照");
    let pre = snapshot::capture_and_save(
        &inst,
        &settings,
        "pre-sync",
        Some(format!("同步前：{}", job.source_instance_name)),
    )
    .map_err(|e| AppError::new("SYNC_SNAPSHOT", format!("同步前快照失败：{e}")))?;
    on_progress(7, "预下载同步载荷（暂不写入环境）");
    let mut artifacts: Vec<String> = Vec::new();
    if scope.comfyui && job.diff.core.changed {
        if let Some(archive) = &manifest.source.core.full_archive {
            artifacts.push(archive.clone());
        }
        if let Some(archive) = &manifest.source.core.patch {
            artifacts.push(archive.clone());
        }
        if let Some(archive) = &manifest.source.core.untracked_archive {
            artifacts.push(archive.clone());
        }
    }
    let mut node_needed: BTreeSet<String> = job
        .diff
        .nodes
        .added
        .iter()
        .map(|n| n.name.clone())
        .collect();
    node_needed.extend(job.diff.nodes.changed.iter().map(|n| n.name.clone()));
    if scope.nodes {
        for node in &manifest.source.nodes {
            if !node_needed.contains(&node.name) {
                continue;
            }
            if let Some(archive) = &node.archive {
                artifacts.push(archive.clone());
            }
        }
    }
    if scope.python {
        for wheel in &manifest.source.python.custom_wheels {
            if let Some(archive) = &wheel.archive {
                artifacts.push(archive.clone());
            }
        }
    }
    let total_artifacts = artifacts.len().max(1);
    let artifacts_started = std::time::Instant::now();
    on_progress(
        7,
        &format!("载荷文件 {} 个，开始下载复用本地缓存", artifacts.len()),
    );
    if artifacts.is_empty() {
        on_progress(7, "没有需要下载的载荷文件");
    }
    let mut jobs: Vec<ArtifactJob> = Vec::new();
    for (i, archive) in artifacts.iter().enumerate() {
        let local = inbox_path(&inbox, archive)?;
        let slot_start = 7 + (i * 18 / total_artifacts) as u8;
        let slot_end = 7 + ((i + 1) * 18 / total_artifacts) as u8;
        if local.is_file() {
            on_progress(
                slot_end,
                &format!(
                    "复用本地载荷 {}/{}：{}",
                    i + 1,
                    artifacts.len(),
                    archive.rsplit('/').next().unwrap_or(archive)
                ),
            );
            continue;
        }
        jobs.push(ArtifactJob {
            index: jobs.len(),
            url: format!(
                "/api/v1/artifact/{}/{}",
                encode_path(&manifest.generation_id),
                encode_path(archive)
            ),
            local,
            sha: artifact_sha(manifest, archive),
            label: archive.rsplit('/').next().unwrap_or(archive).to_string(),
            slot_start,
            slot_end,
        });
    }
    if !jobs.is_empty() {
        // 小文件多时串行会有明显延迟；限 3 路并发，避免把机械盘/局域网打满。
        const PARALLEL: usize = 3;
        let total_jobs = jobs.len();
        let mut stats: Vec<(std::time::Instant, u64, String, bool)> = jobs
            .iter()
            .map(|_| (std::time::Instant::now(), 0u64, String::new(), true))
            .collect();
        let (tx, rx) = std::sync::mpsc::channel::<ArtifactEvent>();
        std::thread::scope(|scope| -> Result<(), AppError> {
            let launch = |idx: usize| {
                let tx = tx.clone();
                let job = jobs[idx].clone();
                let leader = leader.to_string();
                let token = token.to_string();
                scope.spawn(move || {
                    let result = download_artifact_with_retry(&job, &leader, &token, cancel, &tx);
                    let _ = tx.send(ArtifactEvent::Done(job.index, result));
                });
            };
            let mut launched = total_jobs.min(PARALLEL);
            for idx in 0..launched {
                launch(idx);
            }
            let mut finished = 0usize;
            while finished < total_jobs {
                let event = rx.recv().map_err(|_| {
                    AppError::new("SYNC_DOWNLOAD", "并行下载线程异常退出，未收到完成事件")
                })?;
                match event {
                    ArtifactEvent::Progress(index, written, total) => {
                        let job = &jobs[index];
                        let (speed_at, speed_bytes, speed_text, first) = &mut stats[index];
                        if *first {
                            *first = false;
                            *speed_at = std::time::Instant::now();
                            *speed_bytes = written;
                        } else {
                            let elapsed = speed_at.elapsed().as_secs_f64();
                            if elapsed >= 0.4 {
                                let rate = written.saturating_sub(*speed_bytes) as f64 / elapsed;
                                *speed_text = format!(" · {}", human_rate(rate));
                                *speed_at = std::time::Instant::now();
                                *speed_bytes = written;
                            }
                        }
                        let pct = if total > 0 {
                            job.slot_start
                                + ((job.slot_end.saturating_sub(job.slot_start)) as u64 * written
                                    / total) as u8
                        } else {
                            job.slot_start
                        };
                        let file_pct = if total > 0 {
                            (written.saturating_mul(100) / total).min(100)
                        } else {
                            0
                        };
                        let size = if total > 0 {
                            format!("{}/{}", human_bytes(written), human_bytes(total))
                        } else {
                            human_bytes(written)
                        };
                        on_progress(
                            pct.min(job.slot_end),
                            &format!("下载 {}：{file_pct}% · {size}{}", job.label, speed_text),
                        );
                    }
                    ArtifactEvent::Done(index, result) => {
                        if let Err(e) = result {
                            return Err(e);
                        }
                        finished += 1;
                        let job = &jobs[index];
                        let done_size = fs::metadata(&job.local).map(|m| m.len()).unwrap_or(0);
                        on_progress(
                            job.slot_end,
                            &format!("下载 {} 完成（{}）", job.label, human_bytes(done_size)),
                        );
                        if launched < total_jobs {
                            launch(launched);
                            launched += 1;
                        }
                    }
                }
            }
            Ok(())
        })?;
    }
    log::info!(
        "同步 {}：载荷下载完成，{} 个，耗时 {:.1}s",
        job.id,
        artifacts.len(),
        artifacts_started.elapsed().as_secs_f64()
    );

    check_cancel(cancel_allowed)?;
    let mut report = SyncReport {
        pre_snapshot: Some(pre.id.clone()),
        ..Default::default()
    };
    let mut py_result = PythonApplyResult::default();
    let mut py_env = PythonEnvStage::default();

    // 1) Python 版本：只重建 venv，依赖和轮子留到 ComfyUI 本体之后。
    check_cancel(cancel_allowed)?;
    if scope.python {
        on_progress(
            30,
            &format!(
                "Python 版本：{} → {}（先确保目标环境版本与源端一致）",
                job.diff.python.from_version, job.diff.python.to_version
            ),
        );
        py_env = apply_python_version(
            job,
            manifest,
            &inst,
            &settings,
            pre_write_cancel,
            on_progress,
        )?;
    } else {
        on_progress(30, "本次未选择 Python，跳过 Python 版本同步");
    }

    // 2) 实例设置：启动参数等，必须在 ComfyUI 本体之前落定。
    check_cancel(cancel_allowed)?;
    if scope.comfyui && manifest.format >= 2 {
        if job.diff.launch.changed {
            on_progress(
                38,
                &format!(
                    "实例设置，{}:{} → {}:{}，可同步参数 {} 个",
                    job.diff.launch.from_listen,
                    job.diff.launch.from_port,
                    job.diff.launch.to_listen,
                    job.diff.launch.to_port,
                    job.diff.launch.to_extra_args.len()
                ),
            );
            for arg in job.diff.launch.to_extra_args.iter().take(20) {
                on_progress(38, &format!("  + 启动参数 {}", arg));
            }
            if !job.diff.launch.to_local_extra_args.is_empty() {
                on_progress(
                    38,
                    &format!(
                        "  ! 源端目录类参数未同步（目标机路径不同）：{} 个",
                        job.diff.launch.to_local_extra_args.len()
                    ),
                );
                for arg in job.diff.launch.to_local_extra_args.iter().take(20) {
                    on_progress(38, &format!("  ! 忽略 {}", arg));
                }
            }
            if !job.diff.launch.from_local_extra_args.is_empty() {
                on_progress(
                    38,
                    &format!(
                        "  = 保留目标本机目录参数，{} 个",
                        job.diff.launch.from_local_extra_args.len()
                    ),
                );
                for arg in job.diff.launch.from_local_extra_args.iter().take(20) {
                    on_progress(38, &format!("  = 保留 {}", arg));
                }
            }
            inst.server = merge_launch_server(&manifest.source.server, &inst.server);
        } else {
            on_progress(38, "实例设置无变化");
        }
    } else if !scope.comfyui {
        on_progress(38, "本次未选择 ComfyUI，跳过实例设置同步");
    } else {
        on_progress(38, "源端清单版本较旧，跳过实例设置同步");
    }

    // 3) ComfyUI 本体版本。
    check_cancel(cancel_allowed)?;
    if scope.comfyui {
        on_progress(
            46,
            &format!(
                "ComfyUI 本体，{} → {}：commit {} → {}，通道 {} → {}",
                job.diff.core.from_version.as_deref().unwrap_or("—"),
                job.diff.core.to_version.as_deref().unwrap_or("—"),
                job.diff
                    .core
                    .from_commit
                    .as_deref()
                    .map(|c| short_chars(c, 10))
                    .unwrap_or_else(|| "—".to_string()),
                job.diff
                    .core
                    .to_commit
                    .as_deref()
                    .map(|c| short_chars(c, 10))
                    .unwrap_or_else(|| "—".to_string()),
                job.diff.core.from_channel,
                job.diff.core.to_channel
            ),
        );
        if job.diff.core.changed {
            if job.diff.core.source_dirty {
                on_progress(48, "  源端有已跟踪修改，将按补丁同步");
            }
            apply_core(job, manifest, &inst, &git, &inbox)?;
            cancel_allowed = false;
        } else {
            on_progress(48, "  ComfyUI 本体无变化");
        }
    } else {
        on_progress(46, "本次未选择 ComfyUI 本体，跳过");
    }

    // 4) 依赖与自建 wheel。
    check_cancel(cancel_allowed)?;
    if scope.python {
        on_progress(
            58,
            &format!(
                "依赖与轮子：包新增 {}、删除 {}、改版本 {}；自建包新增 {}、更新 {}、删除 {}",
                job.diff.python.packages_added.len(),
                job.diff.python.packages_removed.len(),
                job.diff.python.packages_changed.len(),
                job.diff.python.wheels_added.len(),
                job.diff.python.wheels_changed.len(),
                job.diff.python.wheels_removed.len()
            ),
        );
        let env_rebuilt = py_env.rebuilt;
        py_result = apply_python_downloads(
            job,
            manifest,
            &mut inst,
            &settings,
            py_env,
            &inbox,
            leader,
            token,
            pre_write_cancel,
            on_progress,
        )?;
        let python_changed = !job.diff.python.packages_added.is_empty()
            || !job.diff.python.packages_removed.is_empty()
            || !job.diff.python.packages_changed.is_empty()
            || !job.diff.python.wheels_added.is_empty()
            || !job.diff.python.wheels_removed.is_empty()
            || !job.diff.python.wheels_changed.is_empty();
        if python_changed || env_rebuilt {
            cancel_allowed = false;
        }
    } else {
        on_progress(58, "本次未选择 Python，跳过依赖与轮子同步");
    }

    // 5) 自定义节点。
    check_cancel(cancel_allowed)?;
    if scope.nodes {
        on_progress(
            75,
            &format!(
                "自定义节点：新增 {}，更新{}，目标多出{}",
                job.diff.nodes.added.len(),
                job.diff.nodes.changed.len(),
                job.diff.nodes.removed.len()
            ),
        );
        for node in job.diff.nodes.added.iter().take(30) {
            on_progress(75, &format!("  + 节点 {}", node.name));
        }
        for node in job.diff.nodes.changed.iter().take(30) {
            on_progress(75, &format!("  ~ 节点 {}：{}", node.name, node.reason));
        }
        if job.options.remove_extra_nodes {
            for node in job.diff.nodes.removed.iter().take(30) {
                on_progress(75, &format!("  - 删除目标多余节点 {}", node.name));
            }
        }
        let nodes_changed = !job.diff.nodes.added.is_empty()
            || !job.diff.nodes.changed.is_empty()
            || (job.options.remove_extra_nodes && !job.diff.nodes.removed.is_empty());
        apply_nodes(job, manifest, &inst, &job.options, &inbox)?;
        if nodes_changed {
            cancel_allowed = false;
        }
    } else {
        on_progress(75, "本次未选择自定义节点，跳过");
    }

    // 6) 模型放到最后：前面版本、依赖、节点都落定后再动大文件。
    check_cancel(cancel_allowed)?;
    if scope.models {
        on_progress(
            84,
            &format!(
                "共享模型：新增{}，覆盖{}，目标多出{}，可复用 {} 个；需传输 {}",
                job.diff.models.added.len(),
                job.diff.models.modified.len(),
                job.diff.models.removed.len(),
                job.diff.models.unchanged,
                human_bytes(job.diff.models.transfer_bytes)
            ),
        );
        for model in job.diff.models.added.iter().take(30) {
            on_progress(
                84,
                &format!(
                    "  + 模型 {}：{}：",
                    model.relative_path,
                    human_bytes(model.size)
                ),
            );
        }
        for model in job.diff.models.modified.iter().take(30) {
            on_progress(
                84,
                &format!(
                    "  ~ 覆盖模型 {}：{} →{}：",
                    model.relative_path,
                    human_bytes(model.from_size),
                    human_bytes(model.size)
                ),
            );
        }
        if job.options.remove_extra_models {
            for model in job.diff.models.removed.iter().take(30) {
                on_progress(84, &format!("  - 删除目标多余模型 {}", model.relative_path));
            }
        }
        // 同一个客户端里共用模型库的A/B 实例也不能在改模型时运行；否则目标端可能
        // 一边被写模型一边被 ComfyUI 读取。这里主动停掉同库实例（它们不参与本次同步）。
        let touching_models = scope.models
            && (!job.diff.models.added.is_empty()
                || !job.diff.models.modified.is_empty()
                || (job.options.remove_extra_models && !job.diff.models.removed.is_empty()));
        if touching_models {
            if let Ok(list) = crate::commands::instance::instance_list() {
                let target_store = model_store(&inst, &settings);
                let target_key = target_store
                    .to_string_lossy()
                    .replace('/', "\\")
                    .to_lowercase();
                for other in list {
                    if other.id == inst.id || other.is_remote() {
                        continue;
                    }
                    let key = model_store(&other, &settings)
                        .to_string_lossy()
                        .replace('/', "\\")
                        .to_lowercase();
                    if key != target_key {
                        continue;
                    }
                    if state.runs.lock().contains_key(&other.id) {
                        on_progress(84, &format!("停止共用模型库的实例 {}", other.name));
                        crate::commands::run::stop_instance(state, &other.id)?;
                    }
                }
            }
        }
        let models_started = std::time::Instant::now();
        let model_count = apply_models(
            job,
            &job.diff,
            manifest,
            &inst,
            &settings,
            &job.options,
            leader,
            token,
            pre_write_cancel,
            on_progress,
        )?;
        report.models = model_count;
        log::info!(
            "同步 {}：模型完成，{} 个，{}，耗时 {:.1}s",
            job.id,
            model_count,
            human_bytes(job.diff.models.transfer_bytes),
            models_started.elapsed().as_secs_f64()
        );
    } else {
        on_progress(84, "本次未选择模型，跳过模型同步");
    }

    on_progress(90, "更新实例登记");
    if scope.comfyui {
        inst.comfy.channel = manifest.source.core.channel.clone();
        inst.comfy.reference = manifest.source.core.reference.clone();
        inst.comfy.commit = manifest.source.core.commit.clone().unwrap_or_default();
        inst.comfy.version = manifest.source.core.version.clone();
    }
    if scope.python {
        inst.python_version = manifest.source.python.short_version.clone();
    }
    inst.state = RunState::Stopped;
    // 源端清单只带版本字符串；目标机真正装上的 torch / CUDA / Python 必须再实测一次，
    // 否则同步后概览和设置还会显示目标机原来的引擎。
    let live = crate::commands::instance::refresh_instance_env(&mut inst);
    on_progress(
        92,
        &format!(
            "读取目标环境：Python {} · torch {} · CUDA {}",
            if live.python_version_full.is_empty() {
                "未检测到"
            } else {
                live.python_version_full.as_str()
            },
            if live.torch_version.is_empty() {
                "未检测到"
            } else {
                live.torch_version.as_str()
            },
            if live.cuda_available {
                "可用"
            } else {
                "不可用"
            }
        ),
    );
    save_instance(&inst)?;
    // 核心/venv/节点/模型都已经落盘，instance.json 也已更新；从这里开始
    // 后置校验、快照、lastApplied 失败不能再回滚核心，否则会得到
    // “代码回旧、登记是新”的半新半旧状态。
    rollback.disarm();
    // 实例登记已经更新，后面的校验/快照/写 lastApplied 必须走完，不能再取消。
    on_progress(94, "校验目标状态");
    let after = capture_instance_state_scoped(&inst, &settings, Some(&git), scope, None)?;
    let mut warnings = Vec::new();
    if scope.comfyui && !core_versions_match(&after.core, &manifest.source.core) {
        warnings.push(format!(
            "目标 ComfyUI 版本与源端不一致：目标 {} / 源 {}",
            after.core.version.as_deref().unwrap_or("未知"),
            manifest.source.core.version.as_deref().unwrap_or("未知")
        ));
    }
    if scope.nodes {
        for node in &manifest.source.nodes {
            if let Some(found) = after.nodes.iter().find(|n| n.name == node.name) {
                if found.worktree_hash != node.worktree_hash {
                    warnings.push(format!("节点 {} 内容与源端不一致", node.name));
                }
            } else {
                warnings.push(format!("节点 {} 没有安装成功", node.name));
            }
        }
    }
    let post = snapshot::capture_and_save(
        &inst,
        &settings,
        "post-sync",
        Some(format!("同步后：{}", job.source_instance_name)),
    )
    .map_err(|e| AppError::new("SYNC_SNAPSHOT", format!("同步后快照失败：{e}")))?;
    let fingerprint = state_fingerprint_scoped(
        &capture_instance_state_scoped(&inst, &settings, Some(&git), scope, None)?,
        scope,
    );
    write_last_applied(
        &inst.id,
        &LastApplied {
            generation_id: manifest.generation_id.clone(),
            source_instance_id: manifest.source_instance_id.clone(),
            source_instance_name: manifest.source_instance_name.clone(),
            applied_at: now(),
            snapshot_id: Some(post.id.clone()),
            fingerprint,
        },
    )?;
    report.post_snapshot = Some(post.id);
    report.core = manifest
        .source
        .core
        .commit
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    report.python = manifest.source.python.version.clone();
    report.nodes = manifest.source.nodes.len();
    warnings.extend(py_result.warnings);
    report.issues.extend(py_result.issues);
    report.partial = !report.issues.is_empty();
    for warning in &warnings {
        on_progress(94, &format!("  ! {}", warning));
    }
    report.warnings = warnings;
    log::info!(
        "同步 {}：应用完成，总耗时 {:.1}s",
        job.id,
        job_started.elapsed().as_secs_f64()
    );
    Ok(report)
}

#[derive(Default)]
struct PythonApplyResult {
    warnings: Vec<String>,
    issues: Vec<SyncIssue>,
}

#[derive(Default)]
struct PythonEnvStage {
    venv_swap: Option<VenvSwap>,
    rebuilt: bool,
}

/// 同步第一阶段：只处理 Python 版本本身（必要时重建 venv）。
/// 依赖和自建 wheel 留到 ComfyUI 本体之后再装，顺序是：
/// Python 版本 → 实例设置 → ComfyUI 版本 → 依赖/轮子 → 节点 → 模型。
#[allow(clippy::too_many_arguments)]
fn apply_python_version(
    job: &SyncJob,
    manifest: &SyncManifest,
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<PythonEnvStage, AppError> {
    let root = PathBuf::from(&inst.root_dir);
    let venv = current_venv(inst);
    let target_py = python_exe(&venv);
    // 没触发重建也顺手清理到期旧环境，避免用户一直不同步时目录永远留着。
    prune_old_venvs(&venv, settings.sync.keep_old_venv_days);

    if target_py.exists() && manifest.source.python.version == inst.python_version_full(settings) {
        on_progress(
            30,
            &format!("Python 版本已一致：{}", manifest.source.python.version),
        );
        return Ok(PythonEnvStage::default());
    }
    if inst.python.is_existing() {
        return Err(AppError::new(
            "SYNC_PYTHON_EXISTING",
            "目标实例使用已有 Python 环境，启动器不能重建；请把它切回「自建 venv」后再同步",
        ));
    }

    let uv = crate::commands::env::resolve_uv()?;
    on_progress(
        30,
        &format!(
            "重建 venv：Python {}（目标旧环境 {}）",
            manifest.source.python.version,
            inst.python_version_full(settings)
        ),
    );
    let old = if venv.exists() {
        // 只在 Python 版本变化/venv 缺失时重建。旧环境同盘临时改名，
        // 成功后按设置保留几天供回退，失败则自动恢复。
        let old = venv.with_file_name(format!(
            ".venv-comfyfleet-swap-{}",
            job.id.chars().take(8).collect::<String>()
        ));
        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        if let Some(parent) = old.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&venv, &old)
            .map_err(|e| AppError::new("SYNC_VENV", format!("准备重建 venv 失败，{e}")))?;
        Some(old)
    } else {
        None
    };
    let venv_swap = VenvSwap::new(venv.clone(), old, settings.sync.keep_old_venv_days);
    let args = vec![
        "venv".to_string(),
        "--seed".to_string(),
        "--python".to_string(),
        manifest.source.python.version.clone(),
        "--python-preference".to_string(),
        "managed".to_string(),
        venv.to_string_lossy().to_string(),
    ];
    let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let out = run_uv_stream_cancellable(
        &uv,
        &args,
        &root,
        &mut |line| {
            report_install_line(on_progress, 31, "创建环境：", line, &mut stream_last);
        },
        cancel,
    )?;
    if !out.ok() {
        return Err(AppError::new(
            "SYNC_VENV",
            format!(
                "创建 Python {} 环境失败，{}",
                manifest.source.python.version,
                out.detail()
            ),
        ));
    }
    on_progress(
        36,
        &format!(
            "Python {} 环境就绪，稍后安装依赖与轮子",
            manifest.source.python.version
        ),
    );
    // 不在这里提交 swap：后面 ComfyUI/依赖/节点失败时仍要能回滚整套环境；
    // 安装完依赖（或确认无包变化）后由 apply_python_downloads 提交。
    Ok(PythonEnvStage {
        venv_swap: Some(venv_swap),
        rebuilt: true,
    })
}

/// 安装普通依赖与自定义 wheel/项目。venv 版本已在第一阶段就绪；
/// 这里只负责依赖，安装顺序仍是普通依赖→自定义包。
#[allow(clippy::too_many_arguments)]
fn apply_python_downloads(
    job: &SyncJob,
    manifest: &SyncManifest,
    inst: &mut InstanceConfig,
    settings: &GlobalSettings,
    env: PythonEnvStage,
    inbox: &Path,
    leader: &str,
    token: &str,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u8, &str),
) -> Result<PythonApplyResult, AppError> {
    let root = PathBuf::from(&inst.root_dir);
    let venv = current_venv(inst);
    let target_py = python_exe(&venv);
    let py = &job.diff.python;
    let py_changed = py.from_version != py.to_version
        || !py.packages_added.is_empty()
        || !py.packages_removed.is_empty()
        || !py.packages_changed.is_empty()
        || !py.wheels_added.is_empty()
        || !py.wheels_removed.is_empty()
        || !py.wheels_changed.is_empty();
    if !py_changed && target_py.exists() {
        on_progress(58, "依赖与轮子无变化");
        let mut result = PythonApplyResult::default();
        for wheel in &job.diff.python.missing_wheels {
            let reason = wheel
                .reason
                .as_deref()
                .unwrap_or("本地 wheel 库没有副本，也没有记录远程来源")
                .to_string();
            result.warnings.push(format!("{}：{reason}", wheel.name));
            result.issues.push(SyncIssue {
                kind: if wheel.incompatible {
                    "incompatible".to_string()
                } else {
                    "missing".to_string()
                },
                name: wheel.name.clone(),
                reason,
            });
        }
        if let Some(mut swap) = env.venv_swap {
            swap.commit();
        }
        return Ok(result);
    }
    if inst.python.is_existing() {
        return Err(AppError::new(
            "SYNC_PYTHON_EXISTING",
            "目标实例使用已有 Python 环境，启动器不能重建；请把它切回「自建 venv」后再同步",
        ));
    }
    let uv = crate::commands::env::resolve_uv()?;
    // 这里已经完成（或确认不需要）venv 重建；之后再列变更明细，避免日志
    // 看起来像“先装包、后重建环境”。明细只是待安装项，不是已完成。
    on_progress(
        58,
        if env.rebuilt {
            "venv 重建完成，开始安装依赖"
        } else {
            "venv 已就绪，开始安装依赖"
        },
    );
    for pkg in py.packages_added.iter().take(30) {
        on_progress(60, &format!("  + 待新增 {}=={}", pkg.name, pkg.to));
    }
    for pkg in py.packages_changed.iter().take(30) {
        on_progress(
            60,
            &format!("  ~ 待更新 {}：{} → {}", pkg.name, pkg.from, pkg.to),
        );
    }
    for pkg in py.packages_removed.iter().take(30) {
        on_progress(60, &format!("  - 待删除 {}=={}", pkg.name, pkg.from));
    }
    for wheel in py.wheels_added.iter().take(20) {
        on_progress(60, &format!("  + 待安装自建包 {} {}", wheel.name, wheel.to));
    }
    for wheel in py.wheels_changed.iter().take(20) {
        on_progress(
            60,
            &format!(
                "  ~ 待更新自建包 {}：{} → {}",
                wheel.name, wheel.from, wheel.to
            ),
        );
    }
    let req_path = inbox.join("requirements.txt");
    let lines = requirement_lines(
        &manifest.source.python.packages,
        &manifest.source.python.custom_wheels,
    )?;
    fs::write(&req_path, lines.join("\n"))?;
    // 源端 freeze 是“已经装成什么样”的快照，不一定满足新版元数据里的依赖
    // 约束（例如 albumentations 2.0.8 声明 albucore==0.0.24，而环境里实际
    // 是 0.2.21）。用 `uv pip sync` 会先做依赖求解，直接报无解；这里按冻结
    // 清单精确安装、忽略传递依赖，再用 --exact 清掉多余包，效果等同于同步
    // 一份可运行的现有环境。
    let mut args = vec![
        "pip".to_string(),
        "install".to_string(),
        "--no-deps".to_string(),
        "--exact".to_string(),
        "-r".to_string(),
        req_path.to_string_lossy().to_string(),
        "--python".to_string(),
        target_py.to_string_lossy().to_string(),
    ];
    // 目标端可能配置了缺包的 PyPI 镜像（例如镜像还没同步 ComfyUI 新拆出的
    // media-assets 包）。官方 PyPI 作为最后兜底；requirements 全是源端精确
    // 版本且 --no-deps，不会因为兜底索引把依赖版本带偏。
    let torch_index = torch_index_for(&manifest.source.python);
    let pypi_index = settings
        .mirrors
        .pypi
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let indexes = install_indexes(torch_index.as_deref(), pypi_index);
    for (index, value) in indexes.iter().enumerate() {
        args.push(if index == 0 {
            "--index-url".to_string()
        } else {
            "--extra-index-url".to_string()
        });
        args.push(value.clone());
    }
    // uv 默认只在“第一个包含该包的索引”里找版本。允许跨索引匹配；requirements
    // 全是源端 pip freeze 的精确版本，容忍度只用于找到同一个版本。
    if indexes.len() > 1 {
        args.push("--index-strategy".to_string());
        args.push("unsafe-best-match".to_string());
    }
    on_progress(
        62,
        &format!(
            "安装普通依赖：{} 个包，按冻结清单精确安装（不重新求解依赖）",
            lines.len()
        ),
    );
    let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let out = run_uv_stream_cancellable(
        &uv,
        &args,
        &root,
        &mut |line| {
            report_install_line(on_progress, 62, "安装依赖：", line, &mut stream_last);
        },
        cancel,
    )?;
    if !out.ok() {
        return Err(AppError::new(
            "SYNC_PIP",
            format!("安装普通依赖失败：{}", out.detail()),
        ));
    }
    on_progress(64, "普通依赖安装完成");
    let mut wheel_warnings: Vec<String> = Vec::new();
    let mut wheel_issues: Vec<SyncIssue> = Vec::new();
    for wheel in &manifest.source.python.custom_wheels {
        if wheel.vcs.as_deref() == Some("git") {
            on_progress(
                66,
                &format!(
                    "安装 Git 自建包 {} {}",
                    wheel.name,
                    wheel
                        .commit
                        .as_deref()
                        .map(|c| format!("@{}", short_chars(c, 10)))
                        .unwrap_or_default()
                ),
            );
            let url = wheel
                .source_ref
                .as_deref()
                .filter(|value| !value.is_empty());
            let Some(url) = url else {
                let reason = "缺少仓库地址".to_string();
                on_progress(66, &format!("  ! 跳过 Git 包 {}：{}", wheel.name, reason));
                wheel_warnings.push(format!("{}：{reason}", wheel.name));
                wheel_issues.push(SyncIssue {
                    kind: "install".to_string(),
                    name: wheel.name.clone(),
                    reason,
                });
                continue;
            };
            let spec = match vcs_install_spec(url, wheel.commit.as_deref().unwrap_or("")) {
                Ok(spec) => spec,
                Err(e) => {
                    let reason = e.message;
                    on_progress(66, &format!("  ! 跳过 Git 包 {}：{}", wheel.name, reason));
                    wheel_warnings.push(format!("{}：{reason}", wheel.name));
                    wheel_issues.push(SyncIssue {
                        kind: "install".to_string(),
                        name: wheel.name.clone(),
                        reason,
                    });
                    continue;
                }
            };
            let mut args = vec![
                "pip".to_string(),
                "install".to_string(),
                "--no-deps".to_string(),
                spec,
                "--python".to_string(),
                target_py.to_string_lossy().to_string(),
            ];
            if let Some(index) = settings
                .mirrors
                .pypi
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                args.push("--index-url".to_string());
                args.push(index.to_string());
            }
            let prefix = format!("安装 Git 包 {}：", wheel.name);
            let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let out = run_uv_stream_cancellable(
                &uv,
                &args,
                &root,
                &mut |line| {
                    report_install_line(on_progress, 66, &prefix, line, &mut stream_last);
                },
                cancel,
            )?;
            if !out.ok() {
                let detail = out.detail();
                let summary = detail
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("uv 安装失败")
                    .trim()
                    .to_string();
                on_progress(
                    66,
                    &format!("  ! 安装 Git 包 {} 失败，继续同步：{}", wheel.name, summary),
                );
                wheel_warnings.push(format!("{}：{}", wheel.name, summary));
                wheel_issues.push(SyncIssue {
                    kind: "install".to_string(),
                    name: wheel.name.clone(),
                    reason: detail,
                });
            }
            continue;
        }
        // 远程 URL 包不在同步包里带文件，直接让目标端用 URL 安装。
        if wheel.source == "remote-url" {
            let Some(url) = wheel
                .source_ref
                .as_deref()
                .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
            else {
                let reason = "远程 URL 为空或不是 http(s) 地址".to_string();
                on_progress(66, &format!("跳过远程 wheel {}", wheel.name));
                wheel_warnings.push(format!("{}：{reason}", wheel.name));
                wheel_issues.push(SyncIssue {
                    kind: "missing".to_string(),
                    name: wheel.name.clone(),
                    reason,
                });
                continue;
            };
            if let Some(reason) = wheel_compat_reason(wheel, &manifest.source.python) {
                on_progress(
                    66,
                    &format!(
                        "兼容性提示：{} 与源环境不兼容（{}），仍按完全一致策略用 URL 安装",
                        wheel.name, reason
                    ),
                );
                wheel_warnings.push(format!("{}：{reason}（仍按完全一致策略安装）", wheel.name));
            }
            on_progress(66, &format!("安装远程 wheel {}", wheel.name));
            let args = vec![
                "pip".to_string(),
                "install".to_string(),
                "--no-deps".to_string(),
                url.to_string(),
                "--python".to_string(),
                target_py.to_string_lossy().to_string(),
            ];
            let prefix = format!("安装远程 wheel {}：", wheel.name);
            let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let out = run_uv_stream_cancellable(
                &uv,
                &args,
                &root,
                &mut |line| {
                    report_install_line(on_progress, 66, &prefix, line, &mut stream_last);
                },
                cancel,
            )?;
            if !out.ok() {
                let uv_detail = out.detail();
                let uv_summary = last_detail_line(&uv_detail);
                on_progress(
                    66,
                    &format!(
                        "uv 安装远程 wheel {} 失败，改用 pip 重试：{}",
                        wheel.name, uv_summary
                    ),
                );
                let pip_args = pip_wheel_install_args(url);
                let pip_prefix = format!("pip 安装远程 wheel {}：", wheel.name);
                let mut pip_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
                let pip_out = run_pip_stream_cancellable(
                    &target_py,
                    &pip_args,
                    &root,
                    &mut |line| {
                        report_install_line(on_progress, 66, &pip_prefix, line, &mut pip_last);
                    },
                    cancel,
                )?;
                if pip_out.ok() {
                    on_progress(
                        66,
                        &format!("  ! uv 拒绝安装 {}，已用 pip 安装成功", wheel.name),
                    );
                    wheel_warnings.push(format!(
                        "{}：uv 因 wheel 版本/文件名校验失败，已用 pip 安装成功",
                        wheel.name
                    ));
                } else {
                    let pip_detail = pip_out.detail();
                    let pip_summary = last_detail_line(&pip_detail);
                    on_progress(
                        66,
                        &format!(
                            "  ! 安装远程 wheel {} 失败，继续同步：{}",
                            wheel.name, pip_summary
                        ),
                    );
                    wheel_warnings.push(format!(
                        "{}：uv 和 pip 均安装失败：{}",
                        wheel.name, pip_summary
                    ));
                    wheel_issues.push(SyncIssue {
                        kind: "install".to_string(),
                        name: wheel.name.clone(),
                        reason: format!("uv:\n{uv_detail}\npip:\n{pip_detail}"),
                    });
                }
            }
            continue;
        }
        let Some(archive) = &wheel.archive else {
            // 缺件不阻断其它包；结束后由报告统一提示。
            let reason = wheel
                .missing_reason
                .as_deref()
                .unwrap_or("本地 wheel 库没有副本，也没有记录远程来源");
            on_progress(
                66,
                &format!("跳过缺少制品的自定义包 {}：{}", wheel.name, reason),
            );
            wheel_warnings.push(format!("{}：{}", wheel.name, reason));
            wheel_issues.push(SyncIssue {
                kind: if wheel.incompatible {
                    "incompatible".to_string()
                } else {
                    "missing".to_string()
                },
                name: wheel.name.clone(),
                reason: reason.to_string(),
            });
            continue;
        };
        let raw_local = match inbox_path(inbox, archive) {
            Ok(path) => path,
            Err(e) => {
                let reason = e.message;
                on_progress(66, &format!("  ! 跳过自定义包 {}：{}", wheel.name, reason));
                wheel_warnings.push(format!("{}：{reason}", wheel.name));
                wheel_issues.push(SyncIssue {
                    kind: "missing".to_string(),
                    name: wheel.name.clone(),
                    reason,
                });
                continue;
            }
        };
        // 老代次远程 URL 可能把制品名存成 `cstr` 这种没有扩展名的形式，
        // uv 会直接拒绝解析。安装前按内容还原成 .whl/.zip/.tar.gz 的名字。
        let (local, install_name) = if archive.starts_with("env/projects/") {
            (raw_local, wheel.filename.clone())
        } else {
            let local = match ensure_install_archive(
                &raw_local,
                &wheel.filename,
                &wheel.name,
                &wheel.version,
            ) {
                Ok(local) => local,
                Err(e) => {
                    let reason = e.message;
                    on_progress(66, &format!("  ! 跳过自定义包 {}：{}", wheel.name, reason));
                    wheel_warnings.push(format!("{}：{reason}", wheel.name));
                    wheel_issues.push(SyncIssue {
                        kind: "missing".to_string(),
                        name: wheel.name.clone(),
                        reason,
                    });
                    continue;
                }
            };
            let name = local
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or(wheel.filename.as_str())
                .to_string();
            (local, name)
        };
        if let Some(reason) = wheel_compat_reason(wheel, &manifest.source.python) {
            // 目标是“与源完全一致”，兼容性只提示，不阻止安装。
            on_progress(
                66,
                &format!(
                    "兼容性提示：{} 与源环境不兼容（{}），仍按完全一致策略安装",
                    wheel.name, reason
                ),
            );
            wheel_warnings.push(format!("{}：{reason}（仍按完全一致策略安装）", wheel.name));
        }
        let _ = &install_name;
        if archive.starts_with("env/projects/") {
            if !safe_name(&wheel.name) {
                let reason = format!("自定义包名不安全，拒绝删除/解压：{}", wheel.name);
                on_progress(
                    66,
                    &format!("  ! 跳过自定义项目 {}：{}", wheel.name, reason),
                );
                wheel_warnings.push(format!("{}：{reason}", wheel.name));
                wheel_issues.push(SyncIssue {
                    kind: "install".to_string(),
                    name: wheel.name.clone(),
                    reason,
                });
                continue;
            }
            on_progress(66, &format!("安装本地项目 {}", wheel.name));
            let dir = inbox.join("projects").join(&wheel.name);
            if dir.exists() {
                let _ = fs::remove_dir_all(&dir);
            }
            if let Err(e) = extract_zip(&local, &dir) {
                let reason = format!("解压失败，{}", e.message);
                on_progress(
                    66,
                    &format!("  ! 跳过自定义项目 {}：{}", wheel.name, reason),
                );
                wheel_warnings.push(format!("{}：{reason}", wheel.name));
                wheel_issues.push(SyncIssue {
                    kind: "install".to_string(),
                    name: wheel.name.clone(),
                    reason,
                });
                continue;
            }
            let mut args = vec![
                "pip".to_string(),
                "install".to_string(),
                "--no-deps".to_string(),
                dir.to_string_lossy().to_string(),
                "--python".to_string(),
                target_py.to_string_lossy().to_string(),
            ];
            if let Some(index) = settings
                .mirrors
                .pypi
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                args.push("--index-url".to_string());
                args.push(index.to_string());
            }
            let prefix = format!("安装项目 {}：", wheel.name);
            let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let out = run_uv_stream_cancellable(
                &uv,
                &args,
                &root,
                &mut |line| {
                    report_install_line(on_progress, 66, &prefix, line, &mut stream_last);
                },
                cancel,
            )?;
            if !out.ok() {
                let detail = out.detail();
                let summary = detail
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("uv 安装失败")
                    .trim()
                    .to_string();
                on_progress(
                    66,
                    &format!(
                        "  ! 安装自定义项目 {} 失败，继续同步：{}",
                        wheel.name, summary
                    ),
                );
                wheel_warnings.push(format!("{}：{}", wheel.name, summary));
                wheel_issues.push(SyncIssue {
                    kind: "install".to_string(),
                    name: wheel.name.clone(),
                    reason: detail,
                });
            }
        } else {
            on_progress(66, &format!("安装自定义包 {}", wheel.name));
            let args = vec![
                "pip".to_string(),
                "install".to_string(),
                "--no-deps".to_string(),
                local.to_string_lossy().to_string(),
                "--python".to_string(),
                target_py.to_string_lossy().to_string(),
            ];
            let prefix = format!("安装自定义包 {}：", wheel.name);
            let mut stream_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let out = run_uv_stream_cancellable(
                &uv,
                &args,
                &root,
                &mut |line| {
                    report_install_line(on_progress, 66, &prefix, line, &mut stream_last);
                },
                cancel,
            )?;
            if !out.ok() {
                let uv_detail = out.detail();
                let uv_summary = last_detail_line(&uv_detail);
                on_progress(
                    66,
                    &format!(
                        "uv 安装自定义 wheel {} 失败，改用 pip 重试：{}",
                        wheel.name, uv_summary
                    ),
                );
                let local_spec = local.to_string_lossy();
                let pip_args = pip_wheel_install_args(&local_spec);
                let pip_prefix = format!("pip 安装自定义 wheel {}：", wheel.name);
                let mut pip_last = std::time::Instant::now() - std::time::Duration::from_secs(1);
                let pip_out = run_pip_stream_cancellable(
                    &target_py,
                    &pip_args,
                    &root,
                    &mut |line| {
                        report_install_line(on_progress, 66, &pip_prefix, line, &mut pip_last);
                    },
                    cancel,
                )?;
                if pip_out.ok() {
                    on_progress(
                        66,
                        &format!("  ! uv 拒绝安装 {}，已用 pip 安装成功", wheel.name),
                    );
                    wheel_warnings.push(format!(
                        "{}：uv 因 wheel 版本/文件名校验失败，已用 pip 安装成功",
                        wheel.name
                    ));
                } else {
                    let pip_detail = pip_out.detail();
                    let pip_summary = last_detail_line(&pip_detail);
                    on_progress(
                        66,
                        &format!(
                            "  ! 安装自定义 wheel {} 失败，继续同步：{}",
                            wheel.name, pip_summary
                        ),
                    );
                    wheel_warnings.push(format!(
                        "{}：uv 和 pip 均安装失败：{}",
                        wheel.name, pip_summary
                    ));
                    wheel_issues.push(SyncIssue {
                        kind: "install".to_string(),
                        name: wheel.name.clone(),
                        reason: format!("uv:\n{uv_detail}\npip:\n{pip_detail}"),
                    });
                }
            }
        }
    }
    let mut warnings = wheel_warnings;
    // 包版本以已安装元数据为准（uv pip list），不要从 direct URL 的 wheel
    // 文件名取版本；否则同一颗 wheel 可能被读成 2.8.3+cu... 与 2.8.3 两个版本。
    if let Ok(got) = snapshot::installed_packages(inst, &uv, &target_py) {
        let want = &manifest.source.python.packages;
        let mut diffs = Vec::new();
        for (name, version) in want {
            let expected = percent_decode(version);
            let matches = got
                .get(name)
                .is_some_and(|actual| package_versions_match(name, actual, &expected));
            if !matches {
                diffs.push(format!(
                    "{name}: 目标 {} / 源 {}",
                    got.get(name).map(String::as_str).unwrap_or("缺失"),
                    expected
                ));
            }
        }
        if !diffs.is_empty() {
            log::warn!(
                "同步后包清单有差异（{} 项），{:?}",
                diffs.len(),
                &diffs[..diffs.len().min(8)]
            );
            warnings.push(format!(
                "Python 包清单有 {} 项与源端不一致：{}",
                diffs.len(),
                diffs[..diffs.len().min(6)].join("：")
            ));
        }
    }
    // 同步只负责把源端的包搬过来；依赖冲突由诊断页按需检查，
    // 不在同步完成时做 pip check。
    let _ = &manifest.generation_id;
    let _ = leader;
    let _ = token;
    if let Some(mut swap) = env.venv_swap {
        swap.commit();
    }
    Ok(PythonApplyResult {
        warnings,
        issues: wheel_issues,
    })
}

impl InstanceConfig {
    fn python_version_full(&self, _settings: &GlobalSettings) -> String {
        // 目标端没有保存完整patch 版本；venv 的python.exe 可以告诉我们。
        let py = current_venv(self).join("Scripts").join("python.exe");
        let out = proc::run(
            &py,
            &["-c", "import platform;print(platform.python_version())"],
            None,
            &BTreeMap::new(),
        )
        .ok();
        out.filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_else(|| self.python_version.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comfyfleet-sync-test-{tag}-{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn percent_decode_never_slices_inside_utf8() {
        assert_eq!(percent_decode("%E4%B8%AD"), "中");
        assert_eq!(percent_decode("%中"), "%中");
        assert_eq!(percent_decode("%A"), "%A");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        assert_eq!(percent_decode("a%20b"), "a b");
    }

    #[test]
    fn core_hash_ignores_nodes_models_and_git() {
        let one = temp_dir("core-a");
        let two = temp_dir("core-b");
        for dir in [&one, &two] {
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::write(dir.join(".git").join("config"), "git").unwrap();
            fs::create_dir_all(dir.join("custom_nodes").join("n")).unwrap();
            fs::write(dir.join("custom_nodes").join("n").join("a.py"), "x").unwrap();
            fs::create_dir_all(dir.join("models")).unwrap();
            fs::write(dir.join("models").join("m.bin"), "model").unwrap();
            fs::write(dir.join("main.py"), "print(1)").unwrap();
        }
        fs::write(two.join("custom_nodes").join("n").join("a.py"), "different").unwrap();
        fs::write(two.join("models").join("m.bin"), "other").unwrap();
        assert_eq!(hash_core_tree(&one), hash_core_tree(&two));
        fs::write(two.join("main.py"), "print(2)").unwrap();
        assert_ne!(hash_core_tree(&one), hash_core_tree(&two));
        let _ = fs::remove_dir_all(&one);
        let _ = fs::remove_dir_all(&two);
    }

    #[test]
    fn core_hash_ignores_crlf_line_endings() {
        let one = temp_dir("core-crlf-a");
        let two = temp_dir("core-crlf-b");
        fs::write(one.join("main.py"), "print(1)\r\nprint(2)\r\n").unwrap();
        fs::write(two.join("main.py"), "print(1)\nprint(2)\n").unwrap();
        assert_eq!(hash_core_tree(&one), hash_core_tree(&two));
        // 另加一个文件来验证内容变化仍能被识别；不要在同一路径上覆盖同长度内容，
        // 否则 Windows 文件时间的粗粒度会让 stat 缓存命中旧哈希。
        fs::write(two.join("third.py"), "print(3)\n").unwrap();
        assert_ne!(hash_core_tree(&one), hash_core_tree(&two));
        let _ = fs::remove_dir_all(&one);
        let _ = fs::remove_dir_all(&two);
    }

    #[test]
    fn sync_cache_prunes_old_generations_but_keeps_active() {
        let root = temp_dir("prune-generations");
        let generations = root.join("generations");
        fs::create_dir_all(&generations).unwrap();
        for name in [
            "20260101_000000-aaaa",
            "20260102_000000-bbbb",
            "20260103_000000-cccc",
            "20260104_000000-dddd",
        ] {
            let dir = generations.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data.bin"), [7u8; 10]).unwrap();
        }
        let protected = BTreeSet::from(["20260101_000000-aaaa".to_string()]);
        let report = prune_cache_dirs(&generations, 2, &protected, true);
        assert_eq!(report.removed, 1, "只应删掉被两个新代次挤出的一条");
        assert_eq!(report.freed_bytes, 10);
        assert!(
            generations.join("20260101_000000-aaaa").exists(),
            "活动代次必须保留"
        );
        assert!(!generations.join("20260102_000000-bbbb").exists());
        assert!(generations.join("20260103_000000-cccc").exists());
        assert!(generations.join("20260104_000000-dddd").exists());

        let info = sync_cache_info_at(&root);
        assert_eq!(info.generation_count, 3);
        assert_eq!(info.generation_bytes, 30);
        assert_eq!(info.total_bytes, 30);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn core_zip_excludes_models_and_nodes() {
        let root = temp_dir("zip");
        fs::create_dir_all(root.join("custom_nodes")).unwrap();
        fs::create_dir_all(root.join("models")).unwrap();
        fs::write(root.join("main.py"), "print(1)").unwrap();
        fs::write(root.join("custom_nodes").join("a.py"), "node").unwrap();
        fs::write(root.join("models").join("m.bin"), "model").unwrap();
        let zip = root.join("core.zip");
        zip_dir_ex(&root, &zip, false, CORE_EXCLUDE).unwrap();
        let out = root.join("out");
        extract_zip(&zip, &out).unwrap();
        assert!(out.join("main.py").is_file());
        assert!(!out.join("custom_nodes").exists());
        assert!(!out.join("models").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_rejects_parent_traversal() {
        let root = temp_dir("zip-slip");
        let zip_path = root.join("bad.zip");
        let file = File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opt = zip::write::SimpleFileOptions::default();
        zip.start_file("../evil.txt", opt).unwrap();
        zip.write_all(b"evil").unwrap();
        zip.finish().unwrap();
        let out = root.join("out");
        assert!(extract_zip(&zip_path, &out).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_rejects_windows_ads_and_device_names() {
        for name in ["file.txt:payload", "NUL", "trailing."] {
            let root = temp_dir("zip-win-path");
            let zip_path = root.join("bad.zip");
            let file = File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opt = zip::write::SimpleFileOptions::default();
            zip.start_file(name, opt).unwrap();
            zip.write_all(b"evil").unwrap();
            zip.finish().unwrap();
            let out = root.join("out");
            assert!(
                extract_zip(&zip_path, &out).is_err(),
                "zip entry should be rejected: {name}"
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn diff_covers_four_groups() {
        let mut source = InstanceState {
            instance_id: "s".to_string(),
            name: "源".to_string(),
            ..Default::default()
        };
        source.core.commit = Some("bbbb".to_string());
        source.core.worktree_hash = "h2".to_string();
        source.python.version = "3.13.6".to_string();
        source
            .python
            .packages
            .insert("torch".to_string(), "2.9.0".to_string());
        source.nodes.push(SyncNodeState {
            name: "node-a".to_string(),
            enabled: true,
            worktree_hash: "n2".to_string(),
            ..Default::default()
        });
        source.models.push(SyncModelFile {
            file_id: "m1".to_string(),
            relative_path: "checkpoints/a.safetensors".to_string(),
            size: 10,
            sha256: Some("hash".to_string()),
            ..Default::default()
        });
        let mut target = InstanceState {
            instance_id: "t".to_string(),
            name: "目标".to_string(),
            ..Default::default()
        };
        target.core.commit = Some("aaaa".to_string());
        target.core.worktree_hash = "h1".to_string();
        target.python.version = "3.12.8".to_string();
        let diff = diff_states(&source, &target);
        assert!(diff.core.changed);
        assert_eq!(diff.python.packages_added.len(), 1);
        assert_eq!(diff.nodes.added.len(), 1);
        assert_eq!(diff.models.added.len(), 1);
        assert!(diff.needs_apply);
    }

    #[test]
    fn package_diff_ignores_local_build_tag_when_only_one_side_has_it() {
        let mut source = InstanceState::default();
        source
            .python
            .packages
            .insert("flash-attn".to_string(), "2.8.3".to_string());
        let mut target = InstanceState::default();
        target.python.packages.insert(
            "flash-attn".to_string(),
            "2.8.3+cu130torch2.10.0cxx11abiTRUE".to_string(),
        );
        let diff = diff_states(&source, &target);
        assert!(
            diff.python.packages_changed.is_empty(),
            "同一上游版本、本地构建标签只在一侧出现，不应报包版本变化"
        );

        // 两边都带不同的本地构建标签，仍然要报：cu128 / cu130 不是同一个构建。
        source.python.packages.insert(
            "flash-attn".to_string(),
            "2.8.3+cu130torch2.10.0cxx11abiTRUE".to_string(),
        );
        target.python.packages.insert(
            "flash-attn".to_string(),
            "2.8.3+cu128torch2.10.0cxx11abiTRUE".to_string(),
        );
        let diff = diff_states(&source, &target);
        assert_eq!(diff.python.packages_changed.len(), 1);

        // torch 全家继续严格比较，不能把不同 CUDA 构建当成同一个。
        target
            .python
            .packages
            .insert("torch".to_string(), "2.10.0+cu128".to_string());
        source
            .python
            .packages
            .insert("torch".to_string(), "2.10.0+cu130".to_string());
        let diff = diff_states(&source, &target);
        assert!(diff
            .python
            .packages_changed
            .iter()
            .any(|p| p.name == "torch"));
    }

    #[test]
    fn package_diff_ignores_unknown_versions_but_not_torch() {
        let mut source = InstanceState::default();
        let mut target = InstanceState::default();
        // 旧清单/直接 URL 包可能只留包名，版本为空；不能报成一次降级。
        source
            .python
            .packages
            .insert("argostranslate".to_string(), String::new());
        target
            .python
            .packages
            .insert("argostranslate".to_string(), "1.9.6".to_string());
        let diff = diff_states(&source, &target);
        assert!(
            diff.python.packages_changed.is_empty(),
            "未知版本不应制造包变更"
        );

        // torch 是保护包，版本缺失仍要报出来，不能悄悄跳过。
        source
            .python
            .packages
            .insert("torch".to_string(), String::new());
        target
            .python
            .packages
            .insert("torch".to_string(), "2.10.0+cu130".to_string());
        let diff = diff_states(&source, &target);
        assert!(diff
            .python
            .packages_changed
            .iter()
            .any(|p| p.name == "torch"));
    }

    #[test]
    fn core_version_check_ignores_worktree_hash() {
        let mut source = SyncCoreState::default();
        source.version = Some("v0.44.0".to_string());
        source.worktree_hash = "source-hash".to_string();
        let mut target = SyncCoreState::default();
        target.version = Some("0.44.0".to_string());
        target.worktree_hash = "target-hash".to_string();
        assert!(core_versions_match(&source, &target));
        target.version = Some("v0.45.0".to_string());
        assert!(!core_versions_match(&source, &target));
    }

    #[test]
    fn scoped_diff_leaves_unselected_groups_empty() {
        let mut source = InstanceState::default();
        source.core.commit = Some("bbbb".to_string());
        source.core.worktree_hash = "h2".to_string();
        source.python.version = "3.13.6".to_string();
        source
            .python
            .packages
            .insert("torch".to_string(), "2.9.0".to_string());
        source.nodes.push(SyncNodeState {
            name: "node-a".to_string(),
            enabled: true,
            worktree_hash: "n2".to_string(),
            ..Default::default()
        });
        source.models.push(SyncModelFile {
            file_id: "m1".to_string(),
            relative_path: "checkpoints/a.safetensors".to_string(),
            size: 10,
            sha256: Some("hash".to_string()),
            ..Default::default()
        });
        let target = InstanceState::default();
        let diff = diff_states_with_scope(
            &source,
            &target,
            SyncScope {
                comfyui: true,
                nodes: false,
                python: false,
                models: false,
            },
        );
        assert!(diff.core.changed);
        assert!(diff.python.packages_added.is_empty());
        assert!(diff.nodes.added.is_empty());
        assert!(diff.models.added.is_empty());
    }

    #[test]
    fn scoped_fingerprint_ignores_unselected_groups() {
        let mut target = InstanceState::default();
        target.core.commit = Some("aaaa".to_string());
        target.models.push(SyncModelFile {
            file_id: "m1".to_string(),
            relative_path: "checkpoints/a.safetensors".to_string(),
            size: 10,
            sha256: Some("hash".to_string()),
            ..Default::default()
        });
        let only_core = SyncScope {
            comfyui: true,
            nodes: false,
            python: false,
            models: false,
        };
        let before = state_fingerprint_scoped(&target, only_core);
        target.models[0].sha256 = Some("changed-after-confirm".to_string());
        assert_eq!(before, state_fingerprint_scoped(&target, only_core));
        assert_ne!(before, state_fingerprint_scoped(&target, SyncScope::all()));
    }

    #[test]
    fn launch_args_are_diffed_and_in_fingerprint() {
        let mut source = InstanceState::default();
        source.server = crate::config::ServerOpts {
            listen: "0.0.0.0".to_string(),
            port: 8288,
            cors_origin: Some("http://example.test".to_string()),
            extra_args: vec!["--preview-method".to_string(), "auto".to_string()],
        };
        let mut target = InstanceState::default();
        let diff = diff_states(&source, &target);
        assert!(diff.launch.changed);
        assert!(diff.needs_apply);
        assert_eq!(diff.launch.from_port, 0);
        assert_eq!(diff.launch.to_port, 8288);
        assert_eq!(diff.launch.to_extra_args.len(), 2);

        let before = state_fingerprint(&target);
        target.server = source.server.clone();
        assert_ne!(state_fingerprint(&target), before);
        let same = diff_states(&source, &target);
        assert!(!same.launch.changed);
        assert!(!same.needs_apply);
    }

    #[test]
    fn every_directory_launch_flag_is_local() {
        for line in [
            "--models-directory D:\\models",
            "--input-directory D:\\input",
            "--output-directory D:\\output",
            "--base-directory D:\\base",
            "--temp-directory D:\\temp",
            "--user-directory D:\\user",
            "--front-end-root D:\\web",
            "--extra-model-paths-config D:\\extra.yaml",
            "--models-directory=D:\\models",
            "--output-directory=D:\\out dir",
        ] {
            assert!(line_has_local_path_arg(line), "应识别为目录参数，{line}");
        }
        for line in [
            "--preview-method auto",
            "--lowvram",
            "--listen 0.0.0.0",
            "--port 9010",
            "--disable-smart-memory",
        ] {
            assert!(!line_has_local_path_arg(line), "不应识别为目录参数：{line}");
        }
    }

    #[test]
    fn launch_directory_args_stay_on_target_machine() {
        let mut source = InstanceState::default();
        source.server.extra_args = vec![
            "--models-directory D:\\source-models".to_string(),
            "--preview-method auto".to_string(),
        ];
        let mut target = InstanceState::default();
        target.server.extra_args = vec![
            "--models-directory D:\\target-models".to_string(),
            "--lowvram".to_string(),
        ];

        let diff = diff_states(&source, &target);
        assert!(diff.launch.changed, "普通参数不同仍然要同步");
        assert_eq!(diff.launch.to_extra_args, vec!["--preview-method auto"]);
        assert_eq!(diff.launch.from_extra_args, vec!["--lowvram"]);
        assert_eq!(
            diff.launch.to_local_extra_args,
            vec!["--models-directory D:\\source-models"]
        );
        assert_eq!(
            diff.launch.from_local_extra_args,
            vec!["--models-directory D:\\target-models"]
        );

        let merged = merge_launch_server(&source.server, &target.server);
        assert_eq!(
            merged.extra_args,
            vec![
                "--preview-method auto",
                "--models-directory D:\\target-models"
            ],
            "源端目录被丢弃，目标目录原样保留"
        );

        // 只有目录参数不同，不能算一次同步变化。        target.server.port = source.server.port;
        target.server.listen = source.server.listen.clone();
        target.server.cors_origin = source.server.cors_origin.clone();
        target.server.extra_args = vec![
            "--preview-method auto".to_string(),
            "--models-directory D:\\another-target-models".to_string(),
        ];
        let only_local = diff_states(&source, &target);
        assert!(!only_local.launch.changed);
        assert!(!only_local.needs_apply);
    }

    #[test]
    fn node_archive_keeps_git_only_when_requested() {
        let root = temp_dir("node-git");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git").join("config"), "git").unwrap();
        fs::write(root.join("node.py"), "print(1)").unwrap();
        let with_git = root.join("with-git.zip");
        zip_dir(&root, &with_git, true).unwrap();
        let out = root.join("out1");
        extract_zip(&with_git, &out).unwrap();
        assert!(out.join(".git").join("config").is_file());
        assert!(out.join("node.py").is_file());
        let without_git = root.join("without-git.zip");
        zip_dir(&root, &without_git, false).unwrap();
        let out2 = root.join("out2");
        extract_zip(&without_git, &out2).unwrap();
        assert!(!out2.join(".git").exists());
        assert!(out2.join("node.py").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn store_id_is_stable_and_not_path_host_specific() {
        let one = temp_dir("store-one");
        let two = temp_dir("store-two");
        let a = store_id(&one);
        let b = store_id(&one);
        let c = store_id(&two);
        assert!(a.starts_with("store-"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(same_model_store(&a, &b));
        // 旧版本的路径 ID 即使相同，也不能当成同一个物理库。
        assert!(!same_model_store("path-abc", "path-abc"));
        assert!(one.join(STORE_MARKER).is_file());
        let _ = fs::remove_dir_all(&one);
        let _ = fs::remove_dir_all(&two);
    }

    #[test]
    fn clear_core_keeps_user_directories() {
        let root = temp_dir("clear-core");
        fs::create_dir_all(root.join("custom_nodes").join("n")).unwrap();
        fs::create_dir_all(root.join("models")).unwrap();
        fs::write(root.join("custom_nodes").join("n").join("a.py"), "node").unwrap();
        fs::write(root.join("models").join("m.bin"), "model").unwrap();
        fs::write(root.join("extra_model_paths.yaml"), "paths").unwrap();
        fs::write(root.join("stale_module.py"), "stale").unwrap();
        fs::write(root.join("main.py"), "main").unwrap();
        clear_core_except(&root).unwrap();
        assert!(root.join("custom_nodes").join("n").join("a.py").is_file());
        assert!(root.join("models").join("m.bin").is_file());
        assert!(root.join("extra_model_paths.yaml").is_file());
        assert!(!root.join("stale_module.py").exists());
        assert!(!root.join("main.py").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn download_stops_when_cancelled() {
        use std::net::TcpListener;
        use std::sync::Arc;

        let root = temp_dir("download-cancel");
        let dest = root.join("big.bin");
        let cancel = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 6553600\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for _ in 0..100 {
                if stream.write_all(&[7u8; 65536]).is_err() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            flag.store(true, Ordering::Relaxed);
        });
        let result = download_file_with_progress(
            &format!("http://{addr}"),
            "",
            "/api/v1/model/big",
            &dest,
            None,
            Some(&cancel),
            None,
        );
        assert!(result.is_err());
        assert!(!dest.exists());
        let _ = server.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn redacts_url_credentials_without_changing_repository_path() {
        assert_eq!(
            redact_url("https://user:ghp_secret@github.com/a/b.git?token=x&ref=main#readme"),
            "https://***@github.com/a/b.git?token=***&ref=main#readme"
        );
        assert_eq!(
            redact_url("git@github.com:a/b.git"),
            "git@github.com:a/b.git"
        );
        assert_eq!(
            redact_url("https://github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
    }

    #[test]
    fn download_does_not_follow_redirect_with_member_token() {
        use std::net::TcpListener;

        let root = temp_dir("download-redirect");
        let dest = root.join("model.bin");
        let sink = TcpListener::bind("127.0.0.1:0").unwrap();
        sink.set_nonblocking(true).unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_addr = redirect.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            assert!(request.contains("x-comfyfleet-token: secret"), "{request}");
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{sink_addr}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let result = download_file_with_progress(
            &format!("http://{redirect_addr}"),
            "secret",
            "/api/v1/model/test",
            &dest,
            None,
            None,
            None,
        );
        assert!(result.is_err(), "3xx 必须按错误处理");
        assert!(
            sink.accept().is_err(),
            "带令牌的下载请求不能跟随重定向到另一台主机"
        );
        let _ = server.join();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn download_resumes_partial_file() {
        use sha2::{Digest, Sha256};
        use std::net::TcpListener;

        let root = temp_dir("download-resume");
        let dest = root.join("model.bin");
        let body = b"abcdefghij";
        fs::write(dest.with_extension("bin.part"), &body[..3]).unwrap();
        let expected = hex::encode(Sha256::digest(body));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            assert!(request.contains("range: bytes=3-"), "{request}");
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes 3-9/10\r\nConnection: close\r\n\r\n",
                body.len() - 3
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body[3..]).unwrap();
        });
        download_file_with_progress(
            &format!("http://{addr}"),
            "",
            "/api/v1/model/test",
            &dest,
            Some(&expected),
            None,
            None,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(!dest.with_extension("bin.part").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_models_downloads_directly_to_store_even_if_marker_matches() {
        use sha2::{Digest, Sha256};
        use std::net::TcpListener;

        let root = temp_dir("model-direct");
        let body = b"direct-model-download".to_vec();
        let expected = hex::encode(Sha256::digest(&body));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = body.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&payload).unwrap();
        });

        let mut storage = config::InstanceStorage::default();
        storage.use_shared_models = false;
        let inst = InstanceConfig {
            id: "target".to_string(),
            name: "target".to_string(),
            root_dir: root.to_string_lossy().to_string(),
            engine: String::new(),
            engine_detail: None,
            python_version: "3.13".to_string(),
            python: config::PythonEnv::default(),
            comfy: config::ComfyRef {
                channel: "stable".to_string(),
                reference: String::new(),
                commit: String::new(),
                version: None,
            },
            server: ServerOpts::default(),
            storage,
            env: BTreeMap::new(),
            overrides: config::InstanceOverrides {
                pypi: None,
                git: None,
                hf: None,
                hf_offline: None,
            },
            node_deps_auto: None,
            auto_restart: false,
            kind: "local".to_string(),
            remote: None,
            imported: false,
            state: config::InstanceState::Stopped,
            created_at: String::new(),
            last_started_at: None,
        };
        let mut job = SyncJob {
            id: "model-direct-job".to_string(),
            generation_id: "gen".to_string(),
            ..Default::default()
        };
        job.diff.models.added.push(ModelChange {
            kind: "checkpoints".to_string(),
            relative_path: "checkpoints/direct.bin".to_string(),
            size: body.len() as u64,
            ..Default::default()
        });
        job.diff.models.transfer_bytes = body.len() as u64;
        let mut manifest = SyncManifest {
            generation_id: "gen".to_string(),
            ..Default::default()
        };
        // 目录被整份复制时 marker id 也会相同；目标文件缺失时仍必须传输，
        // 不能因为“看起来是同一个库”就直接跳过。
        let target_store = model_store(&inst, &GlobalSettings::default());
        manifest.source.model_store_id = store_id(&target_store);
        assert!(same_model_store(
            &manifest.source.model_store_id,
            &store_id(&target_store)
        ));
        manifest.source.models.push(SyncModelFile {
            file_id: "file".to_string(),
            kind: "checkpoints".to_string(),
            relative_path: "checkpoints/direct.bin".to_string(),
            size: body.len() as u64,
            sha256: Some(expected.clone()),
            source_path: None,
        });
        let mut progress = Vec::new();
        let count = apply_models(
            &job,
            &job.diff,
            &manifest,
            &inst,
            &GlobalSettings::default(),
            &SyncOptions::default(),
            &format!("http://{addr}"),
            "",
            &AtomicBool::new(false),
            &mut |pct, message| progress.push((pct, message.to_string())),
        )
        .unwrap();
        server.join().unwrap();
        let dest = root.join("ComfyUI/models/checkpoints/direct.bin");
        assert_eq!(count, 1);
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert!(progress
            .iter()
            .any(|(_, message)| message.contains("下载模型")));
        assert_eq!(
            hash_cache()
                .lock()
                .get(&file_cache_key(&dest).unwrap())
                .map(String::as_str),
            Some(expected.as_str())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn download_reports_progress_and_caches_hash() {
        use sha2::{Digest, Sha256};
        use std::net::TcpListener;

        let root = temp_dir("download-progress");
        let dest = root.join("model.bin");
        let body = vec![9u8; 300_000];
        let expected = hex::encode(Sha256::digest(&body));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = body.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&payload).unwrap();
        });
        let mut seen = Vec::new();
        download_file_with_progress(
            &format!("http://{addr}"),
            "",
            "/api/v1/model/progress",
            &dest,
            Some(&expected),
            None,
            Some(&mut |written, total| seen.push((written, total))),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(
            seen.last().copied(),
            Some((body.len() as u64, body.len() as u64))
        );
        let key = file_cache_key(&dest).unwrap();
        assert_eq!(
            hash_cache().lock().get(&key).map(String::as_str),
            Some(expected.as_str())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_paths_reject_traversal() {
        assert!(safe_relative_path("checkpoints/a.bin"));
        assert!(!safe_relative_path("../outside.bin"));
        assert!(!safe_relative_path("checkpoints/../../outside.bin"));
        assert!(!safe_relative_path("C:\\Windows\\evil.bin"));
        assert!(!safe_relative_path(""));
        assert!(!safe_relative_path("models/NUL"));
        assert!(!safe_relative_path("models/CON.txt"));
        assert!(!safe_relative_path("models/trailing."));
        assert!(!safe_relative_path("models/trailing "));
        assert!(!safe_relative_path("models/ads:payload"));
        assert!(!safe_relative_path("."));
    }

    #[test]
    fn node_names_allow_registry_suffixes_but_not_paths() {
        assert!(safe_name("comfyui_custom_nodes_alekpet@nightly"));
        assert!(safe_name("ComfyUI-Impact-Pack"));
        assert!(safe_name("节点 包 (test)"));
        assert!(safe_relative_path(
            "nodes/comfyui_custom_nodes_alekpet@nightly.zip"
        ));
        assert!(!safe_name("../outside"));
        assert!(!safe_name("a/b"));
        assert!(!safe_name("a\\b"));
        assert!(!safe_name("con"));
        assert!(!safe_name("CON.txt"));
        assert!(!safe_name("name."));
        assert!(!safe_name("name "));
    }

    #[test]
    fn local_wheels_are_resolved_from_the_configured_library() {
        let root = temp_dir("meta-wheels");
        let lib = root.join("wheels");
        fs::create_dir_all(&lib).unwrap();
        let stored = lib.join("demo-1.0.0-py3-none-any.whl");
        fs::write(&stored, b"fake-wheel").unwrap();
        let stored_str = stored.to_string_lossy().to_string();
        let editable = root.join("project");
        fs::create_dir_all(&editable).unwrap();
        let file_url =
            |path: &Path| format!("file:///{}", path.to_string_lossy().replace('\\', "/"));
        let meta = serde_json::json!({
            "packages": [
                // 原始下载目录已经清掉了，但 wheel 库里放了同名文件。
                {"name": "demo", "version": "1.0.0", "directUrl": {"url": file_url(&root.join("downloads").join("demo-1.0.0-py3-none-any.whl"))}},
                {"name": "gone", "version": "2.0", "directUrl": {"url": "file:///Z:/definitely-missing-x.whl"}},
                {"name": "remote", "version": "3.0", "directUrl": {"url": "https://example.com/remote-3.0-py3-none-any.whl"}},
                {"name": "editable-demo", "version": "4.0", "directUrl": {"url": file_url(&editable), "dir_info": {"editable": true}}},
                {"name": "normal", "version": "5.0"}
            ]
        });
        let wheels = custom_wheels_from_meta(&meta, Some(&lib));
        assert_eq!(wheels.len(), 4);
        let demo = wheels.iter().find(|w| w.name == "demo").unwrap();
        assert!(demo.available, "库里有同名文件就该用上");
        assert_eq!(demo.source, "local-wheel");
        assert_eq!(demo.filename, "demo-1.0.0-py3-none-any.whl");
        assert_eq!(demo.source_ref.as_deref(), Some(stored_str.as_str()));
        let gone = wheels.iter().find(|w| w.name == "gone").unwrap();
        assert!(!gone.available);
        assert!(gone
            .missing_reason
            .as_deref()
            .unwrap_or("")
            .contains(&lib.to_string_lossy().to_string()));
        let remote = wheels.iter().find(|w| w.name == "remote").unwrap();
        assert!(remote.available);
        assert_eq!(remote.source, "remote-url");
        let edit = wheels.iter().find(|w| w.name == "editable-demo").unwrap();
        assert!(edit.available);
        assert_eq!(edit.source, "editable");

        // 没设置 wheel 库目录：同样是“缺”，但要提示先去设置里指定。
        let no_lib = custom_wheels_from_meta(&meta, None);
        let gone = no_lib.iter().find(|w| w.name == "gone").unwrap();
        assert!(!gone.available);
        assert!(gone
            .missing_reason
            .as_deref()
            .unwrap_or("")
            .contains("本地 Wheel 库"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_local_wheel_uses_recorded_origin_url() {
        let meta = serde_json::json!({
            "packages": [{
                "name": "xformers",
                "version": "0.0.34+5f95c7af.d20260113",
                "directUrl": {
                    "url": "file:///Z:/deleted-cache/xformers-0.0.34-cp39-abi3-win_amd64.whl",
                    "comfyfleet_origin": {
                        "url": "https://example.com/xformers-0.0.34+torch2.10cu130-cp39-abi3-win_amd64.whl"
                    }
                }
            }]
        });
        let wheels = custom_wheels_from_meta(&meta, None);
        assert_eq!(wheels.len(), 1);
        let wheel = &wheels[0];
        assert!(wheel.available);
        assert_eq!(wheel.source, "remote-url");
        assert_eq!(
            wheel.source_ref.as_deref(),
            Some("https://example.com/xformers-0.0.34+torch2.10cu130-cp39-abi3-win_amd64.whl")
        );
        assert!(wheel
            .missing_reason
            .as_deref()
            .unwrap_or("")
            .contains("URL"));
    }

    #[test]
    fn url_installed_cached_wheel_keeps_url_source() {
        let root = temp_dir("meta-url-cached");
        let lib = root.join("wheels");
        fs::create_dir_all(&lib).unwrap();
        let stored = lib.join("xformers-0.0.35-py39-none-win_amd64.whl");
        fs::write(&stored, b"fake-wheel").unwrap();
        let file_url = format!("file:///{}", stored.to_string_lossy().replace('\\', "/"));
        let origin =
            "https://example.com/xformers-0.0.35+cu132torch2.12.1cxx11abi1-py39-none-win_amd64.whl";
        let meta = serde_json::json!({
            "packages": [{
                "name": "xformers",
                "version": "0.0.35+cu132torch2.12.1cxx11abi1",
                "directUrl": {
                    "url": file_url,
                    "comfyfleet_origin": { "url": origin }
                }
            }]
        });
        let wheels = custom_wheels_from_meta(&meta, Some(&lib));
        let wheel = wheels.iter().find(|w| w.name == "xformers").unwrap();
        assert_eq!(wheel.source, "remote-url");
        assert_eq!(wheel.source_ref.as_deref(), Some(origin));
        assert!(wheel.archive.is_none(), "URL 安装的 wheel 不应再带本地文件");
        assert!(wheel.available);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn custom_wheels_handle_git_and_local_directory_installs() {
        let root = temp_dir("meta-vcs");
        let local = root.join("cstr");
        fs::create_dir_all(&local).unwrap();
        let file_url =
            |path: &Path| format!("file:///{}", path.to_string_lossy().replace('\\', "/"));
        let meta = serde_json::json!({
            "packages": [
                {
                    "name": "ffmpy",
                    "version": "1.0.0",
                    "directUrl": {
                        "url": "https://github.com/example/ffmpy.git",
                        "vcs_info": {
                            "vcs": "git",
                            "commit_id": "0123456789abcdef",
                            "requested_revision": "main"
                        }
                    }
                },
                {
                    "name": "cstr",
                    "version": "0.2.0",
                    "directUrl": {
                        "url": file_url(&local),
                        "dir_info": {}
                    }
                },
                {
                    "name": "editme",
                    "version": "0.3.0",
                    "directUrl": {
                        "url": file_url(&local),
                        "dir_info": {"editable": true}
                    }
                }
            ]
        });
        let wheels = custom_wheels_from_meta(&meta, None);
        let git = wheels.iter().find(|w| w.name == "ffmpy").unwrap();
        assert_eq!(git.source, "vcs");
        assert!(git.available);
        assert_eq!(git.commit.as_deref(), Some("0123456789abcdef"));
        assert_eq!(
            vcs_install_spec(
                git.source_ref.as_deref().unwrap(),
                git.commit.as_deref().unwrap()
            )
            .unwrap(),
            "git+https://github.com/example/ffmpy.git@0123456789abcdef"
        );
        assert!(!git.sha256.is_empty());

        let dir = wheels.iter().find(|w| w.name == "cstr").unwrap();
        assert_eq!(dir.source, "local-dir");
        assert!(dir.available);
        assert_eq!(dir.filename, "cstr.zip");

        let edit = wheels.iter().find(|w| w.name == "editme").unwrap();
        assert_eq!(edit.source, "editable");
        assert!(edit.available);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unchanged_vcs_wheel_does_not_show_as_changed() {
        let mut source = InstanceState::default();
        source.python.custom_wheels.push(CustomWheel {
            name: "ffmpy".into(),
            version: "1.0.0".into(),
            filename: "ffmpy.git".into(),
            sha256: "stable-vcs-sha".into(),
            source: "vcs".into(),
            available: true,
            source_ref: Some("https://example.com/ffmpy.git".into()),
            vcs: Some("git".into()),
            commit: Some("0123456789abcdef".into()),
            missing_reason: Some("Git 仓库安装".into()),
            ..Default::default()
        });
        let mut target = source.clone();
        target.python.custom_wheels[0].available = true;
        let diff = diff_states(&source, &target);
        assert!(diff.python.wheels_changed.is_empty());
        assert!(diff.python.wheels_added.is_empty());
        assert!(diff.python.missing_wheels.is_empty());
    }

    #[test]
    fn wheel_source_disappearing_is_not_a_content_change() {
        let mut source = InstanceState::default();
        source.python.custom_wheels.push(CustomWheel {
            name: "demo-pkg".into(),
            version: "1.0.0".into(),
            filename: "demo_pkg-1.0.0-py3-none-any.whl".into(),
            sha256: "a".repeat(64),
            source: "local-wheel".into(),
            available: true,
            ..Default::default()
        });
        let mut target = source.clone();
        // 目标端安装后原始 wheel 被清理：来源变成 missing、available=false，但内容哈希还在。
        target.python.custom_wheels[0].source = "missing".into();
        target.python.custom_wheels[0].available = false;
        target.python.custom_wheels[0].missing_reason = Some("本地文件已不存在".into());
        let diff = diff_states(&source, &target);
        assert!(diff.python.wheels_changed.is_empty());
        assert!(diff.python.wheels_added.is_empty());
        assert!(diff.python.wheels_removed.is_empty());

        // 目标端连原始文件名都记不下来时，也不能因为“没有哈希”就报变化。        target.python.custom_wheels[0].sha256.clear();
        target.python.custom_wheels[0].filename = "cached.whl".into();
        let diff = diff_states(&source, &target);
        assert!(diff.python.wheels_changed.is_empty());
    }

    #[test]
    fn wheel_name_separators_are_canonical_and_missing_hash_is_conservative() {
        let mut source = InstanceState::default();
        source.python.custom_wheels.push(CustomWheel {
            name: "demo-pkg".into(),
            version: "1.0.0".into(),
            filename: "demo_pkg-1.0.0-py3-none-any.whl".into(),
            source: "local-wheel".into(),
            ..Default::default()
        });
        let mut target = source.clone();
        target.python.custom_wheels[0].name = "demo_pkg".into();
        target.python.custom_wheels[0].source = "missing".into();
        let diff = diff_states(&source, &target);
        assert!(diff.python.wheels_changed.is_empty());
        assert!(diff.python.wheels_added.is_empty());
        assert!(diff.python.wheels_removed.is_empty());

        // 同名版本但文件名不同（例如 cp312 → cp313），没有哈希也要报变化。
        target.python.custom_wheels[0].filename = "demo_pkg-1.0.0-cp313-cp313-win_amd64.whl".into();
        let diff = diff_states(&source, &target);
        assert_eq!(diff.python.wheels_changed.len(), 1);
    }

    #[test]
    fn direct_url_archive_hash_is_kept_when_file_is_gone() {
        let meta = serde_json::json!({
            "packages": [{
                "name": "demo",
                "version": "1.0.0",
                "directUrl": {
                    "url": "https://example.com/demo-1.0.0-py3-none-any.whl",
                    "archive_info": {"hashes": {"sha256": "ABCDEF"}}
                }
            }]
        });
        let wheels = custom_wheels_from_meta(&meta, None);
        assert_eq!(wheels.len(), 1);
        assert_eq!(wheels[0].sha256, "abcdef");
        assert_eq!(wheels[0].source, "remote-url");
    }

    fn wheel_for(filename: &str) -> CustomWheel {
        CustomWheel {
            name: "demo".to_string(),
            version: "1.0".to_string(),
            filename: filename.to_string(),
            source: "local-wheel".to_string(),
            available: true,
            ..Default::default()
        }
    }

    fn source_python() -> SyncPythonState {
        SyncPythonState {
            version: "3.13.6".to_string(),
            short_version: "3.13".to_string(),
            abi: "cp313-win_amd64".to_string(),
            platform: "win-amd64".to_string(),
            torch: Some("2.10.0+cu130".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn wheel_compat_uses_source_python_abi3_and_platform() {
        let py = source_python();
        assert!(
            wheel_compat_reason(&wheel_for("xformers-0.0.34-cp39-abi3-win_amd64.whl"), &py)
                .is_none()
        );
        assert!(wheel_compat_reason(&wheel_for("demo-1.0-py3-none-any.whl"), &py).is_none());
        let wrong_py =
            wheel_compat_reason(&wheel_for("demo-1.0-cp312-cp312-win_amd64.whl"), &py).unwrap();
        assert!(wrong_py.contains("Python"));
        let wrong_platform =
            wheel_compat_reason(&wheel_for("demo-1.0-cp313-cp313-linux_x86_64.whl"), &py).unwrap();
        assert!(wrong_platform.contains("平台"));
        assert!(wheel_compat_reason(&wheel_for("demo-1.0.tar.gz"), &py).is_none());
    }

    #[test]
    fn incompatible_available_wheels_are_not_reported_as_missing() {
        let mut source = InstanceState::default();
        let mut wheel = wheel_for("llama_cpp_python-0.3.46+cu128-cp313-cp313-win_amd64.whl");
        wheel.archive = Some("env/wheels/llama.whl".to_string());
        wheel.incompatible = true;
        wheel.compat_warning = Some("轮子要求 CUDA 128，源客户端是 130".to_string());
        source.python.custom_wheels = vec![wheel];
        let diff = diff_states(&source, &InstanceState::default());
        assert!(diff.python.missing_wheels.is_empty());
        assert_eq!(diff.python.incompatible_wheels.len(), 1);
        assert_eq!(diff.python.wheels_added.len(), 1);
    }

    #[test]
    fn incompatible_wheels_are_flagged_but_still_stay_available() {
        let mut py = source_python();
        let mut wheel = wheel_for("demo-1.0-cp312-cp312-win_amd64.whl");
        wheel.archive = Some("env/wheels/demo-1.0-cp312-cp312-win_amd64.whl".to_string());
        py.custom_wheels = vec![wheel];
        mark_incompatible_wheels(&mut py);
        assert!(py.custom_wheels[0].available);
        assert!(py.custom_wheels[0].incompatible);
        assert!(py.custom_wheels[0].archive.is_some());
        assert!(py.custom_wheels[0]
            .compat_warning
            .as_deref()
            .unwrap_or("")
            .contains("Python"));
    }

    #[test]
    fn wheel_compat_rejects_torch_and_cuda_from_source_env() {
        let py = source_python();
        let wrong_torch = wheel_compat_reason(
            &wheel_for("flash_attn-2.8.2+torch2.9-cp313-cp313-win_amd64.whl"),
            &py,
        )
        .unwrap();
        assert!(wrong_torch.contains("torch"));
        let wrong_cuda = wheel_compat_reason(
            &wheel_for("flash_attn-2.8.2+torch2.10cu128-cp313-cp313-win_amd64.whl"),
            &py,
        )
        .unwrap();
        assert!(wrong_cuda.contains("CUDA"));
        assert!(wheel_compat_reason(
            &wheel_for("flash_attn-2.8.2+torch2.10cu130-cp313-cp313-win_amd64.whl"),
            &py
        )
        .is_none());
        // 只要轮子带 CUDA 标签，就必须与源客户端 CUDA 一致，包括
        // llama-cpp-python 这种没有 torch 标签的独立扩展。
        let llama_cuda = wheel_compat_reason(
            &wheel_for("llama_cpp_python-0.3.46+cu128-cp313-cp313-win_amd64.whl"),
            &py,
        )
        .unwrap();
        assert!(llama_cuda.contains("CUDA 128"));
    }

    #[cfg(windows)]
    #[test]
    fn free_space_reports_volume_capacity() {
        let dir = temp_dir("free-space");
        assert!(free_space(&dir) > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn core_rollback_restores_code_on_failure() {
        let root = temp_dir("rollback");
        let comfy = root.join("ComfyUI");
        fs::create_dir_all(&comfy).unwrap();
        fs::write(comfy.join("main.py"), "broken-new").unwrap();
        let source = root.join("backup-src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("main.py"), "good-old").unwrap();
        let backup_dir = root.join("backups").join("job1");
        fs::create_dir_all(&backup_dir).unwrap();
        zip_dir_ex(
            &source,
            &backup_dir.join("ComfyUI-core-before.zip"),
            false,
            CORE_EXCLUDE,
        )
        .unwrap();
        let rollback = CoreRollback {
            comfy: comfy.clone(),
            backup_dir,
            armed: true,
        };
        drop(rollback);
        assert_eq!(
            fs::read_to_string(comfy.join("main.py")).unwrap(),
            "good-old"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn torch_index_falls_back_to_local_cuda_label() {
        let mut py = SyncPythonState::default();
        py.packages
            .insert("torch".to_string(), "2.14.1+cu130".to_string());
        assert_eq!(
            torch_index_fallback(&py).as_deref(),
            Some("https://download.pytorch.org/whl/cu130/")
        );
        py.packages
            .insert("torch".to_string(), "2.14.1".to_string());
        assert_eq!(torch_index_fallback(&py), None);
        py.torch = Some("2.14.1+cu128".to_string());
        assert_eq!(
            torch_index_fallback(&py).as_deref(),
            Some("https://download.pytorch.org/whl/cu128/")
        );
    }

    #[test]
    fn venv_swap_rolls_back_and_commits_cleanly() {
        let root = temp_dir("venv-swap");
        let venv = root.join(".venv");
        let old = root.join(".venv-comfyfleet-swap-test");
        fs::create_dir_all(&venv).unwrap();
        fs::write(venv.join("marker"), "new").unwrap();
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("marker"), "old").unwrap();
        {
            let _swap = VenvSwap::new(venv.clone(), Some(old.clone()), 0);
        }
        assert!(venv.join("marker").exists());
        assert_eq!(fs::read_to_string(venv.join("marker")).unwrap(), "old");
        assert!(!old.exists());

        fs::remove_dir_all(&venv).unwrap();
        fs::create_dir_all(&venv).unwrap();
        fs::write(venv.join("marker"), "newer").unwrap();
        fs::create_dir_all(&old).unwrap();
        let mut swap = VenvSwap::new(venv.clone(), Some(old.clone()), 7);
        swap.commit();
        assert_eq!(fs::read_to_string(venv.join("marker")).unwrap(), "newer");
        assert!(!old.exists());
        let retained = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".venv-comfyfleet-old-")
            })
            .count();
        assert_eq!(retained, 1, "旧 venv 应保留到过期为止");

        // 按目录名时间戳判断保留期：旧环境本身很老，也不应该在刚保留时被删。
        let expired = root.join(".venv-comfyfleet-old-20000101-000000-deadbeef");
        fs::create_dir_all(&expired).unwrap();
        prune_old_venvs(&venv, 7);
        assert!(!expired.exists());
        assert_eq!(
            fs::read_dir(&root)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".venv-comfyfleet-old-")
                })
                .count(),
            1,
            "7 天内的旧环境不能被时间戳误删"
        );

        // 保留天数设为 0 时，下一次同步应把这些旧目录全部清掉。
        let stale = root.join(".venv-comfyfleet-old-stale");
        fs::create_dir_all(&stale).unwrap();
        prune_old_venvs(&venv, 0);
        assert!(!root.join(".venv-comfyfleet-old-stale").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_to_backup_handles_nested_venv_tree() {
        let root = temp_dir("move-backup");
        let from = root.join("instance").join(".venv");
        fs::create_dir_all(from.join("Lib").join("site-packages")).unwrap();
        fs::write(from.join("pyvenv.cfg"), "home = python").unwrap();
        fs::write(from.join("Lib").join("site-packages").join("a.py"), "x").unwrap();
        let to = root
            .join("appdata")
            .join("backups")
            .join("job")
            .join(".venv-1");
        move_to_backup(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(
            fs::read_to_string(to.join("pyvenv.cfg")).unwrap(),
            "home = python"
        );
        assert_eq!(
            fs::read_to_string(to.join("Lib").join("site-packages").join("a.py")).unwrap(),
            "x"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_to_backup_copies_across_drives() {
        let from = temp_dir("cross-drive-source");
        fs::create_dir_all(from.join("Lib")).unwrap();
        fs::write(from.join("pyvenv.cfg"), "cross").unwrap();
        fs::write(from.join("Lib").join("x.txt"), "y").unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let to = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("cross-drive-backup-{stamp}"));
        let from_drive = from.components().next();
        let to_drive = to.components().next();
        if from_drive == to_drive {
            // 临时目录和target 在同一个盘时走的是 rename，测不到跨盘回退。
            let _ = fs::remove_dir_all(&from);
            return;
        }
        move_to_backup(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(to.join("pyvenv.cfg")).unwrap(), "cross");
        assert_eq!(
            fs::read_to_string(to.join("Lib").join("x.txt")).unwrap(),
            "y"
        );
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn copy_dir_recursive_preserves_tree() {
        let root = temp_dir("copy-tree");
        let from = root.join("from");
        let to = root.join("to");
        fs::create_dir_all(from.join("x")).unwrap();
        fs::write(from.join("x").join("a.txt"), "a").unwrap();
        copy_dir_recursive(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join("x").join("a.txt")).unwrap(), "a");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drift_confirmation_checks_both_preview_and_last_applied() {
        let last = LastApplied {
            fingerprint: "v2:old".into(),
            ..Default::default()
        };
        assert!(!target_drift_requires_confirmation(
            "v2:new",
            "v2:new",
            Some(&last),
            true
        ));
        assert!(!target_drift_requires_confirmation(
            "v2:new", "v2:new", None, false
        ));
        assert!(target_drift_requires_confirmation(
            "v2:new",
            "v2:changed",
            None,
            false
        ));
        assert!(target_drift_requires_confirmation(
            "v2:new",
            "v2:new",
            Some(&last),
            false
        ));
        // 旧版本无版本前缀的指纹也必须先确认一次。
        let legacy = LastApplied {
            fingerprint: "not-versioned".into(),
            ..Default::default()
        };
        assert!(target_drift_requires_confirmation(
            "v2:new",
            "v2:new",
            Some(&legacy),
            false
        ));
    }

    #[test]
    fn artifact_sha_covers_wheel_and_project() {
        let mut manifest = SyncManifest::default();
        manifest.source.python.custom_wheels.push(CustomWheel {
            name: "demo".into(),
            filename: "demo-1.0.0-py3-none-any.whl".into(),
            archive: Some("env/wheels/demo-1.0.0-py3-none-any.whl".into()),
            sha256: "wheel-sha".into(),
            ..Default::default()
        });
        manifest.source.python.custom_wheels.push(CustomWheel {
            name: "editme".into(),
            archive: Some("env/projects/editme.zip".into()),
            sha256: "project-sha".into(),
            ..Default::default()
        });
        assert_eq!(
            artifact_sha(&manifest, "env/wheels/demo-1.0.0-py3-none-any.whl").as_deref(),
            Some("wheel-sha")
        );
        assert_eq!(
            artifact_sha(&manifest, "env/projects/editme.zip").as_deref(),
            Some("project-sha")
        );
        assert_eq!(artifact_sha(&manifest, "nodes/unknown.zip"), None);
    }

    #[test]
    fn fingerprint_is_versioned() {
        let state = InstanceState::default();
        assert!(state_fingerprint(&state).starts_with(FINGERPRINT_VERSION));
    }

    #[test]
    fn preparation_fingerprint_ignores_runtime_but_tracks_content_and_scope() {
        let settings = GlobalSettings::default();
        let mut a = InstanceState {
            instance_id: "source-a".to_string(),
            name: "源 A".to_string(),
            root_dir: r"D:\ComfyUI".to_string(),
            ..Default::default()
        };
        let all = preparation_fingerprint(&a, SyncScope::all(), &settings);
        let node_scope = SyncScope {
            comfyui: false,
            python: false,
            nodes: true,
            models: false,
        };
        let nodes = preparation_fingerprint(&a, node_scope, &settings);
        assert_ne!(all, nodes, "不同同步范围不能复用同一份代次");

        // 只同步节点时，模型变化不应该让节点准备数据失效。
        a.models.push(SyncModelFile {
            file_id: "model".to_string(),
            kind: "checkpoints".to_string(),
            relative_path: "checkpoints/a.bin".to_string(),
            size: 1,
            ..Default::default()
        });
        assert_eq!(nodes, preparation_fingerprint(&a, node_scope, &settings));
        a.models.clear();

        // 运行状态、磁盘余量、上次同步时间不是内容变化。
        a.state = "running".to_string();
        a.disk_free = 1;
        a.last_applied = Some(LastApplied {
            generation_id: "old".to_string(),
            applied_at: now(),
            ..Default::default()
        });
        assert_eq!(
            all,
            preparation_fingerprint(&a, SyncScope::all(), &settings)
        );
        let baseline = InstanceState {
            state: "stopped".to_string(),
            disk_free: 0,
            last_applied: None,
            ..a.clone()
        };

        // 自定义 wheel 的内容变化必须重新准备。
        a.python.custom_wheels.push(CustomWheel {
            name: "demo".to_string(),
            version: "1.0".to_string(),
            filename: "demo-1.0.0-py3-none-any.whl".to_string(),
            sha256: "changed".to_string(),
            size: 10,
            available: true,
            ..Default::default()
        });
        assert_ne!(
            all,
            preparation_fingerprint(&a, SyncScope::all(), &settings)
        );

        // 镜像设置会改变准备出来的索引/依赖，也必须重新准备。
        let mut mirrors = settings.clone();
        mirrors.mirrors.torch = Some("https://mirror.example/pytorch".to_string());
        assert_ne!(
            all,
            preparation_fingerprint(&baseline, SyncScope::all(), &mirrors)
        );
    }

    #[test]
    fn extensionless_wheel_gets_valid_name() {
        let root = temp_dir("wheel-name");
        let raw = root.join("cstr");
        let file = File::create(&raw).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opt = zip::write::SimpleFileOptions::default();
        zip.start_file("cstr-0.1.0.dist-info/WHEEL", opt).unwrap();
        zip.write_all(b"Wheel-Version: 1.0\nTag: py3-none-any\n")
            .unwrap();
        zip.start_file("cstr-0.1.0.dist-info/METADATA", opt)
            .unwrap();
        zip.write_all(b"Name: cstr\nVersion: 0.1.0\n").unwrap();
        zip.finish().unwrap();

        let name = normalize_archive_filename(&raw, "cstr", "cstr", "0.1.0").unwrap();
        assert_eq!(name, "cstr-0.1.0-py3-none-any.whl");
        let install = ensure_install_archive(&raw, "cstr", "cstr", "0.1.0").unwrap();
        assert_eq!(install.file_name().unwrap(), "cstr-0.1.0-py3-none-any.whl");
        assert!(install.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn encoded_wheel_filename_is_decoded_before_install() {
        let root = temp_dir("wheel-encoded-name");
        let decoded = "llama_cpp_python-0.4.5+cu130-cp313-cp313-win_amd64.whl";
        let encoded = decoded.replace('+', "%2B");
        let raw = root.join(&encoded);
        fs::write(&raw, b"placeholder").unwrap();

        let install =
            ensure_install_archive(&raw, &encoded, "llama_cpp_python", "0.4.5%2Bcu130").unwrap();
        assert_eq!(install.file_name().unwrap(), decoded);
        assert!(install.exists());
        assert_eq!(
            remote_filename(&format!("https://example.invalid/{encoded}?download=1")),
            decoded
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extensionless_archives_get_supported_extension() {
        let root = temp_dir("archive-name");
        let tar_gz = root.join("cstr");
        fs::write(&tar_gz, [0x1f, 0x8b, 0x08, 0x00]).unwrap();
        assert_eq!(
            normalize_archive_filename(&tar_gz, "cstr", "cstr", "0.1.0").unwrap(),
            "cstr.tar.gz"
        );

        let zip = root.join("src");
        let file = File::create(&zip).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("setup.py", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.finish().unwrap();
        assert_eq!(
            normalize_archive_filename(&zip, "src", "src", "0.1.0").unwrap(),
            "src.zip"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_paths_reject_traversal_and_absolute_paths() {
        for bad in [
            "",
            "..",
            "../x",
            "..\\x",
            "/etc/passwd",
            "C:\\Windows\\x",
            "C:x",
            "\\\\server\\share\\x",
            "env/../../x",
            "env\\..\\..\\x",
        ] {
            assert!(!safe_relative_path(bad), "should reject {bad:?}");
        }
        for good in ["env/x.zip", "models/sdxl/a.safetensors", "nodes/foo.zip"] {
            assert!(safe_relative_path(good), "should accept {good:?}");
        }
        assert!(inbox_path(Path::new("inbox"), "../x").is_err());
        assert_eq!(
            inbox_path(Path::new("inbox"), "env/x.zip").unwrap(),
            Path::new("inbox").join("env/x.zip")
        );
    }

    #[test]
    fn requirements_names_and_versions_are_strict() {
        assert!(safe_package_name("torch"));
        assert!(safe_package_name("comfyui-frontend-package"));
        assert!(!safe_package_name("torch\n--extra-index-url"));
        assert!(!safe_package_name("../x"));
        assert!(safe_package_version("2.1.0+cu121"));
        assert!(safe_package_version("1.0.0rc1"));
        assert!(!safe_package_version("1.0\n--index-url"));
        assert!(!safe_package_version(""));
    }

    #[test]
    fn torch_index_mismatch_uses_local_cuda_label() {
        let mut py = SyncPythonState::default();
        py.packages
            .insert("torch".to_string(), "2.10.0+cu130".to_string());
        py.torch_index_url = Some("https://download.pytorch.org/whl/cpu/".to_string());
        assert_eq!(
            torch_index_for(&py).as_deref(),
            Some("https://download.pytorch.org/whl/cu130/")
        );
        py.torch_index_url = Some("https://download.pytorch.org/whl/cu130/".to_string());
        assert_eq!(
            torch_index_for(&py).as_deref(),
            Some("https://download.pytorch.org/whl/cu130/")
        );
        py.torch_index_url = None;
        assert_eq!(
            torch_index_for(&py).as_deref(),
            Some("https://download.pytorch.org/whl/cu130/")
        );
    }

    #[test]
    fn transient_progress_replaces_instead_of_appending() {
        let mut job = SyncJob::default();
        push_job_log(&mut job, "下载 ComfyUI-MatAnyone.zip：10% · 1 MiB/10 MiB");
        push_job_log(&mut job, "下载 ComfyUI-MatAnyone.zip：20% · 2 MiB/10 MiB");
        assert_eq!(job.logs.len(), 1);
        assert!(job.logs[0].contains("20%"));
        push_job_log(&mut job, "下载 ComfyUI-MatAnyone.zip 完成（10 MiB）");
        assert_eq!(job.logs.len(), 1);
        assert!(job.logs[0].contains("完成"));
        push_job_log(&mut job, "下载 Next.zip：1% · 1 MiB/10 MiB");
        assert_eq!(job.logs.len(), 2);
        push_job_log(&mut job, "安装依赖：Prepared 1 package");
        push_job_log(&mut job, "安装依赖：Installed 1 package");
        push_job_log(&mut job, "普通依赖安装完成");
        assert_eq!(job.logs.len(), 2);
        assert!(job.logs[1].contains("普通依赖安装完成"));
    }

    #[test]
    fn requirement_lines_skip_custom_and_unversioned_packages() {
        let mut packages = BTreeMap::new();
        packages.insert("argostranslate".to_string(), String::new());
        packages.insert("llama-cpp-python".to_string(), "0.4.5%2Bcu130".to_string());
        packages.insert("aiohttp".to_string(), "3.9.5".to_string());
        packages.insert("unknown".to_string(), String::new());
        let wheels = vec![
            CustomWheel {
                name: "argostranslate".to_string(),
                ..Default::default()
            },
            // PEP 503 里 -/_/. 等价，自定义包名不能因为分隔符不同就漏掉。
            CustomWheel {
                name: "llama_cpp_python".to_string(),
                ..Default::default()
            },
        ];
        assert_eq!(
            requirement_lines(&packages, &wheels).unwrap(),
            vec!["aiohttp==3.9.5"]
        );
    }

    #[test]
    fn requirement_lines_still_reject_injection() {
        let mut packages = BTreeMap::new();
        packages.insert("safe".to_string(), "1.0".to_string());
        packages.insert(
            "bad".to_string(),
            "1.0\n--index-url=https://evil".to_string(),
        );
        let err = requirement_lines(&packages, &[]).unwrap_err();
        assert_eq!(err.code, "SYNC_REQUIREMENTS");
    }

    #[test]
    fn install_indexes_always_fall_back_to_official_pypi() {
        assert_eq!(install_indexes(None, None), vec!["https://pypi.org/simple"]);
        assert_eq!(
            install_indexes(
                Some("https://download.pytorch.org/whl/cu130"),
                Some("https://mirror.example/simple/")
            ),
            vec![
                "https://download.pytorch.org/whl/cu130",
                "https://mirror.example/simple",
                "https://pypi.org/simple"
            ]
        );
        assert_eq!(
            install_indexes(Some("https://pypi.org/simple/"), None),
            vec!["https://pypi.org/simple"]
        );
    }

    #[test]
    fn required_space_includes_backup_margin() {
        let mut diff = SyncDiff::default();
        diff.models.modified.push(ModelChange {
            from_size: 100,
            ..Default::default()
        });
        diff.models.transfer_bytes = 500;
        let totals = SyncTotals {
            files: 1,
            bytes: 200,
        };
        // 载荷 200 + 新模型500 + 旧模型备份100 + 64 MiB 余量。
        assert_eq!(required_space(&diff, &totals), 800 + 64 * 1024 * 1024);
    }
}
