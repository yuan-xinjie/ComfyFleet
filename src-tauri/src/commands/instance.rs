use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tauri::Manager;

use crate::config::{
    self, ComfyRef, DirChoice, EngineDetail, InstanceConfig, InstanceOverrides, InstanceState,
    InstanceStorage, ModelDir, PythonEnv, RemoteTarget, ServerOpts,
};
use crate::error::AppError;

const ILLEGAL_NAME_CHARS: &str = "\\/:*?\"<>|";

/// 实例自己的元数据目录：标记、快照都放在这里，不再往实例根目录散落点文件。
const INSTANCE_META_DIR: &str = ".comfyfleet";
const INSTANCE_MARKER: &str = ".instance";
const LEGACY_INSTANCE_MARKER: &str = ".comfyfleet-instance";
/// 跟着实例目录走的可移植参数文件。导入时优先读它，而不是重新猜一套默认值。
const PROFILE_FILE: &str = "profile.json";
const PROFILE_FORMAT: u32 = 1;

/// 导入/重定位都会“先检查目录归属、再写登记”，必须串行，避免两次并发操作
/// 同时通过检查后把同一个根目录登记两次。
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(INSTANCE_META_DIR).join(INSTANCE_MARKER)
}

fn profile_path(root: &Path) -> PathBuf {
    root.join(INSTANCE_META_DIR).join(PROFILE_FILE)
}

/// 实例级全部可配置项的可移植快照。
///
/// 这里**不包含** `rootDir / state / lastStartedAt`：它们属于运行环境或登记位置，
/// 重新导入/重定位时必须以实际目录和启动结果为准。`rootDir` 只作为重定位时
/// 修正绝对路径的“旧根”提示，不直接信任。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceProfile {
    pub format: u32,
    pub saved_at: String,
    pub id: String,
    pub name: String,
    pub root_dir: String,
    pub engine: String,
    pub engine_detail: Option<EngineDetail>,
    pub python_version: String,
    pub python: PythonEnv,
    pub comfy: ComfyRef,
    pub server: ServerOpts,
    pub storage: InstanceStorage,
    pub env: BTreeMap<String, String>,
    pub overrides: InstanceOverrides,
    pub node_deps_auto: Option<String>,
    pub auto_restart: bool,
    pub created_at: String,
}

impl Default for InstanceProfile {
    fn default() -> Self {
        Self {
            format: PROFILE_FORMAT,
            saved_at: String::new(),
            id: String::new(),
            name: String::new(),
            root_dir: String::new(),
            engine: "cpu".to_string(),
            engine_detail: None,
            python_version: "3.12".to_string(),
            python: PythonEnv::default(),
            comfy: ComfyRef {
                channel: "stable".to_string(),
                reference: String::new(),
                commit: String::new(),
                version: None,
            },
            server: ServerOpts {
                listen: "127.0.0.1".to_string(),
                port: 8189,
                cors_origin: None,
                extra_args: Vec::new(),
            },
            storage: InstanceStorage::default(),
            env: BTreeMap::new(),
            overrides: InstanceOverrides::default(),
            node_deps_auto: None,
            auto_restart: false,
            created_at: String::new(),
        }
    }
}

impl InstanceProfile {
    fn from_config(cfg: &InstanceConfig) -> Self {
        Self {
            format: PROFILE_FORMAT,
            saved_at: chrono::Utc::now().to_rfc3339(),
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            root_dir: cfg.root_dir.clone(),
            engine: cfg.engine.clone(),
            engine_detail: cfg.engine_detail.clone(),
            python_version: cfg.python_version.clone(),
            python: cfg.python.clone(),
            comfy: cfg.comfy.clone(),
            server: cfg.server.clone(),
            storage: cfg.storage.clone(),
            env: cfg.env.clone(),
            overrides: cfg.overrides.clone(),
            node_deps_auto: cfg.node_deps_auto.clone(),
            auto_restart: cfg.auto_restart,
            created_at: cfg.created_at.clone(),
        }
    }
}

fn legacy_marker_path(root: &Path) -> PathBuf {
    root.join(LEGACY_INSTANCE_MARKER)
}

/// 兼容旧版根目录下的 `.comfyfleet-instance`：存在任一个都算已归属。
fn marker_exists(root: &Path) -> bool {
    marker_path(root).exists() || legacy_marker_path(root).exists()
}

/// 把旧标记搬进 `.comfyfleet\.instance`。只搬文件，不覆盖已经存在的新标记。
fn migrate_instance_marker(root: &Path) -> Result<(), AppError> {
    let old = legacy_marker_path(root);
    let new = marker_path(root);
    if new.exists() || !old.is_file() {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(&old, &new).is_err() {
        let bytes = std::fs::read(&old)?;
        config::atomic_write(&new, &bytes)?;
        std::fs::remove_file(&old)?;
    }
    Ok(())
}

fn read_profile(root: &Path) -> Result<Option<InstanceProfile>, AppError> {
    let path = profile_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    // 参数文件正常只有几 KB；限制大小，避免从不可信目录读入超大文件把内存打满。
    let size = std::fs::metadata(&path)
        .map_err(|e| AppError::new("PROFILE_READ", format!("参数文件读取失败：{e}")))?
        .len();
    if size > 4 * 1024 * 1024 {
        return Err(AppError::new(
            "PROFILE_TOO_LARGE",
            format!("参数文件过大（{size} 字节），已拒绝读取"),
        ));
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| AppError::new("PROFILE_READ", format!("参数文件读取失败：{e}")))?;
    // 用户可能用记事本/旧版 PowerShell 编辑，UTF-8 BOM 不该被当成 JSON 语法错误。
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let profile: InstanceProfile = serde_json::from_str(raw)
        .map_err(|e| AppError::new("PROFILE_CORRUPT", format!("参数文件格式损坏：{e}")))?;
    if profile.format > PROFILE_FORMAT {
        return Err(AppError::new(
            "PROFILE_VERSION",
            format!(
                "参数文件版本 {} 高于当前启动器支持的版本 {}，请升级 ComfyFleet",
                profile.format, PROFILE_FORMAT
            ),
        ));
    }
    Ok(Some(profile))
}

/// 把实例配置写成 `<root>/.comfyfleet/profile.json`。
///
/// 失败只记日志：主登记文件 `instance.json` 已经写好时，不能因为目录只读
/// 或权限问题让设置保存失败；下次保存会再尝试。
fn write_profile(cfg: &InstanceConfig) -> Result<(), AppError> {
    if cfg.is_remote() {
        return Ok(());
    }
    let root = Path::new(&cfg.root_dir);
    if !root.is_dir() {
        return Ok(());
    }
    let profile = InstanceProfile::from_config(cfg);
    config::atomic_write(
        &profile_path(root),
        serde_json::to_string_pretty(&profile)?.as_bytes(),
    )
}

/// 老实例第一次被读取时补一份参数文件；已有文件（包括损坏文件）一律不覆盖。
fn ensure_profile(cfg: &InstanceConfig) {
    if cfg.is_remote() {
        return;
    }
    let root = Path::new(&cfg.root_dir);
    match read_profile(root) {
        Ok(None) => {
            if let Err(e) = write_profile(cfg) {
                log::warn!("实例「{}」补写参数文件失败：{}", cfg.name, e.message);
            }
        }
        Ok(Some(_)) => {}
        // 损坏的文件不要静默覆盖：导入预览会提示，用户仍可手工修复。
        Err(e) => log::warn!("实例「{}」参数文件不可读：{}", cfg.name, e.message),
    }
}

fn read_marker_id(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(marker_path(root)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

fn write_marker(root: &Path, id: &str, name: &str, imported: bool) -> Result<(), AppError> {
    config::atomic_write(
        &marker_path(root),
        serde_json::to_string(&serde_json::json!({
            "id": id,
            "name": name,
            "imported": imported
        }))?
        .as_bytes(),
    )
}

/// 导入和重定位共用的根目录识别：既接受实例根，也接受 `...\\ComfyUI` 本体目录。
fn resolve_import_root(raw: &str) -> Result<PathBuf, AppError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::new("PATH_EMPTY", "请填写实例目录"));
    }
    let picked = PathBuf::from(raw);
    if !picked.is_absolute() {
        return Err(AppError::new(
            "PATH_NOT_ABSOLUTE",
            "请填绝对路径（例如 D:\\ComfyUI\\ComfyUI_windows_portable）",
        ));
    }
    if !picked.exists() {
        return Err(AppError::new(
            "PATH_MISSING",
            format!("目录不存在：{}", picked.display()),
        ));
    }
    if picked.join("ComfyUI").join("main.py").is_file() {
        Ok(picked)
    } else if picked.join("main.py").is_file() {
        picked
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| AppError::new("PATH_INVALID", "无法确定实例根目录"))
    } else {
        Err(AppError::new(
            "COMFY_MISSING",
            format!(
                "{} 里找不到 ComfyUI/main.py（便携版请选包含 ComfyUI 子目录的那一层）",
                picked.display()
            ),
        ))
    }
}

/// 重定位时只改“在新旧根之间的绝对路径”，外部路径保持原样。
/// 注意：环境变量值不一定是路径，未命中时必须保留原始空白，不能顺手 trim。
fn rebase_root_path(old_root: &str, new_root: &str, value: &str) -> String {
    let original = value;
    let value = value.trim();
    if value.is_empty() || !Path::new(value).is_absolute() {
        return original.to_string();
    }
    let old = old_root.trim_end_matches(['\\', '/']);
    let new = new_root.trim_end_matches(['\\', '/']);
    if old.is_empty() || new.is_empty() {
        return original.to_string();
    }
    // 用户可能混用 `/` 和 `\`（手输路径、便携版脚本、旧配置都会出现），
    // 比较前统一成正斜杠；返回时再统一成 Windows 反斜杠。
    let old_slash = old.replace('\\', "/").to_ascii_lowercase();
    let value_slash = value.replace('\\', "/");
    let lower_value = value_slash.to_ascii_lowercase();
    if lower_value == old_slash {
        return new.to_string();
    }
    let prefix = format!("{old_slash}/");
    if !lower_value.starts_with(&prefix) {
        return original.to_string();
    }
    let rest = &value_slash[old_slash.len() + 1..];
    format!("{new}\\{}", rest.replace('/', "\\"))
}

fn strip_path_quotes(value: &str) -> (&str, Option<char>) {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        (&value[1..value.len() - 1], Some(bytes[0] as char))
    } else {
        (value, None)
    }
}

/// 重写 CLI 参数里的路径，支持 `--out=D:\\x`、`--out="D:\\x"` 和整段带引号的路径。
/// 参数可能包含空格或等号，不能简单按 whitespace 重新切。
fn rebase_cli_arg(old_root: &str, new_root: &str, arg: &str) -> String {
    fn rebase_wrapped(old_root: &str, new_root: &str, value: &str) -> Option<String> {
        let (inner, quote) = strip_path_quotes(value);
        if inner.is_empty() || !Path::new(inner).is_absolute() {
            return None;
        }
        let rebased = rebase_root_path(old_root, new_root, inner);
        if rebased == inner {
            return None;
        }
        Some(match quote {
            Some(q) => format!("{q}{rebased}{q}"),
            None => rebased,
        })
    }

    if let Some((key, value)) = arg.split_once('=') {
        if let Some(rebased) = rebase_wrapped(old_root, new_root, value) {
            return format!("{key}={rebased}");
        }
    }
    rebase_wrapped(old_root, new_root, arg).unwrap_or_else(|| arg.to_string())
}

fn rebase_optional_path(old_root: &str, new_root: &str, value: Option<String>) -> Option<String> {
    value.map(|p| rebase_root_path(old_root, new_root, &p))
}

/// 把配置里所有“位于旧根内”的绝对路径重写到新根；不移动磁盘文件。
fn rebase_instance_path_fields(cfg: &mut InstanceConfig, old_root: &str, new_root: &str) {
    cfg.python.path = rebase_optional_path(old_root, new_root, cfg.python.path.take());
    for dir in &mut cfg.storage.extra_models {
        dir.path = rebase_root_path(old_root, new_root, &dir.path);
    }
    cfg.storage.disabled_shared_models = cfg
        .storage
        .disabled_shared_models
        .iter()
        .map(|p| rebase_root_path(old_root, new_root, p))
        .collect();
    cfg.storage.input.path =
        rebase_optional_path(old_root, new_root, cfg.storage.input.path.take());
    cfg.storage.output.path =
        rebase_optional_path(old_root, new_root, cfg.storage.output.path.take());
    cfg.server.extra_args = cfg
        .server
        .extra_args
        .iter()
        .map(|arg| rebase_cli_arg(old_root, new_root, arg))
        .collect();
    for value in cfg.env.values_mut() {
        *value = rebase_root_path(old_root, new_root, value);
    }
    sanitize_storage(&mut cfg.storage);
}

/// 重定位后把实例配置里的路径从旧根搬到新根，不移动磁盘文件。
fn rebase_instance_paths(mut cfg: InstanceConfig, new_root: &Path) -> InstanceConfig {
    let old_root = cfg.root_dir.clone();
    let new_root = new_root.to_string_lossy().to_string();
    cfg.root_dir = new_root.clone();
    rebase_instance_path_fields(&mut cfg, &old_root, &new_root);
    cfg
}

/// 导入弹窗用：只读参数文件摘要，不执行 torch 探测。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProfilePreview {
    pub root: String,
    pub found: bool,
    pub valid: bool,
    pub name: Option<String>,
    pub engine: Option<String>,
    pub python_version: Option<String>,
    pub saved_at: Option<String>,
    pub extra_args: usize,
    pub env_vars: usize,
    pub auto_restart: bool,
    pub node_deps_auto: Option<String>,
    /// 参数文件里会被安全策略改写/丢弃的项，导入前先告诉用户。
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

/// 实例里记录的 Python 版本统一是「主.次」（`3.13`），和引擎矩阵、下拉选项一致。
/// 导入时探到的是完整版本（`3.13.6`），如果原样存，设置页的下拉会对不上、显示成第一项。
pub(crate) fn python_minor(version: &str) -> String {
    let trimmed = version.trim();
    let mut parts = trimmed.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) if !major.is_empty() && !minor.is_empty() => {
            format!("{major}.{minor}")
        }
        _ => trimmed.to_string(),
    }
}

/// 把历史实例里的`3.13.6` 收敛成`3.13`；返回是否改过。
fn normalize_python_version(cfg: &mut InstanceConfig) -> bool {
    let normalized = python_minor(&cfg.python_version);
    if normalized == cfg.python_version {
        return false;
    }
    log::info!(
        "实例 {} 的Python 版本 {} →{}（矩阵下拉统一用主.次）",
        cfg.id,
        cfg.python_version,
        normalized
    );
    cfg.python_version = normalized;
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstanceReq {
    pub name: String,
    pub root_dir: String,
    pub engine: String,
    pub python_version: String,
    pub port: u16,
    pub channel: String,
}

/// 添加 / 编辑云端实例：只存地址，不创建本地目录。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInstanceReq {
    /// 有值= 编辑已有云端实例
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

fn validate_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::new("NAME_EMPTY", "实例名称不能为空"));
    }
    if name.chars().count() > 64 {
        return Err(AppError::new("NAME_TOO_LONG", "实例名称不能超过 64 个字符"));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(AppError::new(
            "NAME_ILLEGAL",
            "实例名称不能包含换行/制表符等控制字符",
        ));
    }
    if let Some(bad) = name.chars().find(|c| ILLEGAL_NAME_CHARS.contains(*c)) {
        return Err(AppError::new(
            "NAME_ILLEGAL",
            format!("实例名称不能包含字符，{bad}"),
        ));
    }
    Ok(())
}

/// 引擎档的固定信息：一律从 engine-matrix.json 读取。
///
/// 曾经这里硬编码了 torch 版本，和矩阵各自演化后就会对不上—
/// 数据只能有一个来源。
fn engine_detail(engine: &str) -> Result<Option<EngineDetail>, AppError> {
    let matrix = crate::matrix::load()?;
    match matrix.engines.iter().find(|e| e.id == engine) {
        Some(def) => Ok(Some(EngineDetail {
            torch_index_url: crate::matrix::torch_index_url(&matrix, def),
            torch_version: def.torch_version.clone(),
            cuda_tag: def.cuda_version.as_ref().map(|_| def.index_tag.clone()),
        })),
        None if engine == "custom" => Ok(None),
        None => Err(AppError::new(
            "ENGINE_UNKNOWN",
            format!("未知引擎档：{engine}"),
        )),
    }
}

#[tauri::command]
pub fn instance_list() -> Result<Vec<InstanceConfig>, AppError> {
    let dir = config::instances_dir();
    let mut out: Vec<InstanceConfig> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let manifest = entry.path().join("instance.json");
            if !manifest.exists() {
                continue;
            }
            match std::fs::read_to_string(&manifest) {
                Ok(raw) => match serde_json::from_str::<InstanceConfig>(&raw) {
                    Ok(mut cfg) => {
                        normalize_python_version(&mut cfg);
                        ensure_profile(&cfg);
                        out.push(cfg)
                    }
                    Err(e) => log::warn!("跳过损坏的 instance.json：{:?}：{e}", manifest),
                },
                Err(e) => log::warn!("读取 instance.json 失败，{:?}：{e}", manifest),
            }
        }
    }

    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(out)
}

/// 实例 id 的唯一合法形式：十六进制目录名（防路径穿越）。
pub fn validate_instance_id(id: &str) -> Result<(), AppError> {
    // id 会直接拼进实例目录；限长避免恶意/损坏的 profile.json 造出超长路径。
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::new("ID_INVALID", format!("非法实例 id：{id}")));
    }
    Ok(())
}

fn validate_channel(channel: &str) -> Result<(), AppError> {
    if !matches!(channel, "stable" | "dev") {
        return Err(AppError::new(
            "CHANNEL_INVALID",
            format!("不支持的更新通道「{channel}」（只支持 stable / dev）"),
        ));
    }
    Ok(())
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let text = p.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

fn normalized_root(raw: &str) -> PathBuf {
    // 新目录可能整条都不存在：递归找到最近的已存在祖先并 canonicalize，
    // 再把剩余组件拼回去。只对“直接父目录”canonicalize 会失败并退回原始
    // 字符串，导致和已有实例的 \\?\D:\... 形式永远比较不相等。
    fn walk(p: &std::path::Path) -> PathBuf {
        if let Ok(c) = p.canonicalize() {
            return c;
        }
        match p.parent() {
            Some(parent) => match p.file_name() {
                Some(name) => walk(parent).join(name),
                None => p.to_path_buf(),
            },
            None => p.to_path_buf(),
        }
    }
    strip_verbatim(walk(std::path::Path::new(raw)))
}

fn roots_overlap(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    let a = norm(a);
    let b = norm(b);
    a == b || a.starts_with(&format!("{b}\\")) || b.starts_with(&format!("{a}\\"))
}

/// 保存配置失败时，把刚被挪走的 venv 备份还原，避免“配置没落盘、venv 却没了”。
fn restore_venv_backup(root: &std::path::Path) {
    let venv = root.join(".venv");
    if venv.exists() {
        return;
    }
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(".venv.bak-") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    backups.sort_by(|a, b| b.0.cmp(&a.0));
    if let Some((_, latest)) = backups.into_iter().next() {
        if std::fs::rename(&latest, &venv).is_ok() {
            log::warn!("保存设置失败，已把 {} 还原为 .venv", latest.display());
        }
    }
}

/// 新根/待删根不能和任何其它本地实例互相包含；`exclude` 用于更新/删除自身。
fn ensure_root_isolated(raw: &str, exclude: Option<&str>) -> Result<(), AppError> {
    let new_root = normalized_root(raw);
    for other in instance_list().unwrap_or_default() {
        if other.is_remote() || exclude == Some(other.id.as_str()) {
            continue;
        }
        if roots_overlap(&new_root, &normalized_root(&other.root_dir)) {
            return Err(AppError::new(
                "ROOT_OVERLAP",
                format!(
                    "目录与已有实例「{}」的根目录重叠：{}\n两个实例不能共用/嵌套目录，否则删除时会误删对方。",
                    other.name, other.root_dir
                ),
            ));
        }
    }
    Ok(())
}

/// 读取单个实例的登记信息。
pub fn load_instance(id: &str) -> Result<InstanceConfig, AppError> {
    validate_instance_id(id)?;
    let manifest = config::instances_dir().join(id).join("instance.json");
    if !manifest.exists() {
        return Err(AppError::new("NOT_FOUND", format!("实例不存在：{id}")));
    }
    let raw = std::fs::read_to_string(&manifest)?;
    let mut cfg: InstanceConfig = serde_json::from_str(&raw)
        .map_err(|e| AppError::new("MANIFEST_CORRUPT", format!("instance.json 损坏，{e}")))?;
    if cfg.id != id {
        return Err(AppError::new(
            "MANIFEST_ID_MISMATCH",
            format!(
                "instance.json 的 id（{}）与目录名（{id}）不一致，拒绝加载",
                cfg.id
            ),
        ));
    }
    if cfg.is_remote() {
        // instance.json 可能被手工编辑/集群同步改过：每次加载都重新校验云端地址，
        // 不能只在保存时校验。
        let remote = cfg
            .remote
            .as_mut()
            .ok_or_else(|| AppError::new("REMOTE_INVALID", "云端实例缺少 remote 地址"))?;
        if remote.port == 0 {
            return Err(AppError::new("REMOTE_INVALID", "云端实例端口不能为 0"));
        }
        let (host, tls) = normalize_remote_host(&remote.host)?;
        remote.host = host;
        if let Some(tls) = tls {
            remote.tls = tls;
        }
    }
    if !cfg.is_remote() {
        // 老实例的归属标记在根目录；加载时顺手搬进 .comfyfleet，失败不影响实例使用。
        if let Err(e) = migrate_instance_marker(Path::new(&cfg.root_dir)) {
            log::warn!("实例 {} 的归属标记迁移失败：{}", cfg.id, e.message);
        }
    }
    heal_engine_detail(&mut cfg);
    sanitize_storage(&mut cfg.storage);
    // 老版本登记的实例第一次被读取时补一份参数文件，之后重导入才有东西可读。
    ensure_profile(&cfg);
    Ok(cfg)
}

/// 补齐缺失的派生数据。
///
/// `engineDetail` 记录的是**目标机实际装上的** torch；不能每次加载都拿矩阵里的
/// 推荐版本覆盖它，否则同步/手动换过 torch 后，界面会一直显示矩阵版本。
/// 这里只在历史配置完全没有这项时补一份兜底，真实值由 `refresh_instance_env`
/// 在同步后和界面主动刷新时读取。
fn heal_engine_detail(cfg: &mut InstanceConfig) {
    // 云端实例没有本地引擎 / Python，不能拿矩阵去纠正
    if cfg.is_remote() {
        return;
    }
    // 只收敛到内存里：导入时存的是完整版本的历史数据，不在加载时偷偷改用户文件，
    // 用户在设置页点保存时才会报3.13 写回去。
    normalize_python_version(cfg);
    if cfg.engine_detail.is_some() {
        return;
    }
    match engine_detail(&cfg.engine) {
        Ok(Some(fresh)) => {
            cfg.engine_detail = Some(fresh);
            if let Err(e) = save_instance(cfg) {
                log::warn!("补齐实例引擎信息后写盘失败：{}", e.message);
            }
        }
        Ok(None) => {}
        Err(e) => {
            log::warn!(
                "实例 {} 的引擎。{}」不在矩阵里，{}",
                cfg.id,
                cfg.engine,
                e.message
            );
        }
    }
}

#[tauri::command]
pub fn instance_create(req: CreateInstanceReq) -> Result<InstanceConfig, AppError> {
    // 根目录检查和归属标记写入必须原子：两个并发创建同时通过重叠检查，
    // 会留下两个登记指向同一个根目录，之后 purge 一个就会删掉另一个。
    let _registry = registry_guard();
    let name = req.name.trim().to_string();
    validate_name(&name)?;

    let root_raw = req.root_dir.trim().to_string();
    if root_raw.is_empty() {
        return Err(AppError::new("PATH_EMPTY", "实例目录不能为空"));
    }
    let root = PathBuf::from(&root_raw);
    if !root.is_absolute() {
        return Err(AppError::new(
            "PATH_NOT_ABSOLUTE",
            "实例目录必须是绝对路径（例如 D:\\ComfyUI\\stable）",
        ));
    }
    // 禁止新根与任何已有实例根互相包含：否则 purge 父目录会把另一个实例一起删掉。
    let new_root = normalized_root(&root_raw);
    if let Ok(entries) = std::fs::read_dir(config::instances_dir()) {
        for entry in entries.flatten() {
            let other_id = entry.file_name().to_string_lossy().to_string();
            if validate_instance_id(&other_id).is_err() {
                continue;
            }
            if let Ok(other) = load_instance(&other_id) {
                if other.is_remote() {
                    continue;
                }
                if roots_overlap(&new_root, &normalized_root(&other.root_dir)) {
                    return Err(AppError::new(
                        "ROOT_OVERLAP",
                        format!(
                            "新目录与已有实例「{}」的根目录重叠：{}\n两个实例不能共用/嵌套目录，否则删除时会误删对方。",
                            other.name, other.root_dir
                        ),
                    ));
                }
            }
        }
    }
    if req.port < 1024 {
        return Err(AppError::new("PORT_INVALID", "端口需大于等于 1024"));
    }

    let req_channel = req.channel.trim().to_string();
    validate_channel(&req_channel)?;
    let engine = req.engine.trim().to_string();
    let detail = engine_detail(&engine)?;

    // 已有内容但不是我们建的目录：拒绝接管，避免污染用户现有安装。
    // 已有我们自己的 marker 也不能复用：那说明这个根已经属于另一个实例，
    // 再登记会造成两个实例共享根，purge 时把对方一起删掉。
    if root.exists() {
        if marker_exists(&root) {
            return Err(AppError::new(
                "ROOT_OWNED_BY_OTHER",
                format!(
                    "目录已经属于另一个 ComfyFleet 实例，不能重复使用：{}\n请换一个空目录。",
                    root.display()
                ),
            ));
        }
        let non_empty = std::fs::read_dir(&root)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(AppError::new(
                "ROOT_NOT_EMPTY",
                format!(
                    "目录已存在且非空，{}\n请换一个空目录（「导入已有安装」是 M7 功能）。",
                    root.display()
                ),
            ));
        }
    }

    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let id = id[..8].to_string();

    let instance_dir = config::instances_dir().join(&id);
    let manifest = instance_dir.join("instance.json");
    if manifest.exists() {
        return Err(AppError::new("EXISTS", "实例登记冲突，请重试"));
    }

    std::fs::create_dir_all(&instance_dir)?;
    // 新建实例先建空的 ComfyUI 目录，安装器会识别“空占位目录”并在 clone
    // 成功后直接替换，不会把它误备份成 ComfyUI.broken-*。
    // 快照跟随实例，保存在 <root>\.comfyfleet\snapshots。
    for sub in ["ComfyUI", "logs"] {
        std::fs::create_dir_all(root.join(sub))?;
    }
    // 归属标记：删除磁盘文件前必须校验，防止误删用户目录
    config::atomic_write(
        &marker_path(&root),
        serde_json::to_string(&serde_json::json!({"id": id, "name": name}))?.as_bytes(),
    )?;

    let reference = if req_channel == "dev" { "master" } else { "" };
    let cfg = InstanceConfig {
        id: id.clone(),
        name,
        root_dir: root.to_string_lossy().to_string(),
        engine,
        engine_detail: detail,
        python_version: python_minor(&req.python_version),
        python: Default::default(),
        comfy: ComfyRef {
            channel: req_channel,
            reference: reference.to_string(),
            commit: String::new(),
            version: None,
        },
        server: ServerOpts {
            listen: "127.0.0.1".to_string(),
            port: req.port,
            cors_origin: None,
            extra_args: Vec::new(),
        },
        storage: Default::default(),
        env: Default::default(),
        overrides: Default::default(),
        node_deps_auto: None,
        auto_restart: false,
        kind: "local".to_string(),
        remote: None,
        imported: false,
        state: InstanceState::Uninitialized,
        created_at: chrono::Utc::now().to_rfc3339(),
        last_started_at: None,
    };

    save_instance(&cfg)?;
    log::info!("登记实例「{}」→ {}", cfg.name, cfg.root_dir);
    Ok(cfg)
}

/// 云端地址归一化：允许用户直接精`https://host`，但路径、查询串和端口必须分开。
/// 返回 (host, 件scheme 推断的tls)
fn normalize_remote_host(raw: &str) -> Result<(String, Option<bool>), AppError> {
    let mut text = raw.trim();
    let mut tls = None;
    let lower = text.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("https://") {
        text = &text[text.len() - rest.len()..];
        tls = Some(true);
    } else if let Some(rest) = lower.strip_prefix("http://") {
        text = &text[text.len() - rest.len()..];
        tls = Some(false);
    }

    let host = text.trim().trim_end_matches('/').trim();
    if host.is_empty() {
        return Err(AppError::new("REMOTE_HOST_EMPTY", "云端地址不能为空"));
    }
    if host.len() > 255 {
        return Err(AppError::new("REMOTE_HOST_TOO_LONG", "云端地址太长"));
    }
    if host.contains('/') || host.contains('\\') || host.contains('@') {
        return Err(AppError::new(
            "REMOTE_HOST_INVALID",
            "云端地址只填域名或IP，不要带路径或账号",
        ));
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '?' | '#' | '"' | '\'' | '<' | '>'))
    {
        return Err(AppError::new(
            "REMOTE_HOST_INVALID",
            "云端地址里不能有空格或特殊字符",
        ));
    }

    let normalized = if host.starts_with('[') && host.ends_with(']') {
        host[1..host.len() - 1].to_string()
    } else {
        host.to_string()
    };
    if normalized.is_empty() {
        return Err(AppError::new("REMOTE_HOST_EMPTY", "云端地址不能为空"));
    }
    // 单个冒号说明用户把端口写进host 了（IPv6 会有多个冒号，
    if !normalized.contains("::") && normalized.matches(':').count() == 1 {
        return Err(AppError::new(
            "REMOTE_HOST_HAS_PORT",
            "端口请填在「端口」框里，地址里只填域名或 IP",
        ));
    }
    if !normalized
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
    {
        return Err(AppError::new("REMOTE_HOST_INVALID", "云端地址格式不对"));
    }
    Ok((normalized, tls))
}

#[tauri::command]
pub fn instance_remote_save(req: RemoteInstanceReq) -> Result<InstanceConfig, AppError> {
    let (host, scheme_tls) = normalize_remote_host(&req.host)?;
    if req.port == 0 {
        return Err(AppError::new("PORT_INVALID", "端口需在 1 到 65535 之间"));
    }
    let name = if req.name.trim().is_empty() {
        host.clone()
    } else {
        req.name.trim().to_string()
    };
    validate_name(&name)?;
    let target = RemoteTarget {
        host: host.clone(),
        port: req.port,
        tls: scheme_tls.unwrap_or(req.tls),
    };

    let cfg = match req.id {
        Some(id) => {
            let mut existing = load_instance(&id)?;
            if !existing.is_remote() {
                return Err(AppError::new(
                    "NOT_REMOTE",
                    "这个实例是本机实例，不能用云端地址编辑",
                ));
            }
            existing.name = name;
            existing.server.listen = host;
            existing.server.port = req.port;
            existing.remote = Some(target);
            existing
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string().replace('-', "");
            let id = id[..8].to_string();
            InstanceConfig {
                id,
                name,
                root_dir: String::new(),
                engine: "remote".to_string(),
                engine_detail: None,
                python_version: String::new(),
                python: Default::default(),
                comfy: ComfyRef {
                    channel: "remote".to_string(),
                    reference: String::new(),
                    commit: String::new(),
                    version: None,
                },
                server: ServerOpts {
                    listen: host,
                    port: req.port,
                    cors_origin: None,
                    extra_args: Vec::new(),
                },
                storage: Default::default(),
                env: Default::default(),
                overrides: Default::default(),
                node_deps_auto: Some("off".to_string()),
                auto_restart: false,
                kind: "remote".to_string(),
                remote: Some(target),
                imported: true,
                state: InstanceState::Ready,
                created_at: chrono::Utc::now().to_rfc3339(),
                last_started_at: None,
            }
        }
    };
    save_instance(&cfg)?;
    log::info!(
        "保存云端实例「{}」：{}",
        cfg.name,
        cfg.remote.as_ref().map(|r| r.url()).unwrap_or_default()
    );
    Ok(cfg)
}

/// 导入检测结果（纯数据，便于测试，
#[derive(Debug, Clone)]
pub struct Detected {
    /// 识别到的实例根（测试与日志用，
    #[allow(dead_code)]
    pub root: PathBuf,
    /// 认出来的 Python 环境（venv、裸 python.exe、便携版 python_embeded 都算，
    pub python: Option<crate::runtime::PyEnv>,
    /// 界面上统一用「主.次。
    pub python_version: String,
    /// 解释器真实完整版本，如3.13.6
    pub python_version_full: String,
    pub engine: String,
    pub channel: String,
    pub reference: String,
    pub version: Option<String>,
    pub torch_version: String,
    pub cuda_available: bool,
    pub cuda_device: Option<String>,
}

/// 主动从实例venv 里读到的真实环境。
///
/// 和`InstanceConfig` 里的缓存值不同：每次调用都实际执行python / import torch：
/// 供「同步完成后」和「界面刷新」使用。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEnv {
    pub python_version: String,
    pub python_version_full: String,
    pub torch_version: String,
    pub cuda_available: bool,
    pub cuda_device: Option<String>,
    pub engine: String,
    pub engine_detail: Option<EngineDetail>,
    /// 是否修正交instance.json 里缓存的字段（界面据此决定要不要重新拉列表）
    pub changed: bool,
}

/// 在已有安装目录里识别出实例所需的信息。
///
/// 判定依据一律用**实测**：Python 版本看 python，引擎看 torch 的 `+cuXXX` 标记，
/// 通道看 git 是否停在 tag 上。识别不到就留空并让调用方标注"未检测到"，不猜。
#[allow(dead_code)] // 旧调用点已改成 detect_existing_with；留着给测试与外部调用
pub fn detect_existing(root: &Path, git: Option<&Path>) -> Detected {
    detect_existing_with(root, git, None)
}

/// 同 `detect_existing`，但允许用户在导入时指定 Python 环境（venv 目录 / python.exe）。
///
/// 指定了就用指定的：版本、torch、引擎全部按它实测，不再猜目录里的其它解释器。
pub fn detect_existing_with(
    root: &Path,
    git: Option<&Path>,
    python_override: Option<crate::runtime::PyEnv>,
) -> Detected {
    let python_env = python_override.or_else(|| crate::runtime::detect_python(root));
    let python = python_env.as_ref().map(|p| p.python.clone());

    let python_version_full = python
        .as_ref()
        .and_then(|p| {
            crate::proc::run(p, &["-V"], None, &Default::default())
                .ok()
                .filter(|o| o.ok())
                .map(|o| o.stdout.replace("Python ", "").trim().to_string())
        })
        .unwrap_or_default();
    let python_version = python_minor(&python_version_full);

    let probe = python.as_ref().and_then(|p| {
        crate::proc::run(
            p,
            &[
                "-c",
                "import json,torch;c=bool(torch.cuda.is_available());print(json.dumps({'v':torch.__version__,'c':c,'n':(torch.cuda.get_device_name(0) if c else None)}))",
            ],
            None,
            &Default::default(),
        )
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout)
    });
    let parsed = probe
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let torch_version = parsed
        .as_ref()
        .and_then(|v| v.get("v").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    let cuda_available = parsed
        .as_ref()
        .and_then(|v| v.get("c").and_then(|x| x.as_bool()))
        .unwrap_or(false);
    let cuda_device = parsed
        .as_ref()
        .and_then(|v| v.get("n").and_then(|x| x.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // 引擎：按 torch 的`+cuXXX` 安装标记找矩阵里最精确的一档；
    // 找不到同 torch 版本时退回该 CUDA 轨道的通用档；没有标记就是 CPU。
    let engine = engine_for_torch(&torch_version);

    // 通道：停在tag 一= stable
    let (channel, reference, version) = match git {
        Some(git) if root.join("ComfyUI").join(".git").exists() => {
            match crate::commands::version::read_version(git, &root.join("ComfyUI"), "stable") {
                Ok(v) if v.branch == "HEAD" => {
                    ("stable".to_string(), v.current.clone(), Some(v.current))
                }
                _ => ("dev".to_string(), "master".to_string(), None),
            }
        }
        _ => ("dev".to_string(), "master".to_string(), None),
    };

    Detected {
        root: root.to_path_buf(),
        python: python_env,
        python_version,
        python_version_full,
        engine,
        channel,
        reference,
        version,
        torch_version,
        cuda_available,
        cuda_device,
    }
}

/// 用torch 的真实版本反推矩阵里最合适的引擎档。
fn engine_for_torch(torch_version: &str) -> String {
    let base = torch_version
        .split('+')
        .next()
        .unwrap_or(torch_version)
        .trim();
    let tag = torch_version
        .split_once("+cu")
        .and_then(|(_, rest)| rest.split('.').next())
        .filter(|s| !s.is_empty())
        .map(|s| format!("cu{s}"));

    let Ok(matrix) = crate::matrix::load() else {
        return "cpu".to_string();
    };

    if let Some(tag) = tag.as_deref() {
        // 合CUDA 轨道里优先选torch 版本完全一致的档，界面下拉才能对上真实值
        if let Some(def) = matrix
            .engines
            .iter()
            .find(|e| e.index_tag == tag && e.torch_version == base)
        {
            return def.id.clone();
        }
        if let Some(def) = matrix.engines.iter().find(|e| e.index_tag == tag) {
            return def.id.clone();
        }
        return "cpu".to_string();
    }

    matrix
        .engines
        .iter()
        .find(|e| e.index_tag == "cpu" && e.torch_version == base)
        .or_else(|| matrix.engines.iter().find(|e| e.id == "cpu"))
        .map(|e| e.id.clone())
        .unwrap_or_else(|| "cpu".to_string())
}

/// 实际探测实例环境，并把探测结果写回配置（不落盘，调用方决定何时保存）。
pub fn refresh_instance_env(inst: &mut InstanceConfig) -> LiveEnv {
    let root = std::path::Path::new(&inst.root_dir);
    let py = crate::runtime::resolve_python(inst);
    let detected = detect_existing_with(root, None, py);
    let detail = engine_detail(&detected.engine).ok().flatten().map(|mut d| {
        if !detected.torch_version.is_empty() {
            d.torch_version = detected.torch_version.clone();
        }
        d
    });

    let before = (
        inst.python_version.clone(),
        inst.engine.clone(),
        inst.engine_detail.clone(),
    );
    if !detected.python_version.is_empty() {
        inst.python_version = detected.python_version.clone();
    }
    // torch 没探到（包坏了venv 不在）时不要生CPU 覆盖用户原来的配置
    if !detected.torch_version.is_empty() {
        inst.engine = detected.engine.clone();
        inst.engine_detail = detail.clone();
    }
    let changed = before
        != (
            inst.python_version.clone(),
            inst.engine.clone(),
            inst.engine_detail.clone(),
        );

    LiveEnv {
        python_version: detected.python_version,
        python_version_full: detected.python_version_full,
        torch_version: detected.torch_version,
        cuda_available: detected.cuda_available,
        cuda_device: detected.cuda_device,
        engine: detected.engine,
        engine_detail: detail,
        changed,
    }
}

/// 设置项/ 概览页主动重读真实环境：探测后只在有变化时落盘。
#[tauri::command]
pub async fn instance_env_refresh(instance_id: String) -> Result<LiveEnv, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _op = crate::state::OpGuard::acquire(&instance_id, "重读环境")?;
        let mut inst = load_instance(&instance_id)?;
        let live = refresh_instance_env(&mut inst);
        if live.changed {
            save_instance(&inst)?;
            log::info!(
                "实例「{}」重读环境：Python {} · torch {} · CUDA {}",
                inst.name,
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
            );
        }
        Ok(live)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

/// 导入时给识别结果配一个Python 设置，
/// 标准 `.venv` / `venv` →保持「自建venv」；便携版之类的裸解释器 →「已有环境。 相对路径。
fn python_config_for(root: &Path, py: Option<&crate::runtime::PyEnv>) -> PythonEnv {
    let Some(py) = py else {
        return PythonEnv::default();
    };
    let standard = crate::runtime::find_venv(root)
        .map(|v| v == py.root)
        .unwrap_or(false);
    if standard {
        return PythonEnv::default();
    }
    let path = py
        .python
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| py.python.to_string_lossy().to_string());
    PythonEnv {
        mode: "existing".to_string(),
        path: Some(path),
    }
}

/// 导入已有安装（M7）：只登记，不复制、不改动用户现有目录。
#[tauri::command]
pub async fn instance_import(
    app: tauri::AppHandle,
    path: String,
    name: Option<String>,
    python_path: Option<String>,
) -> Result<InstanceConfig, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        instance_import_impl(handle, path, name, python_path)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?;
    if let Ok(cfg) = &result {
        let mut detail = format!("目录：{}", cfg.root_dir);
        if !cfg.python_version.is_empty() {
            detail.push_str(&format!("\nPython {}", cfg.python_version));
        }
        crate::activity::record(
            &cfg.id,
            "import",
            "ok",
            format!("导入实例 {}", cfg.name),
            Some(detail),
        );
    }
    result
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelocateInstanceReq {
    pub id: String,
    pub root_dir: String,
}

/// 把实例登记重新指向一个已经存在的实例根。
///
/// 只改登记和配置里的路径，**不移动/复制磁盘文件**；真正的目录搬迁由用户先完成。
/// 新目录必须已有 `ComfyUI/main.py`，且不能与其它实例共用/嵌套根目录。
#[tauri::command]
pub fn instance_relocate(
    app: tauri::AppHandle,
    req: RelocateInstanceReq,
) -> Result<InstanceConfig, AppError> {
    let _registry = registry_guard();
    let _op = crate::state::OpGuard::acquire(&req.id, "重新定位实例目录")?;
    let mut cfg = load_instance(&req.id)?;
    if cfg.is_remote() {
        return Err(AppError::new(
            "REMOTE_INSTANCE",
            "云端实例没有本地目录，不能重新定位",
        ));
    }
    let app_state = app.state::<crate::state::AppState>();
    if app_state.runs.lock().contains_key(&req.id)
        || matches!(
            cfg.state,
            InstanceState::Running | InstanceState::Starting | InstanceState::Stopping
        )
    {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止实例，再重新定位目录",
        ));
    }

    let new_root = resolve_import_root(&req.root_dir)?;
    let new_text = new_root.to_string_lossy().to_string();
    let old_text = cfg.root_dir.clone();
    if normalized_root(&new_text) == normalized_root(&old_text) {
        return Ok(cfg);
    }
    // 新旧根不能互相包含：重新定位是“换到一个独立的目录”，不是把实例塞进旧目录内部。
    if roots_overlap(&normalized_root(&new_text), &normalized_root(&old_text)) {
        return Err(AppError::new(
            "ROOT_OVERLAP",
            format!(
                "新目录不能和旧目录互相包含：\n旧：{old_text}\n新：{new_text}\n请先把目录移动到独立的根目录。"
            ),
        ));
    }
    ensure_root_isolated(&new_text, Some(&cfg.id))?;

    migrate_instance_marker(&new_root)?;
    if marker_exists(&new_root) {
        match read_marker_id(&new_root) {
            Some(id) if id == cfg.id => {}
            Some(id) => {
                return Err(AppError::new(
                    "ROOT_OWNED_BY_OTHER",
                    format!("新目录已经属于另一个实例（id {id}），不能把当前实例移过去"),
                ))
            }
            None => {
                return Err(AppError::new(
                    "ROOT_OWNED_BY_OTHER",
                    "新目录已有实例标记但无法读取归属 id，拒绝接管",
                ))
            }
        }
    }
    if let Ok(Some(profile)) = read_profile(&new_root) {
        if !profile.id.trim().is_empty() && profile.id != cfg.id {
            return Err(AppError::new(
                "ROOT_OWNED_BY_OTHER",
                format!(
                    "新目录里的参数文件属于另一个实例（id {}），拒绝覆盖",
                    profile.id
                ),
            ));
        }
    }

    cfg = rebase_instance_paths(cfg, &new_root);
    cfg.state = if Path::new(&cfg.root_dir)
        .join("ComfyUI")
        .join("main.py")
        .is_file()
        && crate::runtime::resolve_python(&cfg).is_some()
    {
        InstanceState::Ready
    } else {
        InstanceState::Uninitialized
    };
    write_marker(&new_root, &cfg.id, &cfg.name, cfg.imported)?;
    save_instance(&cfg)?;
    crate::activity::record(
        &cfg.id,
        "relocate",
        "ok",
        "重新定位实例目录",
        Some(format!("{old_text}\n→ {new_text}")),
    );
    log::info!(
        "实例「{}」目录重新定位：{} → {}",
        cfg.name,
        old_text,
        cfg.root_dir
    );
    Ok(cfg)
}

/// 导入前预览：只读 `<root>/.comfyfleet/profile.json`，让界面能提前提示
/// “有参数文件 / 没有参数文件 / 文件损坏”，而不是导入完才告诉用户。
#[tauri::command]
pub fn instance_import_profile(path: String) -> Result<ImportProfilePreview, AppError> {
    let root = resolve_import_root(&path)?;
    let root_text = root.to_string_lossy().to_string();
    match read_profile(&root) {
        Ok(None) => Ok(ImportProfilePreview {
            root: root_text,
            found: false,
            valid: false,
            name: None,
            engine: None,
            python_version: None,
            saved_at: None,
            extra_args: 0,
            env_vars: 0,
            auto_restart: false,
            node_deps_auto: None,
            warnings: Vec::new(),
            error: None,
        }),
        Ok(Some(profile)) => {
            let warnings = profile_security_warnings(&profile);
            let name = profile.name.trim();
            let engine = profile.engine.trim();
            let python_version = python_minor(&profile.python_version);
            let name_ok = !name.is_empty() && validate_name(name).is_ok();
            let node_deps_auto = match profile
                .node_deps_auto
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
            {
                "off" | "remind" | "auto" => profile.node_deps_auto.clone(),
                _ => None,
            };
            Ok(ImportProfilePreview {
                root: root_text,
                found: true,
                valid: true,
                // 只报告真正会被导入采用的值；非法值导入时会回退，不能让预览“画饼”。
                name: name_ok.then(|| name.to_string()),
                engine: profile_engine_known(engine).then(|| engine.to_string()),
                python_version: profile_python_supported(engine, &python_version)
                    .then_some(python_version),
                saved_at: Some(profile.saved_at),
                extra_args: profile.server.extra_args.len(),
                env_vars: profile.env.len(),
                auto_restart: profile.auto_restart,
                node_deps_auto,
                warnings,
                error: None,
            })
        }
        Err(e) => Ok(ImportProfilePreview {
            root: root_text,
            found: true,
            valid: false,
            name: None,
            engine: None,
            python_version: None,
            saved_at: None,
            extra_args: 0,
            env_vars: 0,
            auto_restart: false,
            node_deps_auto: None,
            warnings: Vec::new(),
            error: Some(e.message),
        }),
    }
}

fn profile_id_is_free(id: &str) -> bool {
    validate_instance_id(id).is_ok()
        && !config::instances_dir()
            .join(id)
            .join("instance.json")
            .is_file()
}

/// 重新导入时优先复用旧 id：`.comfyfleet` 标记、快照和活动记录都按 id 关联。
/// 旧 id 已被别的实例占用时才生成新 id，绝不覆盖已存在的登记。
fn choose_import_id(profile: Option<&InstanceProfile>, marker_id: Option<&str>) -> String {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(id) = marker_id {
        candidates.push(id);
    }
    if let Some(profile) = profile {
        candidates.push(profile.id.as_str());
    }
    for id in candidates {
        if profile_id_is_free(id) {
            return id.to_string();
        }
    }
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    id[..8].to_string()
}

/// 把参数文件合入“按当前磁盘探测出来的默认配置”。
///
/// 原则：参数文件优先，但字段必须通过和设置页一样的校验；无效字段回退到
/// 当前环境的探测结果，不能让一个损坏/过期的文件把实例变成不可启动状态。
fn profile_engine_known(engine: &str) -> bool {
    engine == "custom" || engine_detail(engine).ok().flatten().is_some()
}

fn profile_python_supported(engine: &str, version: &str) -> bool {
    if engine == "custom" {
        return true;
    }
    let matrix = match crate::matrix::load() {
        Ok(matrix) => matrix,
        // 矩阵读不出来时不要因为“校验不了”丢掉用户的 Python 设置。
        Err(_) => return true,
    };
    matrix
        .engines
        .iter()
        .find(|e| e.id == engine)
        .map(|e| e.python_support.iter().any(|v| v == version))
        .unwrap_or(false)
}

fn loopback_listen(value: &str) -> bool {
    let value = value
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    value == "localhost"
        || value
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn safe_cors_origin(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("http://127.0.0.1")
        || value.starts_with("http://localhost")
        || value.starts_with("http://[::1]")
        || value.starts_with("https://127.0.0.1")
        || value.starts_with("https://localhost")
        || value.starts_with("https://[::1]")
}

fn dangerous_import_arg(arg: &str) -> bool {
    let name = arg.split('=').next().unwrap_or("").trim();
    // 目录类参数是用户实例设置的一部分（导入时会 rebase 到新根），
    // 但监听/端口/CORS 不能由外部 profile 决定。
    matches!(name, "--listen" | "--port" | "--enable-cors-header")
}

fn dangerous_import_env(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("PYTHON")
        || key.starts_with("PIP_")
        || key.starts_with("UV_")
        || matches!(
            key.as_str(),
            "HF_ENDPOINT"
                | "HF_HUB_ENDPOINT"
                | "PATH"
                | "PATHEXT"
                | "COMSPEC"
                | "SYSTEMROOT"
                | "WINDIR"
                | "LD_LIBRARY_PATH"
                | "DYLD_LIBRARY_PATH"
        )
}

fn path_within(root: &str, path: &str) -> bool {
    let root = root.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    let path = path.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    !root.is_empty()
        && path != root
        && (path.starts_with(&format!("{root}\\")) || path.starts_with(&format!("{root}/")))
}

fn profile_security_warnings(profile: &InstanceProfile) -> Vec<String> {
    let mut warnings = Vec::new();
    if !profile.server.listen.trim().is_empty() && !loopback_listen(&profile.server.listen) {
        warnings.push(format!(
            "原监听地址 {} 会改为 127.0.0.1（避免导入后被局域网访问）",
            profile.server.listen.trim()
        ));
    }
    if profile
        .server
        .cors_origin
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .is_some_and(|v| !safe_cors_origin(v))
    {
        warnings.push("CORS 来源不是本机回环地址，将被清空".into());
    }
    if profile
        .server
        .extra_args
        .iter()
        .any(|a| dangerous_import_arg(a))
    {
        warnings.push("包含监听/端口/CORS 启动参数，导入时会被移除".into());
    }
    if profile.env.keys().any(|k| dangerous_import_env(k)) {
        warnings.push("包含会改变 Python/依赖/模型来源的环境变量，导入时会被移除".into());
    }
    if profile.overrides.pypi.is_some()
        || profile.overrides.git.is_some()
        || profile.overrides.hf.is_some()
    {
        warnings.push("镜像/远端覆盖不会从参数文件继承，请在设置页重新填写".into());
    }
    if profile
        .python
        .path
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && !path_within(&profile.root_dir, p))
    {
        warnings.push("Python 解释器在实例目录之外，导入时会改用目录内探测结果".into());
    }
    warnings
}

fn apply_profile_to_import(
    mut cfg: InstanceConfig,
    profile: &InstanceProfile,
    explicit_name: bool,
    explicit_python: bool,
) -> InstanceConfig {
    let actual_root = cfg.root_dir.clone();
    let detected_engine = cfg.engine.clone();
    let detected_engine_detail = cfg.engine_detail.clone();
    let detected_python = cfg.python.clone();
    let detected_python_version = cfg.python_version.clone();

    if !explicit_name {
        let name = profile.name.trim();
        if !name.is_empty() && validate_name(name).is_ok() {
            cfg.name = name.to_string();
        }
    }

    let profile_engine = profile.engine.trim();
    if !profile_engine.is_empty() && profile_engine_known(profile_engine) {
        cfg.engine = profile_engine.to_string();
        // 已知引擎以矩阵里的 torch 详情为准，避免文件里的旧/伪造 engineDetail 与引擎 id 不一致。
        cfg.engine_detail = if profile_engine == "custom" {
            profile.engine_detail.clone()
        } else {
            engine_detail(profile_engine)
                .ok()
                .flatten()
                .or_else(|| profile.engine_detail.clone())
        };
    }

    // 用户在导入弹窗里显式指定了 Python 环境：它应该覆盖参数文件，否则“指一下 python.exe”
    // 会被旧 profile 静默改回去。
    if !explicit_python {
        let version = python_minor(&profile.python_version);
        if !version.trim().is_empty() {
            if profile_python_supported(&cfg.engine, &version) {
                cfg.python_version = version;
            } else {
                log::warn!(
                    "参数文件里的 Python {} 不受引擎 {} 支持，保留当前探测到的 Python {}",
                    version,
                    cfg.engine,
                    detected_python_version
                );
            }
        }

        let mode = profile.python.mode.trim().to_ascii_lowercase();
        if mode == "managed" || mode == "existing" {
            cfg.python = profile.python.clone();
            cfg.python.mode = mode;
            cfg.python.path = cfg
                .python
                .path
                .take()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            // 参数文件可能来自别人打包的目录：解释器只能指向实例根内，
            // 否则导入后启动 ComfyUI 就等于执行任意程序。
            if cfg
                .python
                .path
                .as_deref()
                .is_some_and(|p| !path_within(&actual_root, p))
            {
                log::warn!("参数文件里的 Python 路径不在实例目录内，回退到探测结果");
                cfg.python = detected_python.clone();
                cfg.python_version = detected_python_version.clone();
            }
        } else {
            log::warn!(
                "参数文件里的 Python 模式「{}」无效，回退到当前目录探测结果",
                profile.python.mode
            );
            cfg.python = detected_python.clone();
            cfg.python_version = detected_python_version.clone();
        }
    }

    let channel = profile.comfy.channel.trim();
    if channel == "stable" || channel == "dev" {
        cfg.comfy.channel = channel.to_string();
        cfg.comfy.reference = profile.comfy.reference.trim().to_string();
        if cfg.comfy.channel == "dev" && cfg.comfy.reference.is_empty() {
            cfg.comfy.reference = "master".to_string();
        }
        cfg.comfy.commit = profile.comfy.commit.trim().to_string();
        cfg.comfy.version = profile.comfy.version.clone();
    }

    let mut server = profile.server.clone();
    if let Ok((listen, port)) = validate_server(&server.listen, server.port) {
        // 导入的目录可能是下载来的：监听地址和 CORS 不能跟着 profile 走。
        server.listen = if loopback_listen(&listen) {
            listen
        } else {
            "127.0.0.1".to_string()
        };
        server.port = port;
        server.cors_origin = server
            .cors_origin
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && safe_cors_origin(s));
        server.extra_args = server
            .extra_args
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty() && !a.contains('\0') && !dangerous_import_arg(a))
            .collect();
        cfg.server = server;
    }

    cfg.storage = profile.storage.clone();
    let mut env = BTreeMap::new();
    for (key, value) in &profile.env {
        let key = key.trim();
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || key.chars().any(char::is_whitespace)
            || value.contains('\0')
            || dangerous_import_env(key)
        {
            continue;
        }
        env.insert(key.to_string(), value.clone());
    }
    cfg.env = env;
    // 镜像/远端覆盖可以改变依赖和模型来源，不从外部 profile 继承。
    cfg.overrides = InstanceOverrides {
        pypi: None,
        git: None,
        hf: None,
        hf_offline: profile.overrides.hf_offline,
    };
    cfg.node_deps_auto = match profile
        .node_deps_auto
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
    {
        "off" => Some("off".to_string()),
        "remind" => Some("remind".to_string()),
        "auto" => Some("auto".to_string()),
        "" => None,
        _ => cfg.node_deps_auto.clone(),
    };
    cfg.auto_restart = profile.auto_restart;
    if !profile.created_at.trim().is_empty() {
        cfg.created_at = profile.created_at.trim().to_string();
    }

    // 参数文件里记录的旧根是重定位/搬目录的线索：只修正位于旧根下的绝对路径。
    if !profile.root_dir.trim().is_empty() {
        rebase_instance_path_fields(&mut cfg, &profile.root_dir, &actual_root);
    }

    // 已有 Python 环境路径可能随目录移动失效；失效时回退到当前目录探测到的环境，
    // 不让一条过期路径阻止导入。自建 venv 不在这里判死，留给安装/启动流程处理。
    if cfg.python.is_existing() {
        if let Some(path) = cfg.python.path.as_deref() {
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                Path::new(&actual_root).join(path)
            };
            if crate::runtime::detect_python(&absolute).is_none() {
                log::warn!(
                    "参数文件里的 Python 环境 {} 已不可用，回退到当前目录探测结果",
                    absolute.display()
                );
                cfg.python = detected_python;
                cfg.python_version = detected_python_version.clone();
            }
        }
    }

    // 引擎和 Python 版本必须能对上：显式 Python 环境/旧 profile 都可能产生矩阵外的组合。
    // 对不上时退回当前目录探测到的引擎，再不行就退回探测到的版本。
    if !profile_python_supported(&cfg.engine, &cfg.python_version) {
        log::warn!(
            "参数文件组合的引擎 {} + Python {} 不受支持，回退到当前检测结果",
            cfg.engine,
            cfg.python_version
        );
        cfg.engine = detected_engine;
        cfg.engine_detail = detected_engine_detail;
        if !profile_python_supported(&cfg.engine, &cfg.python_version) {
            cfg.python_version = detected_python_version;
        }
    }

    cfg.state = if Path::new(&actual_root)
        .join("ComfyUI")
        .join("main.py")
        .is_file()
        && crate::runtime::resolve_python(&cfg).is_some()
    {
        InstanceState::Ready
    } else {
        InstanceState::Uninitialized
    };
    cfg
}

fn instance_import_impl(
    app: tauri::AppHandle,
    path: String,
    name: Option<String>,
    python_path: Option<String>,
) -> Result<InstanceConfig, AppError> {
    let _registry = registry_guard();
    // 用户可能选的是 ComfyUI 本体目录，也可能选的是上级根目录
    let root = resolve_import_root(&path)?;

    // 参数文件是用户目录里的数据：损坏不能阻止导入，但要明确回退到初始化。
    let profile = match read_profile(&root) {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "导入 {} 时参数文件不可用（{}），将按默认设置初始化",
                root.display(),
                e.message
            );
            None
        }
    };
    migrate_instance_marker(&root)?;

    // 同一个目录不能登记两次；嵌套/共用根也不行：删除父实例的 purge
    // 会把另一个实例的整个目录一起删掉。
    if let Ok(list) = instance_list() {
        let root_norm = normalized_root(&root.to_string_lossy());
        if let Some(existing) = list
            .iter()
            .find(|c| !c.is_remote() && roots_overlap(&root_norm, &normalized_root(&c.root_dir)))
        {
            let same = normalized_root(&existing.root_dir) == root_norm;
            return Err(AppError::new(
                if same {
                    "ALREADY_REGISTERED"
                } else {
                    "ROOT_OVERLAP"
                },
                if same {
                    format!("该目录已登记为实例「{}」", existing.name)
                } else {
                    format!(
                        "目录与已有实例「{}」的根目录重叠：{}\n两个实例不能共用/嵌套目录，否则删除时会误删对方。",
                        existing.name, existing.root_dir
                    )
                },
            ));
        }
    }

    let settings = config::load_settings().unwrap_or_default();
    let git_path = crate::git::resolve(&app, &settings)
        .ok()
        .map(|g| PathBuf::from(g.path));
    // 用户指定的Python 环境优先：填 venv 目录、python.exe、或便携版python_embeded 都认
    let python_override = match python_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(crate::runtime::detect_python(Path::new(raw)).ok_or_else(|| {
            AppError::new(
                "PY_INVALID",
                format!(
                    "{} 里没找到 python.exe。可以填 venv 目录、python.exe，或便携版的 python_embeded 目录。",
                    raw
                ),
            )
        })?),
        None => None,
    };
    let explicit_python = python_override.is_some();
    let detected = detect_existing_with(&root, git_path.as_deref(), python_override);

    let explicit_name = name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let name = explicit_name.clone().unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "导入的实例".to_string())
    });
    validate_name(&name)?;

    let detail = engine_detail(&detected.engine)?;
    let id = choose_import_id(profile.as_ref(), read_marker_id(&root).as_deref());
    let instance_dir = config::instances_dir().join(&id);
    std::fs::create_dir_all(&instance_dir)?;

    // 归属标记：导入实例永不删除磁盘内容（instance_delete 里按 imported 拦截）。
    // 标记里的旧 id 能复用时 choose_import_id 已复用；否则按本次登记重写，
    // 避免标记和实例登记永远指向两个 id。
    write_marker(&root, &id, &name, true)?;

    let state = if detected.python.is_some() && root.join("ComfyUI").join("main.py").exists() {
        InstanceState::Ready
    } else {
        InstanceState::Uninitialized
    };

    let base = InstanceConfig {
        id: id.clone(),
        name,
        root_dir: root.to_string_lossy().to_string(),
        engine: detected.engine.clone(),
        engine_detail: detail,
        python_version: detected.python_version.clone(),
        python: python_config_for(&root, detected.python.as_ref()),
        comfy: ComfyRef {
            channel: detected.channel.clone(),
            reference: detected.reference.clone(),
            commit: String::new(),
            version: detected.version.clone(),
        },
        server: ServerOpts {
            listen: "127.0.0.1".to_string(),
            // 没有参数文件时的默认值：避开官方默认 8188，也避开多数人自建的 8188。
            port: 8189,
            cors_origin: None,
            extra_args: Vec::new(),
        },
        storage: Default::default(),
        env: Default::default(),
        overrides: Default::default(),
        node_deps_auto: None,
        auto_restart: false,
        kind: "local".to_string(),
        remote: None,
        imported: true,
        state,
        created_at: chrono::Utc::now().to_rfc3339(),
        last_started_at: None,
    };
    let cfg = match profile.as_ref() {
        Some(profile) => {
            apply_profile_to_import(base, profile, explicit_name.is_some(), explicit_python)
        }
        None => base,
    };
    save_instance(&cfg)?;
    log::info!(
        "导入实例「{}」→ {}（{}；引擎 {}，torch {}，Python 环境 {}）",
        cfg.name,
        cfg.root_dir,
        if profile.is_some() {
            "已恢复参数文件"
        } else {
            "未找到参数文件，使用默认设置"
        },
        cfg.engine,
        if detected.torch_version.is_empty() {
            "未检测到"
        } else {
            &detected.torch_version
        },
        if detected.python.is_some() {
            "已找到"
        } else {
            "未找到"
        }
    );
    Ok(cfg)
}

/// 以原子写更新 `instance.json`（安装启停等状态变更都用它，
/// 节点依赖的自动模式：off / remind / auto。默认（没有这段配置， remind。
#[tauri::command]
pub fn instance_set_node_auto(id: String, mode: String) -> Result<InstanceConfig, AppError> {
    let mut cfg = load_instance(&id)?;
    let mode = mode.trim().to_lowercase();
    if !matches!(mode.as_str(), "off" | "remind" | "auto") {
        return Err(AppError::new(
            "BAD_MODE",
            format!("未知的依赖处理模式：{mode}（只认 off / remind / auto）"),
        ));
    }
    let previous = cfg
        .node_deps_auto
        .clone()
        .unwrap_or_else(|| "remind".to_string());
    cfg.node_deps_auto = Some(mode.clone());
    save_instance(&cfg)?;
    crate::activity::record(
        &cfg.id,
        "node-deps-mode",
        "ok",
        "修改节点依赖处理方式",
        Some(format!(
            "{} →{}",
            match previous.as_str() {
                "off" => "不处理",
                "auto" => "自动安装",
                _ => "提醒成",
            },
            match mode.as_str() {
                "off" => "不处理",
                "auto" => "自动安装",
                _ => "提醒成",
            }
        )),
    );
    Ok(cfg)
}

pub fn save_instance(cfg: &InstanceConfig) -> Result<(), AppError> {
    validate_instance_id(&cfg.id)?;
    let manifest = config::instances_dir().join(&cfg.id).join("instance.json");
    config::atomic_write(&manifest, serde_json::to_string_pretty(cfg)?.as_bytes())?;
    // 可移植参数文件是“跟目录走”的副本；写失败不能影响主登记。
    if let Err(e) = write_profile(cfg) {
        log::warn!("实例「{}」参数文件写入失败：{}", cfg.name, e.message);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInstanceReq {
    pub id: String,
    pub name: String,
    pub engine: String,
    pub python_version: String,
    /// 这个实例用哪个Python 环境（自建venv / 已有环境，
    #[serde(default)]
    pub python: PythonEnv,
    /// 设置页不传：只更新它自己拥有的字段，避免把启动参数页/版本页刚保存的值写回旧值。
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// 外层的 None = 不改；Some(None) = 清空；Some(Some(v)) = 设置。
    #[serde(default)]
    pub cors_origin: Option<Option<String>>,
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub overrides: InstanceOverrides,
    /// 进程非人为停止后自动重启
    #[serde(default)]
    pub auto_restart: bool,
    /// 引擎 / Python 变更会重置环境，必须显式确认
    #[serde(default)]
    pub allow_env_change: bool,
}

/// 更新实例设置。
///
/// 几条硬规则：
/// - 引擎/JSON 必须在引擎矩阵里，且该引擎支持所选Python
/// - 改Python 版本必须把旧 venv 挪走（否则`python.exe` 与配置不符，后续安装会装到错解释器里，
/// - 只改引擎 / torch 版本，*保留 venv**，只把状态标或Uninitialized；下次安装会复用 venv 并替换torch 全套：
///   不会新建环境，也不会动其它已装包
/// - 监听地址必须是合法 IP：`0.0.0.0` 合法，但调用方需要自行提示风险
#[tauri::command]
pub fn instance_update(req: UpdateInstanceReq) -> Result<InstanceConfig, AppError> {
    // 配置写命令也要和安装/切换版本/同步串行：否则会拿旧快照覆盖刚保存的字段，
    // 甚至在 uv pip install 期间把 .venv 改名。
    let _op = crate::state::OpGuard::acquire(&req.id, "保存设置")?;
    let cfg = load_instance(&req.id)?;
    if cfg.is_remote() {
        return Err(AppError::new(
            "REMOTE_INSTANCE",
            "云端实例只在首页编辑地址，不使用本机实例设置",
        ));
    }
    let id = cfg.id.clone();
    let before = cfg.clone();
    let updated = apply_update(cfg, req)?;
    if let Err(e) = save_instance(&updated) {
        if before.python_version != updated.python_version {
            restore_venv_backup(std::path::Path::new(&before.root_dir));
        }
        return Err(e);
    }
    let changes = describe_settings_change(&before, &updated);
    crate::activity::record(
        &id,
        "instance-settings",
        "ok",
        "修改实例设置",
        Some(if changes.is_empty() {
            "（没有字段变化）".to_string()
        } else {
            changes.join("\n")
        }),
    );
    log::info!("实例设置已更新：{}：{id}：", updated.name);
    Ok(updated)
}

/// 设置页保存后记一条「改了什么」：只列变化的字段，别把整份配置塞进日志
fn describe_settings_change(old: &InstanceConfig, new: &InstanceConfig) -> Vec<String> {
    let mut lines = Vec::new();
    if old.name != new.name {
        lines.push(format!("名称，{} →{}", old.name, new.name));
    }
    if old.engine != new.engine {
        lines.push(format!("引擎，{} →{}", old.engine, new.engine));
    }
    if old.python_version != new.python_version {
        lines.push(format!(
            "Python 版本，{} →{}",
            old.python_version, new.python_version
        ));
    }
    if old.python.mode != new.python.mode || old.python.path != new.python.path {
        lines.push(format!(
            "Python 环境，{}",
            if new.python.is_existing() {
                new.python
                    .path
                    .clone()
                    .unwrap_or_else(|| "（未填路径）".to_string())
            } else {
                "自建 venv".to_string()
            }
        ));
    }
    if old.server.listen != new.server.listen || old.server.port != new.server.port {
        lines.push(format!(
            "监听，{}:{} →{}:{}",
            old.server.listen, old.server.port, new.server.listen, new.server.port
        ));
    }
    if old.server.cors_origin != new.server.cors_origin {
        lines.push(format!(
            "CORS：{} →{}",
            old.server
                .cors_origin
                .clone()
                .unwrap_or_else(|| "关闭".to_string()),
            new.server
                .cors_origin
                .clone()
                .unwrap_or_else(|| "关闭".to_string())
        ));
    }
    if old.server.extra_args != new.server.extra_args {
        lines.push(format!("启动参数，{} 个", new.server.extra_args.len()));
    }
    if old.env != new.env {
        lines.push(format!("环境变量，{} 一", new.env.len()));
    }
    if old.overrides.pypi != new.overrides.pypi {
        lines.push(format!(
            "PyPI 覆盖，{}",
            new.overrides
                .pypi
                .clone()
                .unwrap_or_else(|| "跟随全局".to_string())
        ));
    }
    if old.overrides.git != new.overrides.git {
        lines.push(format!(
            "Git 覆盖，{}",
            new.overrides
                .git
                .clone()
                .unwrap_or_else(|| "跟随全局".to_string())
        ));
    }
    if old.overrides.hf != new.overrides.hf || old.overrides.hf_offline != new.overrides.hf_offline
    {
        lines.push(format!(
            "HuggingFace：{}",
            new.overrides
                .hf
                .clone()
                .unwrap_or_else(|| "跟随全局".to_string())
        ));
    }
    if old.comfy.channel != new.comfy.channel {
        lines.push(format!(
            "通道，{} →{}",
            old.comfy.channel, new.comfy.channel
        ));
    }
    if old.auto_restart != new.auto_restart {
        lines.push(format!(
            "自动重启：{}",
            if new.auto_restart { "开启" } else { "关闭" }
        ));
    }
    lines
}

/// 检测一个路径能不能当这个实例的 Python 环境（设置页「检测」按钮）。
///
/// `path` 留空 = 按实例当前配置解析（managed 查 .venv，existing 看已填路径）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonProbe {
    pub python: String,
    pub root: String,
    pub version: String,
    pub venv_style: bool,
    pub managed: bool,
}

#[tauri::command]
pub async fn instance_python_probe(
    instance_id: String,
    path: Option<String>,
) -> Result<Option<PythonProbe>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞

    tauri::async_runtime::spawn_blocking(move || instance_python_probe_impl(instance_id, path))
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn instance_python_probe_impl(
    instance_id: String,
    path: Option<String>,
) -> Result<Option<PythonProbe>, AppError> {
    let inst = load_instance(&instance_id)?;
    let root = PathBuf::from(&inst.root_dir);
    let env = match path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let p = PathBuf::from(raw);
            let p = if p.is_absolute() { p } else { root.join(p) };
            crate::runtime::detect_python(&p)
        }
        None => crate::runtime::resolve_python(&inst),
    };
    Ok(env.map(|py| {
        let version = crate::proc::run(&py.python, &["-V"], None, &Default::default())
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.replace("Python ", "").trim().to_string())
            .unwrap_or_default();
        PythonProbe {
            python: py.python.display().to_string(),
            root: py.root.display().to_string(),
            version,
            venv_style: py.venv_style,
            managed: py.managed,
        }
    }))
}

/// 纯逻辑部分：不读写磁盘（除 Python 版本变更时挪走旧 venv），便于单测
pub fn apply_update(
    mut cfg: InstanceConfig,
    req: UpdateInstanceReq,
) -> Result<InstanceConfig, AppError> {
    let name = req.name.trim().to_string();
    validate_name(&name)?;
    // 旧导入数据可能存的是 3.13.6；统一或3.13 再和矩阵/venv 比较
    let python_version = python_minor(&req.python_version);

    let matrix = crate::matrix::load()?;
    let engine = matrix.engines.iter().find(|e| e.id == req.engine).cloned();
    match &engine {
        Some(def) => {
            if !def.python_support.iter().any(|v| v == &python_version) {
                return Err(AppError::new(
                    "PYTHON_UNSUPPORTED",
                    format!(
                        "引擎「{}」不支持 Python {}（可用：{}）",
                        def.label,
                        python_version,
                        def.python_support.join(" / ")
                    ),
                ));
            }
        }
        // custom 是导入/手工环境：没有矩阵版本约束，但不能把拼错的引擎静默当 custom。
        None if req.engine == "custom" => {}
        None => {
            return Err(AppError::new(
                "ENGINE_UNKNOWN",
                format!("引擎「{}」不在引擎矩阵里", req.engine),
            ))
        }
    }

    let env_changed = req.engine != cfg.engine || python_version != cfg.python_version;
    let old_python_version = cfg.python_version.clone();
    let old_was_existing = cfg.python.is_existing();
    if env_changed && !req.allow_env_change {
        return Err(AppError::new(
            "ENV_CHANGE_NEEDS_CONFIRM",
            "修改引擎或Python 版本会重置运行环境（venv / torch），需要确认后再保存",
        ));
    }

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in &req.env {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        if key.contains('=') || key.chars().any(|c| c.is_whitespace()) {
            return Err(AppError::new(
                "ENV_KEY_INVALID",
                format!("环境变量名不合法，{key}（不能包含等号或空白，"),
            ));
        }
        env.insert(key.to_string(), value.clone());
    }

    let clean = |s: Option<String>| s.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());

    cfg.name = name;
    cfg.engine = req.engine.clone();
    cfg.engine_detail = engine_detail(&req.engine)?;
    cfg.python_version = python_version;
    // 通道决定"以后装什么，不是现在就换代码"：ComfyUI 已经落地时改这个字段，
    // 只会让 instance.json 与磁盘上的 checkout 自相矛盾。真正的切换在「版本」页
    //（checkout + 重装依赖），后端这里再拦一次。
    let req_channel = req
        .channel
        .as_deref()
        .unwrap_or(cfg.comfy.channel.as_str())
        .trim()
        .to_string();
    validate_channel(&req_channel)?;
    let comfy_installed = PathBuf::from(&cfg.root_dir)
        .join("ComfyUI")
        .join("main.py")
        .exists();
    if req_channel != cfg.comfy.channel && comfy_installed {
        return Err(AppError::new(
            "CHANNEL_NEEDS_SWITCH",
            "已装好的实例请到「版本」页切换稳定特/ 开发版（会 checkout 并重装依赖），不要在这里改通道",
        ));
    }
    // 通道切换必须同步引用，否则会出现「dev 通道 + 稳定特tag」这种自相矛盾的记录
    if cfg.comfy.channel != req_channel {
        cfg.comfy.reference = if req_channel == "dev" {
            "master".to_string()
        } else {
            String::new() // 留空：下次安装切换时再解析成最新tag
        };
    } else if req_channel == "dev" && cfg.comfy.reference.is_empty() {
        cfg.comfy.reference = "master".to_string();
    }
    cfg.comfy.channel = req_channel;
    if req.listen.is_some() || req.port.is_some() {
        let (listen, port) = validate_server(
            req.listen.as_deref().unwrap_or(&cfg.server.listen),
            req.port.unwrap_or(cfg.server.port),
        )?;
        cfg.server.listen = listen;
        cfg.server.port = port;
    }
    if let Some(cors) = req.cors_origin {
        cfg.server.cors_origin = clean(cors);
    }
    if let Some(extra_args) = req.extra_args {
        cfg.server.extra_args = extra_args
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
    }
    cfg.env = env;
    cfg.auto_restart = req.auto_restart;

    // ---- Python 环境（每个实例自己定，
    let mut py = req.python;
    py.mode = if py.mode.trim().eq_ignore_ascii_case("existing") {
        "existing".to_string()
    } else {
        "managed".to_string()
    };
    py.path = py
        .path
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if py.mode == "existing" {
        if let Some(raw) = &py.path {
            let abs = PathBuf::from(raw);
            let abs = if abs.is_absolute() {
                abs
            } else {
                PathBuf::from(&cfg.root_dir).join(abs)
            };
            if crate::runtime::detect_python(&abs).is_none() {
                return Err(AppError::new(
                    "PYTHON_NOT_FOUND",
                    format!(
                        "{} 下没找到 python.exe（可以填 venv 目录、python.exe 本身，或便携版 python_embeded）",
                        abs.display()
                    ),
                ));
            }
        }
    }
    let py_mode_changed = py.is_existing() != cfg.python.is_existing();
    cfg.python = py;
    if py_mode_changed && !cfg.python.is_existing() {
        // 从「已有环境」切回「自建venv」：没有 .venv 就得重新装
        if crate::runtime::find_venv(Path::new(&cfg.root_dir)).is_none() {
            cfg.state = InstanceState::Uninitialized;
        }
    }
    // 指向的环境可用、本体也在：直接可用，不用再走一遍安装
    if matches!(cfg.state, InstanceState::Uninitialized)
        && crate::runtime::resolve_python(&cfg).is_some()
        && Path::new(&cfg.root_dir)
            .join("ComfyUI")
            .join("main.py")
            .exists()
    {
        cfg.state = InstanceState::Ready;
    }

    cfg.overrides = InstanceOverrides {
        pypi: clean(req.overrides.pypi),
        git: clean(req.overrides.git),
        hf: clean(req.overrides.hf),
        hf_offline: req.overrides.hf_offline,
    };

    // 所有校验都通过了才动 venv：非法 env/通道/已有环境路径不能先把 .venv 挪走。
    if env_changed {
        let venv = PathBuf::from(&cfg.root_dir).join(".venv");
        if cfg.python_version != old_python_version && !old_was_existing && venv.exists() {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let backup = PathBuf::from(&cfg.root_dir).join(format!(".venv.bak-{stamp}"));
            std::fs::rename(&venv, &backup).map_err(|e| {
                AppError::new(
                    "VENV_MOVE_FAILED",
                    format!("Python 版本变更，但旧 venv 挪走失败（可能有进程占用）：{e}"),
                )
            })?;
            log::info!("Python 版本变更，旧 venv 已备份到 {}", backup.display());
        }
        // 引擎/Python 变了，旧环境即使目录还在也不能算 Ready（torch 仍是旧的）。
        cfg.state = InstanceState::Uninitialized;
    }

    Ok(cfg)
}

/// 监听地址与端口：设置页和启动参数页共用的校验
pub fn validate_server(listen: &str, port: u16) -> Result<(String, u16), AppError> {
    let listen = listen.trim();
    let listen = if listen.is_empty() {
        "127.0.0.1"
    } else {
        listen
    };
    if listen.parse::<std::net::IpAddr>().is_err() {
        return Err(AppError::new(
            "LISTEN_INVALID",
            format!("监听地址不是合法的 IP：{listen}"),
        ));
    }
    if port == 0 {
        return Err(AppError::new("PORT_INVALID", "端口必须在 1 到 65535 之间"));
    }
    Ok((listen.to_string(), port))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLaunchReq {
    pub id: String,
    pub listen: String,
    pub port: u16,
    #[serde(default)]
    pub cors_origin: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// 只改「服务与网络 / 启动参数」，不碰引擎、名称、环境变量、镜像。
///
/// 单独开一条命令是为了不让两个页面的草稿互相覆盖：设置页保存时不该把启动参数页
/// 刚写进去的值带回旧值。
#[tauri::command]
pub fn instance_launch_update(req: UpdateLaunchReq) -> Result<InstanceConfig, AppError> {
    let _op = crate::state::OpGuard::acquire(&req.id, "保存启动参数")?;
    let mut cfg = load_instance(&req.id)?;
    let before = cfg.clone();
    let (listen, port) = validate_server(&req.listen, req.port)?;
    cfg.server.listen = listen;
    cfg.server.port = port;
    cfg.server.cors_origin = req
        .cors_origin
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    cfg.server.extra_args = req
        .extra_args
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    save_instance(&cfg)?;

    let mut lines = Vec::new();
    if before.server.listen != cfg.server.listen || before.server.port != cfg.server.port {
        lines.push(format!(
            "监听，{}:{} →{}:{}",
            before.server.listen, before.server.port, cfg.server.listen, cfg.server.port
        ));
    }
    if before.server.extra_args != cfg.server.extra_args {
        let added: Vec<&String> = cfg
            .server
            .extra_args
            .iter()
            .filter(|a| !before.server.extra_args.contains(a))
            .collect();
        let removed: Vec<&String> = before
            .server
            .extra_args
            .iter()
            .filter(|a| !cfg.server.extra_args.contains(a))
            .collect();
        let mut line = format!("启动参数，{} 个", cfg.server.extra_args.len());
        if !added.is_empty() {
            line.push_str(&format!(
                "\n新增，{}",
                added
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if !removed.is_empty() {
            line.push_str(&format!(
                "\n移除，{}",
                removed
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        lines.push(line);
    }
    crate::activity::record(
        &cfg.id,
        "launch-args",
        "ok",
        "修改启动参数",
        Some(if lines.is_empty() {
            "（没有变化）".to_string()
        } else {
            lines.join("\n")
        }),
    );
    log::info!("实例启动参数已更新：{}：{}：", cfg.name, cfg.id);
    Ok(cfg)
}

#[tauri::command]
pub async fn instance_delete(
    app: tauri::AppHandle,
    id: String,
    purge: bool,
) -> Result<(), AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<crate::state::AppState>();
        instance_delete_impl(state, id, purge)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn instance_delete_impl(
    state: tauri::State<'_, crate::state::AppState>,
    id: String,
    purge: bool,
) -> Result<(), AppError> {
    // 删除和创建/导入互斥：检查归属后、真正删除前，不能让另一个实例
    // 抢先把同一个根目录登记进来。
    let _registry = registry_guard();
    validate_instance_id(&id)?;
    let _op = crate::state::OpGuard::acquire(&id, "删除实例")?;
    if state.runs.lock().contains_key(&id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止实例再删除（运行中的文件被占用，强删会留下半销毁目录）",
        ));
    }
    let dir = config::instances_dir().join(&id);
    let manifest = dir.join("instance.json");
    if !manifest.exists() {
        return Err(AppError::new("NOT_FOUND", "实例登记不存在"));
    }
    let raw = std::fs::read_to_string(&manifest)?;
    // 只移除登记时，即使 instance.json 已损坏也应该能删掉这个坏掉的登记；
    // 只有 purge 需要可靠地拿到根目录，解析失败必须中止。
    let parsed: Option<InstanceConfig> = serde_json::from_str(&raw).ok();

    if purge {
        let cfg = parsed.ok_or_else(|| {
            AppError::new(
                "MANIFEST_CORRUPT",
                "instance.json 损坏，无法确定要清除的实例目录",
            )
        })?;
        if cfg.id != id {
            return Err(AppError::new(
                "MANIFEST_ID_MISMATCH",
                format!(
                    "instance.json 的 id（{}）与要删除的 id（{id}）不一致",
                    cfg.id
                ),
            ));
        }
        if cfg.is_remote() {
            return Err(AppError::new(
                "PURGE_REMOTE",
                "云端实例没有本地文件可删，只能移除地址",
            ));
        }
        if cfg.imported {
            return Err(AppError::new(
                "PURGE_IMPORTED",
                "导入的实例不允许删除磁盘文件",
            ));
        }
        let root = PathBuf::from(&cfg.root_dir);
        migrate_instance_marker(&root)?;
        let marker = marker_path(&root);
        if !marker.exists() {
            return Err(AppError::new(
                "PURGE_REFUSED",
                "目录缺少 .comfyfleet\\.instance 标记，拒绝删除磁盘文件",
            ));
        }
        let marker_raw = std::fs::read_to_string(&marker).map_err(|e| {
            AppError::new("PURGE_REFUSED", format!("无法读取实例标记，拒绝删除：{e}"))
        })?;
        let marker_id = serde_json::from_str::<serde_json::Value>(&marker_raw)
            .ok()
            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_string));
        if marker_id.as_deref() != Some(id.as_str()) {
            return Err(AppError::new(
                "PURGE_REFUSED",
                "实例标记不属于当前实例（可能是共享根或被手工改过），拒绝删除磁盘文件",
            ));
        }
        // 双保险：即使历史数据里已经存在嵌套/同根实例，也绝不递归删掉对方的目录。
        ensure_root_isolated(&cfg.root_dir, Some(&id))?;
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .map_err(|e| AppError::new("PURGE_FAILED", e.to_string()))?;
        }
        // 旧版快照在启动器数据目录里；清掉实例目录时顺手清掉，避免孤儿。
        let _ = std::fs::remove_dir_all(crate::snapshot::legacy_root(&id));
    } else if let Some(cfg) = parsed.filter(|c| c.id == id) {
        if cfg.is_remote() {
            // 远程实例没有本地目录，快照只能留在启动器侧；移除实例时一并清理。
            let _ = std::fs::remove_dir_all(crate::snapshot::legacy_root(&id));
        } else {
            // 只移除登记、保留磁盘文件：先把旧版快照迁进实例目录，
            // 这样重新导入时时间线还在。
            let _ = crate::snapshot::dir(&cfg);
        }
    }

    std::fs::remove_dir_all(&dir)?;
    // 快照跟随实例：保留文件就保留，purge 时已随实例根目录一起删除。
    // 操作日志同样清掉：日志是「这个实例的」，实例登记都没了，留着只会变成孤儿文件
    let _ = std::fs::remove_file(crate::activity::path(&id));
    log::info!("移除实例登记，{id}：purge={purge}；");
    Ok(())
}

/// 输入/输出目录的模式收敛：非法值一律退回「自己的目录」。
///
/// 读旧文件时也走这里，坏数据不该让实例打不开。
fn sanitize_storage(storage: &mut InstanceStorage) {
    config::normalize_model_dirs(&mut storage.extra_models);
    // 关掉的共享目录按路径记：去空行、去重（忽略大小写与结尾斜杠，
    let mut kept: Vec<String> = Vec::new();
    for path in storage.disabled_shared_models.drain(..) {
        let path = path.trim().trim_end_matches(['\\', '/']).to_string();
        if path.is_empty() {
            continue;
        }
        if !kept.iter().any(|k| k.eq_ignore_ascii_case(&path)) {
            kept.push(path);
        }
    }
    storage.disabled_shared_models = kept;
    for choice in [&mut storage.input, &mut storage.output] {
        let mode = choice.mode.trim().to_ascii_lowercase();
        choice.mode = match mode.as_str() {
            "shared" => "shared".to_string(),
            "custom" => "custom".to_string(),
            _ => "own".to_string(),
        };
        let path = choice.path.as_deref().unwrap_or("").trim().to_string();
        choice.path = if choice.mode == "custom" && !path.is_empty() {
            Some(path)
        } else {
            None
        };
        // custom 但路径没了：退回自己的目录，别把空路径递给 ComfyUI
        if choice.mode == "custom" && choice.path.is_none() {
            choice.mode = "own".to_string();
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStorageReq {
    pub id: String,
    #[serde(default)]
    pub use_shared_models: bool,
    #[serde(default)]
    pub disabled_shared_models: Vec<String>,
    #[serde(default)]
    pub extra_models: Vec<ModelDir>,
    #[serde(default)]
    pub input: DirChoice,
    #[serde(default)]
    pub output: DirChoice,
}

#[tauri::command]
pub fn instance_storage_update(req: UpdateStorageReq) -> Result<InstanceConfig, AppError> {
    let _op = crate::state::OpGuard::acquire(&req.id, "保存存储设置")?;
    let mut cfg = load_instance(&req.id)?;
    let mut storage = InstanceStorage {
        use_shared_models: req.use_shared_models,
        disabled_shared_models: req.disabled_shared_models,
        extra_models: req.extra_models,
        input: req.input,
        output: req.output,
    };
    // 自定义目录必须给路径：悄悄退回「自己的目录」会让用户以为设置生效了
    for (label, choice) in [("输入", &storage.input), ("输出", &storage.output)] {
        if choice.mode.trim().eq_ignore_ascii_case("custom")
            && choice.path.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(AppError::new(
                "STORAGE_PATH_EMPTY",
                format!("选了自定义{label}目录，但路径是空的"),
            ));
        }
        if choice.mode.trim().eq_ignore_ascii_case("custom") {
            if let Some(path) = choice.path.as_deref().map(str::trim) {
                if !Path::new(path).is_absolute() {
                    return Err(AppError::new(
                        "STORAGE_PATH_RELATIVE",
                        format!("自定义{label}目录必须是绝对路径：{path}"),
                    ));
                }
            }
        }
    }
    for dir in &storage.extra_models {
        let path = dir.path.trim();
        if !path.is_empty() && !Path::new(path).is_absolute() {
            return Err(AppError::new(
                "STORAGE_PATH_RELATIVE",
                format!("附加模型目录必须是绝对路径：{path}"),
            ));
        }
    }
    sanitize_storage(&mut storage);
    // 附加目录写空行是常事（界面上一行还没填完），直接丢掉，不算错误
    cfg.storage = storage;
    save_instance(&cfg)?;
    {
        let mut lines = vec![format!(
            "共享模型，{}：{} 个附加目录）",
            if cfg.storage.use_shared_models {
                "开启"
            } else {
                "关闭"
            },
            cfg.storage.extra_models.len()
        )];
        for (label, choice) in [("输入", &cfg.storage.input), ("输出", &cfg.storage.output)] {
            let where_ = if choice.mode.trim().eq_ignore_ascii_case("custom") {
                choice
                    .path
                    .clone()
                    .unwrap_or_else(|| "（未填）".to_string())
            } else {
                "实例自己的目录".to_string()
            };
            lines.push(format!("{label}：{where_}"));
        }
        if !cfg.storage.disabled_shared_models.is_empty() {
            lines.push(format!(
                "屏蔽的共享目录：{} 一",
                cfg.storage.disabled_shared_models.len()
            ));
        }
        crate::activity::record(
            &cfg.id,
            "storage",
            "ok",
            "修改存储设置",
            Some(lines.join("\n")),
        );
    }
    // yaml 每次启动都会重写，这里删掉旧文件，免得「改了设置但没启动」时残留旧内容被误读
    let yaml = PathBuf::from(&cfg.root_dir).join(crate::storage::YAML_NAME);
    if yaml.exists() {
        let _ = std::fs::remove_file(&yaml);
    }
    log::info!("实例 {} 存储设置已更新", cfg.id);
    Ok(cfg)
}

/// 只读预览：界面拿它显示「启动时实际生效的目录」和将要写入的yaml
#[tauri::command]
pub fn instance_storage_plan(
    state: tauri::State<'_, crate::state::AppState>,
    id: String,
    draft: Option<InstanceStorage>,
) -> Result<crate::storage::StoragePlan, AppError> {
    let mut inst = load_instance(&id)?;
    // 界面传草稿就按草稿算：勾选框还没保存时，下面的「启动时实际生效」也能跟着变
    if let Some(mut storage) = draft {
        sanitize_storage(&mut storage);
        inst.storage = storage;
    }
    let settings = state.settings.lock().clone();
    Ok(crate::storage::plan(&inst, &settings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ComfyRef, ServerOpts};

    #[test]
    fn channel_is_whitelisted() {
        assert!(validate_channel("stable").is_ok());
        assert!(validate_channel("dev").is_ok());
        assert!(validate_channel("nightly").is_err());
        assert!(validate_channel("").is_err());
        assert!(validate_channel(" stable ").is_err());
    }

    #[test]
    fn nested_instance_roots_are_rejected() {
        let base = PathBuf::from(r"D:\ComfyUI\fleet");
        assert!(roots_overlap(&base, &base));
        assert!(roots_overlap(&base, &base.join("child")));
        assert!(roots_overlap(&base.join("child"), &base));
        assert!(!roots_overlap(&base, &PathBuf::from(r"D:\ComfyUI\fleet2")));
        assert!(!roots_overlap(&base, &PathBuf::from(r"D:\Other\fleet")));
    }

    #[test]
    #[ignore]
    fn real_detect_existing_install() {
        let Ok(root) = std::env::var("COMFYFLEET_IMPORT_ROOT") else {
            eprintln!("未设置COMFYFLEET_IMPORT_ROOT，跳过");
            return;
        };
        let git = crate::git::ensure().ok();
        let d = detect_existing(std::path::Path::new(&root), git.as_deref());
        println!(
            "root={}\npython_env={:?}\npython={}\ntorch={}\nengine={}\nchannel={} reference={} version={:?}",
            d.root.display(),
            d.python,
            d.python_version,
            d.torch_version,
            d.engine,
            d.channel,
            d.reference,
            d.version
        );
        assert!(!d.python_version.is_empty(), "应识别出 python 版本");
        assert!(!d.engine.is_empty());
    }

    fn sample() -> InstanceConfig {
        InstanceConfig {
            id: "test0001".to_string(),
            name: "旧名字".to_string(),
            root_dir: "C:\\definitely\\not\\exists\\comfyfleet-test".to_string(),
            engine: "cpu".to_string(),
            engine_detail: None,
            python_version: "3.12".to_string(),
            python: Default::default(),
            comfy: ComfyRef {
                channel: "stable".to_string(),
                reference: "v0.43.1".to_string(),
                commit: String::new(),
                version: Some("v0.43.1".to_string()),
            },
            server: ServerOpts {
                listen: "127.0.0.1".to_string(),
                port: 8188,
                cors_origin: None,
                extra_args: Vec::new(),
            },
            storage: Default::default(),
            env: Default::default(),
            overrides: Default::default(),
            node_deps_auto: None,
            auto_restart: false,
            kind: "local".to_string(),
            remote: None,
            imported: false,
            state: InstanceState::Ready,
            created_at: "2026-10-28T00:00:00Z".to_string(),
            last_started_at: None,
        }
    }

    fn req(engine: &str, python: &str, allow_env_change: bool) -> UpdateInstanceReq {
        UpdateInstanceReq {
            id: "test0001".to_string(),
            name: "新名字".to_string(),
            engine: engine.to_string(),
            python_version: python.to_string(),
            channel: Some("dev".to_string()),
            listen: Some("0.0.0.0".to_string()),
            port: Some(9000),
            cors_origin: Some(Some("  http://localhost:3000  ".to_string())),
            extra_args: Some(vec!["--lowvram".to_string(), "  ".to_string()]),
            env: [(
                "PYTORCH_CUDA_ALLOC_CONF".to_string(),
                "expandable_segments:True".to_string(),
            )]
            .into_iter()
            .collect(),
            python: Default::default(),
            overrides: InstanceOverrides {
                pypi: Some("https://mirror.example/simple".to_string()),
                git: Some("   ".to_string()),
                hf: None,
                hf_offline: Some(true),
            },
            auto_restart: false,
            allow_env_change,
        }
    }

    #[test]
    fn existing_python_path_must_resolve() {
        let mut r = req("cpu", "3.12", false);
        r.python = PythonEnv {
            mode: "existing".into(),
            path: Some("C:\\definitely\\missing\\python.exe".into()),
        };
        assert_eq!(
            apply_update(sample(), r).unwrap_err().code,
            "PYTHON_NOT_FOUND"
        );
    }

    #[test]
    fn managed_python_is_the_default() {
        let out = apply_update(sample(), req("cpu", "3.12", false)).expect("apply");
        assert_eq!(out.python.mode, "managed");
        assert!(out.python.path.is_none());
    }

    #[test]
    fn python_version_is_normalized_to_major_minor() {
        assert_eq!(python_minor("3.13.6"), "3.13");
        assert_eq!(python_minor(" 3.13 "), "3.13");
        assert_eq!(python_minor(""), "");
    }

    #[test]
    fn legacy_instance_marker_moves_into_comfyfleet_dir() {
        let root = std::env::temp_dir().join(format!("comfyfleet-marker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = legacy_marker_path(&root);
        std::fs::write(&old, br#"{"id":"abc","name":"old"}"#).unwrap();
        assert!(marker_exists(&root));

        migrate_instance_marker(&root).unwrap();
        let new = marker_path(&root);
        assert!(new.is_file());
        assert!(!old.exists());
        assert_eq!(
            std::fs::read_to_string(&new).unwrap(),
            r#"{"id":"abc","name":"old"}"#
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 老导入数据存的是 3.13.6；保存设置时要认成 3.13，不能报「不支持 Python 3.13.6」。
    #[test]
    fn update_accepts_full_python_version_and_stores_minor() {
        let out = apply_update(sample(), req("cpu", "3.13.6", true)).expect("apply");
        assert_eq!(out.python_version, "3.13");
    }

    /// 只换 CUDA / torch 版本时不能把 venv 挪走：下次安装复用同一个环境，只替换torch 全套。
    #[test]
    fn torch_only_change_keeps_existing_venv() {
        let root =
            std::env::temp_dir().join(format!("comfyfleet-torch-switch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".venv").join("Scripts")).unwrap();
        std::fs::write(root.join(".venv").join("marker"), "keep").unwrap();

        let mut cfg = sample();
        cfg.root_dir = root.to_string_lossy().to_string();
        cfg.engine = "nvidia-cu130".to_string();
        cfg.python_version = "3.13".to_string();
        cfg.state = InstanceState::Ready;

        let out = apply_update(cfg, req("nvidia-cu126-2.10.0", "3.13", true)).expect("apply");
        assert_eq!(out.engine, "nvidia-cu126-2.10.0");
        assert!(
            root.join(".venv").join("marker").exists(),
            "venv 不应被挪起"
        );
        let backups = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".venv.bak-"))
            .count();
        assert_eq!(backups, 0, "只换 torch 不应产生 .venv.bak");
        assert!(matches!(out.state, InstanceState::Uninitialized));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 只有 Python 版本变化才备份venv；这是用户真正会看到「新环境」的唯一情况。
    #[test]
    fn python_change_moves_old_venv_aside() {
        let root =
            std::env::temp_dir().join(format!("comfyfleet-python-switch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::write(root.join(".venv").join("marker"), "old").unwrap();

        let mut cfg = sample();
        cfg.root_dir = root.to_string_lossy().to_string();
        cfg.engine = "cpu".to_string();
        cfg.python_version = "3.12".to_string();

        let out = apply_update(cfg, req("cpu", "3.13", true)).expect("apply");
        assert_eq!(out.python_version, "3.13");
        assert!(!root.join(".venv").exists(), "旧 venv 应被挪走");
        assert!(std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".venv.bak-")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trims_and_applies_fields() {
        let mut r = req("cpu", "3.12", false);
        r.auto_restart = true;
        let out = apply_update(sample(), r).expect("should apply");
        assert!(out.auto_restart, "自动重启开关应写入实例配置");
        assert_eq!(out.name, "新名字");
        assert_eq!(out.server.listen, "0.0.0.0");
        assert_eq!(out.server.port, 9000);
        assert_eq!(
            out.server.cors_origin.as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(out.server.extra_args, vec!["--lowvram".to_string()]);
        assert_eq!(out.comfy.channel, "dev");
        assert_eq!(out.comfy.reference, "master", "切到 dev 时应补默认分支");
        assert_eq!(out.env.len(), 1);
        assert_eq!(
            out.overrides.pypi.as_deref(),
            Some("https://mirror.example/simple")
        );
        assert_eq!(out.overrides.git, None, "空白字符串应归一个None");
        assert_eq!(out.overrides.hf_offline, Some(true));
        // 引擎/Python 没变，状态不该被重置
        assert!(matches!(out.state, InstanceState::Ready));
    }

    #[test]
    fn rejects_bad_listen_and_port() {
        let mut r = req("cpu", "3.12", false);
        r.listen = Some("192.168.1".to_string());
        assert_eq!(
            apply_update(sample(), r).unwrap_err().code,
            "LISTEN_INVALID"
        );

        let mut r = req("cpu", "3.12", false);
        r.port = Some(0);
        assert_eq!(apply_update(sample(), r).unwrap_err().code, "PORT_INVALID");
    }

    #[test]
    fn rejects_bad_env_key() {
        let mut r = req("cpu", "3.12", false);
        r.env = [("BAD KEY".to_string(), "1".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            apply_update(sample(), r).unwrap_err().code,
            "ENV_KEY_INVALID"
        );
    }

    #[test]
    fn engine_change_requires_confirm_then_resets_state() {
        let r = req("nvidia-cu130", "3.12", false);
        assert_eq!(
            apply_update(sample(), r).unwrap_err().code,
            "ENV_CHANGE_NEEDS_CONFIRM"
        );

        let out = apply_update(sample(), req("nvidia-cu130", "3.12", true)).expect("should apply");
        assert_eq!(out.engine, "nvidia-cu130");
        assert!(matches!(out.state, InstanceState::Uninitialized));
    }

    #[test]
    fn rejects_python_unsupported_by_engine() {
        // cpu 支持 3.11–3.14，3.9 不在矩阵里
        let r = req("cpu", "3.9", false);
        assert_eq!(
            apply_update(sample(), r).unwrap_err().code,
            "PYTHON_UNSUPPORTED"
        );
    }

    #[test]
    fn engine_for_torch_prefers_exact_build() {
        assert_eq!(engine_for_torch("2.9.1+cu130"), "nvidia-cu130-2.9.1");
        assert_eq!(engine_for_torch("2.14.1+cu130"), "nvidia-cu130");
        assert_eq!(engine_for_torch("2.9.0+cpu"), "cpu-2.9.0");
        assert_eq!(engine_for_torch(""), "cpu");
    }

    #[test]
    fn engine_detail_follows_matrix() {
        let matrix = crate::matrix::load().expect("matrix");
        let cu130 = matrix
            .engines
            .iter()
            .find(|e| e.id == "nvidia-cu130")
            .unwrap();
        let detail = engine_detail("nvidia-cu130").unwrap().unwrap();
        assert_eq!(detail.torch_version, cu130.torch_version);
        assert_eq!(detail.cuda_tag.as_deref(), Some("cu130"));

        let cpu = engine_detail("cpu").unwrap().unwrap();
        assert_eq!(cpu.cuda_tag, None);
        assert!(engine_detail("不存在").is_err());
        assert!(engine_detail("custom").unwrap().is_none());
    }
    /// 真机上已有的 instance.json 必须能加载，且 engineDetail 会被自愈成矩阵里的值
    #[test]
    fn real_instance_files_load_and_self_heal() {
        let dir = config::instances_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return, // 没有实例目录：跳过
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().to_string();
            if !id.chars().all(|c| c.is_ascii_hexdigit()) || id.is_empty() {
                continue;
            }
            let cfg = match load_instance(&id) {
                Ok(c) => c,
                Err(e) => panic!("实例 {id} 加载失败，{}", e.message),
            };
            if let Some(fresh) = engine_detail(&cfg.engine).unwrap_or(None) {
                assert_eq!(
                    cfg.engine_detail.as_ref().map(|d| &d.torch_version),
                    Some(&fresh.torch_version),
                    "实例 {id} 的engineDetail 未与矩阵对齐"
                );
            }
            checked += 1;
        }
        println!("检查了 {checked} 个真实实例");
    }

    #[test]
    fn remote_host_accepts_domain_ip_and_strips_scheme() {
        assert_eq!(
            normalize_remote_host("https://comfy.example.com/").unwrap(),
            ("comfy.example.com".to_string(), Some(true))
        );
        assert_eq!(
            normalize_remote_host(" 203.0.113.9 ").unwrap(),
            ("203.0.113.9".to_string(), None)
        );
        assert_eq!(
            normalize_remote_host("[2001:db8::1]").unwrap(),
            ("2001:db8::1".to_string(), None)
        );
        assert!(normalize_remote_host("comfy.example.com/path").is_err());
        assert!(normalize_remote_host("comfy.example.com:8188").is_err());
        assert!(normalize_remote_host("").is_err());
    }

    #[test]
    fn remote_target_url_uses_tls_and_ipv6_brackets() {
        let t = RemoteTarget {
            host: "comfy.example.com".to_string(),
            port: 8443,
            tls: true,
        };
        assert_eq!(t.url(), "https://comfy.example.com:8443");
        let v6 = RemoteTarget {
            host: "2001:db8::1".to_string(),
            port: 8188,
            tls: false,
        };
        assert_eq!(v6.url(), "http://[2001:db8::1]:8188");
    }

    #[test]
    fn profile_roundtrips_all_instance_settings() {
        let root =
            std::env::temp_dir().join(format!("comfyfleet-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut cfg = sample();
        cfg.root_dir = root.to_string_lossy().to_string();
        cfg.name = "参数实例".to_string();
        cfg.engine = "cpu".to_string();
        cfg.python_version = "3.13".to_string();
        cfg.python = PythonEnv {
            mode: "existing".to_string(),
            path: Some("python_embeded/python.exe".to_string()),
        };
        cfg.server.listen = "0.0.0.0".to_string();
        cfg.server.port = 9001;
        cfg.server.extra_args = vec!["--lowvram".to_string(), "--preview-method auto".to_string()];
        cfg.env
            .insert("HF_HOME".to_string(), "D:\\cache\\hf".to_string());
        cfg.overrides.pypi = Some("https://mirror.example/simple".to_string());
        cfg.node_deps_auto = Some("auto".to_string());
        cfg.auto_restart = true;
        cfg.storage.extra_models.push(ModelDir {
            path: "D:\\models\\extra".to_string(),
            type_name: Some("checkpoints".to_string()),
            is_default: false,
        });
        cfg.storage.input = DirChoice {
            mode: "custom".to_string(),
            path: Some("D:\\input".to_string()),
        };

        write_profile(&cfg).unwrap();
        let profile = read_profile(&root).unwrap().expect("profile should exist");
        assert_eq!(profile.name, "参数实例");
        assert_eq!(profile.server.port, 9001);
        assert_eq!(profile.server.extra_args.len(), 2);
        assert_eq!(
            profile.env.get("HF_HOME").map(String::as_str),
            Some("D:\\cache\\hf")
        );
        assert_eq!(profile.node_deps_auto.as_deref(), Some("auto"));
        assert!(profile.auto_restart);
        assert_eq!(profile.storage.extra_models.len(), 1);
        assert_eq!(profile.storage.input.path.as_deref(), Some("D:\\input"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_broken_profile_is_reported() {
        let root =
            std::env::temp_dir().join(format!("comfyfleet-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".comfyfleet")).unwrap();
        assert!(read_profile(&root).unwrap().is_none());

        // 记事本/旧 PowerShell 可能加 UTF-8 BOM，不能把它当 JSON 语法错误。
        let valid = serde_json::to_string(&InstanceProfile::from_config(&sample())).unwrap();
        std::fs::write(profile_path(&root), format!("\u{feff}{valid}")).unwrap();
        assert!(read_profile(&root).unwrap().is_some());

        std::fs::write(profile_path(&root), b"{broken").unwrap();
        assert_eq!(read_profile(&root).unwrap_err().code, "PROFILE_CORRUPT");

        std::fs::write(profile_path(&root), vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
        assert_eq!(read_profile(&root).unwrap_err().code, "PROFILE_TOO_LARGE");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_import_restores_settings_and_rebases_paths() {
        let old_root = "D:\\old\\fleet";
        let new_root = "D:\\new\\fleet";
        let mut source = sample();
        source.root_dir = old_root.to_string();
        source.name = "参数里的名字".to_string();
        source.server.port = 9002;
        source.server.extra_args = vec![format!("--output-directory={old_root}\\out")];
        source
            .env
            .insert("HF_HOME".to_string(), format!("{old_root}\\cache"));
        source.storage.extra_models.push(ModelDir {
            path: format!("{old_root}\\models"),
            type_name: None,
            is_default: false,
        });
        source.auto_restart = true;
        source.node_deps_auto = Some("auto".to_string());
        let profile = InstanceProfile::from_config(&source);

        let mut base = sample();
        base.root_dir = new_root.to_string();
        base.name = "目录名".to_string();
        let cfg = apply_profile_to_import(base, &profile, false, false);
        assert_eq!(cfg.name, "参数里的名字");
        assert_eq!(cfg.server.port, 9002);
        assert_eq!(
            cfg.server.extra_args,
            vec![format!("--output-directory={new_root}\\out")]
        );
        assert_eq!(cfg.env.get("HF_HOME"), Some(&format!("{new_root}\\cache")));
        assert_eq!(
            cfg.storage.extra_models[0].path,
            format!("{new_root}\\models")
        );
        assert!(cfg.auto_restart);
        assert_eq!(cfg.node_deps_auto.as_deref(), Some("auto"));

        let explicit = apply_profile_to_import(
            {
                let mut b = sample();
                b.root_dir = new_root.to_string();
                b.name = "用户输入".to_string();
                b
            },
            &profile,
            true,
            false,
        );
        assert_eq!(explicit.name, "用户输入", "显式填写的名称应优先");
    }

    #[test]
    fn rebase_only_rewrites_paths_inside_old_root() {
        let old_root = "D:\\old\\fleet";
        let new_root = "D:\\new\\fleet";
        assert_eq!(
            rebase_root_path(old_root, new_root, "D:\\old\\fleet\\models"),
            "D:\\new\\fleet\\models"
        );
        assert_eq!(
            rebase_root_path(old_root, new_root, "d:\\OLD\\FLEET\\input"),
            "D:\\new\\fleet\\input"
        );
        assert_eq!(
            rebase_root_path(old_root, new_root, "D:\\old\\fleet\\models"),
            "D:\\new\\fleet\\models"
        );
        assert_eq!(
            rebase_root_path("D:/old/fleet/", new_root, "D:\\old\\fleet\\models"),
            "D:\\new\\fleet\\models",
            "旧根和值的斜杠方向不一致也应能重写"
        );
        assert_eq!(
            rebase_root_path(old_root, new_root, "D:\\other\\models"),
            "D:\\other\\models"
        );
        assert_eq!(
            rebase_root_path(old_root, new_root, "relative\\models"),
            "relative\\models"
        );
    }

    #[test]
    fn relocation_rebases_python_storage_args_and_env() {
        let old_root = "D:\\old\\fleet";
        let new_root = "D:\\new\\fleet";
        let mut cfg = sample();
        cfg.root_dir = old_root.to_string();
        cfg.python = PythonEnv {
            mode: "existing".to_string(),
            path: Some(format!("{old_root}\\python_embeded\\python.exe")),
        };
        cfg.storage.extra_models.push(ModelDir {
            path: format!("{old_root}\\models"),
            type_name: None,
            is_default: false,
        });
        cfg.storage.input = DirChoice {
            mode: "custom".to_string(),
            path: Some(format!("{old_root}\\input")),
        };
        cfg.server.extra_args = vec![
            "--lowvram".to_string(),
            format!("--output-directory={old_root}\\out"),
            format!("--temp-directory=\"{old_root}\\quoted out\""),
            "  keep surrounding spaces  ".to_string(),
        ];
        cfg.env
            .insert("HF_HOME".to_string(), format!("{old_root}\\cache"));
        cfg.env
            .insert("KEEP_SPACES".to_string(), "  not a path  ".to_string());

        let out = rebase_instance_paths(cfg, Path::new(new_root));
        assert_eq!(out.root_dir, new_root);
        assert_eq!(
            out.python.path,
            Some(format!("{new_root}\\python_embeded\\python.exe"))
        );
        assert_eq!(
            out.storage.extra_models[0].path,
            format!("{new_root}\\models")
        );
        assert_eq!(out.storage.input.path, Some(format!("{new_root}\\input")));
        assert_eq!(
            out.server.extra_args,
            vec![
                "--lowvram".to_string(),
                format!("--output-directory={new_root}\\out"),
                format!("--temp-directory=\"{new_root}\\quoted out\""),
                "  keep surrounding spaces  ".to_string(),
            ]
        );
        assert_eq!(out.env.get("HF_HOME"), Some(&format!("{new_root}\\cache")));
        assert_eq!(
            out.env.get("KEEP_SPACES"),
            Some(&"  not a path  ".to_string()),
            "非路径环境变量不能被 trim 或改写"
        );
    }

    #[test]
    fn explicit_python_override_wins_over_profile() {
        let mut profile = InstanceProfile::default();
        profile.engine = "cpu".to_string();
        profile.python_version = "9.9".to_string();
        profile.python = PythonEnv {
            mode: "existing".to_string(),
            path: Some("D:\\old\\python.exe".to_string()),
        };
        let mut base = sample();
        base.python = PythonEnv {
            mode: "existing".to_string(),
            path: Some("C:\\chosen\\python.exe".to_string()),
        };

        let cfg = apply_profile_to_import(base, &profile, false, true);
        assert_eq!(
            cfg.python.path.as_deref(),
            Some("C:\\chosen\\python.exe"),
            "用户显式填写的 Python 环境应优先于参数文件"
        );
        assert_eq!(cfg.python_version, "3.12", "显式 Python 时不应套用旧版本号");
    }

    #[test]
    fn invalid_profile_python_falls_back() {
        let mut profile = InstanceProfile::default();
        profile.engine = "cpu".to_string();
        profile.python_version = "9.9".to_string();
        profile.python = PythonEnv {
            mode: "not-a-mode".to_string(),
            path: Some("D:\\old\\python.exe".to_string()),
        };
        let cfg = apply_profile_to_import(sample(), &profile, false, false);
        assert_eq!(cfg.python.mode, "managed");
        assert_eq!(cfg.python.path, None);
        assert_eq!(
            cfg.python_version, "3.12",
            "不支持的 Python 版本应回退到探测值"
        );
    }

    #[test]
    fn profile_missing_name_and_bad_fields_fall_back() {
        let mut profile = InstanceProfile::default();
        profile.name = "bad/name".to_string();
        profile.server.port = 0;
        profile.engine = "not-an-engine".to_string();
        let cfg = apply_profile_to_import(sample(), &profile, false, false);
        assert_eq!(cfg.name, "旧名字", "非法名称应保留探测结果");
        assert_eq!(cfg.server.port, 8188, "非法端口应保留默认值");
        assert_eq!(cfg.engine, "cpu", "不存在的引擎不应写入");
    }

    #[test]
    fn old_instance_json_defaults_to_local() {
        let raw = r#"{
          "id":"test0002","name":"old","rootDir":"C:\\x","engine":"cpu",
          "engineDetail":null,"pythonVersion":"3.12",
          "comfy":{"channel":"stable","ref":"","commit":"","version":null},
          "server":{"listen":"127.0.0.1","port":8188,"corsOrigin":null,"extraArgs":[]},
          "env":{},"imported":false,"state":"ready",
          "createdAt":"2026-10-30T00:00:00Z","lastStartedAt":null
        }"#;
        let cfg: InstanceConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.kind, "local");
        assert!(cfg.remote.is_none());
        assert!(!cfg.is_remote());
    }
}
