//! 环境体检（M6）：把环境坏在哪变成可读的结论，而不是让用户猜。
//!
//! 原则（写进产品口径，不要越界承诺）：
//! - 只做**检测/ 解释 / 建议**；不做自动解决依赖冲突"这种保证。
//! - 每条结论都要能落回到一个可复现的证据（pip check 的原始行、torch 的实际输出）。
//! - 检测不到就如实词检测不到，不编造。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, State};

use crate::commands::deps;
use crate::commands::instance::load_instance;
use crate::error::AppError;
use crate::proc;
use crate::state::AppState;

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    /// error | warn | info
    pub level: String,
    pub title: String,
    pub detail: String,
    /// 建议动作（人话，不是按钮，
    pub suggestion: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagReport {
    pub python_version: String,
    pub torch_version: String,
    pub cuda_available: bool,
    pub gpu_name: String,
    pub package_count: u32,
    pub checked_at: String,
    pub issues: Vec<Issue>,
    /// pip check 的原始输出，便于用户贴日志
    pub pip_check: Vec<String>,
}

/// 解析 `pip check` 输出。真实样例：
/// `transformers 5.21.0 has requirement tokenizers<0.22,>=0.21, but you have tokenizers 0.23.0.`
pub fn parse_pip_check(stdout: &str) -> Vec<Issue> {
    stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| !l.eq_ignore_ascii_case("No broken requirements found."))
        .map(|line| Issue {
            level: "error".into(),
            title: "依赖版本冲突".into(),
            detail: line.to_string(),
            suggestion: "Resolve the conflicting dependency versions, then reload.".into(),
        })
        .collect()
}

/// requirements.txt 里提取包名（忽略注释、空行、`-r`/URL 等非包行）。
/// 必须按 `deps::classify` 分类：`git+https://…` 否则会被截成包名 `git`。
#[cfg(test)]
pub fn requirement_names(text: &str) -> Vec<String> {
    requirement_names_with_env(text, None)
}

/// 带环境标记过滤的版本：体检拿得到 Python 版本时，跳过
/// `jetson-stats; platform_machine == 'aarch64'` 这类当前机器不适用的依赖，
/// 否则会把它们误报成“缺失”，诱导用户安装装不上的包。
fn requirement_names_with_env(text: &str, env: Option<&deps::MarkerEnv>) -> Vec<String> {
    text.lines()
        .filter_map(deps::classify)
        .filter(|line| line.kind == deps::LineKind::Package)
        .filter(|line| match env {
            Some(env) => deps::requirement_applies(&line.raw, env) == Some(true),
            None => true,
        })
        .filter_map(|line| line.name)
        .map(|name| name.to_lowercase().replace('_', "-"))
        .collect()
}

/// 解析 `pip list --format=json` 一包名 →版本
pub fn parse_pip_list(json: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return out;
    };
    let Some(arr) = value.as_array() else {
        return out;
    };
    for item in arr {
        let name = item.get("name").and_then(|v| v.as_str());
        let version = item.get("version").and_then(|v| v.as_str());
        if let (Some(name), Some(version)) = (name, version) {
            out.insert(name.to_lowercase().replace('_', "-"), version.to_string());
        }
    }
    out
}

/// 找出 requirements 里声明了、但环境里没装的包（包名统一小写 + 下划线转连字符）
pub fn missing_from(required: &[String], installed: &BTreeMap<String, String>) -> Vec<String> {
    let mut missing: Vec<String> = required
        .iter()
        .map(|name| name.to_lowercase().replace('_', "-"))
        .filter(|name| deps::installed_version(installed, name).is_none())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// python 版本是否被引擎支持（支持列表是字符串，如 "3.13"：
pub fn python_supported(version: &str, support: &[String]) -> Option<bool> {
    if support.is_empty() {
        return None;
    }
    let short: String = version.split('.').take(2).collect::<Vec<_>>().join(".");
    Some(support.iter().any(|s| s == &short || s == version))
}

fn run_python(python: &Path, args: &[&str]) -> Option<String> {
    let env = std::collections::BTreeMap::new();
    proc::run(python, args, None, &env)
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
}

/// 真正的体检（纯函数，方便测试与复用，
pub fn diagnose(
    root: &Path,
    python: Option<&Path>,
    python_support: &[String],
    engine_is_cuda: bool,
) -> DiagReport {
    let mut report = DiagReport {
        checked_at: chrono::Local::now().to_rfc3339(),
        ..Default::default()
    };

    let Some(python) = python else {
        report.issues.push(Issue {
            level: "error".into(),
            title: "找不到Python 环境".into(),
            detail: format!(
                "{} 下没有可用的 Python 环境，venv / venv / python_embeded 都没有）",
                root.display()
            ),
            suggestion: "到实例设置里选一个可用的 Python 环境，或初始化 venv。".into(),
        });
        return report;
    };

    report.python_version = run_python(python, &["-V"])
        .map(|s| s.replace("Python ", "").trim().to_string())
        .unwrap_or_default();

    // torch 探针：一条命令同时拿版本、CUDA 可用性、显卡名
    let torch_probe = run_python(
        python,
        &[
            "-c",
            "import torch,json;print(json.dumps({'v':torch.__version__,'c':bool(torch.cuda.is_available()),'g':(torch.cuda.get_device_name(0) if torch.cuda.is_available() else '')}))",
        ],
    );
    match torch_probe
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        Some(Ok(v)) => {
            report.torch_version = v
                .get("v")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            report.cuda_available = v.get("c").and_then(|x| x.as_bool()).unwrap_or(false);
            report.gpu_name = v
                .get("g")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
        }
        _ => {
            report.issues.push(Issue {
                level: "error".into(),
                title: "torch 导入失败".into(),
                detail: "运行 `python -c \"import torch\"` 没有得到正常输出".into(),
                suggestion:
                    "在实例环境终端里手动执行一次，看具体报错（常见是DLL 缺失或CUDA 版本不匹配）"
                        .into(),
            })
        }
    }

    // pip check：官方给出的"依赖是否自洽"结论，最值得修
    let pip_check = run_python(&python, &["-m", "pip", "check"]);
    if let Some(text) = &pip_check {
        report.pip_check = text.lines().map(|l| l.trim().to_string()).collect();
        report.issues.extend(parse_pip_check(text));
    }

    let installed = run_python(&python, &["-m", "pip", "list", "--format=json"])
        .map(|json| parse_pip_list(&json))
        .unwrap_or_default();
    report.package_count = installed.len() as u32;
    // 拿不到 Python 版本时不过滤 marker，避免把可能适用的依赖漏掉。
    let marker_env = (!report.python_version.is_empty())
        .then(|| deps::MarkerEnv::current(&report.python_version));

    // ComfyUI 本体依赖是否装齐
    let comfy_req = root.join("ComfyUI").join("requirements.txt");
    if let Ok(text) = std::fs::read_to_string(&comfy_req) {
        let missing = missing_from(
            &requirement_names_with_env(&text, marker_env.as_ref()),
            &installed,
        );
        if !missing.is_empty() {
            report.issues.push(Issue {
                level: "warn".into(),
                title: "ComfyUI 依赖缺失".into(),
                detail: format!("requirements.txt 里这些包没装，{}", missing.join(", ")),
                suggestion: "重新跑一次环境安装（会补装缺失项），或手动 pip install".into(),
            });
        }
    }

    // 自定义节点各自的 requirements：冲突的主要来源
    let nodes_dir = root.join("ComfyUI").join("custom_nodes");
    if let Ok(entries) = std::fs::read_dir(&nodes_dir) {
        for entry in entries.flatten() {
            let req = entry.path().join("requirements.txt");
            if !req.exists() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&req) else {
                continue;
            };
            let missing = missing_from(
                &requirement_names_with_env(&text, marker_env.as_ref()),
                &installed,
            );
            if !missing.is_empty() {
                report.issues.push(Issue {
                    level: "warn".into(),
                    title: format!("节点 {} 缺少依赖", entry.file_name().to_string_lossy()),
                    detail: format!("未安装：{}", missing.join(", ")),
                    suggestion: "在「节点」页对该节点重新安装依赖，或进环境终端手动 pip install"
                        .into(),
                });
            }
        }
    }

    // 引擎与实测是否一致
    if engine_is_cuda && !report.cuda_available {
        report.issues.push(Issue {
            level: "error".into(),
            title: "装了 CUDA 版 torch，但 CUDA 不可用".into(),
            detail: format!(
                "torch={}：cuda_available=false（显卡：{}）",
                if report.torch_version.is_empty() {
                    "未知"
                } else {
                    &report.torch_version
                },
                if report.gpu_name.is_empty() {
                    "未检测到"
                } else {
                    &report.gpu_name
                }
            ),
            suggestion: "多为驱动版本低于 torch 要求的 CUDA 运行时；升级驱动，或改用 CPU 引擎"
                .into(),
        });
    }

    if let Some(false) = python_supported(&report.python_version, python_support) {
        report.issues.push(Issue {
            level: "warn".into(),
            title: "Python 版本不在该引擎支持列表内".into(),
            detail: format!(
                "当前 {}，支持：{}",
                report.python_version,
                python_support.join(" / ")
            ),
            suggestion: "能跑就先用；遇到编译失败或缺 wheel 时优先换到已验证的版本".into(),
        });
    }

    report
}

#[tauri::command]
pub async fn diag_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    instance_id: String,
) -> Result<DiagReport, AppError> {
    let _ = app;
    let _ = state;
    let inst = load_instance(&instance_id)?;
    let matrix = crate::matrix::load()?;
    let engine = matrix.engines.iter().find(|e| e.id == inst.engine);
    let support = engine.map(|e| e.python_support.clone()).unwrap_or_default();
    let is_cuda = engine.map(|e| e.cuda_version.is_some()).unwrap_or(false);
    let root = PathBuf::from(&inst.root_dir);
    let python = crate::runtime::resolve_python(&inst).map(|py| py.python);

    // 体检会起几次 python，扔到阻塞线程，别占着 IPC 线程
    tauri::async_runtime::spawn_blocking(move || {
        diagnose(&root, python.as_deref(), &support, is_cuda)
    })
    .await
    .map_err(|e| AppError::new("DIAG_JOIN", format!("诊断线程异常，{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pip_check_output() {
        let real = "transformers 5.21.0 has requirement tokenizers<0.22,>=0.21, but you have tokenizers 0.23.0.\n";
        let issues = parse_pip_check(real);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].level, "error");
        assert!(issues[0].detail.contains("tokenizers"));

        // 干净环境：官方提示不是问题
        assert!(parse_pip_check("No broken requirements found.\n").is_empty());
        assert!(parse_pip_check("\n  \n").is_empty());
    }

    #[test]
    fn extracts_requirement_names() {
        let text = "# comment\nnumpy>=1.25\nPillow\n--index-url https://x\n-r other.txt\nkornia==0.9.0\ngit+https://github.com/a/b.git\nname @ https://x/y.whl\n";
        assert_eq!(
            requirement_names(text),
            vec!["numpy", "pillow", "kornia", "name"]
        );
    }

    #[test]
    fn skips_inapplicable_requirement_markers() {
        let env = deps::MarkerEnv::current("3.13.6");
        let text = "numpy>=1.25\njetson-stats; platform_machine == 'mips'\ndemo; sys_platform == 'plan9'\nunknown; platform_release == 'x'\n";
        assert_eq!(requirement_names_with_env(text, Some(&env)), vec!["numpy"]);
        // 没有 Python 版本时保留旧行为，宁可多报也不漏掉可能适用的包。
        assert_eq!(
            requirement_names_with_env(text, None),
            vec!["numpy", "jetson-stats", "demo", "unknown"]
        );
    }

    #[test]
    fn parses_pip_list_json() {
        let json =
            r#"[{"name":"Torch","version":"2.14.1+cu130"},{"name":"pillow","version":"13.0.0"}]"#;
        let map = parse_pip_list(json);
        assert_eq!(map.get("torch").map(|s| s.as_str()), Some("2.14.1+cu130"));
        assert_eq!(map.get("pillow").map(|s| s.as_str()), Some("13.0.0"));
        // 块JSON 不能让体检整体崩
        assert!(parse_pip_list("not json").is_empty());
    }

    #[test]
    fn finds_missing_packages() {
        let mut installed = BTreeMap::new();
        installed.insert("numpy".to_string(), "2.5.4".to_string());
        let missing = missing_from(
            &["numpy".into(), "kornia".into(), "KORNIA".into()],
            &installed,
        );
        assert_eq!(missing, vec!["kornia"]);
    }

    #[test]
    fn python_support_matching() {
        let support = vec!["3.12".to_string(), "3.13".to_string()];
        assert_eq!(python_supported("3.13.6", &support), Some(true));
        assert_eq!(python_supported("3.11.9", &support), Some(false));
        assert_eq!(python_supported("3.13.6", &[]), None);
    }

    /// 真机体检：`COMFYFLEET_DIAG_ROOT=<实例根目录 cargo test real_diag -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_diag_on_instance() {
        let Ok(root) = std::env::var("COMFYFLEET_DIAG_ROOT") else {
            eprintln!("未设置COMFYFLEET_DIAG_ROOT，跳过");
            return;
        };
        let py = crate::runtime::detect_python(Path::new(&root));
        let report = diagnose(
            Path::new(&root),
            py.as_ref().map(|p| p.python.as_path()),
            &["3.12".into(), "3.13".into()],
            true,
        );
        println!("python={}", report.python_version);
        println!("torch={}", report.torch_version);
        println!("cuda={} gpu={}", report.cuda_available, report.gpu_name);
        println!("包数量{}", report.package_count);
        println!("pip check 行数={}", report.pip_check.len());
        for line in &report.pip_check {
            println!("  {line}");
        }
        println!("问题数{}", report.issues.len());
        for issue in &report.issues {
            println!("  [{}] {} — {}", issue.level, issue.title, issue.detail);
        }
        assert!(!report.python_version.is_empty(), "应读到python 版本");
        assert!(report.package_count > 50, "环境里应有大量包");
    }

    /// 没有 Python 环境的目录要给出明确结论，而不是空报告
    #[test]
    fn diagnose_reports_missing_venv() {
        let root = std::env::temp_dir().join("comfyfleet-diag-empty");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        let report = diagnose(&root, None, &[], false);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].title, "找不到Python 环境");
        assert!(report.issues[0].suggestion.contains("Python 环境"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
