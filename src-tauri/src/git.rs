//! git 可执行文件解析：内置 MinGit（随安装包分发）→ 用户自定义 → 系统 PATH。
//!
//! 为什么内置：M1 要用 `git clone` 拉 ComfyUI，M6 要用 tag 切版本。
//! 依赖用户机器上装了 git 是不可接受的（大量 Windows 机器没有），
//! 官方的 ComfyUI Desktop 也是走"自带 uv + 自带 git"的路子。

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::path::BaseDirectory;
use tauri::Manager;

use crate::config;
use crate::error::AppError;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 必须与打包进 `resources/mingit.zip` 的版本一致（解包目录名也用它做版本隔离）
pub const MINGIT_VERSION: &str = "2.56.0.4";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub path: String,
    pub version: String,
    /// bundled | custom | system
    pub source: String,
    pub bundled_version: String,
}

fn zip_path(app: Option<&tauri::AppHandle>) -> Result<PathBuf, AppError> {
    // 发布后：安装目录下的资源
    if let Some(app) = app {
        if let Ok(p) = app
            .path()
            .resolve("resources/mingit.zip", BaseDirectory::Resource)
        {
            if p.exists() {
                return Ok(p);
            }
        }
    }
    // 开发期回退：源码树里的同一份文件（仅 debug 构建启用，
    // 这样 release 包只能走安装目录里的资源，验证不会被回退路径掩盖）
    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("mingit.zip");
        if dev.exists() {
            return Ok(dev);
        }
    }
    Err(AppError::new(
        "MINGIT_MISSING",
        "找不到内置 MinGit（resources/mingit.zip）",
    ))
}

pub fn extract_root() -> PathBuf {
    config::app_root()
        .join("git")
        .join(format!("mingit-{MINGIT_VERSION}"))
}

fn git_exe_in(root: &Path) -> PathBuf {
    root.join("cmd").join("git.exe")
}

/// 首次使用时把 MinGit 解包到数据目录；已解包则直接返回 git.exe。
/// `app` 传 None 时走源码树回退（用于单元测试与开发）。
pub fn ensure_bundled(app: Option<&tauri::AppHandle>) -> Result<PathBuf, AppError> {
    // 首次启动可能从设置页/安装流程并发调用；两个线程同时解包会写坏同一批文件。
    static EXTRACT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = EXTRACT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let root = extract_root();
    let exe = git_exe_in(&root);
    let marker = root.join(".extracted");
    let marker_ok = std::fs::read_to_string(&marker)
        .map(|v| v.trim() == MINGIT_VERSION)
        .unwrap_or(false);
    if exe.exists() && marker_ok {
        return Ok(exe);
    }

    let zip = zip_path(app)?;
    std::fs::create_dir_all(&root)?;
    let file = std::fs::File::open(&zip)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("MINGIT_UNZIP", format!("打开 MinGit 压缩包失败：{e}")))?;

    const MAX_ENTRIES: usize = 200_000;
    const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    if archive.len() > MAX_ENTRIES {
        return Err(AppError::new(
            "MINGIT_UNZIP",
            "MinGit 压缩包条目过多，已拒绝",
        ));
    }
    let started = std::time::Instant::now();
    let mut files = 0usize;
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::new("MINGIT_UNZIP", e.to_string()))?;

        // zip 里的名字是不可信输入：只接受纯相对路径，拒绝 `\`、盘符、UNC、
        // `..` 和符号链接，最后再确认写出的路径仍在 root 之内。
        if entry.is_symlink() {
            return Err(AppError::new(
                "MINGIT_UNZIP",
                format!("MinGit 压缩包含符号链接，已拒绝：{}", entry.name()),
            ));
        }
        let name = entry.name();
        if name.contains('\\') || name.contains(':') {
            return Err(AppError::new(
                "MINGIT_UNZIP",
                format!("MinGit 压缩包含非法路径，已拒绝：{name}"),
            ));
        }
        let rel = Path::new(name);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(AppError::new(
                "MINGIT_UNZIP",
                format!("MinGit 压缩包含越界路径，已拒绝：{name}"),
            ));
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = root.join(rel);
        if !out.starts_with(&root) {
            return Err(AppError::new(
                "MINGIT_UNZIP",
                format!("MinGit 压缩包解压路径越界，已拒绝：{name}"),
            ));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        total = total.saturating_add(entry.size());
        if total > MAX_TOTAL_BYTES {
            return Err(AppError::new(
                "MINGIT_UNZIP",
                "MinGit 解压后体积过大，已拒绝",
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::File::create(&out)?;
        io::copy(&mut entry, &mut f)?;
        files += 1;
    }

    if !exe.exists() {
        return Err(AppError::new(
            "MINGIT_BROKEN",
            format!("解包后仍找不到 git.exe：{}", exe.display()),
        ));
    }
    let _ = std::fs::write(&marker, MINGIT_VERSION);
    log::info!(
        "MinGit {} 已解包：{} 个文件，耗时 {:?}，位置 {}",
        MINGIT_VERSION,
        files,
        started.elapsed(),
        root.display()
    );
    Ok(exe)
}

/// 统一的 git 调用入口：永不等待终端输入（否则 clone 私有库会吊死界面上）
pub fn command(git: &Path) -> Command {
    let mut cmd = Command::new(git);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn probe_version(git: &Path) -> Option<String> {
    let out = command(git).arg("--version").output().ok()?;
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

/// 不拿 `AppHandle` 也能解析 git（快照采集在后台线程里跑，只用来读 status/diff）。
/// 优先用户自定义，其次已解包/可解包的内置 MinGit，最后系统 PATH。
pub fn resolve_quiet(settings: &config::GlobalSettings) -> Option<PathBuf> {
    if let Some(custom) = settings
        .paths
        .git
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let pb = PathBuf::from(custom);
        if probe_version(&pb).is_some() {
            return Some(pb);
        }
    }
    if let Ok(exe) = ensure_bundled(None) {
        if probe_version(&exe).is_some() {
            return Some(exe);
        }
    }
    let system = PathBuf::from("git");
    probe_version(&system).is_some().then_some(system)
}

/// 解析当前生效的 git：内置优先（版本可控），其次用户自定义，最后系统 PATH
pub fn resolve(
    app: &tauri::AppHandle,
    settings: &config::GlobalSettings,
) -> Result<GitInfo, AppError> {
    if let Some(custom) = settings
        .paths
        .git
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let pb = PathBuf::from(custom);
        if let Some(version) = probe_version(&pb) {
            return Ok(GitInfo {
                path: custom.to_string(),
                version,
                source: "custom".to_string(),
                bundled_version: MINGIT_VERSION.to_string(),
            });
        }
        log::warn!("自定义 git 不可用，回退内置：{custom}");
    }

    match ensure_bundled(Some(app)) {
        Ok(exe) => {
            if let Some(version) = probe_version(&exe) {
                return Ok(GitInfo {
                    path: exe.to_string_lossy().to_string(),
                    version,
                    source: "bundled".to_string(),
                    bundled_version: MINGIT_VERSION.to_string(),
                });
            }
            log::warn!("内置 git 无法执行：{}", exe.display());
        }
        Err(e) => log::warn!("内置 git 解包失败：{}", e.message),
    }

    if let Some(version) = probe_version(Path::new("git")) {
        return Ok(GitInfo {
            path: "git".to_string(),
            version,
            source: "system".to_string(),
            bundled_version: MINGIT_VERSION.to_string(),
        });
    }

    Err(AppError::new(
        "GIT_MISSING",
        "没有可用的 git：内置 MinGit 解包失败，且系统 PATH 中也没有 git",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真正解包一次并执行 `git --version`：验证"内置 git 可用"这条链路
    #[test]
    fn bundled_mingit_extracts_and_runs() {
        let exe = match ensure_bundled(None) {
            Ok(p) => p,
            Err(e) => {
                // 源码树里没有 zip 时跳过（例如未执行 fetch 脚本的干净检出）
                eprintln!("skip: {}", e.message);
                return;
            }
        };
        assert!(exe.exists(), "git.exe should exist at {}", exe.display());
        let out = command(&exe)
            .arg("--version")
            .output()
            .expect("git --version should run");
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("git version"), "unexpected output: {text}");
        assert!(
            text.contains("2.56"),
            "bundled git version should match MINGIT_VERSION, got {text}"
        );
    }
}
