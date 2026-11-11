//! git 可执行文件解析：用户自定义 → 按需下载的运行时 MinGit → 系统 PATH。
//!
//! 以前 MinGit 是打进安装包的（+38 MB）；现在改成「设置 → 运行时」里按需下载，
//! 默认不装。解析顺序保持"版本可控优先"：先看用户指定的 git，再看我们管理的
//! MinGit（版本固定、行为可预期），最后才用系统里那个不知道版本的 git。

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::error::AppError;
use crate::toolchain;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// MinGit 版本（解包目录名也用它做版本隔离），实际定义在 toolchain
pub const MINGIT_VERSION: &str = toolchain::GIT_VERSION;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub path: String,
    pub version: String,
    /// bundled | custom | system
    pub source: String,
    pub bundled_version: String,
}

/// 我们管理的 MinGit 是否可用（装好且能跑）。
pub fn ensure() -> Result<PathBuf, AppError> {
    let exe = toolchain::git_exe();
    if exe.is_file() && probe_version(&exe).is_some() {
        return Ok(exe);
    }
    Err(AppError::new(
        "GIT_MISSING",
        "还没有安装 Git 运行时。请到 设置 → 运行时 下载（约 43.7 MB，含 git-lfs），\
         或在 设置 → 路径 里指定系统里已有的 git。",
    ))
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
/// 优先用户自定义，其次我们管理的 MinGit，最后系统 PATH。
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
    if let Ok(exe) = ensure() {
        return Some(exe);
    }
    let system = PathBuf::from("git");
    probe_version(&system).is_some().then_some(system)
}

/// 解析当前生效的 git：用户自定义 → 运行时 MinGit → 系统 PATH
pub fn resolve(
    _app: &tauri::AppHandle,
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
        log::warn!("自定义 git 不可用，回退运行时 MinGit：{custom}");
    }

    match ensure() {
        Ok(exe) => {
            if let Some(version) = probe_version(&exe) {
                return Ok(GitInfo {
                    path: exe.to_string_lossy().to_string(),
                    version,
                    // 前端把 bundled 显示成「内置」，语义即"我们管理的那个"
                    source: "bundled".to_string(),
                    bundled_version: MINGIT_VERSION.to_string(),
                });
            }
            log::warn!("运行时 git 无法执行：{}", exe.display());
        }
        Err(e) => log::warn!("运行时 git 不可用：{}", e.message),
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
        "没有可用的 git：运行时 MinGit 未安装，且系统 PATH 中也没有 git。\
         请到 设置 → 运行时 下载，或在 设置 → 路径 里指定 git.exe。",
    ))
}
