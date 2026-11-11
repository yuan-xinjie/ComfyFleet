//! 快照：把实例此刻「ComfyUI 版本 + 自定义节点 + Python 包」记成一条可回滚的时间线。
//!
//! 语义对齐 ComfyUI Desktop 的 Snapshots（设计见 `docs/03-快照功能设计.md`）：
//!
//! - 历史里只存「真的经历过的状态」：恢复完成后重新拍一张 post-restore 快照。
//!   记录恢复后的**真实**结果，而不是「想恢复到的目标」；
//! - 恢复顺序固定：ComfyUI 版本 → 节点（装/卸、启用/禁用、删多余）→ 包（装/卸、删）。
//! - torch 系与打包工具一律跳过（由引擎矩阵与 uv 自己管），只报告差异。
//! - 恢复前先拍一张 pre-restore 快照：失败时用户点一下就能回到恢复前。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{self, GlobalSettings, InstanceConfig, ServerOpts};
use crate::error::AppError;
use crate::proc;
use crate::runtime;

/// 快照文件格式版本。
pub const SNAPSHOT_VERSION: u32 = 1;

pub const T_MANUAL: &str = "manual";
pub const T_BOOT: &str = "boot";
pub const T_PRE_UPDATE: &str = "pre-update";
pub const T_POST_UPDATE: &str = "post-update";
pub const T_PRE_RESTORE: &str = "pre-restore";
pub const T_PRE_DEPS: &str = "pre-deps";

/// 恢复时不 pip 同步的包：核心工具链 + torch 全家。
///
/// 这些包由引擎矩阵决定（换错就是驱动不兼容 + 几个 GB 重下），
/// 跨机器快照更是如此，所以只报告差异、不动它们。
const PROTECTED_EXACT: &[&str] = &[
    "pip",
    "setuptools",
    "wheel",
    "uv",
    "torch",
    "torchvision",
    "torchaudio",
    "torchcodec",
    "triton",
    "torio",
    "functorch",
];

/// 前缀 `<prefix>` / `<prefix>-*` / `<prefix>_*` 匹配。
/// 不能用裸子串（`torchmetrics`、`torchsde` 必须仍可恢复）。
const PROTECTED_PREFIXES: &[&str] = &[
    "torch",
    "triton",
    "nvidia",
    "cuda",
    "rocm",
    "pytorch-triton",
];

pub fn is_protected_package(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    if PROTECTED_EXACT.contains(&lower.as_str()) {
        return true;
    }
    PROTECTED_PREFIXES.iter().any(|prefix| {
        lower.starts_with(&format!("{prefix}-")) || lower.starts_with(&format!("{prefix}_"))
    })
}

/// 节点目录名只允许这些字符：恢复时要拿它拼路径。
/// 自定义节点目录名：`@` 是合法目录字符（例如 `alekpet@nightly`），
/// 以前的白名单把它误判成非法名，快照恢复时会直接跳过整个节点。
fn is_safe_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

// ---------------------------------------------------------------- 数据模型

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCore {
    pub channel: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPython {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub torch: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotNode {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    pub enabled: bool,
    /// git 工作区有未提交改动（含未跟踪文件）。
    #[serde(default)]
    pub dirty: bool,
    /// 节点的本地内容怎么存：`full` = 整目录归档（手工节点/没有 origin），
    /// `patch` = 在某个 commit 上叠加未提交 diff + 未跟踪文件。`None` = 干净 git 节点。
    #[serde(default)]
    pub content: Option<String>,
    /// 工作区内容指纹（不含 `.git`、缓存）。新旧快照只有两边都有时才比较。
    #[serde(default)]
    pub content_hash: Option<String>,
}

/// 快照里记下的一个 wheel 来源。
///
/// - 本地安装：记 `filename`。恢复时去设置里的 wheel 库找同名文件。
///   **不存绝对路径**，换机器/换程序位置都不失效。
/// - URL 安装：记 `url`，恢复时直接按 URL 重装。
/// - 自定义轮子临时下载：优先看 `comfyfleet_origin` 里的原始 URL。
///   本地临时文件安装完会被删除，恢复时不依赖它。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WheelRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub version: u32,
    /// 文件名（不含 `.json`），也是时间线上的唯一键。
    pub id: String,
    pub created_at: String,
    pub trigger: String,
    #[serde(default)]
    pub label: Option<String>,
    pub comfyui: SnapshotCore,
    pub python: SnapshotPython,
    /// 启动参数（监 / 端口 / CORS / 自定义参数）。
    /// 老快照没有这一项，恢复时保持目标机当前值。
    #[serde(default)]
    pub server: Option<ServerOpts>,
    pub nodes: Vec<SnapshotNode>,
    pub packages: BTreeMap<String, String>,
    /// 不在索引上的包怎么装回来：本地 wheel 记文件名；可选内容哈希，恢复。
    /// 去用户指定的 wheel 库里找同名文件），远程安装的记 `url`。老快照缺这一项，
    /// 恢复时仍按 `name==version` 从索引装，行为不变。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wheels: BTreeMap<String, WheelRef>,
    /// 采集时的降级说明（比 pip freeze 失败）。
    #[serde(default)]
    pub note: Option<String>,
}

impl Snapshot {
    /// 只有「启动」类的无人值守快照会被自动裁掉。
    pub fn auto_prunable(&self) -> bool {
        self.trigger == T_BOOT && self.label.as_deref().unwrap_or("").is_empty()
    }
}

// ---------------------------------------------------------------- 存储

/// 快照目录：跟随实例，放在 `<实例根目录\.comfyfleet\snapshots`。
///
/// 远程实例没有本地根目录，退回启动器数据目录；本地目录不可写时也退回，
/// 避免只读的导入目录让整个快照功能不可用。
pub fn dir(inst: &InstanceConfig) -> PathBuf {
    if inst.is_remote() {
        return legacy_root(&inst.id);
    }
    let local = PathBuf::from(&inst.root_dir)
        .join(".comfyfleet")
        .join("snapshots");
    if !writable_dir(&local) {
        log::warn!(
            "实例 {} 的快照目录不可写，回退到启动器数据目录：{}",
            inst.id,
            local.display()
        );
        return legacy_root(&inst.id);
    }
    migrate_legacy(inst, &local);
    local
}

/// 旧版快照目录：`<数据目录>\snapshots\<实例 id>`。只用于远程实例、只读回退和迁移。
pub fn legacy_root(instance_id: &str) -> PathBuf {
    if !safe_id(instance_id) {
        return config::app_root().join("snapshots").join("_invalid");
    }
    config::app_root().join("snapshots").join(instance_id)
}

/// 目录可写才把快照放在实例里。`create_dir_all` 对已存在的只读目录会成功。
/// 所以还要实际建一个探针文件。
fn writable_dir(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".write-test-{}-{nanos}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => true,
        Err(_) => false,
    }
}

/// 把旧版启动器目录里的快照搬到实例目录。只在两边同一份文件系统时 rename。
/// 跨盘时逐项复制，成功后才删旧文件。任何一项失败都保留旧目录，下次再试。
fn migrate_legacy(inst: &InstanceConfig, dest: &Path) {
    let old = legacy_root(&inst.id);
    if old == dest || !old.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&old) else {
        return;
    };
    let mut failed = false;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(name) = src.file_name() else {
            continue;
        };
        let target = dest.join(name);
        if target.exists() {
            continue;
        }
        if std::fs::rename(&src, &target).is_ok() {
            continue;
        }
        if let Err(e) = move_entry(&src, &target) {
            failed = true;
            log::warn!("迁移快照 {} 失败：{e}", src.display());
        }
    }
    if !failed {
        let _ = std::fs::remove_dir(&old);
    }
}

fn move_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            move_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::remove_dir(src)
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)
    }
}

/// id 只允许字母数字与 `-_.`，且不能带路径分隔符（防目录穿越）。
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn file_of(base: &Path, id: &str) -> Result<PathBuf, AppError> {
    if !safe_id(id) {
        return Err(AppError::new("SNAPSHOT_ID", "快照文件名不合法"));
    }
    Ok(base.join(format!("{id}.json")))
}

fn now_id(trigger: &str) -> String {
    let dt = chrono::Local::now();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{:06x}",
        dt.format("%Y%m%d_%H%M%S_%3f"),
        trigger,
        nanos % 0xff_ffff
    )
}

pub fn save_in(base: &Path, snap: &Snapshot) -> Result<(), AppError> {
    std::fs::create_dir_all(base)
        .map_err(|e| AppError::new("SNAPSHOT_DIR", format!("建快照目录失败：{e}")))?;
    let path = file_of(base, &snap.id)?;
    let text = serde_json::to_string_pretty(snap)
        .map_err(|e| AppError::new("SNAPSHOT_ENCODE", format!("快照序列化失败：{e}")))?;
    config::atomic_write(&path, text.as_bytes())
}

/// 归一化快照中记录的包版本，便于后续比较。
fn normalize_snapshot_packages(snap: &mut Snapshot) {
    for version in snap.packages.values_mut() {
        *version = normalize_package_version(version);
    }
}

pub fn list_in(base: &Path) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<Snapshot>(&text) {
            Ok(mut snap) => {
                normalize_snapshot_packages(&mut snap);
                out.push(snap);
            }
            Err(e) => log::warn!("跳过损坏的快照 {}：{e}", path.display()),
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

pub fn load_in(base: &Path, id: &str) -> Result<Snapshot, AppError> {
    let path = file_of(base, id)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| AppError::new("SNAPSHOT_MISSING", format!("读不到快照：{e}")))?;
    let mut snap: Snapshot = serde_json::from_str(&text)
        .map_err(|e| AppError::new("SNAPSHOT_BAD", format!("快照文件解析失败：{e}")))?;
    normalize_snapshot_packages(&mut snap);
    if snap.version != SNAPSHOT_VERSION {
        return Err(AppError::new(
            "SNAPSHOT_VERSION",
            format!(
                "快照格式版本 {} 不认识（当前 {}",
                snap.version, SNAPSHOT_VERSION
            ),
        ));
    }
    Ok(snap)
}

pub fn delete_in(base: &Path, id: &str) -> Result<(), AppError> {
    let path = file_of(base, id)?;
    std::fs::remove_file(&path)
        .map_err(|e| AppError::new("SNAPSHOT_DELETE", format!("删除快照失败：{e}")))?;
    let payload = payload_dir(base, id);
    if payload.is_dir() {
        let _ = std::fs::remove_dir_all(payload);
    }
    Ok(())
}

/// 裁掉超出上限的自动快照，返回删了几条。
pub fn prune_in(base: &Path, limit: usize) -> usize {
    let all = list_in(base);
    let autos: Vec<&Snapshot> = all.iter().filter(|s| s.auto_prunable()).collect();
    if autos.len() <= limit {
        return 0;
    }
    let mut removed = 0;
    for snap in autos.into_iter().skip(limit) {
        if delete_in(base, &snap.id).is_ok() {
            removed += 1;
        }
    }
    removed
}

// ---------------------------------------------------------------- 采集

///  `ComfyUI\.git\HEAD` 指向的真实提交（`packed-refs` 也认）。
fn read_head(comfy: &Path) -> Option<String> {
    let git_dir = comfy.join(".git");
    if !git_dir.exists() {
        return None;
    }
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        if let Ok(sha) = std::fs::read_to_string(git_dir.join(reference.trim())) {
            return Some(sha.trim().to_string());
        }
        // 引用可能只存在于 packed-refs。
        if let Ok(packed) = std::fs::read_to_string(git_dir.join("packed-refs")) {
            for line in packed.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                if let (Some(sha), Some(name)) = (parts.next(), parts.next()) {
                    if name == reference.trim() {
                        return Some(sha.to_string());
                    }
                }
            }
        }
        return None;
    }
    if head.len() >= 7 {
        Some(head.to_string())
    } else {
        None
    }
}

/// `uv pip list --format freeze` 的解析：只用来提供“真实安装版本”。
/// 直接 URL 包也以它为准，不再从 wheel 文件名猜版本。
fn parse_installed_versions(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let core = line.split(';').next().unwrap_or(line).trim();
        let Some((name, version)) = core.split_once("==") else {
            continue;
        };
        let name = name.trim();
        let version = version.trim();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        out.insert(crate::sync::canonical_dist_name(name), version.to_string());
    }
    out
}

/// 旧版本快照可能把 wheel URL 里的 `%2B` 原样存进包版本；恢复前统一还原成 `+`。
fn normalize_package_version(version: &str) -> String {
    crate::sync::percent_decode(version)
}

/// `uv pip freeze` 的解析：保留原始包名，也保留 `name @ URL` 直接引用。
/// 版本号必须从 `uv pip list --format freeze` 的真实安装元数据取，不能从
/// URL / wheel 文件名取；否则同一颗 wheel 会因为文件名带不带 `+cu...`
/// 被当成两个版本。
fn parse_freeze_with_versions(
    text: &str,
    actual: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("-e ") {
            continue;
        }
        let core = line.split(';').next().unwrap_or(line).trim();
        if let Some((name, _url)) = core.split_once(" @ ") {
            let name = name.trim();
            if !name.is_empty() && !name.contains(char::is_whitespace) {
                // 直接 URL 包的真实版本只认已安装元数据；查不到就留空。
                let version = actual
                    .get(&crate::sync::canonical_dist_name(name))
                    .cloned()
                    .unwrap_or_default();
                out.insert(name.to_string(), version);
            }
            continue;
        }
        if core.contains("://") {
            continue;
        }
        let mut parts = core.splitn(2, "==");
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim();
        let version = version.trim();
        if name.is_empty() || version.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        let version = actual
            .get(&crate::sync::canonical_dist_name(name))
            .cloned()
            .unwrap_or_else(|| version.to_string());
        out.insert(name.to_string(), version);
    }
    out
}

fn venv_python(inst: &InstanceConfig) -> Option<PathBuf> {
    runtime::resolve_python(inst).map(|py| py.python)
}

fn uv_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let cache = config::shared_dir().join("uv-cache");
    let _ = std::fs::create_dir_all(&cache);
    env.insert(
        "UV_CACHE_DIR".to_string(),
        cache.to_string_lossy().to_string(),
    );
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    env
}

pub(crate) fn installed_packages(
    inst: &InstanceConfig,
    uv: &Path,
    python: &Path,
) -> Result<BTreeMap<String, String>, String> {
    installed_packages_in(uv, python, Some(Path::new(&inst.root_dir)), &uv_env())
}

/// 两路读取包清单：`freeze` 保留包名和 `name @ URL` 来源，`list` 提供
/// 实际安装元数据。节点依赖、快照、同步都用它，避免 URL 包从轮子文件名
/// 取到带 `+cu...` 的“版本”。
pub(crate) fn installed_packages_in(
    uv: &Path,
    python: &Path,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let python_arg = python.to_string_lossy().to_string();
    let frozen = proc::run(uv, &["pip", "freeze", "--python", &python_arg], cwd, env)
        .map_err(|e| e.message)?;
    if !frozen.ok() {
        return Err(frozen.detail());
    }

    let listed = proc::run(
        uv,
        &["pip", "list", "--format", "freeze", "--python", &python_arg],
        cwd,
        env,
    )
    .map_err(|e| e.message)?;
    if !listed.ok() {
        return Err(listed.detail());
    }
    let actual = parse_installed_versions(&listed.stdout);
    Ok(parse_freeze_with_versions(&frozen.stdout, &actual))
}

/// 包清单单独抽出来：同步心跳只需要这一项，不应该顺带哈希所有节点目录。
pub fn package_versions(inst: &InstanceConfig) -> (BTreeMap<String, String>, Option<String>) {
    let uv = crate::commands::env::resolve_uv().ok();
    match (uv.as_deref(), venv_python(inst)) {
        (Some(uv), Some(python)) => match installed_packages(inst, uv, &python) {
            Ok(pkgs) => (pkgs, None),
            Err(e) => (BTreeMap::new(), Some(format!("包列表采集失败：{e}"))),
        },
        _ => (
            BTreeMap::new(),
            Some("没有 uv 或虚拟环境，包列表为空。".to_string()),
        ),
    }
}

fn git_status_dirty(git: &Path, dir: &Path) -> bool {
    git_run(
        git,
        dir,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .ok()
    .map(|out| out.ok() && !out.stdout.trim().is_empty())
    .unwrap_or(false)
}

/// 装依赖前用的轻量节点清单：只记名字 / 来源 / commit / 启用状态。
///
/// 不读节点目录内容、不跑 `git status`。补依赖流程只修改 Python 环境，
/// 对节点树做全量 hash + 归档没有实际收益，反而让每次安装先卡几秒。
fn capture_nodes_metadata(inst: &InstanceConfig) -> Vec<SnapshotNode> {
    let dir = nodes_dir(inst);
    crate::commands::nodes::scan_nodes(&dir)
        .unwrap_or_default()
        .into_iter()
        .map(|n| SnapshotNode {
            name: n.name,
            url: n.url,
            commit: n.commit,
            enabled: n.enabled,
            dirty: false,
            content: None,
            content_hash: None,
        })
        .collect()
}

fn capture_nodes(inst: &InstanceConfig, settings: &GlobalSettings) -> Vec<SnapshotNode> {
    let dir = nodes_dir(inst);
    let items = crate::commands::nodes::scan_nodes(&dir).unwrap_or_default();
    let needs_git = items.iter().any(|n| n.url.is_some() && n.commit.is_some());
    let git = needs_git
        .then(|| crate::git::resolve_quiet(settings))
        .flatten();
    items
        .into_iter()
        .map(|n| {
            let path = node_path(inst, &n.name, n.enabled);
            let content_hash = path
                .is_dir()
                .then(|| crate::sync::hash_tree(&path, false))
                .filter(|hash| !hash.is_empty());
            let is_git = n.url.is_some() && n.commit.is_some();
            let dirty = is_git
                && git
                    .as_deref()
                    .map(|git| git_status_dirty(git, &path))
                    .unwrap_or(false);
            // 手工节点没有可重 clone 的来源；git 节点有未提交改动时，
            // 只存 diff + 未跟踪文件，恢复时仍然保 .git 历史。
            let content = if !is_git || git.is_none() {
                Some("full".to_string())
            } else if dirty {
                Some("patch".to_string())
            } else {
                None
            };
            SnapshotNode {
                name: n.name,
                url: n.url,
                commit: n.commit,
                enabled: n.enabled,
                dirty,
                content,
                content_hash,
            }
        })
        .collect()
}

/// 采一条快照（只读；freeze 失败会写 `note`，不阻断）。
pub fn capture(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    trigger: &str,
    label: Option<String>,
) -> Snapshot {
    capture_scoped(
        inst,
        settings,
        trigger,
        label,
        crate::sync::SyncScope::all(),
    )
}

/// 读 venv 的 `*.dist-info/direct_url.json`，记下每个包是从哪儿装的。
///
/// 只读：不复制文件、不联网。先看 `comfyfleet_origin`（临时下载文件装完后
/// 会被删除）；没有来源 URL 才退回文件名，恢复时去用户指定的 wheel 库找。
fn capture_wheels(inst: &InstanceConfig) -> BTreeMap<String, WheelRef> {
    let mut out = BTreeMap::new();
    let Some(py) = runtime::resolve_python(inst) else {
        return out;
    };
    for site in site_packages_dirs(&py.root) {
        let Ok(entries) = std::fs::read_dir(&site) else {
            continue;
        };
        for entry in entries.flatten() {
            let dist_info = entry.path();
            let is_dist_info = dist_info
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".dist-info"));
            if !is_dist_info || !dist_info.is_dir() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(dist_info.join("direct_url.json")) else {
                continue; // 从索引装的包没有这条记录
            };
            let Ok(direct) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if direct.get("dir_info").is_some() {
                continue; // 本地目录 / editable，走同步的另一条链。
            }
            let Some(url) = direct
                .get("url")
                .and_then(|v| v.as_str())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            let name = dist_metadata(&dist_info)
                .0
                .or_else(|| dist_info_parts(&dist_info).map(|(name, _)| name))
                .unwrap_or_default();
            if name.is_empty() || is_protected_package(&name) {
                continue;
            }
            let key = crate::sync::canonical_dist_name(&name);
            // 自定义轮子装完会删除临时 .whl；优先记录原始下载 URL。
            // 快照恢复时直接按 URL 重装，不能依赖已经删掉的文件名。
            if let Some(origin) = direct
                .get("comfyfleet_origin")
                .and_then(|v| v.get("url"))
                .and_then(|v| v.as_str())
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            {
                out.insert(
                    key,
                    WheelRef {
                        url: Some(origin.to_string()),
                        ..Default::default()
                    },
                );
                continue;
            }
            if let Some(path) = file_url_to_path(url) {
                // 快照只记文件名；源文件现在就删掉也不影响记录本身。
                let Some(filename) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| !n.is_empty())
                else {
                    continue;
                };
                out.insert(
                    key,
                    WheelRef {
                        filename: Some(filename.to_string()),
                        url: None,
                    },
                );
            } else if url.starts_with("http://") || url.starts_with("https://") {
                out.insert(
                    key,
                    WheelRef {
                        url: Some(url.to_string()),
                        ..Default::default()
                    },
                );
            }
        }
    }
    out
}

/// venv / 便携包里 `site-packages` 可能的位置。
fn site_packages_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let windows = root.join("Lib").join("site-packages");
    if windows.is_dir() {
        out.push(windows);
    }
    if let Ok(entries) = std::fs::read_dir(root.join("lib")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("python") {
                let dir = entry.path().join("site-packages");
                if dir.is_dir() {
                    out.push(dir);
                }
            }
        }
    }
    out
}

/// `file:///F:/Downloads/x%2By.whl` → `F:\Downloads\x+y.whl`（去掉 `#sha256=` 片段）。
fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                decoded.push(byte);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&decoded);
    let path = PathBuf::from(decoded.replace('/', "\\"));
    path.is_absolute().then_some(path)
}

fn dist_info_parts(path: &Path) -> Option<(String, String)> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".dist-info")?;
    let (name, version) = stem.split_once('-')?;
    Some((name.to_string(), version.to_string()))
}

/// `METADATA` 头部读 `Name:` / `Version:`（头部到第一个空行结束）。
fn dist_metadata(dir: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(dir.join("METADATA")) else {
        return (None, None);
    };
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Name:") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Version:") {
            version = Some(value.trim().to_string());
        }
    }
    (name, version)
}

/// 同步准备时只采集本次勾选的内容组。未勾选的 ComfyUI / Python / 节点
/// 不去 freeze、不哈希节点目录，避免“只同步模型”却把整棵节点树扫一遍。
pub fn capture_scoped(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    trigger: &str,
    label: Option<String>,
    scope: crate::sync::SyncScope,
) -> Snapshot {
    let comfy = Path::new(&inst.root_dir).join("ComfyUI");
    let (packages, note) = if scope.python {
        package_versions(inst)
    } else {
        (BTreeMap::new(), None)
    };
    // 记下每个自定义包从哪儿装的：本地 wheel 记文件名，URL 安装记地址。
    // 只读 dist-info，不复制轮子：副本由用户自己放进 wheel 库。
    let wheels = if scope.python {
        capture_wheels(inst)
    } else {
        BTreeMap::new()
    };
    let torch = packages.get("torch").cloned();
    let comfyui = if scope.comfyui {
        SnapshotCore {
            channel: inst.comfy.channel.clone(),
            reference: inst.comfy.reference.clone(),
            commit: read_head(&comfy)
                .or_else(|| (!inst.comfy.commit.is_empty()).then(|| inst.comfy.commit.clone())),
            version: inst.comfy.version.clone(),
        }
    } else {
        SnapshotCore {
            channel: String::new(),
            reference: String::new(),
            commit: None,
            version: None,
        }
    };
    let python = if scope.python {
        SnapshotPython {
            version: inst.python_version.clone(),
            torch,
        }
    } else {
        SnapshotPython {
            version: String::new(),
            torch: None,
        }
    };
    Snapshot {
        version: SNAPSHOT_VERSION,
        id: now_id(trigger),
        created_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        trigger: trigger.to_string(),
        label: label.filter(|l| !l.trim().is_empty()),
        comfyui,
        python,
        server: scope.comfyui.then(|| inst.server.clone()),
        nodes: if scope.nodes {
            if trigger == T_PRE_DEPS {
                // 补依赖只会改 venv，不会改节点目录。这里再对整棵节点树做
                // hash / 归档会让每次“装前快照”多等好几秒，却对回滚 Python
                // 包没有帮助；只记录节点元数据即可。
                capture_nodes_metadata(inst)
            } else {
                capture_nodes(inst, settings)
            }
        } else {
            Vec::new()
        },
        packages,
        wheels,
        note,
    }
}

// ---------------------------------------------------------------- 节点内容归档

fn payload_dir(base: &Path, id: &str) -> PathBuf {
    base.join(format!("{id}.nodes"))
}

fn push_note(note: &mut Option<String>, text: String) {
    *note = Some(match note.take() {
        Some(existing) if !existing.trim().is_empty() => format!("{existing}；{text}"),
        _ => text,
    });
}

fn archive_skipped_component(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git" | "__pycache__" | ".venv" | "venv" | "node_modules" | ".cache" | ".pytest_cache"
    )
}

fn archive_skipped_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".pyc")
        || lower.ends_with(".pyo")
        || lower == ".ds_store"
        || lower.ends_with(".part")
        || lower.ends_with(".tmp")
}

fn zip_untracked(root: &Path, rels: &[String], dest: &Path) -> Result<usize, AppError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(dest)
        .map_err(|e| AppError::new("SNAPSHOT_ZIP", format!("创建 {} 失败：{e}", dest.display())))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut count = 0usize;
    for rel in rels {
        let rel = rel.replace('\\', "/");
        let rel = rel.trim_start_matches('/');
        let safe_rel = !rel.is_empty()
            && !rel
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            && !(rel.len() > 1 && rel.as_bytes()[1] == b':');
        if !safe_rel
            || rel.split('/').any(archive_skipped_component)
            || rel.rsplit('/').next().is_some_and(archive_skipped_file)
        {
            continue;
        }
        let path = root.join(&rel);
        if !path.is_file() {
            continue;
        }
        zip.start_file(rel, options)
            .map_err(|e| AppError::new("SNAPSHOT_ZIP", e.to_string()))?;
        let mut f = std::fs::File::open(&path)?;
        std::io::copy(&mut f, &mut zip)?;
        count += 1;
    }
    zip.finish()
        .map_err(|e| AppError::new("SNAPSHOT_ZIP", e.to_string()))?;
    Ok(count)
}

fn git_untracked(git: &Path, dir: &Path) -> Result<Vec<String>, String> {
    let out = git_run(
        git,
        dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .map_err(|e| e.message)?;
    if !out.ok() {
        return Err(out.detail());
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// 为需要本地内容的节点生成 payload。失败只降级该节点并把原因写进快照。
/// 绝不让整条快照失败。
fn archive_nodes(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    snap: &mut Snapshot,
    base: &Path,
) -> Result<Vec<String>, AppError> {
    if !snap.nodes.iter().any(|n| n.content.is_some()) {
        return Ok(Vec::new());
    }
    let payload = payload_dir(base, &snap.id);
    std::fs::create_dir_all(&payload)
        .map_err(|e| AppError::new("SNAPSHOT_DIR", format!("建节点归档目录失败：{e}")))?;
    let needs_git = snap
        .nodes
        .iter()
        .any(|n| n.content.as_deref() == Some("patch"));
    let git = needs_git
        .then(|| crate::git::resolve_quiet(settings))
        .flatten();
    let mut failures: Vec<String> = Vec::new();

    for node in &mut snap.nodes {
        let Some(mode) = node.content.clone() else {
            continue;
        };
        let dir = node_path(inst, &node.name, node.enabled);
        let result = match mode.as_str() {
            "full" => sync_zip_node(&dir, &payload, &node.name),
            "patch" => git_patch_node(git.as_deref(), &dir, &payload, &node.name),
            _ => Err(AppError::new(
                "SNAPSHOT_MODE",
                format!("未知归档模式 {mode}"),
            )),
        };
        if let Err(e) = result {
            // 标成 missing：恢复时会明确报「归档缺失」，而不是把它误当成
            // 干净 git 节点不 reset（那会丢掉手工改动）；下次采集也会重新尝试。
            failures.push(format!("{}：{}", node.name, e.message));
            node.content = Some("missing".into());
        }
    }
    if !failures.is_empty() {
        push_note(
            &mut snap.note,
            format!("节点本地内容归档失败：{}", failures.join("。")),
        );
    }
    Ok(failures)
}

fn sync_zip_node(dir: &Path, payload: &Path, name: &str) -> Result<(), AppError> {
    if !dir.is_dir() {
        return Err(AppError::new(
            "SNAPSHOT_NODE_MISSING",
            format!("节点目录不存在：{}", dir.display()),
        ));
    }
    let dest = payload.join(format!("{}.zip", safe_stem(name)));
    crate::sync::zip_dir(dir, &dest, false)?;
    Ok(())
}

fn safe_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn git_patch_node(
    git: Option<&Path>,
    dir: &Path,
    payload: &Path,
    name: &str,
) -> Result<(), AppError> {
    let Some(git) = git else {
        return Err(AppError::new(
            "SNAPSHOT_GIT",
            "没有可用的 git，无法读取未提交改动",
        ));
    };
    if !dir.is_dir() {
        return Err(AppError::new(
            "SNAPSHOT_NODE_MISSING",
            format!("节点目录不存在：{}", dir.display()),
        ));
    }
    let diff = git_run(
        git,
        dir,
        &["diff", "HEAD", "--binary", "--full-index", "--no-color"],
    )
    .map_err(|e| AppError::new("SNAPSHOT_PATCH", e.message))?;
    if !diff.ok() {
        return Err(AppError::new("SNAPSHOT_PATCH", diff.detail()));
    }
    let patch = payload.join(format!("{}.patch", safe_stem(name)));
    config::atomic_write(&patch, diff.stdout.as_bytes())?;

    let untracked = git_untracked(git, dir).map_err(|e| AppError::new("SNAPSHOT_PATCH", e))?;
    let untracked_zip = payload.join(format!("{}.untracked.zip", safe_stem(name)));
    if untracked.is_empty() {
        let _ = std::fs::remove_file(&untracked_zip);
    } else {
        zip_untracked(dir, &untracked, &untracked_zip)?;
    }
    Ok(())
}

fn archive_and_save(
    base: &Path,
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    snap: &mut Snapshot,
    require_archives: bool,
) -> Result<(), AppError> {
    match archive_nodes(inst, settings, snap, base) {
        Ok(failures) if require_archives && !failures.is_empty() => {
            let _ = std::fs::remove_dir_all(payload_dir(base, &snap.id));
            return Err(AppError::new(
                "SNAPSHOT_ARCHIVE",
                format!(
                    "节点内容归档失败，已中止（{}）；请先处理占用/磁盘空间后重试",
                    failures.join("。")
                ),
            ));
        }
        Ok(_) => {}
        Err(e) => {
            let _ = std::fs::remove_dir_all(payload_dir(base, &snap.id));
            return Err(e);
        }
    }
    if let Err(e) = save_in(base, snap) {
        let _ = std::fs::remove_dir_all(payload_dir(base, &snap.id));
        return Err(e);
    }
    Ok(())
}

/// 两条快照是不是同一个状态（不含 id / 时间 / trigger / label）。
pub fn states_match(a: &Snapshot, b: &Snapshot) -> bool {
    if a.comfyui.commit != b.comfyui.commit || a.comfyui.channel != b.comfyui.channel {
        return false;
    }
    if a.server != b.server {
        return false;
    }
    if a.nodes.len() != b.nodes.len() {
        return false;
    }
    let left: BTreeMap<&str, &SnapshotNode> =
        a.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    let right: BTreeMap<&str, &SnapshotNode> =
        b.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    for (name, x) in &left {
        let Some(y) = right.get(name) else {
            return false;
        };
        if x.url != y.url || x.commit != y.commit || x.enabled != y.enabled {
            return false;
        }
        // 老快照没 content 字段；第一次升级后允许补拍一条，
        // 之后 mode + 工作区指纹判断手工节 未提交改动有没有变。
        if x.content != y.content || x.dirty != y.dirty {
            return false;
        }
        if let (Some(hx), Some(hy)) = (&x.content_hash, &y.content_hash) {
            if hx != hy {
                return false;
            }
        }
    }
    a.packages == b.packages
}

/// 启动时拍快照：与最新一条相同就跳过（不然时间线全是重复）。
pub fn capture_boot(inst: &InstanceConfig, settings: &GlobalSettings) -> Option<Snapshot> {
    let base = dir(inst);
    let mut snap = capture(inst, settings, T_BOOT, None);
    if let Some(newest) = list_in(&base).into_iter().next() {
        if states_match(&newest, &snap) {
            return None;
        }
    }
    archive_and_save(&base, inst, settings, &mut snap, false).ok()?;
    prune_in(&base, settings.sync.snapshot_max as usize);
    Some(snap)
}

/// 版本切换/更新前后的快照：调用方决定是否强制。
/// 更新、恢复前使用 required 版本，拍不下来就中止，避免没有回滚点。
///
/// `pre-update` 与上一条一模一样就不写（否则时间线会出现两行完全相同的记录）。
fn capture_for_update_impl(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    trigger: &str,
) -> Result<bool, AppError> {
    let base = dir(inst);
    let mut snap = capture(inst, settings, trigger, None);
    if trigger == T_PRE_UPDATE {
        if let Some(newest) = list_in(&base).into_iter().next() {
            if states_match(&newest, &snap) {
                return Ok(false);
            }
        }
    }
    archive_and_save(&base, inst, settings, &mut snap, true)?;
    Ok(true)
}

pub fn capture_for_update(inst: &InstanceConfig, settings: &GlobalSettings, trigger: &str) {
    if let Err(e) = capture_for_update_impl(inst, settings, trigger) {
        log::warn!("实例 {} 的 {} 快照失败：{}", inst.id, trigger, e.message);
    }
}

/// 更新/切换前的快照：失败必须中止操作，否则出错时没有回滚点。
pub fn capture_pre_update_required(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
) -> Result<(), AppError> {
    capture_for_update_impl(inst, settings, T_PRE_UPDATE)
        .map(|_| ())
        .map_err(|e| {
            AppError::new(
                "PRE_SNAPSHOT_FAILED",
                format!("更新前快照失败，已中止（避免没有回滚点）：{}", e.message),
            )
        })
}

/// 拍一条并落盘（手 / 更新前后 / 恢复前后都用它）。
pub fn capture_and_save(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    trigger: &str,
    label: Option<String>,
) -> Result<Snapshot, AppError> {
    let base = dir(inst);
    let mut snap = capture(inst, settings, trigger, label);
    archive_and_save(&base, inst, settings, &mut snap, true)?;
    Ok(snap)
}

// ---------------------------------------------------------------- 差异

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDelta {
    pub name: String,
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub from_enabled: bool,
    pub to_enabled: bool,
    /// 工作区内容（手工节点/未提交改动）变了。
    #[serde(default)]
    pub content_changed: bool,
    #[serde(default)]
    pub from_dirty: bool,
    #[serde(default)]
    pub to_dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkgDelta {
    pub name: String,
    pub from: String,
    pub to: String,
}

/// 差异里的节点引用：界面上要显示「名 + 版本/commit + 启用态」。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRef {
    pub name: String,
    pub commit: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDiff {
    pub comfyui_changed: bool,
    pub comfyui_from: Option<String>,
    pub comfyui_to: Option<String>,
    /// 启动参数是否有变化；老快照缺这一项时按“未 不变”处理。
    pub launch_changed: bool,
    pub launch_from: Option<ServerOpts>,
    pub launch_to: Option<ServerOpts>,
    pub nodes_added: Vec<NodeRef>,
    pub nodes_removed: Vec<NodeRef>,
    pub nodes_changed: Vec<NodeDelta>,
    pub packages_added: Vec<PkgDelta>,
    pub packages_removed: Vec<PkgDelta>,
    pub packages_changed: Vec<PkgDelta>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDiffSummary {
    pub comfyui_changed: bool,
    pub launch_changed: bool,
    pub nodes_added: usize,
    pub nodes_removed: usize,
    pub nodes_changed: usize,
    pub packages_added: usize,
    pub packages_removed: usize,
    pub packages_changed: usize,
}

impl SnapshotDiff {
    pub fn summary(&self) -> SnapshotDiffSummary {
        SnapshotDiffSummary {
            comfyui_changed: self.comfyui_changed,
            launch_changed: self.launch_changed,
            nodes_added: self.nodes_added.len(),
            nodes_removed: self.nodes_removed.len(),
            nodes_changed: self.nodes_changed.len(),
            packages_added: self.packages_added.len(),
            packages_removed: self.packages_removed.len(),
            packages_changed: self.packages_changed.len(),
        }
    }
}

fn version_text(snap: &Snapshot) -> String {
    snap.comfyui
        .version
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            snap.comfyui
                .commit
                .as_deref()
                .map(|c| crate::sync::short_chars(c, 7))
        })
        .unwrap_or_else(|| snap.comfyui.reference.clone())
}

/// `from` → `to` 的差异（时间线上「这条改了什么 diff(上一 → 这条)）。
pub fn diff(from: &Snapshot, to: &Snapshot) -> SnapshotDiff {
    let mut out = SnapshotDiff::default();

    if from.comfyui.commit != to.comfyui.commit || from.comfyui.channel != to.comfyui.channel {
        out.comfyui_changed = true;
        out.comfyui_from = Some(version_text(from));
        out.comfyui_to = Some(version_text(to));
    }

    if from.server != to.server {
        out.launch_changed = true;
        out.launch_from = from.server.clone();
        out.launch_to = to.server.clone();
    }

    let left: BTreeMap<&str, &SnapshotNode> =
        from.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    let right: BTreeMap<&str, &SnapshotNode> =
        to.nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    for (name, node) in &right {
        match left.get(name) {
            None => out.nodes_added.push(NodeRef {
                name: (*name).to_string(),
                commit: node.commit.clone(),
                enabled: node.enabled,
            }),
            Some(prev) => {
                // 只有两边都读 commit 时才比提交：「读不到」（节点不是 git 仓库                 // 不算改动，否则时间线上每条都会冒出无意义 `  xxx`。
                let commit_differs = match (&prev.commit, &node.commit) {
                    (Some(a), Some(b)) => a != b,
                    _ => false,
                };
                let content_changed = match (&prev.content_hash, &node.content_hash) {
                    (Some(a), Some(b)) => a != b || prev.dirty != node.dirty,
                    _ => false,
                };
                if commit_differs || prev.enabled != node.enabled || content_changed {
                    out.nodes_changed.push(NodeDelta {
                        name: (*name).to_string(),
                        from_commit: prev.commit.clone(),
                        to_commit: node.commit.clone(),
                        from_enabled: prev.enabled,
                        to_enabled: node.enabled,
                        content_changed,
                        from_dirty: prev.dirty,
                        to_dirty: node.dirty,
                    });
                }
            }
        }
    }
    for (name, node) in &left {
        if !right.contains_key(name) {
            out.nodes_removed.push(NodeRef {
                name: (*name).to_string(),
                commit: node.commit.clone(),
                enabled: node.enabled,
            });
        }
    }

    for (name, version) in &to.packages {
        match from.packages.get(name) {
            None => out.packages_added.push(PkgDelta {
                name: name.clone(),
                from: String::new(),
                to: version.clone(),
            }),
            // 旧快照可能只记录包名、没有真实版本（空字符串）。空版本代表
            // “未知”，不能当成一次版本变更，否则老快照恢复会凭空多出包升级。
            Some(prev) if !prev.is_empty() && !version.is_empty() && prev != version => {
                out.packages_changed.push(PkgDelta {
                    name: name.clone(),
                    from: prev.clone(),
                    to: version.clone(),
                })
            }
            _ => {}
        }
    }
    for (name, version) in &from.packages {
        if !to.packages.contains_key(name) {
            out.packages_removed.push(PkgDelta {
                name: name.clone(),
                from: version.clone(),
                to: String::new(),
            });
        }
    }

    out
}

// ---------------------------------------------------------------- 恢复计划

#[derive(Debug, Default)]
pub struct NodePlan {
    pub install: Vec<SnapshotNode>,
    pub switch: Vec<(SnapshotNode, SnapshotNode)>,
    /// 目录还在，但手工内容 / git 未提交改动需要还原。
    pub content: Vec<(SnapshotNode, SnapshotNode)>,
    pub enable: Vec<SnapshotNode>,
    pub disable: Vec<SnapshotNode>,
    pub remove: Vec<SnapshotNode>,
}

fn content_changed(cur: &SnapshotNode, want: &SnapshotNode) -> bool {
    match (&cur.content_hash, &want.content_hash) {
        (Some(a), Some(b)) => a != b || cur.dirty != want.dirty,
        _ => false,
    }
}

pub fn plan_nodes(current: &[SnapshotNode], target: &[SnapshotNode]) -> NodePlan {
    let mut plan = NodePlan::default();
    let by_name: BTreeMap<&str, &SnapshotNode> =
        current.iter().map(|n| (n.name.as_str(), n)).collect();
    let targets: BTreeMap<&str, &SnapshotNode> =
        target.iter().map(|n| (n.name.as_str(), n)).collect();

    for (name, node) in &targets {
        match by_name.get(name) {
            None => plan.install.push((*node).clone()),
            Some(cur) => {
                if cur.enabled != node.enabled {
                    if node.enabled {
                        plan.enable.push((*node).clone());
                    } else {
                        plan.disable.push((*node).clone());
                    }
                }
                let content_needs = content_changed(cur, node)
                    || (node.content.is_some() && cur.commit != node.commit);
                if content_needs && (node.content.is_some() || cur.commit == node.commit) {
                    plan.content.push(((*cur).clone(), (*node).clone()));
                }
                // 整目录归档的节点直接替换目录，不需要先 checkout                 //  patch 的节点由 content 分支 clean+checkout+apply。
                // 切换到 commit，不看启用状态：当前是 `<name>.disabled`、快照里
                // 启用、commit 不同时，也要先切到正确 commit，再 enable 改名。
                if cur.commit != node.commit && node.content.is_none() {
                    plan.switch.push(((*cur).clone(), (*node).clone()));
                }
            }
        }
    }
    for (name, node) in &by_name {
        if !targets.contains_key(name) {
            plan.remove.push((*node).clone());
        }
    }
    plan
}

#[derive(Debug, Default)]
pub struct PkgPlan {
    pub install: Vec<(String, String)>,
    pub remove: Vec<(String, String)>,
    pub protected: Vec<ProtectedDrift>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProtectedDrift {
    pub name: String,
    pub target: Option<String>,
    pub current: Option<String>,
}

impl PkgPlan {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.remove.is_empty()
    }
}

pub fn plan_packages(
    current: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) -> PkgPlan {
    let mut plan = PkgPlan::default();
    for (name, raw_want) in target {
        let want = normalize_package_version(raw_want);
        // 旧快照可能只记录包名、没有版本。空版本不能拼成 `name==`，
        // 也不代表要重装；当“未知”跳过，避免恢复时凭空多装 5 个包。
        if want.is_empty() {
            continue;
        }
        let have = current.get(name);
        if is_protected_package(name) {
            if have != Some(&want) {
                plan.protected.push(ProtectedDrift {
                    name: name.clone(),
                    target: Some(want.clone()),
                    current: have.cloned(),
                });
            }
            continue;
        }
        if have != Some(&want) {
            plan.install.push((name.clone(), want.clone()));
        }
    }
    for (name, have) in current {
        if target.contains_key(name) {
            continue;
        }
        if is_protected_package(name) {
            plan.protected.push(ProtectedDrift {
                name: name.clone(),
                target: None,
                current: Some(have.clone()),
            });
            continue;
        }
        plan.remove.push((name.clone(), have.clone()));
    }
    plan
}

// ---------------------------------------------------------------- 恢复

/// 恢复一个包时该发给 uv 的安装目标。
///
/// 优先级：用户 wheel 库里能找到的本地轮子（按快照记的文件名，其次包名+版本。
/// 按快照记的远程 URL 或 `name==version` 从索引装。
///
/// `Missing` 表示快照记了本地 wheel、但库里没有这颗文件：调用方把这个包列进
/// 报告的“失败”里、提示用户补文件，其余包照常继续装。
enum PackageTarget {
    Spec(String),
    Missing(String),
}

fn package_spec(
    name: &str,
    version: &str,
    target: &Snapshot,
    wheels_dir: Option<&Path>,
) -> PackageTarget {
    let want = normalize_package_version(version);
    let Some(wheel) = target.wheels.get(&crate::sync::canonical_dist_name(name)) else {
        return PackageTarget::Spec(format!("{name}=={want}"));
    };
    // URL 安装的：直接按 URL 重装，不探测网络、不校验失效。
    if let Some(url) = wheel
        .url
        .as_deref()
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
    {
        return PackageTarget::Spec(url.to_string());
    }
    let filename = wheel.filename.as_deref().unwrap_or("");
    match wheels_dir {
        Some(dir) => crate::wheelhouse::find(dir, name, &want, Some(filename))
            .map(|path| PackageTarget::Spec(path.to_string_lossy().to_string()))
            .unwrap_or_else(|| {
                PackageTarget::Missing(missing_wheel_hint(filename, name, Some(dir)))
            }),
        None => PackageTarget::Missing(missing_wheel_hint(filename, name, None)),
    }
}

fn missing_wheel_hint(filename: &str, name: &str, dir: Option<&Path>) -> String {
    let file = if filename.is_empty() {
        format!("{name} 对应的 .whl")
    } else {
        filename.to_string()
    };
    match dir {
        Some(dir) => format!(
            "缺少本地 wheel：把 {file} 放进 wheel 库（{}）后重试",
            dir.display()
        ),
        None => {
            format!("缺少本地 wheel：{file}。先在设置里选一个存在的“本地 Wheel 库”目录，把它放进去再重试")
        }
    }
}

/// 恢复前检查：这次要装的包里，哪些 wheel 在库里找不到文件。
///
/// 按 URL 安装的包不算缺——恢复时直接按记录的 URL 重装。已经装到目标版本的包也
/// 不算，恢复不会动它。
pub fn missing_restore_wheels(
    target: &Snapshot,
    current: &BTreeMap<String, String>,
    settings: &GlobalSettings,
) -> Vec<crate::sync::MissingWheel> {
    let plan = plan_packages(current, &target.packages);
    let dir = crate::wheelhouse::dir(settings);
    let mut out = Vec::new();
    for (name, version) in &plan.install {
        let Some(wheel) = target.wheels.get(&crate::sync::canonical_dist_name(name)) else {
            continue;
        };
        if wheel.url.is_some() {
            continue;
        }
        let want = normalize_package_version(version);
        let filename = wheel.filename.clone().unwrap_or_default();
        let found = dir
            .as_deref()
            .and_then(|dir| crate::wheelhouse::find(dir, name, &want, Some(&filename)));
        if found.is_some() {
            continue;
        }
        out.push(crate::sync::MissingWheel {
            name: name.clone(),
            version: version.clone(),
            filename: filename.clone(),
            source: "local-wheel".to_string(),
            reason: missing_wheel_hint(&filename, name, dir.as_deref()),
        });
    }
    out
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreEvent {
    pub step: String,
    pub phase: String,
    pub message: String,
    pub percent: u8,
}

impl RestoreEvent {
    fn new(step: &str, phase: &str, percent: u8, message: impl Into<String>) -> Self {
        Self {
            step: step.to_string(),
            phase: phase.to_string(),
            percent,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoreOutcome {
    pub changed: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOutcome {
    pub changed: bool,
    pub from: Option<ServerOpts>,
    pub to: Option<ServerOpts>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NameError {
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeOutcome {
    pub installed: Vec<String>,
    pub removed: Vec<String>,
    pub switched: Vec<String>,
    #[serde(default)]
    pub content_restored: Vec<String>,
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub failed: Vec<NameError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PkgOutcome {
    pub installed: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<PkgDelta>,
    pub protected_skipped: Vec<ProtectedDrift>,
    pub failed: Vec<NameError>,
    /// 卸载/校准后仍然和快照不一致的包（正常应为空）。
    #[serde(default)]
    pub leftover: Vec<String>,
}

/// 给命令层用：实例 ComfyUI 目录当前 HEAD。
pub fn head_comfy(root: &str) -> Option<String> {
    read_head(&Path::new(root).join("ComfyUI"))
}

/// 恢复会动多少东西（预览用）。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RestoreTargets {
    pub core_commit: Option<String>,
    pub core_changed: bool,
    pub launch_changed: bool,
    pub nodes_install: usize,
    pub nodes_switch: usize,
    pub nodes_content: usize,
    pub nodes_remove: usize,
    pub nodes_enable: usize,
    pub nodes_disable: usize,
    pub packages_install: usize,
    pub packages_remove: usize,
    pub packages_protected: Vec<ProtectedDrift>,
}

pub fn restore_targets(current: &Snapshot, target: &Snapshot) -> RestoreTargets {
    let nodes = plan_nodes(&current.nodes, &target.nodes);
    let packages = plan_packages(&current.packages, &target.packages);
    RestoreTargets {
        core_changed: current.comfyui.commit != target.comfyui.commit,
        core_commit: target.comfyui.commit.clone(),
        launch_changed: target.server.is_some() && target.server != current.server,
        nodes_install: nodes.install.len(),
        nodes_switch: nodes.switch.len(),
        nodes_content: nodes.content.len(),
        nodes_remove: nodes.remove.len(),
        nodes_enable: nodes.enable.len(),
        nodes_disable: nodes.disable.len(),
        packages_install: packages.install.len(),
        packages_remove: packages.remove.len(),
        packages_protected: packages.protected,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RestoreReport {
    pub target_id: String,
    pub pre_snapshot: Option<String>,
    /// 历史字段；现在恢复完成后不再自动拍快照，恒为 None。
    pub post_snapshot: Option<String>,
    pub core: CoreOutcome,
    pub launch: LaunchOutcome,
    pub nodes: NodeOutcome,
    pub packages: PkgOutcome,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOptions {
    /// 快照里没有的节点：删掉（默认）。
    #[serde(default = "yes")]
    pub remove_extra_nodes: bool,
    /// 快照里没有的包：卸掉（默认）。
    #[serde(default = "yes")]
    pub remove_extra_packages: bool,
}

fn yes() -> bool {
    true
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            remove_extra_nodes: true,
            remove_extra_packages: true,
        }
    }
}

pub struct RestoreCtx<'a> {
    pub inst: &'a InstanceConfig,
    pub settings: &'a GlobalSettings,
    pub git: &'a Path,
    pub uv: &'a Path,
}

fn comfy_dir(inst: &InstanceConfig) -> PathBuf {
    Path::new(&inst.root_dir).join("ComfyUI")
}

fn nodes_dir(inst: &InstanceConfig) -> PathBuf {
    comfy_dir(inst).join("custom_nodes")
}

fn node_path(inst: &InstanceConfig, name: &str, enabled: bool) -> PathBuf {
    let dir = nodes_dir(inst);
    if enabled {
        dir.join(name)
    } else {
        dir.join(format!("{name}.disabled"))
    }
}

fn git_run(git: &Path, dir: &Path, args: &[&str]) -> Result<proc::CmdOutput, AppError> {
    proc::run(git, args, Some(dir), &proc::git_env())
}

fn head_of(git: &Path, dir: &Path) -> Option<String> {
    read_head(dir).or_else(|| {
        git_run(git, dir, &["rev-parse", "HEAD"])
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn checkout(git: &Path, dir: &Path, commit: &str) -> Result<(), String> {
    let out = git_run(git, dir, &["checkout", "-f", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        return Ok(());
    }
    // 本地没有这个提交：拉一次再。
    let _ = git_run(git, dir, &["fetch", "origin", "--tags", "--quiet"]);
    let out = git_run(git, dir, &["checkout", "-f", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        return Ok(());
    }
    let _ = git_run(
        git,
        dir,
        &["fetch", "origin", commit, "--depth", "1", "--quiet"],
    );
    let out = git_run(git, dir, &["checkout", "-f", "-q", commit]).map_err(|e| e.message)?;
    if out.ok() {
        return Ok(());
    }
    Err(out.detail())
}

fn clone_node(git: &Path, url: &str, dest: &Path, commit: Option<&str>) -> Result<(), String> {
    let out = proc::run(
        git,
        &["clone", "--quiet", url, &dest.to_string_lossy()],
        None,
        &proc::git_env(),
    )
    .map_err(|e| e.message)?;
    if !out.ok() {
        let _ = std::fs::remove_dir_all(dest);
        return Err(out.detail());
    }
    if let Some(commit) = commit {
        if let Err(e) = checkout(git, dest, commit) {
            let _ = std::fs::remove_dir_all(dest);
            return Err(format!(
                "clone 成功但切换到 {} 失败：{e}",
                crate::sync::short_chars(commit, 7)
            ));
        }
    }
    Ok(())
}

/// 构造 uv pip 的参数。镜像参数只用于 install。
/// `uv pip uninstall` 不接 `--index-url`，否则整条卸载命令直接报参数错误。
fn pip_command(args: &[String], python: &Path, index: Option<&str>) -> Vec<String> {
    let mut full: Vec<String> = vec![
        "pip".to_string(),
        args[0].clone(),
        "--python".to_string(),
        python.to_string_lossy().to_string(),
    ];
    full.extend(args[1..].iter().cloned());
    if args.first().map(String::as_str) == Some("install") {
        if let Some(index) = index.map(str::trim).filter(|s| !s.is_empty()) {
            full.push("--index-url".to_string());
            full.push(index.to_string());
        }
    }
    full
}

fn uv_pip(ctx: &RestoreCtx<'_>, python: &Path, args: &[String]) -> Result<String, String> {
    let mut env = uv_env();
    let index = ctx
        .settings
        .mirrors
        .pypi
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let full = pip_command(args, python, index);
    env.insert(
        "VIRTUAL_ENV".to_string(),
        Path::new(&ctx.inst.root_dir)
            .join(".venv")
            .to_string_lossy()
            .to_string(),
    );
    let argv: Vec<&str> = full.iter().map(String::as_str).collect();
    let out = proc::run(ctx.uv, &argv, Some(Path::new(&ctx.inst.root_dir)), &env)
        .map_err(|e| e.message)?;
    if out.ok() {
        Ok(format!("{}{}", out.stdout, out.stderr))
    } else {
        Err(out.detail())
    }
}

/// 执行恢复。调用方负责：实例已停止、同一实例没有别的恢复在跑。
pub fn restore(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    opts: RestoreOptions,
    emit: &mut dyn FnMut(RestoreEvent),
) -> Result<RestoreReport, AppError> {
    if target.version != SNAPSHOT_VERSION {
        return Err(AppError::new(
            "SNAPSHOT_VERSION",
            format!("快照格式版本 {} 不认。", target.version),
        ));
    }
    let dir = comfy_dir(ctx.inst);
    if !dir.join(".git").exists() {
        return Err(AppError::new(
            "NO_GIT",
            "这个实例的 ComfyUI 不是 git 仓库，无法恢复版本。",
        ));
    }
    let before_commit = head_of(ctx.git, &dir);
    if let Some(commit) = target
        .comfyui
        .commit
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        let exists = git_run(
            ctx.git,
            &dir,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )?;
        if !exists.ok() {
            let _ = git_run(ctx.git, &dir, &["fetch", "origin", "--tags", "--quiet"]);
            let retry = git_run(
                ctx.git,
                &dir,
                &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            )?;
            if !retry.ok() {
                return Err(AppError::new(
                    "COMMIT_MISSING",
                    format!("仓库里找不到快照记录的提交 {commit}"),
                ));
            }
        }
    }

    let mut report = RestoreReport {
        target_id: target.id.clone(),
        ..Default::default()
    };

    emit(RestoreEvent::new(
        "snapshot",
        "log",
        4,
        format!(
            "目标快照 {}：ComfyUI {} · Python {} · torch {} · {} 个包",
            target.id,
            target.comfyui.version.as_deref().unwrap_or("未记录"),
            target.python.version,
            target.python.torch.as_deref().unwrap_or("未记录"),
            target.packages.len()
        ),
    ));

    // 0. 先给自己留退路
    emit(RestoreEvent::new(
        "snapshot",
        "start",
        3,
        "拍一条恢复前的快照",
    ));
    let pre = capture_and_save(
        ctx.inst,
        ctx.settings,
        T_PRE_RESTORE,
        Some("pre-restore".into()),
    )?;
    report.pre_snapshot = Some(pre.id.clone());
    emit(RestoreEvent::new(
        "snapshot",
        "done",
        6,
        format!("已保存恢复前快照 {}", pre.id),
    ));

    // 1. ComfyUI 版本
    emit(RestoreEvent::new("core", "start", 10, "恢复 ComfyUI 版本"));
    let core = restore_core(ctx, target, before_commit.clone(), emit);
    report.core = core;

    // 2. 节点
    emit(RestoreEvent::new("nodes", "start", 35, "恢复自定义节点"));
    report.nodes = restore_nodes(ctx, target, opts, emit);

    // 3. 恢复包
    emit(RestoreEvent::new("packages", "start", 65, "恢复 Python 包"));
    report.packages = restore_packages(ctx, target, opts, emit);

    // 4. 启动参数：老快照没有这一项就保持目标机当前值
    emit(RestoreEvent::new("launch", "start", 90, "恢复启动参数"));
    if let Some(server) = target.server.clone() {
        report.launch = LaunchOutcome {
            changed: server != ctx.inst.server,
            from: Some(ctx.inst.server.clone()),
            to: Some(server.clone()),
        };
        emit(RestoreEvent::new(
            "launch",
            if report.launch.changed { "done" } else { "log" },
            93,
            if report.launch.changed {
                format!("启动参数已恢复：{}:{}", server.listen, server.port)
            } else {
                "启动参数与快照一致".to_string()
            },
        ));
    } else {
        emit(RestoreEvent::new(
            "launch",
            "warn",
            93,
            "这条老快照没有启动参数，保持当前设置",
        ));
    }

    // 恢复成功就结束：不自动再拍「恢复后」快照。
    // 需要留档时用户手动拍；恢复前那一张 pre-restore 快照就是回滚点。
    emit(RestoreEvent::new("snapshot", "done", 100, "恢复完成"));
    Ok(report)
}

fn restore_core(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    before: Option<String>,
    emit: &mut dyn FnMut(RestoreEvent),
) -> CoreOutcome {
    let dir = comfy_dir(ctx.inst);
    let mut outcome = CoreOutcome {
        from: before.clone(),
        ..Default::default()
    };
    let Some(want) = target
        .comfyui
        .commit
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        emit(RestoreEvent::new("core", "log", 30, "快照没记录版本，跳过"));
        return outcome;
    };
    let some = |sha: &Option<String>| {
        sha.as_deref()
            .map(|s| crate::sync::short_chars(s, 12))
            .unwrap_or_default()
    };
    if let Some(cur) = before.as_deref() {
        if cur.starts_with(want) || want.starts_with(cur) {
            outcome.to = Some(cur.to_string());
            emit(RestoreEvent::new(
                "core",
                "done",
                30,
                format!("版本已经是 {}", some(&outcome.to)),
            ));
            return outcome;
        }
    }
    emit(RestoreEvent::new(
        "core",
        "log",
        15,
        format!("checkout {}", crate::sync::short_chars(want, 7)),
    ));
    match checkout(ctx.git, &dir, want) {
        Ok(()) => {
            outcome.changed = true;
            outcome.to = head_of(ctx.git, &dir);
            emit(RestoreEvent::new(
                "core",
                "done",
                32,
                format!(
                    "ComfyUI 已切换到 {}（原来是 {}。",
                    some(&outcome.to),
                    some(&before)
                ),
            ));
        }
        Err(e) => {
            outcome.error = Some(e.clone());
            emit(RestoreEvent::new(
                "core",
                "warn",
                32,
                format!("切换版本失败：{e}"),
            ));
        }
    }
    outcome
}

fn node_payload_file(inst: &InstanceConfig, target: &Snapshot, name: &str, ext: &str) -> PathBuf {
    payload_dir(&dir(inst), &target.id).join(format!("{}.{ext}", safe_stem(name)))
}

fn replace_node_from_zip(
    inst: &InstanceConfig,
    target: &Snapshot,
    name: &str,
    dir: &Path,
) -> Result<(), String> {
    let zip = node_payload_file(inst, target, name, "zip");
    if !zip.is_file() {
        return Err("快照缺少手工节点归档，无法还原内容。".into());
    }
    // 先解到临时目录，成功后再替换；压缩包损坏时不会先把现有节点删掉。
    let tmp = dir.with_file_name(format!(
        ".{}.snapshot-restore",
        dir.file_name().and_then(|n| n.to_str()).unwrap_or("node")
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = crate::sync::extract_zip(&zip, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("解包节点失败：{}", e.message));
    }
    // 先把旧目录改名而不是直接删：rename 失败时还能换回来，不会出现
    // 「解包成功但旧节点消失」的中间态。
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("node");
    let instance_root = dir
        .ancestors()
        .nth(3)
        .or_else(|| dir.parent())
        .unwrap_or(dir);
    let old = instance_root
        .join(".comfyfleet")
        .join("restore-old")
        .join(format!(
            "{name}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
        ));
    if let Some(p) = old.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("建旧节点备份目录失败：{e}"))?;
    }
    let had_old = dir.exists();
    if had_old {
        std::fs::rename(dir, &old).map_err(|e| format!("备份旧节点失败：{e}"))?;
    }
    if let Err(e) = std::fs::rename(&tmp, dir) {
        let _ = std::fs::remove_dir_all(&tmp);
        if had_old {
            let _ = std::fs::rename(&old, dir);
        }
        return Err(format!("替换节点目录失败：{e}"));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// 把节点目录里非忽略的未跟踪文件移到旁边，返回备份目录（没有就 None）。
/// 备份目录用 `.` 开头，ComfyUI 扫描 custom_nodes 时会忽略它。
fn backup_untracked(git: &Path, dir: &Path) -> Result<Option<PathBuf>, String> {
    let untracked = git_untracked(git, dir).unwrap_or_default();
    if untracked.is_empty() {
        return Ok(None);
    }
    let parent = dir.parent().ok_or("节点目录没有上级目录，无法备份")?;
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("node");
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
    // 不能放在 custom_nodes 里：ComfyUI 会扫描该目录，备份可能被当成第二个节点加载。
    // node → custom_nodes → ComfyUI → 实例根，备份放到实例根的 .comfyfleet 下。
    let instance_root = dir.ancestors().nth(3).unwrap_or(parent);
    let backup = instance_root
        .join(".comfyfleet")
        .join("restore-untracked")
        .join(format!("{name}-{stamp}"));
    std::fs::create_dir_all(&backup).map_err(|e| format!("建未跟踪文件备份目录失败：{e}"))?;
    for rel in untracked {
        let src = dir.join(&rel);
        if !src.exists() {
            continue;
        }
        let dest = backup.join(&rel);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("建备份子目录失败：{e}"))?;
        }
        if let Err(e) = std::fs::rename(&src, &dest) {
            return Err(format!("备份未跟踪文件 {rel} 失败：{e}（未继续清理）"));
        }
    }
    Ok(Some(backup))
}

/// 节点工作区可能有未提交改动；先清干净 checkout，避免 git 拒绝切换。
/// 把旧改动带到快照版本上。调用方已进入恢复流程，用户确认过会覆盖。
fn clean_to_commit(git: &Path, dir: &Path, commit: &str) -> Result<(), String> {
    // 先确认提交真的存在，再动工作区。checkout 内部会 fetch，但那是清空之后。
    // 离线/commit 被 force-push 掉时，用户会先丢未提交改动再看到失败。
    let has_commit = |c: &str| -> bool {
        git_run(git, dir, &["cat-file", "-e", &format!("{c}^{{commit}}")])
            .map(|out| out.ok())
            .unwrap_or(false)
    };
    if !has_commit(commit) {
        let _ = git_run(git, dir, &["fetch", "origin", "--tags", "--quiet"]);
    }
    if !has_commit(commit) {
        let _ = git_run(
            git,
            dir,
            &["fetch", "origin", commit, "--depth", "1", "--quiet"],
        );
    }
    if !has_commit(commit) {
        return Err(format!(
            "目标提交 {} 不在仓库中（离线或已被远端删除），未改动当前工作区",
            crate::sync::short_chars(commit, 12)
        ));
    }
    // 不能把 `git clean -fdq` 直接删：节点目录里常有用户放的模型、配置或脚本。
    // 恢复确认弹窗并没有列出这些路径。先把未跟踪文件移到节点旁边的备份目录，
    // 再清工作区；这样既能恢复到快照，也不会静默丢数据。
    let backup = backup_untracked(git, dir)?;
    if let Some(path) = &backup {
        log::warn!(
            "恢复节点时发现未跟踪文件，已移动到 {}（确认不需要后可删除）",
            path.display()
        );
    }
    let _ = git_run(git, dir, &["reset", "--hard", "-q"]);
    checkout(git, dir, commit)?;
    let out = git_run(git, dir, &["reset", "--hard", "-q", commit]).map_err(|e| e.message)?;
    if !out.ok() {
        return Err(out.detail());
    }
    Ok(())
}

fn apply_node_patch(
    git: &Path,
    inst: &InstanceConfig,
    target: &Snapshot,
    name: &str,
    dir: &Path,
) -> Result<(), String> {
    let patch = node_payload_file(inst, target, name, "patch");
    let untracked = node_payload_file(inst, target, name, "untracked.zip");
    if !patch.is_file() && !untracked.is_file() {
        return Err("快照缺少未提交改动归档，无法还原".into());
    }
    if patch.is_file()
        && std::fs::metadata(&patch)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    {
        let patch_text = patch.to_string_lossy().to_string();
        let out = git_run(
            git,
            dir,
            &["apply", "--binary", "--whitespace=nowarn", &patch_text],
        )
        .map_err(|e| e.message)?;
        if !out.ok() {
            return Err(format!("应用未提交改动失败：{}", out.detail()));
        }
    }
    if untracked.is_file() {
        crate::sync::extract_zip(&untracked, dir)
            .map_err(|e| format!("还原未跟踪文件失败：{}", e.message))?;
    }
    Ok(())
}

fn restore_present_content(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    cur: &SnapshotNode,
    want: &SnapshotNode,
) -> Result<(), String> {
    let dir = node_path(ctx.inst, &want.name, cur.enabled);
    match want.content.as_deref() {
        Some("full") => replace_node_from_zip(ctx.inst, target, &want.name, &dir),
        Some("patch") => {
            let Some(commit) = want.commit.as_deref().filter(|c| !c.trim().is_empty()) else {
                return Err("快照没有记录节点提交，无法还原未提交改动".into());
            };
            clean_to_commit(ctx.git, &dir, commit)?;
            apply_node_patch(ctx.git, ctx.inst, target, &want.name, &dir)
        }
        Some(other) => Err(format!("不认识的节点内容模式：{other}")),
        None => {
            // 目标是干净 git 节点：清掉工作区里多出来的修改和未跟踪文件。
            let Some(commit) = want.commit.as_deref().filter(|c| !c.trim().is_empty()) else {
                return Ok(());
            };
            clean_to_commit(ctx.git, &dir, commit)
        }
    }
}

/// 当前节点不是独立 git 仓库（例如 `.git` 已丢失、只剩文件）时，
/// 不能拿父仓库去 checkout 快照提交；按快照的仓库地址重新克隆并替换目录。
fn replace_node_from_clone(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    node: &SnapshotNode,
    dest: &Path,
) -> Result<(), String> {
    let Some(url) = node.url.as_deref().filter(|u| !u.trim().is_empty()) else {
        return Err("快照没记录仓库地址，无法重新克隆".into());
    };
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("node");
    let tmp = dest.with_file_name(format!(".{name}.snapshot-clone"));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = clone_node(ctx.git, url, &tmp, node.commit.as_deref()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if node.content.as_deref() == Some("patch") {
        if let Err(e) = apply_node_patch(ctx.git, ctx.inst, target, &node.name, &tmp) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    }

    let instance_root = dest
        .ancestors()
        .nth(3)
        .or_else(|| dest.parent())
        .unwrap_or(dest);
    let old = instance_root
        .join(".comfyfleet")
        .join("restore-old")
        .join(format!(
            "{name}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
        ));
    if let Some(p) = old.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("建旧节点备份目录失败：{e}"))?;
    }
    let had_old = dest.exists();
    if had_old {
        std::fs::rename(dest, &old).map_err(|e| format!("备份旧节点失败：{e}"))?;
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        if had_old {
            let _ = std::fs::rename(&old, dest);
        }
        return Err(format!("替换节点目录失败：{e}"));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

fn restore_nodes(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    opts: RestoreOptions,
    emit: &mut dyn FnMut(RestoreEvent),
) -> NodeOutcome {
    let dir = nodes_dir(ctx.inst);
    let mut outcome = NodeOutcome::default();
    let current = capture_nodes(ctx.inst, ctx.settings);
    let plan = plan_nodes(&current, &target.nodes);

    // 多余节点：直接删（与官方一致），文件名先做安全校验
    if opts.remove_extra_nodes {
        for node in &plan.remove {
            if !is_safe_node_name(&node.name) {
                outcome.failed.push(NameError {
                    name: node.name.clone(),
                    error: "目录名不合法，跳过".into(),
                });
                continue;
            }
            let path = node_path(ctx.inst, &node.name, node.enabled);
            if path.parent() != Some(dir.as_path()) {
                continue;
            }
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => {
                    outcome.removed.push(node.name.clone());
                    emit(RestoreEvent::new(
                        "nodes",
                        "log",
                        40,
                        format!("已删除多余节点 {}", node.name),
                    ));
                }
                Err(e) => outcome.failed.push(NameError {
                    name: node.name.clone(),
                    error: format!("删除失败：{e}"),
                }),
            }
        }
    } else if !plan.remove.is_empty() {
        emit(RestoreEvent::new(
            "nodes",
            "log",
            40,
            format!("{} 个快照里没有的节点保留未。", plan.remove.len()),
        ));
    }

    let total = plan.install.len()
        + plan.switch.len()
        + plan.content.len()
        + plan.enable.len()
        + plan.disable.len();
    let mut done = 0usize;

    // 缺的装回去
    for node in &plan.install {
        done += 1;
        let percent = 42 + ((done as f32 / total.max(1) as f32) * 20.0) as u8;
        if !is_safe_node_name(&node.name) {
            outcome.failed.push(NameError {
                name: node.name.clone(),
                error: "目录名不合法".into(),
            });
            continue;
        }
        let dest = node_path(ctx.inst, &node.name, true);
        if node.content.as_deref() == Some("missing") {
            outcome.failed.push(NameError {
                name: node.name.clone(),
                error: "快照的节点内容归档缺失，无法还原".into(),
            });
            emit(RestoreEvent::new(
                "nodes",
                "warn",
                percent,
                format!("{} 的内容归档缺失，跳过", node.name),
            ));
            continue;
        }
        let installed = if node.content.as_deref() == Some("full") {
            emit(RestoreEvent::new(
                "nodes",
                "log",
                percent,
                format!("解包手工节点 {}", node.name),
            ));
            replace_node_from_zip(ctx.inst, target, &node.name, &dest)
        } else {
            let Some(url) = node.url.as_deref().filter(|u| !u.trim().is_empty()) else {
                outcome.failed.push(NameError {
                    name: node.name.clone(),
                    error: "快照没记录仓库地址，也没有本地内容归档".into(),
                });
                emit(RestoreEvent::new(
                    "nodes",
                    "warn",
                    percent,
                    format!("{} 没有仓库地址，跳过", node.name),
                ));
                continue;
            };
            emit(RestoreEvent::new(
                "nodes",
                "log",
                percent,
                format!("克隆 {}", node.name),
            ));
            match clone_node(ctx.git, url, &dest, node.commit.as_deref()) {
                Ok(()) if node.content.as_deref() == Some("patch") => {
                    apply_node_patch(ctx.git, ctx.inst, target, &node.name, &dest)
                }
                Ok(()) => Ok(()),
                Err(e) => Err(e),
            }
        };
        match installed {
            Ok(()) => {
                outcome.installed.push(node.name.clone());
                if !node.enabled {
                    let disabled = node_path(ctx.inst, &node.name, false);
                    if std::fs::rename(&dest, &disabled).is_ok() {
                        outcome.disabled.push(node.name.clone());
                    }
                }
                emit(RestoreEvent::new(
                    "nodes",
                    "log",
                    percent,
                    format!("已装好 {}", node.name),
                ));
            }
            Err(e) => {
                outcome.failed.push(NameError {
                    name: node.name.clone(),
                    error: e.clone(),
                });
                emit(RestoreEvent::new(
                    "nodes",
                    "warn",
                    percent,
                    format!("{} 安装失败：{e}", node.name),
                ));
            }
        }
    }

    // 版本不同/启用状态不同
    for (cur, want) in &plan.switch {
        done += 1;
        let percent = 42 + ((done as f32 / total.max(1) as f32) * 20.0) as u8;
        if !is_safe_node_name(&want.name) {
            outcome.failed.push(NameError {
                name: want.name.clone(),
                error: "目录名不合法，跳过".into(),
            });
            continue;
        }
        let path = node_path(ctx.inst, &want.name, cur.enabled);
        let Some(commit) = want.commit.as_deref() else {
            outcome.failed.push(NameError {
                name: want.name.clone(),
                error: "快照没记录节点提交，无法切换".into(),
            });
            continue;
        };
        // 目录里没有自己的 .git 时，`git -C` 会一路找到 ComfyUI 核心仓库，
        // 把快照里的节点提交误当成核心仓库的提交。这种情况直接重新克隆。
        let own_git = path.join(".git").exists();
        let result = if !own_git || cur.commit.is_none() {
            replace_node_from_clone(ctx, target, want, &path)
        } else {
            clean_to_commit(ctx.git, &path, commit)
        };
        match result {
            Ok(()) => {
                outcome.switched.push(want.name.clone());
                emit(RestoreEvent::new(
                    "nodes",
                    "log",
                    percent,
                    format!(
                        "{} 已切换到 {}",
                        want.name,
                        crate::sync::short_chars(commit, 7)
                    ),
                ));
            }
            Err(e) => {
                outcome.failed.push(NameError {
                    name: want.name.clone(),
                    error: e,
                });
            }
        }
    }

    // 目录还在，但手工内容或未提交改动和快照不一致
    for (cur, want) in &plan.content {
        done += 1;
        let percent = 42 + ((done as f32 / total.max(1) as f32) * 20.0) as u8;
        if !is_safe_node_name(&want.name) {
            outcome.failed.push(NameError {
                name: want.name.clone(),
                error: "目录名不合法，跳过".into(),
            });
            continue;
        }
        match restore_present_content(ctx, target, cur, want) {
            Ok(()) => {
                outcome.content_restored.push(want.name.clone());
                emit(RestoreEvent::new(
                    "nodes",
                    "log",
                    percent,
                    format!("{} 的本地内容已还原", want.name),
                ));
            }
            Err(e) => {
                outcome.failed.push(NameError {
                    name: want.name.clone(),
                    error: e.clone(),
                });
                emit(RestoreEvent::new(
                    "nodes",
                    "warn",
                    percent,
                    format!("{} 内容还原失败：{e}", want.name),
                ));
            }
        }
    }

    for node in &plan.enable {
        if !is_safe_node_name(&node.name) {
            continue;
        }
        match std::fs::rename(
            node_path(ctx.inst, &node.name, false),
            node_path(ctx.inst, &node.name, true),
        ) {
            Ok(()) => outcome.enabled.push(node.name.clone()),
            Err(e) => outcome.failed.push(NameError {
                name: node.name.clone(),
                error: format!("启用失败：{e}"),
            }),
        }
    }
    for node in &plan.disable {
        if !is_safe_node_name(&node.name) {
            continue;
        }
        match std::fs::rename(
            node_path(ctx.inst, &node.name, true),
            node_path(ctx.inst, &node.name, false),
        ) {
            Ok(()) => outcome.disabled.push(node.name.clone()),
            Err(e) => outcome.failed.push(NameError {
                name: node.name.clone(),
                error: format!("禁用失败：{e}"),
            }),
        }
    }

    emit(RestoreEvent::new(
        "nodes",
        "done",
        62,
        format!(
            "节点：装 {} · 切换 {} · 还原内容 {} · 启用 {} · 禁用 {} · 删除 {}{}",
            outcome.installed.len(),
            outcome.switched.len(),
            outcome.content_restored.len(),
            outcome.enabled.len(),
            outcome.disabled.len(),
            outcome.removed.len(),
            if outcome.failed.is_empty() {
                String::new()
            } else {
                format!(" · {} 个失败", outcome.failed.len())
            }
        ),
    ));
    outcome
}

/// uv 的完整输出进控制台；保留尾部，避免几万行刷爆界面。
fn emit_pip_output(emit: &mut dyn FnMut(RestoreEvent), percent: u8, command: &str, output: &str) {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return;
    }
    let start = lines.len().saturating_sub(16);
    emit(RestoreEvent::new(
        "packages",
        "log",
        percent,
        format!("{command}：\n{}", lines[start..].join("\n")),
    ));
}

/// 包名归一化：PEP 503 的 `-` / `.` / `_` 等价，比较 dist-info 目录时用。
fn normalize_dist_name(name: &str) -> String {
    name.trim().to_lowercase().replace(['-', '.'], "_")
}

/// 没有 RECORD 的包，`uv pip uninstall` 不会动它。
/// 这里按 dist-info / top_level.txt 直接清掉模块和元数据，作为卸载的回退。
fn remove_package_leftovers(
    python: &Path,
    root: &Path,
    name: &str,
) -> Result<Vec<PathBuf>, String> {
    let script = "import sysconfig; print(sysconfig.get_paths()['purelib'])";
    let out = proc::run(python, &["-c", script], Some(root), &uv_env())
        .map_err(|e| format!("定位 site-packages 失败：{}", e.message))?;
    if !out.ok() {
        return Err(format!("定位 site-packages 失败：{}", out.detail()));
    }
    let site = PathBuf::from(out.stdout.trim());
    cleanup_site_packages(&site, name)
}

fn cleanup_site_packages(site: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    if !site.is_dir() {
        return Err(format!("site-packages 不存在：{}", site.display()));
    }

    let wanted = normalize_dist_name(name);
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(site)
        .map_err(|e| format!("读取 site-packages 失败：{e}"))?
        .flatten()
        .collect();

    let mut meta_dirs: Vec<PathBuf> = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    for entry in &entries {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let lower = file_name.to_lowercase();
        if let Some(stem) = lower
            .strip_suffix(".dist-info")
            .or_else(|| lower.strip_suffix(".egg-info"))
        {
            let dist = normalize_dist_name(stem.split('-').next().unwrap_or(stem));
            if dist != wanted {
                continue;
            }
            meta_dirs.push(entry.path());
            if let Ok(text) = std::fs::read_to_string(entry.path().join("top_level.txt")) {
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if !modules.iter().any(|m| m == line) {
                        modules.push(line.to_string());
                    }
                }
            }
        }
    }
    if modules.is_empty() {
        modules.push(wanted.clone());
    }

    let mut removed = Vec::new();
    for entry in &entries {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let lower_name = file_name.to_lowercase();
        let is_meta = meta_dirs.iter().any(|p| p == &entry.path());
        if is_meta {
            continue;
        }
        let hit = modules.iter().any(|module| {
            let module = module.to_lowercase();
            lower_name == format!("{module}.py")
                || lower_name == format!("{module}.pyc")
                || lower_name == module
                || lower_name.starts_with(&format!("{module}."))
                || lower_name.starts_with(&format!("{module}_"))
        });
        if !hit || !entry.path().is_file() && !entry.path().is_dir() {
            continue;
        }
        let path = entry.path();
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed.push(path),
            Err(e) => {
                return Err(format!(
                    "删除 {} 失败：{e}（文件可能正被进程占用）",
                    path.display()
                ))
            }
        }
    }

    for path in &meta_dirs {
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => removed.push(path.clone()),
            Err(e) => {
                return Err(format!(
                    "删除 {} 失败：{e}（文件可能正被进程占用）",
                    path.display()
                ))
            }
        }
    }

    if removed.is_empty() {
        return Err("没有找到可清理的模块或 dist-info".into());
    }
    Ok(removed)
}

fn uninstall_packages(
    ctx: &RestoreCtx<'_>,
    python: &Path,
    plan: &[(String, String)],
    outcome: &mut PkgOutcome,
    emit: &mut dyn FnMut(RestoreEvent),
) {
    if plan.is_empty() {
        return;
    }
    emit(RestoreEvent::new(
        "packages",
        "log",
        82,
        format!("卸掉 {} 个快照里没有的包", plan.len()),
    ));
    for chunk in plan.chunks(40) {
        let names: Vec<String> = chunk.iter().map(|(name, _)| name.clone()).collect();
        emit(RestoreEvent::new(
            "packages",
            "log",
            83,
            format!("卸载依赖：{}", names.join(" ")),
        ));
        let mut args = vec!["uninstall".to_string()];
        args.extend(names.iter().cloned());
        match uv_pip(ctx, python, &args) {
            Ok(output) => {
                emit_pip_output(emit, 84, "uv pip uninstall", &output);
                for (name, _) in chunk {
                    outcome.failed.retain(|failure| failure.name != *name);
                    if !outcome.removed.iter().any(|existing| existing == name) {
                        outcome.removed.push(name.clone());
                    }
                }
            }
            Err(batch_error) => {
                emit(RestoreEvent::new(
                    "packages",
                    "log",
                    84,
                    format!("整批卸载失败：{batch_error}），逐个重试。"),
                ));
                for (name, _) in chunk {
                    let args = vec!["uninstall".to_string(), name.clone()];
                    match uv_pip(ctx, python, &args) {
                        Ok(output) => {
                            emit_pip_output(emit, 85, "uv pip uninstall", &output);
                            outcome.failed.retain(|failure| failure.name != *name);
                            if !outcome.removed.iter().any(|existing| existing == name) {
                                outcome.removed.push(name.clone());
                            }
                        }
                        Err(err) => {
                            match remove_package_leftovers(
                                python,
                                Path::new(&ctx.inst.root_dir),
                                name,
                            ) {
                                Ok(removed) => {
                                    emit(RestoreEvent::new(
                                        "packages",
                                        "log",
                                        85,
                                        format!(
                                            "{} 没有 RECORD，uv 无法卸载；已清理 {} 个残留路径",
                                            name,
                                            removed.len()
                                        ),
                                    ));
                                    outcome.failed.retain(|failure| failure.name != *name);
                                    if !outcome.removed.iter().any(|existing| existing == name) {
                                        outcome.removed.push(name.clone());
                                    }
                                }
                                Err(fallback) => outcome.failed.push(NameError {
                                    name: name.clone(),
                                    error: format!("{err}；清理残留失败：{fallback}"),
                                }),
                            }
                        }
                    }
                }
            }
        }
    }
}

fn restore_packages(
    ctx: &RestoreCtx<'_>,
    target: &Snapshot,
    opts: RestoreOptions,
    emit: &mut dyn FnMut(RestoreEvent),
) -> PkgOutcome {
    let mut outcome = PkgOutcome::default();
    let Some(python) = venv_python(ctx.inst) else {
        outcome.failed.push(NameError {
            name: "python".into(),
            error: "找不到虚拟环境，跳过包恢复。".into(),
        });
        return outcome;
    };
    let current = match installed_packages(ctx.inst, ctx.uv, &python) {
        Ok(p) => p,
        Err(e) => {
            outcome.failed.push(NameError {
                name: "pip freeze".into(),
                error: e,
            });
            return outcome;
        }
    };
    emit(RestoreEvent::new(
        "packages",
        "log",
        66,
        format!(
            "Python 环境：{} · 当前 {} 个包 · 快照 {} 个包",
            python.display(),
            current.len(),
            target.packages.len()
        ),
    ));
    // 旧快照采集失败时 packages 可能是空 map；不能把它解释成“目标环境应该没有包”，
    // 否则 remove_extra_packages 会把除保护包外的整个 venv 卸掉。
    if target.packages.is_empty() && !current.is_empty() {
        emit(RestoreEvent::new(
            "packages",
            "warn",
            70,
            "快照没有包清单，跳过包恢复（不会卸载当前环境）",
        ));
        outcome.failed.push(NameError {
            name: "packages".into(),
            error: "快照没有包清单，已跳过包恢复".into(),
        });
        return outcome;
    }
    let mut plan = plan_packages(&current, &target.packages);
    if !opts.remove_extra_packages {
        plan.remove.clear();
    }
    outcome.protected_skipped = plan.protected.clone();
    if plan.is_empty() {
        emit(RestoreEvent::new("packages", "done", 92, "包无需改动"));
        return outcome;
    }
    // 本地 wheel 只从这个目录取；没配置就按“没有库”处理，缺的包会报出来。
    let wheels_dir = crate::wheelhouse::dir(ctx.settings);

    if !plan.install.is_empty() {
        emit(RestoreEvent::new(
            "packages",
            "log",
            72,
            format!("安装/切换 {} 个包。", plan.install.len()),
        ));
        for chunk in plan.install.chunks(40) {
            let mut specs: Vec<String> = Vec::with_capacity(chunk.len());
            // 名字 → 实际安装目标（库里的轮子路径 / URL / name==version）
            let mut by_name: BTreeMap<&str, String> = BTreeMap::new();
            let mut missing: Vec<(String, String)> = Vec::new();
            for (name, version) in chunk {
                match package_spec(name, version, target, wheels_dir.as_deref()) {
                    PackageTarget::Spec(spec) => {
                        by_name.insert(name.as_str(), spec.clone());
                        specs.push(spec);
                    }
                    PackageTarget::Missing(reason) => {
                        emit(RestoreEvent::new(
                            "packages",
                            "warn",
                            73,
                            format!("{name}：{reason}（已跳过，其余包继续）"),
                        ));
                        missing.push((name.clone(), reason));
                    }
                }
            }
            if specs.is_empty() {
                outcome.failed.extend(
                    missing
                        .into_iter()
                        .map(|(name, error)| NameError { name, error }),
                );
                continue;
            }
            emit(RestoreEvent::new(
                "packages",
                "log",
                73,
                format!("安装依赖：{}", specs.join(" ")),
            ));
            let mut args = vec!["install".to_string()];
            args.extend(specs.clone());
            match uv_pip(ctx, &python, &args) {
                Ok(output) => {
                    emit_pip_output(emit, 74, "uv pip install", &output);
                    for (name, version) in chunk {
                        if !by_name.contains_key(name.as_str()) {
                            continue; // 缺 wheel 的包已单独记为失败
                        }
                        if current.get(name).map(|v| v != version).unwrap_or(false) {
                            outcome.changed.push(PkgDelta {
                                name: name.clone(),
                                from: current.get(name).cloned().unwrap_or_default(),
                                to: version.clone(),
                            });
                        } else {
                            outcome.installed.push(name.clone());
                        }
                    }
                }
                Err(batch_error) => {
                    // 整批失败时逐个重试，把真正装不动的包点出来
                    emit(RestoreEvent::new(
                        "packages",
                        "log",
                        76,
                        format!("整批安装失败：{batch_error}），逐个重试。"),
                    ));
                    for (name, version) in chunk {
                        let Some(spec) = by_name.get(name.as_str()).cloned() else {
                            continue; // 缺 wheel 的包不重试
                        };
                        let one = vec!["install".to_string(), spec];
                        emit(RestoreEvent::new(
                            "packages",
                            "log",
                            76,
                            format!("重新安装：{name}=={version}"),
                        ));
                        match uv_pip(ctx, &python, &one) {
                            Ok(output) => {
                                emit_pip_output(emit, 77, "uv pip install", &output);
                                outcome.installed.push(name.clone());
                            }
                            Err(err) => outcome.failed.push(NameError {
                                name: name.clone(),
                                error: err,
                            }),
                        }
                    }
                }
            }
            outcome.failed.extend(
                missing
                    .into_iter()
                    .map(|(name, error)| NameError { name, error }),
            );
        }
    }

    uninstall_packages(ctx, &python, &plan.remove, &mut outcome, emit);

    // 安装目标包时，uv 会把目标快照里没有的依赖也带进来（升级/补齐依赖）。
    // 初次 plan 是安装前的快照，抓不到这些新包；装完再对一次账，保证环境向快照收敛。
    if opts.remove_extra_packages {
        match installed_packages(ctx.inst, ctx.uv, &python) {
            Ok(after_install) => {
                let reconcile = plan_packages(&after_install, &target.packages);
                if !reconcile.remove.is_empty() {
                    emit(RestoreEvent::new(
                        "packages",
                        "log",
                        86,
                        format!(
                            "安装过程带入了 {} 个快照里没有的包，重新对账卸载。",
                            reconcile.remove.len()
                        ),
                    ));
                    uninstall_packages(ctx, &python, &reconcile.remove, &mut outcome, emit);
                }
                match installed_packages(ctx.inst, ctx.uv, &python) {
                    Ok(after_cleanup) => {
                        let residual = plan_packages(&after_cleanup, &target.packages);
                        if !residual.remove.is_empty() {
                            outcome.leftover = residual
                                .remove
                                .iter()
                                .map(|(name, _)| name.clone())
                                .collect();
                            emit(RestoreEvent::new(
                                "packages",
                                "warn",
                                90,
                                format!(
                                    "仍有 {} 个包没卸掉：{}",
                                    outcome.leftover.len(),
                                    outcome.leftover.join("。")
                                ),
                            ));
                        }
                    }
                    Err(e) => outcome.failed.push(NameError {
                        name: "恢复后复核".into(),
                        error: format!("pip freeze 复核失败：{e}"),
                    }),
                }
            }
            Err(e) => outcome.failed.push(NameError {
                name: "安装后复核".into(),
                error: format!("pip freeze 复核失败：{e}"),
            }),
        }
    }

    emit(RestoreEvent::new(
        "packages",
        "done",
        92,
        format!(
            "包：安装 {} · 卸载 {} · 保护跳过 {}{}{}",
            outcome.installed.len() + outcome.changed.len(),
            outcome.removed.len(),
            outcome.protected_skipped.len(),
            if outcome.leftover.is_empty() {
                String::new()
            } else {
                format!(" · 仍多 {} 个", outcome.leftover.len())
            },
            if outcome.failed.is_empty() {
                String::new()
            } else {
                format!(" · {} 个失败", outcome.failed.len())
            }
        ),
    ));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalSettings;

    /// 测试用：自动快照（启动）上限；手/更新前后/恢复类永不自动删。
    const MAX_AUTO: usize = 200;

    fn snap(trigger: &str) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            id: format!("20260101_000000_000-{trigger}-aaaaaa"),
            created_at: "2026-01-01T00:00:00.000+08:00".to_string(),
            trigger: trigger.to_string(),
            label: None,
            comfyui: SnapshotCore {
                channel: "stable".into(),
                reference: "v0.43.1".into(),
                commit: Some("abc1234".into()),
                version: Some("0.43.1".into()),
            },
            python: SnapshotPython {
                version: "3.12".into(),
                torch: Some("2.6.0+cu124".into()),
            },
            server: None,
            nodes: vec![SnapshotNode {
                name: "ComfyUI-Manager".into(),
                url: Some("https://github.com/ltdrdata/ComfyUI-Manager".into()),
                commit: Some("deadb33".into()),
                enabled: true,
                ..Default::default()
            }],
            packages: BTreeMap::from([
                ("torch".to_string(), "2.6.0+cu124".to_string()),
                ("aiohttp".to_string(), "3.9.5".to_string()),
            ]),
            wheels: BTreeMap::new(),
            note: None,
        }
    }

    #[test]
    fn cleanup_removes_packages_without_record() {
        let site = std::env::temp_dir().join(format!("comfyfleet-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&site);
        std::fs::create_dir_all(&site).unwrap();

        // 没有 RECORD 的包：只剩 dist-info，模块目录可能已经不在。
        let telemetry = site.join("flash_attn-2.8.3.dist-info");
        std::fs::create_dir_all(&telemetry).unwrap();
        std::fs::write(telemetry.join("direct_url.json.bak"), "{}").unwrap();
        let module = site.join("flash_attn");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("__init__.py"), "").unwrap();

        // 同名前缀的无关包不能被误删。
        let keep = site.join("other_project");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("__init__.py"), "").unwrap();

        let removed = cleanup_site_packages(&site, "flash-attn").expect("应能清理");
        assert!(removed.iter().any(|p| p == &telemetry));
        assert!(!telemetry.exists());
        assert!(!module.exists());
        assert!(keep.exists());

        // 空 dist-info（sageattention 的情况）也必须能清掉。
        let empty = site.join("sageattention-2.2.0.dist-info");
        std::fs::create_dir_all(&empty).unwrap();
        cleanup_site_packages(&site, "sageattention").expect("空 dist-info 应能清理");
        assert!(!empty.exists());

        let _ = std::fs::remove_dir_all(&site);
    }

    #[test]
    fn freeze_parsing_uses_installed_versions() {
        let text = "-e git+https://github.com/x/y.git#egg=y\naiohttp==3.9.5\ntorch==2.6.0+cu124 ; sys_platform == \"win32\"\nweird @ file:///C:/tmp/pkg\nllama_cpp_python @ https://github.com/x/y/releases/download/v1/llama_cpp_python-0.3.46%2Bcu128-cp313-cp313-win_amd64.whl\n# comment\n";
        let actual = BTreeMap::from([
            ("llama-cpp-python".to_string(), "0.3.46+cu130".to_string()),
            ("aiohttp".to_string(), "3.9.6".to_string()),
        ]);
        let pkgs = parse_freeze_with_versions(text, &actual);
        assert_eq!(pkgs.get("aiohttp").map(String::as_str), Some("3.9.6"));
        assert_eq!(pkgs.get("torch").map(String::as_str), Some("2.6.0+cu124"));
        assert_eq!(pkgs.get("weird").map(String::as_str), Some(""));
        assert_eq!(
            pkgs.get("llama_cpp_python").map(String::as_str),
            Some("0.3.46+cu130")
        );
        assert_eq!(pkgs.len(), 4);

        // 没有已安装元数据时，不能退回 wheel 文件名猜版本。
        let no_metadata = parse_freeze_with_versions(text, &BTreeMap::new());
        assert_eq!(
            no_metadata.get("llama_cpp_python").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn old_snapshot_without_wheels_still_parses() {
        let json = r#"{
          "version": 1,
          "id": "20260101_000000_000-manual-aaaaaa",
          "createdAt": "2026-01-01T00:00:00.000+08:00",
          "trigger": "manual",
          "comfyui": { "channel": "stable", "ref": "v0.3.0" },
          "python": { "version": "3.12" },
          "nodes": [],
          "packages": { "numpy": "2.0.0" }
        }"#;
        let snap: Snapshot = serde_json::from_str(json).expect("老快照必须还能读");
        assert!(snap.wheels.is_empty());
        assert_eq!(
            snap.packages.get("numpy").map(String::as_str),
            Some("2.0.0")
        );
    }

    #[test]
    fn restore_spec_prefers_recorded_source() {
        let mut target = snap(T_MANUAL);
        target.wheels.insert(
            "flash-attn".into(),
            WheelRef {
                url: Some("https://example.com/flash.whl".into()),
                ..Default::default()
            },
        );
        assert!(matches!(
            package_spec("flash-attn", "2.8.4+cu130", &target, None),
            PackageTarget::Spec(spec) if spec == "https://example.com/flash.whl"
        ));
        assert!(matches!(
            package_spec("numpy", "2.0.0", &target, None),
            PackageTarget::Spec(spec) if spec == "numpy==2.0.0"
        ));

        // 记了本地 wheel、库里也有这颗文件：直接用库里的路径装。
        let lib = std::env::temp_dir().join(format!("comfyfleet-snap-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        std::fs::create_dir_all(&lib).unwrap();
        let wheel_file = lib.join("demo_pkg-1.0.0-cp313-cp313-win_amd64.whl");
        std::fs::write(&wheel_file, b"wheel").unwrap();
        target.wheels.insert(
            "demo-pkg".into(),
            WheelRef {
                filename: Some("demo_pkg-1.0.0-cp313-cp313-win_amd64.whl".into()),
                ..Default::default()
            },
        );
        assert!(matches!(
            package_spec("demo-pkg", "1.0.0", &target, Some(&lib)),
            PackageTarget::Spec(spec) if spec == wheel_file.to_string_lossy()
        ));

        // 库里没有：报“缺 wheel”，提示用户补，而不是硬按版本装。
        target.wheels.insert(
            "zzz-absent-pkg".into(),
            WheelRef {
                filename: Some("zzz_absent_pkg-9.9.9-py3-none-any.whl".into()),
                ..Default::default()
            },
        );
        match package_spec("zzz-absent-pkg", "9.9.9", &target, Some(&lib)) {
            PackageTarget::Missing(reason) => {
                assert!(reason.contains("zzz_absent_pkg-9.9.9-py3-none-any.whl"));
                assert!(reason.contains(&lib.to_string_lossy().to_string()));
            }
            PackageTarget::Spec(spec) => panic!("缺 wheel 不能降级成按版本装：{spec}"),
        }
        // 没配 wheel 库时提示去设置里指定目录
        match package_spec("zzz-absent-pkg", "9.9.9", &target, None) {
            PackageTarget::Missing(reason) => assert!(reason.contains("本地 Wheel 库")),
            PackageTarget::Spec(spec) => panic!("没配库也不能按版本装：{spec}"),
        }
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn restore_reports_wheels_missing_from_the_library() {
        let lib =
            std::env::temp_dir().join(format!("comfyfleet-snap-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lib);
        std::fs::create_dir_all(&lib).unwrap();

        let mut target = snap(T_MANUAL);
        target.packages.insert("demo-pkg".into(), "2.0".into());
        target.packages.insert("absent-pkg".into(), "3.1".into());
        target.packages.insert("remote-pkg".into(), "1.5".into());
        target.wheels.insert(
            "demo-pkg".into(),
            WheelRef {
                filename: Some("demo_pkg-2.0.0-py3-none-any.whl".into()),
                ..Default::default()
            },
        );
        target.wheels.insert(
            "absent-pkg".into(),
            WheelRef {
                filename: Some("absent_pkg-3.1.0-py3-none-any.whl".into()),
                ..Default::default()
            },
        );
        target.wheels.insert(
            "remote-pkg".into(),
            WheelRef {
                url: Some("https://example.com/remote-1.5.whl".into()),
                ..Default::default()
            },
        );
        std::fs::write(lib.join("demo_pkg-2.0.0-py3-none-any.whl"), b"wheel").unwrap();

        let mut settings = GlobalSettings::default();
        settings.wheels = Some(lib.to_string_lossy().to_string());
        // demo-pkg 已经是目标版本，恢复不会重装它；remote-pkg 按 URL 重装。
        let current = BTreeMap::from([("demo-pkg".to_string(), "2.0".to_string())]);

        let missing = missing_restore_wheels(&target, &current, &settings);
        let names: Vec<&str> = missing.iter().map(|wheel| wheel.name.as_str()).collect();
        assert_eq!(names, ["absent-pkg"]);
        assert!(missing[0]
            .reason
            .contains("absent_pkg-3.1.0-py3-none-any.whl"));
        assert!(missing[0]
            .reason
            .contains(&lib.to_string_lossy().to_string()));

        // 文件补齐后不再报缺。
        std::fs::write(lib.join("absent_pkg-3.1.0-py3-none-any.whl"), b"wheel").unwrap();
        assert!(missing_restore_wheels(&target, &current, &settings).is_empty());

        // 没配库：本地 wheel 都算缺，提示去设置里选目录。
        settings.wheels = None;
        let missing = missing_restore_wheels(&target, &current, &settings);
        assert!(missing
            .iter()
            .all(|wheel| wheel.reason.contains("本地 Wheel 库")));
        assert_eq!(missing.len(), 1);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn wheels_are_serialized_only_when_present() {
        let mut target = snap(T_MANUAL);
        target.wheels.insert(
            "demo".into(),
            WheelRef {
                filename: Some("demo-1.0.0-py3-none-any.whl".into()),
                url: None,
            },
        );
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"wheels\""));
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.wheels.get("demo").and_then(|w| w.filename.as_deref()),
            Some("demo-1.0.0-py3-none-any.whl")
        );
        let empty = serde_json::to_string(&snap(T_MANUAL)).unwrap();
        assert!(!empty.contains("\"wheels\""), "没有额外来源时不要写空字段");
    }

    #[test]
    fn protected_packages_are_skipped_by_plan() {
        assert!(is_protected_package("torch"));
        assert!(is_protected_package("TorchVision"));
        assert!(is_protected_package("torchcodec"));
        assert!(is_protected_package("nvidia-cublas-cu12"));
        assert!(is_protected_package("triton-windows"));
        // 不是 torch 全家，可以正常恢复
        assert!(!is_protected_package("torchmetrics"));
        assert!(!is_protected_package("torchsde"));
        assert!(!is_protected_package("intel-extension-for-pytorch"));
    }

    #[test]
    fn package_plan_installs_changes_and_removes() {
        let current = BTreeMap::from([
            ("aiohttp".to_string(), "3.9.5".to_string()),
            ("old".to_string(), "1.0".to_string()),
            ("torch".to_string(), "2.5.1".to_string()),
        ]);
        let target = BTreeMap::from([
            ("aiohttp".to_string(), "3.9.5".to_string()),
            ("new".to_string(), "2.0".to_string()),
            ("torch".to_string(), "2.6.0".to_string()),
        ]);
        let plan = plan_packages(&current, &target);
        assert_eq!(plan.install, vec![("new".to_string(), "2.0".to_string())]);
        assert_eq!(plan.remove, vec![("old".to_string(), "1.0".to_string())]);
        assert_eq!(plan.protected.len(), 1);
        assert_eq!(plan.protected[0].name, "torch");
    }

    #[test]
    fn package_plan_ignores_unknown_versions_from_legacy_snapshots() {
        let current = BTreeMap::from([("aiohttp".to_string(), "3.9.5".to_string())]);
        // 旧快照里的空版本是“未知”，不能拼成 aiohttp==，也不应触发安装/删除。
        let target = BTreeMap::from([
            ("aiohttp".to_string(), String::new()),
            ("not-installed".to_string(), String::new()),
        ]);
        let plan = plan_packages(&current, &target);
        assert!(plan.install.is_empty());
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn package_plan_normalizes_percent_encoded_local_version() {
        let current = BTreeMap::from([("llama-cpp-python".to_string(), "0.4.5+cu130".to_string())]);
        // 旧快照把 wheel URL 里的 `+` 存成 `%2B`；不能因此重复报缺包。
        let target =
            BTreeMap::from([("llama-cpp-python".to_string(), "0.4.5%2Bcu130".to_string())]);
        let plan = plan_packages(&current, &target);
        assert!(plan.install.is_empty(), "同一版本不应触发安装");
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn node_plan_covers_install_switch_toggle_and_remove() {
        let current = vec![
            SnapshotNode {
                name: "a".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: true,
                ..Default::default()
            },
            SnapshotNode {
                name: "b".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: true,
                ..Default::default()
            },
            SnapshotNode {
                name: "off".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: false,
                ..Default::default()
            },
            SnapshotNode {
                name: "extra".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: true,
                ..Default::default()
            },
        ];
        let target = vec![
            SnapshotNode {
                name: "a".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: true,
                ..Default::default()
            },
            SnapshotNode {
                name: "b".into(),
                url: Some("u".into()),
                commit: Some("2".into()),
                enabled: true,
                ..Default::default()
            },
            SnapshotNode {
                name: "off".into(),
                url: Some("u".into()),
                commit: Some("1".into()),
                enabled: true,
                ..Default::default()
            },
            SnapshotNode {
                name: "fresh".into(),
                url: Some("u".into()),
                commit: Some("9".into()),
                enabled: true,
                ..Default::default()
            },
        ];
        let plan = plan_nodes(&current, &target);
        assert_eq!(plan.install.len(), 1);
        assert_eq!(plan.install[0].name, "fresh");
        assert_eq!(plan.switch.len(), 1);
        assert_eq!(plan.switch[0].1.name, "b");
        assert_eq!(plan.enable.len(), 1);
        assert_eq!(plan.enable[0].name, "off");
        assert_eq!(plan.remove.len(), 1);
        assert_eq!(plan.remove[0].name, "extra");
        assert!(plan.disable.is_empty());
    }

    #[test]
    fn diff_reports_each_kind() {
        let mut a = snap("manual");
        let mut b = snap("manual");
        b.comfyui.commit = Some("def5678".into());
        b.packages.insert("new".into(), "2.0".into());
        b.packages.remove("aiohttp");
        b.nodes[0].enabled = false;
        b.nodes.push(SnapshotNode {
            name: "added".into(),
            url: Some("u".into()),
            commit: Some("1".into()),
            enabled: true,
            ..Default::default()
        });
        a.packages.insert("gone".into(), "0.1".into());
        // 旧快照里的空版本代表“未知”，不能算成版本变化。
        a.packages.insert("blank".into(), "".into());
        b.packages.insert("blank".into(), "1.2.3".into());

        let d = diff(&a, &b);
        assert!(d.comfyui_changed);
        assert_eq!(d.nodes_added.len(), 1);
        assert_eq!(d.nodes_added[0].name, "added");
        assert_eq!(d.nodes_changed.len(), 1);
        assert!(!d.nodes_changed[0].to_enabled);
        assert!(d.packages_added.iter().any(|p| p.name == "new"));
        assert!(d.packages_removed.iter().any(|p| p.name == "aiohttp"));
        assert!(d.packages_removed.iter().any(|p| p.name == "gone"));
        assert!(!d.packages_changed.iter().any(|p| p.name == "blank"));

        let s = d.summary();
        assert_eq!(s.nodes_added, 1);
        assert_eq!(s.packages_removed, 2);
    }

    #[test]
    fn states_match_ignores_meta_but_not_state() {
        let a = snap("boot");
        let mut b = snap("boot");
        b.id = "other".into();
        b.created_at = "2026-02-02T00:00:00.000+08:00".into();
        b.trigger = T_MANUAL.into();
        assert!(states_match(&a, &b));

        b.packages.insert("x".into(), "1".into());
        assert!(!states_match(&a, &b));
    }

    #[test]
    fn store_roundtrip_and_prune() {
        let base = std::env::temp_dir().join("comfyfleet-snapshot-store");
        let _ = std::fs::remove_dir_all(&base);

        let mut s = snap("boot");
        s.id = "20260101_000000_000-boot-aaaaaa".into();
        save_in(&base, &s).unwrap();
        let mut labelled = snap("boot");
        labelled.id = "20260101_000001_000-boot-bbbbbb".into();
        labelled.created_at = "2026-01-01T00:00:01.000+08:00".into();
        labelled.label = Some("手动".into());
        save_in(&base, &labelled).unwrap();

        let list = list_in(&base);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, labelled.id, "新的排前 ");
        assert_eq!(load_in(&base, &s.id).unwrap().id, s.id);
        //  label 的不算自动快。
        assert!(!list[0].auto_prunable());
        assert!(list[1].auto_prunable());

        delete_in(&base, &s.id).unwrap();
        assert_eq!(list_in(&base).len(), 1);

        // 目录穿越要被。
        assert!(load_in(&base, "../evil").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prune_keeps_only_auto_below_limit() {
        let base = std::env::temp_dir().join("comfyfleet-snapshot-prune");
        let _ = std::fs::remove_dir_all(&base);
        //  MAX_AUTO + 3 条自动快照，再放一条手。
        for i in 0..(MAX_AUTO + 3) {
            let mut s = snap("boot");
            s.id = format!("20260101_{i:06}-boot-{:06x}", i);
            s.created_at = format!("2026-01-01T00:{:02}:{:02}.000+08:00", i / 60, i % 60);
            save_in(&base, &s).unwrap();
        }
        let mut manual = snap("manual");
        manual.id = "20260101_999999-manual-ffffff".into();
        manual.created_at = "2026-01-01T23:59:59.000+08:00".into();
        save_in(&base, &manual).unwrap();

        let removed = prune_in(&base, MAX_AUTO);
        assert_eq!(removed, 3);
        let left = list_in(&base);
        assert_eq!(left.len(), MAX_AUTO + 1);
        assert!(left.iter().any(|s| s.id == manual.id));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn safe_ids_and_names() {
        assert!(safe_id("20260101_000000_000-manual-abcdef"));
        assert!(!safe_id("../x"));
        assert!(!safe_id("a/b"));
        assert!(is_safe_node_name("ComfyUI-Manager"));
        assert!(is_safe_node_name("ComfyUI-Impact-Pack.v2"));
        assert!(is_safe_node_name("comfyui_custom_nodes_alekpet@nightly"));
        assert!(!is_safe_node_name(".hidden"));
        assert!(!is_safe_node_name("a/b"));
        assert!(!is_safe_node_name("a b"));
    }

    #[test]
    fn snapshots_follow_instance_and_migrate_legacy() {
        let tmp = temp_case("follow-instance");
        let inst_root = tmp.join("inst");
        let inst = test_instance(&inst_root, "follow-fixture");

        let old = legacy_root(&inst.id);
        std::fs::create_dir_all(&old).unwrap();
        let mut s = snap(T_MANUAL);
        s.id = "20260101_000000_000-manual-aaaaaa".into();
        save_in(&old, &s).unwrap();
        std::fs::create_dir_all(payload_dir(&old, &s.id)).unwrap();
        std::fs::write(payload_dir(&old, &s.id).join("x.zip"), b"zip").unwrap();

        let base = dir(&inst);
        assert_eq!(base, inst_root.join(".comfyfleet").join("snapshots"));
        assert!(base.join(format!("{}.json", s.id)).is_file());
        assert!(payload_dir(&base, &s.id).join("x.zip").is_file());
        assert!(!old.exists(), "旧版启动器快照目录迁移后应清空");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ------------------------------------------------ 真实 git 的恢复演练
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("起不了 git");
        assert!(
            out.status.success(),
            "git {:?} 失败{}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn make_repo(dir: &Path, file: &str, body: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "snapshot@test"]);
        git_in(dir, &["config", "user.name", "snapshot"]);
        git_in(dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join(file), body).unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "init"]);
        git_in(dir, &["rev-parse", "HEAD"])
    }

    fn node(name: &str, commit: &str, enabled: bool) -> SnapshotNode {
        SnapshotNode {
            name: name.to_string(),
            url: Some(format!("https://example.com/{name}.git")),
            commit: Some(commit.to_string()),
            enabled,
            ..Default::default()
        }
    }

    fn test_instance(root: &Path, id: &str) -> InstanceConfig {
        InstanceConfig {
            id: id.into(),
            name: id.into(),
            root_dir: root.to_string_lossy().to_string(),
            engine: "auto".into(),
            engine_detail: None,
            python_version: "3.12".into(),
            comfy: crate::config::ComfyRef {
                channel: "stable".into(),
                reference: "v0.1".into(),
                commit: "0000000".into(),
                version: Some("0.1".into()),
            },
            server: crate::config::ServerOpts {
                listen: "127.0.0.1".into(),
                port: 8188,
                cors_origin: None,
                extra_args: vec![],
            },
            storage: Default::default(),
            env: Default::default(),
            overrides: Default::default(),
            node_deps_auto: None,
            auto_restart: false,
            python: Default::default(),
            kind: "local".into(),
            remote: None,
            imported: false,
            state: crate::config::InstanceState::Ready,
            created_at: "2026-01-01T00:00:00+08:00".into(),
            last_started_at: None,
        }
    }

    fn temp_case(name: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "comfyfleet-snapshot-{name}-{}-{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("data")).unwrap();
        std::env::set_var(config::DATA_DIR_ENV, tmp.join("data"));
        tmp
    }

    /// 手工拷进来的节点没有 git 地址：快照必须能把整目录存下来再解回去。
    #[test]
    fn manual_node_archive_roundtrip() {
        let tmp = temp_case("manual-node");
        let inst_root = tmp.join("inst");
        let node_dir = inst_root.join("ComfyUI/custom_nodes/ManualNode");
        std::fs::create_dir_all(node_dir.join("sub")).unwrap();
        std::fs::write(node_dir.join("node.py"), "print('manual')\n").unwrap();
        std::fs::write(node_dir.join("sub/data.txt"), "payload\n").unwrap();

        let inst = test_instance(&inst_root, "manual-fixture");
        let settings = GlobalSettings::default();
        let snap = capture_and_save(&inst, &settings, T_MANUAL, None).unwrap();
        let saved = snap.nodes.iter().find(|n| n.name == "ManualNode").unwrap();
        assert_eq!(saved.content.as_deref(), Some("full"));
        assert!(!saved.dirty);
        assert!(saved.content_hash.is_some());
        assert!(node_payload_file(&inst, &snap, "ManualNode", "zip").is_file());

        std::fs::remove_dir_all(&node_dir).unwrap();
        replace_node_from_zip(&inst, &snap, "ManualNode", &node_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(node_dir.join("sub/data.txt")).unwrap(),
            "payload\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 恢复计划要真的把手工目录 git 未提交改动都还原，而不只是「记下来」。
    #[test]
    fn restore_content_plan_roundtrip() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("没有 git，跳 ");
            return;
        }
        let tmp = temp_case("restore-content");
        let inst_root = tmp.join("inst");
        let nodes = inst_root.join("ComfyUI/custom_nodes");

        let manual = nodes.join("ManualNode");
        std::fs::create_dir_all(&manual).unwrap();
        std::fs::write(manual.join("node.py"), "manual-v1\n").unwrap();

        let dirty = nodes.join("DirtyNode");
        let head = make_repo(&dirty, "node.py", "dirty-base\n");
        git_in(
            &dirty,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/DirtyNode.git",
            ],
        );
        std::fs::write(dirty.join("node.py"), "dirty-modified\n").unwrap();
        std::fs::write(dirty.join("extra.txt"), "untracked\n").unwrap();

        let inst = test_instance(&inst_root, "restore-content-fixture");
        let settings = GlobalSettings::default();
        let target = capture_and_save(&inst, &settings, T_MANUAL, None).unwrap();

        // 现场改成和快照不一样：手工目录换内容，git 节点清回干净。
        std::fs::write(manual.join("node.py"), "manual-v2\n").unwrap();
        clean_to_commit(Path::new("git"), &dirty, &head).unwrap();

        let ctx = RestoreCtx {
            inst: &inst,
            settings: &settings,
            git: Path::new("git"),
            uv: Path::new("uv"),
        };
        let mut logs: Vec<String> = Vec::new();
        let outcome = {
            let mut sink = |ev: RestoreEvent| logs.push(ev.message);
            restore_nodes(&ctx, &target, RestoreOptions::default(), &mut sink)
        };
        assert!(outcome.failed.is_empty(), "恢复失败{:?}", outcome.failed);
        assert_eq!(
            std::fs::read_to_string(manual.join("node.py")).unwrap(),
            "manual-v1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dirty.join("node.py")).unwrap(),
            "dirty-modified\n"
        );
        assert_eq!(
            std::fs::read_to_string(dirty.join("extra.txt")).unwrap(),
            "untracked\n"
        );
        assert!(outcome.content_restored.contains(&"ManualNode".to_string()));
        assert!(outcome.content_restored.contains(&"DirtyNode".to_string()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// git 节点改了但没提交：存 diff + 未跟踪文件，恢复后仍停在原提交。
    #[test]
    fn dirty_git_node_patch_roundtrip() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("没有 git，跳 ");
            return;
        }
        let tmp = temp_case("dirty-node");
        let inst_root = tmp.join("inst");
        let node_dir = inst_root.join("ComfyUI/custom_nodes/DirtyNode");
        let head = make_repo(&node_dir, "node.py", "print('base')\n");
        git_in(
            &node_dir,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/DirtyNode.git",
            ],
        );
        std::fs::write(node_dir.join("node.py"), "print('modified')\n").unwrap();
        std::fs::write(node_dir.join("extra.txt"), "untracked\n").unwrap();

        let inst = test_instance(&inst_root, "dirty-fixture");
        let settings = GlobalSettings::default();
        let snap = capture_and_save(&inst, &settings, T_MANUAL, None).unwrap();
        let saved = snap.nodes.iter().find(|n| n.name == "DirtyNode").unwrap();
        assert!(saved.dirty, "git status 应看到未提交改动");
        assert_eq!(saved.content.as_deref(), Some("patch"));
        assert!(
            saved.commit.as_deref().is_some_and(|c| head.starts_with(c)),
            "快照应记录当前提交：{:?} vs {head}",
            saved.commit
        );
        let patch = node_payload_file(&inst, &snap, "DirtyNode", "patch");
        let untracked = node_payload_file(&inst, &snap, "DirtyNode", "untracked.zip");
        assert!(std::fs::read_to_string(&patch)
            .unwrap()
            .contains("modified"));
        assert!(untracked.is_file());

        clean_to_commit(Path::new("git"), &node_dir, &head).unwrap();
        assert_eq!(
            std::fs::read_to_string(node_dir.join("node.py")).unwrap(),
            "print('base')\n"
        );
        apply_node_patch(Path::new("git"), &inst, &snap, "DirtyNode", &node_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(node_dir.join("node.py")).unwrap(),
            "print('modified')\n"
        );
        assert_eq!(
            std::fs::read_to_string(node_dir.join("extra.txt")).unwrap(),
            "untracked\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 用一个临时仓库把整个恢复流程（版 / 节点 / 包）真跑一遍。
    /// 手动执行：cargo test --lib restore_against_fixture -- --ignored --nocapture
    #[test]
    #[ignore = "会跑真实 git，手动执 "]
    fn restore_against_fixture() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("没有 git，跳 ");
            return;
        }
        let tmp = std::env::temp_dir().join("comfyfleet-snapshot-restore");
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::env::set_var(config::DATA_DIR_ENV, &data);

        let inst_root = tmp.join("inst");
        let comfy = inst_root.join("ComfyUI");
        let nodes = comfy.join("custom_nodes");

        // ComfyUI：c1 是当前，c2 是目标。真仓库 /custom_nodes/  ignore         // 否则装过节点的实例永远「有未提交改动」，恢复会被自己挡住。
        std::fs::create_dir_all(&comfy).unwrap();
        git_in(&comfy, &["init", "-q", "-b", "main"]);
        git_in(&comfy, &["config", "user.email", "snapshot@test"]);
        git_in(&comfy, &["config", "user.name", "snapshot"]);
        std::fs::write(comfy.join("main.py"), "print('v1')").unwrap();
        std::fs::write(comfy.join(".gitignore"), "/custom_nodes/\n").unwrap();
        git_in(&comfy, &["add", "-A"]);
        git_in(&comfy, &["commit", "-q", "-m", "v1"]);
        let c1 = git_in(&comfy, &["rev-parse", "HEAD"]);
        std::fs::write(comfy.join("main.py"), "print('v2')").unwrap();
        git_in(&comfy, &["add", "-A"]);
        git_in(&comfy, &["commit", "-q", "-m", "v2"]);
        let c2 = git_in(&comfy, &["rev-parse", "HEAD"]);
        git_in(&comfy, &["checkout", "-q", &c1]);

        // 节点：keep 保留、off 被禁用、extra 多余、gone 要装回来
        let keep = make_repo(&nodes.join("keep"), "node.py", "print('keep')");
        let off = make_repo(&nodes.join("off.disabled"), "node.py", "print('off')");
        make_repo(&nodes.join("extra"), "node.py", "print('extra')");
        let gone_src = tmp.join("gone_src");
        let gone = make_repo(&gone_src, "node.py", "print('gone')");

        let inst = InstanceConfig {
            id: "fixture".into(),
            name: "fixture".into(),
            root_dir: inst_root.to_string_lossy().to_string(),
            engine: "auto".into(),
            engine_detail: None,
            python_version: "3.12".into(),
            comfy: crate::config::ComfyRef {
                channel: "stable".into(),
                reference: "v0.1".into(),
                commit: c1.clone(),
                version: Some("0.1".into()),
            },
            server: crate::config::ServerOpts {
                listen: "127.0.0.1".into(),
                port: 8188,
                cors_origin: None,
                extra_args: vec![],
            },
            storage: Default::default(),
            env: Default::default(),
            overrides: Default::default(),
            node_deps_auto: None,
            auto_restart: false,
            python: Default::default(),
            kind: "local".into(),
            remote: None,
            imported: false,
            state: crate::config::InstanceState::Ready,
            created_at: "2026-01-01T00:00:00+08:00".into(),
            last_started_at: None,
        };

        let target = Snapshot {
            version: SNAPSHOT_VERSION,
            id: "fixture-target".into(),
            created_at: "2026-01-01T00:00:00.000+08:00".into(),
            trigger: T_MANUAL.into(),
            label: Some("fixture".into()),
            comfyui: SnapshotCore {
                channel: "stable".into(),
                reference: "v0.2".into(),
                commit: Some(c2.clone()),
                version: Some("0.2".into()),
            },
            python: SnapshotPython {
                version: "3.12".into(),
                torch: None,
            },
            server: Some(crate::config::ServerOpts {
                listen: "127.0.0.1".into(),
                port: 9011,
                cors_origin: Some("http://localhost:3000".into()),
                extra_args: vec!["--lowvram".into()],
            }),
            nodes: vec![
                node("keep", &keep, true),
                node("off", &off, true),
                SnapshotNode {
                    name: "gone".into(),
                    url: Some(gone_src.to_string_lossy().to_string()),
                    commit: Some(gone.clone()),
                    enabled: true,
                    ..Default::default()
                },
            ],
            packages: BTreeMap::new(),
            wheels: BTreeMap::new(),
            note: None,
        };
        save_in(&dir(&inst), &target).unwrap();

        let settings = GlobalSettings::default();
        let ctx = RestoreCtx {
            inst: &inst,
            settings: &settings,
            git: Path::new("git"),
            uv: Path::new("uv"),
        };
        let mut log: Vec<String> = Vec::new();
        let report = {
            let mut sink =
                |ev: RestoreEvent| log.push(format!("{} {} {}", ev.step, ev.phase, ev.message));
            restore(&ctx, &target, RestoreOptions::default(), &mut sink).unwrap()
        };

        assert_eq!(
            read_head(&comfy).as_deref(),
            Some(c2.as_str()),
            "版本应切到目标提 "
        );
        assert!(report.core.changed && report.core.error.is_none());
        assert!(!nodes.join("extra").exists(), "多余节点应被删除");
        assert!(nodes.join("off").exists(), "被禁用的节点应启 ");
        assert!(!nodes.join("off.disabled").exists());
        assert!(nodes.join("gone").exists(), "缺的节点应装回来");
        assert_eq!(
            git_in(&nodes.join("gone"), &["rev-parse", "HEAD"]),
            gone,
            "装回来的节点要停在快照记的提 "
        );
        assert_eq!(report.nodes.installed, vec!["gone".to_string()]);
        assert_eq!(report.nodes.removed, vec!["extra".to_string()]);
        assert_eq!(report.nodes.enabled, vec!["off".to_string()]);
        assert!(report.pre_snapshot.is_some());
        assert!(report.post_snapshot.is_none(), "恢复完成后不再自动新建快 ");
        assert!(report.launch.changed, "启动参数 8188 变成 9011，应报告改动");
        assert_eq!(report.launch.to.as_ref().map(|s| s.port), Some(9011));
        assert_eq!(
            report
                .launch
                .to
                .as_ref()
                .and_then(|s| s.cors_origin.clone())
                .as_deref(),
            Some("http://localhost:3000")
        );
        // 没有 venv，包这步应明确报错而不是静默跳。
        assert!(report.packages.failed.iter().any(|f| f.name == "python"));

        let timeline = list_in(&dir(&inst));
        assert_eq!(timeline.len(), 2, "目标 + 恢复前；恢复后不再自动新 ");
        assert_eq!(timeline[0].trigger, T_PRE_RESTORE);
        assert_eq!(timeline[1].trigger, T_MANUAL);
        eprintln!("时间线：\n{}", log.join("\n"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn diff_and_states_match_include_launch_params() {
        let a = snap(T_MANUAL);
        let mut b = a.clone();
        b.server = Some(crate::config::ServerOpts {
            listen: "0.0.0.0".into(),
            port: 9000,
            cors_origin: Some("http://localhost:3000".into()),
            extra_args: vec!["--lowvram".into()],
        });
        let d = diff(&a, &b);
        assert!(d.launch_changed);
        assert_eq!(d.launch_to.as_ref().map(|s| s.port), Some(9000));
        assert!(d.summary().launch_changed);
        assert!(!states_match(&a, &b), "只改启动参数也算不同状态");
    }

    #[test]
    fn pip_command_only_adds_mirror_for_install() {
        let python = Path::new("C:\\venv\\python.exe");
        let install = pip_command(
            &["install".into(), "requests==2.0".into()],
            python,
            Some("https://mirror.example/simple"),
        );
        assert!(install.iter().any(|arg| arg == "--index-url"));
        assert!(install
            .iter()
            .any(|arg| arg == "https://mirror.example/simple"));

        let uninstall = pip_command(
            &["uninstall".into(), "requests".into()],
            python,
            Some("https://mirror.example/simple"),
        );
        assert!(
            !uninstall.iter().any(|arg| arg == "--index-url"),
            "uv pip uninstall 不接 --index-url，卸载时不能带镜像参 "
        );
    }
}
