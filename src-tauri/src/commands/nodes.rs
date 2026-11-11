//! 自定义节点管理（M5）：列出 / 安装 / 启停用/ 更新 / 卸载。
//!
//! 约定（与 ComfyUI-Manager 一致）：禁用= 目录名加 `.disabled` 后缀。
//! 用改名而不是删文件，是因为节点被禁用后用户随时可能想恢复。
use std::path::{Path, PathBuf};

use tauri::ipc::Channel;
use tauri::Manager;
use tauri::{AppHandle, State};

use crate::commands::instance::load_instance;
use crate::commands::version::VersionEvent;
use crate::error::AppError;
use crate::proc::{self, git_env};
use crate::state::AppState;

pub const DISABLED_SUFFIX: &str = ".disabled";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    /// 目录名（不含 `.disabled`：
    pub name: String,
    pub enabled: bool,
    /// git | manual
    pub kind: String,
    pub url: Option<String>,
    pub commit: Option<String>,
    pub has_requirements: bool,
    pub size_mib: u32,
}

fn nodes_dir(root: &str) -> PathBuf {
    PathBuf::from(root).join("ComfyUI").join("custom_nodes")
}

/// 件`.git/config` 里读 origin url（比每个节点起一个git 进程快一个数量级，
fn read_git_url(git_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let mut in_origin = false;
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = t.starts_with("[remote \"origin\"");
            continue;
        }
        if in_origin {
            if let Some(v) = t.strip_prefix("url = ") {
                // 界面只需要来源；含令牌的 userinfo/查询参数不能直接展示。
                return Some(crate::sync::redact_url(v.trim()));
            }
        }
    }
    None
}

/// 件`.git/HEAD` + refs 读当前提交（不启子进程）
fn read_git_commit(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        if let Ok(sha) = std::fs::read_to_string(git_dir.join(reference)) {
            return Some(sha.trim().chars().take(7).collect());
        }
        // 打包过的仓库：refs 在packed-refs 重
        let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
        for line in packed.lines() {
            if line.starts_with('#') {
                continue;
            }
            if let Some((sha, name)) = line.split_once(' ') {
                if name.trim() == reference {
                    return Some(sha.chars().take(7).collect());
                }
            }
        }
        return None;
    }
    Some(head.chars().take(7).collect())
}

pub fn scan_nodes(dir: &Path) -> Result<Vec<NodeInfo>, AppError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| AppError::new("NODES_READ", format!("读取节点目录失败，{e}")))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let raw_name = entry.file_name().to_string_lossy().to_string();
        if raw_name.starts_with('.') || raw_name == "__pycache__" {
            continue;
        }
        let (name, enabled) = match raw_name.strip_suffix(DISABLED_SUFFIX) {
            Some(base) => (base.to_string(), false),
            None => (raw_name.clone(), true),
        };
        let git_dir = path.join(".git");
        let is_git = git_dir.exists();
        out.push(NodeInfo {
            name,
            enabled,
            kind: if is_git {
                "git".into()
            } else {
                "manual".into()
            },
            url: if is_git { read_git_url(&git_dir) } else { None },
            commit: if is_git {
                read_git_commit(&git_dir)
            } else {
                None
            },
            has_requirements: path.join("requirements.txt").exists(),
            size_mib: proc::dir_size_mib(&path),
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// 件git URL 猜目录名：取最后一段并去掉 `.git`
pub fn name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    last.trim_end_matches(".git").to_string()
}

fn validate_name(name: &str) -> Result<(), AppError> {
    // `.` / `..` / `...` 在 Windows 上分别解析到 custom_nodes 本身或上一级；
    // 允许它们会让 nodes_remove 删掉整个 ComfyUI。结尾的点也会被 Windows
    // 静默去掉，必须拒绝。
    let ok = !name.is_empty()
        && name.len() <= 80
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
    if !ok {
        return Err(AppError::new(
            "NODE_NAME_INVALID",
            "节点目录名只能包含字母、数字、_、-、.、@，且不能以点开头/结尾，不能是 . 或 ..",
        ));
    }
    Ok(())
}

fn validate_git_url(url: &str) -> Result<(), AppError> {
    let url = url.trim();
    let ok = if let Some(rest) = url.strip_prefix("git@") {
        // git@host:path；必须有 host 和冒号后的路径，不能是本地文件。
        rest.split_once(':')
            .map(|(host, path)| !host.is_empty() && !path.is_empty())
            .unwrap_or(false)
    } else if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        !host.is_empty() && !host.starts_with('.') && !host.contains(' ') && !host.contains('\\')
    } else {
        false
    };
    if !ok {
        return Err(AppError::new(
            "NODE_URL_INVALID",
            "请填写完整的 git 仓库地址（https://…、http://… 或 git@host:path），不能是本地路径",
        ));
    }
    Ok(())
}

fn target_dir(dir: &Path, name: &str, enabled: bool) -> PathBuf {
    dir.join(if enabled {
        name.to_string()
    } else {
        format!("{name}{DISABLED_SUFFIX}")
    })
}

fn git_exe(app: &AppHandle, state: &AppState) -> Result<PathBuf, AppError> {
    let settings = state.settings.lock().clone();
    Ok(PathBuf::from(crate::git::resolve(app, &settings)?.path))
}

fn ensure_stopped(state: &AppState, id: &str) -> Result<(), AppError> {
    if state.runs.lock().contains_key(id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止实例，再修改节点（否则文件被占用、依赖也会写进正在使用的 venv）",
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn nodes_list(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<Vec<NodeInfo>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        nodes_list_impl(state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn nodes_list_impl(
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<Vec<NodeInfo>, AppError> {
    let _ = &state;
    let inst = load_instance(&instance_id)?;
    scan_nodes(&nodes_dir(&inst.root_dir))
}

/// 安装一个自定义节点：git clone →（可选）按它的requirements 装依赖
///
/// 克隆和装依赖都可能跑几十秒，真正的工作扔到阻塞线程，界面不卡。
#[tauri::command]
pub async fn nodes_install(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    url: String,
    name: Option<String>,
    install_requirements: Option<bool>,
    on_event: Channel<VersionEvent>,
) -> Result<NodeInfo, AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "安装节点")?;
    let inst = load_instance(&instance_id)?;
    ensure_stopped(state.inner(), &instance_id)?;
    let dir = nodes_dir(&inst.root_dir);
    std::fs::create_dir_all(&dir)?;

    let node_name = name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name_from_url(&url));
    validate_name(&node_name)?;

    let dest = target_dir(&dir, &node_name, true);
    let disabled = target_dir(&dir, &node_name, false);
    if dest.exists() || disabled.exists() {
        return Err(AppError::new(
            "NODE_EXISTS",
            format!("custom_nodes 下已经有 {}（含已禁用目录）", node_name),
        ));
    }
    validate_git_url(&url)?;

    let git = git_exe(&app, state.inner())?;
    let settings = state.settings.lock().clone();
    let url = url.trim().to_string();
    let with_req = install_requirements.unwrap_or(true);
    // 日志里要留来源与名字：闭包会拿走所有权，先各留一件
    let log_name = node_name.clone();
    let log_url = crate::sync::redact_url(&url);

    let result = tauri::async_runtime::spawn_blocking(move || {
        install_blocking(
            &git, &settings, &inst, &dir, &dest, &node_name, &url, with_req, &on_event,
        )
    })
    .await
    .map_err(|e| AppError::new("NODE_INSTALL_JOIN", format!("安装线程异常，{e}")))?;

    match &result {
        Ok(node) => crate::activity::record(
            &instance_id,
            "node-install",
            "ok",
            format!("安装节点 · {}", node.name),
            Some(format!("来源，{log_url}")),
        ),
        Err(e) => crate::activity::record(
            &instance_id,
            "node-install",
            "error",
            format!("安装节点失败 · {log_name}"),
            Some(format!("来源，{log_url}\n{}\n{}", e.code, e.message)),
        ),
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn install_blocking(
    git: &Path,
    settings: &crate::config::GlobalSettings,
    inst: &crate::config::InstanceConfig,
    dir: &Path,
    dest: &Path,
    node_name: &str,
    url: &str,
    with_req: bool,
    on_event: &Channel<VersionEvent>,
) -> Result<NodeInfo, AppError> {
    let env = git_env();
    let display_url = crate::sync::redact_url(url);
    on_event
        .send(VersionEvent::info("clone", format!("克隆 {display_url}…")))
        .ok();
    let out = proc::run_stream(
        git,
        &[
            "clone",
            "--progress",
            "--depth",
            "1",
            url,
            &dest.to_string_lossy(),
        ],
        None,
        &env,
        &mut |line| {
            let _ = on_event.send(VersionEvent::console(line));
        },
    )?;
    if !out.ok() {
        let mut detail = out.detail();
        if display_url != url {
            // git 的报错有时会原样回显命令行里的 URL，同样不能把令牌带出去。
            detail = detail.replace(url, &display_url);
        }
        let _ = on_event.send(VersionEvent::failed(
            "clone",
            format!("克隆失败：\n{detail}"),
        ));
        // 失败时清掉半成品目录，否则下次会撞「已存在。
        let _ = std::fs::remove_dir_all(dest);
        return Err(AppError::new("CLONE_FAILED", "克隆失败，详情见上方输出"));
    }
    let _ = on_event.send(VersionEvent::done("clone", format!("已克隆到 {node_name}")));

    // 依赖：和「手工装的节点补依赖」走同一条路径（过滤 torch 系、跑 install.py：
    if with_req && dest.join("requirements.txt").exists() {
        let entry = crate::commands::deps::NodeEntry {
            name: node_name.to_string(),
            dir: dest.to_path_buf(),
            enabled: true,
        };
        let mut emit = |ev: VersionEvent| {
            let _ = on_event.send(ev);
        };
        match crate::commands::deps::install_for_node(inst, settings, &entry, &mut emit) {
            Ok(result) => {
                let failed = result.failed.len();
                if failed == 0 {
                    let _ = on_event.send(VersionEvent::done(
                        "deps",
                        format!("节点依赖安装完成，{} 项）", result.installed.len()),
                    ));
                } else {
                    let _ = on_event.send(VersionEvent::warn(
                        "deps",
                        format!("{failed} 个依赖没装上，见上面的输出"),
                    ));
                }
                crate::activity::record(
                    &inst.id,
                    "node-deps",
                    if failed == 0 { "ok" } else { "warn" },
                    format!("装节点时补依赖路 {}", entry.name),
                    Some(crate::commands::deps::describe_node(&result).join("\n")),
                );
            }
            Err(e) => {
                let _ = on_event.send(VersionEvent::warn(
                    "deps",
                    format!("节点已装上，但依赖没装成，{}", e.message),
                ));
                crate::activity::record(
                    &inst.id,
                    "node-deps",
                    "warn",
                    format!("装节点时补依赖失败路 {}", entry.name),
                    Some(format!("{}\n{}", e.code, e.message)),
                );
            }
        }
    }

    let all = scan_nodes(dir)?;
    all.into_iter()
        .find(|n| n.name == node_name)
        .ok_or_else(|| AppError::new("NODE_NOT_FOUND", "安装后没扫描到该节点"))
}

#[tauri::command]
pub fn nodes_toggle(
    state: State<'_, AppState>,
    instance_id: String,
    name: String,
    enabled: bool,
) -> Result<(), AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "切换节点状态")?;
    let inst = load_instance(&instance_id)?;
    let dir = nodes_dir(&inst.root_dir);
    validate_name(&name)?;
    ensure_stopped(state.inner(), &instance_id)?;

    let from = target_dir(&dir, &name, !enabled);
    let to = target_dir(&dir, &name, enabled);
    if !from.exists() {
        return Err(AppError::new(
            "NODE_NOT_FOUND",
            format!("找不到节点目录{}", from.display()),
        ));
    }
    if to.exists() {
        return Err(AppError::new(
            "NODE_EXISTS",
            format!("目标目录已存在：{}", to.display()),
        ));
    }
    std::fs::rename(&from, &to)
        .map_err(|e| AppError::new("NODE_RENAME", format!("改名失败，{e}")))?;
    crate::activity::record(
        &instance_id,
        "node-toggle",
        "ok",
        format!("{}节点 · {}", if enabled { "启用" } else { "禁用" }, name),
        None,
    );
    log::info!(
        "节点 {name} 已{}",
        if enabled {
            "enabled after restart"
        } else {
            "disabled after restart"
        }
    );
    Ok(())
}

#[tauri::command]
pub async fn nodes_update(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    name: String,
    on_event: Channel<VersionEvent>,
) -> Result<NodeUpdate, AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "更新节点")?;
    let inst = load_instance(&instance_id)?;
    let dir = nodes_dir(&inst.root_dir);
    validate_name(&name)?;
    let node = target_dir(&dir, &name, true);
    if !node.exists() {
        return Err(AppError::new(
            "NODE_NOT_FOUND",
            "节点目录不存在（可能被禁用了，",
        ));
    }
    if !node.join(".git").exists() {
        return Err(AppError::new(
            "NODE_NOT_GIT",
            "该节点不是git 仓库，无法更新",
        ));
    }
    let git = git_exe(&app, state.inner())?;
    let running = state.runs.lock().contains_key(&instance_id);
    let log_name = name.clone();

    // git pull 也可能慢（远端慢、仓库大），同样扔到阻塞线程
    let result = tauri::async_runtime::spawn_blocking(move || {
        let env = git_env();
        on_event
            .send(VersionEvent::info("pull", format!("更新 {name}…")))
            .ok();
        // 游离 HEAD 没有 upstream，裸 `git pull` 会报 "You are not currently on a branch"。
        // 显式给出 origin 的 HEAD，让远端默认分支快进到当前游离 HEAD；
        // --ff-only 仍然保证不会产生合并提交或覆盖本地分叉。
        let branch = proc::run(
            &git,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            Some(&node),
            &env,
        )
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
        let args: Vec<&str> = if branch.is_empty() || branch == "HEAD" {
            vec!["pull", "--progress", "--ff-only", "origin", "HEAD"]
        } else {
            vec!["pull", "--progress", "--ff-only"]
        };
        let out = proc::run_stream(
            &git,
            &args,
            Some(&node),
            &env,
            &mut |line| {
                let _ = on_event.send(VersionEvent::console(line));
            },
        )?;
        if !out.ok() {
            let _ = on_event.send(VersionEvent::failed(
                "pull",
                format!("更新失败：\n{}", out.detail()),
            ));
            return Err(AppError::new(
                "NODE_PULL_FAILED",
                "更新失败，详情见上方输出",
            ));
        }
        let _ = on_event.send(VersionEvent::done("pull", "更新完成（重启 ComfyUI 生效）"));
        if running {
            log::warn!("实例正在运行，节点更新要重启后才生效");
        }
        // pull 已经 fetch 过远端，这里直接读本地 refs 就能得到最新状态，
        // 不再多发一次网络请求；前端拿到就能立刻把「可更新」改成「已最新」。
        let fresh = check_one_update(&git, &name, &node, false);
        Ok((out.stdout.trim().to_string(), fresh))
    })
    .await
    .map_err(|e| AppError::new("NODE_UPDATE_JOIN", format!("更新线程异常，{e}")))?;

    match &result {
        Ok((summary, _)) => crate::activity::record(
            &instance_id,
            "node-update",
            "ok",
            format!("更新节点 · {log_name}"),
            Some(summary.clone()).filter(|s| !s.trim().is_empty()),
        ),
        Err(e) => crate::activity::record(
            &instance_id,
            "node-update",
            "error",
            format!("更新节点失败 · {log_name}"),
            Some(format!("{}\n{}", e.code, e.message)),
        ),
    }
    result.map(|(_, fresh)| fresh)
}

/// 一个节点的「有没有新版本」结论
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeUpdate {
    pub name: String,
    /// 远端比本地多的提交数， = 已是最新。浅克隆下数字可能不准，界面只说「可更新」。
    pub behind: i64,
    pub local: Option<String>,
    pub remote: Option<String>,
    /// 检查失败的原因（网络/ 权限…）；失败时界面仍允许手动更新
    pub error: Option<String>,
}

/// 检查哪些git 节点在远端有新提交：fetch 分FETCH_HEAD，不动工作区
#[tauri::command]
pub async fn nodes_check_updates(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    names: Option<Vec<String>>,
) -> Result<Vec<NodeUpdate>, AppError> {
    let inst = load_instance(&instance_id)?;
    let dir = nodes_dir(&inst.root_dir);
    let git = git_exe(&app, state.inner())?;
    let wanted: Option<Vec<String>> =
        names.map(|v| v.into_iter().map(|n| n.to_lowercase()).collect());
    let targets: Vec<(String, PathBuf)> = scan_nodes(&dir)?
        .into_iter()
        .filter(|n| n.kind == "git")
        .filter(|n| {
            wanted
                .as_ref()
                .map(|w| w.iter().any(|x| x.eq_ignore_ascii_case(&n.name)))
                .unwrap_or(true)
        })
        .filter_map(|n| {
            let path = dir.join(&n.name);
            path.join(".git").exists().then_some((n.name, path))
        })
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // 每个节点一次fetch：0 秒超时），串行跑在阻塞线程里
    tauri::async_runtime::spawn_blocking(move || {
        targets
            .into_iter()
            .map(|(name, path)| check_one_update(&git, &name, &path, true))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| AppError::new("NODE_CHECK_JOIN", format!("检查更新线程异常：{e}")))
}

fn rev_parse(git: &Path, dir: &Path, rev: &str) -> Option<String> {
    proc::run(git, &["rev-parse", rev], Some(dir), &git_env())
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `fetch_remote = false` 用于刚 pull 完的场景：pull 已经更新了远端 ref，
/// 再 fetch 一次只会让「更新完立刻出结论」多等一个网络往返。
fn check_one_update(git: &Path, name: &str, dir: &Path, fetch_remote: bool) -> NodeUpdate {
    let local = rev_parse(git, dir, "HEAD");
    let branch = proc::run(
        git,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        Some(dir),
        &git_env(),
    )
    .ok()
    .filter(|o| o.ok())
    .map(|o| o.stdout.trim().to_string())
    .filter(|s| !s.is_empty());
    // 注意：不能加 --depth=1 —— 在完整的克隆上打浅边界会让本地和远端「分叉」，
    // 之后 pull --ff-only 会直接失败。节点仓库都很小，整仓库 fetch 就够了。
    if fetch_remote {
        let fetched = crate::commands::deps::run_with_timeout(
            git,
            &["fetch", "--no-tags", "origin"],
            dir,
            &git_env(),
            std::time::Duration::from_secs(30),
            &mut |_| {},
        );
        match fetched {
            Ok((true, _, false)) => {}
            Ok((_, output, timed_out)) => {
                return NodeUpdate {
                    name: name.to_string(),
                    behind: 0,
                    local,
                    remote: None,
                    error: Some(if timed_out {
                        "拉取远端超时（30 秒）".to_string()
                    } else {
                        let tail: Vec<&str> = output.lines().rev().take(3).collect();
                        format!(
                            "拉取远端失败，{}",
                            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
                        )
                    }),
                }
            }
            Err(e) => {
                return NodeUpdate {
                    name: name.to_string(),
                    behind: 0,
                    local,
                    remote: None,
                    error: Some(e.message),
                }
            }
        }
    }

    // 远端比较对象：分支节点用当前分支的 remote-tracking ref；
    // 游离 HEAD 没有当前分支，先看远端默认分支 origin/HEAD，再退到 FETCH_HEAD。
    // 不能先看 FETCH_HEAD：整仓 fetch 会写入多条分支记录，它不一定是默认分支，
    // 曾把 8.0.2 这种旁支误当成「可更新」，点更新却拉的是 main。
    let detached = branch
        .as_deref()
        .map(|b| b.is_empty() || b == "HEAD")
        .unwrap_or(true);
    let mut candidates: Vec<String> = Vec::new();
    if detached {
        candidates.push("origin/HEAD".to_string());
        candidates.push("FETCH_HEAD".to_string());
    } else {
        if let Some(b) = branch.as_deref() {
            candidates.push(format!("origin/{b}"));
        }
        candidates.push("FETCH_HEAD".to_string());
        candidates.push("origin/HEAD".to_string());
    }

    let mut remote = None;
    let mut behind = 0i64;
    for rev in &candidates {
        let Some(sha) = rev_parse(git, dir, rev) else {
            continue;
        };
        if let Some(l) = &local {
            if *l == sha {
                remote = Some(sha);
                behind = 0;
                break;
            }
        }
        behind = proc::run(
            git,
            &["rev-list", "--count", &format!("HEAD..{rev}")],
            Some(dir),
            &git_env(),
        )
        .ok()
        .filter(|o| o.ok())
        .and_then(|o| o.stdout.trim().parse::<i64>().ok())
        .unwrap_or(0);
        remote = Some(sha);
        break;
    }

    NodeUpdate {
        name: name.to_string(),
        behind,
        local,
        remote,
        error: None,
    }
}

/// 卸载：git 节点直接删；手工目录必须显式允许（里面可能塞了模型或权重，
#[tauri::command]
pub async fn nodes_remove(
    app: tauri::AppHandle,
    instance_id: String,
    name: String,
    allow_manual: Option<bool>,
) -> Result<(), AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        nodes_remove_impl(state, instance_id, name, allow_manual)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn nodes_remove_impl(
    state: State<'_, AppState>,
    instance_id: String,
    name: String,
    allow_manual: Option<bool>,
) -> Result<(), AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "移除节点")?;
    let inst = load_instance(&instance_id)?;
    let dir = nodes_dir(&inst.root_dir);
    validate_name(&name)?;
    ensure_stopped(state.inner(), &instance_id)?;
    let enabled = target_dir(&dir, &name, true);
    let disabled = target_dir(&dir, &name, false);
    let path = if enabled.exists() { enabled } else { disabled };
    if !path.exists() {
        return Err(AppError::new("NODE_NOT_FOUND", "节点目录不存在"));
    }
    let is_git = path.join(".git").exists();
    if !is_git && !allow_manual.unwrap_or(false) {
        return Err(AppError::new(
            "NODE_MANUAL_CONFIRM",
            "该节点不是git 仓库，删除后无法恢复，请再确认一次",
        ));
    }
    std::fs::remove_dir_all(&path)
        .map_err(|e| AppError::new("NODE_REMOVE", format!("删除失败，{e}")))?;
    crate::activity::record(
        &instance_id,
        "node-remove",
        "ok",
        format!("移除节点 · {name}"),
        Some(format!(
            "类型，{}\n目录，{}",
            if is_git {
                "git 仓库"
            } else {
                "手工节点（已确认："
            },
            path.display()
        )),
    );
    log::info!("已删除节点{name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_name_from_url() {
        assert_eq!(
            name_from_url("https://github.com/ltdrdata/ComfyUI-Manager.git"),
            "ComfyUI-Manager"
        );
        assert_eq!(
            name_from_url("https://github.com/ltdrdata/ComfyUI-Manager/"),
            "ComfyUI-Manager"
        );
        assert_eq!(name_from_url("git@github.com:a/b.git"), "b");
    }

    #[test]
    fn rejects_dangerous_names() {
        assert!(validate_name("ok-name_1.x").is_ok());
        assert!(validate_name("..\\evil").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("...").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("trailing.").is_err());
        assert!(validate_name("C:").is_err());
    }

    #[test]
    fn rejects_local_and_incomplete_urls() {
        assert!(validate_git_url("https://github.com/a/b.git").is_ok());
        assert!(validate_git_url("git@github.com:a/b.git").is_ok());
        assert!(validate_git_url("httpfoo").is_err());
        assert!(validate_git_url("C:\\local\\repo").is_err());
        assert!(validate_git_url("https://").is_err());
        assert!(validate_git_url("git@host").is_err());
    }

    /// 扫描要能区分启用/禁用、识别git 节点与手工节点
    #[test]
    fn scans_enabled_and_disabled_nodes() {
        let root = std::env::temp_dir().join("comfyfleet-nodes-scan");
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("ComfyUI").join("custom_nodes");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // git 节点（造一个最小 .git）
        let git_node = dir.join("ComfyUI-Manager");
        std::fs::create_dir_all(git_node.join(".git")).expect("mkdir");
        std::fs::write(
            git_node.join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/ltdrdata/ComfyUI-Manager.git\n",
        )
        .expect("write config");
        std::fs::write(git_node.join(".git").join("HEAD"), "ref: refs/heads/main\n").expect("head");
        std::fs::create_dir_all(git_node.join(".git").join("refs").join("heads")).expect("mkdir");
        std::fs::write(
            git_node
                .join(".git")
                .join("refs")
                .join("heads")
                .join("main"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .expect("ref");
        std::fs::write(git_node.join("requirements.txt"), "numpy\n").expect("req");

        // 被禁用的手工节点
        std::fs::create_dir_all(dir.join("my-node.disabled")).expect("mkdir");

        let nodes = scan_nodes(&dir).expect("scan");
        let manager = nodes
            .iter()
            .find(|n| n.name == "ComfyUI-Manager")
            .expect("manager");
        assert!(manager.enabled);
        assert_eq!(manager.kind, "git");
        assert_eq!(
            manager.url.as_deref(),
            Some("https://github.com/ltdrdata/ComfyUI-Manager.git")
        );
        assert_eq!(manager.commit.as_deref(), Some("0123456"));
        assert!(manager.has_requirements);

        let manual = nodes.iter().find(|n| n.name == "my-node").expect("manual");
        assert!(!manual.enabled);
        assert_eq!(manual.kind, "manual");

        let _ = std::fs::remove_dir_all(&root);
    }
}
