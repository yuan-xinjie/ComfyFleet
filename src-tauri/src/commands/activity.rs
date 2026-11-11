use crate::activity::ActivityEntry;
use crate::error::AppError;

/// 读实例的操作日志（最新在前）。日志文件很小（上限由全局设置控制），但保持和
/// 其它读命令一致的写法：重活丢阻塞线程，界面永远不卡。
#[tauri::command]
pub async fn activity_list(
    instance_id: String,
    limit: Option<usize>,
) -> Result<Vec<ActivityEntry>, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        crate::activity::list(&instance_id, limit.unwrap_or(crate::activity::MAX_LIST))
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}
