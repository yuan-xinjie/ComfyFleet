//! ComfyUI 版本管理（M4）：查当前版本、列可用 tag、切换、更新
//!
//! 三条硬规则：
//! 1. **实例在跑时不许切版本**（正在执行的代码换掉会让人彻底看不懂现象，
//! 2. **工作区有未提交改动时先列清理清单**：ComfyUI 里手改过文件很常见，
//!    用户确认后再 reset/clean 指定的文件，不静默覆盖。
//! 3. 切完版本要*重装 requirements**：不同tag 的依赖不同，不重装就会「新代码 + 旧依赖。
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::ipc::Channel;
use tauri::Manager;
use tauri::{AppHandle, State};

use crate::commands::instance::load_instance;
use crate::config::{self, InstanceConfig, InstanceState};
use crate::error::AppError;
use crate::proc::{self, git_env};
use crate::runtime;
use crate::state::AppState;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    /// tag 名，没有 tag 时退回短 commit
    pub current: String,
    pub commit: String,
    /// 分支名；detached（停在某个tag）时显"HEAD"
    pub branch: String,
    pub dirty: bool,
    pub channel: String,
    pub remote: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionDirtyFile {
    /// git status 的两字符状态，如 M / ?? / R
    pub status: String,
    /// 相对 ComfyUI 目录的路径；重命名时取新路径
    pub path: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagInfo {
    pub name: String,
    pub date: String,
    pub current: bool,
}

/// 开发版列表里的一条提交
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    pub date: String,
    pub subject: String,
    pub current: bool,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionEvent {
    pub stage: String,
    pub message: String,
    pub level: String,
    pub done: bool,
    pub failed: bool,
}

impl VersionEvent {
    pub fn info(stage: &str, message: impl Into<String>) -> Self {
        // 所有控制台/进度事件都经过同一入口：子进程（git/uv）可能把带 userinfo
        // 或 token 查询参数的 URL 原样回显，不能只在个别调用点脱敏。
        let message = crate::sync::redact_url(&message.into());
        Self {
            stage: stage.into(),
            message,
            level: "info".into(),
            done: false,
            failed: false,
        }
    }
    pub fn warn(stage: &str, message: impl Into<String>) -> Self {
        Self {
            level: "warn".into(),
            ..Self::info(stage, message)
        }
    }
    pub fn done(stage: &str, message: impl Into<String>) -> Self {
        Self {
            done: true,
            ..Self::info(stage, message)
        }
    }
    pub fn failed(stage: &str, message: impl Into<String>) -> Self {
        Self {
            level: "error".into(),
            failed: true,
            done: true,
            ..Self::info(stage, message)
        }
    }
    /// 子进程的原始输出行：日志弹窗当控制台显示，不做任何包装
    pub fn console(message: impl Into<String>) -> Self {
        Self {
            stage: "console".into(),
            ..Self::info("console", message)
        }
    }
}

pub fn comfy_dir(root: &str) -> PathBuf {
    PathBuf::from(root).join("ComfyUI")
}

/// 开发版跟随的分支。ComfyUI 的默认分支就是 master（不是 main）。
pub const DEV_BRANCH: &str = "master";

fn git_exe(app: &AppHandle, state: &AppState) -> Result<PathBuf, AppError> {
    let settings = state.settings.lock().clone();
    let info = crate::git::resolve(app, &settings)?;
    Ok(PathBuf::from(info.path))
}

/// 读当前版本状态（纯本地操作，不联网）
pub fn read_version(git: &Path, dir: &Path, channel: &str) -> Result<VersionInfo, AppError> {
    if !dir.join(".git").exists() {
        return Err(AppError::new(
            "NOT_A_REPO",
            format!(
                "{} 不是 git 仓库（可能是导入的安装或归档包），无法做版本管理",
                dir.display()
            ),
        ));
    }
    let env = git_env();
    let describe = proc::run_ok(git, &["describe", "--tags", "--always"], Some(dir), &env)?;
    let commit = proc::run_ok(git, &["rev-parse", "--short", "HEAD"], Some(dir), &env)?;
    let branch = proc::run_ok(git, &["rev-parse", "--abbrev-ref", "HEAD"], Some(dir), &env)?;
    let status = proc::run_ok(git, &["status", "--porcelain"], Some(dir), &env)?;
    let remote = proc::run(git, &["remote", "get-url", "origin"], Some(dir), &env)
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();

    Ok(VersionInfo {
        current: describe,
        commit,
        branch,
        dirty: !status.trim().is_empty(),
        channel: channel.to_string(),
        remote,
    })
}

/// 列出工作区里会被切版本清理的改动。只展示 git 未忽略的文件；`--untracked-files=all`
/// 会把未跟踪目录展开到文件，确认清单和实际清理范围一致。
pub fn list_dirty_files(git: &Path, dir: &Path) -> Result<Vec<VersionDirtyFile>, AppError> {
    let env = git_env();
    // 直接 run 不 run_ok：run_ok 会 trim 首个状态位前面的空格（" M main.py" → "M main.py"）。
    // -z 避免 git 给含空格/中文的路径加引号，清理时可以原样作为 pathspec。
    let out = proc::run(
        git,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        Some(dir),
        &env,
    )?;
    if !out.ok() {
        return Err(AppError::new(
            "CMD_FAILED",
            format!("读取工作区状态失败：\n{}", out.detail()),
        ));
    }
    let mut fields = out.stdout.split('\0');
    let mut files = Vec::new();
    while let Some(line) = fields.next() {
        if line.len() < 3 {
            continue;
        }
        let status = line[..2].trim().to_string();
        // 不能 trim：Windows 允许文件名以空格开头，trim 后会指向另一个路径。
        let path = line[3..].to_string();
        // rename/copy 的 -z 输出会再跟一个「原路径」字段，跳过它。
        if status.starts_with('R') || status.starts_with('C') {
            let _ = fields.next();
        }
        files.push(VersionDirtyFile { status, path });
    }
    Ok(files)
}

/// 清理确认过的工作区改动。`reset --hard` 处理已跟踪文件；未跟踪文件按
/// `git status -uall` 的明确路径删除，不用无路径的 `git clean -fd` 横扫整个目录。
fn clean_dirty_workspace(
    git: &Path,
    dir: &Path,
    files: &[VersionDirtyFile],
    on: &Channel<VersionEvent>,
) -> Result<(), AppError> {
    if files.is_empty() {
        return Ok(());
    }
    let reset = run_git(git, &["reset", "--hard", "HEAD"], dir, on)?;
    if !reset.ok() {
        return Err(AppError::new(
            "CLEAN_FAILED",
            format!("清理已跟踪改动失败：\n{}", reset.detail()),
        ));
    }
    // 只清理确认清单里的路径；分批避免 Windows 命令行过长。
    for chunk in files.chunks(40) {
        let mut args = vec!["clean".to_string(), "-fd".to_string(), "--".to_string()];
        // 必须用 literal pathspec：Windows 允许 `[abc]` 这样的文件名，
        // 默认 pathspec 会把 `[abc]` 当通配符，连 `a` 一起删掉。
        args.extend(chunk.iter().map(|file| format!(":(literal){}", file.path)));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let cleaned = run_git(git, &refs, dir, on)?;
        if !cleaned.ok() {
            return Err(AppError::new(
                "CLEAN_FAILED",
                format!("清理未提交文件失败：\n{}", cleaned.detail()),
            ));
        }
    }
    let remaining = list_dirty_files(git, dir)?;
    if !remaining.is_empty() {
        return Err(AppError::new(
            "CLEAN_FAILED",
            format!(
                "清理后仍有 {} 个未提交文件，先手动处理后才能继续",
                remaining.len()
            ),
        ));
    }
    on.send(VersionEvent::done(
        "clean",
        format!("已清理 {} 个未提交文件", files.len()),
    ))
    .ok();
    Ok(())
}

/// 列出本地已有 tag（按版本号倒序，纯本地操作），日期取发版时间到秒。
pub fn list_tags_local(git: &Path, dir: &Path) -> Result<Vec<TagInfo>, AppError> {
    let env = git_env();
    let current = proc::run(
        git,
        &["describe", "--tags", "--exact-match", "HEAD"],
        Some(dir),
        &env,
    )
    .ok()
    .filter(|o| o.ok())
    .map(|o| o.stdout.trim().to_string());

    let raw = proc::run_ok(
        git,
        &[
            "for-each-ref",
            // 按版本号倒序（v0.43.1 > v0.3.10）；但 creatordate 在同一秒内会乱序，
            // 实测本地仓库两个 tag 同秒创建时顺序不可靠
            "--sort=-v:refname",
            // 日期 + 具体时间（本地时区），列表里每一行都要能看出是哪天几点发的
            "--format=%(refname:short)|%(creatordate:iso-local)",
            "refs/tags",
        ],
        Some(dir),
        &env,
    )?;

    Ok(raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (name, date) = line.split_once('|').unwrap_or((line, ""));
            TagInfo {
                name: name.trim().to_string(),
                date: date.trim().to_string(),
                current: current.as_deref() == Some(name.trim()),
            }
        })
        .collect())
}

/// 版本号比较：v0.43.1 > v0.3.10（逐段按数字比，非数字段退化为字符串比，
fn version_key(name: &str) -> Vec<(u32, String)> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| match part.parse::<u32>() {
            Ok(n) => (n, String::new()),
            Err(_) => (0, part.to_string()),
        })
        .collect()
}

/// 是不是真实发版 tag（`v0.43.1` 这种）。
///
/// ComfyUI 的远端还有一个会移动的`latest` tag：它的名字按版本号排序会排到最前面，
/// 界面上「最新稳定版」就会变成`latest`。它指向「当前的某个发版」，不是稳定的锚点，
/// 不能当版本号用。
fn is_version_tag(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('v') else {
        return false;
    };
    let mut parts = 0;
    for part in rest.split('.') {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

fn sort_desc(tags: &mut [TagInfo]) {
    tags.sort_by(|a, b| {
        // 先按「是不是版本 tag」分组（版本 tag 在前），组内再按版本号倒序
        is_version_tag(&b.name)
            .cmp(&is_version_tag(&a.name))
            .then_with(|| version_key(&b.name).cmp(&version_key(&a.name)))
    });
}

/// 列出可用 tag：本地 + 远端。
///
/// 只读本地不够用：安装时是 `clone --depth 1 --branch <tag>`：
/// 本地只有一个 tag，列表会只剩一项（实测 v0.43.1 那次就是）。
/// 远端名字本身没有日期，所以缺失的 tag 会按需浅拉提交对象（不拉文件树），
/// 让每一行都能显示发版日期和具体时间。
pub fn list_tags(git: &Path, dir: &Path) -> Result<Vec<TagInfo>, AppError> {
    let mut local = list_tags_local(git, dir)?;
    let env = git_env();

    // 远端失败（离线/ 没有 origin）就只用本地，不报错
    let remote: Vec<String> = proc::run(
        git,
        &["ls-remote", "--tags", "--refs", "origin"],
        Some(dir),
        &env,
    )
    .ok()
    .filter(|out| out.ok())
    .map(|out| {
        out.stdout
            .lines()
            .filter_map(|line| {
                let (_, refname) = line.split_once('\t')?;
                let name = refname.trim().strip_prefix("refs/tags/")?;
                (!name.ends_with("^{}")).then(|| name.to_string())
            })
            .collect()
    })
    .unwrap_or_default();

    // 只拿到远端名字时没有日期：把 tag 对象按需浅拉下来（不要文件树），
    // 这样 for-each-ref 才能给出每个版本的发版时间。已经全在本地时不联网。
    let missing = remote
        .iter()
        .any(|name| !local.iter().any(|t| &t.name == name));
    if missing {
        let _ = proc::run(
            git,
            &[
                "fetch",
                "--depth=1",
                "--filter=tree:0",
                "origin",
                "+refs/tags/*:refs/tags/*",
            ],
            Some(dir),
            &env,
        );
        local = list_tags_local(git, dir)?;
    }
    let current = local.iter().find(|t| t.current).map(|t| t.name.clone());

    // fetch 失败（离线 / 服务端不支持 filter）时仍列出远端名字，日期留空
    for name in remote {
        if !local.iter().any(|t| t.name == name) {
            local.push(TagInfo {
                date: String::new(),
                current: current.as_deref() == Some(&name),
                name,
            });
        }
    }

    sort_desc(&mut local);
    local.truncate(60);
    Ok(local)
}

/// 开发版 = master 分支。列出它的最近提交（需要联网）。
///
/// 浅克隆里本地只有 1 个提交，列不出历史，所以要按需技`origin/master`
/// 加深分`limit + 5` 个提交。只取提交对象（不取文件内容），几十个提交
/// 也就几十 KB，比 `--unshallow`（整个仓库）便宜得多。
///
/// 拉不到远端（离线 / 没有 origin）时退化成"只有本地已有的历史：
/// 不报错：至少还能看到当前停在哪。
pub fn list_dev_commits(git: &Path, dir: &Path, limit: usize) -> Result<Vec<CommitInfo>, AppError> {
    if !dir.join(".git").exists() {
        return Err(AppError::new(
            "NOT_A_REPO",
            format!(
                "{} 不是 git 仓库（可能是导入的安装或归档包），没有开发版可列",
                dir.display()
            ),
        ));
    }
    let env = git_env();
    let head = proc::run(git, &["rev-parse", "HEAD"], Some(dir), &env)
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
    let limit = limit.clamp(1, 100);

    // 显式 refspec：`git fetch origin master` 只更新 FETCH_HEAD，
    // 不一定更新 origin/master，后面就没得可 log 了
    let depth = (limit + 5).to_string();
    let refspec = format!("+refs/heads/{DEV_BRANCH}:refs/remotes/origin/{DEV_BRANCH}");
    let _ = proc::run(
        git,
        &["fetch", "--depth", &depth, "origin", &refspec],
        Some(dir),
        &env,
    );

    let range = format!("origin/{DEV_BRANCH}");
    let fmt = "--format=%H%x1f%h%x1f%ad%x1f%s";
    let raw = proc::run(
        git,
        &[
            "log",
            fmt,
            "--date=iso-local",
            "-n",
            &limit.to_string(),
            &range,
        ],
        Some(dir),
        &env,
    )
    .ok()
    .filter(|o| o.ok())
    .map(|o| o.stdout)
    .unwrap_or_else(|| {
        // 没有 origin/master（离线还没fetch 过）就看本地 HEAD
        proc::run(
            git,
            &[
                "log",
                fmt,
                "--date=iso-local",
                "-n",
                &limit.to_string(),
                "HEAD",
            ],
            Some(dir),
            &env,
        )
        .map(|o| o.stdout)
        .unwrap_or_default()
    });

    Ok(raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\u{1f}');
            let sha = parts.next()?.trim().to_string();
            let short = parts.next().unwrap_or("").trim().to_string();
            let date = parts.next().unwrap_or("").trim().to_string();
            let subject = parts.next().unwrap_or("").trim().to_string();
            if sha.is_empty() {
                return None;
            }
            let short = if short.is_empty() {
                short_sha(&sha).to_string()
            } else {
                short
            };
            Some(CommitInfo {
                current: sha == head,
                short,
                sha,
                date,
                subject,
            })
        })
        .collect())
}

/// 与远端比较落后多少提交（需要联网）。
///
/// 返回值：`Some(0)` = 已是最新，`Some(n)` = 落后 n 个提交，
/// `None` = 有更新但数不清（浅克隆的历史被截断，见下）。
pub fn check_behind(git: &Path, dir: &Path, branch: &str) -> Result<Option<u32>, AppError> {
    if branch == "HEAD" || branch.is_empty() {
        return Ok(Some(0));
    }
    SwitchKind::Branch.validate_target(branch)?;
    let env = git_env();
    proc::run_ok(
        git,
        &["fetch", "--depth", "1", "origin", branch],
        Some(dir),
        &env,
    )?;
    let count = proc::run_ok(
        git,
        &["rev-list", "--count", "HEAD..FETCH_HEAD"],
        Some(dir),
        &env,
    )?;
    let behind: u32 = count.trim().parse().unwrap_or(0);
    if behind == 0 {
        return Ok(Some(0));
    }
    // 本应用是 `clone --depth 1`（浅克隆），每次 fetch 也只发depth 1：    // 远端 tip 的祖先被截断，`rev-list --count` 只会数出 1，报「落后1 个提交」是假的。    // 这种情况只能说「有更新」，不能编一个数字。
    let shallow = proc::run(
        git,
        &["rev-parse", "--is-shallow-repository"],
        Some(dir),
        &env,
    )
    .ok()
    .filter(|o| o.ok())
    .map(|o| o.stdout.trim() == "true")
    .unwrap_or(false);
    if shallow {
        Ok(None)
    } else {
        Ok(Some(behind))
    }
}

/// 切换目标的类型。由界面显式给出，不靠名字猜：
/// 远端有`latest` 这种不以 `v` 开头的 tag，靠名字猜会报tag 当成分支，
/// 而「开发版里的某个提交」既不是 tag 也不是分支，只能是游离HEAD。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchKind {
    /// Release tag (stable release)
    Tag,
    /// Branch, follows latest development
    Branch,
    /// A specific commit
    Commit,
}

impl SwitchKind {
    pub fn parse(text: &str) -> Result<Self, AppError> {
        match text {
            "tag" => Ok(Self::Tag),
            "branch" => Ok(Self::Branch),
            "commit" => Ok(Self::Commit),
            other => Err(AppError::new(
                "BAD_KIND",
                format!("未知的切换目标类型「{other}」（只支持 tag / branch / commit）"),
            )),
        }
    }

    /// 切完之后记进 instance.json 的通道
    fn channel(self) -> &'static str {
        match self {
            Self::Tag => "stable",
            Self::Branch | Self::Commit => "dev",
        }
    }

    fn label(self, target: &str) -> String {
        match self {
            Self::Tag => target.to_string(),
            Self::Branch => format!("{target} 分支"),
            Self::Commit => format!("提交 {}", short_sha(target)),
        }
    }

    /// 拒绝以 `-` 开头的 git 选项注入，以及控制字符/非法 ref 片段。
    fn validate_target(self, target: &str) -> Result<(), AppError> {
        let target = target.trim();
        let ok = !target.is_empty()
            && target.len() <= 200
            && !target.starts_with('-')
            && !target.contains('\0')
            && !target.contains('\n')
            && !target.contains('\r')
            && !target.contains("..");
        let ok = ok
            && match self {
                Self::Commit => {
                    target.len() >= 4
                        && target.len() <= 40
                        && target.chars().all(|c| c.is_ascii_hexdigit())
                }
                Self::Tag | Self::Branch => target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+')),
            };
        if !ok {
            return Err(AppError::new(
                "BAD_TARGET",
                format!("非法的 git 切换目标：{target}（不能以 - 开头，不能含 .. 或控制字符）"),
            ));
        }
        Ok(())
    }
}

pub fn short_sha(sha: &str) -> &str {
    // 返回 &str 会要求借用原串；取前 7 个字符的边界，不能按字节硬切。
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// 跑一条git 命令：命令本身和输出都原样进日志。
///
/// 日志要能当控制台看（用户切版本失败时会整段贴出来），
/// 所以连 `$ git fetch …` 这一行也发出去，而不是只发自己的结论。
fn run_git(
    git: &Path,
    args: &[&str],
    dir: &Path,
    on: &Channel<VersionEvent>,
) -> Result<proc::CmdOutput, AppError> {
    let env = git_env();
    // 阻塞式管道里 git 默认不打印进度；加 --progress 才能看到 Receiving objects 和 MiB/s。
    let mut owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    if args.first() == Some(&"fetch") && !owned.iter().any(|a| a == "--progress") {
        owned.insert(1, "--progress".to_string());
    }
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let _ = on.send(VersionEvent::console(format!("$ git {}", owned.join(" "))));
    proc::run_stream(git, &refs, Some(dir), &env, &mut |line| {
        let _ = on.send(VersionEvent::console(line));
    })
}

/// 切到指定 tag / 分支 / 提交。
pub fn switch_to(
    git: &Path,
    dir: &Path,
    target: &str,
    kind: SwitchKind,
    on: &Channel<VersionEvent>,
) -> Result<(), AppError> {
    let target = target.trim();
    kind.validate_target(target)?;
    match kind {
        SwitchKind::Tag => {
            let env = git_env();
            // 本地已有这个 tag 就不必联网（离线切版本要能用，
            let local = proc::run(
                git,
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/tags/{target}"),
                ],
                Some(dir),
                &env,
            )
            .map(|o| o.ok())
            .unwrap_or(false);
            if !local {
                // 浅克隆里时tag 的提交可能不存在，按需单取一个（比--unshallow 省一个数量级流量，
                let out = run_git(
                    git,
                    &["fetch", "--depth", "1", "origin", "tag", target],
                    dir,
                    on,
                )?;
                if !out.ok() {
                    return Err(AppError::new(
                        "FETCH_FAILED",
                        format!("可tag {target} 失败：\n{}", out.detail()),
                    ));
                }
            }
            let out = run_git(
                git,
                &["checkout", "--detach", &format!("tags/{target}")],
                dir,
                on,
            )?;
            if !out.ok() {
                return Err(AppError::new(
                    "CHECKOUT_FAILED",
                    format!("切换到{target} 失败：\n{}", out.detail()),
                ));
            }
        }
        SwitchKind::Branch => {
            let fetch = run_git(git, &["fetch", "--depth", "1", "origin", target], dir, on)?;
            if fetch.ok() {
                let out = run_git(git, &["checkout", "-B", target, "FETCH_HEAD"], dir, on)?;
                if !out.ok() {
                    return Err(AppError::new(
                        "CHECKOUT_FAILED",
                        format!("切换到分支{target} 失败：\n{}", out.detail()),
                    ));
                }
            } else {
                // 拉不到远端（离线 / 没有 origin）时退回本地分支
                let out = run_git(git, &["checkout", target], dir, on)?;
                if !out.ok() {
                    return Err(AppError::new(
                        "FETCH_FAILED",
                        format!(
                            "取分支{target} 失败，本地也没有该分支：\n{}",
                            fetch.detail()
                        ),
                    ));
                }
            }
        }
        SwitchKind::Commit => {
            // 只要这一个提交：浅克隆按需单取，不把整个历史拉下来
            let fetch = run_git(git, &["fetch", "--depth", "1", "origin", target], dir, on)?;
            if fetch.ok() {
                let out = run_git(git, &["checkout", "--detach", target], dir, on)?;
                if !out.ok() {
                    return Err(AppError::new(
                        "CHECKOUT_FAILED",
                        format!("切到提交 {} 失败：\n{}", short_sha(target), out.detail()),
                    ));
                }
            } else {
                let out = run_git(git, &["checkout", "--detach", target], dir, on)?;
                if !out.ok() {
                    return Err(AppError::new(
                        "FETCH_FAILED",
                        format!(
                            "取提交{} 失败，本地也没有该提交：\n{}",
                            short_sha(target),
                            fetch.detail()
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 在分支上更新到远端最新。返回(git 输出, 是否「强制对齐到远端。。
///
/// 浅克隆（本应用用 `--depth 1` 安装）里新拉下来的tip 与本地HEAD 之间没有
/// 共同祖先，`merge --ff-only` 会被 git 直接拒绝（实测：`refusing to merge
/// unrelated histories`）。这时只能把分支指到远端 tip：工作区已由调用方确认干净：
/// 时HEAD 仍在 reflog 里（`git reset --hard HEAD@{1}` 可找回）。
pub fn pull_ff(
    git: &Path,
    dir: &Path,
    branch: &str,
    on: &Channel<VersionEvent>,
) -> Result<(String, bool), AppError> {
    SwitchKind::Branch.validate_target(branch)?;
    let fetched = run_git(git, &["fetch", "--depth", "1", "origin", branch], dir, on)?;
    if !fetched.ok() {
        return Err(AppError::new(
            "FETCH_FAILED",
            format!("拉取 {branch} 失败：\n{}", fetched.detail()),
        ));
    }

    let merged = run_git(git, &["merge", "--ff-only", "FETCH_HEAD"], dir, on)?;
    if merged.ok() {
        return Ok((merged.stdout.trim().to_string(), false));
    }

    // 只有“浅克隆没有共同祖先”才允许对齐远端。若 HEAD 与 FETCH_HEAD
    // 有共同祖先却仍不能 ff，说明本地有额外提交/工作区变化，reset --hard
    // 会静默丢弃它们；这种情况必须报错让用户处理。
    let has_common = proc::run(
        git,
        &["merge-base", "HEAD", "FETCH_HEAD"],
        Some(dir),
        &git_env(),
    )
    .map(|o| o.ok())
    .unwrap_or(false);
    if has_common {
        return Err(AppError::new(
            "UPDATE_NOT_FAST_FORWARD",
            format!(
                "无法快进且本地与远端有共同历史，拒绝 reset --hard（可能丢本地提交）。请先处理工作区后重试：\n{}",
                merged.detail()
            ),
        ));
    }

    let reset = run_git(git, &["reset", "--hard", "FETCH_HEAD"], dir, on)?;
    if !reset.ok() {
        return Err(AppError::new(
            "UPDATE_FAILED",
            format!(
                "快进失败，对齐远端也失败：\n{}\n{}",
                merged.detail(),
                reset.detail()
            ),
        ));
    }
    Ok((
        "浅克隆没有共同祖先，已直接对齐远端 tip（旧 HEAD 在 reflog：git reset --hard HEAD@{1}）"
            .to_string(),
        true,
    ))
}

fn ensure_stopped(state: &AppState, id: &str) -> Result<(), AppError> {
    if state.runs.lock().contains_key(id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行，请先停止再切版本（切完还要重装依赖，",
        ));
    }
    Ok(())
}

/// 切完版本后按新版本requirements 重装依赖。
///
/// 成功返回给操作日志用的摘要（index + 最后几十行输出）；
/// 失败返回「给用户看的错误 + 给日志用的完整输出」——日志要留证据，弹窗要短。
fn reinstall_requirements(
    state: &AppState,
    inst: &InstanceConfig,
    on: &Channel<VersionEvent>,
    cancel: &AtomicBool,
) -> Result<String, (AppError, String)> {
    if cancel.load(Ordering::Relaxed) {
        let msg = "已取消依赖安装（代码本身的切换/更新已完成）";
        let _ = on.send(VersionEvent::failed("deps", msg));
        return Err((AppError::new("VERSION_CANCELLED", msg), msg.to_string()));
    }
    let settings = state.settings.lock().clone();
    let root = Path::new(&inst.root_dir);
    // 没有 Python 环境只说明这次重装做不了，版本本身已经切过去了：
    // 结论要写进日志弹窗，别只弹一个toast 就完了
    let Some(py) = runtime::resolve_python(inst) else {
        let msg = "找不到 Python 环境（实例设置 → Python 环境）：版本已经切换，但依赖没重装";
        let _ = on.send(VersionEvent::failed("deps", msg));
        return Err((AppError::new("VENV_MISSING", msg), msg.to_string()));
    };
    let python = py.python.clone();
    let requirements = comfy_dir(&root.to_string_lossy()).join("requirements.txt");
    if !requirements.exists() {
        let _ = on.send(VersionEvent::warn(
            "deps",
            "新版本没有requirements.txt，跳过依赖安装",
        ));
        return Ok("新版本没有requirements.txt，跳过依赖安装".to_string());
    }

    let uv = crate::commands::env::resolve_uv().map_err(|e| {
        let detail = format!("找不到 uv：{}", e.message);
        (e, detail)
    })?;
    let uv_cache = config::shared_dir().join("uv-cache");
    std::fs::create_dir_all(&uv_cache).map_err(|e| {
        let detail = format!("建uv 缓存目录失败，{e}");
        (AppError::new("UV_CACHE_FAILED", detail.clone()), detail)
    })?;

    let mut env = std::collections::BTreeMap::new();
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

    // 切版本后重装 requirements 也不能动 torch 全家：用当前实测版本钉住。
    // freeze/写约束失败时必须中止，不能在没有约束的情况下继续装依赖，
    // 否则 torchvision/torch 可能被换版本，CUDA 环境直接被破坏。
    let installed = match crate::commands::deps::freeze(&py) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!(
                "无法读取已安装包版本，为保护 torch 已中止依赖重装：{}",
                e.message
            );
            let _ = on.send(VersionEvent::failed("deps", msg.clone()));
            return Err((AppError::new("FREEZE_FAILED", msg.clone()), msg));
        }
    };
    let constraint_path = match crate::commands::deps::write_torch_constraints(&installed) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("写 torch 版本约束失败，已中止依赖重装：{}", e.message);
            let _ = on.send(VersionEvent::failed("deps", msg.clone()));
            return Err((AppError::new("CONSTRAINT_FAILED", msg.clone()), msg));
        }
    };
    let constraint_str = constraint_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    // 镜像同步滞后很常见（新版本包还没同步过去）：失败就换官方源再试一次，
    // 别把「镜像没这个版本」直接甩成「依赖装不上。
    let candidates = crate::pip::index_candidates(settings.mirrors.pypi.as_deref());
    let mut last_detail = String::new();
    let mut last_index = String::new();
    for (n, index_url) in candidates.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let msg = "已取消依赖安装（代码本身的切换/更新已完成）";
            let _ = on.send(VersionEvent::failed("deps", msg));
            return Err((AppError::new("VERSION_CANCELLED", msg), msg.to_string()));
        }
        if n > 0 {
            let _ = on.send(VersionEvent::warn(
                "deps",
                format!("镜像里装不上（可能还没同步到），改用官方 PyPI 重试，{index_url}"),
            ));
        }
        let _ = on.send(VersionEvent::info(
            "deps",
            format!("按新版本 requirements 重装依赖（index {index_url}）…"),
        ));
        let _ = on.send(VersionEvent::console(format!(
            "$ uv pip install --python {} -r {} --index-url {index_url}",
            python.display(),
            requirements.display()
        )));

        let python_str = python.to_string_lossy().to_string();
        let requirements_str = requirements.to_string_lossy().to_string();
        let mut argv: Vec<&str> = vec![
            "pip",
            "install",
            "--python",
            &python_str,
            "-r",
            &requirements_str,
            "--index-url",
            index_url,
        ];
        if let Some(path) = &constraint_str {
            argv.push("--constraint");
            argv.push(path);
        }

        let out = proc::run_stream_cancellable(
            &uv,
            &argv,
            None,
            &env,
            &mut |line| {
                let _ = on.send(VersionEvent::console(line));
            },
            cancel,
        )
        .map_err(|e| {
            let detail = format!("uv 启动失败，{}\n{}", e.code, e.message);
            (e, detail)
        })?;

        if out.ok() {
            let _ = on.send(VersionEvent::done("deps", "依赖已按新版本重装完成"));
            if let Some(path) = &constraint_path {
                let _ = std::fs::remove_file(path);
            }
            // 成功也留证据：装了哪些包 / 是否只是审计一遍，全在 uv 的最后几行里
            return Ok(format!(
                "index {index_url}\n{}",
                tail_detail(&out.detail(), 60)
            ));
        }
        last_detail = out.detail();
        last_index = index_url.clone();
    }

    let _ = on.send(VersionEvent::failed(
        "deps",
        format!("依赖重装失败：\n{last_detail}"),
    ));
    if let Some(path) = &constraint_path {
        let _ = std::fs::remove_file(path);
    }
    Err((
        AppError::new(
            "DEPS_FAILED",
            "依赖重装失败。版本已经切过去了，只是环境可能不完整：再点一次「切换」会重跑依赖重装，或在「快照」页回滚到「切换版本前」那条。",
        ),
        format!("index {last_index}\n{last_detail}"),
    ))
}

/// 输出太长时留最后几十行：uv 的结论（Installed / Audited / error）都在结尾
fn tail_detail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.trim_end().to_string();
    }
    let skipped = lines.len() - max_lines;
    format!("…（省略前{skipped} 行）\n{}", lines[skipped..].join("\n"))
}

/// 分拉完代码但依赖没装成：代码已经是新的了，别让实例看起来一切正常。
/// 保留 Uninitialized（还没装环境的实例不该被这个覆盖），其余标成 Error，概览和诊断一眼能看到。
fn mark_env_broken(inst: &InstanceConfig, on: &Channel<VersionEvent>) {
    let mut cfg = inst.clone();
    if !matches!(cfg.state, InstanceState::Uninitialized) {
        cfg.state = InstanceState::Error;
    }
    let _ = crate::commands::instance::save_instance(&cfg);
    let _ = on.send(VersionEvent::warn(
        "deps",
        "实例状态已标成「出错」：版本是新的，但依赖没装全，直接启动可能失败",
    ));
}

#[tauri::command]
pub async fn version_info(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<VersionInfo, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_info_impl(handle.clone(), state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_info_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<VersionInfo, AppError> {
    let inst = load_instance(&instance_id)?;
    let git = git_exe(&app, state.inner())?;
    read_version(&git, &comfy_dir(&inst.root_dir), &inst.comfy.channel)
}

/// 切换/更新前的清理清单。只读，不改工作区。
#[tauri::command]
pub async fn version_dirty_files(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<Vec<VersionDirtyFile>, AppError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        let inst = load_instance(&instance_id)?;
        let git = git_exe(&handle, state.inner())?;
        list_dirty_files(&git, &comfy_dir(&inst.root_dir))
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

#[tauri::command]
pub async fn version_tags(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<Vec<TagInfo>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_tags_impl(handle.clone(), state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_tags_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<Vec<TagInfo>, AppError> {
    let inst = load_instance(&instance_id)?;
    let git = git_exe(&app, state.inner())?;
    list_tags(&git, &comfy_dir(&inst.root_dir))
}

/// 开发版（master）的最近提交，供「可用版本→开发版」列表用；需要联网
#[tauri::command]
pub async fn version_commits(
    app: tauri::AppHandle,
    instance_id: String,
    limit: Option<u32>,
) -> Result<Vec<CommitInfo>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_commits_impl(handle.clone(), state, instance_id, limit)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_commits_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    limit: Option<u32>,
) -> Result<Vec<CommitInfo>, AppError> {
    let inst = load_instance(&instance_id)?;
    let git = git_exe(&app, state.inner())?;
    list_dev_commits(
        &git,
        &comfy_dir(&inst.root_dir),
        limit.unwrap_or(30) as usize,
    )
}

#[tauri::command]
pub async fn version_check(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<Option<u32>, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_check_impl(handle.clone(), state, instance_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_check_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<Option<u32>, AppError> {
    let inst = load_instance(&instance_id)?;
    let git = git_exe(&app, state.inner())?;
    let dir = comfy_dir(&inst.root_dir);
    read_version(&git, &dir, &inst.comfy.channel)?;
    check_behind(&git, &dir, DEV_BRANCH)
}

/// 取消正在进行的版本切换/更新的依赖重装阶段。
#[tauri::command]
pub fn version_cancel(state: State<'_, AppState>, instance_id: String) -> Result<(), AppError> {
    crate::commands::instance::validate_instance_id(&instance_id)?;
    let map = state.version_cancels.lock();
    match map.get(&instance_id) {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            log::info!("已请求取消实例 {instance_id} 的版本依赖重装");
            Ok(())
        }
        None => Err(AppError::new(
            "NOT_SWITCHING",
            "这个实例当前没有正在进行的版本切换/更新",
        )),
    }
}

#[tauri::command]
pub async fn version_switch(
    app: tauri::AppHandle,
    instance_id: String,
    target: String,
    kind: String,
    clean_workspace: Option<bool>,
    on_event: Channel<VersionEvent>,
) -> Result<VersionInfo, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_switch_impl(
            handle.clone(),
            state,
            instance_id,
            target,
            kind,
            clean_workspace.unwrap_or(false),
            on_event,
        )
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_switch_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    target: String,
    // tag（稳定版， branch（跟随开发版， commit（开发版的某个提交）
    kind: String,
    clean_workspace: bool,
    on_event: Channel<VersionEvent>,
) -> Result<VersionInfo, AppError> {
    // 先抢实例锁，再注册取消标志：否则第二个并发请求会先覆盖/清掉
    // 第一个请求的取消标志，导致正在跑的依赖安装无法取消。
    let _op = crate::state::OpGuard::acquire(&instance_id, "切换版本")?;
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .version_cancels
        .lock()
        .insert(instance_id.clone(), Arc::clone(&cancel));
    let result = version_switch_inner(
        app,
        state.inner(),
        instance_id.clone(),
        target,
        kind,
        clean_workspace,
        on_event,
        &cancel,
    );
    {
        let mut map = state.version_cancels.lock();
        if map
            .get(&instance_id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &cancel))
        {
            map.remove(&instance_id);
        }
    }
    result
}

fn version_switch_inner(
    app: AppHandle,
    state: &AppState,
    instance_id: String,
    target: String,
    kind: String,
    clean_workspace: bool,
    on_event: Channel<VersionEvent>,
    cancel: &AtomicBool,
) -> Result<VersionInfo, AppError> {
    let kind = SwitchKind::parse(&kind)?;
    let target = target.trim().to_string();
    kind.validate_target(&target)?;
    if kind == SwitchKind::Branch && target != DEV_BRANCH {
        return Err(AppError::new(
            "BAD_TARGET",
            format!("开发版分支固定为 {DEV_BRANCH}，不支持切换到其他分支"),
        ));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::new("VERSION_CANCELLED", "已取消版本切换"));
    }
    let mut inst = load_instance(&instance_id)?;
    ensure_stopped(state, &instance_id)?;
    let git = git_exe(&app, state)?;
    let dir = comfy_dir(&inst.root_dir);

    let before = read_version(&git, &dir, &inst.comfy.channel)?;
    if before.dirty && !clean_workspace {
        return Err(AppError::new(
            "DIRTY_TREE",
            "ComfyUI 目录有未提交的改动；先确认清理清单，再切换版本",
        ));
    }
    let label = kind.label(&target);
    // 切换前的状态：出问题就靠这条回原；拍不下来就不冒险切换。
    crate::snapshot::capture_pre_update_required(&inst, &state.settings.lock().clone())?;
    if before.dirty {
        let files = list_dirty_files(&git, &dir)?;
        on_event
            .send(VersionEvent::warn(
                "clean",
                format!(
                    "工作区有 {} 个未提交文件，切换前会清理：\n{}",
                    files.len(),
                    files
                        .iter()
                        .map(|file| format!("  {} {}", file.status, file.path))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ))
            .ok();
        clean_dirty_workspace(&git, &dir, &files, &on_event)?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::new(
            "VERSION_CANCELLED",
            "已取消版本切换（尚未改动代码）",
        ));
    }
    on_event
        .send(VersionEvent::info("checkout", format!("切换到 {label}…")))
        .ok();

    switch_to(&git, &dir, &target, kind, &on_event)?;

    let after = read_version(&git, &dir, &inst.comfy.channel)?;

    // 通道按“切的是什么”记，引用按真实 git 状态记。
    // 停在某个提交上是游离 HEAD，但它仍然是开发版，不能因为 branch=HEAD 就记成稳定版。
    inst.comfy.channel = kind.channel().to_string();
    inst.comfy.reference = if after.branch != "HEAD" {
        after.branch.clone()
    } else {
        target.clone()
    };
    inst.comfy.version = Some(after.current.clone());
    // commit 也要对齐真实 HEAD：否则界面显示新提交、登记文件还留在旧值
    inst.comfy.commit = after.commit.clone();
    crate::commands::instance::save_instance(&inst)?;

    // 说清它停在哪：tag 报提交号、分支报分支名、具体提交报游离 HEAD
    let where_ = match kind {
        SwitchKind::Tag => format!("提交 {}", after.commit),
        SwitchKind::Branch => format!("分支 {}", after.branch),
        SwitchKind::Commit => format!("游离 HEAD，提交{}", after.commit),
    };
    on_event
        .send(VersionEvent::done(
            "checkout",
            format!(
                "已切换到 {}（{where_} · {}）",
                after.current,
                if kind.channel() == "dev" {
                    "开发版"
                } else {
                    "稳定特"
                }
            ),
        ))
        .ok();

    // 依赖重装：失败不回滚版本，但要让用户明确知道环境可能不完整
    let deps_log = match reinstall_requirements(state, &inst, &on_event, cancel) {
        Ok(log) => log,
        Err((e, detail)) => {
            mark_env_broken(&inst, &on_event);
            crate::activity::record(
                &instance_id,
                "version-switch",
                "error",
                format!("切换到{}：依赖没装成", after.current),
                Some(format!(
                    "{} → {}\n提交 {} → {}\n代码已经切过去了，失败的是依赖重装\n\n{detail}",
                    before.current, after.current, before.commit, after.commit
                )),
            );
            return Err(e);
        }
    };
    crate::activity::record(
        &instance_id,
        "version-switch",
        "ok",
        format!("切换到{}", after.current),
        Some(format!(
            "{} → {}\n提交 {} → {}\n依赖已按新版本 requirements 重装\n\n{deps_log}",
            before.current, after.current, before.commit, after.commit
        )),
    );
    crate::snapshot::capture_for_update(
        &inst,
        &state.settings.lock().clone(),
        crate::snapshot::T_POST_UPDATE,
    );

    inst.state = InstanceState::Ready;
    crate::commands::instance::save_instance(&inst)?;
    Ok(after)
}

#[tauri::command]
pub async fn version_update(
    app: tauri::AppHandle,
    instance_id: String,
    clean_workspace: Option<bool>,
    on_event: Channel<VersionEvent>,
) -> Result<VersionInfo, AppError> {
    // 重活丢到阻塞线程：命令在异步线程池里跑，主线程（界面）不再被阻塞
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = handle.state::<AppState>();
        version_update_impl(
            handle.clone(),
            state,
            instance_id,
            clean_workspace.unwrap_or(false),
            on_event,
        )
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAILED", format!("后台线程异常，{e}")))?
}

fn version_update_impl(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
    clean_workspace: bool,
    on_event: Channel<VersionEvent>,
) -> Result<VersionInfo, AppError> {
    // 与切换版本相同：先抢实例锁，再注册取消标志。
    let _op = crate::state::OpGuard::acquire(&instance_id, "更新版本")?;
    let cancel = Arc::new(AtomicBool::new(false));
    state
        .version_cancels
        .lock()
        .insert(instance_id.clone(), Arc::clone(&cancel));
    let result = version_update_inner(
        app,
        state.inner(),
        instance_id.clone(),
        clean_workspace,
        on_event,
        &cancel,
    );
    {
        let mut map = state.version_cancels.lock();
        if map
            .get(&instance_id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &cancel))
        {
            map.remove(&instance_id);
        }
    }
    result
}

fn version_update_inner(
    app: AppHandle,
    state: &AppState,
    instance_id: String,
    clean_workspace: bool,
    on_event: Channel<VersionEvent>,
    cancel: &AtomicBool,
) -> Result<VersionInfo, AppError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::new("VERSION_CANCELLED", "已取消版本更新"));
    }
    let mut inst = load_instance(&instance_id)?;
    ensure_stopped(state, &instance_id)?;
    let git = git_exe(&app, state)?;
    let dir = comfy_dir(&inst.root_dir);
    let before = read_version(&git, &dir, &inst.comfy.channel)?;
    if before.dirty && !clean_workspace {
        return Err(AppError::new(
            "DIRTY_TREE",
            "有未提交的改动；先确认清理清单，再更新",
        ));
    }
    if before.branch == "HEAD" {
        return Err(AppError::new(
            "DETACHED_HEAD",
            format!("当前没有跟随分支（停在某个 tag / 提交上），先切到开发版 {DEV_BRANCH} 再更新"),
        ));
    }
    if before.branch != DEV_BRANCH {
        return Err(AppError::new(
            "WRONG_BRANCH",
            format!(
                "当前分支是 {}，版本更新固定跟随 {DEV_BRANCH}；先切到开发版 {DEV_BRANCH} 再更新",
                before.branch
            ),
        ));
    }

    let branch = DEV_BRANCH.to_string();
    crate::snapshot::capture_pre_update_required(&inst, &state.settings.lock().clone())?;
    if before.dirty {
        let files = list_dirty_files(&git, &dir)?;
        on_event
            .send(VersionEvent::warn(
                "clean",
                format!(
                    "工作区有 {} 个未提交文件，更新前会清理：\n{}",
                    files.len(),
                    files
                        .iter()
                        .map(|file| format!("  {} {}", file.status, file.path))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ))
            .ok();
        clean_dirty_workspace(&git, &dir, &files, &on_event)?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(AppError::new(
            "VERSION_CANCELLED",
            "已取消版本更新（尚未拉取代码）",
        ));
    }
    on_event
        .send(VersionEvent::info(
            "pull",
            format!("拉取 {branch} 最新提交"),
        ))
        .ok();
    let (log, hard_synced) = pull_ff(&git, &dir, &branch, &on_event)?;
    let after = read_version(&git, &dir, &inst.comfy.channel)?;
    inst.comfy.version = Some(after.current.clone());
    inst.comfy.commit = after.commit.clone();
    crate::commands::instance::save_instance(&inst)?;
    // 快进成功时git 自己那句 "Updating x..y" 有信息量；走了对齐远端的分支则由下面的warn 说明
    let head = format!("已更新到 {}（{}）", after.current, after.commit);
    on_event
        .send(VersionEvent::done(
            "pull",
            if hard_synced || log.is_empty() {
                head
            } else {
                format!("{head}\n{log}")
            },
        ))
        .ok();
    if hard_synced {
        on_event
            .send(VersionEvent::warn(
                "pull",
                "浅克隆里 git 数不出共同祖先，这次是直接对齐远端tip；本地若有自己的提交，请用reflog 找回",
            ))
            .ok();
    }

    let deps_log = match reinstall_requirements(state, &inst, &on_event, cancel) {
        Ok(log) => log,
        Err((e, detail)) => {
            mark_env_broken(&inst, &on_event);
            crate::activity::record(
                &instance_id,
                "version-update",
                "error",
                format!("更新到{}：依赖没装成", after.current),
                Some(format!(
                    "分支 {branch}\n提交 {} → {}\n代码已经拉过去了，失败的是依赖重装\n\n{detail}",
                    before.commit, after.commit
                )),
            );
            return Err(e);
        }
    };
    inst.state = InstanceState::Ready;
    crate::commands::instance::save_instance(&inst)?;
    crate::activity::record(
        &instance_id,
        "version-update",
        "ok",
        format!("更新到{}", after.current),
        Some(format!(
            "分支 {branch}\n提交 {} → {}\n依赖已按新版本 requirements 重装\n\n{deps_log}",
            before.commit, after.commit
        )),
    );
    crate::snapshot::capture_for_update(
        &inst,
        &state.settings.lock().clone(),
        crate::snapshot::T_POST_UPDATE,
    );
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的空事件通道：日志内容不参与断言，只要不 panic
    fn ch() -> Channel<VersionEvent> {
        Channel::new(|_| Ok(()))
    }

    #[test]
    fn version_events_redact_url_credentials() {
        let event = VersionEvent::console(
            "fatal: https://user:ghp_secret@github.com/a/b.git?token=abc&x=1",
        );
        assert!(!event.message.contains("ghp_secret"), "{}", event.message);
        assert!(!event.message.contains("token=abc"), "{}", event.message);
        assert!(
            event.message.contains("***@github.com/a/b.git"),
            "{}",
            event.message
        );
    }

    /// 远端那个会移动的 `latest` tag 必须排在版本号后面，否则界面「最新稳定版」会变成 latest
    #[test]
    fn tags_sort_versions_first() {
        let mk = |name: &str| TagInfo {
            name: name.to_string(),
            date: String::new(),
            current: false,
        };
        let mut tags = vec![
            mk("latest"),
            mk("v0.3.10"),
            mk("v0.43.0"),
            mk("v0.43.1"),
            mk("nightly"),
        ];
        sort_desc(&mut tags);
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names[0], "v0.43.1");
        assert_eq!(names[1], "v0.43.0");
        assert_eq!(names[2], "v0.3.10");
        assert!(names[3..].contains(&"latest"), "非版本tag 应排在后面");
        assert!(!is_version_tag("latest"));
        assert!(is_version_tag("v0.43.1"));
        assert!(is_version_tag("v1.2"));
        assert!(!is_version_tag("v"));
        assert!(!is_version_tag("v1"));
    }

    /// 造一个本地仓库（不联网），验证版本读取/ 分tag / 切换 / dirty 检测
    #[test]
    fn version_flow_on_scratch_repo() {
        let git = match crate::git::ensure() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip: {}", e.message);
                return;
            }
        };
        let dir = std::env::temp_dir().join("comfyfleet-version-scratch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        let mut env = git_env();
        env.insert("GIT_AUTHOR_NAME".into(), "t".into());
        env.insert("GIT_AUTHOR_EMAIL".into(), "t@t".into());
        env.insert("GIT_COMMITTER_NAME".into(), "t".into());
        env.insert("GIT_COMMITTER_EMAIL".into(), "t@t".into());

        let step = |args: &[&str]| {
            let out = proc::run(&git, args, Some(&dir), &env).expect("git run");
            assert!(out.ok(), "git {args:?} 失败：\n{}", out.detail());
            out.stdout
        };

        step(&["init", "-q", "-b", "master"]);
        std::fs::write(dir.join("main.py"), "print(1)\n").expect("write");
        step(&["add", "."]);
        step(&["commit", "-q", "-m", "first"]);
        step(&["tag", "v0.1"]);
        std::fs::write(dir.join("main.py"), "print(2)\n").expect("write");
        step(&["commit", "-q", "-am", "second"]);
        step(&["tag", "v0.2"]);

        // 当前版本 = v0.2，分支master，干净
        let info = read_version(&git, &dir, "dev").expect("read_version");
        assert_eq!(info.current, "v0.2");
        assert_eq!(info.branch, "master");
        assert!(!info.dirty);
        assert_eq!(info.commit.len(), 7);

        // tag 列表：本地两个，按版本号倒序
        let tags = list_tags_local(&git, &dir).expect("list_tags_local");
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["v0.2", "v0.1"]);
        assert!(tags[0].current, "v0.2 应标记为当前版本");
        assert!(
            tags[0].date.contains(' ') && tags[0].date.contains(':'),
            "tag 日期要带具体时间，实际是 {:?}",
            tags[0].date
        );

        // 合并远端时不能把版本号比成字符串序（v0.43.1 必须排在 v0.3.10 前面，
        let mut merged = vec![
            TagInfo {
                name: "v0.3.10".into(),
                date: String::new(),
                current: false,
            },
            TagInfo {
                name: "v0.43.1".into(),
                date: String::new(),
                current: false,
            },
            TagInfo {
                name: "v0.9.99".into(),
                date: String::new(),
                current: false,
            },
        ];
        sort_desc(&mut merged);
        let order: Vec<&str> = merged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(order, vec!["v0.43.1", "v0.9.99", "v0.3.10"]);

        // 切到时tag：变成游离HEAD
        switch_to(&git, &dir, "v0.1", SwitchKind::Tag, &ch()).expect("switch to tag");
        let info = read_version(&git, &dir, "stable").expect("read");
        assert_eq!(info.current, "v0.1");
        assert_eq!(info.branch, "HEAD");

        // 停在 tag 上更新应当被识别为detached（命令层会拒绝）
        assert_eq!(info.branch, "HEAD");

        // 切回开发版分支
        switch_to(&git, &dir, "master", SwitchKind::Branch, &ch()).expect("switch back");
        let info = read_version(&git, &dir, "dev").expect("read");
        assert_eq!(info.branch, "master");

        // 改文件 → dirty 必须为真，清理清单要能列出被改的文件
        std::fs::write(dir.join("main.py"), "print(3)\n").expect("write");
        std::fs::write(dir.join("untracked.txt"), "temp\n").expect("write");
        // Windows 允许文件名以空格开头；状态解析不能 trim 掉它
        std::fs::write(dir.join(" leading.txt"), "temp\n").expect("write");
        let info = read_version(&git, &dir, "dev").expect("read");
        assert!(info.dirty, "未提交改动必须被识别");

        let files = list_dirty_files(&git, &dir).expect("dirty list");
        assert!(files.iter().any(|f| f.path == "main.py" && f.status == "M"));
        assert!(files
            .iter()
            .any(|f| f.path == "untracked.txt" && f.status == "??"));
        assert!(
            files
                .iter()
                .any(|f| f.path == " leading.txt" && f.status == "??"),
            "前导空格文件名必须原样保留：{files:?}"
        );
        clean_dirty_workspace(&git, &dir, &files, &ch()).expect("clean dirty");
        assert!(!read_version(&git, &dir, "dev").expect("read").dirty);
        assert_eq!(
            std::fs::read_to_string(dir.join("main.py")).unwrap().trim(),
            "print(2)"
        );
        assert!(!dir.join("untracked.txt").exists(), "未跟踪文件应被清理");
        assert!(
            !dir.join(" leading.txt").exists(),
            "前导空格文件也必须能按原路径清理"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机上的仓库是`clone --depth 1`（浅克隆）：
    /// 切到 master、再更新到最新，都必须能用（旧实现会在merge --ff-only 上失败）
    #[test]
    fn shallow_clone_can_switch_channel_and_update() {
        let git = match crate::git::ensure() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skip: {}", e.message);
                return;
            }
        };
        let base =
            std::env::temp_dir().join(format!("comfyfleet-shallow-{}", uuid::Uuid::new_v4()));
        let origin = base.join("origin");
        let clone = base.join("clone");
        std::fs::create_dir_all(&origin).expect("mkdir origin");

        let mut env = git_env();
        for (k, v) in [
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ] {
            env.insert(k.into(), v.into());
        }
        let run_in = |dir: &Path, args: &[&str]| -> String {
            let out = proc::run(&git, args, Some(dir), &env).expect("git run");
            assert!(out.ok(), "git {args:?} 失败：\n{}", out.detail());
            out.stdout.trim().to_string()
        };

        run_in(&origin, &["init", "-q", "-b", "master"]);
        std::fs::write(origin.join("main.py"), "print(1)\n").expect("write");
        run_in(&origin, &["add", "."]);
        run_in(&origin, &["commit", "-q", "-m", "first"]);
        run_in(&origin, &["tag", "v0.1"]);
        std::fs::write(origin.join("main.py"), "print(2)\n").expect("write");
        run_in(&origin, &["commit", "-q", "-am", "second"]);

        // 和应用安装时一样：depth 1 + 停在 tag 一
        let url = format!(
            "file:///{}",
            origin.display().to_string().replace('\\', "/")
        );
        run_in(
            &base,
            &[
                "clone", "-q", "--depth", "1", "--branch", "v0.1", &url, "clone",
            ],
        );

        // 稳定版 → 开发版
        switch_to(&git, &clone, "master", SwitchKind::Branch, &ch())
            .expect("浅克隆也要能切到 master");
        let info = read_version(&git, &clone, "dev").expect("read");
        assert_eq!(info.branch, "master", "切完应在 master 分支");

        // 远端前进一个提交 → 浅克隆里 merge --ff-only 会被 git 拒绝，必须仍有出路
        std::fs::write(origin.join("main.py"), "print(3)\n").expect("write");
        run_in(&origin, &["commit", "-q", "-am", "third"]);
        let remote_tip = run_in(&origin, &["rev-parse", "HEAD"]);

        let (_log, hard_synced) = pull_ff(&git, &clone, "master", &ch()).expect("浅克隆也要能更新");
        assert!(hard_synced, "浅克隆应走「对齐远端 tip」的兜底");
        assert_eq!(
            run_in(&clone, &["rev-parse", "HEAD"]),
            remote_tip,
            "更新后应与远端 tip 一致"
        );

        // 浅克隆说得出「有更新」，但数不出几个提交 —— 不能编数字
        std::fs::write(origin.join("main.py"), "print(4)\n").expect("write");
        run_in(&origin, &["commit", "-q", "-am", "fourth"]);
        assert_eq!(check_behind(&git, &clone, "master").unwrap(), None);

        // 开发版列表：能列出 master 的最近提交（浅克隆按需加深），最新一条在最前
        let commits = list_dev_commits(&git, &clone, 10).expect("列开发版提交");
        assert!(commits.len() >= 3, "应能列出多条提交，{commits:?}");
        assert_eq!(commits[0].sha, run_in(&origin, &["rev-parse", "HEAD"]));
        assert_eq!(commits[0].subject, "fourth");
        assert!(commits.iter().any(|c| c.subject == "second"));

        // 切到开发版里的某个具体提交：游离HEAD，内容回到那一列
        let second = commits
            .iter()
            .find(|c| c.subject == "second")
            .expect("应能找到 second 提交")
            .sha
            .clone();
        switch_to(&git, &clone, &second, SwitchKind::Commit, &ch()).expect("应能切到指定提交");
        let info = read_version(&git, &clone, "dev").expect("read");
        assert_eq!(info.branch, "HEAD", "切到具体提交应是游离 HEAD");
        assert_eq!(run_in(&clone, &["rev-parse", "HEAD"]), second);
        assert_eq!(
            std::fs::read_to_string(clone.join("main.py"))
                .unwrap()
                .trim(),
            "print(2)"
        );

        // 再切图tag：稳定版依旧是tag
        switch_to(&git, &clone, "v0.1", SwitchKind::Tag, &ch()).expect("切回 tag");
        let info = read_version(&git, &clone, "stable").expect("read");
        assert_eq!(info.branch, "HEAD");
        assert_eq!(
            std::fs::read_to_string(clone.join("main.py"))
                .unwrap()
                .trim(),
            "print(1)"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
