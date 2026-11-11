//! 可选运行时（uv / MinGit + git-lfs）的查询、安装、卸载。

use tauri::ipc::Channel;

use crate::error::AppError;
use crate::toolchain;

#[tauri::command]
pub fn toolchain_status() -> toolchain::RuntimeStatus {
    toolchain::status()
}

/// 下载并安装一个运行时。`kind` 取 `uv` 或 `git`（装 git 会连带装 git-lfs）。
/// 下载是阻塞 IO，放到阻塞线程池，别占住 async 运行时。
#[tauri::command]
pub async fn toolchain_install(
    kind: String,
    on_event: Channel<toolchain::RuntimeEvent>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || toolchain::install(&kind, on_event))
        .await
        .map_err(|e| AppError::new("RUNTIME_TASK", format!("下载任务异常结束：{e}")))?
}

#[tauri::command]
pub fn toolchain_remove(kind: String) -> Result<(), AppError> {
    toolchain::remove(&kind)
}
