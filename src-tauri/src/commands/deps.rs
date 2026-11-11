//! 节点依赖：检测（缺什么）+ 安装（补上）。
//!
//! 动因：节点是用户自己 `git clone` / 拷进 `custom_nodes` 的，启动器原来只在
//! 「它自己装节点」时顺手裁requirements；手工装进来的节点只能靠「诊断」页看到
//! 一句「缺依赖」，没有动作。这里把「看」和「修」接起来。
//!
//! 保护规则，026-10-29 用户拍板）：
//! - torch 全家（`torch/torchvision/torchaudio/triton/nvidia-*/cuda-*` 等），
//!   **环境里没有→裁*：*已经装了 →跳过**，不管文件要求的是升级还是降级，
//!   只把「文件要求什么、现在是什么」告诉用户，由用户自己决定要不要手动装。
//! - `pip/setuptools/wheel/uv` 这些打包工具同样跳过（动了会把环境搞坏）。
//! 安装前自动拍一条快照；实例在跑时拒绝（venv 被占用，装完也不生效）。
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::ipc::Channel;
use tauri::State;

use crate::commands::instance::load_instance;
use crate::commands::version::VersionEvent;
use crate::config::{GlobalSettings, InstanceConfig};
use crate::error::AppError;
use crate::proc;
use crate::runtime;
use crate::snapshot;
use crate::state::AppState;

/// 节点自带的install.py 最多跑这么之
const INSTALL_PY_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------- 解析

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `name==1.0` / `name>=2 ; python_version<"3.12"`
    Package,
    /// `-e .` / `-e git+https://…`
    Editable,
    /// Direct URL links (https:// or git+https://)
    Url,
    /// `-r other.txt` / `-c constraints.txt`
    Include,
    /// `--index-url` / `--find-links` options
    Index,
    /// 其它 pip 指令
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReqLine {
    pub raw: String,
    pub kind: LineKind,
    pub name: Option<String>,
}

/// `name==1.2.3` →`Some("1.2.3")`；只有精确`==` 才返回（范围约束不好判）
pub fn pinned_exact(spec: &str) -> Option<String> {
    let rest = spec.trim().strip_prefix("==")?;
    if rest.contains('*') || rest.contains(',') || rest.contains(' ') {
        return None;
    }
    Some(rest.trim().to_string())
}

/// 裸链接（`https://…`、`git+https://…`）不是包含
pub fn looks_like_url(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("git+")
        || lower.starts_with("ftp://")
        || lower.starts_with("file://")
        // 本地文件：没有空格才算（`name @ https://x/y.whl` 是直接引用，不是裸链接）
        || (!lower.contains(' ') && (lower.ends_with(".whl") || lower.ends_with(".tar.gz")))
}

/// 从一条requirement 里取包名：忽略extras、版本符、环境标记
pub fn package_name(line: &str) -> Option<String> {
    if looks_like_url(line) {
        return None;
    }
    let head = line.split(';').next().unwrap_or(line);
    let head = head.split('@').next().unwrap_or(head);
    let name: String = head
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name.to_lowercase().replace('_', "-"))
    }
}

/// 版本约束部分（`>=1,<2` / `==1.0`）；但 `name @ url` 返回 None
pub fn version_spec(line: &str) -> Option<String> {
    let head = line.split(';').next().unwrap_or(line).trim();
    let idx = head.find(|c: char| matches!(c, '=' | '<' | '>' | '!' | '~'))?;
    let spec = head[idx..].trim();
    if spec.is_empty() || spec.starts_with('@') {
        None
    } else {
        Some(spec.to_string())
    }
}

// ---------------------------------------------------------------- 环境标记
//
// requirements 里常见 `jetson-stats; platform_machine == 'aarch64'` 这种
// PEP 508 环境标记。旧逻辑只取分号前的包名，x64 机器上也会把 jetson-stats
// 判成「未安装」；自动补依赖于是每次启动都装一遍（装不上就永远 pending）。
// 这里按当前 Python / 系统环境判断标记：false → 跳过，unknown → 也跳过，
// 绝不把不适用或看不懂的标记当成缺依赖。

/// 当前解释器 / 操作系统信息，用于判断 PEP 508 marker。
#[derive(Debug, Clone)]
pub struct MarkerEnv {
    values: BTreeMap<String, String>,
}

impl MarkerEnv {
    pub fn current(python_full_version: &str) -> Self {
        let full = python_full_version.trim();
        let full = if full.is_empty() { "0" } else { full };
        let mut values = BTreeMap::new();
        values.insert("python_full_version".into(), full.to_string());
        values.insert("python_version".into(), short_python_version(full));
        values.insert("implementation_version".into(), full.to_string());
        values.insert("implementation_name".into(), "cpython".into());
        values.insert("platform_python_implementation".into(), "CPython".into());
        values.insert("extra".into(), String::new());

        let (sys_platform, platform_system, os_name) = match std::env::consts::OS {
            "windows" => ("win32", "Windows", "nt"),
            "linux" => ("linux", "Linux", "posix"),
            "macos" => ("darwin", "Darwin", "posix"),
            other => (other, other, "posix"),
        };
        values.insert("sys_platform".into(), sys_platform.into());
        values.insert("platform_system".into(), platform_system.into());
        values.insert("os_name".into(), os_name.into());

        let machine = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", "x86_64") => "AMD64",
            ("windows", "aarch64") => "ARM64",
            ("linux", "x86_64") => "x86_64",
            ("linux", "aarch64") => "aarch64",
            ("macos", "aarch64") => "arm64",
            ("macos", "x86_64") => "x86_64",
            (_, arch) => arch,
        };
        values.insert("platform_machine".into(), machine.into());
        Self { values }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
    }
}

fn short_python_version(full: &str) -> String {
    let mut parts = Vec::new();
    for part in full.split('.') {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            break;
        }
        parts.push(digits);
        if parts.len() == 2 {
            break;
        }
    }
    if parts.is_empty() {
        "0".into()
    } else {
        parts.join(".")
    }
}

fn environment_marker(line: &str) -> Option<&str> {
    line.split_once(';')
        .map(|(_, marker)| marker.trim())
        .filter(|marker| !marker.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkerToken {
    Word(String),
    Text(String),
    Op(String),
    And,
    Or,
    Not,
    In,
    LParen,
    RParen,
}

fn lex_marker(text: &str) -> Option<Vec<MarkerToken>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        match c {
            '(' => out.push(MarkerToken::LParen),
            ')' => out.push(MarkerToken::RParen),
            '\'' | '"' => {
                let quote = c;
                let mut value = String::new();
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    } else if next == quote {
                        closed = true;
                        break;
                    } else {
                        value.push(next);
                    }
                }
                if !closed {
                    return None;
                }
                out.push(MarkerToken::Text(value));
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '.' => {
                let mut word = String::from(c);
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || next == '_' || next == '.' {
                        word.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(match word.to_ascii_lowercase().as_str() {
                    "and" => MarkerToken::And,
                    "or" => MarkerToken::Or,
                    "not" => MarkerToken::Not,
                    "in" => MarkerToken::In,
                    _ => MarkerToken::Word(word),
                });
            }
            '=' | '!' | '<' | '>' | '~' => {
                let mut op = String::from(c);
                while let Some(&next) = chars.peek() {
                    if matches!(next, '=' | '~') && op.len() < 3 {
                        op.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !matches!(
                    op.as_str(),
                    "==" | "!=" | "<=" | ">=" | "<" | ">" | "~=" | "==="
                ) {
                    return None;
                }
                out.push(MarkerToken::Op(op));
            }
            _ => return None,
        }
    }
    Some(out)
}

#[derive(Debug, Clone)]
struct MarkerValue {
    text: String,
    /// Python 版本类变量：比较时按版本号，不按字符串。
    version: bool,
    /// 变量名（用于 platform_machine 的别名归一）。
    var: Option<String>,
}

fn is_version_var(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "python_version" | "python_full_version" | "implementation_version"
    )
}

struct MarkerParser<'a> {
    tokens: &'a [MarkerToken],
    pos: usize,
    env: &'a MarkerEnv,
}

impl<'a> MarkerParser<'a> {
    fn parse(mut self) -> Option<bool> {
        let value = self.parse_or()?;
        if self.pos == self.tokens.len() {
            Some(value)
        } else {
            None
        }
    }

    fn parse_or(&mut self) -> Option<bool> {
        let mut value = self.parse_and()?;
        while self.eat_or() {
            let rhs = self.parse_and()?;
            value = value || rhs;
        }
        Some(value)
    }

    fn parse_and(&mut self) -> Option<bool> {
        let mut value = self.parse_atom()?;
        while self.eat_and() {
            let rhs = self.parse_atom()?;
            value = value && rhs;
        }
        Some(value)
    }

    fn parse_atom(&mut self) -> Option<bool> {
        if self.eat_lparen() {
            let value = self.parse_or()?;
            return if self.eat_rparen() { Some(value) } else { None };
        }
        let left = self.parse_value()?;
        let op = self.parse_op()?;
        let right = self.parse_value()?;
        compare_marker(&left, &op, &right)
    }

    fn parse_value(&mut self) -> Option<MarkerValue> {
        match self.next()? {
            MarkerToken::Word(name) => Some(MarkerValue {
                text: self.env.get(&name)?.to_string(),
                version: is_version_var(&name),
                var: Some(name.to_ascii_lowercase()),
            }),
            MarkerToken::Text(text) => Some(MarkerValue {
                text,
                version: false,
                var: None,
            }),
            _ => None,
        }
    }

    fn parse_op(&mut self) -> Option<String> {
        match self.next()? {
            MarkerToken::Op(op) => Some(op),
            MarkerToken::In => Some("in".into()),
            MarkerToken::Not => {
                if matches!(self.next(), Some(MarkerToken::In)) {
                    Some("not in".into())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn next(&mut self) -> Option<MarkerToken> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn eat_lparen(&mut self) -> bool {
        if matches!(self.tokens.get(self.pos), Some(MarkerToken::LParen)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_rparen(&mut self) -> bool {
        if matches!(self.tokens.get(self.pos), Some(MarkerToken::RParen)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_and(&mut self) -> bool {
        if matches!(self.tokens.get(self.pos), Some(MarkerToken::And)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_or(&mut self) -> bool {
        if matches!(self.tokens.get(self.pos), Some(MarkerToken::Or)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn marker_applies(marker: &str, env: &MarkerEnv) -> Option<bool> {
    let tokens = lex_marker(marker)?;
    MarkerParser {
        tokens: &tokens,
        pos: 0,
        env,
    }
    .parse()
}

/// 判断一条 requirement 在当前环境下是否适用。
/// 无 marker 返回 `Some(true)`；marker 无法解析时返回 `None`（调用方应跳过）。
/// 供体检等只读扫描复用，避免把 `jetson-stats; platform_machine == 'aarch64'`
/// 这类不适用依赖报成缺失。
pub fn requirement_applies(raw: &str, env: &MarkerEnv) -> Option<bool> {
    match environment_marker(raw) {
        Some(marker) => marker_applies(marker, env),
        None => Some(true),
    }
}

fn machine_key(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x86-64" => "x86_64".into(),
        "aarch64" | "arm64" => "aarch64".into(),
        "i386" | "i686" | "x86" => "x86".into(),
        other => other.into(),
    }
}

fn text_marker_eq(left: &MarkerValue, right: &MarkerValue) -> bool {
    let var = left.var.as_deref().or(right.var.as_deref());
    if var == Some("platform_machine") {
        machine_key(&left.text) == machine_key(&right.text)
    } else {
        left.text.eq_ignore_ascii_case(&right.text)
    }
}

fn marker_contains(left: &MarkerValue, right: &MarkerValue, negate: bool) -> bool {
    let var = left.var.as_deref().or(right.var.as_deref());
    let found = if var == Some("platform_machine") {
        let want = machine_key(&left.text);
        right
            .text
            .split_whitespace()
            .any(|part| machine_key(part) == want)
    } else {
        right
            .text
            .to_ascii_lowercase()
            .contains(&left.text.to_ascii_lowercase())
    };
    if negate {
        !found
    } else {
        found
    }
}

fn parse_version_parts(value: &str) -> Option<Vec<u64>> {
    let base = value.trim().split(['+', '-']).next().unwrap_or(value);
    let mut parts = Vec::new();
    for part in base.split('.') {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            break;
        }
        parts.push(digits.parse::<u64>().ok()?);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn version_cmp(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left = parse_version_parts(left)?;
    let right = parse_version_parts(right)?;
    let len = left.len().max(right.len());
    for i in 0..len {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        match a.cmp(&b) {
            std::cmp::Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

fn version_eq(left: &str, right: &str) -> bool {
    version_cmp(left, right) == Some(std::cmp::Ordering::Equal)
}

fn compatible_release(actual: &str, required: &str) -> Option<bool> {
    parse_version_parts(actual)?;
    let required_parts = parse_version_parts(required)?;
    if version_cmp(actual, required)? == std::cmp::Ordering::Less {
        return Some(false);
    }
    let mut upper = required_parts;
    if upper.len() <= 1 {
        upper[0] += 1;
    } else {
        upper.pop();
        if let Some(last) = upper.last_mut() {
            *last += 1;
        }
    }
    Some(
        version_cmp(
            actual,
            &upper
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join("."),
        )? == std::cmp::Ordering::Less,
    )
}

fn compare_marker(left: &MarkerValue, op: &str, right: &MarkerValue) -> Option<bool> {
    if op == "in" {
        return Some(marker_contains(left, right, false));
    }
    if op == "not in" {
        return Some(marker_contains(left, right, true));
    }
    if op == "===" {
        return Some(left.text == right.text);
    }
    if op == "==" || op == "!=" {
        let equal = if left.version || right.version {
            version_eq(&left.text, &right.text)
        } else {
            text_marker_eq(left, right)
        };
        return Some(if op == "==" { equal } else { !equal });
    }
    if op == "~=" {
        return compatible_release(&left.text, &right.text);
    }
    let ordering = if left.version || right.version {
        version_cmp(&left.text, &right.text)?
    } else {
        left.text
            .to_ascii_lowercase()
            .cmp(&right.text.to_ascii_lowercase())
    };
    Some(match op {
        "<" => ordering == std::cmp::Ordering::Less,
        "<=" => ordering != std::cmp::Ordering::Greater,
        ">" => ordering == std::cmp::Ordering::Greater,
        ">=" => ordering != std::cmp::Ordering::Less,
        _ => return None,
    })
}

/// 一行requirements →分类结果；注释、空行返回None
pub fn classify(raw_line: &str) -> Option<ReqLine> {
    let raw = raw_line.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let lower = raw.to_lowercase();
    let (kind, name) = if lower.starts_with("-e ") || lower.starts_with("--editable") {
        (LineKind::Editable, None)
    } else if lower.starts_with("-r")
        || lower.starts_with("--requirement")
        || lower.starts_with("-c ")
        || lower.starts_with("--constraint")
    {
        (LineKind::Include, None)
    } else if lower.starts_with("--index-url")
        || lower.starts_with("-i ")
        || lower.starts_with("--extra-index-url")
        || lower.starts_with("--find-links")
        || lower.starts_with("-f ")
        || lower.starts_with("--trusted-host")
        || lower.starts_with("--no-index")
        || lower.starts_with("--pre")
        || lower.starts_with("--only-binary")
        || lower.starts_with("--no-binary")
    {
        (LineKind::Index, None)
    } else if lower.starts_with('-') {
        (LineKind::Other, None)
    } else if looks_like_url(raw) {
        (LineKind::Url, None)
    } else {
        // `name @ https://…` 是 PEP 508 直接引用：名字已知（可能是 torch 这类），按包处理
        (LineKind::Package, package_name(raw))
    };
    Some(ReqLine {
        raw: raw.to_string(),
        kind,
        name,
    })
}

pub fn parse_requirements(text: &str) -> Vec<ReqLine> {
    text.lines().filter_map(classify).collect()
}

// ---------------------------------------------------------------- 计划

#[derive(Debug, Clone)]
pub struct PendingLine {
    pub line: String,
    pub reason: String,
}

/// 一个节点的依赖计划
#[derive(Debug, Clone, Default)]
pub struct NodePlan {
    /// 要装/要调整的（安装时会真正执行）
    pub pending: Vec<PendingLine>,
    /// 明确跳过的（torch 已装、文件里的指令行，
    pub skipped: Vec<PendingLine>,
    /// 只有安装时才能执行、无法静态判断的（`-e`、直接链接）
    pub manual: Vec<String>,
}

fn norm_pkg(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

/// Triton 在 Windows / PyTorch 里有多个发行名；requirements 写 `triton`、
/// pip 里装成 `triton-windows` 时不能判成缺依赖。
fn triton_family(name: &str) -> bool {
    matches!(
        norm_pkg(name).as_str(),
        "triton" | "triton-windows" | "pytorch-triton" | "pytorch-triton-windows"
    )
}

/// 按规范化包名查已装版本，并把 Triton 的几种发行名视为同一个包。
pub fn installed_version<'a>(
    installed: &'a BTreeMap<String, String>,
    name: &str,
) -> Option<&'a String> {
    let want = norm_pkg(name);
    if triton_family(&want) {
        installed
            .iter()
            .find(|(key, _)| triton_family(key))
            .map(|(_, version)| version)
    } else {
        installed
            .iter()
            .find(|(key, _)| norm_pkg(key) == want)
            .map(|(_, version)| version)
    }
}

/// 依据「环境里已装了什么」决定每一行装不装。
#[cfg(test)]
pub fn plan_requirements(lines: &[ReqLine], installed: &BTreeMap<String, String>) -> NodePlan {
    plan_requirements_with_env(lines, installed, &MarkerEnv::current(""))
}

pub fn plan_requirements_with_env(
    lines: &[ReqLine],
    installed: &BTreeMap<String, String>,
    marker_env: &MarkerEnv,
) -> NodePlan {
    let mut plan = NodePlan::default();
    for line in lines {
        if matches!(
            line.kind,
            LineKind::Package | LineKind::Editable | LineKind::Url
        ) {
            if let Some(marker) = environment_marker(&line.raw) {
                match marker_applies(marker, marker_env) {
                    Some(true) => {}
                    Some(false) => {
                        plan.skipped.push(PendingLine {
                            line: line.raw.clone(),
                            reason: format!("当前环境不适用（{marker}），跳过"),
                        });
                        continue;
                    }
                    None => {
                        plan.skipped.push(PendingLine {
                            line: line.raw.clone(),
                            reason: format!("无法判断环境标记（{marker}），跳过"),
                        });
                        continue;
                    }
                }
            }
        }
        match line.kind {
            LineKind::Package => {
                let Some(name) = &line.name else {
                    plan.skipped.push(PendingLine {
                        line: line.raw.clone(),
                        reason: "无法识别包名，跳过".into(),
                    });
                    continue;
                };
                let spec = version_spec(&line.raw);
                let protected = snapshot::is_protected_package(name);
                match installed_version(installed, name) {
                    Some(current) if protected => {
                        let want = spec.unwrap_or_else(|| "未指定版本".into());
                        plan.skipped.push(PendingLine {
                            line: line.raw.clone(),
                            reason: format!(
                                "{name} 属于 torch 系；环境已有 {current}，文件要求 {want}；不升级也不降级",
                            ),
                        });
                    }
                    Some(current) => {
                        // 普通包：只有精确 `==` 且版本不同才算「要调整」。
                        if let Some(want) = spec.as_deref().and_then(pinned_exact) {
                            if want != current.as_str() {
                                plan.pending.push(PendingLine {
                                    line: line.raw.clone(),
                                    reason: format!("版本不符：已装 {current}，文件要求 =={want}"),
                                });
                            }
                        }
                    }
                    None if protected => {
                        plan.pending.push(PendingLine {
                            line: line.raw.clone(),
                            reason: "torch 系包，环境里没有，按文件装".into(),
                        });
                    }
                    None => {
                        plan.pending.push(PendingLine {
                            line: line.raw.clone(),
                            reason: "未安装".into(),
                        });
                    }
                }
            }
            LineKind::Editable => plan.manual.push(line.raw.clone()),
            LineKind::Url => plan.manual.push(line.raw.clone()),
            LineKind::Include => plan.skipped.push(PendingLine {
                line: line.raw.clone(),
                reason: "二级 requirements 引用不自动展开（防止环），需要的话手动装".into(),
            }),
            LineKind::Index => plan.skipped.push(PendingLine {
                line: line.raw.clone(),
                reason: "文件里指定的镜像被忽略，统一走启动器设置的PyPI 镜像".into(),
            }),
            LineKind::Other => plan.skipped.push(PendingLine {
                line: line.raw.clone(),
                reason: "不认识的 pip 指令，跳过".into(),
            }),
        }
    }
    plan
}

// ---------------------------------------------------------------- 报告

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepLine {
    pub line: String,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDeps {
    pub name: String,
    pub enabled: bool,
    pub has_requirements: bool,
    pub has_install_py: bool,
    pub has_pyproject: bool,
    pub declared: usize,
    /// 要装 / 要调整的
    pub pending: Vec<DepLine>,
    /// 明确跳过的（含原因：torch 已装、镜像行被忽略…）
    pub skipped: Vec<DepLine>,
    /// `-e` / 直接链接，只有真装的时候才执行
    pub manual: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepsReport {
    /// 没有 venv：只能报告、不能装
    pub no_venv: bool,
    pub python_version: String,
    pub package_count: usize,
    pub checked_at: String,
    /// 有依赖要处理的节点数
    pub pending_nodes: usize,
    pub nodes: Vec<NodeDeps>,
    /// `uv pip check` 的冲突行（空 = 自洽：
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NodeEntry {
    pub name: String,
    pub dir: PathBuf,
    pub enabled: bool,
}

/// 列出 custom_nodes 下所有带 requirements.txt / install.py 的节点
pub fn collect_nodes(root: &Path) -> Vec<NodeEntry> {
    let dir = root.join("ComfyUI").join("custom_nodes");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let raw = entry.file_name().to_string_lossy().to_string();
        if raw.starts_with('.') || raw == "__pycache__" {
            continue;
        }
        let (name, enabled) = match raw.strip_suffix(".disabled") {
            Some(base) => (base.to_string(), false),
            None => (raw, true),
        };
        out.push(NodeEntry {
            name,
            dir: path,
            enabled,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// 纯函数：给定「已装包表」算出整份报告（方便单测，不碰磁盘以外的东西，
pub fn build_report(
    root: &Path,
    installed: &BTreeMap<String, String>,
    python_version: String,
    conflicts: Vec<String>,
) -> DepsReport {
    let mut nodes = Vec::new();
    let mut pending_nodes = 0usize;
    let marker_env = MarkerEnv::current(&python_version);
    for entry in collect_nodes(root) {
        let req = entry.dir.join("requirements.txt");
        let has_requirements = req.exists();
        let has_install_py = entry.dir.join("install.py").exists();
        let has_pyproject = entry.dir.join("pyproject.toml").exists();
        if !has_requirements && !has_install_py {
            continue;
        }
        let lines = std::fs::read_to_string(&req)
            .map(|text| parse_requirements(&text))
            .unwrap_or_default();
        let plan = plan_requirements_with_env(&lines, installed, &marker_env);
        if !plan.pending.is_empty() {
            pending_nodes += 1;
        }
        nodes.push(NodeDeps {
            name: entry.name,
            enabled: entry.enabled,
            has_requirements,
            has_install_py,
            has_pyproject,
            declared: lines.len(),
            pending: plan
                .pending
                .into_iter()
                .map(|p| DepLine {
                    line: p.line,
                    reason: p.reason,
                })
                .collect(),
            skipped: plan
                .skipped
                .into_iter()
                .map(|p| DepLine {
                    line: p.line,
                    reason: p.reason,
                })
                .collect(),
            manual: plan.manual,
        });
    }
    // 有事的排前面，其次是启用的    nodes.sort_by_key(|n| (n.pending.is_empty(), !n.enabled, n.name.to_lowercase()));
    DepsReport {
        no_venv: false,
        python_version,
        package_count: installed.len(),
        checked_at: chrono::Local::now().to_rfc3339(),
        pending_nodes,
        nodes,
        conflicts,
    }
}

// ---------------------------------------------------------------- 环境

fn uv() -> Result<PathBuf, AppError> {
    crate::commands::env::resolve_uv()
}

/// 找不到Python 环境时的说法：已有环境要指到具体路径，别再说「去设置里装环境。
fn no_python_message(inst: &InstanceConfig) -> String {
    if inst.python.is_existing() {
        format!(
            "这个实例用的是已有 Python 环境，但 {} 下没找到 python.exe；到实例设置 → Python 环境里改一下路径",
            inst.python
                .path
                .clone()
                .unwrap_or_else(|| "（路径留空，自动在实例目录里找）".to_string())
        )
    } else {
        "找不到实例的虚拟环境：先在实例设置里装好环境，或把「Python 环境」改成已有环境".to_string()
    }
}

fn uv_env(py: &runtime::PyEnv) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let cache = crate::config::shared_dir().join("uv-cache");
    let _ = std::fs::create_dir_all(&cache);
    env.insert("UV_CACHE_DIR".into(), cache.to_string_lossy().to_string());
    env.insert("PYTHONUTF8".into(), "1".into());
    env.insert("PYTHONUNBUFFERED".into(), "1".into());
    // install.py 里常自己再调 pip；强制进度条，别因为非交互把下载速度吞掉。
    env.insert("PIP_PROGRESS_BAR".into(), "on".into());
    env.insert("PIP_NO_INPUT".into(), "1".into());
    env.insert("PIP_NO_COLOR".into(), "1".into());
    // 便携版/ 自定义解释器不是 venv，别计VIRTUAL_ENV
    if py.venv_style {
        env.insert("VIRTUAL_ENV".into(), py.root.to_string_lossy().to_string());
    }
    let bin = py.bin_dir();
    let path = std::env::var("PATH").unwrap_or_default();
    env.insert("PATH".into(), format!("{};{}", bin.to_string_lossy(), path));
    env
}

/// 装包源：配置的镜像+ 官方 PyPI 兜底（镜像没同步到时自动换官方重试）
fn index_candidates(settings: &GlobalSettings) -> Vec<String> {
    crate::pip::index_candidates(settings.mirrors.pypi.as_deref())
}

pub fn freeze(py: &runtime::PyEnv) -> Result<BTreeMap<String, String>, AppError> {
    let uv = uv()?;
    snapshot::installed_packages_in(&uv, &py.python, None, &uv_env(py))
        .map_err(|e| AppError::new("FREEZE_FAILED", format!("读取已装包列表失败：{e}")))
}

/// 已安装包的 PEP 610 `direct_url.json`。`git+https://...` / `-e .` 这类
/// requirements 没有版本号，不能只靠 `pip freeze` 判断是否已装；这里保留
/// 来源 URL、git 提交和 editable 标记，用于和 manual 行做比对。
#[derive(Debug, Clone, Default)]
struct DirectUrlInfo {
    name: String,
    version: String,
    url: String,
    vcs: String,
    commit: String,
    requested: String,
    editable: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDirectUrl {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    vcs: String,
    #[serde(default)]
    commit: String,
    #[serde(default)]
    requested: String,
    #[serde(default)]
    editable: bool,
}

/// 从当前 Python 环境读取所有带 `direct_url.json` 的发行包。
/// 读不到（老 pip、精简环境、异常）就返回空表，调用方保持原来的保守行为。
fn installed_direct_urls(py: &runtime::PyEnv) -> Vec<DirectUrlInfo> {
    const SCRIPT: &str = r#"
import importlib.metadata as md, json
out = []
for dist in md.distributions():
    try:
        text = dist.read_text('direct_url.json')
        if not text:
            continue
        data = json.loads(text)
    except Exception:
        continue
    vcs = data.get('vcs_info') or {}
    dir_info = data.get('dir_info') or {}
    out.append({
        'name': dist.metadata.get('Name') or '',
        'version': dist.metadata.get('Version') or '',
        'url': data.get('url') or '',
        'vcs': vcs.get('vcs') or '',
        'commit': vcs.get('commit_id') or '',
        'requested': vcs.get('requested_revision') or '',
        'editable': bool(dir_info.get('editable')),
    })
print(json.dumps(out))
"#;
    let Ok(out) = proc::run(&py.python, &["-c", SCRIPT], None, &uv_env(py)) else {
        return Vec::new();
    };
    if !out.ok() {
        return Vec::new();
    }
    let raw: Vec<RawDirectUrl> = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
    raw.into_iter()
        .map(|r| DirectUrlInfo {
            name: r.name,
            version: r.version,
            url: r.url,
            vcs: r.vcs,
            commit: r.commit,
            requested: r.requested,
            editable: r.editable,
        })
        .collect()
}

/// 归一化 git/URL 来源，忽略 `git+`、`#egg=...`、末尾 `.git` 和斜杠差异。
fn normalize_manual_url(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if let Some(rest) = s.strip_prefix("git+") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("hg+") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("svn+") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("bzr+") {
        s = rest.to_string();
    }
    if let Some((base, _)) = s.split_once('#') {
        s = base.to_string();
    }
    s = s.trim_end_matches('/').to_string();
    if let Some(base) = s.strip_suffix(".git") {
        s = base.to_string();
    }
    s = s.trim_end_matches('/').to_string();
    // git@host:owner/repo 和 https://host/owner/repo 视为同一来源。
    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            s = format!("https://{host}/{path}");
        }
    }
    s.to_ascii_lowercase()
}

/// 拆出 `git+https://host/repo.git@ref#egg=name` 的 URL 和 ref。
/// 不把 `ssh://git@host/...` 里的 userinfo `git@` 误当成 revision。
fn split_manual_rev(url: &str) -> (&str, Option<&str>) {
    if let Some(scheme) = url.find("://") {
        let after_scheme = &url[scheme + 3..];
        if let Some(slash) = after_scheme.find('/') {
            let path_start = scheme + 3 + slash;
            if let Some(at) = url.rfind('@') {
                if at > path_start {
                    return (&url[..at], Some(&url[at + 1..]));
                }
            }
        }
    }
    (url, None)
}

fn manual_source(line: &str) -> Option<(String, Option<String>)> {
    let mut spec = line.trim();
    if let Some(rest) = spec.strip_prefix("-e ") {
        spec = rest.trim();
    }
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    if spec.is_empty() {
        return None;
    }
    let spec = spec.split('#').next().unwrap_or(spec).trim();
    let raw = if let Some(rest) = spec.strip_prefix("git+") {
        rest
    } else if let Some(rest) = spec.strip_prefix("hg+") {
        rest
    } else if let Some(rest) = spec.strip_prefix("svn+") {
        rest
    } else if let Some(rest) = spec.strip_prefix("bzr+") {
        rest
    } else {
        spec
    };
    let (url, rev) = split_manual_rev(raw);
    let rev = rev
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    Some((url.to_string(), rev))
}

fn rev_matches(info: &DirectUrlInfo, rev: &str) -> bool {
    if info.requested.eq_ignore_ascii_case(rev) {
        return true;
    }
    if info.commit.is_empty() {
        return false;
    }
    let commit = info.commit.to_ascii_lowercase();
    let rev = rev.to_ascii_lowercase();
    commit == rev || (rev.len() >= 7 && commit.starts_with(&rev))
}

fn file_url_path(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    let decoded = crate::sync::percent_decode(raw);
    // Windows file URL: file:///D:/x -> D:\x；Unix 下保留开头的 /。
    #[cfg(windows)]
    let decoded = decoded.trim_start_matches('/').replace('/', "\\");
    PathBuf::from(decoded).canonicalize().ok()
}

fn manual_installed_reason(info: &DirectUrlInfo) -> String {
    let name = if info.name.is_empty() {
        "该来源".to_string()
    } else {
        info.name.clone()
    };
    let version = if info.version.is_empty() {
        String::new()
    } else {
        format!(" {}", info.version)
    };
    let commit = if info.commit.is_empty() {
        String::new()
    } else {
        format!("（git {}）", &info.commit[..info.commit.len().min(8)])
    };
    format!("已安装：{name}{version}{commit}；跳过重新安装")
}

fn manual_installed(line: &str, node_dir: &Path, direct: &[DirectUrlInfo]) -> Option<String> {
    let editable = line.trim_start().starts_with("-e ");
    let (url, rev) = manual_source(line)?;
    if !url.contains("://") && !url.starts_with("git@") {
        // `-e .` / `-e ../pkg`：和 direct_url 的 file:// 路径比较。
        let path = Path::new(&url);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            node_dir.join(path)
        };
        let canonical = std::fs::canonicalize(&path).ok()?;
        let want = canonical
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        return direct.iter().find_map(|info| {
            if editable && !info.editable {
                return None;
            }
            let got = file_url_path(&info.url)?;
            let got = got
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            (got == want).then(|| manual_installed_reason(info))
        });
    }

    let want = normalize_manual_url(&url);
    let mut candidates = direct.iter().filter(|info| {
        // 非 git 的 direct_url（wheel/本地目录）没有 vcs_info，也允许匹配。
        (info.vcs.is_empty() || info.vcs == "git") && normalize_manual_url(&info.url) == want
    });
    let info = match rev.as_deref() {
        Some(rev) => candidates.find(|info| rev_matches(info, rev))?,
        None => candidates.next()?,
    };
    Some(manual_installed_reason(info))
}

/// 把已经能从 `direct_url.json` 证明装好的 manual 行移到 skipped，
/// 只留下真正没装的 manual 行。这样自动补依赖既不会重复安装，也不会
/// 因为 manual 一律重装而把未锁 commit 的 git 源更新到新 HEAD。
fn mark_installed_manual(report: &mut DepsReport, root: &Path, direct: &[DirectUrlInfo]) {
    let custom = root.join("ComfyUI").join("custom_nodes");
    for node in &mut report.nodes {
        let node_dir = [
            custom.join(&node.name),
            custom.join(format!("{}.disabled", node.name)),
        ]
        .into_iter()
        .find(|p| p.is_dir())
        .unwrap_or_else(|| custom.join(&node.name));
        let mut remaining = Vec::new();
        for line in std::mem::take(&mut node.manual) {
            match manual_installed(&line, &node_dir, direct) {
                Some(reason) => node.skipped.push(DepLine { line, reason }),
                None => remaining.push(line),
            }
        }
        node.manual = remaining;
    }
    // manual 缺失也要计入，否则只有 manual 缺失时「全部补依赖」按钮不出现。
    report.pending_nodes = report
        .nodes
        .iter()
        .filter(|n| !n.pending.is_empty() || !n.manual.is_empty())
        .count();
}

/// torch 全家桶的 constraints：已装的按实测版本钉死；没装 torchcodec 的，
/// 按 torch 版本从对照表补一条，防止节点依赖顺手装个不配套的 torchcodec。
pub(crate) fn torch_constraints(installed: &BTreeMap<String, String>) -> Option<String> {
    let mut text = String::new();
    for name in ["torch", "torchvision", "torchaudio", "torchcodec"] {
        if let Some(version) = installed.get(name) {
            if !version.trim().is_empty() {
                text.push_str(&format!("{name}=={version}\n"));
            }
        }
    }
    if !installed.contains_key("torchcodec") {
        if let Some(pin) = installed
            .get("torch")
            .and_then(|t| crate::compat::stack_for(t))
        {
            text.push_str(&format!("torchcodec=={}\n", pin.torchcodec));
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 把 torch 全家桶约束写成临时文件；返回 None 表示环境里没有可锁的包。
pub(crate) fn write_torch_constraints(
    installed: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>, AppError> {
    match torch_constraints(installed) {
        Some(text) => {
            let path = temp_file("torch-constraints", "txt")?;
            std::fs::write(&path, text)?;
            Ok(Some(path))
        }
        None => Ok(None),
    }
}

fn pip_check(py: &runtime::PyEnv) -> Vec<String> {
    let Ok(uv) = uv() else {
        return Vec::new();
    };
    let Ok(out) = proc::run(
        &uv,
        &["pip", "check", "--python", &py.python.to_string_lossy()],
        None,
        &uv_env(py),
    ) else {
        return Vec::new();
    };
    out.stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        // 不同版本 uv 的「没问题」说法不一样，统一丢掉
        .filter(|l| {
            let lower = l.to_lowercase();
            !lower.starts_with("no broken requirements")
                && !lower.starts_with("all installed packages are compatible")
                && !lower.starts_with("checked ")
                && !lower.starts_with("using python")
        })
        .collect()
}

fn python_version(py: &runtime::PyEnv) -> String {
    proc::run(&py.python, &["-V"], None, &BTreeMap::new())
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.replace("Python ", "").trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 命令：检测
#[tauri::command]
pub async fn nodes_deps_status(instance_id: String) -> Result<DepsReport, AppError> {
    // 要跑 uv pip freeze + pip check，扔到阻塞线程，别卡界面
    tauri::async_runtime::spawn_blocking(move || {
        let inst = load_instance(&instance_id)?;
        let root = PathBuf::from(&inst.root_dir);
        let Some(py) = runtime::resolve_python(&inst) else {
            let mut report = build_report(&root, &BTreeMap::new(), String::new(), Vec::new());
            report.no_venv = true;
            report.pending_nodes = 0;
            return Ok(report);
        };
        // freeze、pip check、direct_url 扫描和 Python 版本查询互不依赖，
        // 并行走完再组装报告；否则 Windows 上连续等 4 个子进程会明显拖慢
        // “没有要装的依赖”这种本应很快的检查。
        let frozen_py = py.clone();
        let check_py = py.clone();
        let direct_py = py.clone();
        let version_py = py.clone();
        let (installed, conflicts, direct_urls, py_version) = std::thread::scope(|scope| {
            let frozen = scope.spawn(move || freeze(&frozen_py));
            let checked = scope.spawn(move || pip_check(&check_py));
            let direct = scope.spawn(move || installed_direct_urls(&direct_py));
            let version = scope.spawn(move || python_version(&version_py));
            (frozen.join(), checked.join(), direct.join(), version.join())
        });
        let installed =
            installed.map_err(|_| AppError::new("DEPS_FREEZE_THREAD", "读取已装包线程异常"))??;
        let conflicts = conflicts.unwrap_or_default();
        let direct_urls = direct_urls.unwrap_or_default();
        let py_version = py_version.unwrap_or_default();
        let mut report = build_report(&root, &installed, py_version, conflicts);
        mark_installed_manual(&mut report, &root, &direct_urls);
        Ok(report)
    })
    .await
    .map_err(|e| AppError::new("DEPS_JOIN", format!("检测依赖线程异常：{e}")))?
}

// ---------------------------------------------------------------- 命令：安装
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DepsOptions {
    /// 节点自带 install.py 要不要跑（默认跑，和 ComfyUI-Manager 一致）
    pub run_install_py: bool,
}

impl Default for DepsOptions {
    fn default() -> Self {
        Self {
            run_install_py: true,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedLine {
    pub line: String,
    pub error: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPyResult {
    pub ran: bool,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDepsResult {
    pub name: String,
    pub installed: Vec<String>,
    pub skipped: Vec<DepLine>,
    pub failed: Vec<FailedLine>,
    pub install_py: Option<InstallPyResult>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepsInstallReport {
    pub snapshot_id: Option<String>,
    pub nodes: Vec<NodeDepsResult>,
    pub torch_before: Option<String>,
    pub torch_after: Option<String>,
    pub packages_before: usize,
    pub packages_after: usize,
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
}

fn spec_args(line: &str) -> Vec<String> {
    let raw = line.trim();
    if let Some(rest) = raw.strip_prefix("-e ") {
        return vec!["-e".into(), rest.trim().to_string()];
    }
    if let Some(rest) = raw.strip_prefix("--editable ") {
        return vec!["-e".into(), rest.trim().to_string()];
    }
    vec![raw.to_string()]
}

fn temp_file(tag: &str, ext: &str) -> Result<PathBuf, AppError> {
    let dir = crate::config::app_root().join("tmp");
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "deps-{}-{}.{}",
        tag.replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        chrono::Local::now().timestamp_millis(),
        ext
    );
    Ok(dir.join(name))
}

/// 读输出文件的新增部分，按 `\n` / `\r\n` 切日志行。
///
/// 单独的 `\r` 是终端里覆盖当前行的进度刷新，只保留最后一次内容；否则
/// pip/tqdm 下载一个大文件时会把几千条 `0% / 1% / 2%…` 都塞进控制台。
fn drain_output_file(
    path: &Path,
    offset: &mut u64,
    pending: &mut Vec<u8>,
    last_progress: &mut Instant,
    complete: bool,
    on_line: &mut dyn FnMut(&str),
) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        // 文件被截断/重建：重新开始。
        *offset = 0;
    }
    if len > *offset {
        if file.seek(SeekFrom::Start(*offset)).is_err() {
            return;
        }
        let mut chunk = Vec::new();
        let _ = file.take(len - *offset).read_to_end(&mut chunk);
        *offset += chunk.len() as u64;
        pending.extend_from_slice(&chunk);
    }
    let mut consumed = 0usize;
    let mut i = 0usize;
    while i < pending.len() {
        match pending[i] {
            b'\n' => {
                let text = String::from_utf8_lossy(&pending[consumed..i]);
                let text = text.trim();
                if !text.is_empty() {
                    on_line(text);
                }
                consumed = i + 1;
                i = consumed;
            }
            b'\r' => {
                // 缓冲末尾的 \r 可能是被拆开的 \r\n，留到下一次读再判断。
                if i + 1 >= pending.len() {
                    if !complete {
                        break;
                    }
                    let text = String::from_utf8_lossy(&pending[consumed..i]);
                    let text = text.trim();
                    if !text.is_empty() {
                        on_line(text);
                    }
                    consumed = i + 1;
                    break;
                }
                if pending[i + 1] == b'\n' {
                    let text = String::from_utf8_lossy(&pending[consumed..i]);
                    let text = text.trim();
                    if !text.is_empty() {
                        on_line(text);
                    }
                    consumed = i + 2;
                    i = consumed;
                } else {
                    // 单独的 \r：当前行旧内容被覆盖。控制台需要看进度，
                    // 每秒最多送一次；前端会把连续进度更新到同一行。
                    if last_progress.elapsed() >= Duration::from_secs(1) {
                        let text = String::from_utf8_lossy(&pending[consumed..i]);
                        let text = text.trim();
                        if !text.is_empty() {
                            on_line(text);
                        }
                        *last_progress = Instant::now();
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
    if complete && !pending.is_empty() {
        let end = if pending.last() == Some(&b'\r') {
            pending.len() - 1
        } else {
            pending.len()
        };
        let text = String::from_utf8_lossy(&pending[..end]);
        let text = text.trim();
        if !text.is_empty() {
            on_line(text);
        }
        pending.clear();
    }
}

/// 跑带超时的子进程。输出文件边跑边读，install.py 不再长时间只有一行
/// “运行中…”；pip 的换行/回车进度都会实时进控制台。
pub(crate) fn run_with_timeout(
    exe: &Path,
    args: &[&str],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<(bool, String, bool), AppError> {
    use std::process::{Command, Stdio};

    let out_path = temp_file("installpy", "out")?;
    let err_path = temp_file("installpy", "err")?;
    let out_file = std::fs::File::create(&out_path)?;
    let err_file = std::fs::File::create(&err_path)?;

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(out_file)
        .stderr(err_file);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new("INSTALL_PY_SPAWN", format!("启动 install.py 失败，{e}")))?;

    let mut text = String::new();
    let mut out_offset = 0u64;
    let mut err_offset = 0u64;
    let mut out_pending = Vec::new();
    let mut err_pending = Vec::new();
    let mut out_last = Instant::now();
    let mut err_last = Instant::now();

    let started = Instant::now();
    let mut timed_out = false;
    let ok = loop {
        {
            let mut record = |line: &str| {
                if text.len() < 2 * 1024 * 1024 {
                    text.push_str(line);
                    text.push('\n');
                }
                on_line(line);
            };
            drain_output_file(
                &out_path,
                &mut out_offset,
                &mut out_pending,
                &mut out_last,
                false,
                &mut record,
            );
            drain_output_file(
                &err_path,
                &mut err_offset,
                &mut err_pending,
                &mut err_last,
                false,
                &mut record,
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if started.elapsed() > timeout => {
                // install.py 常拉起 pip/编译器子进程，只杀父进程会把它们留在
                // 后台继续写 venv。
                let _ = crate::runtime::kill_pid_tree(child.id());
                let _ = child.wait();
                timed_out = true;
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(150)),
            Err(_) => break false,
        }
    };

    {
        let mut record = |line: &str| {
            if text.len() < 2 * 1024 * 1024 {
                text.push_str(line);
                text.push('\n');
            }
            on_line(line);
        };
        drain_output_file(
            &out_path,
            &mut out_offset,
            &mut out_pending,
            &mut out_last,
            true,
            &mut record,
        );
        drain_output_file(
            &err_path,
            &mut err_offset,
            &mut err_pending,
            &mut err_last,
            true,
            &mut record,
        );
    }
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);

    let text = text.trim();
    let tail: String = if text.chars().count() > 2000 {
        text.chars()
            .rev()
            .take(2000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        text.to_string()
    };
    Ok((ok && !timed_out, tail, timed_out))
}

/// 装一个节点的依赖：过滤→uv pip install →失败逐行重试 →install.py
fn install_one(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    node: &NodeEntry,
    options: &DepsOptions,
    emit: &mut dyn FnMut(VersionEvent),
) -> Result<NodeDepsResult, AppError> {
    let Some(py) = runtime::resolve_python(inst) else {
        return Err(AppError::new("NO_VENV", no_python_message(inst)));
    };
    let uv = uv()?;
    let mut env = uv_env(&py);
    let installed = freeze(&py)?;
    let direct_urls = installed_direct_urls(&py);

    // 节点依赖经常声明 torch 系包；如果不动 constraints，uv 会顺手把 torch 换掉。
    let constraints_path = write_torch_constraints(&installed)?;
    if let Some(path) = &constraints_path {
        // install.py 里自己调 pip/uv 时也能继承这两个环境变量
        env.insert("PIP_CONSTRAINT".into(), path.to_string_lossy().to_string());
        env.insert("UV_CONSTRAINT".into(), path.to_string_lossy().to_string());
    }

    let req = node.dir.join("requirements.txt");
    let lines = std::fs::read_to_string(&req)
        .map(|t| parse_requirements(&t))
        .unwrap_or_default();
    let marker_env = MarkerEnv::current(&python_version(&py));
    let plan = plan_requirements_with_env(&lines, &installed, &marker_env);

    let mut result = NodeDepsResult {
        name: node.name.clone(),
        installed: Vec::new(),
        skipped: plan
            .skipped
            .iter()
            .map(|p| DepLine {
                line: p.line.clone(),
                reason: p.reason.clone(),
            })
            .collect(),
        failed: Vec::new(),
        install_py: None,
    };

    let mut manual = Vec::new();
    for line in plan.manual {
        match manual_installed(&line, &node.dir, &direct_urls) {
            Some(reason) => result.skipped.push(DepLine { line, reason }),
            None => manual.push(line),
        }
    }

    let to_install: Vec<String> = plan
        .pending
        .iter()
        .map(|p| p.line.clone())
        .chain(manual.iter().cloned())
        .collect();

    if !to_install.is_empty() {
        emit(VersionEvent::info(
            "deps",
            format!(
                "{}：准备装 {} 个（跳过 {} 个）",
                node.name,
                plan.pending.len() + manual.len(),
                result.skipped.len()
            ),
        ));
        // 过滤后的 requirements：保持原始语法（`-e .` 之类），只留要装的行
        let filtered = to_install.join("\n") + "\n";
        let tmp = temp_file(&node.name, "txt")?;
        std::fs::write(&tmp, filtered)?;
        let indexes = index_candidates(settings);
        let python_str = py.python.to_string_lossy().to_string();
        let tmp_str = tmp.to_string_lossy().to_string();
        let mut batch_ok = false;
        let mut batch_detail = String::new();
        for (n, index) in indexes.iter().enumerate() {
            if n > 0 {
                emit(VersionEvent::warn(
                    "deps",
                    format!(
                        "{}：镜像里装不上（可能还没同步到），改用官方PyPI 重试",
                        node.name
                    ),
                ));
            }
            let mut argv: Vec<&str> = vec![
                "pip",
                "install",
                "--python",
                &python_str,
                "-r",
                &tmp_str,
                "--index-url",
                index,
            ];
            let constraint_str = constraints_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string());
            if let Some(path) = &constraint_str {
                argv.push("--constraint");
                argv.push(path);
            }
            let out = proc::run_stream(&uv, &argv, Some(&node.dir), &env, &mut |line| {
                if !line.trim().is_empty() {
                    emit(VersionEvent::info("deps", line.trim()));
                }
            });
            match out {
                Ok(o) if o.ok() => {
                    batch_ok = true;
                    break;
                }
                Ok(o) => batch_detail = o.detail(),
                Err(e) => batch_detail = e.message,
            }
        }
        let _ = std::fs::remove_file(&tmp);

        if batch_ok {
            result.installed = to_install.clone();
        } else if to_install.len() == 1 {
            // 只有一行时 batch 已经按所有镜像试过一遍；再逐行重试只是
            // 用同一命令再跑一次，白白多等一轮解析。直接记失败即可。
            let line = to_install[0].clone();
            emit(VersionEvent::warn(
                "deps",
                format!("{}：安装失败：{}", node.name, batch_detail),
            ));
            result.failed.push(FailedLine {
                line,
                error: batch_detail,
            });
        } else {
            // 整批失败：逐个来，把装不动的行点出来
            let detail = batch_detail;
            emit(VersionEvent::warn(
                "deps",
                format!("{}：整批安装失败，逐行重试：{detail}", node.name),
            ));
            let python_str = py.python.to_string_lossy().to_string();
            let constraint_str = constraints_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string());
            for line in &to_install {
                let args = spec_args(line);
                let mut last_error = String::new();
                let mut ok = false;
                for index in &indexes {
                    let mut argv: Vec<&str> = vec!["pip", "install", "--python"];
                    argv.push(&python_str);
                    for a in &args {
                        argv.push(a);
                    }
                    argv.push("--index-url");
                    argv.push(index);
                    if let Some(path) = &constraint_str {
                        argv.push("--constraint");
                        argv.push(path);
                    }
                    match proc::run_stream(&uv, &argv, Some(&node.dir), &env, &mut |line| {
                        if !line.trim().is_empty() {
                            emit(VersionEvent::info("deps", line.trim()));
                        }
                    }) {
                        Ok(o) if o.ok() => {
                            ok = true;
                            break;
                        }
                        Ok(o) => last_error = o.detail(),
                        Err(e) => last_error = e.message,
                    }
                }
                if ok {
                    result.installed.push(line.clone());
                } else {
                    result.failed.push(FailedLine {
                        line: line.clone(),
                        error: last_error,
                    });
                }
            }
        }
    } else if !result.skipped.is_empty() {
        emit(VersionEvent::info(
            "deps",
            format!(
                "{}：没有要装的，跳过 {} 条",
                node.name,
                result.skipped.len()
            ),
        ));
    }

    if options.run_install_py
        && node.dir.join("install.py").exists()
        && !settings.install.allow_install_script
    {
        emit(VersionEvent::warn(
            "deps",
            format!(
                "{}：全局设置禁止运行 install.py，已跳过（只会安装 requirements.txt）",
                node.name
            ),
        ));
    }
    if options.run_install_py
        && settings.install.allow_install_script
        && node.dir.join("install.py").exists()
    {
        emit(VersionEvent::info(
            "deps",
            format!("{}：运行 install.py…", node.name),
        ));
        match run_with_timeout(
            &py.python,
            &["install.py"],
            &node.dir,
            &env,
            INSTALL_PY_TIMEOUT,
            &mut |line| {
                let _ = emit(VersionEvent::info(
                    "deps",
                    format!("{}：{}", node.name, line),
                ));
            },
        ) {
            Ok((ok, message, timed_out)) => {
                let message = if timed_out {
                    format!("install.py 超时被终止：{message}")
                } else {
                    message
                };
                emit(if ok {
                    VersionEvent::done("deps", format!("{}：install.py 完成", node.name))
                } else {
                    VersionEvent::warn("deps", format!("{}：install.py 失败", node.name))
                });
                result.install_py = Some(InstallPyResult {
                    ran: true,
                    ok: ok && !timed_out,
                    message,
                });
            }
            Err(e) => {
                result.install_py = Some(InstallPyResult {
                    ran: false,
                    ok: false,
                    message: e.message,
                });
            }
        }
    }

    if let Some(path) = &constraints_path {
        let _ = std::fs::remove_file(path);
    }

    Ok(result)
}

/// 给刚克隆好的节点补依赖（`nodes_install` 复用）；失败只报事件，不阻断安装。
///
/// 传的是整个实例配置（不是实例根）：这样「自建venv / 已有环境」两种模式都能解析。
/// 以前这里从节点目录往上推两级找实例根，得到的是`<实例根/ComfyUI`：
/// 于是列clone 完就报「找不到实例的虚拟环境」。
pub fn install_for_node(
    inst: &InstanceConfig,
    settings: &GlobalSettings,
    node: &NodeEntry,
    emit: &mut dyn FnMut(VersionEvent),
) -> Result<NodeDepsResult, AppError> {
    install_one(inst, settings, node, &DepsOptions::default(), emit)
}

/// 一个节点的依赖结果 → 日志里的几行（装了什么 / 跳过什么、为什么 / 什么没装上）。
pub fn describe_node(node: &NodeDepsResult) -> Vec<String> {
    let mut lines = vec![format!("● {}", node.name)];
    if node.installed.is_empty() && node.skipped.is_empty() && node.failed.is_empty() {
        lines.push("  依赖已经是齐的".to_string());
    }
    if !node.installed.is_empty() {
        lines.push(format!("  新装 {} 项：", node.installed.len()));
        for pkg in &node.installed {
            lines.push(format!("    + {pkg}"));
        }
    }
    if !node.skipped.is_empty() {
        lines.push(format!("  跳过 {} 项：", node.skipped.len()));
        for dep in &node.skipped {
            lines.push(format!(
                "    - {}：{}",
                dep.line,
                one_line(&dep.reason, 200)
            ));
        }
    }
    if !node.failed.is_empty() {
        lines.push(format!("  失败 {} 项：", node.failed.len()));
        for bad in &node.failed {
            lines.push(format!("    ! {}：{}", bad.line, one_line(&bad.error, 300)));
        }
    }
    if let Some(py) = &node.install_py {
        if py.ran {
            lines.push(format!(
                "  install.py：{}，{}",
                if py.ok { "运行成功" } else { "失败" },
                one_line(&py.message, 300)
            ));
        }
    }
    lines
}

/// 一批补依赖的完整账本：逐节点明细+ torch / 包数 / pip check / 提示。
pub fn describe_report(report: &DepsInstallReport) -> String {
    let mut lines = Vec::new();
    for node in &report.nodes {
        lines.extend(describe_node(node));
    }
    lines.push(format!(
        "torch：{} → {}",
        report.torch_before.as_deref().unwrap_or("—"),
        report.torch_after.as_deref().unwrap_or("—")
    ));
    lines.push(format!(
        "包总数：{} → {}",
        report.packages_before, report.packages_after
    ));
    if !report.conflicts.is_empty() {
        lines.push(format!("pip check：{}", report.conflicts.join("；")));
    }
    if let Some(id) = &report.snapshot_id {
        lines.push(format!("装之前快照：{id}"));
    }
    if !report.warnings.is_empty() {
        lines.push(format!("提示：{}", report.warnings.join("；")));
    }
    lines.join("\n")
}

/// 失败原因常带整段 stderr：日志里压成一行，别把时间线撑开
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}

#[tauri::command]
pub async fn nodes_install_deps(
    state: State<'_, AppState>,
    instance_id: String,
    names: Option<Vec<String>>,
    options: Option<DepsOptions>,
    on_event: Channel<VersionEvent>,
) -> Result<DepsInstallReport, AppError> {
    let _op = crate::state::OpGuard::acquire(&instance_id, "安装节点依赖")?;
    let inst = load_instance(&instance_id)?;
    if state.runs.lock().contains_key(&instance_id) {
        return Err(AppError::new(
            "INSTANCE_RUNNING",
            "实例正在运行：先停止再装依赖（venv 被占用，装完也不会生效）",
        ));
    }
    if runtime::resolve_python(&inst).is_none() {
        return Err(AppError::new("NO_VENV", no_python_message(&inst)));
    }
    let settings = state.settings.lock().clone();
    let options = options.unwrap_or_default();
    // uv 装包 / 路install.py 几十秒到几分钟：整段丢到阻塞线程，IPC 与界面都不卡
    let result = tauri::async_runtime::spawn_blocking(move || {
        run_install(inst, settings, names, options, on_event)
    })
    .await
    .map_err(|e| AppError::new("DEPS_JOIN", format!("装依赖线程异常：{e}")))?;

    match &result {
        Ok(report) => {
            let names: Vec<String> = report.nodes.iter().map(|n| n.name.clone()).collect();
            let failed: usize = report.nodes.iter().map(|n| n.failed.len()).sum();
            crate::activity::record(
                &instance_id,
                "node-deps",
                if failed > 0 { "warn" } else { "ok" },
                format!("补依赖路径 {}", names.join("、")),
                Some(describe_report(report)),
            );
        }
        Err(e) => crate::activity::record(
            &instance_id,
            "node-deps",
            "error",
            "补依赖失败",
            Some(format!("{}\n{}", e.code, e.message)),
        ),
    }
    result
}

/// 真正的安装流程：只在这里碰会话状态，全程不需要State
fn run_install(
    inst: InstanceConfig,
    settings: GlobalSettings,
    names: Option<Vec<String>>,
    options: DepsOptions,
    on_event: Channel<VersionEvent>,
) -> Result<DepsInstallReport, AppError> {
    let root = PathBuf::from(&inst.root_dir);
    let Some(py) = runtime::resolve_python(&inst) else {
        return Err(AppError::new("NO_VENV", no_python_message(&inst)));
    };

    let mut targets: Vec<NodeEntry> = collect_nodes(&root)
        .into_iter()
        .filter(|n| n.dir.join("requirements.txt").exists() || n.dir.join("install.py").exists())
        .collect();
    if let Some(names) = &names {
        let wanted: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        targets.retain(|n| wanted.contains(&n.name.to_lowercase()));
    } else {
        // “全部补依赖”不应执行已禁用节点自带的 install.py。
        targets.retain(|n| n.enabled);
    }
    if targets.is_empty() {
        return Err(AppError::new(
            "NO_TARGETS",
            "没有要处理的节点（这些节点没有 requirements.txt 或 install.py）",
        ));
    }

    let before = freeze(&py)?;
    let packages_before = before.len();
    let torch_before = before.get("torch").cloned();

    // 装依赖前先拍一条快照：装坏了能一键回到装之前
    let snapshot_id = match snapshot::capture_and_save(
        &inst,
        &settings,
        snapshot::T_PRE_DEPS,
        Some(format!(
            "装依赖前：{}",
            targets
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        )),
    ) {
        Ok(snap) => Some(snap.id),
        Err(e) => {
            let _ = on_event.send(VersionEvent::warn(
                "deps",
                format!("装依赖前的快照没拍成（继续安装），{}", e.message),
            ));
            None
        }
    };

    let mut emit = |ev: VersionEvent| {
        let _ = on_event.send(ev);
    };

    let mut results = Vec::new();
    let mut skipped_install_py = Vec::new();
    for node in &targets {
        if options.run_install_py
            && !settings.install.allow_install_script
            && node.dir.join("install.py").exists()
        {
            skipped_install_py.push(format!(
                "{}：全局设置禁止运行 install.py，本次只处理 requirements.txt",
                node.name
            ));
        }
        match install_one(&inst, &settings, node, &options, &mut emit) {
            Ok(result) => results.push(result),
            Err(e) => {
                emit(VersionEvent::failed(
                    "deps",
                    format!("{}：{}", node.name, e.message),
                ));
                results.push(NodeDepsResult {
                    name: node.name.clone(),
                    installed: Vec::new(),
                    skipped: Vec::new(),
                    failed: vec![FailedLine {
                        line: node.name.clone(),
                        error: e.message,
                    }],
                    install_py: None,
                });
            }
        }
    }

    let after = freeze(&py).unwrap_or_else(|_| before.clone());
    let conflicts = pip_check(&py);
    let torch_after = after.get("torch").cloned();

    let mut warnings = skipped_install_py;
    if torch_before != torch_after {
        warnings.push(format!(
            "torch 版本变了：{} → {}；如果这不是你要的，回滚到「装依赖前」的快照",
            torch_before.clone().unwrap_or_else(|| "未装".into()),
            torch_after.clone().unwrap_or_else(|| "未装".into())
        ));
    }
    if !conflicts.is_empty() {
        warnings.push(format!(
            "装完发现 {} 条依赖冲突，见下面 pip check",
            conflicts.len()
        ));
    }

    emit(VersionEvent::done(
        "deps",
        format!("装依赖结束：处理人{} 个节点", results.len()),
    ));

    Ok(DepsInstallReport {
        snapshot_id,
        nodes: results,
        torch_before,
        torch_after,
        packages_before,
        packages_after: after.len(),
        conflicts,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用实例：rootDir 指向临时目录，Python 走自建venv
    fn inst_at(root: &Path) -> InstanceConfig {
        let root = root.to_string_lossy().replace('\\', "\\\\");
        serde_json::from_str(&format!(
            r#"{{"id":"abcdef01","name":"t","rootDir":"{root}","engine":"cpu","engineDetail":null,"pythonVersion":"3.12","comfy":{{"channel":"stable","ref":"","commit":"","version":null}},"server":{{"listen":"127.0.0.1","port":8188,"corsOrigin":null,"extraArgs":[]}},"env":{{}},"overrides":{{}},"imported":false,"state":"ready","createdAt":"","lastStartedAt":null}}"#
        ))
        .expect("instance json")
    }

    fn installed(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn torch_constraints_pin_installed_versions_and_codec() {
        let text = torch_constraints(&installed(&[
            ("torch", "2.14.1+cu130"),
            ("torchvision", "0.29.1+cu130"),
            ("torchaudio", "2.11.1"),
        ]))
        .expect("本torch 就该有约束");
        assert!(text.contains("torch==2.14.1+cu130"));
        assert!(text.contains("torchvision==0.29.1+cu130"));
        assert!(text.contains("torchaudio==2.11.1"));
        // 没装 torchcodec：按 torch 版本行0.17.0，防止节点依赖顺手装错        assert!(text.contains("torchcodec==0.17.0"));

        let already =
            torch_constraints(&installed(&[("torch", "2.8.0"), ("torchcodec", "0.7.0")])).unwrap();
        assert!(already.contains("torchcodec==0.7.0"));
        assert!(!already.contains("0.17.0"));

        assert!(torch_constraints(&installed(&[])).is_none());
    }

    #[test]
    fn classify_kinds() {
        assert_eq!(
            classify("numpy==1.26.4 # 常用").map(|l| l.kind),
            Some(LineKind::Package)
        );
        assert_eq!(classify("# 注释"), None);
        assert_eq!(classify("   "), None);
        assert_eq!(classify("-e .").map(|l| l.kind), Some(LineKind::Editable));
        assert_eq!(
            classify("git+https://github.com/a/b").map(|l| l.kind),
            Some(LineKind::Url)
        );
        assert_eq!(
            classify("-r requirements-extra.txt").map(|l| l.kind),
            Some(LineKind::Include)
        );
        assert_eq!(
            classify("--index-url https://mirror/simple").map(|l| l.kind),
            Some(LineKind::Index)
        );
        assert_eq!(
            classify("name @ https://x/y.whl").map(|l| l.kind),
            Some(LineKind::Package)
        );
    }

    #[test]
    fn names_and_specs() {
        assert_eq!(
            package_name("Pillow>=10 ; python_version<'3.12'"),
            Some("pillow".into())
        );
        assert_eq!(
            package_name("opencv_python_headless"),
            Some("opencv-python-headless".into())
        );
        assert_eq!(package_name("https://x/y.whl"), None);
        assert_eq!(version_spec("numpy==1.26.4"), Some("==1.26.4".into()));
        assert_eq!(version_spec("numpy"), None);
        assert_eq!(pinned_exact("==1.26.4"), Some("1.26.4".into()));
        assert_eq!(pinned_exact(">=1.0"), None);
        assert_eq!(pinned_exact("==1.*"), None);
    }

    fn direct_url(name: &str, version: &str, url: &str, commit: &str) -> DirectUrlInfo {
        DirectUrlInfo {
            name: name.into(),
            version: version.into(),
            url: url.into(),
            vcs: "git".into(),
            commit: commit.into(),
            requested: String::new(),
            editable: false,
        }
    }

    #[test]
    fn manual_git_dependency_is_installed_from_direct_url() {
        let urls = vec![
            direct_url(
                "img2texture",
                "1.0.6",
                "https://github.com/ltdrdata/img2texture.git",
                "d6159abea44a0b2cf77454d3d46962c8b21eb9d3",
            ),
            direct_url(
                "cstr",
                "0.1.0",
                "https://github.com/ltdrdata/cstr",
                "0520c29a18a7a869a6e5983861d6f7a4c86f8e9b",
            ),
        ];
        let dir = Path::new(".");
        let reason = manual_installed(
            "git+https://github.com/ltdrdata/img2texture.git",
            dir,
            &urls,
        )
        .expect("同仓库 .git 差异不应影响判断");
        assert!(reason.contains("已安装"));
        assert!(reason.contains("1.0.6"));
        assert!(
            manual_installed("git+https://github.com/ltdrdata/cstr", dir, &urls).is_some(),
            "direct_url 带 .git、requirements 不带 .git 时也要能匹配"
        );
        assert!(
            manual_installed("git+https://github.com/ltdrdata/other.git", dir, &urls).is_none(),
            "别的仓库不能误判成已装"
        );

        // 锁了 commit 时，只有 direct_url 的 commit 匹配才算已装。
        let pinned = "git+https://github.com/ltdrdata/img2texture.git@d6159abea44a0b2cf77454d3d46962c8b21eb9d3";
        assert!(manual_installed(pinned, dir, &urls).is_some());
        let wrong = "git+https://github.com/ltdrdata/img2texture.git@0000000000000000000000000000000000000000";
        assert!(manual_installed(wrong, dir, &urls).is_none());
    }

    #[test]
    fn markers_that_do_not_apply_are_skipped() {
        let env = MarkerEnv::current("3.13.5");
        if machine_key(env.get("platform_machine").unwrap()) == "aarch64" {
            return;
        }
        let plan = plan_requirements_with_env(
            &parse_requirements("jetson-stats; platform_machine == 'aarch64'\n"),
            &BTreeMap::new(),
            &env,
        );
        assert!(
            plan.pending.is_empty(),
            "x64 上不该把 aarch64 专属包判成缺依赖"
        );
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.contains("当前环境不适用"));
    }

    #[test]
    fn markers_compare_python_version_and_unknown_markers_are_skipped() {
        let env = MarkerEnv::current("3.13.5");
        assert_eq!(marker_applies("python_version >= '3.12'", &env), Some(true));
        assert_eq!(marker_applies("python_version < '3.12'", &env), Some(false));
        assert_eq!(
            marker_applies("python_version >= '3.12' and python_version < '4.0'", &env),
            Some(true)
        );

        let plan = plan_requirements_with_env(
            &parse_requirements("demo-pkg; platform_release == '6.8.0'\n"),
            &BTreeMap::new(),
            &env,
        );
        assert!(
            plan.pending.is_empty(),
            "看不懂的 marker 不应反复触发自动安装"
        );
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.contains("无法判断环境标记"));
    }

    #[test]
    fn applicable_marker_still_installs() {
        let env = MarkerEnv::current("3.11.9");
        let plan = plan_requirements_with_env(
            &parse_requirements("demo-pkg; python_version >= '3.11'\n"),
            &BTreeMap::new(),
            &env,
        );
        assert_eq!(plan.pending.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn triton_aliases_count_as_installed() {
        let map = installed(&[("triton-windows", "3.2.0")]);
        assert_eq!(
            installed_version(&map, "triton").map(String::as_str),
            Some("3.2.0")
        );
        assert_eq!(
            installed_version(&map, "pytorch-triton").map(String::as_str),
            Some("3.2.0")
        );
        let plan = plan_requirements(&parse_requirements("triton>=3.0\n"), &map);
        assert!(
            plan.pending.is_empty(),
            "triton-windows 已装就不该再判缺依赖"
        );
    }

    #[test]
    fn protected_missing_installs_and_installed_skips() {
        let lines = parse_requirements("torch>=2.4\ntorchvision\nnumpy==1.26.4\n");
        // 环境里只有 torch（版本不同）→ torch 跳过；torchvision 没有 → 装；numpy 没有 → 裁
        let plan = plan_requirements(
            &lines,
            &installed(&[("torch", "2.5.1+cu121"), ("numpy", "1.26.4")]),
        );
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].line, "torchvision");
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.contains("不升级也不降级"));
        // numpy 版本一致，不出现。
        assert!(plan.pending.iter().all(|p| !p.line.starts_with("numpy")));
    }

    #[test]
    fn non_protected_version_drift_is_pending() {
        let lines = parse_requirements("numpy==1.24.0\n");
        let plan = plan_requirements(&lines, &installed(&[("numpy", "2.0.0")]));
        assert_eq!(plan.pending.len(), 1);
        assert!(plan.pending[0].reason.contains("版本不符"));
    }

    #[test]
    fn directives_are_reported_not_installed() {
        let lines = parse_requirements(
            "--index-url https://download.pytorch.org/whl/cu121\n-r extra.txt\n-e .\ngit+https://github.com/x/y\n",
        );
        let plan = plan_requirements(&lines, &BTreeMap::new());
        assert!(plan.pending.is_empty());
        assert_eq!(plan.manual.len(), 2);
        assert_eq!(plan.skipped.len(), 2);
        assert!(plan.skipped[0].reason.contains("镜像"));
    }

    #[test]
    fn install_for_node_takes_instance_root_and_finds_venv() {
        // 回归：以前 install_for_node 从节点目录往上推两级，得到 ComfyUI/；
        // 于是 clone 完立刻报 NO_VENV。现在传入实例根，就不该再报这个错。
        let tmp = std::env::temp_dir().join(format!("comfyfleet-deps-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("inst");
        let scripts = root.join(".venv").join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("python.exe"), b"").unwrap();
        let node_dir = root.join("ComfyUI").join("custom_nodes").join("DemoNode");
        std::fs::create_dir_all(&node_dir).unwrap();
        std::fs::write(node_dir.join("requirements.txt"), "demo-pkg\n").unwrap();

        let entry = NodeEntry {
            name: "DemoNode".into(),
            dir: node_dir,
            enabled: true,
        };
        let inst = inst_at(&root);
        let err = install_for_node(&inst, &GlobalSettings::default(), &entry, &mut |_| {})
            .expect_err("偏python 跑不了freeze，应该报错");
        assert_ne!(err.code, "NO_VENV", "实例根有 venv 却被当成没有，{err:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn report_marks_nodes_with_pending() {
        let dir = std::env::temp_dir().join(format!("comfyfleet-deps-test-{}", std::process::id()));
        let nodes = dir.join("ComfyUI").join("custom_nodes");
        std::fs::create_dir_all(nodes.join("NodeA")).unwrap();
        std::fs::create_dir_all(nodes.join("NodeB.disabled")).unwrap();
        std::fs::write(
            nodes.join("NodeA").join("requirements.txt"),
            "aiohttp==3.9.5\n",
        )
        .unwrap();
        std::fs::write(
            nodes.join("NodeB.disabled").join("requirements.txt"),
            "numpy==1.26.4\n",
        )
        .unwrap();
        std::fs::write(
            nodes.join("NodeB.disabled").join("install.py"),
            "print(1)\n",
        )
        .unwrap();

        let report = build_report(
            &dir,
            &installed(&[("aiohttp", "3.9.5"), ("numpy", "1.26.4")]),
            "3.12.7".into(),
            Vec::new(),
        );
        assert_eq!(report.nodes.len(), 2);
        assert_eq!(report.pending_nodes, 0);
        assert!(report.nodes.iter().all(|n| n.pending.is_empty()));
        assert!(
            !report
                .nodes
                .iter()
                .find(|n| n.name == "NodeB")
                .unwrap()
                .enabled
        );

        let report = build_report(&dir, &BTreeMap::new(), "3.12.7".into(), Vec::new());
        assert_eq!(report.pending_nodes, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_collapses_carriage_return_progress() {
        let dir =
            std::env::temp_dir().join(format!("comfyfleet-drain-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.log");
        std::fs::write(&path, b"Downloading a\n0%\r50%\r").unwrap();
        let mut offset = 0u64;
        let mut pending = Vec::new();
        let mut last = Instant::now();
        let mut lines = Vec::new();
        drain_output_file(
            &path,
            &mut offset,
            &mut pending,
            &mut last,
            false,
            &mut |line| lines.push(line.to_string()),
        );
        assert_eq!(lines, vec!["Downloading a"]);

        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"100%\nDone\n").unwrap();
        }
        drain_output_file(
            &path,
            &mut offset,
            &mut pending,
            &mut last,
            true,
            &mut |line| lines.push(line.to_string()),
        );
        assert_eq!(lines, vec!["Downloading a", "100%", "Done"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn install_py_runner_streams_output_before_exit() {
        let mut lines: Vec<(Instant, String)> = Vec::new();
        let (ok, tail, timed_out) = run_with_timeout(
            Path::new("cmd"),
            &[
                "/c",
                "echo first-line & ping -n 2 127.0.0.1 >nul & echo second-line",
            ],
            Path::new("."),
            &BTreeMap::new(),
            Duration::from_secs(10),
            &mut |line| lines.push((Instant::now(), line.to_string())),
        )
        .expect("应能执行");
        assert!(ok);
        assert!(!timed_out);
        assert!(tail.contains("second-line"), "tail={tail}");
        let first = lines
            .iter()
            .find(|(_, line)| line.contains("first-line"))
            .map(|(at, _)| *at)
            .expect("第一行应被实时回调");
        let second = lines
            .iter()
            .find(|(_, line)| line.contains("second-line"))
            .map(|(at, _)| *at)
            .expect("第二行应被实时回调");
        assert!(
            second.duration_since(first) >= Duration::from_millis(300),
            "第二行应在等待后到达，而不是退出时才一起出现"
        );
    }
}
