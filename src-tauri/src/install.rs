//! 环境安装执行器：venv → ComfyUI 本体 → requirements → torch 三元组 → triton → verify。
//!
//! 与 Tauri 解耦（不持有 AppHandle、不使用 State），命令层只负责把进度转发成事件。
//! 这样同一套逻辑能直接在单元测试 / 命令行里跑真装机。
//!
//! 设计约束：
//! - 每一步幂等：已完成的步骤可跳过（重复点击不会把 venv 删了重装）。
//! - 子进程输出逐行流式转发，失败时把最后 40 行作为错误正文。
//! - torch 版本只从引擎矩阵来，本模块不接受“任意版本”参数。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::error::AppError;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const STEP_TOTAL: u8 = 6;
/// 安装步骤没有总时限：慢网络下 torch/CUDA 可能下载很久，用户随时可以点取消。
/// 网络完全无响应时由 uv/pip 自己的 HTTP 超时报错，而不是启动器按总耗时杀进程。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Venv,
    Torch,
    Triton,
    Comfy,
    Requirements,
    Verify,
}

impl Step {
    pub fn index(self) -> u8 {
        match self {
            Step::Venv => 1,
            Step::Comfy => 2,
            Step::Requirements => 3,
            Step::Torch => 4,
            Step::Triton => 5,
            Step::Verify => 6,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Step::Venv => "venv",
            Step::Torch => "torch",
            Step::Triton => "triton",
            Step::Comfy => "comfy",
            Step::Requirements => "requirements",
            Step::Verify => "verify",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Step::Venv => "创建虚拟环境",
            Step::Torch => "安装 torch 三件套",
            Step::Triton => "安装 triton",
            Step::Comfy => "下载 ComfyUI 本体",
            Step::Requirements => "安装 ComfyUI 依赖",
            Step::Verify => "校验环境",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallEvent {
    pub step: String,
    pub step_index: u8,
    pub step_total: u8,
    /// start | log | done | warn
    pub phase: String,
    pub message: String,
}

impl InstallEvent {
    fn start(step: Step, message: impl Into<String>) -> Self {
        Self {
            step: step.code().to_string(),
            step_index: step.index(),
            step_total: STEP_TOTAL,
            phase: "start".into(),
            message: message.into(),
        }
    }

    fn log(step: Step, message: impl Into<String>) -> Self {
        Self {
            step: step.code().to_string(),
            step_index: step.index(),
            step_total: STEP_TOTAL,
            phase: "log".into(),
            message: message.into(),
        }
    }

    fn done(step: Step, message: impl Into<String>) -> Self {
        Self {
            step: step.code().to_string(),
            step_index: step.index(),
            step_total: STEP_TOTAL,
            phase: "done".into(),
            message: message.into(),
        }
    }

    fn warn(step: Step, message: impl Into<String>) -> Self {
        Self {
            step: step.code().to_string(),
            step_index: step.index(),
            step_total: STEP_TOTAL,
            phase: "warn".into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub python_version: String,
    pub torch_version: String,
    pub cuda_available: bool,
    pub cuda_device: Option<String>,
    pub comfy_dir: String,
    pub engine_id: String,
    pub tag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub root: PathBuf,
    pub venv: PathBuf,
    pub python_version: String,
    pub engine_id: String,
    /// 来自引擎矩阵，调用方不得自行指定。
    pub torch_version: String,
    /// 版本 local label（cu130 / cpu），不能从自定义镜像 URL 里猜。
    pub torch_tag: String,
    /// torch 索引；可能是官方源，也可能是用户配置的镜像。
    pub index_url: String,
    /// 自定义镜像同时作为 find-links：兼容阿里云等没有 PEP 503 目录的扁平 wheel 镜像。
    pub torch_find_links: Option<String>,
    /// 兜底索引：torch 的 Python 依赖并不全在 pytorch 索引里。
    pub extra_index_url: String,
    pub uv_cache: PathBuf,
    pub uv: PathBuf,
    pub git: PathBuf,
    pub comfy_repo: String,
    /// stable 时按最新版 tag 检出；dev 时用默认分支。
    pub channel: String,
    pub skip_requirements: bool,
}

impl InstallOptions {
    fn python_exe(&self) -> PathBuf {
        self.venv.join("Scripts").join("python.exe")
    }

    fn comfy_dir(&self) -> PathBuf {
        self.root.join("ComfyUI")
    }
}

fn base(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// 把子进程输出按 `\n` / `\r\n` 切成日志行。
///
/// `\r` 单独出现表示终端里“回到行首覆盖当前行”，pip/tqdm 的进度条就是这样
/// 刷新的。这里不把每次刷新都变成一条新日志，只保留该行最后一次内容；否则
/// 一次大文件下载会灌出几千行 `1% / 2% / 3%…`。真正的换行（`\n` / `\r\n`）
/// 才产生新日志行。
fn pump<R: std::io::Read>(mut reader: R, tx: std::sync::mpsc::Sender<String>) {
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last_progress = std::time::Instant::now();
    let progress_interval = std::time::Duration::from_secs(1);

    let emit = |segment: &[u8]| -> bool {
        let text = String::from_utf8_lossy(segment).trim().to_string();
        if text.is_empty() {
            return true;
        }
        tx.send(text).is_ok()
    };

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        pending.extend_from_slice(&buf[..n]);

        let mut consumed = 0usize;
        let mut i = 0usize;
        while i < pending.len() {
            match pending[i] {
                b'\n' => {
                    if !emit(&pending[consumed..i]) {
                        return;
                    }
                    consumed = i + 1;
                    i = consumed;
                }
                b'\r' => {
                    // 缓冲末尾的 \r 可能是被拆开的 \r\n，留到下一次读再判断。
                    if i + 1 >= pending.len() {
                        break;
                    }
                    if pending[i + 1] == b'\n' {
                        if !emit(&pending[consumed..i]) {
                            return;
                        }
                        consumed = i + 2;
                        i = consumed;
                    } else {
                        // 单独的 \r：当前行旧内容被覆盖。控制台需要看进度，
                        // 每秒最多送一次；前端会把连续进度更新到同一行。
                        if last_progress.elapsed() >= progress_interval {
                            if !emit(&pending[consumed..i]) {
                                return;
                            }
                            last_progress = std::time::Instant::now();
                        }
                        consumed = i + 1;
                        i = consumed;
                    }
                }
                _ => i += 1,
            }
        }
        if consumed > 0 {
            pending.drain(..consumed);
        }
    }
    // EOF 时最后一行可能没有换行；也可能以 \r 结束（最后一条进度）。
    let end = if pending.last() == Some(&b'\r') {
        pending.len() - 1
    } else {
        pending.len()
    };
    let _ = emit(&pending[..end]);
}

/// 跑一个子进程，逐行流式上报，返回最后若干行用于错误定位。
fn cancelled(step: Step) -> AppError {
    AppError::new(
        "INSTALL_CANCELLED",
        format!(
            "已取消「{}」；已终止当前子进程，可随时重新安装",
            step.label()
        ),
    )
}

/// 处理已存在、但没有 main.py 的 ComfyUI 目录。
///
/// 新建实例会预建一个空 ComfyUI 占位目录，这种目录必须直接移除；只有里面
/// 确实有用户文件时才改名成 `ComfyUI.broken-*` 保留。
fn replace_incomplete_comfy(
    comfy_dir: &Path,
    staging: &Path,
) -> Result<Option<PathBuf>, AppError> {
    if !comfy_dir.exists() {
        return Ok(None);
    }
    let empty = std::fs::read_dir(comfy_dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false);
    if empty {
        std::fs::remove_dir(comfy_dir).map_err(|e| {
            AppError::new(
                "COMFY_REPLACE_FAILED",
                format!(
                    "空的 ComfyUI 占位目录无法删除，可能被占用：{}：{e}\n临时 clone 在 {}",
                    comfy_dir.display(),
                    staging.display()
                ),
            )
        })?;
        return Ok(None);
    }
    // 有内容的旧目录不能静默删除，改名保留给用户排查/恢复。
    let dest = comfy_dir.with_file_name(format!(
        "ComfyUI.broken-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::rename(comfy_dir, &dest).map_err(|e| {
        AppError::new(
            "COMFY_REPLACE_FAILED",
            format!(
                "ComfyUI 目录已存在且无法移走（可能被占用）：{}：{e}\n临时 clone 在 {}",
                comfy_dir.display(),
                staging.display()
            ),
        )
    })?;
    log::warn!("旧 ComfyUI 目录已移到 {}", dest.display());
    Ok(Some(dest))
}

fn stream(
    cmd: &mut Command,
    step: Step,
    emit: &mut dyn FnMut(InstallEvent),
    cancel: &AtomicBool,
) -> Result<Vec<String>, AppError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled(step));
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let program = cmd.get_program().to_string_lossy().to_string();
    // 把完整命令行打进控制台：失败时用户复制日志就能复现，不用猜启动器跑了什。
    let command_line = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    emit(InstallEvent::log(
        step,
        format!(
            "$ {program}{}{command_line}",
            if command_line.is_empty() { "" } else { " " }
        ),
    ));
    let mut child = cmd.spawn().map_err(|e| {
        AppError::new(
            "SPAWN_FAILED",
            format!("无法启动 {program}（{e}）。请确认它已安装且可从 PATH 访问。"),
        )
    })?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(mut stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(&mut stdout, tx));
    }
    if let Some(mut stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(&mut stderr, tx));
    }
    drop(tx);

    let mut tail: VecDeque<String> = VecDeque::with_capacity(40);
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                let line = line.trim_end().to_string();
                if line.trim().is_empty() {
                    continue;
                }
                emit(InstallEvent::log(step, line.clone()));
                if tail.len() == 40 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            Err(RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = crate::runtime::kill_pid_tree(child.id());
                    let _ = child.wait();
                    return Err(cancelled(step));
                }
                // 子进程已退出但管道被孙进程占着时，先收干已缓冲的输出再收尾。
                // 不再按总耗时终止：慢网络也让用户自己决定是否取消。
                if child.try_wait().ok().flatten().is_some() {
                    while let Ok(line) = rx.try_recv() {
                        let line = line.trim_end().to_string();
                        if !line.trim().is_empty() {
                            emit(InstallEvent::log(step, line.clone()));
                            if tail.len() == 40 {
                                tail.pop_front();
                            }
                            tail.push_back(line);
                        }
                    }
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // 管道断开不等于进程结束（它可能关闭 stdout 后继续跑）；这里继续等，
    // 只有用户取消才终止进程树。
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = crate::runtime::kill_pid_tree(child.id());
                    let _ = child.wait();
                    return Err(cancelled(step));
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(AppError::new("WAIT_FAILED", e.to_string())),
        }
    };

    if !status.success() {
        let code = format!("{}_FAILED", step.code().to_uppercase());
        return Err(AppError::new(
            &code,
            format!(
                "{} 失败（exit={:?}）\n{}",
                step.label(),
                status.code(),
                tail.into_iter().collect::<Vec<_>>().join("\n")
            ),
        ));
    }
    Ok(tail.into_iter().collect())
}

fn has_pip(python: &Path) -> bool {
    crate::proc::run(
        python,
        &["-m", "pip", "--version"],
        None,
        &std::collections::BTreeMap::new(),
    )
    .map(|o| o.code == 0)
    .unwrap_or(false)
}

fn latest_stable_tag(git: &Path, repo: &str) -> Option<String> {
    let mut env = std::collections::BTreeMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    let out = crate::proc::run(git, &["ls-remote", "--tags", "--refs", repo], None, &env).ok()?;
    if out.code != 0 {
        return None;
    }
    let text = out.stdout;
    let mut best: Option<(Vec<u32>, String)> = None;
    for line in text.lines() {
        let Some((_sha, reference)) = line.split_once('\t') else {
            continue;
        };
        let Some(tag) = reference.strip_prefix("refs/tags/") else {
            continue;
        };
        let numeric = tag.trim_start_matches('v');
        let parts: Vec<u32> = numeric
            .split('.')
            .map(|p| p.parse::<u32>().unwrap_or(u32::MAX))
            .collect();
        if parts.len() != 3 || parts.iter().any(|&p| p == u32::MAX) {
            continue;
        }
        if best.as_ref().map_or(true, |(cur, _)| parts > *cur) {
            best = Some((parts, tag.to_string()));
        }
    }
    best.map(|(_, tag)| tag)
}

fn git_command(git: &Path) -> Command {
    let mut cmd = base(git);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// 把版本锁定写成临时 constraints 文件，返回路径；装完由调用方删除。
/// local label 来自引擎矩阵，不能从镜像 URL 的最后一段去猜。
/// 只有网络/解压类失败才值得换官方源重试；版本不存在、参数错误等重试只会浪费十几分钟。
fn is_transport_failure(err: &AppError) -> bool {
    let text = format!("{} {}", err.code, err.message).to_lowercase();
    [
        "timed out",
        "timeout",
        "transport",
        "error decoding response body",
        "failed to unzip wheel",
        "connection reset",
        "connection aborted",
        "connection closed",
        "broken pipe",
        "temporarily unavailable",
        "http/2",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn index_tag(opts: &InstallOptions) -> &str {
    let tag = opts.torch_tag.trim();
    if tag.is_empty() {
        "cpu"
    } else {
        tag
    }
}

fn write_constraints(
    pin: &crate::compat::StackPin,
    tag: Option<&str>,
) -> Result<PathBuf, AppError> {
    let path = std::env::temp_dir().join(format!(
        "comfyfleet-torch-{}-{}.txt",
        std::process::id(),
        chrono::Local::now().timestamp_millis()
    ));
    std::fs::write(&path, crate::compat::constraints_text(pin, tag))?;
    Ok(path)
}

pub fn run(
    opts: &InstallOptions,
    emit: &mut dyn FnMut(InstallEvent),
    cancel: &AtomicBool,
) -> Result<InstallReport, AppError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled(Step::Venv));
    }
    let python = opts.python_exe();
    let comfy_dir = opts.comfy_dir();
    let mut tag: Option<String> = None;

    // ---- 1/5 venv -------------------------------------------------------
    emit(InstallEvent::start(
        Step::Venv,
        format!("Python {}", opts.python_version),
    ));
    if python.exists() {
        emit(InstallEvent::log(
            Step::Venv,
            "venv 已存在，复用（不重建，保护已装包）",
        ));
    } else {
        std::fs::create_dir_all(&opts.uv_cache)?;
        let mut cmd = base(&opts.uv);
        cmd.args(["venv", "--seed", "--python"])
            .arg(&opts.python_version)
            .args(["--python-preference", "only-managed"])
            .arg(&opts.venv)
            .current_dir(&opts.root)
            .env("UV_CACHE_DIR", &opts.uv_cache)
            .env("PYTHONUTF8", "1")
            .env("UV_HTTP_TIMEOUT", "600")
            .env("UV_HTTP_CONNECT_TIMEOUT", "30")
            .env("UV_HTTP_RETRIES", "5");
        stream(&mut cmd, Step::Venv, emit, cancel)?;
    }
    if !python.exists() {
        return Err(AppError::new(
            "VENV_BROKEN",
            format!("venv 创建后仍找不到 python.exe：{}", python.display()),
        ));
    }
    if !has_pip(&python) {
        emit(InstallEvent::log(
            Step::Venv,
            "缺少 pip，执行 ensurepip 兜底",
        ));
        let mut cmd = base(&python);
        cmd.args(["-m", "ensurepip", "--upgrade"]);
        stream(&mut cmd, Step::Venv, emit, cancel)?;
        if !has_pip(&python) {
            return Err(AppError::new(
                "PIP_MISSING",
                "venv 缺少 pip，ensurepip 也没装上；用户将无法 pip install",
            ));
        }
    }
    emit(InstallEvent::done(
        Step::Venv,
        format!("{}", opts.venv.display()),
    ));

    // ---- 2/5 ComfyUI 本体 ------------------------------------------------
    emit(InstallEvent::start(
        Step::Comfy,
        format!("{} 通道", opts.channel),
    ));
    if comfy_dir.join("main.py").exists() {
        emit(InstallEvent::log(Step::Comfy, "ComfyUI 已存在，跳过下载"));
    } else {
        if opts.channel == "stable" {
            match latest_stable_tag(&opts.git, &opts.comfy_repo) {
                Some(t) => {
                    emit(InstallEvent::log(
                        Step::Comfy,
                        format!("最新稳定版 tag：{t}"),
                    ));
                    tag = Some(t);
                }
                None => emit(InstallEvent::warn(
                    Step::Comfy,
                    "取不到 tag 列表，改用默认分支",
                )),
            }
        }
        // 不能直接 clone 到 ComfyUI：失败会留下半目录，下次 clone 撞“已存在”，
        // 永远重试不了。先 clone 到 staging，成功后再替换。
        let staging = comfy_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(
                "ComfyUI.install-{}-{}",
                std::process::id(),
                chrono::Local::now().format("%Y%m%d%H%M%S")
            ));
        let _ = std::fs::remove_dir_all(&staging);
        let mut cmd = git_command(&opts.git);
        cmd.arg("clone").arg("--progress").arg("--depth").arg("1");
        if let Some(t) = &tag {
            cmd.arg("--branch").arg(t);
        }
        cmd.arg(&opts.comfy_repo).arg(&staging);
        if let Err(e) = stream(&mut cmd, Step::Comfy, emit, cancel) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        if !staging.join("main.py").exists() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(AppError::new(
                "COMFY_MISSING",
                format!("clone 完成但找不到 main.py：{}", staging.display()),
            ));
        }
        let backup = replace_incomplete_comfy(&comfy_dir, &staging)?;
        if let Err(e) = std::fs::rename(&staging, &comfy_dir) {
            // 新目录没换上去：把旧目录恢复回来，别把实例留在“两边都没有”的状态。
            if let Some(backup) = &backup {
                if std::fs::rename(backup, &comfy_dir).is_ok() {
                    log::warn!("替换失败，已把旧 ComfyUI 目录移回 {}", comfy_dir.display());
                }
            }
            return Err(AppError::new(
                "COMFY_REPLACE_FAILED",
                format!(
                    "安装目录替换失败：{}：{e}\n临时 clone 留在 {}",
                    comfy_dir.display(),
                    staging.display()
                ),
            ));
        }
    }
    emit(InstallEvent::done(
        Step::Comfy,
        comfy_dir.display().to_string(),
    ));

    // ---- 3/5 requirements ------------------------------------------------
    emit(InstallEvent::start(
        Step::Requirements,
        "ComfyUI/requirements.txt",
    ));
    let requirements = comfy_dir.join("requirements.txt");
    if opts.skip_requirements {
        emit(InstallEvent::warn(
            Step::Requirements,
            "已要求跳过（测试用）",
        ));
    } else if !requirements.exists() {
        emit(InstallEvent::warn(
            Step::Requirements,
            "没有 requirements.txt，跳过",
        ));
    } else {
        // torch 放到最后安装，这里先用不带 +cu 的版本约束，让 requirements 的
        // torch 传递依赖能解析到 CPU 轮子；稍后 torch 步骤会替换成 CUDA 轮子。
        // 这样 ComfyUI 本体和依赖先落地，最后再锁定 GPU 版 torch，避免被依赖改写。
        let stack = crate::compat::stack_for(&opts.torch_version);
        let constraints = match &stack {
            Some(pin) => Some(write_constraints(pin, None)?),
            None => None,
        };
        // 镜像没同步到新版本包时自动退回官 PyPI：新版本 requirements 常引到刚发的。
        let candidates = crate::pip::index_candidates(Some(opts.extra_index_url.as_str()));
        let mut last_err: Option<AppError> = None;
        for (n, index) in candidates.iter().enumerate() {
            if n > 0 {
                emit(InstallEvent::warn(
                    Step::Requirements,
                    format!("镜像里装不上（可能还没同步到），改用官方 PyPI 重试：{index}"),
                ));
            }
            let mut cmd = base(&opts.uv);
            cmd.args(["pip", "install", "--python"])
                .arg(&python)
                .arg("-r")
                .arg(&requirements)
                .arg("--index-url")
                .arg(index)
                .arg("--extra-index-url")
                .arg(&opts.index_url)
                .args(["--index-strategy", "unsafe-best-match"])
                .env("UV_CACHE_DIR", &opts.uv_cache)
                .env("PYTHONUTF8", "1")
                .env("VIRTUAL_ENV", &opts.venv)
                .env("UV_HTTP_TIMEOUT", "600")
                .env("UV_HTTP_CONNECT_TIMEOUT", "30")
                .env("UV_HTTP_RETRIES", "5");
            if let Some(path) = &constraints {
                cmd.arg("--constraint").arg(path);
            }
            match stream(&mut cmd, Step::Requirements, &mut *emit, cancel) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(path) = &constraints {
            let _ = std::fs::remove_file(path);
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        emit(InstallEvent::done(Step::Requirements, "依赖已安装"));
    }

    // ---- 4/5 torch ------------------------------------------------------
    emit(InstallEvent::start(
        Step::Torch,
        format!("torch {}", opts.torch_version),
    ));
    let stack = crate::compat::stack_for(&opts.torch_version);
    if python.exists() {
        emit(InstallEvent::log(
            Step::Torch,
            "复用现有 venv：按锁定版本替换 torch 全套，不新建环境（旧版本由 uv 卸掉）",
        ));
    }
    if let Some(pin) = &stack {
        emit(InstallEvent::log(
            Step::Torch,
            format!(
                "版本锁定：torchvision {} · torchaudio {} · torchcodec {}",
                pin.torchvision, pin.torchaudio, pin.torchcodec
            ),
        ));
    } else {
        emit(InstallEvent::warn(
            Step::Torch,
            format!(
                "对照表里没有 torch {}，torchvision/torchaudio 交给 uv 解析（可能装出不对应的版本）",
                opts.torch_version
            ),
        ));
    }
    {
        let tag = index_tag(opts);
        // torch 的 Windows CUDA 轮子接近 2GB。默认 30 秒的 HTTP 读超时只适合小包，
        // 国内镜像一旦抖动就会在解压流里报 “operation timed out”，然后整个安装失败。
        // 这里放宽读超时并提高重试次数；真正的字节流仍在持续输出。
        let apply_uv_net = |cmd: &mut Command| {
            cmd.env("UV_CACHE_DIR", &opts.uv_cache)
                .env("PYTHONUTF8", "1")
                .env("VIRTUAL_ENV", &opts.venv)
                .env("UV_HTTP_TIMEOUT", "600")
                .env("UV_HTTP_CONNECT_TIMEOUT", "30")
                .env("UV_HTTP_RETRIES", "5");
        };
        let build = |index_url: &str, find_links: Option<&str>| {
            let mut cmd = base(&opts.uv);
            cmd.args(["pip", "install", "--python"]).arg(&python);
            match &stack {
                // 按表精确安装：不带 +cu130 的 `torch==2.10.0` 会被已装的
                // `2.10.0+cpu` 满足，uv 就不会换成 CUDA 轮子。
                Some(pin) => {
                    cmd.arg(format!(
                        "torch=={}",
                        crate::compat::tagged_version(&pin.torch, tag)
                    ))
                    .arg(format!(
                        "torchvision=={}",
                        crate::compat::tagged_version(&pin.torchvision, tag)
                    ))
                    .arg(format!(
                        "torchaudio=={}",
                        crate::compat::tagged_torchaudio(&pin.torchaudio, tag)
                    ));
                }
                None => {
                    cmd.arg(format!(
                        "torch=={}",
                        crate::compat::tagged_version(&opts.torch_version, tag)
                    ))
                    .args(["torchvision", "torchaudio"]);
                }
            }
            cmd.arg("--index-url")
                .arg(index_url)
                .arg("--extra-index-url")
                .arg(&opts.extra_index_url)
                // torch 索引和 PyPI 同时存在同名包，必须显式允许跨索引取最高版本。
                .args(["--index-strategy", "unsafe-best-match"]);
            if let Some(links) = find_links {
                // 不少国内镜像（阿里云/SJTU）只是扁平 wheel 目录，不是 PEP 503 索引；
                // 同时传给 --find-links 才能从镜像下载，而不是退回官方源。
                cmd.arg("--find-links").arg(links);
            }
            apply_uv_net(&mut cmd);
            cmd
        };

        let official = format!("https://download.pytorch.org/whl/{tag}/");
        let mut candidates = vec![(opts.index_url.clone(), opts.torch_find_links.clone())];
        if opts.index_url.trim_end_matches('/') != official.trim_end_matches('/') {
            candidates.push((official, None));
        }

        let mut last_err: Option<AppError> = None;
        for (attempt, (index_url, find_links)) in candidates.into_iter().enumerate() {
            if attempt > 0 {
                emit(InstallEvent::warn(
                    Step::Torch,
                    "镜像下载中断，清理 torch 缓存后改用 PyTorch 官方源重试（约 1.9GB，需数分钟）",
                ));
                // 镜像可能留下截断的 wheel；不清理的话 uv 可能继续复用坏缓存。
                let _ = base(&opts.uv)
                    .args(["cache", "clean", "torch"])
                    .env("UV_CACHE_DIR", &opts.uv_cache)
                    .output();
            }
            let mut cmd = build(&index_url, find_links.as_deref());
            match stream(&mut cmd, Step::Torch, emit, cancel) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) if e.code == "INSTALL_CANCELLED" => return Err(e),
                Err(e) => {
                    let retryable = is_transport_failure(&e);
                    emit(InstallEvent::warn(
                        Step::Torch,
                        format!("torch 安装失败：{}", e.message.lines().next().unwrap_or("")),
                    ));
                    last_err = Some(e);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    emit(InstallEvent::done(
        Step::Torch,
        format!("torch {}", opts.torch_version),
    ));

    // ---- 5/6 triton -----------------------------------------------------
    // Triton 必须跟 torch 的 minor 对齐；装完 torch 立刻补，避免节点自己
    // 从 PyPI 拉到不匹配的版本。CPU 档没有 Triton 轮子，直接跳过。
    let triton_spec = crate::compat::triton_spec(&opts.torch_version);
    emit(InstallEvent::start(
        Step::Triton,
        triton_spec
            .clone()
            .unwrap_or_else(|| format!("torch {}", opts.torch_version)),
    ));
    if opts.torch_tag == "cpu" {
        emit(InstallEvent::warn(
            Step::Triton,
            "CPU 环境没有 Triton 轮子，跳过",
        ));
    } else if let Some(spec) = triton_spec {
        emit(InstallEvent::log(
            Step::Triton,
            format!("torch {} → {spec}", opts.torch_version),
        ));
        let candidates = crate::pip::index_candidates(Some(opts.extra_index_url.as_str()));
        let mut last_err: Option<AppError> = None;
        for (n, index) in candidates.iter().enumerate() {
            if n > 0 {
                emit(InstallEvent::warn(
                    Step::Triton,
                    format!("镜像里装不上，改用官方 PyPI 重试：{index}"),
                ));
            }
            let mut cmd = base(&opts.uv);
            cmd.args(["pip", "install", "--python"])
                .arg(&python)
                .arg("-U")
                .arg(&spec)
                .arg("--index-url")
                .arg(index)
                .env("UV_CACHE_DIR", &opts.uv_cache)
                .env("PYTHONUTF8", "1")
                .env("VIRTUAL_ENV", &opts.venv)
                .env("UV_HTTP_TIMEOUT", "600")
                .env("UV_HTTP_CONNECT_TIMEOUT", "30")
                .env("UV_HTTP_RETRIES", "5");
            match stream(&mut cmd, Step::Triton, emit, cancel) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) if e.code == "INSTALL_CANCELLED" => return Err(e),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            // Triton 是加速增强项：装不上时保留可用的 torch 环境，把原因写进日志。
            emit(InstallEvent::warn(
                Step::Triton,
                format!("Triton 安装失败，环境其余部分可用：{}", e.message),
            ));
        } else {
            emit(InstallEvent::done(Step::Triton, format!("{spec} 已安装")));
        }
    } else {
        emit(InstallEvent::warn(
            Step::Triton,
            format!(
                "对照表里没有 torch {}，跳过 Triton（避免装错版本）",
                opts.torch_version
            ),
        ));
    }

    // ---- 6/6 verify -----------------------------------------------------
    emit(InstallEvent::start(Step::Verify, "import torch"));
    let script = concat!(
        "import sys\n",
        "print('PY=' + sys.version.split()[0])\n",
        "import torch\n",
        "print('TORCH=' + torch.__version__)\n",
        "avail = torch.cuda.is_available()\n",
        "print('CUDA=' + ('1' if avail else '0'))\n",
        "if avail:\n",
        "    print('DEVICE=' + torch.cuda.get_device_name(0))\n",
    );
    let mut cmd = base(&python);
    cmd.args(["-c", script])
        .env("PYTHONUTF8", "1")
        .env("VIRTUAL_ENV", &opts.venv);
    let out = stream(&mut cmd, Step::Verify, emit, cancel)?;
    let pick = |key: &str| -> Option<String> {
        out.iter()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
    };
    let torch_version = pick("TORCH=").ok_or_else(|| {
        AppError::new(
            "VERIFY_FAILED",
            format!("校验脚本没有输出 TORCH= 行，输出如下：\n{}", out.join("\n")),
        )
    })?;
    let cuda_available = pick("CUDA=").as_deref() == Some("1");
    let cuda_device = pick("DEVICE=");
    let python_version = pick("PY=").unwrap_or_else(|| opts.python_version.clone());

    if opts.engine_id.starts_with("nvidia") && !cuda_available {
        emit(InstallEvent::warn(
            Step::Verify,
            format!("torch {torch_version} reports CUDA unavailable; GPU inference may fail"),
        ));
        return Err(AppError::new(
            "TORCH_CUDA_UNAVAILABLE",
            format!(
                "选择的是 NVIDIA 档，但装到的是 CPU torch（{torch_version}）。\
                 环境没有标记为可用；请确认显卡驱动正常后重试安装。"
            ),
        ));
    } else {
        emit(InstallEvent::done(
            Step::Verify,
            format!(
                "Python {python_version} · torch {torch_version}{}",
                cuda_device
                    .as_ref()
                    .map(|d| format!(" · {d}"))
                    .unwrap_or_default()
            ),
        ));
    }

    Ok(InstallReport {
        python_version,
        torch_version,
        cuda_available,
        cuda_device,
        comfy_dir: comfy_dir.display().to_string(),
        engine_id: opts.engine_id.clone(),
        tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        for chunk in chunks {
            pump(*chunk, tx.clone());
        }
        drop(tx);
        rx.into_iter().collect()
    }

    fn temp_root(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "comfyfleet-install-test-{tag}-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn empty_new_instance_comfy_placeholder_is_replaced_without_broken_backup() {
        let root = temp_root("empty-comfy");
        let comfy = root.join("ComfyUI");
        std::fs::create_dir_all(&comfy).unwrap();
        let staging = root.join("ComfyUI.install-test");
        let backup = replace_incomplete_comfy(&comfy, &staging).unwrap();
        assert!(backup.is_none(), "空占位目录不应产生 .broken 备份");
        assert!(!comfy.exists(), "空占位目录应被移除，给新 clone 让位");
        let broken = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("ComfyUI.broken-"));
        assert!(!broken);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nonempty_incomplete_comfy_is_still_backed_up() {
        let root = temp_root("nonempty-comfy");
        let comfy = root.join("ComfyUI");
        std::fs::create_dir_all(&comfy).unwrap();
        std::fs::write(comfy.join("user-note.txt"), b"keep me").unwrap();
        let staging = root.join("ComfyUI.install-test");
        let backup = replace_incomplete_comfy(&comfy, &staging)
            .unwrap()
            .expect("有内容的旧目录必须备份");
        assert!(!comfy.exists());
        assert_eq!(
            std::fs::read_to_string(backup.join("user-note.txt")).unwrap(),
            "keep me"
        );
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ComfyUI.broken-"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_network_and_unzip_failures_are_retryable() {
        let timeout = AppError::new(
            "TORCH_FAILED",
            "Failed to unzip wheel: ... operation timed out",
        );
        assert!(is_transport_failure(&timeout));
        let missing = AppError::new(
            "TORCH_FAILED",
            "No solution found when resolving dependencies",
        );
        assert!(!is_transport_failure(&missing));
    }

    #[test]
    fn cancelled_before_start_never_spawns() {
        let cancel = AtomicBool::new(true);
        let opts = InstallOptions {
            root: PathBuf::from("C:\\comfyfleet-cancel-test"),
            venv: PathBuf::from("C:\\comfyfleet-cancel-test\\.venv"),
            python_version: "3.12".into(),
            engine_id: "cpu".into(),
            torch_version: "2.0.0".into(),
            torch_tag: "cpu".into(),
            index_url: String::new(),
            torch_find_links: None,
            extra_index_url: String::new(),
            uv_cache: PathBuf::from("C:\\comfyfleet-cancel-test\\cache"),
            uv: PathBuf::from("uv"),
            git: PathBuf::from("git"),
            comfy_repo: String::new(),
            channel: "stable".into(),
            skip_requirements: true,
        };
        let err = run(&opts, &mut |_| {}, &cancel).expect_err("must cancel before touching disk");
        assert_eq!(err.code, "INSTALL_CANCELLED");
    }

    #[cfg(windows)]
    #[test]
    fn stream_kills_child_when_cancelled() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let mut cmd = base("cmd.exe");
        cmd.args(["/C", "ping", "-n", "20", "127.0.0.1"]);
        let started = std::time::Instant::now();
        let err =
            stream(&mut cmd, Step::Venv, &mut |_| {}, &cancel).expect_err("must cancel");
        assert_eq!(err.code, "INSTALL_CANCELLED");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "cancellation took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pump_splits_crlf_and_cr() {
        // `\r\n` 相邻：早期实现会在这里切出 start > end 的区间并 panic。
        let lines = collect(&[b"a\r\nb\nc\r\nd\n"]);
        assert_eq!(lines, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn pump_handles_split_chunks() {
        let lines = collect(&[b"hello\r\n", b"world\r\n"]);
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn pump_keeps_progress_updates() {
        // \r 进度刷新会被节流，但最终状态必须送出。
        let lines = collect(&[b"10%\r50%\r100%\n"]);
        assert!(lines.last().map(|l| l.contains("100%")).unwrap_or(false));
    }

    #[test]
    fn pump_only_keeps_final_carriage_return_state() {
        // pip/tqdm 的进度条每 0.1 秒用 \r 盖一次；不能每帧都变成一条新日志。
        let lines = collect(&[b"Downloading a\n0%\r25%\r50%\r100%\nDone\n"]);
        assert_eq!(lines, vec!["Downloading a", "100%", "Done"]);
    }

    /// 真实装一遍（默认 CPU 档 + 跳过 requirements，约 300 MB 下载）。
    /// 手动运行：
    ///   `cargo test real_install_smoke -- --ignored --nocapture`
    ///   换引擎：`$env:COMFYFLEET_SMOKE_ENGINE='nvidia-cu130'; cargo test real_install_smoke -- --ignored --nocapture`
    ///   带 ComfyUI 依赖：`$env:COMFYFLEET_SMOKE_REQ='1'`
    #[test]
    #[ignore = "需要网络与数百 MB～数 GB 下载，手动触发"]
    fn real_install_smoke() {
        let matrix = crate::matrix::load().expect("matrix");
        let engine_id =
            std::env::var("COMFYFLEET_SMOKE_ENGINE").unwrap_or_else(|_| "cpu".to_string());
        let python_version =
            std::env::var("COMFYFLEET_SMOKE_PY").unwrap_or_else(|_| "3.12".to_string());
        let with_requirements = std::env::var("COMFYFLEET_SMOKE_REQ").is_ok();
        let engine = matrix
            .engines
            .iter()
            .find(|e| e.id == engine_id)
            .unwrap_or_else(|| panic!("engine {engine_id} not in matrix"));

        let root = std::env::temp_dir().join(format!("comfyfleet-install-smoke-{engine_id}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");

        let opts = InstallOptions {
            venv: root.join(".venv"),
            root: root.clone(),
            python_version,
            engine_id: engine.id.clone(),
            torch_version: engine.torch_version.clone(),
            torch_tag: engine.index_tag.clone(),
            index_url: crate::matrix::torch_index_url(&matrix, engine),
            torch_find_links: None,
            extra_index_url: "https://pypi.org/simple".to_string(),
            uv_cache: crate::config::shared_dir().join("uv-cache"),
            uv: PathBuf::from("uv"),
            git: crate::git::ensure_bundled(None).expect("bundled git"),
            comfy_repo: "https://github.com/comfyanonymous/ComfyUI".to_string(),
            channel: "stable".to_string(),
            skip_requirements: !with_requirements,
        };

        let cancel = AtomicBool::new(false);
        let mut started = Vec::new();
        let report = run(
            &opts,
            &mut |e| {
                if e.phase == "start" {
                    started.push(e.step.clone());
                }
                println!(
                    "[{}/{} {}] {}",
                    e.step_index, e.step_total, e.phase, e.message
                )
            },
            &cancel,
        )
        .expect("install should succeed");

        assert_eq!(
            started,
            ["venv", "comfy", "requirements", "torch", "triton", "verify"],
            "首次安装必须先装 ComfyUI 本体和依赖，再装 torch 和匹配的 triton"
        );
        assert!(!report.torch_version.is_empty());
        assert!(root.join("ComfyUI").join("main.py").exists());
        assert!(root
            .join(".venv")
            .join("Scripts")
            .join("python.exe")
            .exists());
        println!("REPORT: {report:?}");
    }
}
