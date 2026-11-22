//! Python 侧进度事件：解析、缓存和快照。
//!
//! Python 桥把 tqdm 的左右两段文字和数值写成一行 JSON 发给父进程：
//!
//! `\x1eCFP {"op":"update",...}`
//!
//! 这里只负责协议和状态，不依赖 Tauri；事件通过 `ProcHandle` 的 sink 推给命令层。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Python 桥写进度行时使用的固定前缀，和 `comfyfleet_progress.py` 保持一致。
pub const PROGRESS_PREFIX: &str = "\u{1e}CFP ";

/// 一个结构化进度条。`seq` 只用于同实例内的排序，不发给前端。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonProgress {
    pub id: String,
    pub name: String,
    pub indeterminate: bool,
    pub value: f64,
    pub max: f64,
    pub left: String,
    pub right: String,
    pub speed: Option<String>,
    #[serde(skip)]
    pub seq: u64,
}

impl PythonProgress {
    fn new(id: String) -> Self {
        Self {
            id,
            name: String::new(),
            indeterminate: false,
            value: 0.0,
            max: 0.0,
            left: String::new(),
            right: String::new(),
            speed: None,
            seq: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSnapshot {
    pub rev: u64,
    pub progress: Vec<PythonProgress>,
}

/// Python 桥发来的消息。
#[derive(Debug, Deserialize)]
#[serde(tag = "op")]
pub enum ProgressMessage {
    #[serde(rename = "update")]
    Update {
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        indeterminate: bool,
        #[serde(default)]
        value: f64,
        #[serde(default)]
        max: f64,
        #[serde(default)]
        left: Option<String>,
        #[serde(default)]
        right: Option<String>,
        #[serde(default)]
        speed: Option<String>,
    },
    #[serde(rename = "remove")]
    Remove { id: String },
}

pub fn parse_message(line: &str) -> Option<ProgressMessage> {
    serde_json::from_str(line).ok()
}

/// 单实例的进度条集合。进程退出时整个 state 跟着 `ProcHandle` 一起释放。
#[derive(Debug, Default)]
pub struct ProgressState {
    rev: u64,
    bars: BTreeMap<String, PythonProgress>,
}

impl ProgressState {
    /// 应用一条消息。没有变化时返回 `None`，避免无意义地刷新前端。
    pub fn apply(&mut self, message: ProgressMessage) -> Option<ProgressSnapshot> {
        match message {
            ProgressMessage::Update {
                id,
                name,
                indeterminate,
                value,
                max,
                left,
                right,
                speed,
            } => {
                self.rev = self.rev.saturating_add(1);
                let seq = self.rev;
                let bar = self
                    .bars
                    .entry(id.clone())
                    .or_insert_with(|| PythonProgress::new(id));
                if let Some(name) = name {
                    bar.name = name;
                }
                bar.indeterminate = indeterminate;
                bar.value = value;
                bar.max = max;
                if let Some(left) = left {
                    bar.left = left;
                }
                if let Some(right) = right {
                    bar.right = right;
                }
                bar.speed = speed;
                bar.seq = seq;
                Some(self.snapshot())
            }
            ProgressMessage::Remove { id } => {
                if self.bars.remove(&id).is_some() {
                    self.rev = self.rev.saturating_add(1);
                    Some(self.snapshot())
                } else {
                    None
                }
            }
        }
    }

    /// 按最近更新时间升序返回，前端取最后一个就是当前最活跃的进度条。
    pub fn snapshot(&self) -> ProgressSnapshot {
        let mut progress: Vec<PythonProgress> = self.bars.values().cloned().collect();
        progress.sort_by_key(|p| p.seq);
        ProgressSnapshot {
            rev: self.rev,
            progress,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_update_and_remove() {
        let update = parse_message(
            r#"{"op":"update","id":"py:1","name":"Sampling","indeterminate":false,"value":12,"max":20,"left":"Sampling: 60%","right":"12/20 [00:08<00:05, 1.50it/s]","speed":"1.50 it/s"}"#,
        )
        .expect("update");
        assert!(matches!(update, ProgressMessage::Update { .. }));

        let remove = parse_message(r#"{"op":"remove","id":"py:1"}"#).expect("remove");
        assert!(matches!(remove, ProgressMessage::Remove { id } if id == "py:1"));
    }

    #[test]
    fn snapshot_is_ordered_by_last_update() {
        let mut state = ProgressState::default();
        let first = state
            .apply(
                parse_message(r#"{"op":"update","id":"a","name":"A","value":1,"max":10}"#).unwrap(),
            )
            .unwrap();
        assert_eq!(first.progress.len(), 1);
        assert_eq!(first.progress[0].id, "a");

        state
            .apply(
                parse_message(r#"{"op":"update","id":"b","name":"B","value":2,"max":10}"#).unwrap(),
            )
            .unwrap();
        state
            .apply(
                parse_message(r#"{"op":"update","id":"a","name":"A","value":3,"max":10}"#).unwrap(),
            )
            .unwrap();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.progress.len(), 2);
        assert_eq!(snapshot.progress[0].id, "b");
        assert_eq!(snapshot.progress[1].id, "a");
        assert_eq!(snapshot.progress[1].value, 3.0);
    }

    #[test]
    fn removing_missing_bar_does_not_bump_revision() {
        let mut state = ProgressState::default();
        assert!(state
            .apply(parse_message(r#"{"op":"remove","id":"missing"}"#).unwrap())
            .is_none());
        assert_eq!(state.snapshot().rev, 0);
    }
}
