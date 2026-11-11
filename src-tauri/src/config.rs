use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::error::AppError;

// ---------------------------------------------------------------- 路径约定

/// 数据目录 = **安装目录**下的 `data`（exe 旁边），便携式布局。
/// 卸载/搬走安装目录时数据跟着走，不再散落在 `%APPDATA%`。
const DATA_DIR_NAME: &str = "data";

/// 逃生口：用环境变量把数据目录钉到别处（自动化测试、或安装目录真不可写）。
pub const DATA_DIR_ENV: &str = "COMFYFLEET_DATA_DIR";

/// 设置结构版本。
/// v2 把自动快照默认上限从 200 改成 20；
/// v3 增加 UV 缓存自动清理阈值（默认 10 GB）；
/// v4 清掉旧版默认的 PyPI / PyTorch 镜像，默认走官方源；
/// v5 拆成暗色 / 亮色两套配色，并给卡片单独的颜色；
/// v6 本地 wheel 库默认落到数据目录下的 wheel/
const SETTINGS_VERSION: u32 = 6;

/// 旧版默认 PyPI 镜像。升级到 v4 时如果配置里还是这个值，视为没改过，直接清空
const LEGACY_DEFAULT_PYPI: &str = "https://mirrors.aliyun.com/pypi/simple/";
/// 旧版可能写入的 PyTorch 镜像；同样按“默认值”清掉
const LEGACY_DEFAULT_TORCH: &str = "https://mirrors.aliyun.com/pytorch-wheels/";

/// 安装目录 = 可执行文件所在目录。NSIS 的 currentUser 安装模式把它放在当前用户
/// `%LOCALAPPDATA%` 下，普通用户可写，所以数据可以就放在旁边。
pub fn install_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Legacy data directory from 0.1; used only for upgrade migration.
/// 现在只用于升级迁移，以及"安装目录不可写"时的回退。
pub fn legacy_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(["YH", "ComfyUI"].join("-"))
}

/// 路径解析只做一次（含首次迁移），之后所有命令拿到同一个结果。
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn app_root() -> PathBuf {
    DATA_ROOT.get_or_init(resolve_root).clone()
}

fn resolve_root() -> PathBuf {
    if let Some(dir) = env_override() {
        if ensure_writable(&dir) {
            log::info!("数据目录由 {DATA_DIR_ENV} 指定：{}", dir.display());
            return dir;
        }
        log::warn!("{DATA_DIR_ENV} 指定的目录不可写，忽略：{}", dir.display());
    }

    let target = install_dir().join(DATA_DIR_NAME);
    if ensure_writable(&target) {
        if auto_migrate_enabled() {
            migrate_legacy(&legacy_root(), &target);
        } else if legacy_root().is_dir() && !target.join("config.json").exists() {
            log::info!(
                "构建产物目录（{}）不自动迁移旧数据；旧数据仍在 {}，正式安装后首次启动才迁移。",
                install_dir().display(),
                legacy_root().display()
            );
        }
        return target;
    }

    let legacy = legacy_root();
    log::warn!(
        "安装目录不可写（{}），数据目录回退到 {}；想固定在安装目录请用当前用户安装。",
        install_dir().display(),
        legacy.display()
    );
    let _ = fs::create_dir_all(&legacy);
    legacy
}

/// 自动迁移只在"像安装目录"的位置做：
/// - 开发构建（`tauri dev` / `cargo run`）不搬真实数据；
/// - `cargo build` 的产物在 `target\` 下，那也不是安装目录，搬过去会把用户数据拖进构建目录。
fn auto_migrate_enabled() -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    !install_dir()
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("target"))
}

fn env_override() -> Option<PathBuf> {
    let raw = std::env::var(DATA_DIR_ENV).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let dir = PathBuf::from(raw);
    if dir.is_absolute() {
        Some(dir)
    } else {
        log::warn!("{DATA_DIR_ENV} 需要绝对路径，已忽略：{raw}");
        None
    }
}

/// 建目录 + 真写一个探针文件：避免 `create_dir_all` 看不出"目录存在但不可写"。
fn ensure_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".comfyfleet-write-probe-{}", std::process::id()));
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

fn dir_is_empty(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}

/// Upgrade migration: move legacy data into the install directory.
/// 同盘用 rename（瞬时）；跨盘退化为"复制到临时目录再改名"，失败就原样保留。
/// 迁移全程不删旧目录，除非复制完整成功 —— 宁可多留一份，不能让实例凭空消失。
fn migrate_legacy(legacy: &Path, target: &Path) {
    if !legacy.is_dir() || legacy == target {
        return;
    }
    if target.join("config.json").exists() {
        return; // target already initialized; leave legacy backup untouched
    }
    if target.exists() {
        if !dir_is_empty(target) {
            log::warn!(
                "{} 非空却没有 config.json，跳过自动迁移。",
                target.display()
            );
            return;
        }
        let _ = fs::remove_dir(target);
    }

    match fs::rename(legacy, target) {
        Ok(()) => {
            log::info!(
                "数据目录已迁移：{} → {}",
                legacy.display(),
                target.display()
            );
            return;
        }
        Err(e) => log::warn!("无法直接移动数据目录（{}），改为复制", e),
    }

    let started = std::time::Instant::now();
    let staging = target.with_file_name(format!("{DATA_DIR_NAME}-migrating"));
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = copy_dir_all(legacy, &staging) {
        log::warn!(
            "复制旧数据目录失败（{}），继续使用旧目录 {}",
            e,
            legacy.display()
        );
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    if let Err(e) = fs::rename(&staging, target) {
        log::warn!("迁移目录改名失败（{}），继续使用旧目录。", e);
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    log::info!(
        "数据目录已迁移（复制）：{} → {}，耗时 {:.1}s",
        legacy.display(),
        target.display(),
        started.elapsed().as_secs_f32()
    );
    if let Err(e) = fs::remove_dir_all(legacy) {
        log::warn!("旧数据目录删除失败，可手动清理 {}：{}", legacy.display(), e);
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub fn config_path() -> PathBuf {
    app_root().join("config.json")
}

pub fn instances_dir() -> PathBuf {
    app_root().join("instances")
}

pub fn shared_dir() -> PathBuf {
    app_root().join("shared")
}

pub fn default_models_root() -> PathBuf {
    shared_dir().join("models")
}

/// 默认本地 wheel 库：数据目录下的 `wheel/`。数据目录默认就是安装目录的
/// `data`，所以最终落点是 `data/wheel`；相对路径没有写死，数据目录被环境变量
/// 或回退逻辑换掉时，默认库也跟着走。
pub fn default_wheels_dir() -> PathBuf {
    app_root().join("wheel")
}

// ---------------------------------------------------------------- 设置模型

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GlobalSettings {
    pub version: u32,
    pub mirrors: Mirrors,
    /// 云端 wheel 仓库地址（目前支持 ModelScope 模型仓库）。
    #[serde(default)]
    pub wheel_repo: Option<String>,
    /// 本地 wheel 库目录：用户手动安装的本地轮子放这里，恢复快照 / 同步时按
    /// 文件名或「包名 + 版本」来取。默认是数据目录下的 wheel/；留空 = 没有库，
    /// 缺失的自定义包直接报缺。
    #[serde(default)]
    pub wheels: Option<String>,
    pub paths: Paths,
    pub storage: StorageSettings,
    pub install: InstallOpts,
    pub network: NetworkOpts,
    pub ui: UiOpts,
    pub activity: ActivityOpts,
    pub sync: SyncSettings,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            mirrors: Mirrors::default(),
            wheel_repo: None,
            wheels: Some(default_wheels_dir().to_string_lossy().to_string()),
            paths: Paths::default(),
            storage: StorageSettings::default(),
            install: InstallOpts::default(),
            network: NetworkOpts::default(),
            ui: UiOpts::default(),
            activity: ActivityOpts::default(),
            sync: SyncSettings::default(),
        }
    }
}

/// 局域网同步的默认策略。每次预览里仍可临时改；这里只决定打开同步页时的默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncSettings {
    /// 预览里默认勾选“删除目标多余节点”。
    pub remove_extra_nodes: bool,
    /// 预览里默认勾选“删除目标多余模型”。
    pub remove_extra_models: bool,
    /// 成功换掉旧 venv 后保留几天；0 = 不保留。范围由 normalize 限制。
    pub keep_old_venv_days: u32,
    /// 自动快照最多保留多少条；手动快照不自动删除
    pub snapshot_max: u32,

    /// 最近几次同步产生的缓存（代次、目标收件箱、覆盖备份）保留多少组
    pub sync_cache_keep: u32,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            remove_extra_nodes: false,
            remove_extra_models: false,
            keep_old_venv_days: 7,
            snapshot_max: 20,
            sync_cache_keep: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Mirrors {
    pub pypi: Option<String>,
    /// PyTorch 镜像根地址，安装时自动补引擎目录；留空用官方源
    pub torch: Option<String>,
    pub hf: Option<String>,
    pub github_proxy: Option<String>,
}

impl Default for Mirrors {
    fn default() -> Self {
        Self {
            // PyPI / PyTorch 默认留空，走官方源；HuggingFace 仍默认用镜像
            pypi: None,
            torch: None,
            hf: Some("https://hf-mirror.com".to_string()),
            github_proxy: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Paths {
    pub git: Option<String>,
    /// 旧的「共享模型根目录」单值。已被 `storage.sharedModels` 取代。
    /// 只在读旧配置时用来迁移（normalize 里会同步成第一个共享目录，不再由界面编辑）。
    pub models_root: String,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            git: None,
            models_root: default_models_root().to_string_lossy().to_string(),
        }
    }
}

/// 一个模型目录。
///
/// `type_name` 为空 = 「整个 models 根目录」，下面的 `checkpoints/`、`loras/` 这类子目录自动展开。
/// 有 type_name = 这个目录只装某一类模型（比如专门放 `D:\sd\checkpoints`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelDir {
    pub path: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// 全局共享目录里「下载保存往哪走」的标记，写进 yaml 的 `is_default`
    pub is_default: bool,
}

/// 全局共享目录：模型可多个，输入/输出各只能一个（这是 ComfyUI 自己的模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageSettings {
    pub shared_models: Vec<ModelDir>,
    pub shared_input: Option<String>,
    pub shared_output: Option<String>,
    /// UV 下载/构建缓存的上限（GB）；超过后自动清空，0 = 关闭自动清理
    pub uv_cache_max_gb: u32,
}

pub const DEFAULT_UV_CACHE_GB: u32 = 10;
pub const MAX_UV_CACHE_GB: u32 = 1024;

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            shared_models: vec![ModelDir {
                path: default_models_root().to_string_lossy().to_string(),
                type_name: None,
                is_default: true,
            }],
            shared_input: None,
            shared_output: None,
            uv_cache_max_gb: DEFAULT_UV_CACHE_GB,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstallOpts {
    pub pypi_fallback: bool,
    pub allow_install_script: bool,
    pub auto_snapshot: bool,
}

impl Default for InstallOpts {
    fn default() -> Self {
        Self {
            pypi_fallback: true,
            // 保持旧行为：节点依赖默认允许 install.py；用户可在配置里关掉。
            // 这个开关以前从未被读取，false 的默认值会与界面“默认执行”矛盾
            allow_install_script: true,
            auto_snapshot: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetworkOpts {
    pub listen_default: String,
    pub port_range: [u16; 2],
    pub detect_timeout_ms: u64,
    /// HuggingFace 离线：只用本地缓存，不发起网络请求。
    /// `serde(default)` 保证旧 config.json（没有该字段）仍能解析。
    #[serde(default)]
    pub hf_offline: bool,
}

impl Default for NetworkOpts {
    fn default() -> Self {
        Self {
            listen_default: "127.0.0.1".to_string(),
            port_range: [8188, 8288],
            detect_timeout_ms: 1_800_000,
            hf_offline: false,
        }
    }
}

/// 操作日志：每个实例一个 JSONL，超过上限就从最旧的记录开始丢。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivityOpts {
    /// 单个实例的操作日志文件上限（字节）。
    pub max_bytes: u64,
}

impl Default for ActivityOpts {
    fn default() -> Self {
        Self {
            max_bytes: crate::activity::DEFAULT_CAP_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThemePalette {
    /// 主题强调色：主按钮、链接、焦点环都从这里派生
    pub accent: String,
    /// 背景渐变的三段颜色
    pub bg_top: String,
    pub bg_mid: String,
    pub bg_bottom: String,
    /// 卡片 / 面板底色；透明度另外统一控制
    pub card_color: String,
}

pub const DEFAULT_ACCENT: &str = "#2b63ff";
pub const DEFAULT_BG_TOP: &str = "#1a2036";
pub const DEFAULT_BG_MID: &str = "#111525";
pub const DEFAULT_BG_BOTTOM: &str = "#020203";
pub const DEFAULT_CARD_COLOR: &str = "#161b2a";
pub const DEFAULT_LIGHT_BG_TOP: &str = "#f7f9ff";
pub const DEFAULT_LIGHT_BG_MID: &str = "#eef2fa";
pub const DEFAULT_LIGHT_BG_BOTTOM: &str = "#e3e9f4";
pub const DEFAULT_LIGHT_CARD_COLOR: &str = "#ffffff";
pub const DEFAULT_CARD_TRANSPARENCY: u8 = 12;
pub const MAX_CARD_TRANSPARENCY: u8 = 80;

pub fn default_dark_palette() -> ThemePalette {
    ThemePalette {
        accent: DEFAULT_ACCENT.to_string(),
        bg_top: DEFAULT_BG_TOP.to_string(),
        bg_mid: DEFAULT_BG_MID.to_string(),
        bg_bottom: DEFAULT_BG_BOTTOM.to_string(),
        card_color: DEFAULT_CARD_COLOR.to_string(),
    }
}

pub fn default_light_palette() -> ThemePalette {
    ThemePalette {
        accent: DEFAULT_ACCENT.to_string(),
        bg_top: DEFAULT_LIGHT_BG_TOP.to_string(),
        bg_mid: DEFAULT_LIGHT_BG_MID.to_string(),
        bg_bottom: DEFAULT_LIGHT_BG_BOTTOM.to_string(),
        card_color: DEFAULT_LIGHT_CARD_COLOR.to_string(),
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        default_dark_palette()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiOpts {
    pub theme: String,
    pub locale: String,
    pub font_size: u32,
    /// 暗色 / 亮色两套配色分别保存，切换主题不会覆盖另一套
    pub dark: ThemePalette,
    pub light: ThemePalette,
    /// 卡片透明度（0–100%）：0 为不透明，数字越大越透。
    pub card_transparency: u8,

    // v4 及更早版本把配色平铺在 ui 下；只用于升级迁移，不再写回配置文件
    #[serde(rename = "accent", default, skip_serializing)]
    legacy_accent: Option<String>,
    #[serde(rename = "bgTop", default, skip_serializing)]
    legacy_bg_top: Option<String>,
    #[serde(rename = "bgMid", default, skip_serializing)]
    legacy_bg_mid: Option<String>,
    #[serde(rename = "bgBottom", default, skip_serializing)]
    legacy_bg_bottom: Option<String>,
}

impl Default for UiOpts {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            locale: "zh-CN".to_string(),
            font_size: 13,
            dark: default_dark_palette(),
            light: default_light_palette(),
            card_transparency: DEFAULT_CARD_TRANSPARENCY,
            legacy_accent: None,
            legacy_bg_top: None,
            legacy_bg_mid: None,
            legacy_bg_bottom: None,
        }
    }
}

impl GlobalSettings {
    /// 修正从磁盘读到的可疑值（空字符串、非法端口区间等）。
    pub fn normalize(&mut self) {
        fn clean(v: &mut Option<String>) {
            let trimmed = v.as_deref().unwrap_or("").trim().to_string();
            *v = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
        }
        clean(&mut self.mirrors.pypi);
        clean(&mut self.mirrors.torch);
        clean(&mut self.mirrors.hf);
        clean(&mut self.mirrors.github_proxy);
        clean(&mut self.wheel_repo);
        clean(&mut self.wheels);
        clean(&mut self.paths.git);

        // v6：老配置没有本地 wheel 库的，补上数据目录下的默认库。之后用户主动
        // 清空则保持为空，不再每次启动强行塞回去。
        if self.version < 6 && self.wheels.is_none() {
            self.wheels = Some(default_wheels_dir().to_string_lossy().to_string());
        }

        // 操作日志上限：0 / 乱填一律拉回安全区间，避免无限增长或一写就裁没
        self.activity.max_bytes = self.activity.max_bytes.clamp(
            crate::activity::MIN_CAP_BYTES,
            crate::activity::MAX_CAP_BYTES,
        );

        // Legacy default model paths are rewritten to the new default.
        let legacy_models = legacy_root().join("shared").join("models");
        if self.paths.models_root.trim().is_empty()
            || same_path(&self.paths.models_root, &legacy_models.to_string_lossy())
        {
            self.paths.models_root = default_models_root().to_string_lossy().to_string();
        }

        // 旧版本只有一个「共享模型根目录」；读到老配置时把它迁成共享目录列表的第一项。
        // 老配置里没有 storage 段，serde 会填默认值（默认目录），所以这里还要比较
        // modelsRoot 是不是被用户改过：改过就接管，否则就是新配置、什么都不用做
        let default_root = default_models_root().to_string_lossy().to_string();
        let only_default_entry = self.storage.shared_models.len() == 1
            && same_path(&self.storage.shared_models[0].path, &default_root)
            && self.storage.shared_models[0].type_name.is_none();
        if self.storage.shared_models.is_empty() {
            self.storage.shared_models = vec![ModelDir {
                path: self.paths.models_root.clone(),
                type_name: None,
                is_default: true,
            }];
        } else if only_default_entry && !same_path(&self.paths.models_root, &default_root) {
            self.storage.shared_models[0].path = self.paths.models_root.clone();
        }
        normalize_model_dirs(&mut self.storage.shared_models);
        let first = self
            .storage
            .shared_models
            .first()
            .map(|m| m.path.clone())
            .unwrap_or_default();
        self.paths.models_root = first;
        clean(&mut self.storage.shared_input);
        clean(&mut self.storage.shared_output);
        // 0 表示关闭自动清理；上限 1024 GB，避免配置写成天文数字后乘法溢出
        self.storage.uv_cache_max_gb = self.storage.uv_cache_max_gb.min(MAX_UV_CACHE_GB);
        if self.network.listen_default.trim().is_empty() {
            self.network.listen_default = "127.0.0.1".to_string();
        }
        let [lo, hi] = self.network.port_range;
        if lo < 1024 || hi <= lo {
            self.network.port_range = [8188, 8288];
        }
        if self.network.detect_timeout_ms < 10_000 {
            self.network.detect_timeout_ms = 1_800_000;
        }
        // venv 旧环境最多保留 30 天；0 表示不保留。避免配置里写个天文数字把磁盘占满
        self.sync.keep_old_venv_days = self.sync.keep_old_venv_days.min(30);
        // 自动快照最多保留 1..=2000 条，默认 20；手动快照不自动清理
        self.sync.snapshot_max = self.sync.snapshot_max.clamp(1, 2000);
        // v1 的默认值是 200。旧配置里仍是 200 的，升级时迁到新的默认 20；
        // 用户主动改成其它值的保持不动
        if self.version < 2 && self.sync.snapshot_max == 200 {
            self.sync.snapshot_max = 20;
        }
        // 同步缓存按“最近几次”保留，默认 5；上限 100，避免配置写错后误删太多
        self.sync.sync_cache_keep = self.sync.sync_cache_keep.clamp(1, 100);
        // v4：旧配置里仍是旧版默认镜像的，视为没有主动设置，清空后走官方源
        if self.version < 4 {
            let is_url = |value: &Option<String>, url: &str| {
                value
                    .as_deref()
                    .map(|v| {
                        v.trim()
                            .trim_end_matches('/')
                            .eq_ignore_ascii_case(url.trim_end_matches('/'))
                    })
                    .unwrap_or(false)
            };
            if is_url(&self.mirrors.pypi, LEGACY_DEFAULT_PYPI) {
                self.mirrors.pypi = None;
            }
            if is_url(&self.mirrors.torch, LEGACY_DEFAULT_TORCH) {
                self.mirrors.torch = None;
            }
        }
        // v5 之前配色平铺在 ui 下：把旧值迁进暗色配色，亮色配色使用默认值
        let legacy_accent = self.ui.legacy_accent.take();
        let legacy_bg_top = self.ui.legacy_bg_top.take();
        let legacy_bg_mid = self.ui.legacy_bg_mid.take();
        let legacy_bg_bottom = self.ui.legacy_bg_bottom.take();
        if self.version < 5 {
            self.ui.dark = ThemePalette {
                accent: legacy_accent.unwrap_or_else(|| DEFAULT_ACCENT.to_string()),
                bg_top: legacy_bg_top.unwrap_or_else(|| DEFAULT_BG_TOP.to_string()),
                bg_mid: legacy_bg_mid.unwrap_or_else(|| DEFAULT_BG_MID.to_string()),
                bg_bottom: legacy_bg_bottom.unwrap_or_else(|| DEFAULT_BG_BOTTOM.to_string()),
                card_color: DEFAULT_CARD_COLOR.to_string(),
            };
            self.ui.light = default_light_palette();
        }
        self.ui.theme = if self.ui.theme.trim().eq_ignore_ascii_case("light") {
            "light".to_string()
        } else {
            "dark".to_string()
        };

        fn clean_color(v: &mut String, fallback: &str) {
            let s = v.trim().to_ascii_lowercase();
            let valid =
                s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit());
            *v = if valid { s } else { fallback.to_string() };
        }
        let dark_defaults = default_dark_palette();
        let light_defaults = default_light_palette();
        for (palette, defaults) in [
            (&mut self.ui.dark, &dark_defaults),
            (&mut self.ui.light, &light_defaults),
        ] {
            clean_color(&mut palette.accent, &defaults.accent);
            clean_color(&mut palette.bg_top, &defaults.bg_top);
            clean_color(&mut palette.bg_mid, &defaults.bg_mid);
            clean_color(&mut palette.bg_bottom, &defaults.bg_bottom);
            clean_color(&mut palette.card_color, &defaults.card_color);
        }
        self.ui.card_transparency = self.ui.card_transparency.min(MAX_CARD_TRANSPARENCY);
        if self.version < SETTINGS_VERSION {
            self.version = SETTINGS_VERSION;
        }
    }
}

/// Windows 路径比较：大小写不敏感、忽略结尾分隔符。
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim().trim_end_matches(['\\', '/']).to_ascii_lowercase()
    }
    !a.trim().is_empty() && norm(a) == norm(b)
}

/// 去空行、去重（路径大小写不敏感）、保证正好一个「下载落点」。
pub fn normalize_model_dirs(dirs: &mut Vec<ModelDir>) {
    let mut seen: Vec<(String, String)> = Vec::new();
    dirs.retain_mut(|d| {
        d.path = d.path.trim().to_string();
        if d.path.is_empty() {
            return false;
        }
        d.type_name = d
            .type_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let key = (
            d.path.trim_end_matches(['\\', '/']).to_ascii_lowercase(),
            d.type_name.clone().unwrap_or_default(),
        );
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
    // 只允许一个下载落点：保留第一个标了的，其余清掉
    let mut claimed = false;
    for d in dirs.iter_mut() {
        if d.is_default {
            if claimed {
                d.is_default = false;
            } else {
                claimed = true;
            }
        }
    }
    if !claimed {
        if let Some(first) = dirs.first_mut() {
            first.is_default = true;
        }
    }
}

// ---------------------------------------------------------------- 实例模型

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstanceState {
    Uninitialized,
    Installing,
    Ready,
    Starting,
    Running,
    Stopping,
    Stopped,
    Updating,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineDetail {
    pub torch_index_url: String,
    pub torch_version: String,
    pub cuda_tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComfyRef {
    pub channel: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerOpts {
    pub listen: String,
    pub port: u16,
    pub cors_origin: Option<String>,
    pub extra_args: Vec<String>,
}

/// 云端实例：只记地址，不在这台机器上装环境。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteTarget {
    /// 域名或 IP（不带协议、不带端口）
    pub host: String,
    pub port: u16,
    /// true = https，false = http
    pub tls: bool,
}

impl RemoteTarget {
    /// 浏览器里用的完整地址；IPv6 要加方括号。
    pub fn url(&self) -> String {
        let host = self.host.trim();
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        format!(
            "{}://{}:{}",
            if self.tls { "https" } else { "http" },
            host,
            self.port
        )
    }
}

fn default_instance_kind() -> String {
    "local".to_string()
}

/// 实例级覆盖：留空 = 继承全局设置。
/// 用 `default` 是为了让旧的 instance.json（没有这个字段）仍能解析。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceOverrides {
    pub pypi: Option<String>,
    pub git: Option<String>,
    pub hf: Option<String>,
    /// HuggingFace 离线：只用本地缓存，不发起网络请求。
    pub hf_offline: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceConfig {
    pub id: String,
    pub name: String,
    pub root_dir: String,
    pub engine: String,
    pub engine_detail: Option<EngineDetail>,
    pub python_version: String,
    /// 这个实例用哪个 Python 环境。旧实例没有这段，默认自建 venv。
    #[serde(default)]
    pub python: PythonEnv,
    pub comfy: ComfyRef,
    pub server: ServerOpts,
    /// 模型 / 输入 / 输出目录怎么落。旧实例没有这段，按默认（用共享模型、自己管输入输出）解释。
    #[serde(default)]
    pub storage: InstanceStorage,
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub overrides: InstanceOverrides,
    /// 节点依赖的自动模式：off / remind / auto；旧实例没有这段，按 remind 解释。
    #[serde(default)]
    pub node_deps_auto: Option<String>,
    /// 进程非人为停止后自动重启；旧实例默认关闭
    #[serde(default)]
    pub auto_restart: bool,
    /// local（本机装环境）| remote（只存地址）
    #[serde(default = "default_instance_kind")]
    pub kind: String,
    /// 仅 kind = remote 时有效。
    #[serde(default)]
    pub remote: Option<RemoteTarget>,
    pub imported: bool,
    pub state: InstanceState,
    pub created_at: String,
    pub last_started_at: Option<String>,
}

impl InstanceConfig {
    pub fn is_remote(&self) -> bool {
        self.kind.eq_ignore_ascii_case("remote") || self.remote.is_some()
    }
}

/// 实例的 Python 环境：自建 venv（默认）或指向已有环境。
///
/// 每个实例自己定，没有全局 Python 设置。
/// - managed：App 在实例目录里建 `.venv`，用引擎矩阵里选的 Python 版本；
/// - existing：用用户指定的环境（venv 目录、`python.exe`、便携版 `python_embeded` 都行），
///   App 只读它，不建、不删。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PythonEnv {
    /// managed | existing
    pub mode: String,
    /// existing 时的路径：目录或解释器；相对路径按实例根解析；留空 = 自动在实例目录里建。
    pub path: Option<String>,
}

impl Default for PythonEnv {
    fn default() -> Self {
        Self {
            mode: "managed".to_string(),
            path: None,
        }
    }
}

impl PythonEnv {
    pub fn is_existing(&self) -> bool {
        self.mode.trim().eq_ignore_ascii_case("existing")
    }
}

/// 输入/输出目录的归属：自己管 / 全局共享 / 自己指定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DirChoice {
    /// own | shared | custom
    pub mode: String,
    /// mode = custom 时用；其它模式忽略。
    pub path: Option<String>,
}

impl Default for DirChoice {
    fn default() -> Self {
        Self {
            mode: "own".to_string(),
            path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceStorage {
    /// 用不用全局共享模型目录（总开关）
    pub use_shared_models: bool,
    /// 总开关开着时，这个实例不要的共享目录（按路径记）。
    pub disabled_shared_models: Vec<String>,
    /// 只对这个实例生效的附加模型目录。
    pub extra_models: Vec<ModelDir>,
    pub input: DirChoice,
    pub output: DirChoice,
}

impl Default for InstanceStorage {
    fn default() -> Self {
        Self {
            use_shared_models: true,
            disabled_shared_models: Vec::new(),
            extra_models: Vec::new(),
            input: DirChoice::default(),
            output: DirChoice::default(),
        }
    }
}

// ---------------------------------------------------------------- 读写

pub fn load_settings() -> Result<GlobalSettings, AppError> {
    let path = config_path();
    let mut settings = if path.exists() {
        match fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<GlobalSettings>(&raw).ok())
        {
            Some(s) => s,
            None => {
                // 主文件损坏：尝试 .bak
                let bak = sibling(&path, ".bak");
                let recovered = fs::read_to_string(&bak)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<GlobalSettings>(&raw).ok());
                match recovered {
                    Some(s) => {
                        log::warn!("config.json 损坏，已从 .bak 恢复");
                        s
                    }
                    None => {
                        log::warn!("config.json 与 .bak 均不可用，使用默认设置。");
                        GlobalSettings::default()
                    }
                }
            }
        }
    } else {
        GlobalSettings::default()
    };
    settings.normalize();
    Ok(settings)
}

pub fn save_settings(settings: &GlobalSettings) -> Result<(), AppError> {
    let mut normalized = settings.clone();
    normalized.normalize();
    let json = serde_json::to_string_pretty(&normalized)?;
    atomic_write(&config_path(), json.as_bytes())
}

/// 原子写：备份旧文件 → 写 tmp → 落盘 → rename。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let _ = fs::copy(path, sibling(path, ".bak"));
    }
    // 固定 `.tmp` 会让两个并发写互相截断；进程 id + 单调序号保证唯一。
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = sibling(path, &format!(".tmp-{}-{seq}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧版 config.json（没有 hfOffline）必须还能解析，否则用户设置会被静默重置
    #[test]
    fn old_global_config_without_new_fields_still_parses() {
        let raw = r#"{
            "version": 1,
            "mirrors": { "pypi": "https://mirror.example/simple", "git": null, "hf": null, "githubProxy": null },
            "paths": { "python": null, "git": null, "modelsRoot": "D:\\models" },
            "install": {},
            "network": { "listenDefault": "127.0.0.1", "portRange": [8188, 8288], "detectTimeoutMs": 1000 },
            "ui": { "theme": "dark", "locale": "zh-CN", "logoBg": "\u0023211927" }
        }"#;
        let s: GlobalSettings = serde_json::from_str(raw).expect("old config should parse");
        assert_eq!(s.paths.models_root, "D:\\models");
        assert_eq!(
            s.mirrors.pypi.as_deref(),
            Some("https://mirror.example/simple")
        );
        assert!(
            s.mirrors.torch.is_none(),
            "旧 Git 镜像字段应被忽略，不能当成 PyTorch 镜像"
        );
        assert!(!s.network.hf_offline, "缺省应为 false");
        assert_eq!(s.ui.font_size, 13);
        assert_eq!(s.ui.card_transparency, DEFAULT_CARD_TRANSPARENCY);
        assert_eq!(s.ui.dark.accent, DEFAULT_ACCENT);
        assert_eq!(s.ui.dark.bg_top, DEFAULT_BG_TOP);
        assert_eq!(s.ui.dark.card_color, DEFAULT_CARD_COLOR);
        assert_eq!(s.ui.light.bg_top, DEFAULT_LIGHT_BG_TOP);
        assert_eq!(s.sync.snapshot_max, 20, "缺省保留 20 条自动快照");
        assert_eq!(s.sync.sync_cache_keep, 5, "缺省保留最近 5 次同步缓存");
        assert_eq!(s.storage.uv_cache_max_gb, 10, "缺省 UV 缓存上限 10 GB");
    }

    #[test]
    fn snapshot_limit_is_clamped() {
        let mut s = GlobalSettings::default();
        s.sync.snapshot_max = 0;
        s.normalize();
        assert_eq!(s.sync.snapshot_max, 1);
        s.sync.snapshot_max = 99_999;
        s.normalize();
        assert_eq!(s.sync.snapshot_max, 2000);
    }

    #[test]
    fn old_default_snapshot_limit_migrates_to_20() {
        let mut s = GlobalSettings::default();
        s.version = 1;
        s.sync.snapshot_max = 200;
        s.normalize();
        assert_eq!(s.sync.snapshot_max, 20, "旧的默认 200 应迁到新默认 20");
        assert_eq!(s.version, 6);

        let mut custom = GlobalSettings::default();
        custom.version = 1;
        custom.sync.snapshot_max = 150;
        custom.normalize();
        assert_eq!(
            custom.sync.snapshot_max, 150,
            "用户主动设置的其它值不能被覆盖"
        );
        assert_eq!(custom.version, 6);
    }

    #[test]
    fn old_config_gets_default_wheel_library() {
        let expected = default_wheels_dir().to_string_lossy().to_string();

        let mut old = GlobalSettings::default();
        old.version = 5;
        old.wheels = None;
        old.normalize();
        assert_eq!(old.wheels.as_deref(), Some(expected.as_str()));
        assert_eq!(old.version, 6);

        let mut custom = GlobalSettings::default();
        custom.version = 5;
        custom.wheels = Some("D:\\my-wheels".to_string());
        custom.normalize();
        assert_eq!(custom.wheels.as_deref(), Some("D:\\my-wheels"));

        // 升级完成后用户主动清空，不再每次启动强行补回来。
        let mut cleared = GlobalSettings::default();
        cleared.version = 6;
        cleared.wheels = None;
        cleared.normalize();
        assert!(cleared.wheels.is_none());
    }

    #[test]
    fn legacy_default_mirrors_are_cleared() {
        assert!(Mirrors::default().pypi.is_none());
        assert!(Mirrors::default().torch.is_none());

        let mut s = GlobalSettings::default();
        s.version = 3;
        s.mirrors.pypi = Some("https://mirrors.aliyun.com/pypi/simple".to_string());
        s.mirrors.torch = Some("https://mirrors.aliyun.com/pytorch-wheels/".to_string());
        s.normalize();
        assert!(s.mirrors.pypi.is_none(), "旧默认 PyPI 镜像应清空");
        assert!(s.mirrors.torch.is_none(), "旧默认 PyTorch 镜像应清空");
        assert_eq!(s.version, 6);

        let mut custom = GlobalSettings::default();
        custom.version = 3;
        custom.mirrors.pypi = Some("https://mirror.example/simple".to_string());
        custom.normalize();
        assert_eq!(
            custom.mirrors.pypi.as_deref(),
            Some("https://mirror.example/simple"),
            "用户主动设置的镜像不能被清掉"
        );
    }

    #[test]
    fn old_storage_without_uv_limit_gets_default_10() {
        let s: GlobalSettings =
            serde_json::from_str(r#"{"storage":{"sharedInput":null,"sharedOutput":null}}"#)
                .expect("partial storage should parse");
        assert_eq!(s.storage.uv_cache_max_gb, 10);
    }

    #[test]
    fn uv_cache_limit_defaults_to_10_and_clamps() {
        assert_eq!(StorageSettings::default().uv_cache_max_gb, 10);
        let mut s = GlobalSettings::default();
        s.storage.uv_cache_max_gb = 0;
        s.normalize();
        assert_eq!(s.storage.uv_cache_max_gb, 0, "0 表示关闭自动清理");
        s.storage.uv_cache_max_gb = 99_999;
        s.normalize();
        assert_eq!(s.storage.uv_cache_max_gb, MAX_UV_CACHE_GB);
    }

    #[test]
    fn sync_cache_retention_is_clamped() {
        let mut s = GlobalSettings::default();
        s.sync.sync_cache_keep = 0;
        s.normalize();
        assert_eq!(s.sync.sync_cache_keep, 1);
        s.sync.sync_cache_keep = 99_999;
        s.normalize();
        assert_eq!(s.sync.sync_cache_keep, 100);
    }

    #[test]
    fn invalid_theme_colors_fall_back_to_defaults() {
        let mut s = GlobalSettings::default();
        s.ui.dark.accent = "red".to_string();
        s.ui.dark.bg_top = "#12".to_string();
        s.ui.light.card_color = "white".to_string();
        s.normalize();
        assert_eq!(s.ui.dark.accent, DEFAULT_ACCENT);
        assert_eq!(s.ui.dark.bg_top, DEFAULT_BG_TOP);
        assert_eq!(s.ui.light.card_color, DEFAULT_LIGHT_CARD_COLOR);
    }

    #[test]
    fn card_transparency_is_clamped() {
        let mut s = GlobalSettings::default();
        s.ui.card_transparency = 90;
        s.normalize();
        assert_eq!(s.ui.card_transparency, MAX_CARD_TRANSPARENCY);

        s.ui.card_transparency = 0;
        s.normalize();
        assert_eq!(s.ui.card_transparency, 0);
    }

    /// 旧版 instance.json（没 overrides / engineDetail）必须还能解。
    #[test]
    fn old_instance_config_without_overrides_still_parses() {
        let raw = r#"{
            "id": "846a6efc",
            "name": "ComfyUI-test",
            "rootDir": "D:\\project\\ComfyFleet\\ComfyUI-test",
            "engine": "cpu",
            "pythonVersion": "3.13",
            "comfy": { "channel": "stable", "ref": "", "commit": "", "version": null },
            "server": { "listen": "127.0.0.1", "port": 8188, "corsOrigin": null, "extraArgs": [] },
            "env": {},
            "imported": false,
            "state": "uninitialized",
            "createdAt": "2026-10-28T00:00:00Z",
            "lastStartedAt": null
        }"#;
        let c: InstanceConfig = serde_json::from_str(raw).expect("old instance should parse");
        assert_eq!(c.id, "846a6efc");
        assert!(c.overrides.pypi.is_none());
        assert!(c.overrides.hf_offline.is_none());
    }

    // ------------------------------------------------------------ 数据目录迁移

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "comfyfleet-config-test-{tag}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    /// 旧数据目录整体搬到安装目录：同盘 rename，旧目录随之消失
    #[test]
    fn migration_moves_legacy_tree() {
        let src = temp_path("legacy");
        let dst = temp_path("data");
        fs::create_dir_all(src.join("instances").join("846a6efc")).unwrap();
        fs::write(src.join("config.json"), br#"{"version":1}"#).unwrap();
        fs::write(
            src.join("instances").join("846a6efc").join("instance.json"),
            b"{}",
        )
        .unwrap();

        migrate_legacy(&src, &dst);

        assert!(dst.join("config.json").exists(), "config.json 应搬到新目录");
        assert!(
            dst.join("instances")
                .join("846a6efc")
                .join("instance.json")
                .exists(),
            "实例目录应完整搬过去"
        );
        assert!(!src.exists(), "迁移成功后旧目录应被清理");

        let _ = fs::remove_dir_all(&dst);
    }

    /// 新目录已经有 config.json（已初始化）时，旧目录必须原样保。
    #[test]
    fn migration_never_touches_initialized_dir() {
        let src = temp_path("legacy");
        let dst = temp_path("data");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("config.json"), b"legacy").unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("config.json"), b"current").unwrap();

        migrate_legacy(&src, &dst);

        assert_eq!(
            fs::read_to_string(dst.join("config.json")).unwrap(),
            "current",
            "新目录的配置不能被覆 "
        );
        assert!(src.join("config.json").exists(), "旧目录要原样保留");

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    /// 新目录非空但没有 config.json：无法判断谁是谁，跳过迁移而不是覆。
    #[test]
    fn migration_skips_unknown_non_empty_dir() {
        let src = temp_path("legacy");
        let dst = temp_path("data");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("config.json"), b"legacy").unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("stray.txt"), b"x").unwrap();

        migrate_legacy(&src, &dst);

        assert!(!dst.join("config.json").exists(), "不该往陌生目录里搬东西");
        assert!(src.join("config.json").exists(), "旧目录要原样保留");

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    /// 旧配置只有一个 modelsRoot：迁移成共享模型列表的第一项（下载落点）。
    #[test]
    fn sync_settings_default_and_clamp() {
        // 旧 config 没有 sync 段也要能打开同步页：默认不删多余节点/模型，旧 venv 保留 7 天。
        let mut s: GlobalSettings = serde_json::from_str("{}").unwrap();
        assert!(!s.sync.remove_extra_nodes);
        assert!(!s.sync.remove_extra_models);
        assert_eq!(s.sync.keep_old_venv_days, 7);

        s.sync.keep_old_venv_days = 999;
        s.normalize();
        assert_eq!(s.sync.keep_old_venv_days, 30);
    }

    #[test]
    fn legacy_models_root_migrates_into_shared_models() {
        let mut s = GlobalSettings::default();
        s.storage.shared_models.clear();
        s.paths.models_root = "D:\\models".to_string();

        s.normalize();

        assert_eq!(s.storage.shared_models.len(), 1);
        assert_eq!(s.storage.shared_models[0].path, "D:\\models");
        assert!(s.storage.shared_models[0].is_default);
        assert!(s.storage.shared_models[0].type_name.is_none());
    }

    /// The legacy default models path moves with the data directory.
    #[test]
    fn legacy_default_models_root_is_rewritten() {
        let legacy_models = legacy_root().join("shared").join("models");
        let mut s = GlobalSettings::default();
        s.storage.shared_models.clear();
        s.paths.models_root = legacy_models.to_string_lossy().to_string();

        s.normalize();

        assert_eq!(
            s.storage.shared_models[0].path,
            default_models_root().to_string_lossy().to_string()
        );
    }

    /// 老配置文件里没有 storage 段：modelsRoot 是用户改过的值，要接管成共享目录
    #[test]
    fn old_config_without_storage_takes_over_models_root() {
        let raw = r#"{
            "version": 1,
            "paths": { "python": null, "git": null, "modelsRoot": "D:\\my-models" },
            "install": {}, "network": {}, "ui": {}
        }"#;
        let mut s: GlobalSettings = serde_json::from_str(raw).expect("old config");
        assert!(s.storage.shared_models.len() == 1, "默认值先顶上");

        s.normalize();

        assert_eq!(s.storage.shared_models[0].path, "D:\\my-models");
        assert!(s.storage.shared_models[0].is_default);
    }

    /// 新配置里用户就是想要默认目录：modelsRoot 同步成它，不会被来回。
    #[test]
    fn new_config_keeps_default_shared_dir() {
        let mut s = GlobalSettings::default();
        let expected = default_models_root().to_string_lossy().to_string();

        s.normalize();

        assert_eq!(s.storage.shared_models.len(), 1);
        assert_eq!(s.storage.shared_models[0].path, expected);
        assert_eq!(s.paths.models_root, expected);
    }

    /// 共享目录去空行、去重，且只留一个下载落。
    #[test]
    fn shared_models_are_deduped_and_single_default() {
        let mut s = GlobalSettings::default();
        s.storage.shared_models = vec![
            ModelDir {
                path: " D:\\a ".to_string(),
                type_name: None,
                is_default: true,
            },
            ModelDir {
                path: "d:\\a\\".to_string(),
                type_name: None,
                is_default: true,
            },
            ModelDir {
                path: "".to_string(),
                type_name: None,
                is_default: false,
            },
            ModelDir {
                path: "D:\\b".to_string(),
                type_name: Some(" loras ".to_string()),
                is_default: true,
            },
        ];

        s.normalize();

        assert_eq!(s.storage.shared_models.len(), 2);
        assert_eq!(s.storage.shared_models[0].path, "D:\\a");
        assert_eq!(
            s.storage.shared_models[1].type_name.as_deref(),
            Some("loras")
        );
        assert!(s.storage.shared_models[0].is_default);
        assert!(!s.storage.shared_models[1].is_default, "只允许一个下载落点");
        assert_eq!(s.paths.models_root, "D:\\a", "旧字段跟着第一个共享目录走");
    }

    /// "目录存在但不可写"必须被判成不可写，否则数据会写一半失败。
    #[test]
    fn ensure_writable_probes_real_writes() {
        let dir = temp_path("writable");
        assert!(ensure_writable(&dir), "新建目录应可写");
        assert!(dir.is_dir());

        let file = temp_path("file");
        fs::write(&file, b"x").unwrap();
        assert!(!ensure_writable(&file), "被文件占位的路径不可写");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&file);
    }

    /// 安装目录就是 exe 所在目录（数据目录挂在它下面）
    #[test]
    fn install_dir_is_exe_parent() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(install_dir(), exe.parent().unwrap().to_path_buf());
    }
}
