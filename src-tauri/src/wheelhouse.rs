//! 本地 wheel 库：用户在设置里指定一个目录，自己把 `*.whl` 放进去。
//!
//! 恢复快照、跨机同步时按「文件名」或「包名 + 版本」在这个目录里找；找不到就
//! 提示用户补文件，任务继续执行（缺的包列在报告里）。
//!
//! 这里不做副本、不做索引、也不扫描 venv：目录本身就是事实来源，用户可以随时
//! 增删。用户手动装包时轮子就放在自己选的位置，不需要再复制一份。

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use walkdir::WalkDir;

use crate::config::GlobalSettings;
use crate::error::AppError;

/// 给界面显示的库状态。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInfo {
    pub path: String,
    pub count: usize,
    pub bytes: u64,
    /// 设置里填了目录（哪怕目录不存在）。
    pub configured: bool,
    /// 目录当前存在。
    pub exists: bool,
}

/// 设置里填的目录：相对路径按数据目录解析，这样换程序位置、整个数据目录拷走都还能用。
fn resolved(settings: &GlobalSettings) -> Option<PathBuf> {
    let raw = settings.wheels.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        crate::config::app_root().join(path)
    })
}

/// 启动时把默认库建出来，避免设置里显示默认路径却写着“目录不存在”。
pub fn ensure_default_dir() {
    let dir = crate::config::default_wheels_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("创建默认 wheel 库失败（{}）：{e}", dir.display());
    }
}

/// 设置里配置的目录，不管当前是否存在。用于切换目录时找旧库。
pub fn configured_dir(settings: &GlobalSettings) -> Option<PathBuf> {
    resolved(settings)
}

/// 一次 wheel 库迁移的结果。只记真正移动过的文件，便于配置落盘失败时回滚。
#[derive(Debug, Default)]
pub struct Migration {
    moved: Vec<(PathBuf, PathBuf)>,
    skipped: usize,
}

impl Migration {
    pub fn moved(&self) -> usize {
        self.moved.len()
    }

    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// 把已经移动过去的文件搬回原处；用于设置写盘失败后的回滚。
    pub fn rollback(&self) -> Result<(), AppError> {
        let mut first: Option<AppError> = None;
        for (src, dst) in self.moved.iter().rev() {
            if !dst.exists() || src.exists() {
                continue;
            }
            if let Err(e) = move_one(dst, src) {
                if first.is_none() {
                    first = Some(e);
                }
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// 切换本地 wheel 库时，把旧目录里的归档文件真正搬到新目录。
///
/// 保留相对子目录和文件名，因此快照里记录的本地 wheel 文件名仍然能命中。
/// 目标已有同名但内容不同的文件时不覆盖，直接报错；迁移失败会尽量回滚已搬走的文件。
pub fn migrate_library(from: &Path, to: &Path) -> Result<Migration, AppError> {
    let mut report = Migration::default();
    if from == to {
        return Ok(report);
    }
    if !from.is_dir() {
        fs::create_dir_all(to)
            .map_err(|e| migrate_err(format!("创建新的 wheel 库失败（{}）：{e}", to.display())))?;
        return Ok(report);
    }
    if to.starts_with(from) {
        return Err(migrate_err(format!(
            "新的 wheel 库不能放在旧库目录里面：{}",
            to.display()
        )));
    }
    fs::create_dir_all(to)
        .map_err(|e| migrate_err(format!("创建新的 wheel 库失败（{}）：{e}", to.display())))?;

    let files = archives(from);
    for src in files {
        let rel = match src.strip_prefix(from) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        let dst = to.join(rel);
        if dst.exists() {
            match same_file(&src, &dst) {
                Ok(true) => {
                    // 目标里已经有完全相同的一份；保留两边，不误删旧文件。
                    report.skipped += 1;
                    continue;
                }
                Ok(false) => {
                    let err = migrate_err(format!(
                        "新 wheel 库里已有不同内容的同名文件：{}。没有覆盖，设置未保存。",
                        dst.display()
                    ));
                    return Err(with_rollback(err, report.rollback()));
                }
                Err(e) => {
                    let err = migrate_err(format!(
                        "比较新旧 wheel 文件失败（{}）：{e}",
                        dst.display()
                    ));
                    return Err(with_rollback(err, report.rollback()));
                }
            }
        }
        if let Err(e) = move_one(&src, &dst) {
            return Err(with_rollback(e, report.rollback()));
        }
        report.moved.push((src, dst));
    }
    Ok(report)
}

fn migrate_err(message: impl Into<String>) -> AppError {
    AppError::new("WHEEL_MIGRATE", message)
}

fn with_rollback(err: AppError, rollback: Result<(), AppError>) -> AppError {
    match rollback {
        Ok(()) => err,
        Err(rb) => migrate_err(format!(
            "{}；回滚已移动文件也失败：{}",
            err.message, rb.message
        )),
    }
}

/// 先 rename；跨盘时 fallback 到 copy + 删除源文件。任何一步失败都保留源文件。
fn move_one(from: &Path, to: &Path) -> Result<(), AppError> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| migrate_err(format!("创建迁移目标目录失败（{}）：{e}", parent.display())))?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to).map_err(|e| {
        migrate_err(format!(
            "迁移 wheel 失败（{} → {}）：{e}",
            from.display(),
            to.display()
        ))
    })?;
    if let Err(e) = fs::remove_file(from) {
        // copy 成功但删源失败：撤掉目标，保持“源文件还在”的旧状态。
        let _ = fs::remove_file(to);
        return Err(migrate_err(format!(
            "迁移 wheel 后删除旧文件失败（{}）：{e}",
            from.display()
        )));
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let mut fa = fs::File::open(a)?;
    let mut fb = fs::File::open(b)?;
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let na = fa.read(&mut ba)?;
        let nb = fb.read(&mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// 目录存在才有值；没配置或目录被删掉都给 `None`，调用方按“没有库”处理。
pub fn dir(settings: &GlobalSettings) -> Option<PathBuf> {
    resolved(settings).filter(|path| path.is_dir())
}

pub fn info(settings: &GlobalSettings) -> LibraryInfo {
    let Some(path) = resolved(settings) else {
        return LibraryInfo {
            path: String::new(),
            count: 0,
            bytes: 0,
            configured: false,
            exists: false,
        };
    };
    let exists = path.is_dir();
    let files = if exists { archives(&path) } else { Vec::new() };
    LibraryInfo {
        path: path.to_string_lossy().to_string(),
        count: files.len(),
        bytes: files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum(),
        configured: true,
        exists,
    }
}

fn is_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".whl") || name.ends_with(".zip") || name.ends_with(".tar.gz")
}

/// 库里所有轮子/归档。允许子目录（按 CUDA、按包分文件夹都行），但不跟符号链接。
fn archives(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| is_archive(path))
        .collect()
}

/// `<包名>-<版本>(-<构建号>)?-<py>-<abi>-<平台>.whl` → (名字, 版本, tags)。
///
/// 名字本身可以带 `-`，所以从右往左切三段标签；构建号（纯数字）存在时跳过。
/// 认不出来就把整段当名字，交给文件名精确匹配兜底。
pub(crate) fn wheel_name_parts(filename: &str) -> (String, String, Vec<String>) {
    let stem = filename
        .strip_suffix(".whl")
        .or_else(|| filename.strip_suffix(".zip"))
        .or_else(|| filename.strip_suffix(".tar.gz"))
        .unwrap_or(filename);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return (stem.to_string(), String::new(), Vec::new());
    }
    // 标签固定三段；前面紧挨着的那段如果是纯数字，就是构建号。
    let mut version_index = parts.len() - 3;
    if version_index >= 2 && parts[version_index - 1].chars().all(|c| c.is_ascii_digit()) {
        version_index -= 1;
    }
    if version_index < 2 {
        return (stem.to_string(), String::new(), Vec::new());
    }
    (
        parts[..version_index - 1].join("-"),
        parts[version_index - 1].to_string(),
        parts[version_index..]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    )
}

/// 在 wheel 库里找一颗轮子。
///
/// 顺序：文件名精确匹配（用户按快照里记的名字放的）→ 包名 + 版本。
/// 版本对不上时，只有库里该包**只有一个**候选才兜底：多版本并存时宁可报缺，
/// 也不能把别的构建悄悄装进去。
pub fn find(dir: &Path, name: &str, version: &str, filename: Option<&str>) -> Option<PathBuf> {
    let files = archives(dir);
    if let Some(want) = filename.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(hit) = files.iter().find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(want))
        }) {
            return Some(hit.clone());
        }
    }
    let want_name = crate::sync::canonical_dist_name(name);
    let mut fallback: Option<PathBuf> = None;
    let mut candidates = 0usize;
    for path in &files {
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !fname.to_ascii_lowercase().ends_with(".whl") {
            continue;
        }
        let (pkg, ver, _) = wheel_name_parts(fname);
        if crate::sync::canonical_dist_name(&pkg) != want_name {
            continue;
        }
        if ver.eq_ignore_ascii_case(version) {
            return Some(path.clone());
        }
        candidates += 1;
        fallback = Some(path.clone());
    }
    if candidates == 1 {
        fallback
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("comfyfleet-wheels-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"wheel").unwrap();
        path
    }

    #[test]
    fn parses_wheel_filename() {
        let (name, version, tags) = wheel_name_parts("my-pkg-1.2.3-cp313-cp313-win_amd64.whl");
        assert_eq!(name, "my-pkg");
        assert_eq!(version, "1.2.3");
        assert_eq!(tags, ["cp313", "cp313", "win_amd64"]);
        // 带构建号的写法不能把构建号当版本。
        let (name, version, _) = wheel_name_parts("my-pkg-1.2.3-1-cp313-cp313-win_amd64.whl");
        assert_eq!(name, "my-pkg");
        assert_eq!(version, "1.2.3");
    }

    #[test]
    fn finds_by_filename_then_by_name_and_version() {
        let root = temp_root("find");
        let exact = touch(
            &root,
            "sageattn3-1.0.0+cu130torch2.10.0-cp313-cp313-win_amd64.whl",
        );
        let _ = touch(&root, "other-0.1.0-cp313-cp313-win_amd64.whl");

        let by_name = find(
            &root,
            "sageattn3",
            "1.0.0+cu130torch2.10.0",
            Some("sageattn3-1.0.0+cu130torch2.10.0-cp313-cp313-win_amd64.whl"),
        );
        assert_eq!(by_name, Some(exact.clone()));

        // 文件名对不上（用户改了名），按包名+版本还能命中。
        let by_version = find(&root, "sageattn3", "1.0.0+cu130torch2.10.0", None);
        assert_eq!(by_version, Some(exact));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_ambiguous_versions() {
        let root = temp_root("ambiguous");
        let _ = touch(&root, "flash_attn-2.8.0-cp313-cp313-win_amd64.whl");
        let _ = touch(&root, "flash_attn-2.8.4-cp313-cp313-win_amd64.whl");
        // 版本没有精确命中、库里又有两个构建：不能猜。
        assert!(find(&root, "flash-attn", "2.8.2", None).is_none());
        // 精确命中不受影响。
        assert!(find(&root, "flash-attn", "2.8.4", None).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_library_is_reported_without_error() {
        let settings = GlobalSettings {
            wheels: None,
            ..GlobalSettings::default()
        };
        let info = info(&settings);
        assert!(!info.configured);
        assert_eq!(info.count, 0);
        assert!(dir(&settings).is_none());
    }

    #[test]
    fn switching_library_moves_nested_archives() {
        let root = temp_root("migrate");
        let old = root.join("old");
        let new = root.join("new");
        let nested = touch(
            &old,
            "cuda13/demo-1.0.0-cp313-cp313-win_amd64.whl",
        );
        let top = touch(&old, "top-1.0.0-py3-none-any.whl");
        std::fs::write(old.join("readme.txt"), b"keep").unwrap();

        let report = migrate_library(&old, &new).unwrap();

        assert_eq!(report.moved(), 2);
        assert_eq!(report.skipped(), 0);
        assert!(!nested.exists());
        assert!(!top.exists());
        assert!(new
            .join("cuda13/demo-1.0.0-cp313-cp313-win_amd64.whl")
            .is_file());
        assert!(new.join("top-1.0.0-py3-none-any.whl").is_file());
        assert!(!new.join("readme.txt").exists());
        assert!(old.join("readme.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migration_skips_identical_file_already_in_target() {
        let root = temp_root("migrate-same");
        let old = root.join("old");
        let new = root.join("new");
        let src = touch(&old, "demo-1.0.0-py3-none-any.whl");
        let dst = touch(&new, "demo-1.0.0-py3-none-any.whl");

        let report = migrate_library(&old, &new).unwrap();

        assert_eq!(report.moved(), 0);
        assert_eq!(report.skipped(), 1);
        assert!(src.exists(), "相同文件保留旧副本，不误删");
        assert!(dst.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migration_rolls_back_when_target_has_different_file() {
        let root = temp_root("migrate-conflict");
        let old = root.join("old");
        let new = root.join("new");
        let a = touch(&old, "a-1.0.0-py3-none-any.whl");
        let b = touch(&old, "b-1.0.0-py3-none-any.whl");
        std::fs::create_dir_all(&new).unwrap();
        let target_b = new.join("b-1.0.0-py3-none-any.whl");
        std::fs::write(&target_b, b"different").unwrap();

        let err = migrate_library(&old, &new).unwrap_err();

        assert_eq!(err.code, "WHEEL_MIGRATE");
        assert!(a.is_file() && b.is_file(), "冲突后已移动的文件要回滚");
        assert!(!new.join("a-1.0.0-py3-none-any.whl").exists());
        assert_eq!(std::fs::read(&target_b).unwrap(), b"different");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn info_counts_nested_wheels_only() {
        let root = temp_root("info");
        touch(&root, "a-1.0.0-cp313-cp313-win_amd64.whl");
        touch(&root, "cuda13/b-2.0.0-cp313-cp313-win_amd64.whl");
        std::fs::write(root.join("readme.txt"), b"x").unwrap();
        let settings = GlobalSettings {
            wheels: Some(root.to_string_lossy().to_string()),
            ..GlobalSettings::default()
        };
        let info = info(&settings);
        assert_eq!(info.count, 2);
        assert!(info.configured && info.exists);
        let _ = std::fs::remove_dir_all(&root);
    }
}
