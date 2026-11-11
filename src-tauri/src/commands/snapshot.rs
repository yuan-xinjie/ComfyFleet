//! 快照的 IPC 层：列表 / 详情 / 手动拍 / 删除 / 恢复。
//!
//! 恢复是长操作，用 `Channel` 把每一步推给界面（和安装、版本切换一个套路）。

use std::collections::HashSet;

use parking_lot::Mutex;
use tauri::ipc::Channel;
use tauri::Manager;
use tauri::State;

use crate::commands::instance::{load_instance, validate_instance_id};
use crate::config::{self, GlobalSettings};
use crate::error::AppError;
use crate::snapshot::{self, RestoreEvent, RestoreOptions, RestoreReport, Snapshot};
use crate::state::AppState;

/// 同一实例同时只允许一个快照操作在跑（恢复 / 拍快照 / 删除）。
static BUSY: Mutex<Option<HashSet<String>>> = Mutex::new(None);

struct BusyGuard(String);

impl BusyGuard {
    fn acquire(id: &str) -> Result<Self, AppError> {
        let mut slot = BUSY.lock();
        let set = slot.get_or_insert_with(HashSet::new);
        if set.contains(id) {
            return Err(AppError::new(
                "SNAPSHOT_BUSY",
                "这个实例的快照操作还没结束，等它跑完",
            ));
        }
        set.insert(id.to_string());
        Ok(Self(id.to_string()))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(set) = BUSY.lock().as_mut() {
            set.remove(&self.0);
        }
    }
}

fn settings_snapshot() -> GlobalSettings {
    config::load_settings().unwrap_or_default()
}

/// 时间线一行：摘要 + 与上一条的差异计数。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    pub id: String,
    pub created_at: String,
    pub trigger: String,
    pub label: Option<String>,
    pub comfyui_version: String,
    pub node_count: usize,
    pub package_count: usize,
    pub note: Option<String>,
    pub diff: snapshot::SnapshotDiffSummary,
    pub previous_version: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDetail {
    pub snapshot: Snapshot,
    pub version: String,
    pub diff_previous: Option<snapshot::SnapshotDiff>,
    pub diff_current: snapshot::SnapshotDiff,
}

fn version_of(snap: &Snapshot) -> String {
    snap.comfyui
        .version
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            snap.comfyui
                .commit
                .as_deref()
                .map(|c| crate::sync::short_chars(c, 7))
        })
        .unwrap_or_else(|| snap.comfyui.reference.clone())
}

fn summarize(list: &[Snapshot], index: usize) -> SnapshotSummary {
    let snap = &list[index];
    // 列表是倒序：index + 1 才是更早的那一条
    let previous = list.get(index + 1);
    let diff = previous
        .map(|prev| snapshot::diff(prev, snap))
        .unwrap_or_default();
    SnapshotSummary {
        id: snap.id.clone(),
        created_at: snap.created_at.clone(),
        trigger: snap.trigger.clone(),
        label: snap.label.clone(),
        comfyui_version: version_of(snap),
        node_count: snap.nodes.len(),
        package_count: snap.packages.len(),
        note: snap.note.clone(),
        diff: diff.summary(),
        previous_version: previous.map(version_of),
    }
}

#[tauri::command]
pub fn snapshot_list(instance_id: String) -> Result<Vec<SnapshotSummary>, AppError> {
    validate_instance_id(&instance_id)?;
    let inst = load_instance(&instance_id)?;
    let list = snapshot::list_in(&snapshot::dir(&inst));
    Ok((0..list.len()).map(|i| summarize(&list, i)).collect())
}

#[tauri::command]
pub async fn snapshot_detail(instance_id: String, id: String) -> Result<SnapshotDetail, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞

    tauri::async_runtime::spawn_blocking(move || snapshot_detail_impl(instance_id, id))
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn snapshot_detail_impl(instance_id: String, id: String) -> Result<SnapshotDetail, AppError> {
    let inst = load_instance(&instance_id)?;
    let settings = settings_snapshot();
    let base = snapshot::dir(&inst);
    let list = snapshot::list_in(&base);
    let snap = snapshot::load_in(&base, &id)?;
    let index = list.iter().position(|s| s.id == id).unwrap_or(0);
    let current = snapshot::capture(&inst, &settings, "peek", None);
    Ok(SnapshotDetail {
        version: version_of(&snap),
        diff_previous: list.get(index + 1).map(|prev| snapshot::diff(prev, &snap)),
        diff_current: snapshot::diff(&current, &snap),
        snapshot: snap,
    })
}

#[tauri::command]
pub async fn snapshot_create(
    instance_id: String,
    label: Option<String>,
) -> Result<SnapshotSummary, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞

    tauri::async_runtime::spawn_blocking(move || snapshot_create_impl(instance_id, label))
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn snapshot_create_impl(
    instance_id: String,
    label: Option<String>,
) -> Result<SnapshotSummary, AppError> {
    let _guard = BusyGuard::acquire(&instance_id)?;
    // 不能在其他长操作（安装、切版本、同步应用）进行到一半时拍快照。
    let _op = crate::state::OpGuard::acquire(&instance_id, "创建快照")?;
    let inst = load_instance(&instance_id)?;
    let settings = settings_snapshot();
    let snap = snapshot::capture_and_save(&inst, &settings, snapshot::T_MANUAL, label)?;
    crate::activity::record(
        &instance_id,
        "snapshot-create",
        "ok",
        format!(
            "创建快照{}",
            snap.label
                .as_deref()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default()
        ),
        Some(format!(
            "快照 {}\n节点 {} 个 · 包 {} 个{}",
            snap.id,
            snap.nodes.len(),
            snap.packages.len(),
            snap.note
                .as_deref()
                .map(|n| format!("\n{n}"))
                .unwrap_or_default()
        )),
    );
    let list = snapshot::list_in(&snapshot::dir(&inst));
    let index = list.iter().position(|s| s.id == snap.id).unwrap_or(0);
    Ok(summarize(&list, index))
}

#[tauri::command]
pub fn snapshot_delete(instance_id: String, id: String) -> Result<(), AppError> {
    validate_instance_id(&instance_id)?;
    let _guard = BusyGuard::acquire(&instance_id)?;
    let inst = load_instance(&instance_id)?;
    snapshot::delete_in(&snapshot::dir(&inst), &id)?;
    crate::activity::record(
        &instance_id,
        "snapshot-delete",
        "ok",
        "删除快照",
        Some(format!("快照 {id}")),
    );
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestorePreview {
    pub running: bool,
    pub diff: snapshot::SnapshotDiff,
    pub targets: snapshot::RestoreTargets,
    /// 这次要装、但 wheel 库里找不到文件的包；界面先让用户补齐或确认跳过。
    pub missing_wheels: Vec<crate::sync::MissingWheel>,
}

/// 恢复前的预览：会改什么、有多少项、torch 系哪些被跳过。
#[tauri::command]
pub async fn snapshot_restore_preview(
    app: tauri::AppHandle,
    instance_id: String,
    id: String,
) -> Result<RestorePreview, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        snapshot_restore_preview_impl(state, instance_id, id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn snapshot_restore_preview_impl(
    state: State<'_, AppState>,
    instance_id: String,
    id: String,
) -> Result<RestorePreview, AppError> {
    let inst = load_instance(&instance_id)?;
    let settings = settings_snapshot();
    let target = snapshot::load_in(&snapshot::dir(&inst), &id)?;
    let current = snapshot::capture(&inst, &settings, "peek", None);
    let missing_wheels = snapshot::missing_restore_wheels(&target, &current.packages, &settings);
    Ok(RestorePreview {
        running: state.runs.lock().contains_key(&instance_id),
        diff: snapshot::diff(&current, &target),
        targets: snapshot::restore_targets(&current, &target),
        missing_wheels,
    })
}

/// 恢复：执行前先确认实例没在跑，执行完把实例记录对齐到真实 HEAD。
#[tauri::command]
pub async fn snapshot_restore(
    app: tauri::AppHandle,
    instance_id: String,
    id: String,
    options: Option<RestoreOptions>,
    on_event: Channel<RestoreEvent>,
) -> Result<RestoreReport, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        snapshot_restore_impl(handle.clone(), state, instance_id, id, options, on_event)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn snapshot_restore_impl(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    id: String,
    options: Option<RestoreOptions>,
    on_event: Channel<RestoreEvent>,
) -> Result<RestoreReport, AppError> {
    let _guard = BusyGuard::acquire(&instance_id)?;
    let _op = crate::state::OpGuard::acquire(&instance_id, "恢复快照")?;
    if state.runs.lock().contains_key(&instance_id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止，再恢复快照（否则文件被占用、包也会被装回去又覆盖）",
        ));
    }
    let mut inst = load_instance(&instance_id)?;
    let settings = settings_snapshot();
    let target = snapshot::load_in(&snapshot::dir(&inst), &id)?;

    let git = crate::git::resolve(&app, &settings)?;
    let uv = crate::commands::env::resolve_uv()?;
    let ctx = snapshot::RestoreCtx {
        inst: &inst,
        settings: &settings,
        git: std::path::Path::new(&git.path),
        uv: &uv,
    };

    let mut sink = |ev: RestoreEvent| {
        let _ = on_event.send(ev);
    };
    let report = snapshot::restore(&ctx, &target, options.unwrap_or_default(), &mut sink)?;

    // 恢复后 git 可能换了 commit：把实例记录对齐到真实 HEAD，
    // 否则「版本」页还会显示切换前的状态。
    let mut dirty = false;
    // 版本没恢复成功时不能把记录写成目标版本，否则首页/后续快照都会带着错误标签。
    if report.core.error.is_none() {
        if let Some(head) = snapshot::head_comfy(&inst.root_dir) {
            inst.comfy.commit = head;
            inst.comfy.channel = target.comfyui.channel.clone();
            inst.comfy.reference = target.comfyui.reference.clone();
            inst.comfy.version = target.comfyui.version.clone();
            dirty = true;
        }
    }
    if let Some(server) = report.launch.to.clone() {
        inst.server = server;
        dirty = true;
    }
    if dirty {
        crate::commands::instance::save_instance(&inst)?;
    }
    {
        let mut lines = vec![format!("快照 {id}")];
        if report.core.changed {
            lines.push(format!(
                "ComfyUI：{} → {}",
                report.core.from.clone().unwrap_or_else(|| "?".into()),
                report.core.to.clone().unwrap_or_else(|| "?".into())
            ));
        }
        lines.push(format!(
            "节点：+{} −{} ~{}（失败 {}）",
            report.nodes.installed.len(),
            report.nodes.removed.len(),
            report.nodes.switched.len(),
            report.nodes.failed.len()
        ));
        lines.push(format!(
            "包：+{} −{} ~{}（失败 {}）",
            report.packages.installed.len(),
            report.packages.removed.len(),
            report.packages.changed.len(),
            report.packages.failed.len()
        ));
        if report.launch.changed {
            lines.push(format!(
                "启动参数：{}:{} → {}:{}",
                report
                    .launch
                    .from
                    .as_ref()
                    .map(|s| s.listen.clone())
                    .unwrap_or_else(|| "?".into()),
                report.launch.from.as_ref().map(|s| s.port).unwrap_or(0),
                report
                    .launch
                    .to
                    .as_ref()
                    .map(|s| s.listen.clone())
                    .unwrap_or_else(|| "?".into()),
                report.launch.to.as_ref().map(|s| s.port).unwrap_or(0),
            ));
        } else if report.launch.to.is_none() {
            lines.push("启动参数：老快照未记录，保持当前设置".to_string());
        }
        if !report.warnings.is_empty() {
            lines.push(format!("提示：{}", report.warnings.join("；")));
        }
        let bad = !report.nodes.failed.is_empty() || !report.packages.failed.is_empty();
        crate::activity::record(
            &instance_id,
            "snapshot-restore",
            if bad { "warn" } else { "ok" },
            format!(
                "恢复快照{}",
                target
                    .label
                    .as_deref()
                    .map(|l| format!(" · {l}"))
                    .unwrap_or_default()
            ),
            Some(lines.join("\n")),
        );
    }
    Ok(report)
}
