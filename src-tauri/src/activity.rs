//! 实例操作日志：每次改动这个实例的动作（装节点、切版本、改参数、快照、启停…）
//! 都往 `<数据目录>\activity\<实例 id>.jsonl` 追加一行 JSON。
//!
//! - 追加写，一行一条：崩了也最多丢最后半行（读取时坏行直接跳过）
//! - 文件超过「全局设置 → 操作日志上限」就从最旧的整行开始丢，永远保留最新的
//! - 写失败只 log，绝不让日志拖垮真正的操作。
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::AppError;

/// 单条记录：前端按 `kind` 上色 / 分组，`action` 是那句话，`detail` 展开后看详情。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: String,
    /// 本地时间 RFC3339（带时区偏移），界面显示时再格式化。
    pub at: String,
    /// start / stop / node-install / version-switch / launch-args / storage / snapshot。
    pub kind: String,
    /// ok / info / warn / error
    pub level: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 一次最多给界面这么多条（文件上 2MB 时大概几千条，没必要一次全传）
pub const MAX_LIST: usize = 500;

pub const DEFAULT_CAP_BYTES: u64 = 2 * 1024 * 1024;
pub const MIN_CAP_BYTES: u64 = 64 * 1024;
pub const MAX_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// 单条记录的细节上限（字符）：宁可截断一条，也别让一次安装输出把整份日志挤爆
pub const MAX_DETAIL_CHARS: usize = 8_000;

static WRITE_LOCK: Mutex<()> = Mutex::new(());
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn dir() -> std::path::PathBuf {
    config::app_root().join("activity")
}

/// 实例 id 同样要防目录穿越（和快照的规则一致）
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn path(instance_id: &str) -> std::path::PathBuf {
    // safe_id 之前只定义未使用；这里必须兜底，避免 activity 接口被原始 id 穿越。
    if !safe_id(instance_id) {
        return dir().join("_invalid.jsonl");
    }
    dir().join(format!("{instance_id}.jsonl"))
}

/// 上限按设置在 64 KB  64 MB 之间夹一下：太小会把有用的记录裁没，太大没必。
pub fn cap_bytes() -> u64 {
    config::load_settings()
        .map(|s| s.activity.max_bytes)
        .unwrap_or(DEFAULT_CAP_BYTES)
        .clamp(MIN_CAP_BYTES, MAX_CAP_BYTES)
}

/// 追加一条记录。`level` 只认 ok / info / warn / error（其它按 info 处理）。
pub fn record(
    instance_id: &str,
    kind: &str,
    level: &str,
    action: impl Into<String>,
    detail: Option<String>,
) {
    let entry = ActivityEntry {
        id: next_id(),
        at: chrono::Local::now().to_rfc3339(),
        kind: kind.to_string(),
        level: match level {
            "ok" | "info" | "warn" | "error" => level.to_string(),
            _ => "info".to_string(),
        },
        action: action.into(),
        detail: detail.filter(|d| !d.trim().is_empty()).map(clip_detail),
    };
    if let Err(e) = append_in(&dir(), instance_id, &entry, cap_bytes()) {
        log::warn!("实例 {} 的操作日志写入失败：{}", instance_id, e.message);
    }
}

fn next_id() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}-{seq}", chrono::Local::now().timestamp_millis())
}

/// 细节太长就留头留尾：开头说明装了什么，结尾是真正的报错 / 结论
pub fn clip_detail(detail: String) -> String {
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail;
    }
    let head: String = detail.chars().take(MAX_DETAIL_CHARS * 3 / 4).collect();
    let tail: String = detail
        .chars()
        .skip(detail.chars().count() - MAX_DETAIL_CHARS / 4)
        .collect();
    format!("{head}\n…（中间省略）…\n{tail}")
}

fn append_in(
    base: &Path,
    instance_id: &str,
    entry: &ActivityEntry,
    cap: u64,
) -> Result<(), AppError> {
    if !safe_id(instance_id) {
        return Err(AppError::new(
            "ACTIVITY_BAD_ID",
            "实例 id 不合法，拒绝写日志",
        ));
    }
    // 全局锁：多个命令线程同时追加时不至于互相踩行
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    fs::create_dir_all(base)?;
    let file = base.join(format!("{instance_id}.jsonl"));
    let line = serde_json::to_string(entry)?;
    {
        let mut handle = OpenOptions::new().create(true).append(true).open(&file)?;
        handle.write_all(line.as_bytes())?;
        handle.write_all(b"\n")?;
    }
    trim_to_cap(&file, cap)?;
    Ok(())
}

/// 超过上限就从最旧的整行开始丢；一次只处理一次追加，成本与文件大小同阶但发生在后台线。
fn trim_to_cap(file: &Path, cap: u64) -> Result<(), AppError> {
    let len = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        return Ok(());
    }
    let raw = fs::read(file)?;
    let mut start = 0usize;
    while (raw.len() - start) as u64 > cap {
        match raw[start..].iter().position(|&b| b == b'\n') {
            Some(pos) => start += pos + 1,
            None => {
                start = raw.len();
                break;
            }
        }
    }
    // 原子写：裁剪中途断电不会留下半个文件。
    config::atomic_write(file, &raw[start..])?;
    Ok(())
}

/// 最新的在前；坏行（写一半崩了）跳过，不让一条坏记录毁掉整页。
pub fn list(instance_id: &str, limit: usize) -> Result<Vec<ActivityEntry>, AppError> {
    list_in(&dir(), instance_id, limit)
}

fn list_in(base: &Path, instance_id: &str, limit: usize) -> Result<Vec<ActivityEntry>, AppError> {
    if !safe_id(instance_id) {
        return Err(AppError::new("ACTIVITY_BAD_ID", "实例 id 不合法"));
    }
    let file = base.join(format!("{instance_id}.jsonl"));
    let raw = match fs::read_to_string(&file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(AppError::new(
                "ACTIVITY_READ",
                format!("读操作日志失败：{e}"),
            ))
        }
    };
    let mut out: Vec<ActivityEntry> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ActivityEntry>(line).ok())
        .collect();
    out.reverse();
    out.truncate(limit.max(1).min(MAX_LIST));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("comfyfleet-activity-{name}"));
        let _ = fs::remove_dir_all(&base);
        base
    }

    fn entry(action: &str) -> ActivityEntry {
        ActivityEntry {
            id: "x".to_string(),
            at: "2026-10-30T12:00:00+08:00".to_string(),
            kind: "test".to_string(),
            level: "info".to_string(),
            action: action.to_string(),
            detail: None,
        }
    }

    #[test]
    fn trims_oldest_lines_and_keeps_json_valid() {
        let base = tmp_base("trim");
        // 每行大约 100 字节；上限 1 KB 只留最近几条。
        for i in 0..100 {
            let last = format!("动作 {i}");
            append_in(&base, "abc", &entry(&last), 1024).expect("append");
        }
        let raw = fs::read_to_string(base.join("abc.jsonl")).expect("read");
        assert!(raw.len() <= 1024, "文件应被裁到上限内，实际 {}", raw.len());
        for line in raw.lines() {
            serde_json::from_str::<ActivityEntry>(line).expect("每行仍是合法 JSON");
        }
        // 最新的那条必须还在，最旧的那批应该被丢。
        assert!(raw.contains("动作 99"));
        assert!(!raw.contains("\"动作 0\""));
        let list = list_in(&base, "abc", MAX_LIST).expect("list");
        assert_eq!(list[0].action, "动作 99", "list 最新的在前");
    }

    #[test]
    fn list_skips_broken_lines() {
        let base = tmp_base("broken");
        append_in(&base, "abc", &entry("第一条"), 1 << 20).expect("append");
        let file = base.join("abc.jsonl");
        let mut raw = fs::read_to_string(&file).expect("read");
        raw.push_str("{ 这不是 json\n");
        fs::write(&file, raw).expect("write");
        append_in(&base, "abc", &entry("第三条"), 1 << 20).expect("append");
        let list = list_in(&base, "abc", MAX_LIST).expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].action, "第三条");
        assert_eq!(list[1].action, "第一条");
    }

    #[test]
    fn rejects_traversal_ids() {
        let base = tmp_base("bad-id");
        assert!(append_in(&base, "../x", &entry("a"), 1024).is_err());
        assert!(list_in(&base, "a/b", MAX_LIST).is_err());
        assert!(append_in(&base, "ok-1_2.3", &entry("a"), 1024).is_ok());
    }
}
