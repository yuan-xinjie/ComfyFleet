//! 实例启停（M2 第一块）：启动 / 停止 / 日志尾部 / 打开浏览器。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::instance::{load_instance, save_instance};
use crate::config::InstanceState;
use crate::error::AppError;
use crate::runtime::{self, ProcHandle};
use crate::state::{AppState, OpGuard};

/// 就绪等待上限：首次启动要加载模型，可能很慢；超时只记警告，不杀进程。
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(600);
/// 状态写入是 load-modify-save，watch/stop/版本切换可能并发；串行化避免互相覆盖。
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn set_state(id: &str, new: InstanceState, last_started: Option<String>) {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = match load_instance(id) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("更新实例{id} 状态失败：{}", e.message);
            return;
        }
    };
    cfg.state = new;
    if last_started.is_some() {
        cfg.last_started_at = last_started;
    }
    if let Err(e) = save_instance(&cfg) {
        log::warn!("写回实例{id} 状态失败：{}", e.message);
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartOutcome {
    pub port: u16,
    pub url: String,
}

/// ComfyUI 进程真正输出的一行；`seq` 用于消除 `instance_log_state` 的快照序号跳变。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceLogEvent {
    pub instance_id: String,
    pub seq: u64,
    pub line: String,
}

#[tauri::command]
pub async fn instance_start(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<StartOutcome, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        instance_start_impl(handle.clone(), state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("background thread error: {}", e)))?
}

fn instance_start_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<StartOutcome, AppError> {
    let result = start_instance(&app, state.inner(), &instance_id);
    if let Err(e) = &result {
        crate::activity::record(
            &instance_id,
            "start",
            "error",
            "启动失败",
            Some(format!("{}\n{}", e.code, e.message)),
        );
    }
    result
}

/// 启动实例：启动成功后拉起监督线程。
pub fn start_instance(
    app: &AppHandle,
    state: &AppState,
    instance_id: &str,
) -> Result<StartOutcome, AppError> {
    let (outcome, handle, host, epoch) = launch_instance_locked(app, state, instance_id, true)?;
    let id = instance_id.to_string();
    let port = outcome.port;
    let app = app.clone();
    std::thread::spawn(move || supervise(app, id, host, port, handle, epoch));
    Ok(outcome)
}

/// 启动一个进程并把它登记进 runs；`manual` 为 true 时这是一次人工启动，
/// 会清掉停止标记并推进运行代次；自动重启复用原代次，不能清停止标记。
fn launch_instance_locked(
    app: &AppHandle,
    state: &AppState,
    instance_id: &str,
    manual: bool,
) -> Result<(StartOutcome, Arc<ProcHandle>, String, u64), AppError> {
    if state.is_shutting_down() {
        return Err(AppError::new(
            "APP_SHUTTING_DOWN",
            "应用正在退出，不能启动实例",
        ));
    }
    if state.runs.lock().contains_key(instance_id) {
        return Err(AppError::new(
            "ALREADY_RUNNING",
            format!("实例 {instance_id} 已经在运行"),
        ));
    }
    // 检查与插入 runs 不是原子操作；先拿每实例操作锁，挡住并发启动/安装。
    let _op = OpGuard::acquire(instance_id, if manual { "启动" } else { "自动重启" })?;
    if state.is_shutting_down() {
        return Err(AppError::new(
            "APP_SHUTTING_DOWN",
            "应用正在退出，不能启动实例",
        ));
    }
    if !manual && state.stop_requested(instance_id) {
        return Err(AppError::new("STOP_REQUESTED", "实例已被要求停止"));
    }
    if state.runs.lock().contains_key(instance_id) {
        return Err(AppError::new(
            "ALREADY_RUNNING",
            format!("实例 {instance_id} 已经在运行"),
        ));
    }
    let epoch = if manual {
        state.clear_stop(instance_id);
        state.next_run_epoch(instance_id)
    } else {
        state.run_epoch(instance_id).unwrap_or(0)
    };
    let inst = load_instance(instance_id)?;
    if inst.is_remote() {
        return Err(AppError::new(
            "REMOTE_INSTANCE",
            "Remote instance has no local environment",
        ));
    }
    if matches!(
        inst.state,
        crate::config::InstanceState::Installing
            | crate::config::InstanceState::Updating
            | crate::config::InstanceState::Stopping
    ) {
        return Err(AppError::new(
            "INSTANCE_BUSY",
            format!("实例 {} 正在安装/更新/停止，等它结束再启动", inst.name),
        ));
    }

    let root = std::path::PathBuf::from(&inst.root_dir);
    let py = runtime::resolve_python(&inst).ok_or_else(|| {
        let msg = if inst.python.is_existing() {
            format!("Instance {} Python environment is not usable", inst.name)
        } else {
            format!(
                "Instance {} has no venv yet; install the environment first",
                inst.name
            )
        };
        AppError::new("VENV_MISSING", msg)
    })?;
    let comfy = root.join("ComfyUI");
    let main_py = comfy.join("main.py");
    if !main_py.exists() {
        return Err(AppError::new(
            "COMFY_MISSING",
            format!("ComfyUI main.py not found: {}", main_py.display()),
        ));
    }

    let settings = state.settings.lock().clone();

    // 端口冲突直接报错，不自动顺延：端口就是用户定的，改了反而困惑。
    let port = inst.server.port;
    let reserved: Vec<u16> = state.runs.lock().values().map(|h| h.port).collect();
    if reserved.contains(&port) {
        return Err(AppError::new(
            "PORT_IN_USE",
            format!("Port {} is already in use", port),
        ));
    }
    if !runtime::port_free(&inst.server.listen, port) {
        return Err(AppError::new(
            "PORT_IN_USE",
            format!("Port {port} is already in use; change the port or stop the process using it."),
        ));
    }

    let log_path = root.join("logs").join(format!(
        "comfyui-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    // 存储：先把 yaml 与目录准备好，再把参数带进命令行。
    // 这一步失败就不启动——yaml 路径给错会让 ComfyUI 直接崩在启动阶段，报错更难懂。
    let storage_plan = crate::storage::prepare(&inst, &settings)?;
    for warning in &storage_plan.warnings {
        log::warn!("instance {} storage: {}", inst.id, warning);
    }
    let args = runtime::launch_args_with(&inst, &storage_plan.extra_args);
    let python = py.python.clone();
    let launch = runtime::Launch {
        python: python.clone(),
        cwd: comfy,
        args: args.clone(),
        env: runtime::build_env(&inst, &settings, &py),
        log_path: log_path.clone(),
        port,
    };

    let log_app = app.clone();
    let log_instance = inst.id.clone();
    let handle = runtime::spawn_with_sink(
        launch,
        Arc::new(move |line, seq| {
            let _ = log_app.emit(
                "instance-log",
                InstanceLogEvent {
                    instance_id: log_instance.clone(),
                    seq,
                    line: line.to_string(),
                },
            );
        }),
    )?;
    // 记下「本次会话启动过」：之后停了还读日志文件，但重开启动器不会显示上次的旧日志
    state.log_sessions.lock().insert(inst.id.clone());
    log::info!(
        "启动实例 {}：pid={} port={} args={:?} 日志={}",
        inst.id,
        handle.pid,
        port,
        args,
        log_path.display()
    );

    // 写运行记录：万一启动器被强杀，下次启动靠它找回孤儿进程
    let inst_dir = crate::config::instances_dir().join(&inst.id);
    if let Err(e) = runtime::write_record(
        &inst_dir,
        &runtime::RunRecord {
            pid: handle.pid,
            port,
            started_at: chrono::Local::now().to_rfc3339(),
            python: python.to_string_lossy().to_string(),
        },
    ) {
        // 没有运行记录就无法在崩溃后找回进程；宁可启动失败，也不要留一个
        // 占着端口、启动器却不知道的孤儿 ComfyUI。
        let _ = handle.kill_tree();
        return Err(AppError::new(
            "START_RECORD_FAILED",
            format!("启动成功但写运行记录失败，已回滚：{}", e.message),
        ));
    }

    state
        .runs
        .lock()
        .insert(instance_id.to_string(), Arc::clone(&handle));
    set_state(
        instance_id,
        InstanceState::Starting,
        Some(chrono::Local::now().to_rfc3339()),
    );

    // 启动快照：状态没变就不重复记；后台拍，不拖慢启动。
    {
        let snap_inst = inst.clone();
        let snap_settings = settings.clone();
        std::thread::spawn(move || {
            if crate::snapshot::capture_boot(&snap_inst, &snap_settings).is_none() {
                log::debug!(
                    "instance {} state unchanged; skip boot snapshot",
                    snap_inst.id
                );
            }
        });
    }

    let url = format!("http://{}:{port}", runtime::probe_host(&inst.server.listen));
    crate::activity::record(
        instance_id,
        "start",
        "ok",
        format!("启动实例 · {}", inst.name),
        Some(format!("端口 {port}\n{url}")),
    );
    let host = inst.server.listen.clone();
    if state.is_shutting_down() {
        state.runs.lock().remove(instance_id);
        let _ = handle.kill_tree();
        runtime::remove_record(&inst_dir);
        return Err(AppError::new(
            "APP_SHUTTING_DOWN",
            "应用正在退出，已取消启动",
        ));
    }
    Ok((StartOutcome { port, url }, handle, host, epoch))
}

/// 托盘「启动全部实例」：只起那些装好了环境、当前没在跑状
pub fn start_all(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let instances = match crate::commands::instance::instance_list() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("读取实例列表失败：{}", e.message);
            return;
        }
    };
    let mut started = 0;
    for inst in instances {
        if inst.is_remote() {
            continue;
        }
        if matches!(
            inst.state,
            InstanceState::Running | InstanceState::Starting | InstanceState::Stopping
        ) {
            continue;
        }
        match start_instance(app, &state, &inst.id) {
            Ok(_) => started += 1,
            Err(e) => log::warn!("tray: failed to start instance {}: {}", inst.id, e.message),
        }
    }
    log::info!("tray: started {} instances", started);
}

/// 托盘：停止全部实例。
pub fn stop_all(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let mut ids: Vec<String> = state.runs.lock().keys().cloned().collect();
    // 自动重启的退避等待期间 runs 里没有句柄，但状态是 Starting；
    // “停止全部”必须能取消这些待重启实例，否则它们会在几秒后又起来。
    if let Ok(instances) = crate::commands::instance::instance_list() {
        for inst in instances {
            if !inst.is_remote()
                && matches!(
                    inst.state,
                    InstanceState::Starting | InstanceState::Running | InstanceState::Stopping
                )
                && !ids.contains(&inst.id)
            {
                ids.push(inst.id);
            }
        }
    }
    for id in &ids {
        if let Err(e) = stop_instance(&state, id) {
            log::warn!("tray: failed to stop instance {}: {}", id, e.message);
        }
    }
    log::info!("tray: stopped {} instances", ids.len());
}

#[derive(Debug, Clone, Copy)]
struct ExitInfo {
    code: i32,
    ready: bool,
    uptime: Duration,
}

const RESTART_MAX_DELAY: Duration = Duration::from_secs(60);
const RESTART_STABLE: Duration = Duration::from_secs(300);

fn restart_delay(attempt: u32) -> Duration {
    let secs = match attempt {
        0 | 1 => 3,
        2 => 5,
        3 => 10,
        4 => 20,
        5 => 30,
        _ => 60,
    };
    Duration::from_secs(secs).min(RESTART_MAX_DELAY)
}

/// 监督线程只有在“用户没有要求停止、应用没有退出、自己仍是当前代次”时才允许重启。
fn supervision_active(app: &AppHandle, id: &str, epoch: u64) -> bool {
    let Some(st) = app.try_state::<AppState>() else {
        return false;
    };
    !st.is_shutting_down() && !st.stop_requested(id) && st.run_epoch(id) == Some(epoch)
}

/// 等待重启间隔，期间每 250ms 检查一次停止/退出，保证点“停止”能立刻取消。
fn wait_restart(app: &AppHandle, id: &str, epoch: u64, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if !supervision_active(app, id, epoch) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(250)));
    }
}

/// 盯住一个进程直到退出；正常退出返回 Some，人为停止/应用退出/代次过期返回 None。
fn monitor_once(
    app: &AppHandle,
    id: &str,
    host: &str,
    port: u16,
    handle: &Arc<ProcHandle>,
    epoch: u64,
) -> Option<ExitInfo> {
    let started = Instant::now();
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut ready = false;
    let mut late = false;
    let mut port_up_at: Option<Instant> = None;
    let mut ticks: u32 = 0;
    loop {
        std::thread::sleep(POLL);

        if let Some(code) = handle.try_exit() {
            let st = app.try_state::<AppState>()?;
            if st.is_shutting_down() || st.stop_requested(id) || st.run_epoch(id) != Some(epoch) {
                return None;
            }
            {
                let mut runs = st.runs.lock();
                match runs.get(id) {
                    Some(current) if Arc::ptr_eq(current, handle) => {
                        runs.remove(id);
                    }
                    // 已经被停止流程拿走，或者已经被一次新的人工启动替换：旧线程不碰状态。
                    _ => return None,
                }
            }
            // 持有代次锁做收尾：人工启动会先推进代次（拿同一把锁），
            // 避免旧监督线程删掉新进程的运行记录/把新状态写回 Stopped。
            {
                let epochs = st.run_epochs.lock();
                if epochs.get(id) != Some(&epoch) || st.stop_requested(id) {
                    return None;
                }
                runtime::remove_record(&crate::config::instances_dir().join(id));
                if ready {
                    set_state(id, InstanceState::Stopped, None);
                } else {
                    set_state(id, InstanceState::Error, None);
                }
            }
            if ready {
                log::info!("instance {} exited (code {})", id, code);
            } else {
                let tail = handle.tail(25).join("\n");
                log::error!(
                    "instance {} failed to start (exit code {}):\n{}",
                    id,
                    code,
                    tail
                );
            }
            return Some(ExitInfo {
                code,
                ready,
                uptime: started.elapsed(),
            });
        }

        if !ready {
            if runtime::port_open(host, port) {
                let since = *port_up_at.get_or_insert_with(Instant::now);
                let healthy =
                    runtime::http_status(host, port, "/system_stats", Duration::from_secs(3))
                        == Some(200);
                // 端口通了但接口不响应：老版本或者代理。等 20s 仍按就绪处理，
                // 只记警告——不能因为探针失败就说服务没起来。
                if healthy || since.elapsed() > Duration::from_secs(20) {
                    if !healthy {
                        log::warn!("instance {id} health check failed; continuing to wait");
                    }
                    ready = true;
                    set_state(id, InstanceState::Running, None);
                    log::info!(
                        "instance {id} ready at http://{}:{port}",
                        runtime::probe_host(host)
                    );
                }
            } else {
                port_up_at = None;
            }
        }

        if ready {
            ticks += 1;
            if ticks % 5 == 0 {
                if let Some(h) = runtime::probe_health(host, port) {
                    handle.set_health(Some(h));
                }
            }
        }

        if !ready && !late && Instant::now() > deadline {
            // 不杀进程：首次加载模型可能很慢；但必须把“启动超时”显式写进状态，
            // 否则界面永远停在 Starting。之后继续监控，进程退出时仍能收尾/重启。
            late = true;
            set_state(id, InstanceState::Error, None);
            log::warn!(
                "instance {id} still starting after {}s; keeping process alive and monitoring",
                READY_TIMEOUT.as_secs()
            );
            let tail = handle.tail(25).join("\n");
            crate::activity::record(id, "start", "warn", "启动超时，进程仍在运行", Some(tail));
        }
    }
}

/// 一个实例一个监督线程：进程非人为退出 → 按退避重启，永不放弃；
/// 用户停止、应用退出、或新的人工启动会推进代次/停止标记，让旧线程自行退出。
fn supervise(
    app: AppHandle,
    id: String,
    mut host: String,
    mut port: u16,
    mut handle: Arc<ProcHandle>,
    epoch: u64,
) {
    let mut attempt: u32 = 0;
    loop {
        let Some(exit) = monitor_once(&app, &id, &host, port, &handle, epoch) else {
            return;
        };
        if exit.uptime >= RESTART_STABLE {
            attempt = 0;
        }
        let auto_restart = load_instance(&id).map(|i| i.auto_restart).unwrap_or(false);
        if !auto_restart || !supervision_active(&app, &id, epoch) {
            return;
        }

        attempt = attempt.saturating_add(1);
        let tail = handle.tail(25).join("\n");
        let why = if exit.ready {
            "运行中退出"
        } else {
            "启动失败"
        };
        log::warn!(
            "instance {id} {why} (code {}), auto-restart attempt {attempt}",
            exit.code
        );
        crate::activity::record(
            &id,
            "auto-restart",
            "warn",
            format!("{why}，自动重启第 {attempt} 次"),
            Some(format!("退出码 {}\n{tail}", exit.code)),
        );

        loop {
            if !supervision_active(&app, &id, epoch) {
                return;
            }
            let delay = restart_delay(attempt);
            set_state(&id, InstanceState::Starting, None);
            log::info!("instance {id} restarting in {}s", delay.as_secs());
            if !wait_restart(&app, &id, epoch, delay) {
                return;
            }
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            let cfg = match load_instance(&id) {
                Ok(cfg) => cfg,
                Err(_) => return,
            };
            if !cfg.auto_restart {
                set_state(&id, InstanceState::Stopped, None);
                return;
            }
            match launch_instance_locked(&app, &state, &id, false) {
                Ok((outcome, new_handle, new_host, _)) => {
                    log::info!(
                        "instance {id} auto-restarted (pid {}, port {})",
                        new_handle.pid,
                        outcome.port
                    );
                    crate::activity::record(
                        &id,
                        "auto-restart",
                        "ok",
                        format!("自动重启成功（第 {attempt} 次）"),
                        Some(format!("pid {}\n{}", new_handle.pid, outcome.url)),
                    );
                    host = new_host;
                    port = outcome.port;
                    handle = new_handle;
                    break;
                }
                Err(e) => {
                    log::warn!(
                        "instance {id} auto-restart attempt {attempt} failed: {}",
                        e.message
                    );
                    crate::activity::record(
                        &id,
                        "auto-restart",
                        "error",
                        format!("自动重启失败（第 {attempt} 次）"),
                        Some(format!("{}\n{}", e.code, e.message)),
                    );
                    if matches!(
                        e.code.as_str(),
                        "ALREADY_RUNNING" | "STOP_REQUESTED" | "APP_SHUTTING_DOWN"
                    ) {
                        return;
                    }
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
}

#[tauri::command]
pub async fn instance_stop(app: tauri::AppHandle, instance_id: String) -> Result<(), AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        instance_stop_impl(state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("background thread error: {}", e)))?
}

fn instance_stop_impl(state: State<'_, AppState>, instance_id: String) -> Result<(), AppError> {
    stop_instance(state.inner(), &instance_id)
}

/// 停止逻辑本体（托盘也用）
pub fn stop_instance(state: &AppState, instance_id: &str) -> Result<(), AppError> {
    // 先落“人为停止”标记，再拿操作锁：即使此刻自动重启正拿着锁，这个标记也能让
    // 监督线程在下一次检查时退出，不会出现“点了停止却又被拉起来”。
    // 即使停止失败也保留标记，直到用户再次手动启动。
    state.request_stop(instance_id);
    let _op = OpGuard::acquire(instance_id, "停止")?;
    stop_instance_unlocked(state, instance_id)
}

/// 调用方已经持有 OpGuard 时使用；自身不再拿锁，避免同步应用/删除流程自锁。
pub(crate) fn stop_instance_unlocked(state: &AppState, instance_id: &str) -> Result<(), AppError> {
    state.request_stop(instance_id);
    let handle = state.runs.lock().remove(instance_id);
    let inst_dir = crate::config::instances_dir().join(instance_id);
    match handle {
        Some(h) => {
            set_state(instance_id, InstanceState::Stopping, None);
            let kill_result = h.kill_tree();
            let mut exited = false;
            for _ in 0..25 {
                if h.try_exit().is_some() {
                    exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if kill_result.is_err() || !exited {
                // 不能假装已停止：进程可能还占着端口和文件。把句柄放回去，
                // 状态标 Error，让用户/托盘可以重试停止。
                state.runs.lock().insert(instance_id.to_string(), h.clone());
                set_state(instance_id, InstanceState::Error, None);
                let detail = kill_result
                    .err()
                    .map(|e| e.message)
                    .unwrap_or_else(|| "进程在 5 秒内没有退出".to_string());
                crate::activity::record(
                    instance_id,
                    "stop",
                    "error",
                    "停止失败",
                    Some(detail.clone()),
                );
                return Err(AppError::new("STOP_FAILED", detail));
            }
            set_state(instance_id, InstanceState::Stopped, None);
            runtime::remove_record(&inst_dir);
            log::info!(
                "shutdown: stopping instance {} (pid {})",
                instance_id,
                h.pid
            );
            crate::activity::record(
                instance_id,
                "stop",
                "ok",
                "stopped instance",
                Some(format!("pid {}", h.pid)),
            );
            Ok(())
        }
        None => {
            set_state(instance_id, InstanceState::Stopped, None);
            runtime::remove_record(&inst_dir);
            Ok(())
        }
    }
}

/// 读取实例最新日志的尾部（最多 limit 行）。
fn read_latest_log(instance_id: &str, limit: usize) -> Result<Vec<String>, AppError> {
    let inst = load_instance(instance_id)?;
    let dir = std::path::PathBuf::from(&inst.root_dir).join("logs");
    let newest = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "log").unwrap_or(false))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((meta.modified().ok()?, e.path()))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    let Some(path) = newest else {
        return Ok(Vec::new());
    };
    // 只读文件尾部，不能把几 GB 的日志整个读进内存。
    const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
    let mut text = String::new();
    if let Ok(mut file) = std::fs::File::open(&path) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(MAX_READ_BYTES);
        if start > 0 {
            use std::io::{Seek, SeekFrom};
            let _ = file.seek(SeekFrom::Start(start));
        }
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = file.take(MAX_READ_BYTES).read_to_end(&mut buf);
        text = String::from_utf8_lossy(&buf).to_string();
        if start > 0 {
            // 从中间开始时第一行可能是半截，丢掉。
            if let Some(pos) = text.find('\n') {
                text.drain(0..=pos);
            }
        }
    }
    let mut all: Vec<String> = text.lines().map(|l| runtime::strip_ansi(l)).collect();
    if all.len() > limit {
        all.drain(0..all.len() - limit);
    }
    Ok(all)
}

/// 日志快照：尾部+ 当前序号。界面只取一次，之后面`instance-log` 事件增量追加。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceLogState {
    pub lines: Vec<String>,
    pub seq: u64,
}

#[tauri::command]
pub async fn instance_log_state(
    app: tauri::AppHandle,
    instance_id: String,
    lines: Option<usize>,
) -> Result<InstanceLogState, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        instance_log_state_impl(state, instance_id, lines)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("background thread error: {}", e)))?
}

fn instance_log_state_impl(
    state: State<'_, AppState>,
    instance_id: String,
    lines: Option<usize>,
) -> Result<InstanceLogState, AppError> {
    // 前端只能要一个有限的尾部；避免 IPC 返回几十万行把 WebView 卡死。
    let limit = lines.unwrap_or(300).clamp(1, 2000);
    {
        let runs = state.runs.lock();
        if let Some(h) = runs.get(&instance_id) {
            let (lines, seq) = h.tail_with_seq(limit);
            return Ok(InstanceLogState { lines, seq });
        }
    }
    if !state.log_sessions.lock().contains(&instance_id) {
        return Ok(InstanceLogState {
            lines: Vec::new(),
            seq: 0,
        });
    }
    Ok(InstanceLogState {
        lines: read_latest_log(&instance_id, limit)?,
        seq: 0,
    })
}

/// 所有运行中实例的显存/队列情况，供前端轮询。
#[tauri::command]
pub async fn instance_health(
    app: tauri::AppHandle,
) -> Result<HashMap<String, runtime::Health>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        instance_health_impl(state)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("background thread error: {}", e)))?
}

fn instance_health_impl(
    state: State<'_, AppState>,
) -> Result<HashMap<String, runtime::Health>, AppError> {
    let runs = state.runs.lock();
    Ok(runs
        .iter()
        .filter_map(|(id, h)| h.health().map(|health| (id.clone(), health)))
        .collect())
}

/// 生成要交给系统浏览器的地址。监听地址必须重新校验：instance.json 可能来自
/// 集群同步或手工编辑，不能让 `&`、`|` 之类的字符进入命令行。
pub(crate) fn open_url_for_instance(
    inst: &crate::config::InstanceConfig,
) -> Result<String, AppError> {
    if inst.is_remote() {
        return inst
            .remote
            .as_ref()
            .map(|r| r.url())
            .ok_or_else(|| AppError::new("REMOTE_INVALID", "remote instance URL is invalid"));
    }
    let (listen, port) =
        crate::commands::instance::validate_server(&inst.server.listen, inst.server.port)?;
    let host = runtime::probe_host(&listen);
    Ok(format!("http://{host}:{port}"))
}

#[tauri::command]
pub fn instance_open_url(app: AppHandle, instance_id: String) -> Result<String, AppError> {
    let inst = load_instance(&instance_id)?;
    let url = open_url_for_instance(&inst)?;
    // 走 opener 插件而不是 cmd /C start：URL 里的 & 不会被 cmd.exe 当成命令分隔符。
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url.clone(), None::<&str>)
        .map_err(|e| AppError::new("OPEN_FAILED", format!("打开浏览器失败：{e}")))?;
    Ok(url)
}

/// 退出应用时清理所有子进程，避免留下孤儿ComfyUI 占端口
pub fn shutdown_all(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        // 先挡住所有自动重启，再杀进程；否则退出过程中监督线程可能又拉起 ComfyUI。
        state.begin_shutdown();
        // 环境终端是独立的 ConPTY，不在 runs 里；退出应用时必须一起杀。
        {
            let mut ptys = state.ptys.lock();
            for (_, mut session) in ptys.drain() {
                session.kill();
            }
        }
        let handles: Vec<(String, Arc<ProcHandle>)> = {
            let mut runs = state.runs.lock();
            runs.drain().collect()
        };
        for (id, h) in handles {
            log::info!("shutdown: stopping instance {} (pid {})", id, h.pid);
            let _ = h.kill_tree();
            runtime::remove_record(&crate::config::instances_dir().join(&id));
        }
    }
}

/// 启动时清理上次被强杀留下的孤儿进程。
///
/// 顺序很重要：先按 `run.json` 锁 pid → **校验身份** → 杀进程 → 删记录 → 状态归位。
/// 校验身份这一步不能省：pid 会被系统复用，重启后 pid 可能指向完全无关的程序。
pub fn recover_orphans() {
    let dir = crate::config::instances_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let inst_dir = entry.path();
        let id = entry.file_name().to_string_lossy().to_string();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }

        let Some(record) = runtime::read_record(&inst_dir) else {
            // 记录文件存在但解析失败：不能当成没记录就过去，否则孤儿永远清不掉。
            let path = runtime::record_path(&inst_dir);
            if path.exists() {
                log::warn!(
                    "实例{id} 的运行记录无法解析（{}），已删除该记录",
                    path.display()
                );
                runtime::remove_record(&inst_dir);
            }
            // 没有运行记录，但状态停在 running/starting：说明进程早没了，状态没来得及写。
            if let Ok(mut cfg) = load_instance(&id) {
                match cfg.state {
                    InstanceState::Running | InstanceState::Starting | InstanceState::Stopping => {
                        // 没有记录不代表进程一定不存在（可能在写记录前被强杀）。
                        // 不能谎报 Stopped，否则用户再启动会撞端口；标记 Error 让用户先确认。
                        cfg.state = InstanceState::Error;
                        let _ = save_instance(&cfg);
                        log::warn!("实例{id} 状态停在运行中但运行记录缺失，已标记 Error");
                    }
                    InstanceState::Installing | InstanceState::Updating => {
                        // 上次安装/更新被强杀：venv 可能半写，不能谎报 Ready/Stopped。
                        cfg.state = InstanceState::Error;
                        let _ = save_instance(&cfg);
                        log::warn!("实例{id} 上次安装/更新中断，已标记 Error");
                    }
                    _ => {}
                }
            }
            continue;
        };

        let python = std::path::PathBuf::from(&record.python);
        if record.pid > 0 && runtime::verify_process(record.pid, &python) {
            log::warn!(
                "legacy record found for pid {}, port {}; stopping it",
                record.pid,
                record.port
            );
            if let Err(e) = runtime::kill_pid_tree(record.pid) {
                log::warn!("shutdown: failed to stop instance {}: {}", id, e.message);
            }
            std::thread::sleep(Duration::from_millis(400));
        } else {
            log::info!(
                "instance {id} record has no live process (pid {})",
                record.pid
            );
        }

        runtime::remove_record(&inst_dir);
        if let Ok(mut cfg) = load_instance(&id) {
            cfg.state = if matches!(
                cfg.state,
                InstanceState::Installing | InstanceState::Updating
            ) {
                // 安装时被强杀，文件可能不完整；保留可重试的错误态，而不是伪装成 stopped。
                InstanceState::Error
            } else {
                InstanceState::Stopped
            };
            let _ = save_instance(&cfg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{open_url_for_instance, restart_delay};
    use crate::config::{ComfyRef, InstanceConfig, InstanceState, ServerOpts};
    use std::time::Duration;

    fn sample() -> InstanceConfig {
        InstanceConfig {
            id: "test0001".to_string(),
            name: "Audit".to_string(),
            root_dir: "C:\\audit\\instance".to_string(),
            engine: "cpu".to_string(),
            engine_detail: None,
            python_version: "3.12".to_string(),
            python: Default::default(),
            comfy: ComfyRef {
                channel: "stable".to_string(),
                reference: "v0.43.1".to_string(),
                commit: String::new(),
                version: Some("v0.43.1".to_string()),
            },
            server: ServerOpts {
                listen: "127.0.0.1".to_string(),
                port: 8188,
                cors_origin: None,
                extra_args: Vec::new(),
            },
            storage: Default::default(),
            env: Default::default(),
            overrides: Default::default(),
            node_deps_auto: None,
            auto_restart: false,
            kind: "local".to_string(),
            remote: None,
            imported: false,
            state: InstanceState::Stopped,
            created_at: "2026-10-28T00:00:00Z".to_string(),
            last_started_at: None,
        }
    }

    #[test]
    fn restart_backoff_grows_then_caps() {
        assert_eq!(restart_delay(1), Duration::from_secs(3));
        assert_eq!(restart_delay(3), Duration::from_secs(10));
        assert_eq!(restart_delay(5), Duration::from_secs(30));
        assert_eq!(restart_delay(99), Duration::from_secs(60));
    }

    #[test]
    fn open_url_rejects_non_ip_listen() {
        let mut inst = sample();
        inst.server.listen = "127.0.0.1&calc&rem".to_string();
        assert!(open_url_for_instance(&inst).is_err());
        inst.server.listen = "127.0.0.1".to_string();
        assert_eq!(
            open_url_for_instance(&inst).unwrap(),
            "http://127.0.0.1:8188"
        );
        inst.server.listen = "0.0.0.0".to_string();
        assert_eq!(
            open_url_for_instance(&inst).unwrap(),
            "http://127.0.0.1:8188"
        );
    }
}
