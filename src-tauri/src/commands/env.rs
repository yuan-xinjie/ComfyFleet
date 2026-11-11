use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::commands::instance::{load_instance, save_instance};
use crate::config::{self, InstanceState};
use crate::error::AppError;
use crate::gpu::{self, GpuInfo};
use crate::matrix::{self, PythonDef};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn base_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn has_pip(python: &Path) -> bool {
    base_command(python)
        .arg("-m")
        .arg("pip")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `uv venv` 默认不装 pip（官方行为，不是 bug）。
/// 用 `--seed` 装种子包，再用 ensurepip 兜底，保证用户 `pip install` 一定能用。
fn ensure_pip(python: &Path) -> Result<(), AppError> {
    if has_pip(python) {
        return Ok(());
    }
    let out: Output = base_command(python)
        .arg("-m")
        .arg("ensurepip")
        .arg("--upgrade")
        .output()
        .map_err(|e| AppError::new("ENSUREPIP_FAILED", e.to_string()))?;

    if has_pip(python) {
        return Ok(());
    }
    Err(AppError::new(
        "PIP_MISSING",
        format!(
            "venv 已创建但缺少 pip，ensurepip 也失败（exit={:?}）\n{}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    ))
}

/// M1 的最小前置：只创建 venv（不装 torch / requirements）。
/// 目的：让「进入实例环境终端」在当前里程碑就能真正可用。
#[tauri::command]
pub async fn env_init(instance_id: String) -> Result<String, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞

    tauri::async_runtime::spawn_blocking(move || env_init_impl(instance_id))
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn env_init_impl(instance_id: String) -> Result<String, AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "初始化环境")?;
    let inst = load_instance(&instance_id)?;
    if inst.python.is_existing() {
        return Err(AppError::new(
            "PYTHON_EXISTING_MODE",
            "这个实例用的是已有 Python 环境，不需要（也不会）在这里创建 venv",
        ));
    }
    let root = PathBuf::from(&inst.root_dir);
    let venv = root.join(".venv");
    let python = venv.join("Scripts").join("python.exe");

    if python.exists() {
        // 已存在：只补 pip，不重建（保护用户已装的包）
        ensure_pip(&python)?;
        return Ok(format!("虚拟环境已存在：{}", venv.display()));
    }

    let cache = config::shared_dir().join("uv-cache");
    std::fs::create_dir_all(&cache)?;

    let uv = resolve_uv()?;
    let mut cmd = base_command(&uv);
    cmd.arg("venv")
        .arg("--seed") // 关键：uv 默认不装 pip
        .arg("--python")
        .arg(&inst.python_version)
        .arg("--python-preference")
        .arg("only-managed")
        .arg(&venv)
        .current_dir(&root)
        .env("UV_CACHE_DIR", &cache)
        .env("PYTHONUTF8", "1");

    log::info!(
        "创建 venv：instance={} python={} venv={}",
        inst.id,
        inst.python_version,
        venv.display()
    );

    let output = cmd.output().map_err(|e| {
        AppError::new(
            "UV_NOT_FOUND",
            format!("无法执行 uv（{e}）。请确认 uv 已安装并加入 PATH。"),
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        return Err(AppError::new(
            "UV_FAILED",
            format!(
                "uv venv 失败（exit={:?}）\n{}\n{}",
                output.status.code(),
                stdout,
                stderr
            ),
        ));
    }

    if !python.exists() {
        return Err(AppError::new(
            "VENV_BROKEN",
            format!(
                "uv 返回成功但未找到 python.exe：{}\n{}\n{}",
                venv.display(),
                stdout,
                stderr
            ),
        ));
    }

    ensure_pip(&python)?;

    Ok(format!("虚拟环境已创建（含 pip）：{}", venv.display()))
}

// ---------------------------------------------------------------------------
// 环境计划：把「这台机器能装什么」算清楚，供创建向导渲染
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineOption {
    pub id: String,
    pub label: String,
    pub torch_version: String,
    pub cuda_version: Option<String>,
    /// 这个 torch 对应的 torchvision / torchaudio / torchcodec；没有对照数据时为 null
    pub torchvision: Option<String>,
    pub torchaudio: Option<String>,
    pub torchcodec: Option<String>,
    pub index_url: String,
    pub python_support: Vec<String>,
    /// 驱动报告的 CUDA 版本是否满足该档要求。
    /// **仅用于提示与推荐，绝不阻止安装**——用户选什么就装什么。
    pub driver_ok: bool,
    /// 驱动不满足时的说明（driver_ok=false 时必有）
    pub driver_note: Option<String>,
    pub verified: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvPlan {
    pub gpu: Option<GpuInfo>,
    pub engines: Vec<EngineOption>,
    pub pythons: Vec<PythonDef>,
    pub recommended_engine: String,
    pub recommended_python: String,
    pub warnings: Vec<String>,
    /// 矩阵版本与数据日期：让用户知道"这份兼容性数据有多新"
    pub matrix_version: u32,
    pub matrix_updated_at: String,
}

/// 只在 `verified` / `supported` / `untested` 里取最好的那个 Python
fn pick_python(pythons: &[PythonDef], support: &[String]) -> String {
    fn rank(status: &str) -> u8 {
        match status {
            "verified" => 0,
            "supported" => 1,
            _ => 2,
        }
    }
    let mut best: Option<&PythonDef> = None;
    for p in pythons {
        if !support.iter().any(|v| v == &p.version) {
            continue;
        }
        if best.map_or(true, |b| rank(&p.status) < rank(&b.status)) {
            best = Some(p);
        }
    }
    best.map(|p| p.version.clone())
        .unwrap_or_else(|| "3.12".to_string())
}

#[tauri::command]
pub async fn env_plan() -> Result<EnvPlan, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞

    tauri::async_runtime::spawn_blocking(move || env_plan_impl())
        .await
        .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常：{e}")))?
}

fn env_plan_impl() -> Result<EnvPlan, AppError> {
    let plan = build_plan(&matrix::load()?)?;
    // 排障用：出问题时能直接从日志看到检测到了什么、推荐了什么
    log::info!(
        "环境计划：GPU={} · 驱动报告 CUDA={} · 推荐 {}/Py{}",
        plan.gpu
            .as_ref()
            .map(|g| format!("{}（驱动 {}）", g.name, g.driver_version))
            .unwrap_or_else(|| "未检测到 NVIDIA 显卡".to_string()),
        plan.gpu
            .as_ref()
            .and_then(|g| g.cuda_version.clone())
            .unwrap_or_else(|| "未知".to_string()),
        plan.recommended_engine,
        plan.recommended_python
    );
    Ok(plan)
}

fn build_plan(matrix: &matrix::Matrix) -> Result<EnvPlan, AppError> {
    let gpu = gpu::detect();
    let mut engines: Vec<EngineOption> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for e in &matrix.engines {
        let (driver_ok, driver_note) = match (&gpu, &e.cuda_version) {
            (None, Some(_)) => (false, Some("未检测到 NVIDIA 显卡".to_string())),
            (Some(g), Some(required)) => match g.cuda_version.as_deref() {
                Some(have) if matrix::cuda_at_least(have, required) => (true, None),
                Some(have) => (
                    false,
                    Some(format!("驱动报告 CUDA {have}，该档需要 CUDA {required}")),
                ),
                None => (false, Some("读不到驱动报告的 CUDA 版本".to_string())),
            },
            // CPU 档没有 CUDA 要求
            (_, None) => (true, None),
        };

        let pins = crate::compat::stack_for(&e.torch_version);
        engines.push(EngineOption {
            id: e.id.clone(),
            label: e.label.clone(),
            torch_version: e.torch_version.clone(),
            cuda_version: e.cuda_version.clone(),
            torchvision: pins.as_ref().map(|p| p.torchvision.clone()),
            torchaudio: pins.as_ref().map(|p| p.torchaudio.clone()),
            torchcodec: pins.as_ref().map(|p| p.torchcodec.clone()),
            index_url: matrix::torch_index_url(matrix, e),
            python_support: e.python_support.clone(),
            driver_ok,
            driver_note,
            verified: e.verified,
            note: e.note.clone(),
        });
    }

    // 推荐：优先驱动满足的 NVIDIA 档（矩阵里 cu130 在前，天然取最高）
    let recommended = engines
        .iter()
        .find(|e| e.driver_ok && e.vendor_is_nvidia_like())
        .or_else(|| engines.iter().find(|e| e.driver_ok))
        .ok_or_else(|| AppError::new("NO_ENGINE", "没有可用的计算引擎"))?;

    let recommended_engine = recommended.id.clone();
    let recommended_python = pick_python(&matrix.pythons, &recommended.python_support);

    match &gpu {
        None => {
            warnings.push("未检测到 NVIDIA 显卡，将使用 CPU 推理，速度会明显慢于 GPU".to_string())
        }
        Some(g) => {
            if !recommended.id.starts_with("nvidia") {
                warnings.push(format!(
                    "驱动只支持到 CUDA {}，当前只能用 CPU。升级显卡驱动后可改用 GPU 档",
                    g.cuda_version.clone().unwrap_or_else(|| "未知".to_string())
                ));
            }
        }
    }

    Ok(EnvPlan {
        gpu,
        engines,
        pythons: matrix.pythons.clone(),
        recommended_engine,
        recommended_python,
        warnings,
        matrix_version: matrix.version,
        matrix_updated_at: matrix.updated_at.clone(),
    })
}

impl EngineOption {
    fn vendor_is_nvidia_like(&self) -> bool {
        self.id.starts_with("nvidia")
    }
}

/// 解析 uv：优先安装目录下的 `resources/uv.exe`（安装包内置），
/// 再退回开发目录资源，最后才查系统 PATH。
pub fn resolve_uv() -> Result<PathBuf, AppError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("resources").join("uv.exe"));
    }
    // 仅开发构建允许回退到源码目录；release 里不能把编译机绝对路径带进运行时。
    #[cfg(debug_assertions)]
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("uv.exe"),
    );
    candidates.push(PathBuf::from("uv"));

    for candidate in candidates {
        let usable = if candidate == Path::new("uv") {
            true
        } else {
            candidate.is_file()
        };
        if !usable {
            continue;
        }
        if let Ok(o) = base_command(&candidate).arg("--version").output() {
            if o.status.success() {
                return Ok(candidate);
            }
        }
    }

    Err(AppError::new(
        "UV_NOT_FOUND",
        "找不到 uv。安装包应自带 resources\\uv.exe；若文件被杀软隔离，请恢复它，或把 uv 加入 PATH 后重试。",
    ))
}

/// 安装环境：venv → ComfyUI 本体 → requirements → torch → triton → verify
///
/// 引擎与 Python 一律以 `instance.json` 里登记的值 + 引擎矩阵为准，
/// 不接受前端临时指定，避免绕过驱动校验（例如强行在 12.8 驱动上装 cu130）。
#[tauri::command]
pub async fn env_install(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::state::AppState>,
    instance_id: String,
    on_event: tauri::ipc::Channel<crate::install::InstallEvent>,
) -> Result<crate::install::InstallReport, AppError> {
    use crate::install::{self, InstallOptions};

    let _op = crate::state::OpGuard::acquire(&instance_id, "安装环境")?;
    let inst = load_instance(&instance_id)?;
    if inst.is_remote() {
        return Err(AppError::new(
            "REMOTE_INSTANCE",
            "云端实例没有本地环境可安装：只保存地址，直接打开使用",
        ));
    }
    // 环境安装会改 venv 里的包；实例还在跑时必须先停，不能边跑边换 torch
    if state.runs.lock().contains_key(&instance_id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止实例，再安装 / 更新环境",
        ));
    }
    if inst.python.is_existing() {
        return Err(AppError::new(
            "PYTHON_EXISTING_MODE",
            "This instance uses an existing Python environment. Use Nodes > Install deps to manage it.",
        ));
    }
    let matrix = matrix::load()?;
    let plan = build_plan(&matrix)?;

    let option = plan
        .engines
        .iter()
        .find(|e| e.id == inst.engine)
        .ok_or_else(|| {
            AppError::new(
                "ENGINE_UNKNOWN",
                format!("引擎「{}」不在引擎矩阵里", inst.engine),
            )
        })?;
    // 不再因为驱动不满足就拒绝安装：用户选什么就装什么。
    // 只把风险作为 warn 事件推给界面，让用户自己决定。
    if !option.driver_ok {
        let msg = format!(
            "提示：{}。仍按你的选择继续安装；若装完无法使用 GPU，可改用 CPU 档或升级驱动",
            option
                .driver_note
                .clone()
                .unwrap_or_else(|| "驱动信息不足".into())
        );
        log::warn!("实例 {}：{}", inst.id, msg);
        let _ = on_event.send(install::InstallEvent {
            step: "warn".to_string(),
            step_index: 0,
            step_total: install::STEP_TOTAL,
            phase: "warn".to_string(),
            message: msg,
        });
    }
    if !option
        .python_support
        .iter()
        .any(|v| v == &inst.python_version)
    {
        return Err(AppError::new(
            "PYTHON_UNSUPPORTED",
            format!(
                "引擎「{}」不支持 Python {}（可用：{}）",
                option.label,
                inst.python_version,
                option.python_support.join(" / ")
            ),
        ));
    }

    let settings = state.settings.lock().clone();
    let git = crate::git::resolve(&app, &settings)?;
    let uv = resolve_uv()?;

    let engine_def = matrix
        .engines
        .iter()
        .find(|e| e.id == inst.engine)
        .ok_or_else(|| AppError::new("ENGINE_UNKNOWN", "引擎不在矩阵里"))?;

    let root = PathBuf::from(&inst.root_dir);
    let uv_cache = config::shared_dir().join("uv-cache");
    std::fs::create_dir_all(&uv_cache)?;

    let custom_torch = settings
        .mirrors
        .torch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let index_url = matrix::torch_index_url_with_mirror(&matrix, engine_def, custom_torch);
    let torch_find_links = custom_torch.map(|_| index_url.clone());
    let opts = InstallOptions {
        venv: root.join(".venv"),
        root,
        python_version: inst.python_version.clone(),
        engine_id: engine_def.id.clone(),
        torch_version: engine_def.torch_version.clone(),
        torch_tag: engine_def.index_tag.clone(),
        index_url,
        torch_find_links,
        extra_index_url: settings
            .mirrors
            .pypi
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://pypi.org/simple".to_string()),
        uv_cache,
        uv,
        git: PathBuf::from(&git.path),
        comfy_repo: "https://github.com/comfyanonymous/ComfyUI".to_string(),
        channel: inst.comfy.channel.clone(),
        skip_requirements: false,
    };

    log::info!(
        "开始安装环境：instance={} engine={} python={} torch={} git={} torch_index={}",
        inst.id,
        opts.engine_id,
        opts.python_version,
        opts.torch_version,
        git.path,
        opts.index_url
    );

    // 状态机：Installing → Ready / Error（失败不隐藏，让用户看到红标并能重试）
    {
        let mut cfg = inst.clone();
        cfg.state = InstanceState::Installing;
        save_instance(&cfg).map_err(|e| {
            AppError::new(
                "STATE_SAVE_FAILED",
                format!("无法写入安装状态，安装未开始：{}", e.message),
            )
        })?;
    }

    // 注册取消标志：前端点“取消”后，安装循环会在 500ms 内终止当前子进程树。
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .install_cancels
        .lock()
        .insert(instance_id.clone(), Arc::clone(&cancel));
    let joined = tauri::async_runtime::spawn_blocking(move || {
        install::run(
            &opts,
            &mut |event| {
                let _ = on_event.send(event);
            },
            &cancel,
        )
    })
    .await;
    state.install_cancels.lock().remove(&instance_id);
    let outcome =
        joined.map_err(|e| AppError::new("INSTALL_JOIN", format!("安装线程异常：{e}")))?;

    {
        // 安装期间用户可能改了端口/存储/名称；重新读，只覆盖 state/version，
        // 不能拿安装开始前的 inst 快照整体写回。
        let mut cfg = load_instance(&instance_id).unwrap_or_else(|_| inst.clone());
        match &outcome {
            Ok(report) => {
                cfg.state = InstanceState::Ready;
                if let Some(tag) = &report.tag {
                    cfg.comfy.version = Some(tag.clone());
                    cfg.comfy.reference = tag.clone();
                }
                log::info!(
                    "环境安装完成：instance={} python={} torch={} cuda={}",
                    cfg.id,
                    report.python_version,
                    report.torch_version,
                    report.cuda_available
                );
            }
            Err(e) => {
                cfg.state = InstanceState::Error;
                log::warn!(
                    "环境安装失败：instance={} code={} {}",
                    cfg.id,
                    e.code,
                    e.message
                );
            }
        }
        if let Err(e) = save_instance(&cfg) {
            // 安装结果已经出来但状态写不进去：必须报错，不能返回成功让界面永远停在「安装中」。
            return Err(AppError::new(
                "STATE_SAVE_FAILED",
                format!("安装已结束，但状态保存失败：{}", e.message),
            ));
        }
    }
    match &outcome {
        Ok(report) => crate::activity::record(
            &instance_id,
            "env-install",
            "ok",
            "安装环境",
            Some(format!(
                "Python {}\ntorch {} · CUDA {}",
                report.python_version,
                report.torch_version,
                if report.cuda_available {
                    "可用"
                } else {
                    "不可用"
                }
            )),
        ),
        Err(e) => crate::activity::record(
            &instance_id,
            "env-install",
            "error",
            "安装环境失败",
            Some(format!("{}\n{}", e.code, e.message)),
        ),
    }

    // 装环境是 UV 缓存增长最快的一步；装完立刻按阈值检查，不必等后台轮询。
    let uv_limit = state.settings.lock().storage.uv_cache_max_gb;
    if uv_limit > 0 {
        std::thread::spawn(move || {
            crate::uv_cache::auto_prune(uv_limit);
        });
    }

    outcome
}

/// 取消正在进行的 `env_install`。只置标志，由安装循环负责杀子进程并收尾。
#[tauri::command]
pub fn env_cancel_install(
    state: tauri::State<'_, crate::state::AppState>,
    instance_id: String,
) -> Result<(), AppError> {
    crate::commands::instance::validate_instance_id(&instance_id)?;
    let map = state.install_cancels.lock();
    match map.get(&instance_id) {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            log::info!("已请求取消实例 {instance_id} 的环境安装");
            Ok(())
        }
        None => Err(AppError::new(
            "NOT_INSTALLING",
            "这个实例当前没有正在运行的环境安装",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用本机真实显卡跑一遍计划，验证"检测 → 推荐"这条链路
    #[test]
    fn plan_is_consistent_with_machine() {
        let plan = env_plan_impl().expect("env_plan should succeed");
        assert!(!plan.engines.is_empty());

        let rec = plan
            .engines
            .iter()
            .find(|e| e.id == plan.recommended_engine)
            .expect("recommended engine must exist");
        assert!(rec.driver_ok, "推荐引擎必须是驱动满足的那一档");
        assert!(
            rec.python_support.contains(&plan.recommended_python),
            "recommended python must be supported by the engine"
        );

        // CPU 档永远可选
        assert!(plan.engines.iter().any(|e| e.id == "cpu" && e.driver_ok));

        // 关键回归：本机有 NVIDIA 显卡时，CUDA 版本必须能解析出来。
        // 之前 610.x 驱动的 "CUDA UMD Version:" 解析不到，导致所有 CUDA 档被判"不可用"。
        if let Some(g) = &plan.gpu {
            assert!(
                g.cuda_version.is_some(),
                "有显卡却读不到 CUDA 版本：驱动表头解析有回归（GPU={}，驱动={}）",
                g.name,
                g.driver_version
            );
        }

        // 驱动满足 CUDA 13.0 时必须推荐一条 NVIDIA 轨道（具体 13.0/13.2 取决于驱动）
        if let Some(g) = &plan.gpu {
            if matrix::cuda_at_least(g.cuda_version.as_deref().unwrap_or("0"), "13.0") {
                assert!(
                    plan.recommended_engine.starts_with("nvidia-cu"),
                    "驱动支持 CUDA 13.0 时不该推荐 {}",
                    plan.recommended_engine
                );
            }
        }
    }

    /// 硬约束：驱动不满足只能提示，不能变成安装失败。
    /// 这是用户明确要求的行为，用源码扫描锁住，避免以后又被"帮你拦住"。
    #[test]
    fn driver_mismatch_never_blocks_install() {
        let src = include_str!("env.rs");
        // 拆开写，否则会扫到测试自己
        let needle = concat!("ENGINE_", "UNAVAILABLE");
        assert!(
            !src.contains(needle),
            "驱动检测不得再阻止安装（用户选什么就装什么）"
        );
    }
}
