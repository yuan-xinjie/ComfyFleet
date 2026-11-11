//! torch 全家桶版本对照：选了 torch 之后，torchvision / torchaudio / torchcodec 只能装这几版。
//!
//! 数据来自官方发布关系（PyTorch previous-versions 页面 + torchaudio / torchcodec README）：
//! - torchvision 的每个补丁版都精确 `torch==` 到对应 torch 补丁版，必须成对；
//! - torchaudio 从 2.11 起进入维护模式，2.11.1 声明 `torch>=2.11`，兼容之后所有 torch；
//! - torchcodec 从 0.12 起声明 `torch>=2.11`，2.12+ 统一用 0.17.0。
//!
//! 这张表有两个用途：
//! 1. 装环境时直接按表安装，不让 uv 自己解析出「最新但不匹配」的版本；
//! 2. 装 requirements / 节点依赖时写成 constraints，任何包想动 torch 全家都会报错。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackPin {
    pub torch: String,
    pub torchvision: String,
    pub torchaudio: String,
    pub torchcodec: String,
}

/// 第一列：torch（只写两段表示「该 minor 下所有补丁版」）；后三列：对应版本。
const STACKS: &[(&str, &str, &str, &str)] = &[
    ("2.8", "0.23.0", "2.8.0", "0.7.0"),
    ("2.9.0", "0.24.0", "2.9.0", "0.9.1"),
    ("2.9.1", "0.24.1", "2.9.1", "0.9.1"),
    ("2.10", "0.25.0", "2.10.0", "0.10.0"),
    ("2.11", "0.26.0", "2.11.0", "0.11.1"),
    ("2.12.0", "0.27.0", "2.11.1", "0.17.0"),
    ("2.12.1", "0.27.1", "2.11.1", "0.17.0"),
    ("2.13", "0.28.0", "2.11.1", "0.17.0"),
    ("2.14.0", "0.29.0", "2.11.1", "0.17.0"),
    ("2.14.1", "0.29.1", "2.11.1", "0.17.0"),
    ("2.14", "0.29.1", "2.11.1", "0.17.0"),
];

/// 去掉 `+cu130` 这类本地版本后缀，只留发布版本。
fn base_version(torch: &str) -> &str {
    torch.trim().split('+').next().unwrap_or("").trim()
}

fn minor_of(v: &str) -> String {
    let mut it = v.split('.');
    let major = it.next().unwrap_or("");
    let minor = it.next().unwrap_or("");
    format!("{major}.{minor}")
}

/// 查 torch 对应的全家桶版本；表里没有就返回 `None`（调用方应提示而不是瞎猜）。
pub fn stack_for(torch: &str) -> Option<StackPin> {
    let v = base_version(torch);
    if v.is_empty() {
        return None;
    }
    // 先精确匹配补丁版（2.9.1 和 2.9.0 的 torchvision 不同，不能只按 minor）
    let exact = STACKS.iter().find(|(key, _, _, _)| *key == v);
    let row = exact.or_else(|| {
        let minor = minor_of(v);
        STACKS.iter().find(|(key, _, _, _)| *key == minor)
    })?;
    Some(StackPin {
        torch: v.to_string(),
        torchvision: row.1.to_string(),
        torchaudio: row.2.to_string(),
        torchcodec: row.3.to_string(),
    })
}

/// PyTorch minor → Triton minor（Triton 3.x）。只列官方对照表里有的版本，
/// 表外的 torch 不猜，交给调用方提示并跳过。
const TRITON_MINORS: &[(u8, u8, u8)] = &[
    (2, 4, 1),
    (2, 5, 1),
    (2, 6, 2),
    (2, 7, 3),
    (2, 8, 4),
    (2, 9, 5),
    (2, 10, 6),
    (2, 11, 6),
    (2, 12, 7),
    (2, 13, 7),
    (2, 14, 8),
    (2, 15, 9),
];

/// 按 torch 版本给出 Windows 上的 Triton 约束，例如：
/// `2.10.0+cu130` → `triton-windows<3.7`；`2.15.0` → `triton-windows<3.10`。
///
/// 用 `<x.y+1` 而不是 `==3.x`：Triton 的补丁版会跟 torch 补丁版走，锁死 minor
/// 才能让 uv 选到该分支最新、又不会跨到下一代。
pub fn triton_spec(torch: &str) -> Option<String> {
    let v = base_version(torch);
    let mut it = v.split('.');
    let major = it.next()?.parse::<u8>().ok()?;
    let minor = it.next()?.parse::<u8>().ok()?;
    let (_, _, triton) = TRITON_MINORS
        .iter()
        .find(|(tmaj, tmin, _)| *tmaj == major && *tmin == minor)?;
    Some(format!("triton-windows<3.{}", triton + 1))
}

/// 把发布版本和索引 tag 拼成精确版本，例如 `2.10.0` + `cu130` → `2.10.0+cu130`。
///
/// 必须带 local label：`torch==2.10.0` 会被已经装好的 `2.10.0+cpu` 满足，
/// uv 就不会换成 CUDA 轮子；后续 requirements 也可能把它留在 CPU 档。
pub fn tagged_version(version: &str, tag: &str) -> String {
    if version.contains('+') || tag.trim().is_empty() {
        version.to_string()
    } else {
        format!("{version}+{tag}")
    }
}

/// torchaudio 从 2.11.1 起是纯 Python 包（`py3-none-any`，`Requires-Dist: torch>=2.11`），
/// PyTorch 索引里没有 `2.11.1+cuXXX`，强行加 local label 会装不上。
/// 2.11.0 及更早仍是带 CUDA 的二进制包，必须保留 `+cuXXX`。
pub fn tagged_torchaudio(version: &str, tag: &str) -> String {
    if version.contains('+') || version == "2.11.1" {
        version.to_string()
    } else {
        tagged_version(version, tag)
    }
}

/// 生成 uv/pip 的 constraints 内容：四行精确版本。
/// `tag` 为 `cu130` / `cpu` 时，前三行带 local label，防止 requirements 换掉 torch 全家。
pub fn constraints_text(pin: &StackPin, tag: Option<&str>) -> String {
    let with = |version: &str| match tag {
        Some(tag) => tagged_version(version, tag),
        None => version.to_string(),
    };
    let audio = match tag {
        Some(tag) => tagged_torchaudio(&pin.torchaudio, tag),
        None => pin.torchaudio.clone(),
    };
    format!(
        "torch=={}\ntorchvision=={}\ntorchaudio=={}\ntorchcodec=={}\n",
        with(&pin.torch),
        with(&pin.torchvision),
        audio,
        pin.torchcodec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_requested_torch_version() {
        let cases = [
            ("2.8.0", "0.23.0", "2.8.0", "0.7.0"),
            ("2.9.0", "0.24.0", "2.9.0", "0.9.1"),
            ("2.9.1", "0.24.1", "2.9.1", "0.9.1"),
            ("2.10.0", "0.25.0", "2.10.0", "0.10.0"),
            ("2.11.0", "0.26.0", "2.11.0", "0.11.1"),
            ("2.12.0", "0.27.0", "2.11.1", "0.17.0"),
            ("2.12.1", "0.27.1", "2.11.1", "0.17.0"),
            ("2.13.0", "0.28.0", "2.11.1", "0.17.0"),
            ("2.14.0", "0.29.0", "2.11.1", "0.17.0"),
            ("2.14.1", "0.29.1", "2.11.1", "0.17.0"),
        ];
        for (torch, vision, audio, codec) in cases {
            let pin = stack_for(torch).unwrap_or_else(|| panic!("{torch} 应该有对照"));
            assert_eq!(pin.torch, torch);
            assert_eq!(pin.torchvision, vision, "{torch} torchvision");
            assert_eq!(pin.torchaudio, audio, "{torch} torchaudio");
            assert_eq!(pin.torchcodec, codec, "{torch} torchcodec");
        }
    }

    #[test]
    fn strips_cuda_local_suffix() {
        let pin = stack_for("2.14.1+cu130").expect("带 +cu 也要认");
        assert_eq!(pin.torch, "2.14.1");
        assert_eq!(pin.torchvision, "0.29.1");
    }

    #[test]
    fn unknown_version_is_not_guessed() {
        assert!(stack_for("1.13.1").is_none());
        assert!(stack_for("2.9.2").is_none());
        assert!(stack_for("").is_none());
    }

    #[test]
    fn constraints_cover_all_four_packages() {
        let text = constraints_text(&stack_for("2.14.1").unwrap(), None);
        for line in [
            "torch==2.14.1",
            "torchvision==0.29.1",
            "torchaudio==2.11.1",
            "torchcodec==0.17.0",
        ] {
            assert!(text.contains(line), "constraints 缺少 {line}：{text}");
        }
    }

    #[test]
    fn constraints_can_pin_cuda_local_label() {
        let pin = stack_for("2.10.0").unwrap();
        let text = constraints_text(&pin, Some("cu130"));
        for line in [
            "torch==2.10.0+cu130",
            "torchvision==0.25.0+cu130",
            "torchaudio==2.10.0+cu130",
            "torchcodec==0.10.0",
        ] {
            assert!(text.contains(line), "constraints 缺少 {line}：{text}");
        }
        assert_eq!(tagged_version("2.10.0", "cpu"), "2.10.0+cpu");
        assert_eq!(tagged_version("2.10.0+cu130", "cpu"), "2.10.0+cu130");
        assert_eq!(tagged_torchaudio("2.10.0", "cu130"), "2.10.0+cu130");
        assert_eq!(tagged_torchaudio("2.11.1", "cu130"), "2.11.1");
        assert_eq!(tagged_torchaudio("2.11.1", "cu132"), "2.11.1");
    }

    #[test]
    fn maintenance_torchaudio_is_not_local_tagged() {
        let pin = stack_for("2.14.1").unwrap();
        let text = constraints_text(&pin, Some("cu130"));
        assert!(text.contains("torch==2.14.1+cu130"), "{text}");
        assert!(text.contains("torchvision==0.29.1+cu130"), "{text}");
        assert!(text.contains("torchaudio==2.11.1\n"), "{text}");
        assert!(!text.contains("torchaudio==2.11.1+cu130"), "{text}");
    }

    #[test]
    fn maps_triton_for_every_known_torch() {
        let cases = [
            ("2.4.0", "triton-windows<3.2"),
            ("2.5.0+cu121", "triton-windows<3.2"),
            ("2.6.0", "triton-windows<3.3"),
            ("2.7.1", "triton-windows<3.4"),
            ("2.8.0+cu128", "triton-windows<3.5"),
            ("2.9.1", "triton-windows<3.6"),
            ("2.10.0+cu130", "triton-windows<3.7"),
            ("2.11.0", "triton-windows<3.7"),
            ("2.12.1", "triton-windows<3.8"),
            ("2.13.0", "triton-windows<3.8"),
            ("2.14.1+cu130", "triton-windows<3.9"),
            ("2.15.0", "triton-windows<3.10"),
        ];
        for (torch, spec) in cases {
            assert_eq!(triton_spec(torch).as_deref(), Some(spec), "{torch}");
        }
    }

    #[test]
    fn unknown_triton_mapping_is_not_guessed() {
        assert!(triton_spec("2.3.1").is_none());
        assert!(triton_spec("3.0.0").is_none());
        assert!(triton_spec("").is_none());
    }
}
