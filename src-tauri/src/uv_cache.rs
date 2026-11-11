use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use walkdir::WalkDir;

use crate::error::AppError;

/// 同一时间只允许一个清理动作：手动点击和后台自动清理可能撞在一起。
static CLEARING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UvCacheInfo {
    pub path: String,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UvCacheClear {
    pub freed_bytes: u64,
    pub total_bytes: u64,
}

pub fn dir() -> PathBuf {
    crate::config::shared_dir().join("uv-cache")
}

fn tree_bytes(path: &Path) -> u64 {
    if !path.is_dir() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

pub fn info() -> UvCacheInfo {
    // 手动清理和“保存设置后自动清理”都可能与界面刷新同时发生；
    // 等清理结束再统计，避免把删到一半的大小显示给用户。
    for _ in 0..200 {
        if !CLEARING.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let path = dir();
    UvCacheInfo {
        path: path.to_string_lossy().to_string(),
        total_bytes: tree_bytes(&path),
    }
}

/// Windows 的 junction 不是普通目录；直接 `remove_dir_all` 会顺着链接删到别处。
/// UV 缓存是纯缓存，遇到链接就拒绝自动/手动清理，让用户自己处理。
#[cfg(windows)]
fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_reparse_point(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn clear_at(path: &Path) -> io::Result<UvCacheClear> {
    let before = tree_bytes(path);
    if path.exists() {
        if is_reparse_point(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "UV 缓存目录是链接 / junction，拒绝删除，避免误删链接目标",
            ));
        }
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    let total = tree_bytes(path);
    Ok(UvCacheClear {
        freed_bytes: before.saturating_sub(total),
        total_bytes: total,
    })
}

struct ClearGuard;

impl Drop for ClearGuard {
    fn drop(&mut self) {
        CLEARING.store(false, Ordering::SeqCst);
    }
}

fn begin_clear() -> Option<ClearGuard> {
    if CLEARING.swap(true, Ordering::SeqCst) {
        None
    } else {
        Some(ClearGuard)
    }
}

/// 手动清理：整个 UV 缓存清空，保留缓存根目录。
pub fn clear() -> Result<UvCacheClear, AppError> {
    let _guard = begin_clear()
        .ok_or_else(|| AppError::new("UV_CACHE_BUSY", "UV 缓存正在清理，请稍后再试"))?;
    let path = dir();
    clear_at(&path).map_err(|e| {
        AppError::new(
            "UV_CACHE_CLEAR_FAILED",
            format!("清理 UV 缓存失败：{e}。如果有安装 / 同步正在跑，等它结束再试。"),
        )
    })
}

fn prune_if_over(path: &Path, limit_bytes: u64) -> io::Result<Option<UvCacheClear>> {
    if tree_bytes(path) <= limit_bytes {
        return Ok(None);
    }
    clear_at(path).map(Some)
}

/// 启动和后台维护用：超过上限就清空。0 = 关闭。
/// 这里不返回错误给界面，失败只写日志，不能因为缓存清理影响主流程。
pub fn auto_prune(limit_gb: u32) -> Option<UvCacheClear> {
    if limit_gb == 0 {
        return None;
    }
    let _guard = begin_clear()?;
    let limit_bytes = u64::from(limit_gb).saturating_mul(1024 * 1024 * 1024);
    let path = dir();
    match prune_if_over(&path, limit_bytes) {
        Ok(Some(report)) => {
            log::info!(
                "UV 缓存超过 {} GB，已自动清理，释放 {}",
                limit_gb,
                crate::sync::human_bytes(report.freed_bytes)
            );
            Some(report)
        }
        Ok(None) => None,
        Err(e) => {
            log::warn!("UV 缓存自动清理失败（{}）：{e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "comfyfleet-uv-cache-{tag}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn measures_and_clears_cache_directory() {
        let root = temp_dir("clear");
        let archive = root.join("archive-v0").join("aa");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("wheel"), vec![7u8; 4096]).unwrap();

        assert_eq!(tree_bytes(&root), 4096);
        let report = clear_at(&root).unwrap();
        assert_eq!(report.freed_bytes, 4096);
        assert_eq!(report.total_bytes, 0);
        assert!(root.is_dir(), "清理后应保留 UV 缓存根目录");
        assert_eq!(tree_bytes(&root), 0);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn automatic_prune_only_runs_over_the_limit() {
        let root = temp_dir("limit");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("payload.bin"), vec![9u8; 2048]).unwrap();

        assert!(
            prune_if_over(&root, 4096).unwrap().is_none(),
            "低于阈值时不能清理"
        );
        let report = prune_if_over(&root, 1024).unwrap().expect("超过阈值应清理");
        assert_eq!(report.freed_bytes, 2048);
        assert_eq!(report.total_bytes, 0);

        fs::remove_dir_all(&root).ok();
    }
}
