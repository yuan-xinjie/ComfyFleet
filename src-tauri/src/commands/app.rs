use serde::Serialize;
use std::path::PathBuf;
use tauri_plugin_opener::OpenerExt;

use crate::config;
use crate::error::AppError;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub data_dir: String,
    pub config_path: String,
    pub instances_dir: String,
    pub suggested_root: String,
    pub dev: bool,
}

fn path_str(p: PathBuf) -> String {
    p.to_string_lossy().to_string()
}

/// 建议的实例根目录：优先非系统盘，避免模型把 C 盘写满。
fn suggested_root() -> String {
    for drive in ["D:", "E:", "F:", "G:"] {
        if PathBuf::from(format!("{drive}\\")).exists() {
            return format!("{drive}\\ComfyUI");
        }
    }
    match dirs::home_dir() {
        Some(home) => path_str(home.join("ComfyUI")),
        None => "C:\\ComfyUI".to_string(),
    }
}

/// 当前本机网络吞吐；操作控制台轮询它，避免安装脚本静默时用户以为卡死。
#[tauri::command]
pub fn net_speed() -> crate::net::NetSpeed {
    crate::net::sample()
}

#[tauri::command]
pub fn app_info(app: tauri::AppHandle) -> Result<AppInfo, AppError> {
    let root = config::app_root();
    std::fs::create_dir_all(&root).map_err(|e| AppError::new("DATA_DIR", e.to_string()))?;

    Ok(AppInfo {
        name: "ComfyFleet".to_string(),
        version: app.package_info().version.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        data_dir: path_str(root),
        config_path: path_str(config::config_path()),
        instances_dir: path_str(config::instances_dir()),
        suggested_root: suggested_root(),
        dev: cfg!(debug_assertions),
    })
}

#[tauri::command]
pub fn open_path(app: tauri::AppHandle, path: String) -> Result<(), AppError> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(AppError::new(
            "PATH_NOT_FOUND",
            format!("路径不存在：{path}"),
        ));
    }
    // 界面只用它“打开文件夹”；不允许借 open_path 启动任意 .exe/.lnk。
    if !p.is_dir() {
        return Err(AppError::new(
            "PATH_NOT_DIR",
            format!("只能打开文件夹，不能打开文件：{path}"),
        ));
    }
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| AppError::new("OPEN_FAILED", e.to_string()))
}

/// 用系统默认浏览器打开一个 http(s) 链接（例如 ComfyUI 官方文档）。
/// 只放行 http/https：别让前端借这条命令跑 file:// 或自定义协议。
#[tauri::command]
pub fn open_url(app: tauri::AppHandle, url: String) -> Result<(), AppError> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(AppError::new(
            "URL_INVALID",
            format!("只允许 http(s) 链接：{url}"),
        ));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| AppError::new("OPEN_FAILED", e.to_string()))
}
/// 打开 Windows 原生文件夹选择框；取消返回 null。
#[tauri::command]
pub fn pick_folder() -> Result<Option<String>, String> {
    crate::picker::pick_folder()
}

/// 前端把渲染好的 64×64 RGBA 托盘图标推上来，运行中即时换图标。
#[tauri::command]
pub fn set_tray_icon(app: tauri::AppHandle, rgba_base64: String) -> Result<(), AppError> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    let bytes = STANDARD
        .decode(rgba_base64)
        .map_err(|e| AppError::new("TRAY_ICON", e.to_string()))?;
    if bytes.len() != 64 * 64 * 4 {
        return Err(AppError::new("TRAY_ICON", "托盘图标数据尺寸不对"));
    }
    let tray = app
        .tray_by_id("main-tray")
        .ok_or_else(|| AppError::new("TRAY_ICON", "托盘还没有创建"))?;
    tray.set_icon(Some(tauri::image::Image::new_owned(bytes, 64, 64)))
        .map_err(|e| AppError::new("TRAY_ICON", e.to_string()))?;
    log::info!("托盘图标已更新");
    Ok(())
}
