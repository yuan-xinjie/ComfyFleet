//! 实例进程运行时：启动 / 停止 / 日志尾部。
//!
//! 与 Tauri 解耦（只依赖 `std::process`），这样"启动命令怎么写"这类逻辑可以直接单测。
//! 不用构建 AppHandle。
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::config::{GlobalSettings, InstanceConfig};
use crate::error::AppError;

/// 内存里保留的日志行数（完整日志在 logs/*.log，这里只用于界面展示）。
pub const TAIL_MAX: usize = 600;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 找实例的虚拟环境：兼容我们自己建的和用户导入的布局
pub fn find_venv(root: &Path) -> Option<PathBuf> {
    [
        root.join(".venv"),
        root.join("venv"),
        root.join("ComfyUI").join(".venv"),
    ]
    .into_iter()
    .find(|p| p.join("Scripts").join("python.exe").exists())
}

/// 实例实际使用 Python 环境：可能是 App 自建 venv，也可能是用户指过来的已有环境。
#[derive(Debug, Clone)]
pub struct PyEnv {
    /// 环境根：venv 根，或便携版 / 解释器所在目录。
    pub root: PathBuf,
    /// 解释器。
    pub python: PathBuf,
    /// 是否是 App 自建 venv（可重建；切换引用 Python 时会备份它）
    pub managed: bool,
    /// 有没有 venv 的 `Scripts` / `bin` 布局（决定要不要设 VIRTUAL_ENV）。
    pub venv_style: bool,
}

impl PyEnv {
    /// 解释器所在目录：注入 PATH 时用
    pub fn bin_dir(&self) -> PathBuf {
        self.python
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone())
    }
}

/// 把一个解释器路径包成 PyEnv；工具目录是 `Scripts` / `bin` 时，环境根取上一层。
fn interpreter(py: &Path) -> Option<PyEnv> {
    if !py.is_file() {
        return None;
    }
    let dir = py.parent()?;
    let leaf = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let venv_style = leaf == "scripts" || leaf == "bin";
    let root = if venv_style {
        dir.parent().map(Path::to_path_buf)?
    } else {
        dir.to_path_buf()
    };
    Some(PyEnv {
        root,
        python: py.to_path_buf(),
        managed: false,
        venv_style,
    })
}

/// 从一个目录或解释器路径认出一个 Python 环境。
///
/// 认这些布局：venv（`Scripts/python.exe`、`bin/python`）、目录里直接 `python.exe`。
/// 以及 ComfyUI 便携版的 `python_embeded/python.exe`。导入实例时也用它。
pub fn detect_python(path: &Path) -> Option<PyEnv> {
    if path.is_file() {
        return interpreter(path);
    }
    for candidate in [
        path.join("Scripts").join("python.exe"),
        path.join("bin").join("python"),
        path.join("python.exe"),
        path.join("python"),
        // 实例根：App 自建的 venv 在下一层
        path.join(".venv").join("Scripts").join("python.exe"),
        path.join("venv").join("Scripts").join("python.exe"),
        path.join("ComfyUI")
            .join(".venv")
            .join("Scripts")
            .join("python.exe"),
        // ComfyUI 便携版
        path.join("python_embeded").join("python.exe"),
    ] {
        if let Some(py) = interpreter(&candidate) {
            return Some(py);
        }
    }
    None
}

/// 这个实例实际用哪个 Python。
///
/// - `existing`：用实例设置里填的路径（相对路径按实例根解析）；留空就在实例目录里自动找。
/// - `managed`（默认）：在实例目录里找 App 建的 `.venv` / `venv`。
pub fn resolve_python(inst: &InstanceConfig) -> Option<PyEnv> {
    let root = Path::new(&inst.root_dir);
    if inst.python.is_existing() {
        if let Some(raw) = inst
            .python
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let p = Path::new(raw);
            let p = if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            };
            return detect_python(&p);
        }
        return detect_python(root);
    }
    find_venv(root).map(|venv| {
        let python = venv.join("Scripts").join("python.exe");
        PyEnv {
            root: venv,
            python,
            managed: true,
            venv_style: true,
        }
    })
}

/// 组装实例进程的环境变量。
///
/// 优先级：实例 env > 实例镜像覆盖 > 全局镜像 > 默认。
/// 编码相关变量是硬需求：ComfyUI 会打 emoji / 方块字符。
/// 不解码成 UTF-8 会让日志变成乱码甚至读取线程报错。
pub fn build_env(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    py: &PyEnv,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // 便携 / 自定义解释器不是 venv，设 VIRTUAL_ENV 反倒会让 pip 认错环境
    if py.venv_style {
        env.insert(
            "VIRTUAL_ENV".to_string(),
            py.root.to_string_lossy().to_string(),
        );
    }
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
    // 不缓冲：否则日志要等进程退出才出来，启动失败时看不到任何东西。
    env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());
    env.insert("COMFYFLEET_INSTANCE_ID".to_string(), inst.id.clone());
    env.insert("COMFYFLEET_INSTANCE_NAME".to_string(), inst.name.clone());
    env.insert(
        "COMFYFLEET_INSTANCE_ROOT".to_string(),
        inst.root_dir.clone(),
    );

    if let Some(pypi) = inst
        .overrides
        .pypi
        .as_ref()
        .or(settings.mirrors.pypi.as_ref())
    {
        env.insert("PIP_INDEX_URL".to_string(), pypi.clone());
        env.insert("UV_INDEX_URL".to_string(), pypi.clone());
    }
    if let Some(hf) = inst.overrides.hf.as_ref().or(settings.mirrors.hf.as_ref()) {
        env.insert("HF_ENDPOINT".to_string(), hf.clone());
    }
    let hf_offline = inst
        .overrides
        .hf_offline
        .unwrap_or(settings.network.hf_offline);
    if hf_offline {
        env.insert("HF_HUB_OFFLINE".to_string(), "1".to_string());
        env.insert("TRANSFORMERS_OFFLINE".to_string(), "1".to_string());
    }

    for (k, v) in &inst.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// 把「一行启动参数」拆 argv。
///
/// 一行里可以写多个参数（`--preview-method auto`），按空白切；双引号里的空白保留
///（`--models-directory "D:\my models"`）。不支持反斜杠转 — 参数最终由
/// CreateProcess 解析，各语言的转义规则都不一样，与其猜不如只支持引号。
pub fn split_arg_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// 只要服务与网络那段的参数（测试与冒烟用；真实启动 `launch_args_with`。
#[cfg(test)]
pub fn launch_args(inst: &InstanceConfig) -> Vec<String> {
    launch_args_with(inst, &[])
}

/// `storage_args`  `storage::prepare` 给出（模 yaml / 输入 / 输出目录）。
///
/// 存储参数排在自定义参 *前面**：目录归属由「存储」页决定。
/// 用户在启动参数页手写同名参数时，ComfyUI 取后出现的那个值会赢—。
/// 所以界面上把这类参数标成「由存储页管理」，不鼓励手写。
pub fn launch_args_with(inst: &InstanceConfig, storage_args: &[String]) -> Vec<String> {
    let mut args = vec!["main.py".to_string()];
    let listen = inst.server.listen.trim();
    if !listen.is_empty() {
        args.push("--listen".to_string());
        args.push(listen.to_string());
    }
    args.push("--port".to_string());
    args.push(inst.server.port.to_string());
    if let Some(cors) = inst
        .server
        .cors_origin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // 参数名是 ComfyUI 的 --enable-cors-header；早先这里写成 --cors-origin，
        // argparse 会直接报 unrecognized arguments，实例根本起不来。
        args.push("--enable-cors-header".to_string());
        args.push(cors.to_string());
    }
    args.extend(storage_args.iter().cloned());
    for line in &inst.server.extra_args {
        args.extend(split_arg_line(line));
    }
    args
}

/// 实际探测用的地址：监听在 0.0.0.0 时本机要 127.0.0.1
pub fn probe_host(listen: &str) -> &str {
    let listen = listen.trim();
    if listen.is_empty() || listen == "0.0.0.0" || listen == "::" {
        "127.0.0.1"
    } else {
        listen
    }
}

pub fn port_open(host: &str, port: u16) -> bool {
    let host = probe_host(host);
    let addr: SocketAddr = match if host.contains(':') {
        format!("[{host}]:{port}").parse()
    } else {
        format!("{host}:{port}").parse()
    } {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 端口是否可绑定（ connect 探测更可靠：没人监听 connect 会失败，
/// 但被防火墙拦掉的服务也会 connect 失败， bind 失败才是"真被 。
pub fn port_free(listen: &str, port: u16) -> bool {
    let host = probe_host(listen);
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    std::net::TcpListener::bind(&addr).is_ok()
}

/// 读一 HTTP 响应体（只用于本机接口，不引 http 客户端）
pub fn http_body(host: &str, port: u16, path: &str, timeout: Duration) -> Option<String> {
    let host = probe_host(host);
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addr: SocketAddr = addr.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.starts_with("HTTP/1.") || !head.contains(" 200") {
        return None;
    }
    Some(body.to_string())
}

/// 实例健康信息（从 ComfyUI 自己的接口读，不猜）
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub vram_used_mib: u32,
    pub vram_total_mib: u32,
    /// GPU 利用率（0–100）：ComfyUI 接口不提供，由 nvidia-smi 读
    pub gpu_utilization: Option<u32>,
    /// 系统内存：ComfyUI  `/system_stats` 里带（RAM 不是显存，模型加载常先吃满内存）
    pub ram_used_mib: u64,
    pub ram_total_mib: u64,
    pub queue_running: u32,
    pub queue_pending: u32,
    /// 正在执行的任务；队列里没有任务时为 None
    pub task_id: Option<String>,
    pub task_name: Option<String>,
    pub updated_at: String,
}

/// 首页卡片用：从 `/queue` 的第一条运行任务里取一个可读的名字。
///
/// ComfyUI 的队列本身不保存“工作流文件名”，但客户端会把整份 workflow 塞进
/// `extra_data.extra_pnginfo.workflow`。优先用其中显式的 name；没有时退到输出
/// 节点标题/类型（例如“保存图像”），最后才用 prompt id。
fn running_task_name(queue: &serde_json::Value) -> (Option<String>, Option<String>) {
    let Some(item) = queue
        .get("queue_running")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_array())
    else {
        return (None, None);
    };
    let id = item
        .get(1)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let prompt = item.get(2);
    let extra = item.get(3);

    let explicit = [
        "/extra_pnginfo/workflow/name",
        "/extra_pnginfo/workflow/extra/name",
        "/workflow_metadata/name",
        "/workflow_metadata/workflow_name",
    ]
    .iter()
    .filter_map(|path| {
        extra
            .and_then(|e| e.pointer(*path))
            .and_then(|v| v.as_str())
    })
    .map(str::trim)
    .find(|s| !s.is_empty())
    .map(str::to_string);

    let name = explicit
        .or_else(|| task_name_from_prompt(prompt))
        .or_else(|| id.as_deref().map(|s| format!("任务 {}", short_id(s))));
    (id, name)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn task_name_from_prompt(prompt: Option<&serde_json::Value>) -> Option<String> {
    const OUTPUT_LABELS: &[(&str, &str)] = &[
        ("SaveImage", "图像生成"),
        ("PreviewImage", "图像预览"),
        ("SaveAnimatedWEBP", "动图生成"),
        ("SaveAnimatedPNG", "动图生成"),
        ("SaveWEBM", "视频生成"),
        ("SaveVideo", "视频生成"),
        ("VHS_VideoCombine", "视频生成"),
        ("SaveAudio", "音频生成"),
        ("SaveAudioMP3", "音频生成"),
        ("SaveAudioFLAC", "音频生成"),
        ("SaveGLB", "3D 生成"),
    ];
    let nodes = prompt?.as_object()?;
    let mut first: Option<String> = None;
    for node in nodes.values() {
        let class = node
            .get("class_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = node
            .get("_meta")
            .and_then(|m| m.get("title"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if first.is_none() {
            first = title
                .or((!class.is_empty()).then_some(class))
                .map(str::to_string);
        }
        if let Some((_, label)) = OUTPUT_LABELS.iter().copied().find(|(c, _)| *c == class) {
            return Some(title.filter(|s| *s != class).unwrap_or(label).to_string());
        }
    }
    first
}

/// 解析 `/system_stats` + `/queue`
pub fn parse_health(stats: &str, queue: &str) -> Option<Health> {
    let stats: serde_json::Value = serde_json::from_str(stats).ok()?;
    let dev = stats.get("devices")?.as_array()?.first()?;
    let total = dev.get("vram_total")?.as_u64()?;
    let free = dev.get("vram_free")?.as_u64()?;
    let to_mib = |b: u64| (b / 1024 / 1024) as u32;

    // system 段缺了不影响显存/队列，内存按 0 显示（老版 ComfyUI 可能没有。
    let ram_total = stats
        .get("system")
        .and_then(|s| s.get("ram_total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ram_free = stats
        .get("system")
        .and_then(|s| s.get("ram_free"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let to_mib64 = |b: u64| b / 1024 / 1024;

    let queue: serde_json::Value = serde_json::from_str(queue).ok()?;
    let len = |key: &str| {
        queue
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0)
    };
    let (task_id, task_name) = running_task_name(&queue);

    Some(Health {
        vram_total_mib: to_mib(total),
        vram_used_mib: to_mib(total.saturating_sub(free)),
        gpu_utilization: None,
        ram_total_mib: to_mib64(ram_total),
        ram_used_mib: to_mib64(ram_total.saturating_sub(ram_free)),
        queue_running: len("queue_running"),
        queue_pending: len("queue_pending"),
        task_id,
        task_name,
        updated_at: chrono::Local::now().to_rfc3339(),
    })
}

/// 采集一次健康信息（接口不通就返回 None，调用方保留上一次的值）
pub fn probe_health(host: &str, port: u16) -> Option<Health> {
    let timeout = Duration::from_secs(3);
    let stats = http_body(host, port, "/system_stats", timeout)?;
    let queue = http_body(host, port, "/queue", timeout)?;
    let mut health = parse_health(&stats, &queue)?;
    // ComfyUI 的 /system_stats 没有 GPU 利用率，这里单独问 nvidia-smi；
    // 读不到就保持 None，不影响显存/内存/队列。
    health.gpu_utilization = crate::gpu::utilization();
    Some(health)
}

/// 读一 HTTP 状态码（只用于本机健康检查，不引 http 客户端）
pub fn http_status(host: &str, port: u16, path: &str, timeout: Duration) -> Option<u16> {
    let host = probe_host(host);
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addr: SocketAddr = addr.parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // "HTTP/1.1 200 OK"
    line.split_whitespace().nth(1)?.parse().ok()
}

/// 一次启动所需的全部输入（构造好之后 spawn 不再读外部状态）
pub struct Launch {
    pub python: PathBuf,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub log_path: PathBuf,
    pub port: u16,
}

/// 每读到一 ComfyUI 输出就回调一次。运行时本身不依 Tauri。
/// 由命令层把事件推给界面，避免界面反复拉日志文件。
pub type LineSink = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// 全局递增序号：跨进程也不回绕，界面重启实例后不会因为序号归零而丢新日志。
static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

/// 运行中的实例进程句柄
pub struct ProcHandle {
    child: Mutex<Child>,
    tail: Mutex<VecDeque<String>>,
    health: Mutex<Option<Health>>,
    sink: Option<LineSink>,
    pub pid: u32,
    pub port: u16,
}

impl ProcHandle {
    fn push_line(&self, line: String) {
        let seq = {
            let mut tail = self.tail.lock();
            // 序号 tail 锁内递增，tail_with_seq 也在同一把锁里读，快照与事件不会错位。
            let seq = LOG_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
            if tail.len() >= TAIL_MAX {
                tail.pop_front();
            }
            tail.push_back(line.clone());
            seq
        };
        if let Some(sink) = &self.sink {
            sink(&line, seq);
        }
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let tail = self.tail.lock();
        let start = tail.len().saturating_sub(n);
        tail.iter().skip(start).cloned().collect()
    }

    /// 同一个锁里取尾部与序号，保证界面拿到的快照和后续事件能严丝合缝地接上。
    pub fn tail_with_seq(&self, n: usize) -> (Vec<String>, u64) {
        let tail = self.tail.lock();
        let start = tail.len().saturating_sub(n);
        (
            tail.iter().skip(start).cloned().collect(),
            LOG_SEQ.load(Ordering::SeqCst),
        )
    }

    pub fn health(&self) -> Option<Health> {
        self.health.lock().clone()
    }

    pub fn set_health(&self, value: Option<Health>) {
        *self.health.lock() = value;
    }

    /// 已退出则返回退出码（仍在跑返回 None。
    pub fn try_exit(&self) -> Option<i32> {
        let mut child = self.child.lock();
        match child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// 杀整棵进程树。
    ///
    /// 不能 `child.kill()`：ComfyUI 会拉起子进程（自定义节点、加速后端）。
    /// 父进程死了子进程会留在后台占着端口，下次启动就报端口占用。
    pub fn kill_tree(&self) -> Result<(), AppError> {
        // 进程已经退出时不要再按 pid 调 taskkill：Windows 可能已把 pid 复用给
        // 无关进程。子句柄能可靠判断“本进程已退出”。
        if self.try_exit().is_some() {
            return Ok(());
        }
        kill_pid_tree(self.pid)
    }
}

///  pid 杀整棵进程树（用于清理没有句柄的孤儿进程）
pub fn kill_pid_tree(pid: u32) -> Result<(), AppError> {
    if pid == 0 {
        return Err(AppError::new("KILL_INVALID_PID", "pid 无效"));
    }
    let out = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags_none()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AppError::new("KILL_FAILED", format!("结束进程树失败（pid {pid}）：{e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // 128 是 taskkill “进程不存在”的约定退出码；中英文错误都兜一下。
    if out.status.code() == Some(128)
        || stderr.contains("not found")
        || stderr.contains("没有找到")
        || stderr.contains("找不到")
    {
        return Ok(());
    }
    Err(AppError::new(
        "KILL_FAILED",
        format!(
            "结束进程树失败（pid {pid}，exit={:?}）：{}",
            out.status.code(),
            stderr.trim()
        ),
    ))
}

/// 启动器写在实例目录里的运行记录：应用被强杀后靠它找回孤儿进。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub pid: u32,
    pub port: u16,
    pub started_at: String,
    pub python: String,
}

pub fn record_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("run.json")
}

pub fn write_record(instance_dir: &Path, record: &RunRecord) -> Result<(), AppError> {
    let path = record_path(instance_dir);
    let body = serde_json::to_vec_pretty(record)
        .map_err(|e| AppError::new("RECORD_SERIALIZE", format!("序列化运行记录失败：{e}")))?;
    crate::config::atomic_write(&path, &body).map_err(|e| {
        AppError::new(
            "RECORD_WRITE",
            format!("写入 {} 失败：{}", path.display(), e.message),
        )
    })
}

pub fn read_record(instance_dir: &Path) -> Option<RunRecord> {
    let path = record_path(instance_dir);
    let raw = std::fs::read(&path).ok()?;
    // 兼容 UTF-8 BOM 的文件：手工编辑或用别的工具写入时很常见     // 不剥掉会 serde 解析失败，而成败都不能静默
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    match serde_json::from_slice::<RunRecord>(raw) {
        Ok(rec) => Some(rec),
        Err(e) => {
            log::warn!("运行记录 {} 解析失败（忽略）：{e}", path.display());
            None
        }
    }
}

pub fn remove_record(instance_dir: &Path) {
    let _ = std::fs::remove_file(record_path(instance_dir));
}

/// 取进程的可执行路 + 命令行（Windows 无原生轻量接口， CIM。
fn process_command_line(pid: u32) -> Option<String> {
    let script = format!("$p = Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; if ($p) {{ $($p.CommandLine) }}");
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags_none()
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 校验这个 pid 是否真的 该实例的 venv python 在跑 main.py"。
///
/// **绝不能不校验 kill**：机器重启或进程退出后 pid 会被系统复用。
/// 拿一个过 pid 直接 taskkill 可能杀掉完全无关的程序。
pub fn verify_process(pid: u32, python: &Path) -> bool {
    let Some(info) = process_command_line(pid) else {
        return false;
    };
    let lower = info.to_lowercase();
    let python_lower = python.to_string_lossy().to_lowercase();
    // 只比较到 Scripts 目录：命令行里的 python 路径可能带引号或不同大小。
    let scripts_dir = python_lower
        .rsplit_once('\\')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or(python_lower.clone());
    lower.contains(&scripts_dir) && lower.contains("main.py")
}

/// Hide console window on Windows.
trait NoWindow {
    fn creation_flags_none(&mut self) -> &mut Self;
}

impl NoWindow for Command {
    fn creation_flags_none(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

#[cfg(test)]
pub fn spawn(launch: Launch) -> Result<Arc<ProcHandle>, AppError> {
    spawn_inner(launch, None)
}

/// 带实时回调的启动：每读到一行就调用 `sink(line, seq)`。
pub fn spawn_with_sink(launch: Launch, sink: LineSink) -> Result<Arc<ProcHandle>, AppError> {
    spawn_inner(launch, Some(sink))
}

fn spawn_inner(launch: Launch, sink: Option<LineSink>) -> Result<Arc<ProcHandle>, AppError> {
    let port = launch.port;
    if let Some(dir) = launch.log_path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::new("LOG_DIR_FAILED", format!("创建日志目录失败：{e}")))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&launch.log_path)
        .map_err(|e| AppError::new("LOG_OPEN_FAILED", format!("打开日志文件失败：{e}")))?;
    let file = Arc::new(Mutex::new(std::io::BufWriter::new(file)));

    let mut cmd = Command::new(&launch.python);
    cmd.args(&launch.args)
        .current_dir(&launch.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&launch.env)
        .creation_flags_none();

    let mut child = cmd.spawn().map_err(|e| {
        AppError::new(
            "SPAWN_FAILED",
            format!("启动 {} 失败：{e}", launch.python.display()),
        )
    })?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let handle = Arc::new(ProcHandle {
        child: Mutex::new(child),
        tail: Mutex::new(VecDeque::new()),
        health: Mutex::new(None),
        sink,
        pid,
        port,
    });

    if let Some(out) = stdout {
        pump(out, Arc::clone(&handle), Arc::clone(&file));
    }
    if let Some(err) = stderr {
        pump(err, Arc::clone(&handle), Arc::clone(&file));
    }
    Ok(handle)
}

fn pump<R: Read + Send + 'static>(
    stream: R,
    handle: Arc<ProcHandle>,
    file: Arc<Mutex<std::io::BufWriter<std::fs::File>>>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::new();
        let mut last_emit = std::time::Instant::now();
        let throttle = Duration::from_millis(120);

        // tqdm / 下载条会 \r 原地刷新，不能只 \n，否则整段进度会挤成一行         //  \r 刷新很密，按 120ms 节流，避 IPC 把界面拖死。
        let mut flush = |segment: &[u8], is_progress: bool, force: bool| -> bool {
            if segment.is_empty() {
                return true;
            }
            let now = std::time::Instant::now();
            if is_progress && !force && now.duration_since(last_emit) < throttle {
                return true;
            }
            last_emit = now;
            // ComfyUI 的日志带 ANSI 颜色码（\x1b[32m[INFO]\x1b[0m），
            // 不剥掉界面里会显示成一 [32m
            let line = strip_ansi(&String::from_utf8_lossy(segment));
            let line = line.trim_end();
            if line.is_empty() {
                return true;
            }
            {
                let mut f = file.lock();
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
                let _ = f.flush();
            }
            handle.push_line(line.to_string());
            true
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
                let byte = pending[i];
                if byte == b'\n' || byte == b'\r' {
                    // \r\n 算一个换。
                    let skip = if byte == b'\r' && pending.get(i + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    let is_progress = byte == b'\r' && skip == 1;
                    if !flush(&pending[consumed..i], is_progress, false) {
                        return;
                    }
                    consumed = i + skip;
                    i = consumed;
                    continue;
                }
                i += 1;
            }
            if consumed > 0 {
                pending.drain(..consumed);
            }
        }
        let _ = flush(&pending, false, true);
    });
}

/// 去掉 ANSI 转义序列（CSI 与裸 ESC。
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // CSI：一直吃 0x40..=0x7E 的终止字。
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ComfyRef, InstanceOverrides, InstanceState, ServerOpts};

    fn sample(listen: &str) -> InstanceConfig {
        InstanceConfig {
            id: "abcdef01".to_string(),
            name: "t".to_string(),
            root_dir: "C:\\tmp\\x".to_string(),
            engine: "cpu".to_string(),
            engine_detail: None,
            python_version: "3.12".to_string(),
            python: Default::default(),
            comfy: ComfyRef {
                channel: "stable".to_string(),
                reference: String::new(),
                commit: String::new(),
                version: None,
            },
            server: ServerOpts {
                listen: listen.to_string(),
                port: 8188,
                cors_origin: Some("http://127.0.0.1:3000".to_string()),
                extra_args: vec!["--lowvram".to_string()],
            },
            storage: Default::default(),
            env: [("FOO".to_string(), "bar".to_string())]
                .into_iter()
                .collect(),
            overrides: InstanceOverrides::default(),
            node_deps_auto: None,
            auto_restart: false,
            kind: "local".to_string(),
            remote: None,
            imported: false,
            state: InstanceState::Ready,
            created_at: String::new(),
            last_started_at: None,
        }
    }

    #[test]
    fn launch_args_follow_instance_settings() {
        let args = launch_args(&sample("0.0.0.0"));
        assert_eq!(
            args,
            vec![
                "main.py",
                "--listen",
                "0.0.0.0",
                "--port",
                "8188",
                "--enable-cors-header",
                "http://127.0.0.1:3000",
                "--lowvram",
            ]
        );
    }

    #[test]
    fn launch_args_skip_empty_cors() {
        let mut inst = sample("127.0.0.1");
        inst.server.cors_origin = Some("   ".to_string());
        inst.server.extra_args = Vec::new();
        let args = launch_args(&inst);
        assert!(!args.iter().any(|a| a == "--enable-cors-header"));
        assert_eq!(
            args,
            vec!["main.py", "--listen", "127.0.0.1", "--port", "8188"]
        );
    }

    #[test]
    fn launch_args_split_lines_into_tokens() {
        let mut inst = sample("127.0.0.1");
        inst.server.cors_origin = None;
        inst.server.extra_args = vec![
            "--preview-method auto".to_string(),
            "--reserve-vram  2.5".to_string(),
            "--models-directory \"D:\\my models\"".to_string(),
            "--lowvram".to_string(),
        ];
        let args = launch_args(&inst);
        assert_eq!(
            &args[5..],
            [
                "--preview-method",
                "auto",
                "--reserve-vram",
                "2.5",
                "--models-directory",
                "D:\\my models",
                "--lowvram",
            ]
        );
    }

    #[test]
    fn split_arg_line_keeps_quoted_spaces() {
        assert_eq!(split_arg_line(""), Vec::<String>::new());
        assert_eq!(split_arg_line("   "), Vec::<String>::new());
        assert_eq!(split_arg_line("--lowvram"), vec!["--lowvram"]);
        assert_eq!(
            split_arg_line("--output-directory \"C:\\out dir\" --verbose"),
            vec!["--output-directory", "C:\\out dir", "--verbose"]
        );
        assert_eq!(split_arg_line("\"\""), vec![""]);
    }

    #[test]
    fn probe_host_maps_wildcard_to_loopback() {
        assert_eq!(probe_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(probe_host(""), "127.0.0.1");
        assert_eq!(probe_host("::"), "127.0.0.1");
        assert_eq!(probe_host("192.168.1.5"), "192.168.1.5");
    }

    #[test]
    fn build_env_sets_encoding_and_instance_vars() {
        let settings = GlobalSettings::default();
        let inst = sample("127.0.0.1");
        let py = PyEnv {
            root: PathBuf::from("C:\\tmp\\x\\.venv"),
            python: PathBuf::from("C:\\tmp\\x\\.venv\\Scripts\\python.exe"),
            managed: true,
            venv_style: true,
        };
        let env = build_env(&inst, &settings, &py);
        assert_eq!(env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert_eq!(env.get("PYTHONUNBUFFERED").map(String::as_str), Some("1"));
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(
            env.get("COMFYFLEET_INSTANCE_ID").map(String::as_str),
            Some("abcdef01")
        );
    }

    #[test]
    fn instance_env_overrides_mirrors() {
        let mut settings = GlobalSettings::default();
        settings.mirrors.pypi = Some("https://global.example/simple".to_string());
        settings.network.hf_offline = true;
        let mut inst = sample("127.0.0.1");
        inst.env.insert(
            "PIP_INDEX_URL".to_string(),
            "https://mine.example/simple".to_string(),
        );
        let py = PyEnv {
            root: PathBuf::from("C:\\tmp\\x\\.venv"),
            python: PathBuf::from("C:\\tmp\\x\\.venv\\Scripts\\python.exe"),
            managed: true,
            venv_style: true,
        };
        let env = build_env(&inst, &settings, &py);
        assert_eq!(
            env.get("PIP_INDEX_URL").map(String::as_str),
            Some("https://mine.example/simple"),
            "实例 env 必须能覆盖全局镜像"
        );
        assert_eq!(env.get("HF_HUB_OFFLINE").map(String::as_str), Some("1"));
    }

    #[test]
    fn port_open_is_false_for_unused_port() {
        // 用一个几乎不可能被占用的高位端口
        assert!(!port_open("127.0.0.1", 45_999));
    }

    #[test]
    fn port_free_reflects_real_binding() {
        // 占住一个端口：port_free 必须 false
        let busy = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let busy_port = busy.local_addr().expect("addr").port();
        assert!(!port_free("127.0.0.1", busy_port));

        // 释放后必须报 true
        drop(busy);
        assert!(port_free("127.0.0.1", busy_port));
    }

    #[test]
    fn http_status_reads_status_line() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = [0u8; 256];
                let _ = std::io::Read::read(&mut s, &mut req);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert_eq!(
            http_status("127.0.0.1", port, "/system_stats", Duration::from_secs(3)),
            Some(200)
        );
        // 没有服务监听的端口必须返。
        // A closed port must return None.
        assert_eq!(
            http_status(
                "127.0.0.1",
                45_998,
                "/system_stats",
                Duration::from_millis(300)
            ),
            None
        );
    }

    #[test]
    fn run_record_round_trip() {
        let dir = std::env::temp_dir().join("comfyfleet-record-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(read_record(&dir).is_none(), "没有记录时应返回 None");

        let rec = RunRecord {
            pid: 4242,
            port: 8188,
            started_at: "2026-10-28T00:00:00+08:00".to_string(),
            python: "C:\\x\\.venv\\Scripts\\python.exe".to_string(),
        };
        write_record(&dir, &rec).expect("write");
        let back = read_record(&dir).expect("read");
        assert_eq!(back.pid, 4242);
        assert_eq!(back.port, 8188);
        assert_eq!(back.python, rec.python);

        remove_record(&dir);
        assert!(read_record(&dir).is_none());

        //  BOM 的文件（PowerShell 5.x / 某些编辑器会写）必须也能。
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(br#"{"pid":7,"port":9,"startedAt":"x","python":"p"}"#);
        std::fs::write(record_path(&dir), with_bom).expect("write bom");
        let back = read_record(&dir).expect("BOM 文件应能解析");
        assert_eq!(back.pid, 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 校验逻辑：必须是「该路径下的 python 在跑 main.py」才认，否则拒绝杀
    #[test]
    fn verify_process_rejects_unrelated_pids() {
        if process_command_line(std::process::id()).is_none() {
            println!("CIM 查询不可用，跳过 verify_process 测试");
            return;
        }
        // 自己不是 python main.py，任 venv 路径都不该被认领
        assert!(!verify_process(
            std::process::id(),
            Path::new("C:\\some\\venv\\Scripts\\python.exe")
        ));

        // 造一 命令行含 main.py"的进程，用它的真实可执行路径应被认领
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut child = Command::new(&comspec)
            .args(["/c", "ping -n 8 127.0.0.1 > nul & rem main.py"])
            .creation_flags_none()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn helper");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(400));
        let matched = verify_process(pid, Path::new(&comspec));
        let _ = kill_pid_tree(pid);
        let _ = child.wait();
        assert!(matched, "命令行含 main.py 的进程应被认 ");
    }

    /// 用真实抓的接口响应做样本（手写的假数据坑过一次：CUDA 头解析）
    #[test]
    fn parses_real_health_fixtures() {
        let stats = include_str!("../tests/fixtures/system_stats.json");
        let queue = include_str!("../tests/fixtures/queue.json");
        let h = parse_health(stats, queue).expect("真实样本必须能解 ");
        assert_eq!(h.vram_total_mib, 32606);
        assert!(h.vram_used_mib > 0, "空载也应有非零占 ");
        assert!(h.vram_used_mib < h.vram_total_mib);
        assert_eq!(h.queue_running, 0);
        assert_eq!(h.queue_pending, 0);
        // 内存 system.ram_total / ram_free 27 GiB 的机  130800 MiB。
        assert_eq!(h.ram_total_mib, 130_773);
        assert!(h.ram_used_mib > 0, "空载也应占用一部分内存");
        assert!(h.ram_used_mib < h.ram_total_mib);

        // 结构不符时要返回 None，而不 panic
        assert!(parse_health("{}", queue).is_none());
        assert!(parse_health(stats, "not json").is_none());
        // 老版本没 system.ram_* 时内存按 0，显存照样能。
        let no_ram = r#"{"devices":[{"vram_total":1048576,"vram_free":524288}]}"#;
        let h = parse_health(no_ram, queue).expect("缺内存字段也要能解析");
        assert_eq!((h.vram_total_mib, h.ram_total_mib), (1, 0));
    }

    #[test]
    fn parses_running_task_name_from_queue() {
        let stats = include_str!("../tests/fixtures/system_stats.json");
        let queue = r#"{
            "queue_running": [[0, "8d1f6c2a-rest", {
                "9": {"class_type": "SaveImage", "_meta": {"title": "保存成图"}},
                "3": {"class_type": "KSampler", "_meta": {"title": "采样器"}}
            }, {"extra_pnginfo": {"workflow": {"name": "人像精修"}}}, []]],
            "queue_pending": []
        }"#;
        let h = parse_health(stats, queue).expect("运行队列也要能解析");
        assert_eq!(h.queue_running, 1);
        assert_eq!(h.task_id.as_deref(), Some("8d1f6c2a-rest"));
        assert_eq!(h.task_name.as_deref(), Some("人像精修"));

        // 没有显式 workflow.name 时，用输出节点标题；再没有才用任务 id。
        let queue = queue.replace(r#""workflow": {"name": "人像精修"}"#, r#""workflow": {}"#);
        let h = parse_health(stats, &queue).expect("无名字也要能解析");
        assert_eq!(h.task_name.as_deref(), Some("保存成图"));

        let queue = r#"{"queue_running": [[0, "deadbeef-rest", {
            "9": {"class_type": "SaveImage", "_meta": {}}
        }, {}, []]], "queue_pending": []}"#;
        let h = parse_health(stats, queue).expect("只有队列 id 也要能解析");
        assert_eq!(h.task_name.as_deref(), Some("图像生成"));
    }

    #[test]
    fn strips_ansi_color_codes() {
        // 真实样本：ComfyUI  colorama 上色
        assert_eq!(
            strip_ansi("\u{1b}[32m[INFO]\u{1b}[0m Starting server"),
            "[INFO] Starting server"
        );
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[33m[WARNING]\u{1b}[0m hello"),
            "[WARNING] hello"
        );
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\u{1b}[0m"), "");
    }

    /// 真启动一 ComfyUI（用机器上已装好的实例，端口改成 9333 避免冲突）。
    /// 手动运行：`cargo test real_launch_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "会真的启 ComfyUI 占用端口，手动触 "]
    fn real_launch_smoke() {
        let dir = crate::config::instances_dir();
        let mut target: Option<InstanceConfig> = None;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let manifest = entry.path().join("instance.json");
                let Ok(raw) = std::fs::read_to_string(&manifest) else {
                    continue;
                };
                let Ok(inst) = serde_json::from_str::<InstanceConfig>(&raw) else {
                    continue;
                };
                let root = PathBuf::from(&inst.root_dir);
                if find_venv(&root).is_some() && root.join("ComfyUI").join("main.py").exists() {
                    target = Some(inst);
                    break;
                }
            }
        }
        let Some(inst) = target else {
            println!("机器上没有装好的实例，跳 ");
            return;
        };

        let root = PathBuf::from(&inst.root_dir);
        let py = resolve_python(&inst).expect("python env");
        let port = 9333u16;
        let mut args = launch_args(&inst);
        if let Some(i) = args.iter().position(|a| a == "--port") {
            args[i + 1] = port.to_string();
        }
        let log_path = std::env::temp_dir().join("comfyfleet-run-smoke.log");
        let _ = std::fs::remove_file(&log_path);
        let launch = Launch {
            python: py.python.clone(),
            cwd: root.join("ComfyUI"),
            args: args.clone(),
            env: build_env(&inst, &GlobalSettings::default(), &py),
            log_path: log_path.clone(),
            port,
        };
        let handle = spawn(launch).expect("spawn");
        println!("实例 {} 已启 pid={} args={args:?}", inst.id, handle.pid);

        let started = std::time::Instant::now();
        let mut ready = false;
        while started.elapsed().as_secs() < 150 {
            if let Some(code) = handle.try_exit() {
                println!("进程提前退出，退出码 {code}");
                break;
            }
            if port_open("127.0.0.1", port) {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        println!("--- 最 30 行输 ---");
        for line in handle.tail(30) {
            println!("  {line}");
        }
        // 健康检查：/system_stats 必须返回 200，才 真能干活"
        if ready {
            let status = http_status("127.0.0.1", port, "/system_stats", Duration::from_secs(5));
            println!("GET /system_stats  {status:?}");
            assert_eq!(status, Some(200), "/system_stats 未返 200");

            let health = probe_health("127.0.0.1", port).expect("应采到显 队列");
            println!(
                "显存 {} / {} MiB，队 {}+{}",
                health.vram_used_mib,
                health.vram_total_mib,
                health.queue_running,
                health.queue_pending
            );
            assert_eq!(health.vram_total_mib, 32606, "RTX 5090 应报 32606 MiB");
            assert!(health.vram_used_mib > 0, "空载也应有非零显存占 ");
        }
        let _ = handle.kill_tree();
        assert!(
            ready,
            "ComfyUI 未在 150s 内就绪（日志 {} ",
            log_path.display()
        );
    }

    #[test]
    fn detects_python_layouts() {
        let dir = std::env::temp_dir().join(format!("comfyfleet-pyenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // 便携版：<root>/python_embeded/python.exe
        let embed = dir.join("portable");
        std::fs::create_dir_all(embed.join("python_embeded")).unwrap();
        std::fs::write(embed.join("python_embeded").join("python.exe"), b"").unwrap();
        let py = detect_python(&embed).expect("便携版应被认 ");
        assert_eq!(py.root, embed.join("python_embeded"));
        assert!(!py.venv_style, "便携版不 venv");
        assert!(!py.managed);

        // venv root>/.venv/Scripts/python.exe
        let venv_root = dir.join("withvenv");
        let scripts = venv_root.join(".venv").join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("python.exe"), b"").unwrap();
        let py = detect_python(&venv_root).expect("venv");
        assert_eq!(py.root, venv_root.join(".venv"));
        assert!(py.venv_style);
        // 直接给解释器路径也认，环境根取上一。
        let py2 = detect_python(&scripts.join("python.exe")).expect("interpreter");
        assert_eq!(py2.root, venv_root.join(".venv"));

        assert!(detect_python(&dir.join("nope")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_python_follows_instance_mode() {
        let dir =
            std::env::temp_dir().join(format!("comfyfleet-pyenv-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("env").join("Scripts")).unwrap();
        std::fs::write(dir.join("env").join("Scripts").join("python.exe"), b"").unwrap();

        let mut inst = sample("127.0.0.1");
        inst.root_dir = dir.to_string_lossy().to_string();
        // existing mode with a relative path
        inst.python = crate::config::PythonEnv {
            mode: "existing".into(),
            path: Some("env".into()),
        };
        let py = resolve_python(&inst).expect("existing env should resolve");
        assert_eq!(
            py.python,
            dir.join("env").join("Scripts").join("python.exe")
        );
        assert!(!py.managed);

        // managed mode with no .venv must return None.
        inst.python = Default::default();
        assert!(resolve_python(&inst).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
