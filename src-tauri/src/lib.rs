//! ComfyFleet：Tauri 应用入口。
//!
//! M0 范围：应用信息、设置读写、实例登记、PTY 控制台。
//! 所有副作用（进程、文件、网络）都留在 Rust 侧，前端只发意图。

mod accel;
mod activity;
mod cluster;
mod commands;
mod compat;
mod config;
mod error;
mod git;
mod gpu;
mod install;
mod matrix;
mod net;
mod picker;
mod pip;
mod proc;
mod pty;
mod runtime;
mod snapshot;
mod state;
mod storage;
mod sync;
mod toolchain;
mod uv_cache;
mod wheel_repo;
mod wheelhouse;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// UV 缓存没有“安装完成”这种统一回调（节点依赖、加速库、快照恢复都会用到），
/// 所以用低频后台检查兜底；有长操作正在跑时跳过，避免删掉正在读取的 wheel。
fn watch_uv_cache(app: tauri::AppHandle) {
    std::thread::sleep(std::time::Duration::from_secs(20));
    loop {
        let (limit, busy) = {
            let state = app.state::<state::AppState>();
            let limit = state.settings.lock().storage.uv_cache_max_gb;
            (limit, state.uv_work_active())
        };
        if !busy {
            uv_cache::auto_prune(limit);
        }
        std::thread::sleep(std::time::Duration::from_secs(600));
    }
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 托盘：启动器要能"关掉窗口继续运行"，所以必须有托盘入口
fn setup_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let start_all = MenuItem::with_id(app, "start_all", "启动全部实例", true, None::<&str>)?;
    let stop_all = MenuItem::with_id(app, "stop_all", "停止全部实例", true, None::<&str>)?;
    let open_dir = MenuItem::with_id(app, "open_dir", "打开数据目录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 ComfyFleet", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &start_all,
            &stop_all,
            &PredefinedMenuItem::separator(app)?,
            &open_dir,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("ComfyFleet")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            // 启停全部可能要等端口就绪（几十秒）：不能在托盘回调（主线程）里干
            "start_all" => {
                let app = app.clone();
                std::thread::spawn(move || commands::run::start_all(&app));
            }
            "stop_all" => {
                let app = app.clone();
                std::thread::spawn(move || commands::run::stop_all(&app));
            }
            "open_dir" => {
                let _ = std::process::Command::new("explorer")
                    .arg(config::app_root())
                    .spawn();
            }
            "quit" => {
                commands::run::shutdown_all(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// 关窗 = 收进托盘；真退出走托盘菜单，避免顺手关窗把正在跑的实例一起杀。
fn setup_close_to_tray(app: &mut tauri::App) {
    if let Some(win) = app.get_webview_window("main") {
        let handle = app.handle().clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
        });
    }
}

/// Windows 11：无边框窗口也要圆角与阴影，避免白面板直接切在桌面上
#[cfg(target_os = "windows")]
fn apply_window_chrome(app: &tauri::App) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let Ok(hwnd) = win.hwnd() else {
        return;
    };
    let hwnd = hwnd.0;
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: *mut std::ffi::c_void,
            attr: u32,
            pv: *const std::ffi::c_void,
            cb: u32,
        ) -> i32;
    }
    // 33 = DWMWA_WINDOW_CORNER_PREFERENCE，2 = DWMWCP_ROUND
    // 34 = DWMWA_BORDER_COLOR，0xFFFFFFFE = DWMWA_COLOR_NONE
    let round: u32 = 2;
    let none: u32 = 0xFFFF_FFFE;
    unsafe {
        DwmSetWindowAttribute(hwnd, 33, &round as *const u32 as *const _, 4);
        DwmSetWindowAttribute(hwnd, 34, &none as *const u32 as *const _, 4);
    }
}

pub fn run() {
    let builder = tauri::Builder::default();
    // 开 联调时允许两个进程（两个数据目录）互相做 Leader/Follower。
    // 正式按单实例运行，避免用户重复启动。
    #[cfg(debug_assertions)]
    let builder = if std::env::var_os("COMFYFLEET_ALLOW_MULTI").is_some() {
        builder
    } else {
        // single-instance 必须最先注。
        builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
    };
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        if let Some(win) = app.get_webview_window("main") {
            // 用户可能把窗口隐藏到托盘：只 unminimize 不会让它重新出现。
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    }));
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                // 日志也落进数据目录（安装目录\data\logs）：「打开数据目录」里能看到全。
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Folder {
                        path: config::app_root().join("logs"),
                        file_name: None,
                    }),
                ])
                .level(log::LevelFilter::Info)
                .max_file_size(10_000_000)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        // 应用自更新：公钥与更新源在 tauri.conf.json 的 plugins.updater 里
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let settings = match config::load_settings() {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("载入设置失败，使用默认值：{}", e.message);
                    config::GlobalSettings::default()
                }
            };
            log::info!(
                "ComfyFleet 启动，安装目录 {}，数据目录 {}",
                config::install_dir().display(),
                config::app_root().display()
            );
            wheelhouse::ensure_default_dir();
            app.manage(state::AppState::new(settings.clone()));
            // 启动时按设置回收旧同步缓存；正在等待/运行的任务由 prune 内部保护。
            let cache_keep = settings.sync.sync_cache_keep;
            std::thread::spawn(move || {
                let pruned = sync::prune_sync_cache(cache_keep);
                if pruned.removed > 0 {
                    log::info!(
                        "启动清理同步缓存：保留 {} 次，删除 {} 组，释放 {}",
                        cache_keep,
                        pruned.removed,
                        sync::human_bytes(pruned.freed_bytes)
                    );
                }
            });
            // UV 缓存按大小上限自动清理；低频轮询，避开安装/同步进行中的文件。
            let uv_handle = app.handle().clone();
            std::thread::spawn(move || watch_uv_cache(uv_handle));

            // 上次创建/加入过集群：恢复 Leader 服务 Follower Agent
            cluster::autostart(&app.handle().clone());

            // 启动时解析一下 git（运行时 MinGit 未安装时会 WARN），结果写进日志便于排查。
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                // 老版本把 uv 放在安装目录、把 MinGit 放在 data\git；统一收到 runtime 目录
                toolchain::migrate_legacy();
                if !toolchain::uv_installed() {
                    log::warn!("uv 运行时未安装（设置 → 运行 → 运行时 可下载）");
                }
                match git::resolve(&handle, &settings) {
                    Ok(g) => log::info!(
                        "git ready: {} (source {}, path {})",
                        g.version,
                        g.source,
                        g.path
                    ),
                    Err(e) => log::warn!("git 不可用：{}", e.message),
                }
            });

            // 清理上次被强杀留下的孤 ComfyUI（它们会占着端口。
            std::thread::spawn(commands::run::recover_orphans);

            setup_tray(app)?;
            setup_close_to_tray(app);
            apply_window_chrome(app);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::accel::accel_scan,
            commands::activity::activity_list,
            commands::app::app_info,
            commands::app::net_speed,
            commands::app::open_path,
            commands::app::open_url,
            commands::app::pick_folder,
            commands::app::set_tray_icon,
            commands::settings::wheel_repo_list,
            commands::settings::wheel_repo_download,
            commands::settings::wheel_repo_install,
            commands::cluster::cluster_status,
            commands::cluster::cluster_retry_server,
            commands::cluster::cluster_set_lan,
            commands::cluster::cluster_create,
            commands::cluster::cluster_join,
            commands::cluster::cluster_leave,
            commands::cluster::cluster_delete,
            commands::cluster::cluster_remove_member,
            commands::cluster::cluster_cancel_job,
            commands::cluster::cluster_cancel_member_job,
            commands::cluster::cluster_retry_member_job,
            commands::cluster::cluster_delete_job,
            commands::cluster::cluster_confirm_job,
            commands::cluster::cluster_reject_job,
            commands::cluster::cluster_refresh,
            commands::cluster::cluster_compare,
            commands::cluster::cluster_source_changes,
            commands::cluster::sync_request,
            commands::cluster::sync_prepare_status,
            commands::cluster::sync_prepare_cancel,
            commands::cluster::sync_prepare_clear,
            commands::cluster::sync_preview,
            commands::cluster::sync_preview_batch,
            commands::cluster::sync_start,
            commands::cluster::sync_start_batch,
            commands::cluster::sync_job,
            commands::cluster::sync_jobs,
            commands::settings::settings_get,
            commands::settings::settings_save,
            commands::settings::settings_set_theme,
            commands::settings::sync_cache_info,
            commands::settings::wheels_info,
            commands::settings::uv_cache_info,
            commands::settings::uv_cache_clear,
            commands::instance::instance_list,
            commands::instance::instance_create,
            commands::instance::instance_remote_save,
            commands::instance::instance_update,
            commands::instance::instance_python_probe,
            commands::instance::instance_env_refresh,
            commands::instance::instance_launch_update,
            commands::instance::instance_storage_plan,
            commands::instance::instance_storage_update,
            commands::instance::instance_delete,
            commands::env::env_init,
            commands::env::env_install,
            commands::env::env_cancel_install,
            commands::env::env_plan,
            commands::git::git_info,
            commands::pty::pty_spawn,
            commands::pty::pty_spawn_instance,
            commands::pty::pty_write,
            commands::pty::pty_resize,
            commands::pty::pty_kill,
            commands::run::instance_start,
            commands::run::instance_stop,
            commands::run::instance_log_state,
            commands::run::instance_open_url,
            commands::run::instance_health,
            commands::version::version_info,
            commands::version::version_dirty_files,
            commands::version::version_tags,
            commands::version::version_commits,
            commands::version::version_check,
            commands::version::version_cancel,
            commands::version::version_switch,
            commands::version::version_update,
            commands::update::app_update_check,
            commands::update::app_update_install,
            commands::toolchain::toolchain_status,
            commands::toolchain::toolchain_install,
            commands::toolchain::toolchain_remove,
            commands::nodes::nodes_list,
            commands::nodes::nodes_install,
            commands::nodes::nodes_toggle,
            commands::nodes::nodes_update,
            commands::nodes::nodes_remove,
            commands::nodes::nodes_check_updates,
            commands::models::models_list,
            commands::models::models_copy,
            commands::diag::diag_scan,
            commands::instance::instance_import,
            commands::instance::instance_import_profile,
            commands::instance::instance_relocate,
            commands::instance::instance_set_node_auto,
            commands::snapshot::snapshot_list,
            commands::snapshot::snapshot_detail,
            commands::snapshot::snapshot_create,
            commands::snapshot::snapshot_delete,
            commands::snapshot::snapshot_restore_preview,
            commands::snapshot::snapshot_restore,
            commands::deps::nodes_deps_status,
            commands::deps::nodes_install_deps,
        ])
        .build(tauri::generate_context!())
        .expect("ComfyFleet 启动失败")
        .run(|app, event| {
            // 退出时结束所有实例进程，避免留下孤儿 ComfyUI 占端。
            if matches!(event, tauri::RunEvent::Exit) {
                commands::run::shutdown_all(app);
                cluster::stop_runtime(app);
            }
        });
}
