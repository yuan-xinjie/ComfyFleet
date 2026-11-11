//! 集群 / 同步的 Tauri 命令层。
//!
//! 命令只做参数校验、阻塞任务调度和状态编排；真正的网络/同步逻辑在
//! `cluster.rs` 和 `sync.rs`，这样协议和功能都能单独测试。

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;
use tauri::{AppHandle, Manager};

use crate::cluster::{self, ClusterStatus};
use crate::commands::instance::load_instance;
use crate::config::{GlobalSettings, InstanceConfig};
use crate::error::AppError;
use crate::snapshot;
use crate::state::AppState;
use crate::sync::{
    self, SyncDiff, SyncJob, SyncOptions, SyncPrepare, SyncPrepareView, SyncPreview, SyncScope,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateClusterReq {
    pub name: String,
    pub password: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 默认 false：只监听 127.0.0.1，其他机器无法加入，避免无意暴露明文端口。
    #[serde(default)]
    pub allow_lan: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinClusterReq {
    pub host: String,
    pub port: u16,
    pub password: String,
    #[serde(default)]
    pub member_name: String,
}

fn default_port() -> u16 {
    cluster::DEFAULT_PORT
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPreviewReq {
    pub source_instance_id: String,
    pub target_member_id: String,
    pub target_instance_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncTargetReq {
    pub target_member_id: String,
    pub target_instance_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPreviewBatchReq {
    pub source_instance_id: String,
    pub targets: Vec<SyncTargetReq>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStartTargetReq {
    pub target_member_id: String,
    pub target_instance_id: String,
    #[serde(default)]
    pub target_fingerprint: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStartBatchReq {
    pub generation_id: String,
    pub targets: Vec<SyncStartTargetReq>,
    #[serde(default)]
    pub options: SyncOptions,
}

/// 源端只选择客户机，不选择目标实例；目标机在确认时自己选。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRequestReq {
    pub source_instance_id: String,
    pub target_member_ids: Vec<String>,
    /// 不传时按旧行为同步全部四组，保证旧调用方兼容。
    #[serde(default)]
    pub scope: SyncScope,
    /// 用户已确认：缺 wheel 的自定义包直接跳过，不再拦下来。
    #[serde(default)]
    pub skip_missing_wheels: bool,
}

/// `sync_request` 的结果：要么真的开始准备并发出了任务，要么先让用户处理缺的 wheel。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum SyncRequestOutcome {
    Started {
        jobs: Vec<SyncJob>,
    },
    NeedsWheels {
        missing: Vec<sync::MissingWheel>,
        /// 当前配置的 wheel 库目录（没配置/不存在时为空），提示里用。
        library: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStartReq {
    pub generation_id: String,
    pub target_member_id: String,
    pub target_instance_id: String,
    /// 预览时记录的目标状态指纹。目标在预览之后改动过就必须显式允许覆盖，
    /// 不能因为 sync_start 重新采集而把漂移悄悄吞掉。
    #[serde(default)]
    pub target_fingerprint: String,
    #[serde(default)]
    pub options: SyncOptions,
}

#[tauri::command]
pub async fn cluster_status(app: AppHandle) -> Result<ClusterStatus, AppError> {
    // 状态里要读实例登记/磁盘余量；放到阻塞线程，别占 UI 主线程。
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || Ok(cluster::status(&handle)))
        .await
        .map_err(|e| AppError::new("CLUSTER_STATUS", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub fn cluster_retry_server(app: AppHandle) -> Result<ClusterStatus, AppError> {
    cluster::retry_server(&app)?;
    Ok(cluster::status(&app))
}

/// 切换“只监听本机 / 允许局域网”。切换会重建监听端口，已有连接会断开，
/// 成员下一轮心跳会自动重连。
#[tauri::command]
pub async fn cluster_set_lan(app: AppHandle, allow_lan: bool) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        let cfg = {
            let rt = state.cluster.lock();
            let shared = rt
                .leader
                .clone()
                .ok_or_else(|| AppError::new("CLUSTER_NONE", "本机不是集群 Leader"))?;
            let cfg = shared.config.lock().clone();
            cfg
        };
        if cfg.allow_lan == allow_lan {
            return Ok(cluster::status(&handle));
        }
        let mut next = cfg;
        next.allow_lan = allow_lan;
        cluster::save_cluster(&next)?;
        cluster::stop_runtime(&handle);
        // 旧监听线程是 nonblocking accept 循环，给它一点时间释放端口。
        std::thread::sleep(std::time::Duration::from_millis(500));
        let shared = cluster::start_server(next)?;
        cluster::start_source_publisher(&shared, handle.clone());
        state.cluster.lock().leader = Some(shared);
        log::info!("集群监听范围已切换：allow_lan={allow_lan}");
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_SET_LAN", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub async fn cluster_create(
    app: AppHandle,
    req: CreateClusterReq,
) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let shared = cluster::create_cluster(&req.name, &req.password, req.port, req.allow_lan)?;
        cluster::start_source_publisher(&shared, handle.clone());
        let state = handle.state::<AppState>();
        {
            let mut rt = state.cluster.lock();
            rt.leader = Some(shared);
            rt.error = None;
        }
        // 群建好后，本机可能之前加入过别的群；已由 create_cluster 拒绝。
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub async fn cluster_join(app: AppHandle, req: JoinClusterReq) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let member = cluster::join_cluster(&req.host, req.port, &req.password, &req.member_name)?;
        log::info!(
            "已加入集群 {}（{}:{}），成员 {}",
            member.cluster_name,
            member.leader_host,
            member.leader_port,
            member.member_name
        );
        if let Err(e) = cluster::start_agent(handle.clone()) {
            let _ = std::fs::remove_file(cluster::member_path());
            return Err(e);
        }
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub fn cluster_leave(app: AppHandle) -> Result<(), AppError> {
    cluster::stop_runtime(&app);
    cluster::leave_cluster()?;
    Ok(())
}

#[tauri::command]
pub fn cluster_delete(app: AppHandle, password: String) -> Result<(), AppError> {
    if !cluster::cluster_path().exists() {
        return Err(AppError::new("CLUSTER_NONE", "本机不是集群 Leader"));
    }
    cluster::verify_password(&password)?;
    cluster::stop_runtime(&app);
    cluster::remove_cluster()?;
    Ok(())
}

#[tauri::command]
pub fn cluster_remove_member(app: AppHandle, member_id: String) -> Result<ClusterStatus, AppError> {
    cluster::drop_member(&app, &member_id)?;
    Ok(cluster::status(&app))
}

#[tauri::command]
pub fn cluster_cancel_job(app: AppHandle, job_id: String) -> Result<ClusterStatus, AppError> {
    cluster::cancel_job(&app, &job_id)?;
    Ok(cluster::status(&app))
}

#[tauri::command]
pub fn cluster_delete_job(app: AppHandle, job_id: String) -> Result<ClusterStatus, AppError> {
    cluster::delete_job(&app, &job_id)?;
    Ok(cluster::status(&app))
}

#[tauri::command]
pub async fn cluster_confirm_job(
    app: AppHandle,
    job_id: String,
    target_instance_id: String,
    options: Option<SyncOptions>,
) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let inst = load_instance(&target_instance_id)?;
        if inst.is_remote() {
            return Err(AppError::new(
                "SYNC_TARGET_REMOTE",
                "云端实例没有本地环境，不能接收局域网同步",
            ));
        }
        let settings = handle.state::<AppState>().settings.lock().clone();
        let git = crate::git::resolve(&handle, &settings)?;
        let scope = cluster::job_by_id(&handle, &job_id)
            .map(|job| job.scope)
            .unwrap_or_default();
        let target = sync::capture_instance_state_scoped(
            &inst,
            &settings,
            Some(std::path::Path::new(&git.path)),
            scope,
            None,
        )?;
        cluster::confirm_member_job(
            &handle,
            &job_id,
            &target_instance_id,
            target,
            options.unwrap_or_default(),
        )?;
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_CONFIRM", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub async fn cluster_cancel_member_job(
    app: AppHandle,
    job_id: String,
) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        cluster::cancel_member_job(&handle, &job_id)?;
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_CANCEL", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub async fn cluster_retry_member_job(
    app: AppHandle,
    job_id: String,
) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let job = cluster::member_job_by_id(&handle, &job_id)
            .ok_or_else(|| AppError::new("CLUSTER_JOB", "任务不存在"))?;
        if job.status != sync::JOB_FAILED {
            return Err(AppError::new("CLUSTER_JOB", "只有失败的同步可以重试"));
        }
        if job.target_instance_id.trim().is_empty() {
            return Err(AppError::new(
                "SYNC_TARGET",
                "这次同步没有记录目标实例，无法重试",
            ));
        }
        let inst = load_instance(&job.target_instance_id)?;
        if inst.is_remote() {
            return Err(AppError::new(
                "SYNC_TARGET_REMOTE",
                "云端实例没有本地环境，不能接收局域网同步",
            ));
        }
        let settings = handle.state::<AppState>().settings.lock().clone();
        let git = crate::git::resolve(&handle, &settings)?;
        let target = sync::capture_instance_state_scoped(
            &inst,
            &settings,
            Some(std::path::Path::new(&git.path)),
            job.scope,
            None,
        )?;
        cluster::retry_member_job(&handle, &job_id)?;
        cluster::confirm_member_job(
            &handle,
            &job_id,
            &job.target_instance_id,
            target,
            job.options.clone(),
        )?;
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_RETRY", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub async fn cluster_reject_job(app: AppHandle, job_id: String) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        cluster::reject_member_job(&handle, &job_id)?;
        Ok(cluster::status(&handle))
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_REJECT", format!("后台线程异常：{e}")))?
}

#[tauri::command]
pub fn sync_job(app: AppHandle, job_id: String) -> Result<Option<SyncJob>, AppError> {
    Ok(cluster::job_by_id(&app, &job_id))
}

#[tauri::command]
pub fn sync_jobs(app: AppHandle) -> Result<Vec<SyncJob>, AppError> {
    Ok(cluster::jobs(&app))
}

fn source_instance(
    app: &AppHandle,
    id: &str,
) -> Result<(InstanceConfig, GlobalSettings, PathBuf), AppError> {
    let inst = load_instance(id)?;
    if inst.is_remote() {
        return Err(AppError::new(
            "SYNC_SOURCE_REMOTE",
            "云端实例没有本地环境，不能作为同步源",
        ));
    }
    let state = app.state::<AppState>();
    if state.runs.lock().contains_key(id) {
        return Err(AppError::new(
            "SYNC_SOURCE_RUNNING",
            "源实例正在运行：先停止，保证 ComfyUI/节点/包清单是稳定的",
        ));
    }
    let settings = state.settings.lock().clone();
    let git = crate::git::resolve(app, &settings)?;
    Ok((inst, settings, PathBuf::from(git.path)))
}

#[tauri::command]
pub async fn sync_preview(app: AppHandle, req: SyncPreviewReq) -> Result<SyncPreview, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (inst, settings, git) = source_instance(&handle, &req.source_instance_id)?;
        let (_cfg, member, target) = cluster::preview_data(
            &handle,
            &req.source_instance_id,
            &req.target_member_id,
            &req.target_instance_id,
        )?;
        if !member.online {
            return Err(AppError::new(
                "SYNC_TARGET_OFFLINE",
                "目标客户端离线：等它重新连接集群后再试",
            ));
        }
        let target_state = target.clone();
        let mut source = sync::capture_instance_state(&inst, &settings, Some(&git), true);
        // 拍一条正式快照，既作为同步代次的元数据，也方便源端追溯。
        let snap = snapshot::capture(
            &inst,
            &settings,
            "sync",
            Some(format!("同步到 {}", member.name)),
        );
        let manifest = sync::stage_generation(&mut source, Some(snap), Some(&git))?;
        let diff = sync::diff_states(&manifest.source, &target_state);
        let preview = SyncPreview {
            generation_id: manifest.generation_id.clone(),
            source_instance_id: inst.id.clone(),
            source_instance_name: inst.name.clone(),
            target_member_id: member.id.clone(),
            target_instance_id: target_state.instance_id.clone(),
            target_instance_name: target_state.name.clone(),
            target_online: member.online,
            target_last_applied: target_state.last_applied.clone(),
            target_fingerprint: sync::state_fingerprint(&target_state),
            target_disk_free: target_state.disk_free,
            diff,
            totals: manifest.totals.clone(),
        };
        Ok(preview)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

/// 一次采集源实例、打包一份代次，再分别与多个目标实例做 diff。
/// 多目标共享同一个 generationId，避免每台目标重复打包节点/本体。
#[tauri::command]
pub async fn sync_preview_batch(
    app: AppHandle,
    req: SyncPreviewBatchReq,
) -> Result<Vec<SyncPreview>, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let pairs: Vec<(String, String)> = req
            .targets
            .iter()
            .map(|t| (t.target_member_id.clone(), t.target_instance_id.clone()))
            .collect();
        validate_target_pairs(&pairs)?;
        let (inst, settings, git) = source_instance(&handle, &req.source_instance_id)?;
        let mut source = sync::capture_instance_state(&inst, &settings, Some(&git), true);
        let snap = snapshot::capture(
            &inst,
            &settings,
            "sync",
            Some(format!("同步到 {} 个目标", req.targets.len())),
        );
        let manifest = sync::stage_generation(&mut source, Some(snap), Some(&git))?;
        let mut out = Vec::with_capacity(req.targets.len());
        for target in &req.targets {
            let (_cfg, member, target_state) = cluster::preview_data(
                &handle,
                &req.source_instance_id,
                &target.target_member_id,
                &target.target_instance_id,
            )?;
            if !member.online {
                return Err(AppError::new(
                    "SYNC_TARGET_OFFLINE",
                    format!("目标客户端「{}」离线：等它重新连接后再试", member.name),
                ));
            }
            let diff = sync::diff_states(&manifest.source, &target_state);
            out.push(SyncPreview {
                generation_id: manifest.generation_id.clone(),
                source_instance_id: inst.id.clone(),
                source_instance_name: inst.name.clone(),
                target_member_id: member.id.clone(),
                target_instance_id: target_state.instance_id.clone(),
                target_instance_name: target_state.name.clone(),
                target_online: member.online,
                target_last_applied: target_state.last_applied.clone(),
                target_fingerprint: sync::state_fingerprint(&target_state),
                target_disk_free: target_state.disk_free,
                diff,
                totals: manifest.totals.clone(),
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

/// 批量创建同步任务。所有目标先整体校验，再一次性加入队列；
/// 某个目标没有实际变化时跳过，不生成空任务。
#[tauri::command]
pub async fn sync_start_batch(
    app: AppHandle,
    req: SyncStartBatchReq,
) -> Result<Vec<SyncJob>, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let pairs: Vec<(String, String)> = req
            .targets
            .iter()
            .map(|t| (t.target_member_id.clone(), t.target_instance_id.clone()))
            .collect();
        validate_target_pairs(&pairs)?;
        let manifest = sync::load_manifest(&req.generation_id)?;
        let mut jobs = Vec::new();
        for target in &req.targets {
            let (member, target_state) = cluster::member_target(
                &handle,
                &target.target_member_id,
                &target.target_instance_id,
            )
            .ok_or_else(|| {
                AppError::new(
                    "SYNC_TARGET",
                    format!("目标实例不存在：{} / {}", target.target_member_id, target.target_instance_id),
                )
            })?;
            let diff = sync::diff_states(&manifest.source, &target_state);
            if !diff.needs_apply {
                continue;
            }
            if !member.online {
                return Err(AppError::new(
                    "SYNC_TARGET_OFFLINE",
                    format!("目标客户端「{}」不在线", member.name),
                ));
            }
            let current_fingerprint = sync::state_fingerprint(&target_state);
            if sync::target_drift_requires_confirmation(
                &target.target_fingerprint,
                &current_fingerprint,
                target_state.last_applied.as_ref(),
                req.options.allow_dirty_target,
            ) {
                return Err(AppError::new(
                    "SYNC_TARGET_DRIFT",
                    format!(
                        "目标实例「{}」自上次同步后有本地改动；请重新预览并勾选「目标有本地改动，我确认覆盖」",
                        target_state.name
                    ),
                ));
            }
            let needed = sync::required_space(&diff, &manifest.totals);
            if target_state.disk_free > 0 && needed > target_state.disk_free {
                return Err(AppError::new(
                    "SYNC_DISK",
                    format!(
                        "目标实例「{}」磁盘空间不足：预计需要 {}，目标只剩 {}",
                        target_state.name,
                        sync::human_bytes(needed),
                        sync::human_bytes(target_state.disk_free)
                    ),
                ));
            }
            let mut logs = plan_logs(&diff, &req.options);
            if !diff.python.missing_wheels.is_empty() {
                let names: Vec<&str> = diff
                    .python
                    .missing_wheels
                    .iter()
                    .map(|w| w.name.as_str())
                    .collect();
                logs.push(sync::job_log(&format!(
                    "警告：目标实例「{}」缺少自定义包文件：{}；本次跳过，结束后统一提示",
                    target_state.name,
                    names.join(", ")
                )));
            }
            logs.insert(
                0,
                sync::job_log(&format!(
                    "批量同步：本次共选择 {} 个目标，本任务是其中之一",
                    req.targets.len()
                )),
            );
            jobs.push(SyncJob {
                id: uuid::Uuid::new_v4().to_string().replace('-', ""),
                generation_id: manifest.generation_id.clone(),
                source_instance_id: manifest.source_instance_id.clone(),
                source_instance_name: manifest.source_instance_name.clone(),
                target_member_id: member.id.clone(),
                target_member_name: member.name.clone(),
                target_instance_id: target_state.instance_id.clone(),
                target_instance_name: target_state.name.clone(),
                status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                created_at: sync::now(),
                accepted_at: None,
                started_at: None,
                finished_at: None,
                updated_at: sync::now(),
                progress: 0,
                message: "等待目标端确认后才会开始".to_string(),
                error: None,
                options: req.options.clone(),
                scope: SyncScope::all(),
                diff,
                target_fingerprint: if target.target_fingerprint.is_empty() {
                    current_fingerprint
                } else {
                    target.target_fingerprint.clone()
                },
                target_online: member.online,
                cancel_requested: false,
                target_cancel_requested: false,
                target_cancelled_at: None,
                target_cancel_count: 0,
                report: None,
                logs,
            });
        }
        cluster::add_jobs(&handle, jobs.clone())?;
        Ok(jobs)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

/// 源端发起同步：只指定客户机。目标机收到后自己选择实例并确认，源端不预判目标差异。
#[tauri::command]
pub async fn sync_request(
    app: AppHandle,
    req: SyncRequestReq,
) -> Result<SyncRequestOutcome, AppError> {
    let handle = app.clone();
    if !req.scope.any() {
        return Err(AppError::new("SYNC_SCOPE", "至少选择一个同步项目"));
    }
    let source_id = req.source_instance_id.clone();
    {
        // 并发准备先挡掉；预检必须发生在真正准备之前，不能先拷贝再回滚。
        let state = handle.state::<AppState>();
        let prepares = state.sync_prepares.lock();
        if let Some(existing) = prepares.get(&source_id) {
            if !existing.is_done() {
                return Err(AppError::new(
                    "SYNC_PREPARING",
                    "这个源实例正在准备同步，请等它结束或先停止准备",
                ));
            }
        }
    }
    if req.scope.python && !req.skip_missing_wheels {
        let handle = handle.clone();
        let source = req.source_instance_id.clone();
        let (missing, library) = tauri::async_runtime::spawn_blocking(move || {
            let (inst, settings, _git) = source_instance(&handle, &source)?;
            Ok::<_, AppError>((
                sync::missing_custom_wheels(&inst, &settings),
                crate::wheelhouse::dir(&settings).map(|p| p.to_string_lossy().to_string()),
            ))
        })
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))??;
        if !missing.is_empty() {
            return Ok(SyncRequestOutcome::NeedsWheels { missing, library });
        }
    }
    let prep = {
        let state = handle.state::<AppState>();
        let mut prepares = state.sync_prepares.lock();
        if let Some(existing) = prepares.get(&source_id) {
            if !existing.is_done() {
                return Err(AppError::new(
                    "SYNC_PREPARING",
                    "这个源实例正在准备同步，请等它结束或先停止准备",
                ));
            }
        }
        let prep = std::sync::Arc::new(SyncPrepare::new(&source_id));
        prepares.insert(source_id.clone(), prep.clone());
        prep
    };

    let worker = {
        let handle = handle.clone();
        let prep = prep.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let result = (|| -> Result<SyncRequestOutcome, AppError> {
                if req.target_member_ids.is_empty() {
                    return Err(AppError::new("SYNC_TARGET", "至少选择一台目标客户机"));
                }
                if req.target_member_ids.len() > 16 {
                    return Err(AppError::new("SYNC_TARGET", "一次最多选择 16 台目标客户机"));
                }
                let mut seen = HashSet::new();
                let mut members = Vec::new();
                prep.update("校验目标", 1, "检查目标客户机", "确认在线、实例与数量限制");
                for id in &req.target_member_ids {
                    prep.check()?;
                    let id = id.trim();
                    if id.is_empty() || !seen.insert(id.to_string()) {
                        return Err(AppError::new("SYNC_TARGET", "目标客户机不能为空或重复"));
                    }
                    let member = cluster::leader_member(&handle, id).ok_or_else(|| {
                        AppError::new("SYNC_TARGET", format!("目标客户机不存在：{id}"))
                    })?;
                    if !member.online {
                        return Err(AppError::new(
                            "SYNC_TARGET_OFFLINE",
                            format!("目标客户机「{}」离线：等它重新连接后再发", member.name),
                        ));
                    }
                    if member.instances.is_empty() {
                        return Err(AppError::new(
                            "SYNC_TARGET_NO_INSTANCE",
                            format!(
                                "目标客户机「{}」还没有可用实例，先在目标机创建实例并等待上线",
                                member.name
                            ),
                        ));
                    }
                    members.push(member);
                }
                for member in &members {
                    if let Some(name) = cluster::member_active_job_name(&handle, &member.id) {
                        return Err(AppError::new(
                            "SYNC_BUSY",
                            format!(
                                "客户机「{}」已有同步任务在排队/执行，等它完成或取消后再发",
                                name
                            ),
                        ));
                    }
                }
                prep.update(
                    "等待源实例",
                    1,
                    "等待源实例空闲",
                    "获取实例操作锁，避免与启动/安装/更新并发",
                );
                let _guard = crate::state::OpGuard::acquire(&req.source_instance_id, "准备同步")?;
                let (inst, settings, git) = source_instance(&handle, &req.source_instance_id)?;
                prep.update(
                    "扫描源实例",
                    2,
                    "读取源实例",
                    format!("实例「{}」，范围 {}", inst.name, req.scope.summary()),
                );
                prep.check()?;
                let mut source = sync::capture_instance_state_scoped(
                    &inst,
                    &settings,
                    Some(&git),
                    req.scope,
                    Some(prep.as_ref()),
                )?;
                prep.check()?;
                let fingerprint = sync::preparation_fingerprint(&source, req.scope, &settings);
                prep.update(
                    "检查同步缓存",
                    36,
                    "检查可复用的准备数据",
                    "源内容和同步范围都没变时沿用上一次代次",
                );
                let (manifest, prepared_note) = if let Some(manifest) =
                    sync::find_prepared_generation(&source.instance_id, req.scope, &fingerprint)
                {
                    prep.note(format!("复用上次准备的数据：{}", manifest.generation_id));
                    (
                        manifest,
                        "复用上次准备的同步数据（源内容与范围未变化）".to_string(),
                    )
                } else {
                    prep.update(
                        "拍同步快照",
                        37,
                        "记录同步前快照",
                        format!("只记录本次勾选的项目：{}", req.scope.summary()),
                    );
                    let snap = snapshot::capture_scoped(
                        &inst,
                        &settings,
                        "sync",
                        Some(format!("同步到 {} 台客户机", members.len())),
                        req.scope,
                    );
                    prep.check()?;
                    let mut manifest = sync::stage_generation_scoped(
                        &mut source,
                        Some(snap),
                        Some(&git),
                        req.scope,
                        Some(prep.as_ref()),
                    )?;
                    manifest.prepared_fingerprint = fingerprint;
                    sync::save_manifest(&manifest)?;
                    (
                        manifest,
                        "已准备新的同步代次（源内容有变化或首次同步）".to_string(),
                    )
                };
                prep.check()?;
                prep.update(
                    "创建同步任务",
                    98,
                    "写入客户机任务",
                    "目标机下一轮会看到待确认请求",
                );
                let skipped: Vec<String> = source
                    .python
                    .custom_wheels
                    .iter()
                    .filter(|wheel| !wheel.available)
                    .map(|wheel| wheel.name.clone())
                    .collect();
                let jobs: Vec<SyncJob> = members
                    .iter()
                    .map(|member| {
                        let mut logs = vec![
                            sync::job_log(&format!("同步请求已发送到客户机「{}」", member.name)),
                            sync::job_log(&format!("同步范围：{}", req.scope.summary())),
                            sync::job_log(&prepared_note),
                        ];
                        if !skipped.is_empty() {
                            logs.push(sync::job_log(&format!(
                                "按要求跳过缺少文件的自定义包：{}；目标端不会安装这些依赖",
                                skipped.join(", ")
                            )));
                        }
                        logs.push(sync::job_log(
                            "目标机将自行选择本机实例；接受前不会写入任何文件",
                        ));
                        SyncJob {
                            id: uuid::Uuid::new_v4().to_string().replace('-', ""),
                            generation_id: manifest.generation_id.clone(),
                            source_instance_id: manifest.source_instance_id.clone(),
                            source_instance_name: manifest.source_instance_name.clone(),
                            target_member_id: member.id.clone(),
                            target_member_name: member.name.clone(),
                            target_instance_id: String::new(),
                            target_instance_name: String::new(),
                            status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
                            created_at: sync::now(),
                            accepted_at: None,
                            started_at: None,
                            finished_at: None,
                            updated_at: sync::now(),
                            progress: 0,
                            message: "等待目标机选择实例并接受".to_string(),
                            error: None,
                            // 清理目标端多余内容的开关属于目标机：源端创建任务时一律保留。
                            options: SyncOptions::default(),
                            scope: req.scope,
                            diff: SyncDiff::default(),
                            target_fingerprint: String::new(),
                            target_online: true,
                            cancel_requested: false,
                            target_cancel_requested: false,
                            target_cancelled_at: None,
                            target_cancel_count: 0,
                            report: None,
                            logs,
                        }
                    })
                    .collect();
                cluster::add_jobs(&handle, jobs.clone())?;
                Ok(SyncRequestOutcome::Started { jobs })
            })();

            match &result {
                Ok(SyncRequestOutcome::Started { jobs }) => {
                    // 新任务已写入，回收旧代次；当前代次和所有活动任务会被保护。
                    let cache_keep = handle
                        .state::<AppState>()
                        .settings
                        .lock()
                        .sync
                        .sync_cache_keep;
                    let pruned = sync::prune_sync_cache(cache_keep);
                    if pruned.removed > 0 {
                        log::info!(
                            "同步任务已创建，清理旧缓存 {} 组，释放 {}",
                            pruned.removed,
                            sync::human_bytes(pruned.freed_bytes)
                        );
                    }
                    if let Some(manifest_id) = jobs.first().map(|job| job.generation_id.as_str()) {
                        prep.finish(jobs.len(), manifest_id);
                    }
                }
                // 准备线程里不会返回这个分支（预检在进准备前就拦下了），保持穷尽。
                Ok(SyncRequestOutcome::NeedsWheels { .. }) => {}
                Err(error) => {
                    let cleanup_error = cleanup_generation(&prep);
                    if error.code == "SYNC_CANCELLED" {
                        prep.mark_cancelled(cleanup_error);
                    } else {
                        let message = match cleanup_error {
                            Some(cleanup) => {
                                format!("{}；临时文件清理失败：{}", error.message, cleanup)
                            }
                            None => error.message.clone(),
                        };
                        prep.fail(message);
                    }
                }
            }
            result
        })
    };

    match worker.await {
        Ok(result) => result,
        Err(error) => {
            let message = format!("准备线程异常退出：{error}");
            let cleanup = cleanup_generation(&prep);
            prep.fail(match cleanup {
                Some(cleanup) => format!("{message}；临时文件清理失败：{cleanup}"),
                None => message.clone(),
            });
            Err(AppError::new("SYNC_REQUEST", message))
        }
    }
}

fn cleanup_generation(prep: &SyncPrepare) -> Option<String> {
    let id = prep.generation_id()?;
    let dir = sync::generation_dir(&id);
    if !dir.exists() {
        return None;
    }
    let mut last = None;
    for _ in 0..3 {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => return None,
            Err(e) => {
                last = Some(e.to_string());
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        }
    }
    last
}

#[tauri::command]
pub fn sync_prepare_status(app: AppHandle, source_instance_id: String) -> Option<SyncPrepareView> {
    let state = app.state::<AppState>();
    let prepares = state.sync_prepares.lock();
    prepares.get(&source_instance_id).map(|prep| prep.view())
}

#[tauri::command]
pub fn sync_prepare_cancel(app: AppHandle, source_instance_id: String) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    let prepares = state.sync_prepares.lock();
    let prep = prepares
        .get(&source_instance_id)
        .ok_or_else(|| AppError::new("SYNC_PREPARE", "没有正在进行的准备任务"))?;
    if prep.is_done() {
        return Err(AppError::new("SYNC_PREPARE", "准备已经结束，不能再停止"));
    }
    prep.request_cancel();
    Ok(())
}

#[tauri::command]
pub fn sync_prepare_clear(app: AppHandle, source_instance_id: String) -> Result<(), AppError> {
    let state = app.state::<AppState>();
    let mut prepares = state.sync_prepares.lock();
    if let Some(prep) = prepares.get(&source_instance_id) {
        if !prep.is_done() {
            return Err(AppError::new("SYNC_PREPARE", "准备还在进行，不能关闭"));
        }
    }
    prepares.remove(&source_instance_id);
    Ok(())
}

/// 源端展示「与上次成功同步相比」的变化；不连接目标机，也不生成任务。
#[tauri::command]
pub async fn cluster_source_changes(
    app: AppHandle,
    source_instance_id: String,
) -> Result<cluster::SourceChanges, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        cluster::source_changes(&handle, &source_instance_id)
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_CHANGES", format!("后台线程异常：{e}")))?
}

/// 目标端只读比较：选择 Leader 的源实例与本机目标实例，返回五组 diff。
#[tauri::command]
pub async fn cluster_compare(
    app: AppHandle,
    source_instance_id: String,
    target_instance_id: String,
) -> Result<cluster::CompareResp, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        cluster::compare_with_leader(&handle, &source_instance_id, &target_instance_id)
    })
    .await
    .map_err(|e| AppError::new("CLUSTER_COMPARE", format!("后台线程异常：{e}")))?
}

/// 批量目标校验：非空、最多 16 个、同一 (成员, 实例) 不能重复。
fn validate_target_pairs(pairs: &[(String, String)]) -> Result<(), AppError> {
    if pairs.is_empty() {
        return Err(AppError::new("SYNC_TARGET", "至少选择一个目标实例"));
    }
    if pairs.len() > 16 {
        return Err(AppError::new("SYNC_TARGET", "一次最多选择 16 个目标实例"));
    }
    let mut seen = HashSet::new();
    for (member, instance) in pairs {
        if !seen.insert((member.clone(), instance.clone())) {
            return Err(AppError::new("SYNC_TARGET", "目标实例不能重复选择"));
        }
    }
    Ok(())
}

/// 任务创建时就把“到底会改什么”写进日志，目标端确认页和源端控制台都能直接看到。
fn plan_logs(diff: &SyncDiff, options: &SyncOptions) -> Vec<String> {
    let mut logs = vec![sync::job_log(
        "源端已创建同步任务，等待目标客户端检查计划并确认",
    )];
    logs.push(sync::job_log(&format!(
        "ComfyUI 本体：{}，commit {} → {}",
        if diff.core.changed {
            "有变化"
        } else {
            "无变化"
        },
        diff.core
            .from_commit
            .as_deref()
            .map(|c| sync::short_chars(c, 10))
            .unwrap_or_else(|| "—".to_string()),
        diff.core
            .to_commit
            .as_deref()
            .map(|c| sync::short_chars(c, 10))
            .unwrap_or_else(|| "—".to_string())
    )));
    logs.push(sync::job_log(&format!(
        "启动参数：{}:{} → {}:{}，CORS {} → {}，可同步自定义参数 {} → {} 项",
        diff.launch.from_listen,
        diff.launch.from_port,
        diff.launch.to_listen,
        diff.launch.to_port,
        diff.launch.from_cors_origin.as_deref().unwrap_or("—"),
        diff.launch.to_cors_origin.as_deref().unwrap_or("—"),
        diff.launch.from_extra_args.len(),
        diff.launch.to_extra_args.len()
    )));
    for arg in diff.launch.to_extra_args.iter().take(40) {
        logs.push(sync::job_log(&format!("  + 启动参数 {}", arg)));
    }
    if !diff.launch.to_local_extra_args.is_empty() {
        logs.push(sync::job_log(&format!(
            "  ! 源端目录类参数不同步（本机路径）：{} 项",
            diff.launch.to_local_extra_args.len()
        )));
        for arg in diff.launch.to_local_extra_args.iter().take(40) {
            logs.push(sync::job_log(&format!("  ! 忽略 {}", arg)));
        }
    }
    if !diff.launch.from_local_extra_args.is_empty() {
        logs.push(sync::job_log(&format!(
            "  = 保留目标本机目录参数：{} 项",
            diff.launch.from_local_extra_args.len()
        )));
        for arg in diff.launch.from_local_extra_args.iter().take(40) {
            logs.push(sync::job_log(&format!("  = 保留 {}", arg)));
        }
    }
    logs.push(sync::job_log(&format!(
        "Python：{} → {}；包 +{} / -{} / ~{}；自建包 +{} / -{} / ~{}",
        diff.python.from_version,
        diff.python.to_version,
        diff.python.packages_added.len(),
        diff.python.packages_removed.len(),
        diff.python.packages_changed.len(),
        diff.python.wheels_added.len(),
        diff.python.wheels_removed.len(),
        diff.python.wheels_changed.len()
    )));
    for pkg in diff.python.packages_added.iter().take(30) {
        logs.push(sync::job_log(&format!("  + 包 {}=={}", pkg.name, pkg.to)));
    }
    for pkg in diff.python.packages_changed.iter().take(30) {
        logs.push(sync::job_log(&format!(
            "  ~ 包 {}：{} → {}",
            pkg.name, pkg.from, pkg.to
        )));
    }
    for pkg in diff.python.packages_removed.iter().take(30) {
        logs.push(sync::job_log(&format!(
            "  - 删除包 {}=={}",
            pkg.name, pkg.from
        )));
    }
    for wheel in diff.python.wheels_added.iter().take(20) {
        logs.push(sync::job_log(&format!(
            "  + 自建包 {} {}",
            wheel.name, wheel.to
        )));
    }
    for wheel in diff.python.wheels_changed.iter().take(20) {
        logs.push(sync::job_log(&format!(
            "  ~ 自建包 {}：{} → {}",
            wheel.name, wheel.from, wheel.to
        )));
    }
    for wheel in diff.python.wheels_removed.iter().take(20) {
        logs.push(sync::job_log(&format!("  - 卸载目标自建包 {}", wheel.name)));
    }
    logs.push(sync::job_log(&format!(
        "自定义节点：+{} / ~{} / -{}（删除目标多余：{}）",
        diff.nodes.added.len(),
        diff.nodes.changed.len(),
        diff.nodes.removed.len(),
        if options.remove_extra_nodes {
            "是"
        } else {
            "否"
        }
    )));
    for node in diff.nodes.added.iter().take(30) {
        logs.push(sync::job_log(&format!("  + 节点 {}", node.name)));
    }
    for node in diff.nodes.changed.iter().take(30) {
        logs.push(sync::job_log(&format!(
            "  ~ 节点 {}：{}",
            node.name, node.reason
        )));
    }
    if options.remove_extra_nodes {
        for node in diff.nodes.removed.iter().take(30) {
            logs.push(sync::job_log(&format!(
                "  - 删除目标多余节点 {}",
                node.name
            )));
        }
    }
    logs.push(sync::job_log(&format!(
        "共享模型：传输 {}，新增 {} / 覆盖 {} / 目标多出 {}（删除目标多余：{}）",
        sync::human_bytes(diff.models.transfer_bytes),
        diff.models.added.len(),
        diff.models.modified.len(),
        diff.models.removed.len(),
        if options.remove_extra_models {
            "是"
        } else {
            "否"
        }
    )));
    for model in diff.models.added.iter().take(40) {
        logs.push(sync::job_log(&format!(
            "  + 模型 {}（{}）",
            model.relative_path,
            sync::human_bytes(model.size)
        )));
    }
    for model in diff.models.modified.iter().take(40) {
        logs.push(sync::job_log(&format!(
            "  ~ 覆盖模型 {}（{} → {}）",
            model.relative_path,
            sync::human_bytes(model.from_size),
            sync::human_bytes(model.size)
        )));
    }
    if options.remove_extra_models {
        for model in diff.models.removed.iter().take(40) {
            logs.push(sync::job_log(&format!(
                "  - 删除目标多余模型 {}",
                model.relative_path
            )));
        }
    }
    logs
}

#[tauri::command]
pub async fn sync_start(app: AppHandle, req: SyncStartReq) -> Result<SyncJob, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let manifest = sync::load_manifest(&req.generation_id)?;
        let (member, target) =
            cluster::member_target(&handle, &req.target_member_id, &req.target_instance_id)
                .ok_or_else(|| AppError::new("SYNC_TARGET", "目标成员或实例不存在"))?;
        if !member.online {
            return Err(AppError::new("SYNC_TARGET_OFFLINE", "目标客户端不在线"));
        }
        let current_fingerprint = sync::state_fingerprint(&target);
        if sync::target_drift_requires_confirmation(
            &req.target_fingerprint,
            &current_fingerprint,
            target.last_applied.as_ref(),
            req.options.allow_dirty_target,
        ) {
            return Err(AppError::new(
                "SYNC_TARGET_DRIFT",
                "目标实例自上次同步后有本地改动；请重新预览并勾选「目标有本地改动，我确认覆盖」",
            ));
        }
        let diff = sync::diff_states(&manifest.source, &target);
        let needed = sync::required_space(&diff, &manifest.totals);
        if target.disk_free > 0 && needed > target.disk_free {
            return Err(AppError::new(
                "SYNC_DISK",
                format!(
                    "目标磁盘空间不足：预计需要 {}，目标只剩 {}（可能包含备份与解压余量）",
                    sync::human_bytes(needed),
                    sync::human_bytes(target.disk_free)
                ),
            ));
        }
        let mut logs = plan_logs(&diff, &req.options);
        if !diff.python.missing_wheels.is_empty() {
            let names: Vec<&str> = diff
                .python
                .missing_wheels
                .iter()
                .map(|w| w.name.as_str())
                .collect();
            logs.insert(
                0,
                sync::job_log(&format!(
                    "警告：缺少自定义包 wheel：{}；本次跳过，结束后统一提示",
                    names.join(", ")
                )),
            );
        }
        let job = SyncJob {
            id: uuid::Uuid::new_v4().to_string().replace('-', ""),
            generation_id: manifest.generation_id.clone(),
            source_instance_id: manifest.source_instance_id.clone(),
            source_instance_name: manifest.source_instance_name.clone(),
            target_member_id: member.id.clone(),
            target_member_name: member.name.clone(),
            target_instance_id: target.instance_id.clone(),
            target_instance_name: target.name.clone(),
            status: sync::JOB_AWAITING_CONFIRMATION.to_string(),
            created_at: sync::now(),
            accepted_at: None,
            started_at: None,
            finished_at: None,
            updated_at: sync::now(),
            progress: 0,
            message: "等待目标端确认后才会开始".to_string(),
            error: None,
            options: req.options,
            scope: SyncScope::all(),
            diff,
            target_fingerprint: if req.target_fingerprint.is_empty() {
                current_fingerprint
            } else {
                req.target_fingerprint
            },
            target_online: member.online,
            cancel_requested: false,
            target_cancel_requested: false,
            target_cancelled_at: None,
            target_cancel_count: 0,
            report: None,
            logs,
        };
        cluster::add_job(&handle, job.clone())?;
        Ok(job)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

/// 给前端一个轻量的目标状态刷新：目标 Agent 会在下一次心跳重新采集。
#[tauri::command]
pub async fn cluster_refresh(app: AppHandle) -> Result<ClusterStatus, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || Ok(cluster::status(&handle)))
        .await
        .map_err(|e| AppError::new("CLUSTER_STATUS", format!("后台线程异常：{e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_target_pairs_reject_empty_duplicate_and_too_many() {
        assert!(validate_target_pairs(&[]).is_err());
        let pair = ("m1".to_string(), "i1".to_string());
        assert!(validate_target_pairs(&[pair.clone()]).is_ok());
        assert!(validate_target_pairs(&[pair.clone(), pair.clone()]).is_err());
        let many: Vec<(String, String)> = (0..17)
            .map(|i| ("m".to_string(), format!("i{i}")))
            .collect();
        assert!(validate_target_pairs(&many).is_err());
    }
}
