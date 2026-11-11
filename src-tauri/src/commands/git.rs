use tauri::Manager;
use tauri::State;

use crate::error::AppError;
use crate::git::{self, GitInfo};
use crate::state::AppState;

/// 当前生效的 git 来源与版本（内置 MinGit / 自定义 / 系统 PATH）
#[tauri::command]
pub async fn git_info(app: tauri::AppHandle) -> Result<GitInfo, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        git_info_impl(handle.clone(), state)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn git_info_impl(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<GitInfo, AppError> {
    let settings = state.settings.lock().clone();
    git::resolve(&app, &settings)
}
