//! 装包源的选择：先用配置的镜像，装不上再退回官方 PyPI。
//!
//! 镜像同步滞后是常态（新版本包晚几小时甚至几天才同步），
//! 但用户看到的不该是「依赖装不上」这种无解结论 —— 所以凡是按镜像装 requirements
//! 的地方都走这里拿候选源，失败自动换官方源重试一次。

pub const OFFICIAL_PYPI: &str = "https://pypi.org/simple";

/// 候选 index 列表：配置了非官方镜像 → [镜像, 官方]；否则只有官方。
pub fn index_candidates(configured: Option<&str>) -> Vec<String> {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(mirror) if !same_index(mirror, OFFICIAL_PYPI) => {
            vec![mirror.to_string(), OFFICIAL_PYPI.to_string()]
        }
        _ => vec![OFFICIAL_PYPI.to_string()],
    }
}

/// 镜像地址常带/不带结尾斜杠、大小写不一：按「去掉尾斜杠 + 忽略大小写」比
fn same_index(a: &str, b: &str) -> bool {
    a.trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_falls_back_to_official() {
        let got = index_candidates(Some("https://mirrors.aliyun.com/pypi/simple/"));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "https://mirrors.aliyun.com/pypi/simple/");
        assert_eq!(got[1], OFFICIAL_PYPI);
    }

    #[test]
    fn official_or_empty_needs_no_fallback() {
        assert_eq!(index_candidates(None), vec![OFFICIAL_PYPI]);
        assert_eq!(index_candidates(Some("   ")), vec![OFFICIAL_PYPI]);
        assert_eq!(index_candidates(Some(OFFICIAL_PYPI)), vec![OFFICIAL_PYPI]);
        assert_eq!(
            index_candidates(Some("https://pypi.org/simple/")),
            vec![OFFICIAL_PYPI]
        );
        assert_eq!(
            index_candidates(Some("HTTPS://PyPI.org/simple")),
            vec![OFFICIAL_PYPI]
        );
    }
}
