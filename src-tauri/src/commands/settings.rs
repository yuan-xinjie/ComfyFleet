use tauri::State;

use crate::config::{self, GlobalSettings};
use crate::error::AppError;
use crate::state::AppState;
use crate::sync;
use crate::uv_cache;
use crate::wheel_repo;
use crate::wheelhouse;

#[tauri::command]
pub fn settings_get(state: State<'_, AppState>) -> Result<GlobalSettings, AppError> {
    Ok(state.settings.lock().clone())
}

#[tauri::command]
pub fn settings_save(
    state: State<'_, AppState>,
    mut settings: GlobalSettings,
) -> Result<GlobalSettings, AppError> {
    settings.normalize();
    let previous = state.settings.lock().clone();
    // 改了本地 wheel 库目录：先把旧目录里的归档真正搬到新目录。快照/同步
    // 只看当前库路径，所以文件必须跟着走，不能只换设置里的字符串。
    let mut migration: Option<wheelhouse::Migration> = None;
    if let (Some(from), Some(to)) = (
        wheelhouse::configured_dir(&previous),
        wheelhouse::configured_dir(&settings),
    ) {
        if !config::same_path(&from.to_string_lossy(), &to.to_string_lossy()) {
            migration = Some(wheelhouse::migrate_library(&from, &to)?);
        }
    }
    // 和 settings_set_theme 使用同一把锁覆盖整个“落盘 + 更新内存”过程，
    // 避免两个命令交替写入时把对方的修改覆盖掉。
    {
        let mut guard = state.settings.lock();
        if let Err(e) = config::save_settings(&settings) {
            // 文件已经搬过去了但设置没写成功：把文件搬回旧目录，避免旧配置
            // 指向一个已经被搬空的库。
            if let Some(report) = &migration {
                if let Err(rb) = report.rollback() {
                    return Err(AppError::new(
                        "WHEEL_MIGRATE",
                        format!(
                            "设置写盘失败：{}；wheel 文件回滚也失败：{}",
                            e.message, rb.message
                        ),
                    ));
                }
            }
            return Err(e);
        }
        *guard = settings.clone();
    }
    if let Some(report) = &migration {
        log::info!(
            "本地 wheel 库已迁移：移动 {} 个文件，跳过 {} 个已存在的相同文件",
            report.moved(),
            report.skipped()
        );
    }
    // 调小保留次数后立即生效；调大不会删除任何东西。
    let pruned = sync::prune_sync_cache(settings.sync.sync_cache_keep);
    if pruned.removed > 0 {
        log::info!(
            "同步缓存保留 {} 次，已清理 {} 组，释放 {}",
            settings.sync.sync_cache_keep,
            pruned.removed,
            sync::human_bytes(pruned.freed_bytes)
        );
    }
    // 调低 UV 缓存上限后不必等下一次后台轮询；长操作正在进行时由后台稍后处理。
    let uv_limit = settings.storage.uv_cache_max_gb;
    if uv_limit > 0 && !state.uv_work_active() {
        std::thread::spawn(move || {
            uv_cache::auto_prune(uv_limit);
        });
    }
    log::info!("设置已保存");
    // 回传归一化后的结果：界面据此刷新基线，空目录之类的修正不会让「未保存」标记一直亮着
    Ok(settings)
}

/// 顶栏的明暗切换只改当前主题，不动另一套配色；写盘后应用重启仍保留。
#[tauri::command]
pub fn settings_set_theme(
    state: State<'_, AppState>,
    theme: String,
) -> Result<GlobalSettings, AppError> {
    // 读-改-写必须和 settings_save 串行：否则顶栏切换主题时，设置页刚好
    // 在保存一份旧 theme 的快照，会把刚切的主题覆盖回去。
    let mut guard = state.settings.lock();
    let mut settings = guard.clone();
    let next = if theme.trim().eq_ignore_ascii_case("light") {
        "light"
    } else {
        "dark"
    };
    settings.ui.theme = next.to_string();
    settings.normalize();
    config::save_settings(&settings)?;
    *guard = settings.clone();
    drop(guard);
    Ok(settings)
}

#[tauri::command]
pub fn sync_cache_info() -> sync::SyncCacheInfo {
    sync::sync_cache_info()
}

#[tauri::command]
pub async fn wheels_info(state: State<'_, AppState>) -> Result<wheelhouse::LibraryInfo, AppError> {
    // 目录可能很大（用户把整个下载目录指过来），统计放后台线程，别卡界面。
    let settings = state.settings.lock().clone();
    tauri::async_runtime::spawn_blocking(move || wheelhouse::info(&settings))
        .await
        .map_err(|e| AppError::new("WHEELS_JOIN", format!("统计 wheel 库失败：{e}")))
}

/// 云端 wheel 仓库：默认读本地缓存；`refresh=true` 才重新访问 ModelScope。
#[tauri::command]
pub async fn wheel_repo_list(
    state: State<'_, AppState>,
    instance_id: String,
    refresh: bool,
) -> Result<wheel_repo::WheelRepoList, AppError> {
    let repo = state.settings.lock().wheel_repo.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let inst = crate::commands::instance::load_instance(&instance_id)?;
        wheel_repo::list(&inst, repo.as_deref(), refresh)
    })
    .await
    .map_err(|e| AppError::new("WHEEL_REPO_JOIN", format!("读取轮子仓库失败：{e}")))?
}

/// 下载云端仓库里的一个 wheel 到用户选择的文件夹；文件名保持原样，不改实例环境。
#[tauri::command]
pub async fn wheel_repo_download(
    state: State<'_, AppState>,
    path: String,
    dest_dir: String,
) -> Result<String, AppError> {
    let repo = state.settings.lock().wheel_repo.clone();
    tauri::async_runtime::spawn_blocking(move || {
        wheel_repo::download(repo.as_deref(), &path, &dest_dir)
    })
    .await
    .map_err(|e| AppError::new("WHEEL_REPO_JOIN", format!("下载轮子失败：{e}")))?
}

/// 安装云端仓库里的一个 wheel；只允许缓存列表里存在的路径。
#[tauri::command]
pub async fn wheel_repo_install(
    state: State<'_, AppState>,
    instance_id: String,
    path: String,
    on_event: tauri::ipc::Channel<crate::commands::version::VersionEvent>,
) -> Result<crate::accel::EnvReport, AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "安装云端轮子")?;
    let inst = crate::commands::instance::load_instance(&instance_id)?;
    if state.runs.lock().contains_key(&instance_id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止再装（venv 里的 dll 被占用，装完也要重启才生效）。",
        ));
    }
    let repo = state.settings.lock().wheel_repo.clone();
    let settings = state.settings.lock().clone();
    tauri::async_runtime::spawn_blocking(move || {
        wheel_repo::install(&inst, repo.as_deref(), &path, &settings, &on_event)
    })
    .await
    .map_err(|e| AppError::new("WHEEL_REPO_JOIN", format!("安装轮子失败：{e}")))?
}

#[tauri::command]
pub async fn uv_cache_info() -> Result<uv_cache::UvCacheInfo, AppError> {
    tauri::async_runtime::spawn_blocking(uv_cache::info)
        .await
        .map_err(|e| AppError::new("UV_CACHE_JOIN", format!("统计 UV 缓存失败：{e}")))
}

#[tauri::command]
pub async fn uv_cache_clear() -> Result<uv_cache::UvCacheClear, AppError> {
    tauri::async_runtime::spawn_blocking(uv_cache::clear)
        .await
        .map_err(|e| AppError::new("UV_CACHE_JOIN", format!("清理 UV 缓存失败：{e}")))?
}
