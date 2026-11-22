//! 把编译进 exe 的 Python 启动壳落到数据目录。
//!
//! 不让安装包额外带 resources：pybridge 很小，直接 `include_str!`，每次启动对比
//! 内容后原子写入即可。这样开发模式和安装版走的是同一份文件，也不会因为升级
//! 后旧文件残留而跑旧协议。

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::AppError;

const LAUNCH_PY: &str = include_str!("../python/launch.py");
const PROGRESS_PY: &str = include_str!("../python/comfyfleet_progress.py");

/// 返回 `launch.py` 的绝对路径。
pub fn ensure_installed() -> Result<PathBuf, AppError> {
    let dir = config::app_root().join("pybridge");
    fs::create_dir_all(&dir).map_err(|e| {
        AppError::new(
            "PYBRIDGE_DIR",
            format!("创建 Python 进度桥目录失败（{}）：{e}", dir.display()),
        )
    })?;

    write_if_changed(&dir.join("launch.py"), LAUNCH_PY.as_bytes())?;
    write_if_changed(&dir.join("comfyfleet_progress.py"), PROGRESS_PY.as_bytes())?;

    Ok(dir.join("launch.py"))
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<(), AppError> {
    if fs::read(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    config::atomic_write(path, content).map_err(|e| {
        AppError::new(
            "PYBRIDGE_WRITE",
            format!(
                "写入 Python 进度桥失败（{}）：{}",
                path.display(),
                e.message
            ),
        )
    })
}
