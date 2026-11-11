//! 引擎矩阵：把「哪个引擎配哪个 torch / Python」变成可远端更新的数据，而不是写死在代码里。
//!
//! 为什么必须按引擎分别记 torch 版本：实测 `cu129` 索引停在 torch 2.9.0，
//! `cu130` 已经到 2.14.1。用一个全局版本号会直接装出不存在的东西。

use crate::error::AppError;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineDef {
    pub id: String,
    pub label: String,
    pub vendor: String,
    /// 该引擎要求的 CUDA 版本；CPU 为 null
    pub cuda_version: Option<String>,
    pub torch_version: String,
    pub index_tag: String,
    /// 该引擎下可用的 Python 版本（来自索引的 cp 标签实测）
    pub python_support: Vec<String>,
    pub launch_args: Vec<String>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonDef {
    pub version: String,
    /// verified | supported | untested
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Matrix {
    pub version: u32,
    pub updated_at: String,
    pub torch_index_base: String,
    pub pythons: Vec<PythonDef>,
    pub engines: Vec<EngineDef>,
}

const RAW: &str = include_str!("../engine-matrix.json");

pub fn load() -> Result<Matrix, AppError> {
    serde_json::from_str(RAW).map_err(|e| {
        AppError::new(
            "MATRIX_INVALID",
            format!("engine-matrix.json 解析失败：{e}"),
        )
    })
}

/// 比较 `13.0` / `12.9` 这样的版本号：driver >= required
pub fn cuda_at_least(driver: &str, required: &str) -> bool {
    fn parts(v: &str) -> (u32, u32) {
        let mut it = v.trim().split('.');
        let major = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor)
    }
    parts(driver) >= parts(required)
}

/// 引擎的完整 torch 索引地址
pub fn torch_index_url(matrix: &Matrix, engine: &EngineDef) -> String {
    format!(
        "{}/{}/",
        matrix.torch_index_base.trim_end_matches('/'),
        engine.index_tag
    )
}

/// 用户只填镜像根地址（如 `https://mirrors.aliyun.com/pytorch-wheels`），
/// 这里按当前引擎自动补 `cu130` / `cpu` 目录。
///
/// 如果用户把完整地址（含旧 tag）也粘进来，会替换掉最后一段 tag，保证跟随
/// 当前选择，而不是被 URL 里的旧 CUDA 版本锁死。
pub fn torch_index_url_with_mirror(
    matrix: &Matrix,
    engine: &EngineDef,
    mirror: Option<&str>,
) -> String {
    let Some(mirror) = mirror.map(str::trim).filter(|s| !s.is_empty()) else {
        return torch_index_url(matrix, engine);
    };
    // 兼容上一版已经保存的 `{tag}` 配置，新界面不再暴露这个占位符。
    if mirror.contains("{tag}") {
        let url = mirror.replace("{tag}", &engine.index_tag);
        return format!("{}/", url.trim_end_matches('/'));
    }

    let base = mirror.trim_end_matches('/');
    let last = base.rsplit('/').next().unwrap_or("");
    let looks_like_tag = last == "cpu"
        || last
            .strip_prefix("cu")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
    let root = if looks_like_tag {
        base.rsplit_once('/').map(|(root, _)| root).unwrap_or(base)
    } else {
        base
    };
    format!("{}/{}/", root.trim_end_matches('/'), engine.index_tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_parses_and_is_consistent() {
        let m = load().expect("matrix should parse");
        assert!(!m.engines.is_empty());
        let mut ids = std::collections::HashSet::new();
        for e in &m.engines {
            assert!(ids.insert(e.id.clone()), "duplicate engine id {}", e.id);
            assert!(!e.torch_version.is_empty());
            // 界面会把每个档的 torch 版本展示出来，必须都能查到全家桶对照
            assert!(
                crate::compat::stack_for(&e.torch_version).is_some(),
                "engine {} 的 torch {} 没有 torchvision/torchaudio/torchcodec 对照",
                e.id,
                e.torch_version
            );
            assert!(!e.python_support.is_empty());
            // 每个引擎声明的 Python 都必须在全局列表里
            for p in &e.python_support {
                assert!(
                    m.pythons.iter().any(|x| &x.version == p),
                    "engine {} references unknown python {}",
                    e.id,
                    p
                );
            }
        }
    }

    #[test]
    fn cuda_comparison() {
        assert!(cuda_at_least("13.0", "12.9"));
        assert!(cuda_at_least("12.9", "12.9"));
        assert!(!cuda_at_least("12.8", "12.9"));
        assert!(cuda_at_least("13.1", "13.0"));
    }

    #[test]
    fn torch_mirror_uses_engine_tag() {
        let m = load().expect("matrix should parse");
        let engine = m.engines.first().expect("engine");
        let official = torch_index_url(&m, engine);
        assert_eq!(torch_index_url_with_mirror(&m, engine, None), official);
        assert_eq!(
            torch_index_url_with_mirror(&m, engine, Some("  ")),
            official
        );
        let base = format!(
            "https://mirrors.aliyun.com/pytorch-wheels/{}/",
            engine.index_tag
        );
        assert_eq!(
            torch_index_url_with_mirror(
                &m,
                engine,
                Some("https://mirrors.aliyun.com/pytorch-wheels")
            ),
            base
        );
        assert_eq!(
            torch_index_url_with_mirror(
                &m,
                engine,
                Some("https://mirrors.aliyun.com/pytorch-wheels/")
            ),
            base
        );
        // 完整地址里的旧 tag 也要被当前引擎替换，不能装错 CUDA 版本。
        assert_eq!(
            torch_index_url_with_mirror(&m, engine, Some("https://mirror.example/pytorch/cu130/")),
            format!("https://mirror.example/pytorch/{}/", engine.index_tag)
        );
        // 兼容上一版保存的 `{tag}` 配置。
        assert_eq!(
            torch_index_url_with_mirror(
                &m,
                engine,
                Some("https://mirrors.aliyun.com/pytorch-wheels/{tag}")
            ),
            base
        );
    }
}
