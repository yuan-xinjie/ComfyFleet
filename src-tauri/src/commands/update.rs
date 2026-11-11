//! 应用自身在线更新（Tauri updater + NSIS 签名安装包）。
//!
//! 三条硬规则（和版本管理那边一致，理由也一样）：
//! 1. **有实例在跑就不许更新**：更新会重启本程序，正在跑的 ComfyUI 任务会被一起带走。
//! 2. **只提示、不自动装**：检查更新可以在启动后静默做，但安装必须由用户点确认
//!    （`app_update_install` 只会被显式按钮触发，没有自动调用点）。
//! 3. **签名校验交给 updater 插件**：公钥写死在 tauri.conf.json，私钥只在发布机/CI 上。
//!
//! 更新源默认是 ModelScope 模型仓库里的 `app/latest.json`：国内直连比 GitHub 快，
//! 大文件会被 302 到带签名的 CDN 链接，updater 跟随重定向即可。

use std::time::Instant;

use tauri::ipc::Channel;
use tauri_plugin_updater::UpdaterExt;

use crate::error::AppError;
use crate::state::AppState;

/// 一次「检查更新」的结果。前端只在 `available = true` 时给用户看「立即更新」。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdate {
    /// 当前运行的版本，来自 `tauri::package_info`
    pub current: String,
    /// 更新源里的最新版本；没有可用更新时为 None
    pub latest: Option<String>,
    /// 发版说明（latest.json 的 notes）
    pub notes: Option<String>,
    /// 发布时间（RFC3339）
    pub pub_date: Option<String>,
    pub available: bool,
    /// 当前正在运行的实例名：非空时前端应直接告诉用户「先停实例」
    pub running: Vec<String>,
}

/// 下载 / 安装进度。`stage` 取值：download / install / done。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEvent {
    pub stage: String,
    pub message: String,
    pub downloaded: u64,
    /// 服务端没给 Content-Length 时为空，前端显示「已下载 xx MB」而不是百分比
    pub total: Option<u64>,
    pub failed: bool,
    pub done: bool,
}

impl UpdateEvent {
    fn stage(stage: &str, message: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            message: message.into(),
            downloaded: 0,
            total: None,
            failed: false,
            done: false,
        }
    }

    fn progress(downloaded: u64, total: Option<u64>) -> Self {
        let message = match total {
            Some(t) if t > 0 => format!("已下载 {:.1} / {:.1} MB", mb(downloaded), mb(t)),
            _ => format!("已下载 {:.1} MB", mb(downloaded)),
        };
        Self {
            stage: "download".into(),
            message,
            downloaded,
            total,
            failed: false,
            done: false,
        }
    }

    fn done(message: impl Into<String>) -> Self {
        Self {
            stage: "done".into(),
            message: message.into(),
            downloaded: 0,
            total: None,
            failed: false,
            done: true,
        }
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// 正在运行的实例名。实例配置读不出来（被删/损坏）时退回实例 id，
/// 目的只是让用户知道「还有哪些没停」，不需要在这里报错。
fn running_instances(state: &AppState) -> Vec<String> {
    let mut ids: Vec<String> = state.runs.lock().keys().cloned().collect();
    ids.sort();
    ids.into_iter()
        .map(|id| {
            crate::commands::instance::load_instance(&id)
                .map(|cfg| cfg.name)
                .unwrap_or(id)
        })
        .collect()
}

/// 把 updater 的原始错误翻成用户能行动的话。
/// 更新源 404 最常见（还没发过版 / latest.json 路径写错），必须单独说清楚。
fn explain_check_error(err: &str) -> String {
    if err.contains("404") || err.contains("Not Found") {
        return format!(
            "更新源里找不到 latest.json。检查 ModelScope 仓库是否已有 app/latest.json，\
             以及 tauri.conf.json 里的 updater.endpoints 是否指对：{err}"
        );
    }
    if err.contains("Could not fetch") || err.contains("error sending request") {
        return format!("连不上更新源（网络 / 镜像）：{err}");
    }
    if err.contains("signature") || err.contains("Signature") {
        return format!("更新包签名校验失败，已拒绝安装：{err}");
    }
    err.to_string()
}

#[tauri::command]
pub async fn app_update_check(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<AppUpdate, AppError> {
    let current = app.package_info().version.to_string();
    let running = running_instances(&state);

    let updater = app
        .updater()
        .map_err(|e| AppError::new("UPDATE_INIT", format!("更新器初始化失败：{e}")))?;

    let checked = updater
        .check()
        .await
        .map_err(|e| AppError::new("UPDATE_CHECK", explain_check_error(&e.to_string())))?;

    Ok(match checked {
        None => AppUpdate {
            current,
            latest: None,
            notes: None,
            pub_date: None,
            available: false,
            running,
        },
        Some(update) => AppUpdate {
            current,
            latest: Some(update.version.clone()),
            notes: update.body.clone(),
            // updater 用 `time` crate 的 OffsetDateTime（不是 chrono），只取时间戳再交给 chrono 格式化
            pub_date: update
                .date
                .and_then(|d| chrono::DateTime::<chrono::Utc>::from_timestamp(d.unix_timestamp(), 0))
                .map(|dt| dt.to_rfc3339()),
            available: true,
            running,
        },
    })
}

/// 下载并安装更新，装完重启本程序。
///
/// 这里不提供「取消」：updater 的下载没有中断接口，假装能取消只会让用户以为
/// 点一下就能停。UI 明确提示「更新期间不要关闭窗口」即可。
#[tauri::command]
pub async fn app_update_install(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    on_event: Channel<UpdateEvent>,
) -> Result<(), AppError> {
    // 1. 运行中的实例：更新会重启本程序，正在跑的任务会被一起终止，直接拒绝
    let running = running_instances(&state);
    if !running.is_empty() {
        return Err(AppError::new(
            "INSTANCES_RUNNING",
            format!(
                "还有 {} 个实例在运行：{}。更新会重启本程序，请先全部停止再试。",
                running.len(),
                running.join("、")
            ),
        ));
    }
    // 2. 安装环境 / 切版本 / 同步准备都可能在写 venv 或缓存，此时重启会留下半写状态
    if state.uv_work_active() {
        return Err(AppError::new(
            "APP_BUSY",
            "正在安装环境 / 切换版本 / 准备同步，等这些操作结束再更新。",
        ));
    }

    let updater = app
        .updater()
        .map_err(|e| AppError::new("UPDATE_INIT", format!("更新器初始化失败：{e}")))?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| AppError::new("UPDATE_CHECK", explain_check_error(&e.to_string())))?
    else {
        return Err(AppError::new("NO_UPDATE", "已经是最新版本，不需要更新。"));
    };

    let version = update.version.clone();
    let _ = on_event.send(UpdateEvent::stage(
        "download",
        format!("开始下载 ComfyFleet {version}"),
    ));

    // 下载/安装期间监督线程不许再拉起实例；有实例已经被上面拦掉了，这里只是兜底
    state.begin_shutdown();

    // 进度节流：每 1% 或 250ms 推一次，避免大包刷屏把 IPC 打满
    let mut downloaded: u64 = 0;
    let mut last_pct: i64 = -1;
    let mut last_emit = Instant::now();
    let ch_progress = on_event.clone();
    let ch_install = on_event.clone();

    let result = update
        .download_and_install(
            move |chunk_len, content_length| {
                downloaded += chunk_len as u64;
                let pct = content_length
                    .filter(|t| *t > 0)
                    .map(|t| (downloaded.saturating_mul(100) / t) as i64)
                    .unwrap_or(-1);
                let elapsed = last_emit.elapsed().as_millis();
                if pct != last_pct || elapsed >= 250 {
                    last_pct = pct;
                    last_emit = Instant::now();
                    let _ = ch_progress.send(UpdateEvent::progress(downloaded, content_length));
                }
            },
            move || {
                let _ = ch_install.send(UpdateEvent::stage("install", "下载完成，正在安装…"));
            },
        )
        .await;

    if let Err(e) = result {
        // 没能装上（下载中断 / 签名不符 / 安装器起不来）：把「正在退出」标记撤掉，
        // 让实例监督恢复正常，用户不必为了恢复功能重启应用。
        state.cancel_shutdown();
        let message = format!("更新失败：{e}");
        let _ = on_event.send(UpdateEvent {
            stage: "done".into(),
            message: message.clone(),
            downloaded: 0,
            total: None,
            failed: true,
            done: true,
        });
        return Err(AppError::new("UPDATE_INSTALL", message));
    }

    let _ = on_event.send(UpdateEvent::done(format!(
        "ComfyFleet {version} 已安装，正在重启…"
    )));
    // Windows 下 `download_and_install` 启动 NSIS 安装器后会自己退出本进程，
    // 再由安装器把新版本拉起来；这里不要再调用 restart，否则会多开一个实例。
    Ok(())
}
