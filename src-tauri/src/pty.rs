//! PTY 会话管理（Windows 走 ConPTY，由 portable-pty 提供）。
//!
//! 输出以 base64 传输，前端用 `TextDecoder({stream:true})` 解码，
//! 这样跨 read 边界的中文/UTF-8 多字节字符不会被截断成乱码。

use std::collections::BTreeMap;
use std::io::{Read, Write};

use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;

use crate::error::AppError;

#[derive(Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PtyEvent {
    /// 一段原始字节（base64）。前端解码后再写入 xterm。
    Data { b64: String },
    /// 进程退出。`code` 为退出码，-1 表示取不到。
    Exit { code: i32 },
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl PtySession {
    pub fn write(&mut self, data: &str) -> Result<(), AppError> {
        self.writer
            .write_all(data.as_bytes())
            .map_err(|e| AppError::new("PTY_WRITE", e.to_string()))?;
        self.writer
            .flush()
            .map_err(|e| AppError::new("PTY_WRITE", e.to_string()))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(3),
            cols: cols.max(20),
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // 无论是显式 pty_kill 还是 HashMap 被清空，都不能把 cmd/conhost 留在后台。
        let _ = self.killer.kill();
    }
}

pub struct SpawnOptions {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// 追加或覆盖的环境变量
    pub env: BTreeMap<String, String>,
    /// 追加到 PATH 最前面的目录（通常是实例 venv 的 Scripts）
    pub path_prepend: Option<String>,
}

pub fn spawn(
    options: SpawnOptions,
    on_event: Channel<PtyEvent>,
) -> Result<(String, PtySession), AppError> {
    let SpawnOptions {
        program,
        args,
        cwd,
        cols,
        rows,
        env,
        path_prepend,
    } = options;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(3),
            cols: cols.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AppError::new("PTY_OPEN", e.to_string()))?;

    let mut cmd = CommandBuilder::new(&program);
    for a in &args {
        cmd.arg(a);
    }
    if let Some(dir) = cwd.as_ref() {
        cmd.cwd(dir);
    }
    // 全链路 UTF-8，避免 Windows 控制台 GBK 乱码
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    for (key, value) in &env {
        cmd.env(key, value);
    }
    if let Some(prepend) = path_prepend.as_ref() {
        let existing = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{prepend};{existing}"));
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AppError::new("PTY_SPAWN", e.to_string()))?;

    let killer = child.clone_killer();
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| AppError::new("PTY_READER", e.to_string()))?;
    let writer = master
        .take_writer()
        .map_err(|e| AppError::new("PTY_WRITER", e.to_string()))?;

    let id = uuid::Uuid::new_v4().to_string();

    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                    if on_event.send(PtyEvent::Data { b64 }).is_err() {
                        break;
                    }
                }
            }
        }
        let code = child
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        let _ = on_event.send(PtyEvent::Exit { code });
    });

    Ok((
        id,
        PtySession {
            master,
            writer,
            killer,
        },
    ))
}
