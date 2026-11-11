//! 环境探测 IPC。用于导入已有安装时先读一遍 Python / torch / CUDA。

use std::path::Path;

use crate::accel::{self, EnvReport};
use crate::error::AppError;
use crate::runtime;

/// 导入前探一下任意目录 / 解释器（不写任何文件）
#[tauri::command]
pub async fn accel_scan(root: String, python: Option<String>) -> Result<EnvReport, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let picked = python
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|p| runtime::detect_python(Path::new(p)))
            .unwrap_or_else(|| runtime::detect_python(Path::new(root.trim())));
        let py = picked.ok_or_else(|| {
            AppError::new(
                "PY_NOT_FOUND",
                "这个目录 / 路径里没找到 python.exe。填 venv 目录、python.exe 或便携版的 python_embeded 都行。",
            )
        })?;
        accel::inspect(&py.python)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}
