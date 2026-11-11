//! 子进程执行小工具：统一隐藏控制台窗口、收集输出、禁止 git 交互式提示。
//!
//! 单独一层的理由：git / uv 的调用散落在版本管理、节点管理、安装里，
//! 每次都要写一遍 `creation_flags(0x0800_0000)` 和 `GIT_TERMINAL_PROMPT=0`，
//! 漏一次就会弹出黑窗口或卡在等密码。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// 失败时的简短说明（stderr 优先，其次 stdout）
    pub fn detail(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim().to_string()
        } else {
            self.stderr.trim().to_string()
        };
        let lines: Vec<&str> = text.lines().rev().take(6).collect();
        lines.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

/// 非交互环境的 git 变量：CI/无终端时不至于卡在凭据提示上
pub fn git_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    env.insert("GIT_ASKPASS".to_string(), "echo".to_string());
    env.insert("GCM_INTERACTIVE".to_string(), "never".to_string());
    env.insert("GIT_PYTHON_REFRESH".to_string(), "quiet".to_string());
    env
}

pub fn run(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<CmdOutput, AppError> {
    let exe = exe.as_ref();
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // 不能直接用 cmd.output()：git/uv 卡在凭据提示或死锁时没有上限。
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45 * 60);
    let mut child = cmd.spawn().map_err(|e| {
        AppError::new(
            "CMD_FAILED",
            format!("执行失败（{}）：{e}", exe.to_string_lossy()),
        )
    })?;
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                use std::io::Read;
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_thread = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let err_thread = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let deadline = std::time::Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = crate::runtime::kill_pid_tree(child.id());
                let _ = child.wait();
                return Err(AppError::new(
                    "CMD_TIMEOUT",
                    format!(
                        "{} 超过 {} 分钟没有结束，已终止进程树",
                        exe.to_string_lossy(),
                        TIMEOUT.as_secs() / 60
                    ),
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => {
                let _ = crate::runtime::kill_pid_tree(child.id());
                let _ = child.wait();
                return Err(AppError::new(
                    "CMD_FAILED",
                    format!("等待进程失败（{}）：{e}", exe.to_string_lossy()),
                ));
            }
        }
    };
    // 子进程已退出，但孙进程可能还继承着 stdout/stderr 管道；无限 join 会让命令线程
    // 永远卡住。给读线程一个短窗口，拿不到就放弃输出（不阻塞调用方）。
    let join_short = |h: std::thread::JoinHandle<Vec<u8>>| -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !h.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if h.is_finished() {
            h.join().unwrap_or_default()
        } else {
            log::warn!("子进程输出管道仍被占用，放弃等待读线程");
            Vec::new()
        }
    };
    let stdout = join_short(out_thread);
    let stderr = join_short(err_thread);
    Ok(CmdOutput {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
}

/// 把子进程输出按 `\n` / `\r\n` 切成日志行。
///
/// `\r` 单独出现表示终端里“回到行首覆盖当前行”，pip/tqdm 的进度条就是这样
/// 刷新的。这里只保留该行最后一次内容，不把每次刷新都变成一条新日志；否则
/// 一次大文件下载会灌出几千行 `1% / 2% / 3%…`。
fn pump<R: std::io::Read>(
    mut reader: R,
    tx: std::sync::mpsc::Sender<(bool, String)>,
    is_err: bool,
) {
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last_progress = std::time::Instant::now();
    let progress_interval = std::time::Duration::from_secs(1);

    let emit = |segment: &[u8]| -> bool {
        let text = String::from_utf8_lossy(segment).trim().to_string();
        if text.is_empty() {
            return true;
        }
        tx.send((is_err, text)).is_ok()
    };

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        pending.extend_from_slice(&buf[..n]);
        // 恶意/异常程序可能一直不换行；超过 64KB 就分段发出，避免无限增长。
        if pending.len() > 64 * 1024 {
            let rest = pending.split_off(64 * 1024);
            if !emit(&pending) {
                return;
            }
            pending = rest;
            if pending.len() > 64 * 1024 {
                pending.clear();
            }
        }

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

/// 边跑边把输出逐行回调出去（`on_line` 也会收到 stderr 的行）。
///
/// 和 `run` 的区别只是"什么时候能看到输出"：这里是实时的，
/// 所以切版本、装依赖这种要几分钟的活儿用它，界面能当控制台看。
pub fn run_stream(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    on_line: &mut dyn FnMut(&str),
) -> Result<CmdOutput, AppError> {
    run_stream_impl(exe, args, cwd, env, on_line, None)
}

/// 可取消的流式执行：取消后杀进程树并返回 `CMD_CANCELLED`。
pub fn run_stream_cancellable(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    on_line: &mut dyn FnMut(&str),
    cancel: &AtomicBool,
) -> Result<CmdOutput, AppError> {
    run_stream_impl(exe, args, cwd, env, on_line, Some(cancel))
}

/// 用伪终端跑命令：pip / huggingface / git 只有认为自己在终端里，才会输出
/// 带速度的进度条。install.py 往往再拉起这些子进程，普通管道会把进度吞掉。
///
/// 返回 `(输出, 是否超时)`。输出按行回调；PTY 的 stdout/stderr 是合并的，
/// 统一放进 `stdout`，失败时 `detail()` 仍能取到最后几行。
#[allow(dead_code)]
pub fn run_stream_tty(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout: Option<Duration>,
    on_line: &mut dyn FnMut(&str),
) -> Result<(CmdOutput, bool), AppError> {
    let exe = exe.as_ref();
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 240,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AppError::new("CMD_TTY_OPEN", format!("打开伪终端失败：{e}")))?;

    let mut cmd = CommandBuilder::new(exe);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AppError::new("CMD_TTY_SPAWN", format!("启动失败：{e}")))?;
    let pid = child.process_id();
    let mut killer = child.clone_killer();
    drop(pair.slave);

    let master = pair.master;
    let reader = master
        .try_clone_reader()
        .map_err(|e| AppError::new("CMD_TTY_READER", format!("读取伪终端失败：{e}")))?;
    // 保留 writer 到进程退出：Windows ConPTY 上过早关闭写端会变成 Ctrl+C，
    // install.py 会被误杀。这些命令本身不会读 stdin，保留也不会阻塞。
    let _writer = master
        .take_writer()
        .map_err(|e| AppError::new("CMD_TTY_WRITER", format!("写入伪终端失败：{e}")))?;

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>();
    std::thread::spawn(move || pump(reader, tx, false));

    let timeout = timeout.unwrap_or(Duration::from_secs(45 * 60));
    let deadline = Instant::now() + timeout;
    let mut out = CmdOutput {
        code: -1,
        stdout: String::new(),
        stderr: String::new(),
    };
    let mut timed_out = false;

    let take_line = |out: &mut CmdOutput, line: &str, on_line: &mut dyn FnMut(&str)| {
        let clean = clean_tty_line(line);
        if clean.trim().is_empty() {
            return;
        }
        if out.stdout.len() < 2 * 1024 * 1024 {
            out.stdout.push_str(&clean);
            out.stdout.push('\n');
        }
        on_line(&clean);
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok((_, line)) => take_line(&mut out, &line, on_line),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let exited = child.try_wait().ok().flatten().is_some();
                if exited {
                    // 子进程退出后可能还有缓冲输出；收干再收尾。
                    while let Ok((_, line)) = rx.try_recv() {
                        take_line(&mut out, &line, on_line);
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    if let Some(pid) = pid {
                        let _ = crate::runtime::kill_pid_tree(pid);
                    }
                    let _ = killer.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
            }
        }
    }

    if !timed_out {
        let status = child
            .wait()
            .map_err(|e| AppError::new("CMD_TTY_WAIT", format!("等待进程失败：{e}")))?;
        out.code = status.exit_code() as i32;
    }
    Ok((out, timed_out))
}

/// 去掉 PTY 里常见的 ANSI 颜色/光标序列和退格，日志只保留可读文字。
fn clean_tty_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') => {
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c == '\u{8}' {
            out.pop();
            continue;
        }
        if c.is_control() && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

fn run_stream_impl(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    on_line: &mut dyn FnMut(&str),
    cancel: Option<&AtomicBool>,
) -> Result<CmdOutput, AppError> {
    let exe = exe.as_ref();
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| {
        AppError::new(
            "CMD_FAILED",
            format!("执行失败（{}）：{e}", exe.to_string_lossy()),
        )
    })?;

    let (tx, rx) = std::sync::mpsc::channel::<(bool, String)>();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(stdout, tx, false));
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump(stderr, tx, true));
    }
    drop(tx);

    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45 * 60);
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut out = CmdOutput {
        code: -1,
        stdout: String::new(),
        stderr: String::new(),
    };
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok((is_err, line)) => {
                if is_err {
                    out.stderr.push_str(&line);
                    out.stderr.push('\n');
                } else {
                    out.stdout.push_str(&line);
                    out.stdout.push('\n');
                }
                on_line(&line);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    let _ = crate::runtime::kill_pid_tree(child.id());
                    let _ = child.wait();
                    return Err(AppError::new(
                        "CMD_CANCELLED",
                        format!("已取消：{}", exe.to_string_lossy()),
                    ));
                }
                if child.try_wait().ok().flatten().is_some() {
                    // 子进程已退出，但孙进程仍占着管道：收干缓冲后收尾。
                    while let Ok((is_err, line)) = rx.try_recv() {
                        if is_err {
                            out.stderr.push_str(&line);
                            out.stderr.push('\n');
                        } else {
                            out.stdout.push_str(&line);
                            out.stdout.push('\n');
                        }
                        on_line(&line);
                    }
                    break;
                }
                if std::time::Instant::now() > deadline {
                    let _ = crate::runtime::kill_pid_tree(child.id());
                    let _ = child.wait();
                    return Err(AppError::new(
                        "CMD_TIMEOUT",
                        format!(
                            "{} 超过 {} 分钟没有结束，已终止进程树",
                            exe.to_string_lossy(),
                            TIMEOUT.as_secs() / 60
                        ),
                    ));
                }
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| AppError::new("WAIT_FAILED", e.to_string()))?;
    out.code = status.code().unwrap_or(-1);
    Ok(out)
}

/// 要求成功的调用：失败时把命令与输出一起抛出来，方便用户贴日志
pub fn run_ok(
    exe: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<String, AppError> {
    let out = run(&exe, args, cwd, env)?;
    if !out.ok() {
        return Err(AppError::new(
            "CMD_FAILED",
            format!("命令失败（退出码 {}）：\n{}", out.code, out.detail()),
        ));
    }
    Ok(out.stdout.trim().to_string())
}

/// 目录大小（MiB），用于节点列表展示；读不了就返回 0，不要让列表整体失败
pub fn dir_size_mib(dir: &Path) -> u32 {
    fn walk(dir: &Path, acc: &mut u64, depth: u32) {
        if depth > 12 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&path, acc, depth + 1),
                Ok(t) if t.is_file() => {
                    if let Ok(meta) = entry.metadata() {
                        *acc += meta.len();
                    }
                }
                _ => {}
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total, 0);
    (total / 1024 / 1024) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_captures_output_and_exit_code() {
        let out = run("cmd", &["/c", "echo hello"], None, &BTreeMap::new()).expect("应能执行");
        assert!(out.ok());
        assert!(out.stdout.contains("hello"));

        let fail = run("cmd", &["/c", "exit 3"], None, &BTreeMap::new()).expect("应能执行");
        assert_eq!(fail.code, 3);
    }

    #[test]
    fn pump_collapses_carriage_return_progress() {
        let (tx, rx) = std::sync::mpsc::channel();
        pump(
            std::io::Cursor::new(b"Downloading a\n0%\r50%\r100%\nDone\n"),
            tx,
            false,
        );
        let lines: Vec<String> = rx.into_iter().map(|(_, line)| line).collect();
        assert_eq!(lines, vec!["Downloading a", "100%", "Done"]);
    }

    #[test]
    fn run_stream_reports_lines_as_they_come() {
        let mut lines = Vec::new();
        let out = run_stream(
            "cmd",
            &["/c", "echo one & echo two 1>&2 & exit 2"],
            None,
            &BTreeMap::new(),
            &mut |line| lines.push(line.to_string()),
        )
        .expect("应能执行");
        assert_eq!(out.code, 2);
        assert!(
            lines.iter().any(|l| l == "one"),
            "stdout 行要实时回调：{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "two"),
            "stderr 行也要回调：{lines:?}"
        );
        assert!(out.stdout.contains("one"));
        assert!(out.stderr.contains("two"));
    }
}
