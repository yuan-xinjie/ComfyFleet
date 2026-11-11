//! 可选运行时（uv / MinGit + git-lfs）的按需下载。
//!
//! 这三个二进制合计约 62 MB，不再打进安装包（安装包从 55 MB 降到约 10 MB）：
//! 用户真正要用到时，在「设置 → 运行时」里点一下下载，之后缓存在数据目录。
//! 默认一个都不装，装不装、装哪个由用户自己选。
//!
//! 下载顺序：ModelScope 镜像 → GitHub（设置了 github 镜像前缀就走代理）。
//! 镜像目录约定为仓库里的 `ComfyFleet/runtime/<文件名>`，没传过时 404 自动回退。
//!
//! 版本号与 SHA256 必须与远端一致，否则拒绝安装——这些二进制会被直接执行，
//! 校验失败还装进去等于开了个任意代码执行的口子。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tauri::ipc::Channel;

use crate::config;
use crate::error::AppError;

/// uv 版本
pub const UV_VERSION: &str = "0.8.8";
/// MinGit 版本（解包目录名也用它做版本隔离）
pub const GIT_VERSION: &str = "2.56.0.4";
/// git-lfs 版本
pub const LFS_VERSION: &str = "3.8.1";

/// 可选镜像：把三个 zip 传到 ModelScope 仓库的 `ComfyFleet/runtime/` 下即可启用；
/// 没传过就 404，自动回退 GitHub。
const MIRROR_BASE: &str =
    "https://www.modelscope.cn/models/yuanxinjie/AI-windows-whl/resolve/master/ComfyFleet/runtime/";

struct Artifact {
    /// 镜像与 GitHub 统一使用的文件名
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// 展示用体积（MB）
    size_mb: f64,
}

const UV: Artifact = Artifact {
    file: "uv-x86_64-pc-windows-msvc-0.8.8.zip",
    url: "https://github.com/astral-sh/uv/releases/download/0.8.8/uv-x86_64-pc-windows-msvc.zip",
    sha256: "AC76B826E5BA70C7A041E9EE344CBCBA2919CDD3010FFF7F1A83B7864911633E",
    size_mb: 18.7,
};

const GIT: Artifact = Artifact {
    file: "MinGit-2.56.0.4-64-bit.zip",
    url: "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.4/MinGit-2.56.0.4-64-bit.zip",
    sha256: "E3BF78351FE777FBA47DDEE5950082F8A98BB0F02604FDC7BA3EE2CB68FFA877",
    size_mb: 38.0,
};

const GITLFS: Artifact = Artifact {
    file: "git-lfs-windows-amd64-v3.8.1.zip",
    url: "https://github.com/git-lfs/git-lfs/releases/download/v3.8.1/git-lfs-windows-amd64-v3.8.1.zip",
    sha256: "5BB771C12B3DACC535E912D0270A010C209D4DA2E63819C5918E3547C2DEFED9",
    size_mb: 5.7,
};

// ------------------------------------------------------------------ 路径

/// 所有可选运行时统一放在 `<数据目录>\runtime\` 下，卸载/迁移/排查都只看这一个目录：
///
/// ```text
/// <数据目录>\runtime\
///   uv\uv-0.8.8\uv.exe
///   git\mingit-2.56.0.4\...
/// ```
pub fn runtime_root() -> PathBuf {
    config::app_root().join("runtime")
}

/// uv 落盘位置
pub fn uv_exe() -> PathBuf {
    runtime_root()
        .join("uv")
        .join(format!("uv-{UV_VERSION}"))
        .join("uv.exe")
}

/// MinGit 解包位置
pub fn git_root() -> PathBuf {
    runtime_root()
        .join("git")
        .join(format!("mingit-{GIT_VERSION}"))
}

pub fn git_exe() -> PathBuf {
    git_root().join("cmd").join("git.exe")
}

/// git-lfs 必须落在 git 的 exec-path 里，`git lfs` 才找得到（实测干净 PATH 下有效）。
fn lfs_exe() -> PathBuf {
    git_root()
        .join("ucrt64")
        .join("libexec")
        .join("git-core")
        .join("git-lfs.exe")
}

pub fn uv_installed() -> bool {
    uv_exe().is_file()
}

pub fn git_installed() -> bool {
    git_exe().is_file()
}

pub fn lfs_installed() -> bool {
    lfs_exe().is_file()
}

/// 把 0.1 版本留下的老位置迁移到统一的 `runtime` 目录，避免老用户重下 90 MB：
/// - uv：`<安装目录>\resources\uv.exe` → `<数据目录>\runtime\uv\uv-<版本>\uv.exe`
/// - git：`<数据目录>\git\mingit-<版本>` → `<数据目录>\runtime\git\mingit-<版本>`
///
/// 启动时调一次；失败只记日志——解析侧仍保留老路径回退，不影响使用。
pub fn migrate_legacy() {
    if !uv_installed() {
        let legacy = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
            .map(|dir| dir.join("resources").join("uv.exe"))
            .filter(|p| p.is_file());
        if let Some(src) = legacy {
            if let Some(parent) = uv_exe().parent() {
                if std::fs::create_dir_all(parent).is_ok() {
                    // 优先 move（同盘秒完成）；跨盘 rename 失败才退回 copy + 删源，
                    // 免得安装目录里白白留一份 54 MB 的旧 uv.exe
                    let moved = std::fs::rename(&src, uv_exe()).is_ok()
                        || (std::fs::copy(&src, uv_exe()).is_ok()
                            && std::fs::remove_file(&src).is_ok());
                    if moved {
                        log::info!("uv 已迁移到 {}", uv_exe().display());
                    }
                }
            }
        }
    }

    if !git_installed() {
        let legacy = config::app_root()
            .join("git")
            .join(format!("mingit-{GIT_VERSION}"));
        if legacy.is_dir() {
            if let Some(parent) = git_root().parent() {
                if std::fs::create_dir_all(parent).is_ok()
                    && std::fs::rename(&legacy, git_root()).is_ok()
                {
                    log::info!("MinGit 已迁移到 {}", git_root().display());
                }
            }
        }
    }
}

// ------------------------------------------------------------------ 状态

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePart {
    pub installed: bool,
    /// 已安装时从二进制里读到的版本，未安装为 None
    pub version: Option<String>,
    pub path: String,
    pub size_mb: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub uv: RuntimePart,
    pub git: RuntimePart,
    /// git 装好了但 git-lfs 缺失（例如从 0.1 老安装继承过来的目录）
    pub git_lfs: bool,
    /// 全部装齐需要下载多少 MB
    pub download_mb: f64,
    /// 数据目录，方便用户自己去看
    pub root: String,
}

fn probe(cmd: &Path, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new(cmd);
    c.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    text.lines().next().map(|l| l.trim().to_string())
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(m) if m.is_file() => total += m.len(),
            Ok(m) if m.is_dir() => total += dir_bytes(&entry.path()),
            _ => {}
        }
    }
    total
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub fn status() -> RuntimeStatus {
    let uv = if uv_installed() {
        RuntimePart {
            installed: true,
            version: probe(&uv_exe(), &["--version"]),
            path: uv_exe().to_string_lossy().to_string(),
            size_mb: round1(
                std::fs::metadata(uv_exe()).map(|m| m.len()).unwrap_or(0) as f64
                    / (1024.0 * 1024.0),
            ),
        }
    } else {
        RuntimePart {
            installed: false,
            version: None,
            path: uv_exe().to_string_lossy().to_string(),
            size_mb: 0.0,
        }
    };

    let git = if git_installed() {
        RuntimePart {
            installed: true,
            version: probe(&git_exe(), &["--version"]),
            path: git_exe().to_string_lossy().to_string(),
            size_mb: round1(dir_bytes(&git_root()) as f64 / (1024.0 * 1024.0)),
        }
    } else {
        RuntimePart {
            installed: false,
            version: None,
            path: git_exe().to_string_lossy().to_string(),
            size_mb: 0.0,
        }
    };

    RuntimeStatus {
        uv,
        git,
        git_lfs: lfs_installed(),
        download_mb: round1(UV.size_mb + GIT.size_mb + GITLFS.size_mb),
        root: runtime_root().to_string_lossy().to_string(),
    }
}

// ------------------------------------------------------------------ 事件

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEvent {
    /// download | extract | done
    pub stage: String,
    pub message: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub done: bool,
}

fn mb(v: u64) -> f64 {
    v as f64 / (1024.0 * 1024.0)
}

// ------------------------------------------------------------------ 下载

fn with_github_proxy(url: &str) -> String {
    if !(url.contains("github.com") || url.contains("githubusercontent.com")) {
        return url.to_string();
    }
    let settings = config::load_settings().unwrap_or_default();
    match settings
        .mirrors
        .github_proxy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(proxy) => format!("{}/{}", proxy.trim_end_matches('/'), url),
        None => url.to_string(),
    }
}

fn download(
    url: &str,
    dest: &Path,
    label: &str,
    emit: &mut dyn FnMut(RuntimeEvent),
) -> Result<(), AppError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => AppError::new(
            "RUNTIME_HTTP",
            format!("{label} 下载失败：HTTP {code}"),
        ),
        other => AppError::new("RUNTIME_HTTP", format!("{label} 下载失败：{other}")),
    })?;

    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest)?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut downloaded: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| AppError::new("RUNTIME_HTTP", format!("{label} 读取中断：{e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            emit(RuntimeEvent {
                stage: "download".into(),
                message: match total {
                    Some(t) if t > 0 => {
                        format!("正在下载 {label}：{:.1} / {:.1} MB", mb(downloaded), mb(t))
                    }
                    _ => format!("正在下载 {label}：{:.1} MB", mb(downloaded)),
                },
                downloaded,
                total,
                done: false,
            });
        }
    }
    file.flush()?;
    Ok(())
}

fn verify(path: &Path, expect: &str) -> Result<(), AppError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hex::encode(hasher.finalize()).to_uppercase();
    if actual != expect.to_uppercase() {
        return Err(AppError::new(
            "RUNTIME_HASH",
            format!(
                "校验失败，文件可能被篡改或没下完：期望 {}，实际 {}",
                expect.to_uppercase(),
                actual
            ),
        ));
    }
    Ok(())
}

fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join("comfyfleet-runtime")
}

/// 先镜像、后 GitHub；任何一个成功并通过 SHA256 校验即返回本地 zip 路径。
fn fetch(
    artifact: &Artifact,
    label: &str,
    emit: &mut dyn FnMut(RuntimeEvent),
) -> Result<PathBuf, AppError> {
    let dir = tmp_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(artifact.file);
    let _ = std::fs::remove_file(&dest);

    let mirror = format!("{MIRROR_BASE}{}", artifact.file);
    let github = with_github_proxy(artifact.url);
    let mut last: Option<AppError> = None;
    for url in [mirror, github] {
        match download(&url, &dest, label, emit) {
            Ok(()) => match verify(&dest, artifact.sha256) {
                Ok(()) => return Ok(dest),
                Err(e) => {
                    let _ = std::fs::remove_file(&dest);
                    return Err(e);
                }
            },
            Err(e) => {
                log::warn!("{label} 从 {url} 下载失败：{}", e.message);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| AppError::new("RUNTIME_HTTP", format!("{label} 下载失败"))))
}

// ------------------------------------------------------------------ 解包

/// 安全解包：拒绝符号链接、绝对路径、盘符、`..` 穿越与解压炸弹。
fn extract_zip(
    zip: &Path,
    dest: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<usize, AppError> {
    let file = std::fs::File::open(zip)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("RUNTIME_UNZIP", format!("打开压缩包失败：{e}")))?;
    if archive.len() > max_entries {
        return Err(AppError::new("RUNTIME_UNZIP", "压缩包条目过多，已拒绝"));
    }
    std::fs::create_dir_all(dest)?;
    let mut files = 0usize;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::new("RUNTIME_UNZIP", e.to_string()))?;
        if entry.is_symlink() {
            return Err(AppError::new(
                "RUNTIME_UNZIP",
                format!("压缩包含符号链接，已拒绝：{}", entry.name()),
            ));
        }
        let name = entry.name().to_string();
        if name.contains('\\') || name.contains(':') || name.starts_with('/') {
            return Err(AppError::new(
                "RUNTIME_UNZIP",
                format!("压缩包含非法路径，已拒绝：{name}"),
            ));
        }
        let mut parts: Vec<&str> = Vec::new();
        for part in name.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                return Err(AppError::new(
                    "RUNTIME_UNZIP",
                    format!("压缩包含越界路径，已拒绝：{name}"),
                ));
            }
            parts.push(part);
        }
        if parts.is_empty() {
            continue;
        }
        let out = parts.iter().fold(dest.to_path_buf(), |acc, p| acc.join(p));
        if !out.starts_with(dest) {
            return Err(AppError::new(
                "RUNTIME_UNZIP",
                format!("压缩包解压路径越界，已拒绝：{name}"),
            ));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        total += entry.size();
        if total > max_bytes {
            return Err(AppError::new("RUNTIME_UNZIP", "解压体积过大，已拒绝"));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut f)?;
        files += 1;
    }
    Ok(files)
}

fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case(name))
                .unwrap_or(false)
            {
                return Some(p);
            }
        }
    }
    None
}

// ------------------------------------------------------------------ 安装

fn install_uv(emit: &mut dyn FnMut(RuntimeEvent)) -> Result<(), AppError> {
    emit(RuntimeEvent {
        stage: "download".into(),
        message: format!("开始下载 uv {UV_VERSION}（约 {:.1} MB）", UV.size_mb),
        downloaded: 0,
        total: None,
        done: false,
    });
    let zip = fetch(&UV, "uv", emit)?;

    emit(RuntimeEvent {
        stage: "extract".into(),
        message: "校验通过，正在解包 uv…".into(),
        downloaded: 0,
        total: None,
        done: false,
    });

    let target = uv_exe();
    let parent = target.parent().unwrap_or(Path::new(".")).to_path_buf();
    // 解包到临时目录再拷贝：目标目录里可能还有老安装遗留的 resources 其它文件，
    // 不能整目录清掉。
    let staging = tmp_dir().join("uv");
    let _ = std::fs::remove_dir_all(&staging);
    extract_zip(&zip, &staging, 50_000, 512 * 1024 * 1024)?;
    let exe = find_file(&staging, "uv.exe")
        .ok_or_else(|| AppError::new("RUNTIME_BROKEN", "uv 压缩包里没有 uv.exe"))?;
    std::fs::create_dir_all(&parent)?;
    let _ = std::fs::remove_file(&target);
    std::fs::copy(&exe, &target)?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&zip);

    if probe(&target, &["--version"]).is_none() {
        return Err(AppError::new(
            "RUNTIME_BROKEN",
            format!("uv 装好了但跑不起来：{}（可能被杀软拦截）", target.display()),
        ));
    }
    Ok(())
}

fn install_git(emit: &mut dyn FnMut(RuntimeEvent)) -> Result<(), AppError> {
    if !git_installed() {
        emit(RuntimeEvent {
            stage: "download".into(),
            message: format!("开始下载 MinGit {GIT_VERSION}（约 {:.1} MB）", GIT.size_mb),
            downloaded: 0,
            total: None,
            done: false,
        });
        let zip = fetch(&GIT, "MinGit", emit)?;

        emit(RuntimeEvent {
            stage: "extract".into(),
            message: "校验通过，正在解包 MinGit…".into(),
            downloaded: 0,
            total: None,
            done: false,
        });

        let root = git_root();
        let staging = root.with_extension("staging");
        let _ = std::fs::remove_dir_all(&staging);
        extract_zip(&zip, &staging, 200_000, 2 * 1024 * 1024 * 1024)?;
        if !staging.join("cmd").join("git.exe").is_file() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(AppError::new(
                "RUNTIME_BROKEN",
                "MinGit 解包后找不到 cmd\\git.exe",
            ));
        }
        let _ = std::fs::remove_dir_all(&root);
        std::fs::rename(&staging, &root)?;
        let _ = std::fs::remove_file(&zip);
    }

    if !lfs_installed() {
        emit(RuntimeEvent {
            stage: "download".into(),
            message: format!(
                "开始下载 git-lfs {LFS_VERSION}（约 {:.1} MB）",
                GITLFS.size_mb
            ),
            downloaded: 0,
            total: None,
            done: false,
        });
        let zip = fetch(&GITLFS, "git-lfs", emit)?;

        emit(RuntimeEvent {
            stage: "extract".into(),
            message: "校验通过，正在安装 git-lfs…".into(),
            downloaded: 0,
            total: None,
            done: false,
        });

        let staging = tmp_dir().join("lfs");
        let _ = std::fs::remove_dir_all(&staging);
        extract_zip(&zip, &staging, 5_000, 256 * 1024 * 1024)?;
        let exe = find_file(&staging, "git-lfs.exe")
            .ok_or_else(|| AppError::new("RUNTIME_BROKEN", "git-lfs 包里没有 git-lfs.exe"))?;
        if let Some(parent) = lfs_exe().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&exe, lfs_exe())?;
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&zip);
    }

    if probe(&git_exe(), &["--version"]).is_none() {
        return Err(AppError::new(
            "RUNTIME_BROKEN",
            format!("git 装好了但跑不起来：{}", git_exe().display()),
        ));
    }
    Ok(())
}

/// 安装运行时。`key` 取 `uv` 或 `git`（装 git 会连带装 git-lfs）。
/// 同一时刻只允许一个下载任务，避免两个线程写同一个目录。
pub fn install(key: &str, on_event: Channel<RuntimeEvent>) -> Result<(), AppError> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let chan = on_event.clone();
    let mut emit = move |ev: RuntimeEvent| {
        let _ = chan.send(ev);
    };

    let label = match key {
        "uv" => "uv",
        "git" => "Git",
        other => {
            return Err(AppError::new(
                "RUNTIME_UNKNOWN",
                format!("未知的运行时：{other}"),
            ))
        }
    };

    let result = if key == "uv" {
        install_uv(&mut emit)
    } else {
        install_git(&mut emit)
    };

    let message = match &result {
        Ok(()) => format!("{label} 安装完成"),
        Err(e) => format!("{label} 安装失败：{}", e.message),
    };
    let _ = on_event.send(RuntimeEvent {
        stage: "done".into(),
        message,
        downloaded: 0,
        total: None,
        done: true,
    });
    result
}

/// 卸载：只删我们自己的文件/目录，不碰系统 PATH 或系统里的 git。
pub fn remove(key: &str) -> Result<(), AppError> {
    match key {
        "uv" => {
            // 只删 uv.exe：resources 目录里可能还有老安装遗留的其它资源
            let exe = uv_exe();
            if exe.exists() {
                std::fs::remove_file(exe)?;
            }
            Ok(())
        }
        "git" => {
            let root = git_root();
            if root.exists() {
                std::fs::remove_dir_all(root)?;
            }
            Ok(())
        }
        other => Err(AppError::new(
            "RUNTIME_UNKNOWN",
            format!("未知的运行时：{other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_stable() {
        // 两个运行时统一在 <数据目录>\runtime 下，且带版本目录
        assert!(uv_exe().ends_with(format!("uv-{UV_VERSION}\\uv.exe")));
        assert!(git_exe().ends_with(format!("mingit-{GIT_VERSION}\\cmd\\git.exe")));
        assert!(uv_exe().starts_with(runtime_root()));
        assert!(git_root().starts_with(runtime_root()));
    }

    #[test]
    fn artifacts_have_hashes() {
        for a in [&UV, &GIT, &GITLFS] {
            assert_eq!(a.sha256.len(), 64, "{} 的 SHA256 长度不对", a.file);
        }
    }

    /// 真下载一次 uv：验证 URL、SHA256、解包、可执行性整条链路。
    /// 需要联网（约 19 MB），所以默认 ignored：
    ///   $env:COMFYFLEET_DATA_DIR="$env:TEMP\cf-rt"; cargo test -- --ignored install_uv_end_to_end
    #[test]
    #[ignore = "联网下载约 19 MB，手动触发"]
    fn install_uv_end_to_end() {
        let mut events: Vec<RuntimeEvent> = Vec::new();
        {
            let mut emit = |ev: RuntimeEvent| events.push(ev);
            install_uv(&mut emit).expect("下载安装 uv 应成功");
        }
        assert!(uv_installed(), "装完应能检测到 uv：{}", uv_exe().display());
        let version = probe(&uv_exe(), &["--version"]).expect("uv --version 应能跑");
        assert!(version.contains("uv "), "版本输出不对：{version}");
        assert!(!events.is_empty(), "应产生进度事件");

        remove("uv").expect("删除应成功");
        assert!(!uv_installed());
    }
}
