//! SQLite 持久化层。
//!
//! 只放“结构化、会增长、需要局部更新/查询”的数据：集群任务与日志、同步历史、
//! 操作日志、快照/代次索引、哈希缓存。设置、实例配置、成员身份和大文件仍留在
//! JSON/文件系统里，避免把可读、可拷贝的配置变成不透明数据库。
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::cluster::{ClusterConfig, Member};
use crate::error::AppError;

/// 生产库固定在数据目录下；测试可以直接 `Db::open` 临时路径，不碰全局单例。
pub fn default_path() -> PathBuf {
    crate::config::app_root().join("comfyfleet.db")
}

static GLOBAL: OnceLock<Result<Db, String>> = OnceLock::new();

/// 进程级只读/写单连接。SQLite 写入很快，桌面端单用户场景不需要连接池；
/// 所有调用都通过短事务串行化，避免 SQLITE_BUSY 和写丢失。
pub fn global() -> Result<&'static Db, AppError> {
    match GLOBAL.get_or_init(|| Db::open(&default_path()).map_err(|e| e.message)) {
        Ok(db) => Ok(db),
        Err(message) => Err(AppError::new(
            "DB_OPEN",
            format!("打开本地数据库失败：{message}"),
        )),
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cluster_meta (
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    port INTEGER NOT NULL,
    allow_lan INTEGER NOT NULL,
    salt TEXT NOT NULL,
    join_key TEXT NOT NULL,
    iterations INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS members (
    id TEXT PRIMARY KEY,
    online INTEGER NOT NULL,
    last_seen TEXT NOT NULL,
    payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    target_member_id TEXT NOT NULL,
    source_instance_id TEXT NOT NULL,
    logs_count INTEGER NOT NULL DEFAULT 0,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS jobs_updated ON jobs(updated_at);
CREATE INDEX IF NOT EXISTS jobs_target ON jobs(target_member_id);

CREATE TABLE IF NOT EXISTS job_logs (
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    line TEXT NOT NULL,
    PRIMARY KEY (job_id, seq)
);

CREATE TABLE IF NOT EXISTS last_syncs (
    source_instance_id TEXT PRIMARY KEY,
    at TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    source_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS last_applied (
    instance_id TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS activity (
    id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL,
    at TEXT NOT NULL,
    size INTEGER NOT NULL,
    entry_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS activity_instance_at ON activity(instance_id, at DESC);

CREATE TABLE IF NOT EXISTS snapshots (
    base_key TEXT NOT NULL,
    id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    snapshot_json TEXT NOT NULL,
    PRIMARY KEY (base_key, id)
);

CREATE TABLE IF NOT EXISTS snapshot_bases (
    base_key TEXT PRIMARY KEY,
    dir_mtime INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS generations (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    source_instance_id TEXT NOT NULL,
    source_instance_name TEXT NOT NULL,
    path TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hash_cache (
    path_key TEXT PRIMARY KEY,
    hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| AppError::new("DB_DIR", format!("创建数据库目录失败：{e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| AppError::new("DB_OPEN", format!("打开数据库失败：{e}")))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        // WAL：读写不互相阻塞，崩溃恢复也比重写整份 JSON 可靠。
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_error)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(db_error)?;
        conn.execute_batch(SCHEMA).map_err(db_error)?;
        conn.pragma_update(None, "user_version", 1)
            .map_err(db_error)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ 集群

    pub fn cluster_exists(&self) -> Result<bool, AppError> {
        let conn = self.lock();
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cluster_meta LIMIT 1)",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        Ok(exists != 0)
    }

    pub fn cluster_load(&self) -> Result<Option<ClusterConfig>, AppError> {
        let conn = self.lock();
        let meta = conn
            .query_row(
                "SELECT id, version, name, port, allow_lan, salt, join_key, iterations, created_at
                 FROM cluster_meta LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as u32,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)? as u16,
                        row.get::<_, i64>(4)? != 0,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)? as u32,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some((id, version, name, port, allow_lan, salt, join_key, iterations, created_at)) =
            meta
        else {
            return Ok(None);
        };

        let mut cfg = ClusterConfig {
            version,
            id,
            name,
            port,
            allow_lan,
            salt,
            join_key,
            iterations,
            created_at,
            members: Vec::new(),
            jobs: Vec::new(),
            last_syncs: HashMap::new(),
        };

        {
            let mut stmt = conn
                .prepare("SELECT payload FROM members ORDER BY rowid")
                .map_err(db_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db_error)?;
            for row in rows {
                let raw = row.map_err(db_error)?;
                let mut member: Member = serde_json::from_str(&raw).map_err(json_error)?;
                // 和旧 JSON 加载保持一致：重启后先视为离线，等心跳。
                member.online = false;
                cfg.members.push(member);
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT id, payload, logs_count FROM jobs ORDER BY rowid")
                .map_err(db_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(db_error)?;
            for row in rows {
                let (id, raw, _logs_count) = row.map_err(db_error)?;
                let mut job: crate::sync::SyncJob =
                    serde_json::from_str(&raw).map_err(json_error)?;
                let mut logs_stmt = conn
                    .prepare("SELECT line FROM job_logs WHERE job_id=? ORDER BY seq")
                    .map_err(db_error)?;
                let logs = logs_stmt
                    .query_map([&id], |row| row.get::<_, String>(0))
                    .map_err(db_error)?;
                job.logs = logs.collect::<Result<Vec<_>, _>>().map_err(db_error)?;
                cfg.jobs.push(job);
            }
        }

        {
            let mut stmt = conn
                .prepare(
                    "SELECT source_instance_id, at, generation_id, source_json FROM last_syncs",
                )
                .map_err(db_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(db_error)?;
            for row in rows {
                let (source_id, at, generation_id, source_json) = row.map_err(db_error)?;
                let source = serde_json::from_str(&source_json).map_err(json_error)?;
                cfg.last_syncs.insert(
                    source_id,
                    crate::cluster::LastSync {
                        at,
                        generation_id,
                        source,
                    },
                );
            }
        }

        Ok(Some(cfg))
    }

    pub fn cluster_save(&self, cfg: &ClusterConfig) -> Result<(), AppError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;

        tx.execute("DELETE FROM cluster_meta", [])
            .map_err(db_error)?;
        tx.execute(
            "INSERT INTO cluster_meta
             (id, version, name, port, allow_lan, salt, join_key, iterations, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cfg.id,
                cfg.version as i64,
                cfg.name,
                cfg.port as i64,
                cfg.allow_lan as i64,
                cfg.salt,
                cfg.join_key,
                cfg.iterations as i64,
                cfg.created_at
            ],
        )
        .map_err(db_error)?;

        let current_members: HashSet<&str> = cfg.members.iter().map(|m| m.id.as_str()).collect();
        delete_missing(&tx, "members", "id", &current_members)?;
        for member in &cfg.members {
            let payload = serde_json::to_string(member).map_err(json_error)?;
            tx.execute(
                "INSERT INTO members (id, online, last_seen, payload) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   online=excluded.online,
                   last_seen=excluded.last_seen,
                   payload=excluded.payload",
                params![member.id, member.online as i64, member.last_seen, payload],
            )
            .map_err(db_error)?;
        }

        let current_jobs: HashSet<&str> = cfg.jobs.iter().map(|j| j.id.as_str()).collect();
        delete_missing(&tx, "jobs", "id", &current_jobs)?;
        for job in &cfg.jobs {
            let old_logs: i64 = tx
                .query_row("SELECT logs_count FROM jobs WHERE id=?", [&job.id], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(db_error)?
                .unwrap_or(0);
            let logs = &job.logs;
            let logs_count = logs.len() as i64;
            let old_count = old_logs.max(0) as usize;
            let mut start = old_count;
            let mut reset = logs_count < old_logs;
            let mut replace_last: Option<(i64, String)> = None;
            if !reset && old_logs > 0 {
                // push_job_log 到 600 条会从头丢；只比条数会漏掉新日志。
                // 用首行判断窗口是否移动，用末行处理“临时进度替换最后一条”。
                let old_first: Option<String> = tx
                    .query_row(
                        "SELECT line FROM job_logs WHERE job_id=? AND seq=0",
                        [&job.id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db_error)?;
                if old_first.as_deref() != logs.first().map(String::as_str) {
                    reset = true;
                } else if logs_count == old_logs {
                    let last_seq = old_logs - 1;
                    let old_last: Option<String> = tx
                        .query_row(
                            "SELECT line FROM job_logs WHERE job_id=? AND seq=?",
                            params![job.id, last_seq],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(db_error)?;
                    match (old_last, logs.last()) {
                        (Some(old_line), Some(new_line)) if old_line != *new_line => {
                            replace_last = Some((last_seq, new_line.clone()));
                        }
                        (None, Some(_)) => reset = true,
                        _ => {}
                    }
                }
            }
            if reset {
                tx.execute("DELETE FROM job_logs WHERE job_id=?", [&job.id])
                    .map_err(db_error)?;
                start = 0;
            }
            let mut persisted = job.clone();
            persisted.logs.clear();
            let payload = serde_json::to_string(&persisted).map_err(json_error)?;
            tx.execute(
                "INSERT INTO jobs
                 (id, status, updated_at, target_member_id, source_instance_id, logs_count, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   status=excluded.status,
                   updated_at=excluded.updated_at,
                   target_member_id=excluded.target_member_id,
                   source_instance_id=excluded.source_instance_id,
                   payload=excluded.payload",
                params![
                    job.id,
                    job.status,
                    job.updated_at,
                    job.target_member_id,
                    job.source_instance_id,
                    logs_count,
                    payload
                ],
            )
            .map_err(db_error)?;
            if let Some((seq, line)) = replace_last {
                tx.execute(
                    "UPDATE job_logs SET line=? WHERE job_id=? AND seq=?",
                    params![line, job.id, seq],
                )
                .map_err(db_error)?;
            }
            for (seq, line) in logs.iter().enumerate().skip(start) {
                tx.execute(
                    "INSERT OR REPLACE INTO job_logs (job_id, seq, line) VALUES (?1, ?2, ?3)",
                    params![job.id, seq as i64, line],
                )
                .map_err(db_error)?;
            }
            tx.execute(
                "UPDATE jobs SET logs_count=? WHERE id=?",
                params![logs_count, job.id],
            )
            .map_err(db_error)?;
        }

        let current_syncs: HashSet<&str> = cfg.last_syncs.keys().map(|s| s.as_str()).collect();
        delete_missing(&tx, "last_syncs", "source_instance_id", &current_syncs)?;
        for (source_id, last) in &cfg.last_syncs {
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT at, generation_id FROM last_syncs WHERE source_instance_id=?",
                    [source_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db_error)?;
            if let Some((at, generation_id)) = existing {
                // 任务进度更新时 last_syncs 大多数不变；不重复序列化完整模型清单。
                if at == last.at && generation_id == last.generation_id {
                    continue;
                }
            }
            let source_json = serde_json::to_string(&last.source).map_err(json_error)?;
            tx.execute(
                "INSERT INTO last_syncs (source_instance_id, at, generation_id, source_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_instance_id) DO UPDATE SET
                   at=excluded.at,
                   generation_id=excluded.generation_id,
                   source_json=excluded.source_json",
                params![source_id, last.at, last.generation_id, source_json],
            )
            .map_err(db_error)?;
        }

        tx.commit().map_err(db_error)
    }

    pub fn cluster_delete(&self) -> Result<(), AppError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        tx.execute("DELETE FROM job_logs", []).map_err(db_error)?;
        tx.execute("DELETE FROM jobs", []).map_err(db_error)?;
        tx.execute("DELETE FROM last_syncs", []).map_err(db_error)?;
        tx.execute("DELETE FROM members", []).map_err(db_error)?;
        tx.execute("DELETE FROM cluster_meta", [])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    // ------------------------------------------------------------ 目标端上次应用

    pub fn last_applied_get(&self, instance_id: &str) -> Result<Option<String>, AppError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value_json FROM last_applied WHERE instance_id=?",
            [instance_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(db_error)
    }

    pub fn last_applied_put(&self, instance_id: &str, value_json: &str) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO last_applied (instance_id, value_json) VALUES (?1, ?2)
             ON CONFLICT(instance_id) DO UPDATE SET value_json=excluded.value_json",
            params![instance_id, value_json],
        )
        .map_err(db_error)?;
        Ok(())
    }

    // ------------------------------------------------------------ 操作日志

    pub fn activity_append(
        &self,
        instance_id: &str,
        id: &str,
        at: &str,
        entry_json: &str,
        cap: u64,
    ) -> Result<(), AppError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        let size = entry_json.len() as i64 + 1;
        tx.execute(
            "INSERT OR REPLACE INTO activity (id, instance_id, at, size, entry_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, instance_id, at, size, entry_json],
        )
        .map_err(db_error)?;
        prune_activity(&tx, instance_id, cap)?;
        tx.commit().map_err(db_error)
    }

    pub fn activity_list(&self, instance_id: &str, limit: usize) -> Result<Vec<String>, AppError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT entry_json FROM activity
                 WHERE instance_id=?
                 ORDER BY at DESC, id DESC
                 LIMIT ?",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![instance_id, limit.max(1) as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(db_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_error)?);
        }
        Ok(out)
    }

    pub fn activity_import_once(&self, dir: &Path, cap: u64) -> Result<(), AppError> {
        {
            let conn = self.lock();
            if meta_get(&conn, "activity_imported")?.as_deref() == Some("1") {
                return Ok(());
            }
        }
        let mut rows: Vec<(String, String, String, String)> = Vec::new();
        let mut complete = true;
        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries {
                    let Ok(entry) = entry else {
                        complete = false;
                        continue;
                    };
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(raw) = fs::read_to_string(&path) else {
                        complete = false;
                        continue;
                    };
                    for (index, line) in raw.lines().enumerate() {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        let Some(instance_id) = path.file_stem().and_then(|s| s.to_str()) else {
                            continue;
                        };
                        let id = value
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("{instance_id}-migrated-{index}"));
                        let at = value
                            .get("at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        rows.push((instance_id.to_string(), id, at, line.to_string()));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                log::warn!("枚举旧操作日志失败：{e}");
                complete = false;
            }
        }
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        let mut instances: HashSet<String> = HashSet::new();
        for (instance_id, id, at, entry_json) in rows {
            let size = entry_json.len() as i64 + 1;
            tx.execute(
                "INSERT OR IGNORE INTO activity (id, instance_id, at, size, entry_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, instance_id, at, size, entry_json],
            )
            .map_err(db_error)?;
            instances.insert(instance_id);
        }
        for instance_id in &instances {
            prune_activity(&tx, instance_id, cap)?;
        }
        if complete {
            meta_set(&tx, "activity_imported", "1")?;
        }
        tx.commit().map_err(db_error)?;
        if !complete {
            return Err(AppError::new(
                "ACTIVITY_IMPORT",
                "旧操作日志未完整导入，下次重试",
            ));
        }
        Ok(())
    }

    /// 文件兜底写入成功后把导入标记清掉，下一次数据库恢复时会重新吸收 JSONL。
    pub fn activity_mark_unimported(&self) -> Result<(), AppError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        meta_set(&tx, "activity_imported", "0")?;
        tx.commit().map_err(db_error)
    }

    // ------------------------------------------------------------ 快照索引

    pub fn snapshot_upsert(
        &self,
        base: &Path,
        id: &str,
        created_at: &str,
        snapshot_json: &str,
    ) -> Result<(), AppError> {
        let key = path_key(base);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO snapshots (base_key, id, created_at, snapshot_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(base_key, id) DO UPDATE SET
               created_at=excluded.created_at,
               snapshot_json=excluded.snapshot_json",
            params![key, id, created_at, snapshot_json],
        )
        .map_err(db_error)?;
        Ok(())
    }

    pub fn snapshot_list(&self, base: &Path) -> Result<Vec<String>, AppError> {
        let key = path_key(base);
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT snapshot_json FROM snapshots
                 WHERE base_key=?
                 ORDER BY created_at DESC, id DESC",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map([key], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_error)?);
        }
        Ok(out)
    }

    pub fn snapshot_delete(&self, base: &Path, id: &str) -> Result<(), AppError> {
        let key = path_key(base);
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        tx.execute(
            "DELETE FROM snapshots WHERE base_key=? AND id=?",
            params![key, id],
        )
        .map_err(db_error)?;
        // 目录内容变了；下次 list_in 重新扫描并补回其余文件。
        tx.execute("DELETE FROM snapshot_bases WHERE base_key=?", [&key])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub fn snapshot_base_mtime(&self, base: &Path) -> Result<Option<i64>, AppError> {
        let key = path_key(base);
        let conn = self.lock();
        conn.query_row(
            "SELECT dir_mtime FROM snapshot_bases WHERE base_key=?",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)
    }

    pub fn snapshot_set_base_mtime(&self, base: &Path, mtime: i64) -> Result<(), AppError> {
        let key = path_key(base);
        let conn = self.lock();
        conn.execute(
            "INSERT INTO snapshot_bases (base_key, dir_mtime) VALUES (?1, ?2)
             ON CONFLICT(base_key) DO UPDATE SET dir_mtime=excluded.dir_mtime",
            params![key, mtime],
        )
        .map_err(db_error)?;
        Ok(())
    }

    pub fn snapshot_invalidate_base(&self, base: &Path) -> Result<(), AppError> {
        let key = path_key(base);
        let conn = self.lock();
        conn.execute("DELETE FROM snapshot_bases WHERE base_key=?", [&key])
            .map_err(db_error)?;
        Ok(())
    }

    /// 文件被外部删除时，索引也要跟着掉，否则下一次列表会“复活”旧快照。
    pub fn snapshot_prune(&self, base: &Path, keep: &[String]) -> Result<(), AppError> {
        let key = path_key(base);
        let mut conn = self.lock();
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM snapshots WHERE base_key=?")
                .map_err(db_error)?;
            let rows = stmt
                .query_map([&key], |row| row.get::<_, String>(0))
                .map_err(db_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(db_error)?
        };
        let keep: HashSet<&str> = keep.iter().map(String::as_str).collect();
        let tx = conn.transaction().map_err(db_error)?;
        for id in ids {
            if !keep.contains(id.as_str()) {
                tx.execute(
                    "DELETE FROM snapshots WHERE base_key=? AND id=?",
                    params![key, id],
                )
                .map_err(db_error)?;
            }
        }
        tx.commit().map_err(db_error)
    }

    // ------------------------------------------------------------ 同步代次索引

    pub fn generation_upsert(
        &self,
        id: &str,
        created_at: &str,
        source_instance_id: &str,
        source_instance_name: &str,
        path: &Path,
    ) -> Result<(), AppError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO generations
             (id, created_at, source_instance_id, source_instance_name, path)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
               created_at=excluded.created_at,
               source_instance_id=excluded.source_instance_id,
               source_instance_name=excluded.source_instance_name,
               path=excluded.path",
            params![
                id,
                created_at,
                source_instance_id,
                source_instance_name,
                path.to_string_lossy().to_string()
            ],
        )
        .map_err(db_error)?;
        Ok(())
    }

    pub fn generation_prune_missing(&self) -> Result<usize, AppError> {
        let mut conn = self.lock();
        let rows: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, path FROM generations")
                .map_err(db_error)?;
            let mapped = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(db_error)?;
            mapped.collect::<Result<Vec<_>, _>>().map_err(db_error)?
        };
        let missing: Vec<String> = rows
            .into_iter()
            .filter_map(|(id, path)| (!Path::new(&path).is_dir()).then_some(id))
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let tx = conn.transaction().map_err(db_error)?;
        for id in &missing {
            tx.execute("DELETE FROM generations WHERE id=?", [id])
                .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(missing.len())
    }

    // ------------------------------------------------------------ 哈希缓存

    pub fn hash_cache_load(&self) -> Result<HashMap<String, String>, AppError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT path_key, hash FROM hash_cache")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_error)?;
        let mut out = HashMap::new();
        for row in rows {
            let (key, hash) = row.map_err(db_error)?;
            out.insert(key, hash);
        }
        Ok(out)
    }

    pub fn hash_cache_put_many(&self, entries: &[(String, String)]) -> Result<(), AppError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_error)?;
        for (key, hash) in entries {
            tx.execute(
                "INSERT INTO hash_cache (path_key, hash) VALUES (?1, ?2)
                 ON CONFLICT(path_key) DO UPDATE SET hash=excluded.hash",
                params![key, hash],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    /// 返回 (条目数, 近似字节数)，给缓存面板用，避免继续统计旧 JSON。
    pub fn hash_cache_stats(&self) -> Result<(u64, u64), AppError> {
        let conn = self.lock();
        let (count, bytes): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(path_key) + length(hash)), 0) FROM hash_cache",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(db_error)?;
        Ok((count.max(0) as u64, bytes.max(0) as u64))
    }
}

fn db_error(error: rusqlite::Error) -> AppError {
    AppError::new("DB", format!("数据库操作失败：{error}"))
}

fn json_error(error: serde_json::Error) -> AppError {
    AppError::new("DB_JSON", format!("数据库记录解析失败：{error}"))
}

/// 删除表里已经不在当前配置中的行。占位符按实际 id 拼，id 只来自本地配置。
fn delete_missing(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    id_column: &str,
    current: &HashSet<&str>,
) -> Result<(), AppError> {
    let mut old: Vec<String> = {
        let mut stmt = tx
            .prepare(&format!("SELECT {id_column} FROM {table}"))
            .map_err(db_error)?;
        let mapped = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        mapped.collect::<Result<Vec<_>, _>>().map_err(db_error)?
    };
    for id in old.drain(..) {
        if !current.contains(id.as_str()) {
            tx.execute(&format!("DELETE FROM {table} WHERE {id_column}=?"), [&id])
                .map_err(db_error)?;
        }
    }
    Ok(())
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, AppError> {
    conn.query_row("SELECT value FROM meta WHERE key=?", [key], |row| {
        row.get(0)
    })
    .optional()
    .map_err(db_error)
}

fn meta_set(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )
    .map_err(db_error)?;
    Ok(())
}

fn prune_activity(
    tx: &rusqlite::Transaction<'_>,
    instance_id: &str,
    cap: u64,
) -> Result<(), AppError> {
    let total: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(size), 0) FROM activity WHERE instance_id=?",
            [instance_id],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if total <= cap as i64 {
        return Ok(());
    }
    let mut need = total - cap as i64;
    let mut drop: Vec<String> = Vec::new();
    {
        let mut stmt = tx
            .prepare(
                "SELECT id, size FROM activity
                 WHERE instance_id=?
                 ORDER BY at ASC, id ASC",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map([instance_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(db_error)?;
        for row in rows {
            let (id, size) = row.map_err(db_error)?;
            drop.push(id);
            need -= size;
            if need <= 0 {
                break;
            }
        }
    }
    for id in drop {
        tx.execute("DELETE FROM activity WHERE id=?", [id])
            .map_err(db_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ClusterConfig, LastSync, Member};
    use crate::sync::SyncJob;

    fn temp_db() -> (PathBuf, Db) {
        let dir = std::env::temp_dir().join(format!("comfyfleet-db-{}", uuid::Uuid::new_v4()));
        let path = dir.join("comfyfleet.db");
        let db = Db::open(&path).expect("open temp db");
        (dir, db)
    }

    #[test]
    fn cluster_roundtrip_replaces_jobs_and_appends_logs() {
        let (dir, db) = temp_db();
        let mut cfg = ClusterConfig {
            version: 1,
            id: "cluster-1".to_string(),
            name: "测试集群".to_string(),
            port: 17890,
            allow_lan: false,
            salt: "salt".to_string(),
            join_key: "key".to_string(),
            iterations: 1000,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };
        cfg.members.push(Member {
            id: "m1".to_string(),
            name: "成员一".to_string(),
            last_seen: "2026-01-01T00:00:00Z".to_string(),
            online: true,
            ..Default::default()
        });
        let mut job = SyncJob {
            id: "j1".to_string(),
            generation_id: "g1".to_string(),
            source_instance_id: "i1".to_string(),
            target_member_id: "m1".to_string(),
            status: "running".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };
        job.logs.push("第一行".to_string());
        cfg.jobs.push(job);
        cfg.last_syncs.insert(
            "i1".to_string(),
            LastSync {
                at: "2026-01-01T00:00:00Z".to_string(),
                generation_id: "g1".to_string(),
                ..Default::default()
            },
        );

        db.cluster_save(&cfg).expect("save cluster");
        assert!(db.cluster_exists().unwrap());
        let mut loaded = db.cluster_load().unwrap().expect("cluster");
        assert_eq!(loaded.members.len(), 1);
        assert!(!loaded.members[0].online, "重启后成员先离线");
        assert_eq!(loaded.jobs.len(), 1);
        assert_eq!(loaded.jobs[0].logs, vec!["第一行".to_string()]);
        assert_eq!(loaded.last_syncs.get("i1").unwrap().generation_id, "g1");

        loaded.jobs[0].logs.push("第二行".to_string());
        db.cluster_save(&loaded).expect("append log");
        let loaded = db.cluster_load().unwrap().expect("cluster");
        assert_eq!(loaded.jobs[0].logs.len(), 2);

        // push_job_log 到达 600 条会从头丢；落库必须识别窗口移动，而不是只比条数。
        let mut capped = loaded.clone();
        capped.jobs[0].logs.clear();
        for i in 0..601 {
            capped.jobs[0].logs.push(format!("line{i}"));
            if capped.jobs[0].logs.len() > 600 {
                capped.jobs[0].logs.remove(0);
            }
        }
        db.cluster_save(&capped).expect("save capped logs");
        let loaded = db.cluster_load().unwrap().expect("cluster");
        assert_eq!(loaded.jobs[0].logs.len(), 600);
        assert_eq!(loaded.jobs[0].logs.first().unwrap(), "line1");
        assert_eq!(loaded.jobs[0].logs.last().unwrap(), "line600");

        // 临时进度会原地替换最后一条；条数不变也要更新。
        let mut transient = loaded.clone();
        *transient.jobs[0].logs.last_mut().unwrap() = "progress".to_string();
        db.cluster_save(&transient).expect("replace last log");
        let loaded = db.cluster_load().unwrap().expect("cluster");
        assert_eq!(loaded.jobs[0].logs.last().unwrap(), "progress");

        let mut rolling = loaded.clone();
        rolling.jobs[0].logs.push("line601".to_string());
        if rolling.jobs[0].logs.len() > 600 {
            rolling.jobs[0].logs.remove(0);
        }
        db.cluster_save(&rolling).expect("roll logs");
        let loaded = db.cluster_load().unwrap().expect("cluster");
        assert_eq!(loaded.jobs[0].logs.first().unwrap(), "line2");
        assert_eq!(loaded.jobs[0].logs.last().unwrap(), "line601");

        let mut empty = loaded.clone();
        empty.jobs.clear();
        db.cluster_save(&empty).expect("remove jobs");
        let loaded = db.cluster_load().unwrap().expect("cluster");
        assert!(loaded.jobs.is_empty());

        db.cluster_delete().expect("delete cluster");
        assert!(!db.cluster_exists().unwrap());
        assert!(db.cluster_load().unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_applied_roundtrip() {
        let (dir, db) = temp_db();
        assert!(db.last_applied_get("i1").unwrap().is_none());
        db.last_applied_put("i1", "{\"a\":1}").unwrap();
        assert_eq!(
            db.last_applied_get("i1").unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        db.last_applied_put("i1", "{\"a\":2}").unwrap();
        assert_eq!(
            db.last_applied_get("i1").unwrap().as_deref(),
            Some("{\"a\":2}")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn activity_imports_once_and_keeps_newest_rows() {
        let (dir, db) = temp_db();
        let activity_dir = dir.join("activity");
        fs::create_dir_all(&activity_dir).unwrap();
        fs::write(
            activity_dir.join("i1.jsonl"),
            "{\"id\":\"e1\",\"at\":\"2026-01-01T00:00:00Z\",\"kind\":\"x\",\"level\":\"info\",\"action\":\"a\",\"detail\":\"old\"}\n",
        )
        .unwrap();
        db.activity_import_once(&activity_dir, 200).unwrap();
        assert_eq!(db.activity_list("i1", 10).unwrap().len(), 1);
        // 第二次导入不应重复。
        db.activity_import_once(&activity_dir, 200).unwrap();
        assert_eq!(db.activity_list("i1", 10).unwrap().len(), 1);

        let body = |n: usize| {
            format!(
                "{{\"id\":\"e{n}\",\"at\":\"2026-01-0{n}T00:00:00Z\",\"kind\":\"x\",\"level\":\"info\",\"action\":\"a\",\"detail\":\"{}\"}}",
                "x".repeat(80)
            )
        };
        db.activity_append("i1", "e2", "2026-01-02T00:00:00Z", &body(2), 200)
            .unwrap();
        db.activity_append("i1", "e3", "2026-01-03T00:00:00Z", &body(3), 200)
            .unwrap();
        let rows = db.activity_list("i1", 10).unwrap();
        assert!(rows[0].contains("\"e3\""));
        assert!(rows.iter().all(|row| !row.contains("\"e1\"")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_and_generation_indexes() {
        let (dir, db) = temp_db();
        let base = dir.join("snapshots");
        db.snapshot_upsert(&base, "s1", "2026-01-01T00:00:00Z", "{\"id\":\"s1\"}")
            .unwrap();
        db.snapshot_upsert(&base, "s2", "2026-01-02T00:00:00Z", "{\"id\":\"s2\"}")
            .unwrap();
        let rows = db.snapshot_list(&base).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\"s2\""));
        db.snapshot_prune(&base, &["s2".to_string()]).unwrap();
        assert_eq!(db.snapshot_list(&base).unwrap().len(), 1);
        db.snapshot_set_base_mtime(&base, 123).unwrap();
        assert_eq!(db.snapshot_base_mtime(&base).unwrap(), Some(123));
        db.snapshot_delete(&base, "s2").unwrap();
        assert_eq!(db.snapshot_list(&base).unwrap().len(), 0);
        assert_eq!(db.snapshot_base_mtime(&base).unwrap(), None);

        let live = dir.join("g1");
        fs::create_dir_all(&live).unwrap();
        db.generation_upsert("g1", "2026-01-01", "i1", "实例一", &live)
            .unwrap();
        db.generation_upsert("g2", "2026-01-02", "i1", "实例一", &dir.join("gone"))
            .unwrap();
        assert_eq!(db.generation_prune_missing().unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_cache_roundtrip_and_overwrite() {
        let (dir, db) = temp_db();
        let entries = vec![("a".to_string(), "1".to_string())];
        db.hash_cache_put_many(&entries).unwrap();
        assert_eq!(
            db.hash_cache_load().unwrap().get("a").map(String::as_str),
            Some("1")
        );
        db.hash_cache_put_many(&[("a".to_string(), "2".to_string())])
            .unwrap();
        assert_eq!(
            db.hash_cache_load().unwrap().get("a").map(String::as_str),
            Some("2")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
