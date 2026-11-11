//! 云端 wheel 仓库索引。
//!
//! 目前支持 ModelScope 模型仓库的 `<包名>/<版本>/<wheel>` 布局：
//! - 文件列表从 ModelScope 的文件树 API 读取；
//! - 原始列表持久化到数据目录，只有首次或手动刷新才访问网络；
//! - 每次打开实例页只按当前实例的 Python / torch / CUDA 重新标记兼容性，
//!   不重复查询仓库。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::accel::{self, EnvReport};
use crate::commands::version::VersionEvent;
use crate::config::{self, GlobalSettings};
use crate::error::AppError;
use crate::runtime;

const CACHE_FILE: &str = "wheel-repo-cache.json";
const MIN_WHEEL_BYTES: u64 = 10 * 1024;
const MAX_CACHED_WHEELS: usize = 20_000;
const UA: &str = "ComfyFleet/0.1 (+https://github.com/)";

/// 缓存在本地磁盘上的原始文件记录。兼容性不缓存，因为同一个仓库会被不同实例使用。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WheelRepoCache {
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub revision: String,
    pub fetched_at: i64,
    pub wheels: Vec<RepoWheelFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RepoWheelFile {
    pub name: String,
    pub path: String,
    pub group: String,
    pub version: String,
    pub size: u64,
    pub sha256: String,
    pub url: String,
}

/// 给界面的单个 wheel。`compatible` / `label` 是按当前实例环境实时算出来的；
/// `python_tag` / `abi_tag` / `torch_tag` / `cuda_tag` 是文件名里的原始标签，供界面筛选。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoWheel {
    pub name: String,
    pub path: String,
    pub group: String,
    pub version: String,
    pub size: u64,
    pub url: String,
    pub label: String,
    pub note: Option<String>,
    pub compatible: bool,
    pub reasons: Vec<String>,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
    pub torch_tag: Option<String>,
    pub cuda_tag: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WheelRepoList {
    pub repo_url: String,
    /// 没有配置仓库时没有实例环境可读，返回 null。
    pub env: Option<EnvReport>,
    pub fetched_at: i64,
    pub from_cache: bool,
    /// 刷新失败但本地有缓存时保留缓存，并把错误带给界面。
    pub error: Option<String>,
    pub total: usize,
    pub compatible: usize,
    pub wheels: Vec<RepoWheel>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone)]
struct RepoRef {
    owner: String,
    repo: String,
    base_url: String,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(rename = "Code")]
    code: Option<i64>,
    #[serde(rename = "Message")]
    message: Option<String>,
    #[serde(rename = "Data")]
    data: Option<ApiData>,
}

#[derive(Debug, Deserialize)]
struct ApiData {
    #[serde(rename = "Files", default)]
    files: Vec<ApiFile>,
}

#[derive(Debug, Deserialize)]
struct ApiFile {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size", default)]
    size: u64,
    #[serde(rename = "Sha256", default)]
    sha256: String,
}

fn cache_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn cache_path() -> PathBuf {
    config::app_root().join(CACHE_FILE)
}

fn load_cache() -> Option<WheelRepoCache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    let cache: WheelRepoCache = serde_json::from_str(&raw).ok()?;
    if cache.url.trim().is_empty() {
        return None;
    }
    Some(cache)
}

fn save_cache(cache: &WheelRepoCache) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(cache)
        .map_err(|e| AppError::new("WHEEL_REPO_CACHE", format!("序列化轮子缓存失败：{e}")))?;
    config::atomic_write(&cache_path(), &bytes)
        .map_err(|e| AppError::new("WHEEL_REPO_CACHE", format!("写入轮子缓存失败：{e}")))
}

/// 解析用户填写的仓库地址。支持完整 ModelScope 模型页地址，也支持 `owner/repo` 简写。
fn parse_repo_url(raw: &str) -> Result<RepoRef, AppError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::new(
            "WHEEL_REPO_EMPTY",
            "还没有填写云端轮子仓库地址",
        ));
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://www.modelscope.cn/models/{raw}")
    };
    let rest = with_scheme
        .strip_prefix("https://www.modelscope.cn/models/")
        .or_else(|| with_scheme.strip_prefix("https://modelscope.cn/models/"))
        .ok_or_else(|| {
            AppError::new(
                "WHEEL_REPO_URL",
                "目前只支持 ModelScope 模型仓库地址，例如 https://www.modelscope.cn/models/<owner>/<repo>",
            )
        })?;
    // 允许用户直接粘模型页里的 /files /summary 子路径，只取 owner/repo。
    let mut parts = rest
        .split(['/', '?', '#'])
        .filter(|part| !part.trim().is_empty());
    let owner = parts.next().unwrap_or("").to_string();
    let repo = parts.next().unwrap_or("").to_string();
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 120
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(&owner) || !valid(&repo) {
        return Err(AppError::new(
            "WHEEL_REPO_URL",
            "ModelScope 仓库地址里缺少合法的 owner/repo",
        ));
    }
    let base_url = format!("https://www.modelscope.cn/models/{owner}/{repo}");
    let api_url = format!(
        "https://www.modelscope.cn/api/v1/models/{owner}/{repo}/repo/files?Revision=master&Recursive=True"
    );
    Ok(RepoRef {
        owner,
        repo,
        base_url,
        api_url,
    })
}

fn http_text(url: &str) -> Result<String, AppError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(UA)
        .build();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => {
            AppError::new("WHEEL_REPO_HTTP", format!("读取轮子仓库失败：HTTP {code}"))
        }
        other => AppError::new("WHEEL_REPO_HTTP", format!("读取轮子仓库失败：{other}")),
    })?;
    resp.into_string()
        .map_err(|e| AppError::new("WHEEL_REPO_HTTP", format!("读取轮子仓库响应失败：{e}")))
}

fn fetch_cache(repo: &RepoRef) -> Result<WheelRepoCache, AppError> {
    let body = http_text(&repo.api_url)?;
    let parsed: ApiResponse = serde_json::from_str(&body).map_err(|e| {
        AppError::new(
            "WHEEL_REPO_JSON",
            format!("ModelScope 文件列表返回的不是 JSON：{e}"),
        )
    })?;
    if let Some(code) = parsed.code {
        if code != 200 {
            return Err(AppError::new(
                "WHEEL_REPO_API",
                format!(
                    "ModelScope 返回错误 {code}：{}",
                    parsed.message.unwrap_or_default()
                ),
            ));
        }
    }
    let data = parsed
        .data
        .ok_or_else(|| AppError::new("WHEEL_REPO_API", "ModelScope 文件列表缺少 Data"))?;
    let mut wheels = BTreeMap::<String, RepoWheelFile>::new();
    for file in data.files {
        if file.kind != "blob" {
            continue;
        }
        let lower = file.path.to_ascii_lowercase();
        if !lower.ends_with(".whl") {
            continue;
        }
        if file.size > 0 && file.size < MIN_WHEEL_BYTES {
            continue;
        }
        let Some((group, version, name)) = wheel_parts(&file.path) else {
            continue;
        };
        let group = group.to_string();
        let version = version.to_string();
        let name = name.to_string();
        let url = format!(
            "{}/resolve/master/{}",
            repo.base_url,
            encode_path(&file.path)
        );
        wheels.entry(file.path.clone()).or_insert(RepoWheelFile {
            name,
            path: file.path,
            group,
            version,
            size: file.size,
            sha256: file.sha256,
            url,
        });
    }
    let mut wheels: Vec<RepoWheelFile> = wheels.into_values().collect();
    wheels.sort_by(|a, b| {
        a.group
            .to_lowercase()
            .cmp(&b.group.to_lowercase())
            .then_with(|| a.version.to_lowercase().cmp(&b.version.to_lowercase()))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    wheels.truncate(MAX_CACHED_WHEELS);
    Ok(WheelRepoCache {
        url: repo.base_url.clone(),
        owner: repo.owner.clone(),
        repo: repo.repo.clone(),
        revision: "master".to_string(),
        fetched_at: chrono::Utc::now().timestamp(),
        wheels,
    })
}

fn wheel_parts(path: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 3
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return None;
    }
    let name = *parts.last()?;
    if name.len() > 260 {
        return None;
    }
    Some((parts[0], parts[1], name))
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn render(
    cache: WheelRepoCache,
    env: Option<EnvReport>,
    from_cache: bool,
    error: Option<String>,
) -> WheelRepoList {
    let mut wheels = Vec::with_capacity(cache.wheels.len());
    let mut compatible_count = 0usize;
    for file in &cache.wheels {
        let (compatible, label, note, reasons) = match env.as_ref() {
            Some(env) => match accel::wheel_compat(&file.name, env) {
                Ok(m) => (true, m.label, m.note, Vec::new()),
                Err(reason) => (
                    false,
                    format!(
                        "Python {} · torch {} · CUDA {}",
                        env.python,
                        env.torch.as_deref().unwrap_or("—"),
                        env.torch_cuda.as_deref().unwrap_or("—")
                    ),
                    None,
                    vec![reason.to_string()],
                ),
            },
            None => (
                false,
                "未读取实例环境".to_string(),
                None,
                vec!["没有实例环境".to_string()],
            ),
        };
        if compatible {
            compatible_count += 1;
        }
        let tags = accel::parse_wheel_name(&file.name);
        wheels.push(RepoWheel {
            name: file.name.clone(),
            path: file.path.clone(),
            group: file.group.clone(),
            version: file.version.clone(),
            size: file.size,
            url: file.url.clone(),
            label,
            note,
            compatible,
            reasons,
            python_tag: tags.as_ref().map(|t| t.python.clone()).unwrap_or_default(),
            abi_tag: tags.as_ref().map(|t| t.abi.clone()).unwrap_or_default(),
            platform_tag: tags
                .as_ref()
                .map(|t| t.platform.clone())
                .unwrap_or_default(),
            torch_tag: tags.as_ref().and_then(|t| t.torch.clone()),
            cuda_tag: tags.as_ref().and_then(|t| t.cuda.clone()),
        });
    }
    let total = wheels.len();
    let hint = if cache.wheels.is_empty() {
        Some("仓库里还没有 .whl 文件（或者文件列表接口返回为空）。".to_string())
    } else if compatible_count == 0 {
        Some(format!(
            "仓库里有 {total} 个 wheel，但没有和当前实例 Python / torch / CUDA 匹配的版本。"
        ))
    } else {
        None
    };
    WheelRepoList {
        repo_url: cache.url,
        env,
        fetched_at: cache.fetched_at,
        from_cache,
        error,
        total,
        compatible: compatible_count,
        wheels,
        hint,
    }
}

fn empty_list(repo_url: &str) -> WheelRepoList {
    WheelRepoList {
        repo_url: repo_url.to_string(),
        env: None,
        fetched_at: 0,
        from_cache: false,
        error: None,
        total: 0,
        compatible: 0,
        wheels: Vec::new(),
        hint: Some(
            "先在「全局设置 → 网络与镜像」填写云端轮子仓库地址，例如 https://www.modelscope.cn/models/<owner>/<repo>"
                .to_string(),
        ),
    }
}

/// 读取仓库列表。`refresh=false` 时优先用本地缓存；没有缓存才访问网络。
pub fn list(
    inst: &crate::config::InstanceConfig,
    repo_url: Option<&str>,
    refresh: bool,
) -> Result<WheelRepoList, AppError> {
    let raw = repo_url.unwrap_or("").trim();
    if raw.is_empty() {
        return Ok(empty_list(""));
    }
    let repo = parse_repo_url(raw)?;
    let py = runtime::resolve_python(inst).ok_or_else(|| {
        AppError::new(
            "NO_PYTHON",
            "这个实例还没认出 Python 环境（设置 → Python 环境里指一下），没法按环境筛选轮子。",
        )
    })?;
    let env = accel::inspect(&py.python)?;

    let _guard = cache_mutex().lock().unwrap();
    let cached = load_cache().filter(|cache| cache.url == repo.base_url);
    if !refresh {
        if let Some(cache) = cached {
            return Ok(render(cache, Some(env), true, None));
        }
    }
    match fetch_cache(&repo) {
        Ok(cache) => {
            if let Err(e) = save_cache(&cache) {
                log::warn!("轮子仓库缓存写入失败：{}", e.message);
            }
            Ok(render(cache, Some(env), false, None))
        }
        Err(e) => {
            if let Some(cache) = cached {
                Ok(render(cache, Some(env), true, Some(e.message)))
            } else {
                Err(e)
            }
        }
    }
}

fn wheel_distribution(file_name: &str) -> String {
    file_name
        .split('-')
        .next()
        .unwrap_or(file_name)
        .replace('_', "-")
}

/// 下载时严格使用仓库里的原始文件名，不做重命名、不加序号。
fn download_file_name(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || !name.to_ascii_lowercase().ends_with(".whl")
    {
        return Err(AppError::new(
            "WHEEL_REPO_DOWNLOAD_NAME",
            "仓库返回的 wheel 文件名不合法，无法保持原文件名保存",
        ));
    }
    Ok(name)
}

fn http_download(url: &str, dest: &Path) -> Result<(), AppError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .user_agent(UA)
        .build();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => {
            AppError::new("WHEEL_REPO_HTTP", format!("下载轮子失败：HTTP {code}"))
        }
        other => AppError::new("WHEEL_REPO_HTTP", format!("下载轮子失败：{other}")),
    })?;
    let part = dest.with_extension("whl.part");
    let mut file = std::fs::File::create(&part).map_err(|e| {
        AppError::new(
            "WHEEL_REPO_DOWNLOAD",
            format!("创建下载文件失败（{}）：{e}", part.display()),
        )
    })?;
    let copied = std::io::copy(&mut resp.into_reader(), &mut file).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        AppError::new("WHEEL_REPO_DOWNLOAD", format!("下载轮子失败：{e}"))
    })?;
    file.flush().ok();
    drop(file);
    if copied == 0 {
        let _ = std::fs::remove_file(&part);
        return Err(AppError::new("WHEEL_REPO_DOWNLOAD", "下载到的文件是空的"));
    }
    // 文件名必须保持原样，所以同名文件只能覆盖；先删旧文件，Windows 的 rename 不会自动覆盖。
    if dest.exists() {
        std::fs::remove_file(dest).map_err(|e| {
            let _ = std::fs::remove_file(&part);
            AppError::new(
                "WHEEL_REPO_DOWNLOAD",
                format!("覆盖同名轮子失败（{}）：{e}", dest.display()),
            )
        })?;
    }
    std::fs::rename(&part, dest).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        AppError::new(
            "WHEEL_REPO_DOWNLOAD",
            format!("保存轮子失败（{}）：{e}", dest.display()),
        )
    })?;
    Ok(())
}

/// 把一个仓库里的 wheel 保存到用户选择的文件夹，文件名保持仓库里的原名。
///
/// 不经过实例环境，也不改 venv；用户拿到的就是一个可以直接双击查看 / 手动安装的 `.whl`。
pub fn download(repo_url: Option<&str>, path: &str, dest_dir: &str) -> Result<String, AppError> {
    let raw = repo_url.unwrap_or("").trim();
    if raw.is_empty() {
        return Err(AppError::new(
            "WHEEL_REPO_EMPTY",
            "还没有填写云端轮子仓库地址",
        ));
    }
    let dest_dir = dest_dir.trim();
    if dest_dir.is_empty() {
        return Err(AppError::new(
            "WHEEL_REPO_DEST",
            "没有选择保存文件夹，已取消下载",
        ));
    }
    let repo = parse_repo_url(raw)?;
    let cache = load_cache()
        .filter(|cache| cache.url == repo.base_url)
        .ok_or_else(|| {
            AppError::new(
                "WHEEL_REPO_CACHE",
                "本地还没有这个仓库的列表，先刷新仓库再下载",
            )
        })?;
    let wheel = cache
        .wheels
        .iter()
        .find(|wheel| wheel.path == path)
        .ok_or_else(|| {
            AppError::new(
                "WHEEL_REPO_PATH",
                "这个 wheel 不在当前仓库缓存里，先刷新仓库列表",
            )
        })?;
    let name = download_file_name(&wheel.name)?;
    let dir = PathBuf::from(dest_dir);
    std::fs::create_dir_all(&dir).map_err(|e| {
        AppError::new(
            "WHEEL_REPO_DOWNLOAD",
            format!("创建保存文件夹失败（{}）：{e}", dir.display()),
        )
    })?;
    let dest = dir.join(name);
    http_download(&wheel.url, &dest)?;
    Ok(dest.to_string_lossy().to_string())
}

/// 安装仓库里的一个 wheel。只接受当前缓存里存在的路径，避免把任意 URL 传进 uv。
pub fn install(
    inst: &crate::config::InstanceConfig,
    repo_url: Option<&str>,
    path: &str,
    settings: &GlobalSettings,
    on: &Channel<VersionEvent>,
) -> Result<EnvReport, AppError> {
    let raw = repo_url.unwrap_or("").trim();
    if raw.is_empty() {
        return Err(AppError::new(
            "WHEEL_REPO_EMPTY",
            "还没有填写云端轮子仓库地址",
        ));
    }
    let repo = parse_repo_url(raw)?;
    let _guard = cache_mutex().lock().unwrap();
    let cache = load_cache()
        .filter(|cache| cache.url == repo.base_url)
        .ok_or_else(|| {
            AppError::new(
                "WHEEL_REPO_CACHE",
                "本地还没有这个仓库的列表，先刷新仓库再安装",
            )
        })?;
    let wheel = cache
        .wheels
        .iter()
        .find(|wheel| wheel.path == path)
        .ok_or_else(|| {
            AppError::new(
                "WHEEL_REPO_PATH",
                "这个 wheel 不在当前仓库缓存里，先刷新仓库列表",
            )
        })?;
    let name = if wheel.group.trim().is_empty() {
        wheel.name.clone()
    } else {
        wheel.group.clone()
    };
    let pypi = wheel_distribution(&wheel.name);
    drop(_guard);

    accel::install_wheel(inst, &name, &pypi, &wheel.url, settings, on)?;
    let py = runtime::resolve_python(inst).ok_or_else(|| {
        AppError::new("NO_PYTHON", "安装完成但没能重新找到这个实例的 Python 环境")
    })?;
    accel::inspect(&py.python)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modelscope_repo_urls() {
        let repo =
            parse_repo_url("https://www.modelscope.cn/models/demo/wheels").expect("full url");
        assert_eq!(repo.owner, "demo");
        assert_eq!(repo.repo, "wheels");
        assert_eq!(
            repo.api_url,
            "https://www.modelscope.cn/api/v1/models/demo/wheels/repo/files?Revision=master&Recursive=True"
        );

        let shorthand = parse_repo_url("demo/wheels").expect("shorthand");
        assert_eq!(shorthand.base_url, repo.base_url);
        assert!(parse_repo_url("https://example.com/x/y").is_err());
    }

    #[test]
    fn encodes_repo_paths_without_losing_spaces_or_plus() {
        assert_eq!(
            encode_path("Flash Attention/2.8.4/flash_attn-1+cu130.whl"),
            "Flash%20Attention/2.8.4/flash_attn-1%2Bcu130.whl"
        );
    }

    #[test]
    fn collects_only_whl_blobs_in_wheel_version_directories() {
        let body = r#"{
          "Code": 200,
          "Data": { "Files": [
            {"Type":"tree","Path":"Flash Attention","Size":0},
            {"Type":"tree","Path":"Flash Attention/2.8.4","Size":0},
            {"Type":"blob","Path":"Flash Attention/2.8.4/flash_attn-2.8.4+cu130torch2.10-cp313-cp313-win_amd64.whl","Size":100000,"Sha256":"abc"},
            {"Type":"blob","Path":"README.md","Size":1000}
          ]}
        }"#;
        let parsed: ApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.code, Some(200));
        let files = parsed.data.unwrap().files;
        assert_eq!(files.len(), 4);
        let whl = files.iter().find(|f| f.path.ends_with(".whl")).unwrap();
        assert_eq!(whl.size, 100000);
        let (group, version, name) = wheel_parts(&whl.path).unwrap();
        assert_eq!(group, "Flash Attention");
        assert_eq!(version, "2.8.4");
        assert!(name.starts_with("flash_attn-2.8.4+cu130"));
    }

    #[test]
    fn download_keeps_repository_filename() {
        assert_eq!(
            download_file_name("wheel-1.0+cu130torch2.10-cp313-cp313-win_amd64.whl").unwrap(),
            "wheel-1.0+cu130torch2.10-cp313-cp313-win_amd64.whl"
        );
        assert!(download_file_name("../escape.whl").is_err());
        assert!(download_file_name("nested/wheel.whl").is_err());
        assert!(download_file_name("wheel-1.0").is_err());
    }
}
