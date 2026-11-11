//! 环境探测与 wheel 兼容性判断。
//!
//! 两件事：
//! 1. **探测**：问实例的 Python，报出 Python ABI、平台、torch / CUDA 与 C++ ABI。
//! 2. **兼容性**：按 wheel 文件名里的 Python、平台、torch、CUDA、cxx11 标签判断能不能装。
//!
//! 仓库索引、轮子筛选和安装入口在 `wheel_repo`；这里只保留共用的底层能力。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::Channel;

use crate::commands::version::VersionEvent;
use crate::config::{self, GlobalSettings};
use crate::error::AppError;
use crate::runtime;

const UA: &str = "ComfyFleet/0.1 (+https://github.com/)";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvReport {
    /// 3.12.7
    pub python: String,
    /// cp312
    pub abi: String,
    /// win_amd64 / linux_x86_64
    pub platform: String,
    pub torch: Option<String>,
    /// 12.4
    pub torch_cuda: Option<String>,
    /// Windows 上一般读不到；Linux 上决定 flash-attn 轮子的 cxx11 标签
    pub cxx11: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torchvision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torchaudio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torchcodec: Option<String>,
}

// ---------------------------------------------------------------- 探测

/// 一次问清所有信息；import torch 失败也不算错（CPU 档 / 没装 torch）。
const PROBE_PY: &str = r#"
import json, sys, platform
out = {}
out["python"] = platform.python_version()
out["abi"] = "cp%d%d" % sys.version_info[:2]
m = platform.machine().lower()
if sys.platform == "win32":
    out["platform"] = "win_amd64" if m in ("amd64", "x86_64") else "win_" + m
elif sys.platform.startswith("linux"):
    out["platform"] = "linux_x86_64" if m in ("x86_64", "amd64") else "linux_" + m
else:
    out["platform"] = (sys.platform + "_" + m).replace("-", "_")
try:
    import importlib.metadata as md
    def ver(n):
        try:
            return md.version(n)
        except Exception:
            return None
except Exception:
    def ver(n):
        return None
out["torch"] = ver("torch")
out["torchvision"] = ver("torchvision")
out["torchaudio"] = ver("torchaudio")
out["torchcodec"] = ver("torchcodec")
out["torch_cuda"] = None
out["cxx11"] = None
try:
    import torch
    out["torch_cuda"] = torch.version.cuda
    try:
        out["cxx11"] = bool(torch._C._GLIBCXX_USE_CXX11_ABI)
    except Exception:
        pass
except Exception:
    pass
print(json.dumps(out))
"#;

pub fn inspect(python: &Path) -> Result<EnvReport, AppError> {
    let mut env = BTreeMap::new();
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    let out = crate::proc::run(python, &["-c", PROBE_PY], None, &env)?;
    if !out.ok() {
        return Err(AppError::new(
            "ENV_PROBE_FAILED",
            format!("读环境信息失败（{}）：\n{}", python.display(), out.detail()),
        ));
    }
    env_from_json(&out.stdout)
}

fn env_from_json(raw: &str) -> Result<EnvReport, AppError> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| {
        AppError::new(
            "ENV_PROBE_JSON",
            format!("环境信息不是合法 JSON（{e}）：{raw}"),
        )
    })?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    Ok(EnvReport {
        python: text("python").unwrap_or_default(),
        abi: text("abi").unwrap_or_default(),
        platform: text("platform").unwrap_or_default(),
        torch: text("torch"),
        torch_cuda: text("torch_cuda"),
        cxx11: value.get("cxx11").and_then(|v| v.as_bool()),
        torchvision: text("torchvision"),
        torchaudio: text("torchaudio"),
        torchcodec: text("torchcodec"),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WheelMatch {
    pub(crate) label: String,
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WheelTags {
    pub(crate) python: String,
    pub(crate) abi: String,
    pub(crate) platform: String,
    pub(crate) torch: Option<String>,
    pub(crate) cuda: Option<String>,
    pub(crate) cxx11: Option<bool>,
}

pub(crate) fn parse_wheel_name(name: &str) -> Option<WheelTags> {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".whl") {
        return None;
    }
    let stem = &name[..name.len() - 4];
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
    if parts.len() < 4 {
        return None;
    }
    let platform = parts[0].to_ascii_lowercase();
    let abi = parts[1].to_ascii_lowercase();
    let python = parts[2].to_ascii_lowercase();
    let rest = parts[3];
    let version = rest
        .split_once('-')
        .map(|(_, v)| v)
        .unwrap_or("")
        .to_ascii_lowercase();
    let cxx11 = if version.contains("cxx11abitrue") {
        Some(true)
    } else if version.contains("cxx11abifalse") {
        Some(false)
    } else {
        None
    };
    Some(WheelTags {
        python,
        abi,
        platform,
        torch: scan_version(&version, "torch"),
        cuda: scan_version(&version, "cu"),
        cxx11,
    })
}

/// 从版本段里抠 `torch2.6` / `cu12.4` 这样的标签
fn scan_version(haystack: &str, prefix: &str) -> Option<String> {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(prefix) {
        let start = from + pos + prefix.len();
        let mut end = start;
        let mut seen_digit = false;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
            if bytes[end].is_ascii_digit() {
                seen_digit = true;
            }
            end += 1;
        }
        if seen_digit {
            return Some(haystack[start..end].trim_end_matches('.').to_string());
        }
        from = from + pos + 1;
    }
    None
}

/// 平台标签对不上就直接判死；Python 标签允许通用轮、`abi3` 和`py3X-none`
pub(crate) fn python_ok(tags: &WheelTags, env: &EnvReport) -> bool {
    let py = tags.python.as_str();
    if py == "py3" || py == "py2.py3" {
        return true;
    }
    if py == env.abi {
        return true;
    }
    if tags.abi == "abi3" {
        if let Some(ver) = py.strip_prefix("cp") {
            let tag = ver.parse::<u32>().ok();
            let cur = env
                .abi
                .strip_prefix("cp")
                .and_then(|s| s.parse::<u32>().ok());
            if let (Some(t), Some(c)) = (tag, cur) {
                return c >= t;
            }
        }
    }
    // py39-none-win_amd64 这种「纯 Python + 平台」标签，pip 认为 Python 3.9 及以上都能装
    // 所以 sys_tags 在 3.13 也包含 py39-none-win_amd64；但 xformers 的 py39 轮子就是这一类。
    if tags.abi == "none" {
        if let Some(rest) = py.strip_prefix("py") {
            if rest == "3" {
                return true;
            }
            if let Some(minor) = rest.strip_prefix('3') {
                if !minor.is_empty() && minor.chars().all(|c| c.is_ascii_digit()) {
                    let tag_minor = minor.parse::<u32>().unwrap_or(u32::MAX);
                    let env_minor = env
                        .python
                        .split('.')
                        .nth(1)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    return env.python.starts_with("3.") && tag_minor <= env_minor;
                }
            }
        }
    }
    false
}

pub(crate) fn platform_ok(tags: &WheelTags, env: &EnvReport) -> bool {
    let p = tags.platform.as_str();
    if p == "any" {
        return true;
    }
    let want = env.platform.to_ascii_lowercase();
    if want == "win_amd64" {
        return p == "win_amd64";
    }
    if want == "linux_x86_64" {
        return p == "linux_x86_64" || (p.contains("manylinux") && p.ends_with("x86_64"));
    }
    p == want
}

pub(crate) fn torch_ok(tags: &WheelTags, env: &EnvReport) -> bool {
    let Some(tag) = &tags.torch else {
        return true;
    };
    let Some(env_torch) = &env.torch else {
        return false;
    };
    let want = major_minor(env_torch);
    // 轮子标签有 torch2.6 / torch2.10.0 两种写法，统一按 major.minor 比较
    major_minor(tag) == want
}

fn major_minor(version: &str) -> String {
    let base = version.split('+').next().unwrap_or(version);
    let mut parts = base.split('.');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        _ => base.to_string(),
    }
}

fn cuda_norm(tag: &str) -> String {
    tag.chars().filter(|c| c.is_ascii_digit()).collect()
}

pub(crate) fn cuda_ok(tags: &WheelTags, env: &EnvReport) -> bool {
    let Some(tag) = &tags.cuda else {
        return true;
    };
    let Some(env_cuda) = &env.torch_cuda else {
        // CPU 版 torch：任何带 CUDA 标签的轮子都别装
        return false;
    };
    let a = cuda_norm(tag);
    let b = cuda_norm(env_cuda);
    if a.len() < 2 || b.len() < 2 {
        return true;
    }
    // cu12 这种大版本标签：只要前两位一致就算兼容
    if a.len() == 2 {
        return b.starts_with(&a);
    }
    a == b
}

fn cxx11_ok(tags: &WheelTags, env: &EnvReport) -> bool {
    match (tags.cxx11, env.cxx11) {
        (Some(tag), Some(env_abi)) => tag == env_abi,
        // 读不到ABI 时不敢拦，给个提示由用户决定
        _ => true,
    }
}

pub(crate) fn wheel_compat(name: &str, env: &EnvReport) -> Result<WheelMatch, &'static str> {
    let tags = parse_wheel_name(name).ok_or("文件名不是标准 wheel 格式")?;
    if !python_ok(&tags, env) {
        return Err("Python 版本");
    }
    if !platform_ok(&tags, env) {
        return Err("操作系统 / 架构");
    }
    if !torch_ok(&tags, env) {
        return Err("torch 版本");
    }
    if !cuda_ok(&tags, env) {
        return Err("CUDA 版本");
    }
    if !cxx11_ok(&tags, env) {
        return Err("C++ ABI（cxx11）");
    }
    let env_bits = format!(
        "适配 Python {} · {} · torch {} · CUDA {}",
        env.python,
        env.platform,
        env.torch.as_deref().unwrap_or("—"),
        env.torch_cuda.as_deref().unwrap_or("—")
    );
    let label = if tags.platform == "any" {
        format!("通用轮 · {env_bits}")
    } else {
        env_bits
    };
    let note = if tags.platform == "any" {
        Some("纯 Python 通用轮，任何环境都能装".to_string())
    } else if tags.cxx11.is_some() && env.cxx11.is_none() && env.platform.starts_with("linux") {
        // Windows 的DLL 不看 glibcxx ABI，只有Linux 才需要在意
        Some("文件名带 cxx11 标签，但环境里读不到 ABI；装坏了用快照回滚".to_string())
    } else {
        None
    };
    Ok(WheelMatch { label, note })
}

// ---------------------------------------------------------------- 安装

/// 通用安装入口：云端仓库里的 wheel 共用同一套 torch 约束与重试逻辑。
pub fn install_wheel(
    inst: &crate::config::InstanceConfig,
    name: &str,
    pypi: &str,
    target: &str,
    settings: &GlobalSettings,
    on: &Channel<VersionEvent>,
) -> Result<(), AppError> {
    let py = runtime::resolve_python(inst).ok_or_else(|| {
        AppError::new(
            "NO_PYTHON",
            "这个实例还没认出 Python 环境（设置 → Python 环境里指一下）。",
        )
    })?;
    let uv = crate::commands::env::resolve_uv()?;
    let uv_cache = config::shared_dir().join("uv-cache");
    std::fs::create_dir_all(&uv_cache)?;

    let mut env = BTreeMap::new();
    env.insert(
        "UV_CACHE_DIR".to_string(),
        uv_cache.to_string_lossy().to_string(),
    );
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    if py.venv_style {
        env.insert(
            "VIRTUAL_ENV".to_string(),
            py.root.to_string_lossy().to_string(),
        );
    }

    // 锁住现有 torch 家族：装轮子不该悄悄换掉 torch
    let probe = inspect(&py.python).ok();
    let constraints = config::shared_dir().join("accel").join("constraints.txt");
    let mut pins = String::new();
    if let Some(info) = &probe {
        for (name, version) in [
            ("torch", info.torch.clone()),
            ("torchvision", info.torchvision.clone()),
            ("torchaudio", info.torchaudio.clone()),
            ("torchcodec", info.torchcodec.clone()),
        ] {
            if let Some(v) = version {
                pins.push_str(&format!("{name}=={v}\n"));
            }
        }
        // 没装 torchcodec 时按对照表补一条，避免轮子依赖顺手装错版本
        if info.torchcodec.is_none() {
            if let Some(pin) = info.torch.as_deref().and_then(crate::compat::stack_for) {
                pins.push_str(&format!("torchcodec=={}\n", pin.torchcodec));
            }
        }
    }
    let constraint_arg = if pins.is_empty() {
        None
    } else {
        if let Some(dir) = constraints.parent() {
            std::fs::create_dir_all(dir)?;
        }
        config::atomic_write(&constraints, pins.as_bytes())?;
        let _ = on.send(VersionEvent::info(
            "accel",
            "已把当前 torch / torchvision / torchaudio / torchcodec 钉成约束：依赖冲突会直接报错，不会偷偷换掉你的 torch",
        ));
        Some(constraints.to_string_lossy().to_string())
    };

    let url = proxy_url(settings, target);
    let candidates = crate::pip::index_candidates(settings.mirrors.pypi.as_deref());
    let mut last_detail = String::new();
    let mut last_index = String::new();
    let mut local_wheel: Option<CachedWheel> = None;
    let mut manual_download = false;
    // 直接 URL 安装时 uv 会把下载进度吞掉（非 TTY 不输出百分比）。
    // 先自己带进度下载到本地，再让 uv 装本地文件，控制台才能看到真实下载进度。
    if url.starts_with("http") {
        match download_and_normalize(&url, pypi, on) {
            Ok(w) => {
                local_wheel = Some(w);
                manual_download = true;
            }
            Err(e) => {
                let _ = on.send(VersionEvent::warn(
                    "accel",
                    format!("预下载轮子失败，改回让 uv 直接下载：{}", e.message),
                ));
            }
        }
    }
    let mut renamed = false;
    let mut n = 0usize;
    while n < candidates.len() {
        let index_url = candidates[n].clone();
        if n > 0 {
            let _ = on.send(VersionEvent::warn(
                "accel",
                format!("上一个源装不上，改用 {index_url} 重试"),
            ));
        }
        let install_target = match &local_wheel {
            Some(w) => w.path.to_string_lossy().to_string(),
            None => url.clone(),
        };
        let mut args: Vec<String> = vec![
            "pip".into(),
            "install".into(),
            "--python".into(),
            py.python.to_string_lossy().to_string(),
            install_target,
            "--index-url".into(),
            index_url.to_string(),
        ];
        if let Some(c) = &constraint_arg {
            args.push("--constraint".into());
            args.push(c.clone());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let _ = on.send(VersionEvent::console(format!("$ uv {}", argv.join(" "))));
        let out = crate::proc::run_stream(&uv, &argv, None, &env, &mut |line| {
            let _ = on.send(VersionEvent::console(line));
        })?;
        if out.ok() {
            if (renamed || manual_download) && target.starts_with("http") {
                // 本地安装的轮子 direct_url 会指向临时文件；把原始 URL 钉回去，
                // 临时文件删除后同步仍能重新下载。
                let _ = record_wheel_origin(&py, pypi, target);
            }
            let tail = tail_lines(&out.detail(), 40);
            let _ = on.send(VersionEvent::done("accel", format!("{} 安装完成", name)));
            let note = if renamed {
                "\n（社区轮子的文件名版本与包内元数据不一致，已按 METADATA 改名后安装）"
            } else {
                ""
            };
            crate::activity::record(
                &inst.id,
                "accel-install",
                "ok",
                format!("安装轮子 · {}", name),
                Some(format!("{target}\n{note}\n{tail}")),
            );
            return Ok(());
        }
        last_detail = out.detail();
        last_index = index_url.to_string();

        // 社区构建常见：文件名是 2.2.0.post3，METADATA 却是 2.2.0+cu...，uv 拒装。
        // 下载下来按 METADATA 的版本改名，再用本地文件装，内容和校验都不变。
        if local_wheel.is_none()
            && target.starts_with("http")
            && last_detail.contains("Wheel version does not match filename")
        {
            let _ = on.send(VersionEvent::warn(
                "accel",
                "这个轮子的文件名版本号和包内 METADATA 不一致（uv 会拒绝），正在按 METADATA 改名后重试",
            ));
            match download_and_normalize(&url, pypi, on) {
                Ok(w) => {
                    local_wheel = Some(w);
                    renamed = true;
                    continue; // 同一个源，换成改名后的本地文件重试
                }
                Err(e) => {
                    let _ = on.send(VersionEvent::warn(
                        "accel",
                        format!("自动改名失败，{}", e.message),
                    ));
                }
            }
        }
        n += 1;
    }

    let _ = on.send(VersionEvent::failed(
        "accel",
        format!("{} 安装失败：\n{last_detail}", name),
    ));
    crate::activity::record(
        &inst.id,
        "accel-install",
        "error",
        format!("安装轮子失败：{}", name),
        Some(format!("{target}\nindex {last_index}\n{last_detail}")),
    );
    Err(AppError::new(
        "ACCEL_FAILED",
        format!(
            "{} 没装上（index {last_index}）：\n{}\n\n如果报的是 torch 冲突，说明这个轮子要求别的 torch 版本——启动器不会替你换 torch，请先拍快照再手动处理。",
            name,
            tail_lines(&last_detail, 12)
        ),
    ))
}

/// 找到刚装好的 dist-info，把原始下载 URL 记进 PEP 610 的 `direct_url.json`。
///
/// 规范化后的轮子只是临时文件；安装记录保留原始 URL，后续同步重新下载。
/// 未知字段按 PEP 610 会被忽略，不影响 pip / uv 识别包。
fn record_wheel_origin(py: &runtime::PyEnv, pypi: &str, origin: &str) -> Option<()> {
    let mut site_dirs = vec![py.root.join("Lib").join("site-packages")];
    if let Ok(entries) = std::fs::read_dir(py.root.join("lib")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("python") {
                site_dirs.push(entry.path().join("site-packages"));
            }
        }
    }
    let prefix = format!("{}-", pypi.replace('-', "_").to_ascii_lowercase());
    for site in site_dirs {
        let Ok(entries) = std::fs::read_dir(&site) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_ascii_lowercase();
            if !name.starts_with(&prefix) || !name.ends_with(".dist-info") {
                continue;
            }
            let path = entry.path().join("direct_url.json");
            let mut value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| serde_json::json!({}));
            value["comfyfleet_origin"] = serde_json::json!({ "url": origin });
            let bytes = serde_json::to_vec(&value).ok()?;
            if crate::config::atomic_write(&path, &bytes).is_ok() {
                return Some(());
            }
        }
    }
    None
}

/// 下载中的原始文件：失败/提前返回时删掉；成功交给 CachedWheel 管理。
struct TempDownload {
    path: PathBuf,
    armed: bool,
}

impl TempDownload {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 下载并改名后的 wheel：只在本次安装期间存在，装完立即删除。
/// 安装记录里已保留原始 URL，后续同步/重装直接重新下载，不靠本地文件。
struct CachedWheel {
    path: PathBuf,
}

impl Drop for CachedWheel {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 下载轮子，按包内 METADATA 的版本重命名后再交给 uv。
fn download_and_normalize(
    url: &str,
    pypi: &str,
    on: &Channel<VersionEvent>,
) -> Result<CachedWheel, AppError> {
    use std::io::Read as _;

    let dir = config::shared_dir().join("accel").join("downloads");
    std::fs::create_dir_all(&dir)?;
    let raw_name = url
        .rsplit('/')
        .next()
        .unwrap_or("wheel.whl")
        .split('?')
        .next()
        .unwrap_or("wheel.whl")
        .to_string();
    // ModelScope 等仓库会把 wheel 版本里的 `+` 编码成 `%2B`；还原后才能按
    // 标准 wheel 文件名解析。其他 `%XX` 也一并还原。
    let raw_name = crate::sync::percent_decode(&raw_name);
    // URL 最后一段是不可信输入：不能带分隔符/`..`，否则 File::create 会写出下载目录。
    let raw_name = if !raw_name.is_empty()
        && raw_name.len() <= 240
        && raw_name != "."
        && raw_name != ".."
        && raw_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
    {
        raw_name
    } else {
        "wheel.whl".to_string()
    };
    let tmp = dir.join(&raw_name);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(1800))
        .user_agent(UA)
        .build();
    let _ = on.send(VersionEvent::console(format!("下载轮子：{url}")));
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => {
            AppError::new("WHEEL_DOWNLOAD", format!("下载轮子失败：HTTP {code}"))
        }
        other => AppError::new("WHEEL_DOWNLOAD", format!("下载轮子失败，{other}")),
    })?;
    const MAX_WHEEL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    let total = resp
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if total > MAX_WHEEL_BYTES {
        return Err(AppError::new(
            "WHEEL_TOO_LARGE",
            format!("轮子文件太大（{total} 字节），拒绝下载"),
        ));
    }
    // 先给前端一条依赖标题，后面的真实下载百分比会原样更新这一行。
    let label = raw_name.split('-').next().unwrap_or("wheel").to_string();
    let title = if total > 0 {
        format!(
            "Downloading {label} ({})",
            crate::sync::human_bytes(total)
        )
    } else {
        format!("Downloading {label}")
    };
    let _ = on.send(VersionEvent::console(title));
    let mut reader = resp.into_reader();
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        let mut buf = [0u8; 64 * 1024];
        let mut written = 0u64;
        let mut last_at = std::time::Instant::now();
        let mut last_bytes = 0u64;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| AppError::new("WHEEL_DOWNLOAD", format!("读轮子失败：{e}")))?;
            if n == 0 {
                break;
            }
            written = written.saturating_add(n as u64);
            if written > MAX_WHEEL_BYTES {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(AppError::new("WHEEL_TOO_LARGE", "轮子超过 8 GiB，已中止"));
            }
            file.write_all(&buf[..n])
                .map_err(|e| AppError::new("WHEEL_DOWNLOAD", format!("写临时轮子失败：{e}")))?;
            let elapsed = last_at.elapsed().as_secs_f64();
            // 进度事件仍然发，控制台会把连续进度压在同一行原地刷新；
            // 这里降到 1 秒一次，避免 Activity 日志也被刷爆。
            if elapsed >= 1.0 {
                let rate = written.saturating_sub(last_bytes) as f64 / elapsed;
                let pct = if total > 0 { written * 100 / total } else { 0 };
                let size = if total > 0 {
                    format!(
                        "{}/{}",
                        crate::sync::human_bytes(written),
                        crate::sync::human_bytes(total)
                    )
                } else {
                    crate::sync::human_bytes(written)
                };
                let _ = on.send(VersionEvent::console(format!(
                    "下载轮子：{pct}% · {size} · {}",
                    crate::sync::human_rate(rate)
                )));
                last_at = std::time::Instant::now();
                last_bytes = written;
            }
        }
        file.sync_all()
            .map_err(|e| AppError::new("WHEEL_DOWNLOAD", format!("落盘轮子失败：{e}")))?;
        let _ = on.send(VersionEvent::console(if total > 0 {
            format!(
                "下载轮子完成：100% · {} / {}",
                crate::sync::human_bytes(written),
                crate::sync::human_bytes(total)
            )
        } else {
            format!("下载轮子完成：{}", crate::sync::human_bytes(written))
        }));
    }
    let guard = TempDownload {
        path: tmp.clone(),
        armed: true,
    };

    let mut version = None;
    let mut dist = None;
    let file = std::fs::File::open(&tmp)?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("WHEEL_BROKEN", format!("轮子不是合法 zip：{e}")))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| AppError::new("WHEEL_BROKEN", format!("读轮子目录失败：{e}")))?;
        if !entry.name().ends_with(".dist-info/METADATA") {
            continue;
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|e| AppError::new("WHEEL_BROKEN", format!("读 METADATA 失败：{e}")))?;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Version:") {
                version = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Name:") {
                dist = Some(v.trim().to_string());
            }
        }
        break;
    }
    let version = version
        .ok_or_else(|| AppError::new("WHEEL_BROKEN", "轮子里没有 METADATA 的 Version 字段"))?;
    let dist = dist.unwrap_or_else(|| pypi.to_string());
    let dist = dist.to_ascii_lowercase().replace(['-', '.'], "_");
    if version.is_empty()
        || version.len() > 100
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '!' | '_' | '-'))
        || dist.is_empty()
        || dist.len() > 100
        || !dist.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AppError::new(
            "WHEEL_METADATA",
            "轮子 METADATA 里的名称/版本包含不安全字符",
        ));
    }
    let tags = parse_wheel_name(&raw_name)
        .ok_or_else(|| AppError::new("WHEEL_BROKEN", "Wheel filename is not canonical"))?;
    let canonical = format!(
        "{dist}-{version}-{}-{}-{}.whl",
        tags.python, tags.abi, tags.platform
    );
    let dest = dir.join(&canonical);
    // 标准文件名通常 tmp == dest：File::create 已经覆盖，不能再 rename 自己。
    if tmp != dest {
        // 之前中断留下的旧文件会让 Windows 的 rename 失败；这是临时目录，直接替换。
        if dest.exists() {
            let _ = std::fs::remove_file(&dest);
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| AppError::new("WHEEL_RENAME", format!("改名失败，{e}")))?;
    }
    guard.disarm();
    let _ = on.send(VersionEvent::console(format!(
        "按 METADATA 改名为 {canonical}，改用本地文件安装"
    )));
    Ok(CachedWheel { path: dest })
}

/// 配置了 GitHub 代理时，只给 GitHub 下载地址套一层。
///
/// 现在轮子主要来自 ModelScope，把它的 URL 拼到 GitHub 代理后面会直接 404：
/// ModelScope 下载走自己的域名（需要镜像的用户在系统层配置 hosts/代理）。
fn proxy_url(settings: &GlobalSettings, target: &str) -> String {
    let is_github = target.contains("github.com") || target.contains("githubusercontent.com");
    if !target.starts_with("http") || !is_github {
        return target.to_string();
    }
    match settings
        .mirrors
        .github_proxy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(proxy) => format!("{}/{}", proxy.trim_end_matches('/'), target),
        None => target.to_string(),
    }
}

fn tail_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.trim_end().to_string();
    }
    let skipped = lines.len() - max;
    format!("…（省略前{skipped} 行）\n{}", lines[skipped..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_env() -> EnvReport {
        EnvReport {
            python: "3.12.7".into(),
            abi: "cp312".into(),
            platform: "win_amd64".into(),
            torch: Some("2.6.0+cu124".into()),
            torch_cuda: Some("12.4".into()),
            cxx11: None,
            torchvision: None,
            torchaudio: None,
            torchcodec: None,
        }
    }

    #[test]
    fn parses_wheel_tags() {
        let t = parse_wheel_name(
            "flash_attn-2.8.3.post2+cu12torch2.6cxx11abiFALSE-cp312-cp312-linux_x86_64.whl",
        )
        .unwrap();
        assert_eq!(t.python, "cp312");
        assert_eq!(t.platform, "linux_x86_64");
        assert_eq!(t.torch.as_deref(), Some("2.6"));
        assert_eq!(t.cuda.as_deref(), Some("12"));
        assert_eq!(t.cxx11, Some(false));

        let u = parse_wheel_name("xformers-0.0.35-py39-none-win_amd64.whl").unwrap();
        assert_eq!(u.python, "py39");
        assert_eq!(u.abi, "none");
        assert_eq!(u.platform, "win_amd64");
        assert!(u.torch.is_none());
    }

    #[test]
    fn rejects_linux_and_old_python_wheels_on_windows() {
        let env = win_env();
        assert!(wheel_compat(
            "flash_attn-2.8.3.post2+cu12torch2.6cxx11abiFALSE-cp312-cp312-linux_x86_64.whl",
            &env
        )
        .is_err());
        assert!(wheel_compat("xformers-0.0.35-py313-none-win_amd64.whl", &env).is_err());
        assert!(wheel_compat(
            "nunchaku-0.16.1+cu12.4torch2.6-cp312-cp312-win_amd64.whl",
            &env
        )
        .is_ok());
    }

    #[test]
    fn torch_and_cuda_must_match() {
        let env = win_env();
        assert!(wheel_compat(
            "nunchaku-0.16.1+cu12.8torch2.6-cp312-cp312-win_amd64.whl",
            &env
        )
        .is_err());
        assert!(wheel_compat(
            "nunchaku-0.16.1+cu12.4torch2.5-cp312-cp312-win_amd64.whl",
            &env
        )
        .is_err());
        assert!(wheel_compat(
            "nunchaku-0.16.1+cu12.4torch2.6-cp312-cp312-win_amd64.whl",
            &env
        )
        .is_ok());
    }

    #[test]
    fn universal_wheel_is_accepted() {
        let env = win_env();
        let m = wheel_compat("flash_attn_4-4.0.0b37-py3-none-any.whl", &env).unwrap();
        assert!(m.label.contains("通用轮"));
    }

    #[test]
    fn accepts_py39_none_xformers_wheel_on_newer_python() {
        let mut env = win_env();
        env.python = "3.13.6".into();
        env.abi = "cp313".into();
        env.torch = Some("2.14.1+cu130".into());
        env.torch_cuda = Some("13.0".into());
        let name = "xformers-0.0.35+d20260902.cu130torch2.14.0cxx11abi1-py39-none-win_amd64.whl";
        assert!(
            wheel_compat(name, &env).is_ok(),
            "py39-none-win_amd64 在Python 3.13 上是 pip 认可的标签"
        );

        let mut old = env.clone();
        old.python = "3.8.10".into();
        old.abi = "cp38".into();
        assert!(wheel_compat(name, &old).is_err(), "3.8 装不了py39 轮子");

        let mut future = win_env();
        future.python = "3.12.7".into();
        future.abi = "cp312".into();
        assert!(
            wheel_compat("xformers-0.0.35-py313-none-win_amd64.whl", &future).is_err(),
            "3.12 不能装py313 轮子"
        );
    }

    #[test]
    fn cpu_env_rejects_cuda_wheels() {
        let mut env = win_env();
        env.torch = Some("2.6.0".into());
        env.torch_cuda = None;
        assert!(wheel_compat(
            "nunchaku-0.16.1+cu12.4torch2.6-cp312-cp312-win_amd64.whl",
            &env
        )
        .is_err());
    }

    #[test]
    fn downloaded_wheel_is_removed_after_use() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "comfyfleet-accel-drop-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo-1.0.0-py3-none-any.whl");
        std::fs::write(&path, b"wheel").unwrap();
        {
            let wheel = CachedWheel { path: path.clone() };
            assert!(wheel.path.exists());
        }
        assert!(!path.exists(), "安装结束必须删掉临时轮子");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_from_json_reads_versions() {
        let raw = r#"{
            "python":"3.12.7","abi":"cp312","platform":"win_amd64",
            "torch":"2.6.0+cu124","torch_cuda":"12.4","cxx11":null
        }"#;
        let env = env_from_json(raw).unwrap();
        assert_eq!(env.python, "3.12.7");
        assert_eq!(env.torch.as_deref(), Some("2.6.0+cu124"));
        assert_eq!(env.torch_cuda.as_deref(), Some("12.4"));
    }
}
