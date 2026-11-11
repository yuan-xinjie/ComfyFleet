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
        if meta.file_type().is_symlink() {
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
    if meta.file_type().is_symlink() {
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
            if meta.file_type().is_symlink() {
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
}
