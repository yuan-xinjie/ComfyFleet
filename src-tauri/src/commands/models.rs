//! 模型目录资源管理器：列目录、把拖进来的文件复制进当前目录。
//!
//! 前端只传实例和「目录根 + 相对路径」。根必须是这个实例当前生效的模型目录，
//! 相对路径只能是普通路径段；复制落点仍然再做一次 canonicalize 包含检查，
//! 防止通过 junction / 相对路径把文件写到模型目录之外。

use serde::Serialize;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::commands::instance::load_instance;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::{self, StoragePlan};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListing {
    pub path: String,
    pub rel: String,
    pub exists: bool,
    pub entries: Vec<ModelEntry>,
}

/// 模型卡搜索：`rel` 是命中项所在的目录（相对模型根），点结果时按它跳转。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSearchHit {
    pub name: String,
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCopyResult {
    pub copied: u64,
    pub skipped: u64,
    pub failed: u64,
    pub bytes: u64,
    pub target: String,
    pub errors: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOpResult {
    pub done: u64,
    pub skipped: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

impl ModelOpResult {
    fn error(&mut self, message: String) {
        self.failed += 1;
        if self.errors.len() < 12 {
            self.errors.push(message);
        }
    }
}

struct CopyAcc {
    copied: u64,
    skipped: u64,
    failed: u64,
    bytes: u64,
    errors: Vec<String>,
}

impl CopyAcc {
    fn new() -> Self {
        Self {
            copied: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
            errors: Vec::new(),
        }
    }

    fn error(&mut self, message: String) {
        self.failed += 1;
        if self.errors.len() < 12 {
            self.errors.push(message);
        }
    }
}

fn norm_key(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches(&['\\', '/'][..]);
    let replaced = trimmed.replace('/', "\\");
    if cfg!(windows) {
        replaced.to_lowercase()
    } else {
        replaced
    }
}

fn effective_root(plan: &StoragePlan, requested: &str) -> Result<PathBuf, AppError> {
    if requested.trim().is_empty() {
        return Err(AppError::new("MODEL_ROOT_EMPTY", "没有选择模型目录"));
    }
    let key = norm_key(requested);
    plan.models
        .iter()
        .find(|m| norm_key(&m.path) == key)
        .map(|m| PathBuf::from(&m.path))
        .ok_or_else(|| {
            AppError::new(
                "MODEL_ROOT_INVALID",
                format!("这个模型目录不属于当前实例：{requested}"),
            )
        })
}

/// `rel` 只允许普通路径段；`..`、绝对路径、盘符都拒绝。
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            _ => {
                return Err(AppError::new(
                    "MODEL_PATH_INVALID",
                    format!("非法的模型目录路径：{rel}"),
                ))
            }
        }
    }
    Ok(out)
}

fn inside(root: &Path, path: &Path) -> Result<bool, AppError> {
    let root = fs::canonicalize(root)
        .map_err(|e| AppError::new("MODEL_ROOT_READ", format!("读不到模型根目录：{e}")))?;
    let path = fs::canonicalize(path)
        .map_err(|e| AppError::new("MODEL_PATH_READ", format!("读不到模型目录：{e}")))?;
    Ok(path.starts_with(root))
}

fn modified_ms(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn hidden(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name.eq_ignore_ascii_case("System Volume Information")
}

/// 模型目录里新建、重命名、粘贴时只接受单个文件名，不接受路径。
fn clean_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::new("MODEL_NAME_EMPTY", "名称不能为空"));
    }
    if name == "." || name == ".." {
        return Err(AppError::new("MODEL_NAME_INVALID", "名称不能是 . 或 .."));
    }
    if name.starts_with('.') {
        return Err(AppError::new(
            "MODEL_NAME_INVALID",
            "名称不能以点开头；模型目录不显示隐藏项",
        ));
    }
    if name.chars().count() > 255 {
        return Err(AppError::new("MODEL_NAME_LONG", "名称太长"));
    }
    if name.chars().any(|c| {
        c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err(AppError::new(
            "MODEL_NAME_INVALID",
            format!("名称里有不允许的字符：{name}"),
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(AppError::new(
            "MODEL_NAME_INVALID",
            "名称不能以点或空格结尾",
        ));
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return Err(AppError::new(
            "MODEL_NAME_RESERVED",
            format!("这是系统保留名称：{name}"),
        ));
    }
    Ok(name.to_string())
}

fn resolve_dir(plan: &StoragePlan, root: &str, rel: &str) -> Result<PathBuf, AppError> {
    let root_path = effective_root(plan, root)?;
    let target = safe_join(&root_path, rel)?;
    if !target.exists() {
        return Err(AppError::new(
            "MODEL_DIR_MISSING",
            format!("模型目录不存在：{}", target.display()),
        ));
    }
    if !target.is_dir() {
        return Err(AppError::new(
            "MODEL_NOT_DIR",
            format!("不是模型目录：{}", target.display()),
        ));
    }
    if !inside(&root_path, &target)? {
        return Err(AppError::new("MODEL_PATH_ESCAPE", "模型目录越界"));
    }
    fs::canonicalize(&target)
        .map_err(|e| AppError::new("MODEL_DIR_READ", format!("读不到模型目录：{e}")))
}

fn is_reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

fn is_link_like(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse(meta)
}

fn remove_node(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}：{e}", path.display()))?;
    if is_link_like(&meta) {
        // 链接只删链接本身，不能递归进到它指向的目录。
        if meta.is_dir() {
            return fs::remove_dir(path)
                .map_err(|e| format!("删除链接失败 {}：{e}", path.display()));
        }
        return fs::remove_file(path).map_err(|e| format!("删除链接失败 {}：{e}", path.display()));
    }
    if meta.is_dir() {
        let read = fs::read_dir(path).map_err(|e| format!("读目录失败 {}：{e}", path.display()))?;
        for child in read {
            let child = child.map_err(|e| format!("读目录失败 {}：{e}", path.display()))?;
            remove_node(&child.path())?;
        }
        fs::remove_dir(path).map_err(|e| format!("删除目录失败 {}：{e}", path.display()))
    } else if meta.is_file() {
        fs::remove_file(path).map_err(|e| format!("删除文件失败 {}：{e}", path.display()))
    } else {
        Err(format!("不是普通文件或目录：{}", path.display()))
    }
}

fn copy_node(src: &Path, dest: &Path) -> Result<u64, String> {
    let meta = fs::symlink_metadata(src).map_err(|e| format!("{}：{e}", src.display()))?;
    if is_link_like(&meta) {
        // 内部粘贴不跟随链接/junction，避免把模型目录之外的内容带进来。
        return Ok(0);
    }
    if meta.is_dir() {
        fs::create_dir(dest).map_err(|e| format!("建目录失败 {}：{e}", dest.display()))?;
        let read = fs::read_dir(src).map_err(|e| format!("读目录失败 {}：{e}", src.display()))?;
        let mut bytes = 0u64;
        for child in read {
            let child = child.map_err(|e| format!("读目录失败 {}：{e}", src.display()))?;
            let child_dest = dest.join(child.file_name());
            bytes = bytes.saturating_add(copy_node(&child.path(), &child_dest)?);
        }
        Ok(bytes)
    } else if meta.is_file() {
        fs::copy(src, dest).map_err(|e| format!("复制失败 {}：{e}", src.display()))
    } else {
        Err(format!("不是普通文件或目录：{}", src.display()))
    }
}

fn move_node(src: &Path, dest: &Path) -> Result<(), String> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            // 跨盘时 rename 会失败，退回复制 + 删除源；只有复制完整成功才删源。
            copy_node(src, dest)?;
            remove_node(src).map_err(|e| format!("复制完成，但源项删除失败：{e}"))
        }
    }
}

/// 在模型根目录下递归找名字：最多 200 条命中、最多访问 2 万个条目；
/// 跳过隐藏项、符号链接/junction，避免搜索把整个磁盘拖进来。
fn search_model_files(root: &Path, needle: &str) -> Vec<ModelSearchHit> {
    let mut hits = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, rel, depth)) = stack.pop() {
        if hits.len() >= 200 || visited >= 20_000 {
            break;
        }
        let read = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for item in read {
            if hits.len() >= 200 || visited >= 20_000 {
                break;
            }
            let item = match item {
                Ok(v) => v,
                Err(_) => continue,
            };
            visited += 1;
            let name = item.file_name().to_string_lossy().to_string();
            if hidden(&name) {
                continue;
            }
            let meta = match fs::symlink_metadata(item.path()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if is_link_like(&meta) {
                continue;
            }
            let is_dir = meta.is_dir();
            if !is_dir && !meta.is_file() {
                continue;
            }
            if name.to_lowercase().contains(needle) {
                hits.push(ModelSearchHit {
                    name: name.clone(),
                    rel: rel.clone(),
                    is_dir,
                    size: if is_dir { 0 } else { meta.len() },
                });
            }
            if is_dir && depth < 7 {
                let child_rel = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                stack.push((item.path(), child_rel, depth + 1));
            }
        }
    }
    hits.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.rel.to_lowercase().cmp(&b.rel.to_lowercase()))
    });
    hits
}

#[tauri::command]
pub async fn models_search(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    query: String,
) -> Result<Vec<ModelSearchHit>, AppError> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let root_path = effective_root(&plan, &root)?;
    if !root_path.exists() || !root_path.is_dir() {
        return Ok(Vec::new());
    }
    let hits = tauri::async_runtime::spawn_blocking(move || search_model_files(&root_path, &needle))
        .await
        .map_err(|e| AppError::new("MODEL_SEARCH_JOIN", format!("搜索任务中断：{e}")))?;
    Ok(hits)
}

#[tauri::command]
pub fn models_list(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    rel: String,
) -> Result<ModelListing, AppError> {
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let root_path = effective_root(&plan, &root)?;
    let target = safe_join(&root_path, &rel)?;

    if !target.exists() {
        return Ok(ModelListing {
            path: target.to_string_lossy().to_string(),
            rel,
            exists: false,
            entries: Vec::new(),
        });
    }
    if !target.is_dir() {
        return Err(AppError::new(
            "MODEL_NOT_DIR",
            format!("不是模型目录：{}", target.display()),
        ));
    }
    if !inside(&root_path, &target)? {
        return Err(AppError::new("MODEL_PATH_ESCAPE", "模型目录越界"));
    }

    let read = fs::read_dir(&target)
        .map_err(|e| AppError::new("MODEL_DIR_READ", format!("读不到模型目录：{e}")))?;
    let mut entries: Vec<ModelEntry> = Vec::new();
    for item in read {
        let item = match item {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = item.file_name().to_string_lossy().to_string();
        if hidden(&name) {
            continue;
        }
        let meta = match fs::symlink_metadata(item.path()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if is_link_like(&meta) {
            continue;
        }
        let is_dir = meta.is_dir();
        if !is_dir && !meta.is_file() {
            continue;
        }
        entries.push(ModelEntry {
            name,
            is_dir,
            size: if is_dir { 0 } else { meta.len() },
            modified: modified_ms(&meta),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(ModelListing {
        path: target.to_string_lossy().to_string(),
        rel,
        exists: true,
        entries,
    })
}

fn copy_entry(src: &Path, dest_dir: &Path, acc: &mut CopyAcc) {
    let Some(name) = src.file_name() else {
        acc.error(format!("无法读取文件名：{}", src.display()));
        return;
    };
    let dest = dest_dir.join(name);
    let meta = match fs::symlink_metadata(src) {
        Ok(v) => v,
        Err(e) => {
            acc.error(format!("{}：{e}", src.display()));
            return;
        }
    };
    if is_link_like(&meta) {
        acc.skipped += 1;
        return;
    }
    if meta.is_dir() {
        if dest.exists() {
            if !dest.is_dir() {
                acc.error(format!("同名文件已存在：{}", dest.display()));
                return;
            }
        } else if let Err(e) = fs::create_dir_all(&dest) {
            acc.error(format!("建目录失败 {}：{e}", dest.display()));
            return;
        }
        let read = match fs::read_dir(src) {
            Ok(v) => v,
            Err(e) => {
                acc.error(format!("读目录失败 {}：{e}", src.display()));
                return;
            }
        };
        for child in read.flatten() {
            copy_entry(&child.path(), &dest, acc);
        }
        return;
    }
    if !meta.is_file() {
        acc.skipped += 1;
        return;
    }
    if dest.exists() {
        acc.skipped += 1;
        return;
    }
    match fs::copy(src, &dest) {
        Ok(bytes) => {
            acc.copied += 1;
            acc.bytes = acc.bytes.saturating_add(bytes);
        }
        Err(e) => acc.error(format!("复制失败 {}：{e}", src.display())),
    }
}

#[tauri::command]
pub async fn models_copy(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    rel: String,
    sources: Vec<String>,
) -> Result<ModelCopyResult, AppError> {
    if sources.is_empty() {
        return Ok(ModelCopyResult {
            target: String::new(),
            ..Default::default()
        });
    }
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let root_path = effective_root(&plan, &root)?;
    let target = safe_join(&root_path, &rel)?;

    fs::create_dir_all(&target)
        .map_err(|e| AppError::new("MODEL_DIR_CREATE", format!("建目录失败：{e}")))?;
    if !inside(&root_path, &target)? {
        return Err(AppError::new("MODEL_PATH_ESCAPE", "模型目录越界"));
    }
    let target_label = target.to_string_lossy().to_string();
    let target_canon = fs::canonicalize(&target)
        .map_err(|e| AppError::new("MODEL_DIR_READ", format!("读不到目标目录：{e}")))?;

    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut acc = CopyAcc::new();
        for raw in sources {
            let src = PathBuf::from(&raw);
            let meta = match fs::symlink_metadata(&src) {
                Ok(v) => v,
                Err(e) => {
                    acc.error(format!("{raw}：{e}"));
                    continue;
                }
            };
            if is_link_like(&meta) {
                acc.skipped += 1;
                continue;
            }
            let src_canon = match fs::canonicalize(&src) {
                Ok(v) => v,
                Err(e) => {
                    acc.error(format!("{raw}：{e}"));
                    continue;
                }
            };
            // 拖进来的目录如果就是目标目录/目标目录的上级，递归会把自己拷进自己。
            if target_canon.starts_with(&src_canon) {
                acc.error(format!("不能把目录复制到它自己里面：{}", src.display()));
                continue;
            }
            copy_entry(&src, &target_canon, &mut acc);
        }
        ModelCopyResult {
            copied: acc.copied,
            skipped: acc.skipped,
            failed: acc.failed,
            bytes: acc.bytes,
            target: target_label,
            errors: acc.errors,
        }
    })
    .await
    .map_err(|e| AppError::new("MODEL_COPY_JOIN", format!("复制任务中断：{e}")))?;

    Ok(result)
}

#[tauri::command]
pub fn models_rename(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    rel: String,
    name: String,
    new_name: String,
) -> Result<ModelOpResult, AppError> {
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let dir = resolve_dir(&plan, &root, &rel)?;
    let old_name = clean_name(&name)?;
    let next_name = clean_name(&new_name)?;
    if old_name == next_name {
        return Ok(ModelOpResult {
            done: 1,
            ..Default::default()
        });
    }
    let src = dir.join(&old_name);
    let meta = fs::symlink_metadata(&src)
        .map_err(|e| AppError::new("MODEL_ENTRY_READ", format!("读不到模型文件：{e}")))?;
    if is_link_like(&meta) {
        return Err(AppError::new("MODEL_ENTRY_SYMLINK", "不处理链接"));
    }
    if !meta.is_dir() && !meta.is_file() {
        return Err(AppError::new("MODEL_ENTRY_TYPE", "不是普通文件或目录"));
    }
    let dest = dir.join(&next_name);
    if dest.exists() {
        return Err(AppError::new(
            "MODEL_NAME_EXISTS",
            format!("同名文件或文件夹已存在：{next_name}"),
        ));
    }
    fs::rename(&src, &dest)
        .map_err(|e| AppError::new("MODEL_RENAME", format!("重命名失败：{e}")))?;
    Ok(ModelOpResult {
        done: 1,
        ..Default::default()
    })
}

#[tauri::command]
pub fn models_mkdir(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    rel: String,
    name: String,
) -> Result<ModelOpResult, AppError> {
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let dir = resolve_dir(&plan, &root, &rel)?;
    let name = clean_name(&name)?;
    let dest = dir.join(&name);
    if dest.exists() {
        return Err(AppError::new(
            "MODEL_NAME_EXISTS",
            format!("同名文件或文件夹已存在：{name}"),
        ));
    }
    fs::create_dir(&dest)
        .map_err(|e| AppError::new("MODEL_MKDIR", format!("新建文件夹失败：{e}")))?;
    Ok(ModelOpResult {
        done: 1,
        ..Default::default()
    })
}

#[tauri::command]
pub async fn models_delete(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    root: String,
    rel: String,
    names: Vec<String>,
) -> Result<ModelOpResult, AppError> {
    if names.is_empty() {
        return Ok(ModelOpResult::default());
    }
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let dir = resolve_dir(&plan, &root, &rel)?;
    let names = names
        .iter()
        .map(|name| clean_name(name))
        .collect::<Result<Vec<_>, _>>()?;

    tauri::async_runtime::spawn_blocking(move || {
        let mut result = ModelOpResult::default();
        for name in names {
            let path = dir.join(&name);
            match remove_node(&path) {
                Ok(()) => result.done += 1,
                Err(e) => result.error(e),
            }
        }
        result
    })
    .await
    .map_err(|e| AppError::new("MODEL_DELETE_JOIN", format!("删除任务中断：{e}")))
}

#[tauri::command]
pub async fn models_paste(
    state: tauri::State<'_, AppState>,
    instance_id: String,
    source_root: String,
    source_rel: String,
    names: Vec<String>,
    target_root: String,
    target_rel: String,
    cut: bool,
) -> Result<ModelOpResult, AppError> {
    if names.is_empty() {
        return Ok(ModelOpResult::default());
    }
    let inst = load_instance(&instance_id)?;
    let plan = storage::plan(&inst, &state.settings.lock().clone());
    let source_dir = resolve_dir(&plan, &source_root, &source_rel)?;
    let target_dir = resolve_dir(&plan, &target_root, &target_rel)?;
    let names = names
        .iter()
        .map(|name| clean_name(name))
        .collect::<Result<Vec<_>, _>>()?;

    tauri::async_runtime::spawn_blocking(move || {
        let mut result = ModelOpResult::default();
        for name in names {
            let src = source_dir.join(&name);
            let meta = match fs::symlink_metadata(&src) {
                Ok(v) => v,
                Err(e) => {
                    result.error(format!("{}：{e}", src.display()));
                    continue;
                }
            };
            if is_link_like(&meta) {
                result.skipped += 1;
                continue;
            }
            if !meta.is_dir() && !meta.is_file() {
                result.skipped += 1;
                continue;
            }
            let src_canon = match fs::canonicalize(&src) {
                Ok(v) => v,
                Err(e) => {
                    result.error(format!("{}：{e}", src.display()));
                    continue;
                }
            };
            if !src_canon.starts_with(&source_dir) {
                result.error(format!("不能操作模型目录之外的链接：{}", src.display()));
                continue;
            }
            if target_dir.starts_with(&src_canon) {
                result.error(format!("不能粘贴到自身里面：{}", src.display()));
                continue;
            }
            let dest = target_dir.join(&name);
            if dest.exists() {
                result.skipped += 1;
                continue;
            }
            let outcome = if cut {
                move_node(&src, &dest)
            } else {
                copy_node(&src, &dest).map(|_| ())
            };
            match outcome {
                Ok(()) => result.done += 1,
                Err(e) => {
                    let _ = remove_node(&dest);
                    result.error(e);
                }
            }
        }
        result
    })
    .await
    .map_err(|e| AppError::new("MODEL_PASTE_JOIN", format!("粘贴任务中断：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths_stay_relative() {
        let root = Path::new(r"D:\models");
        assert!(safe_join(root, "").is_ok());
        assert!(safe_join(root, "checkpoints").is_ok());
        assert!(safe_join(root, "checkpoints/foo").is_ok());
        assert!(safe_join(root, "..").is_err());
        assert!(safe_join(root, "checkpoints/../../x").is_err());
        assert!(safe_join(root, r"C:\Windows").is_err());
        assert!(safe_join(root, r"\absolute").is_err());
    }

    #[test]
    fn root_keys_ignore_case_and_slashes() {
        assert_eq!(norm_key(r"D:\Models\"), norm_key("d:/models"));
    }

    #[test]
    fn model_names_reject_paths_and_reserved_names() {
        assert!(clean_name("model.safetensors").is_ok());
        assert!(clean_name("子目录").is_ok());
        assert!(clean_name("..").is_err());
        assert!(clean_name("a/b").is_err());
        assert!(clean_name(r"a\b").is_err());
        assert!(clean_name(".hidden").is_err());
        assert!(clean_name("CON").is_err());
        assert!(clean_name("name.").is_err());
    }

    #[test]
    fn copy_move_and_remove_tree() {
        let stamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "comfyfleet-model-test-{}-{stamp}",
            std::process::id()
        ));
        let src = base.join("src");
        let moved = base.join("moved");
        let copied = base.join("copied");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested").join("weight.bin"), b"abc").unwrap();

        assert_eq!(copy_node(&src, &copied).unwrap(), 3);
        assert!(copied.join("nested").join("weight.bin").is_file());
        assert!(src.is_dir());

        move_node(&src, &moved).unwrap();
        assert!(!src.exists());
        assert!(moved.join("nested").join("weight.bin").is_file());

        remove_node(&moved).unwrap();
        remove_node(&copied).unwrap();
        assert!(!moved.exists());
        let _ = fs::remove_dir_all(&base);
    }
}
