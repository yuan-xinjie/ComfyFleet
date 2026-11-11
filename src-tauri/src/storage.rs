//! 存储：把「全局共享目录 + 实例自己的目录」翻译成 ComfyUI 真正认的东西。
//!
//! ComfyUI 原生只有两种表达方式：
//! - 模型：`extra_model_paths.yaml`（可以很多目录、很多类型，还能把某一项标成默认落点）。
//! - 输入/输出：`--input-directory` / `--output-directory`（各只能一个）
//!
//! 所以这里做三件事：算出生效目录、把 yaml 写成 ComfyUI 认识的形状。
//! 启动前把该建的目录建好（`--extra-model-paths-config` 指向的文件不存在会直接起不来）。
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{DirChoice, GlobalSettings, InstanceConfig, ModelDir};
use crate::error::AppError;

/// yaml 文件放在实例根目录：不进 ComfyUI 的 git 树，跟着实例一起搬。
pub const YAML_NAME: &str = "extra_model_paths.yaml";

/// 一个生效目录。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    pub path: String,
    /// own = 实例自己 | extra = 实例附加 | shared = 全局共享
    pub source: String,
    pub exists: bool,
    /// 只有全局共享模型目录可能标默认。
    pub is_default: bool,
    /// 整包 models 根目录 = None；只装某一类 = Some(类型)。
    #[serde(rename = "type")]
    pub type_name: Option<String>,
}

impl DirEntry {
    fn new(
        path: impl Into<String>,
        source: &str,
        is_default: bool,
        type_name: Option<String>,
    ) -> Self {
        let path = path.into();
        let exists = !path.trim().is_empty() && Path::new(&path).is_dir();
        Self {
            path,
            source: source.to_string(),
            exists,
            is_default,
            type_name,
        }
    }
}

/// 全局共享模型目录在这个实例里的状态：界面用它画勾选框
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedModelOption {
    pub path: String,
    ///  ModelDir / DirEntry 一样用 `type` 这个名字，前端读的是 type
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub is_default: bool,
    /// 这个实例用不用它
    pub applies: bool,
    pub exists: bool,
}

/// 启动前算出来的完整计划，界面直接展示这个（所见即所得）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoragePlan {
    pub models: Vec<DirEntry>,
    pub input: DirEntry,
    pub output: DirEntry,
    pub yaml_path: String,
    pub yaml_text: String,
    /// 全局设置里填的共享输 输出目录（可能还没填）：界面用它显示「共享目录是哪个。
    pub shared_input: Option<String>,
    pub shared_output: Option<String>,
    /// 全局共享模型目录全量（含被这个实例关掉的），界面用勾选框列出。
    pub shared_models: Vec<SharedModelOption>,
    /// 会追加到 main.py 的参。
    pub extra_args: Vec<String>,
    /// 不影响启动但要说一声的问题
    pub warnings: Vec<String>,
}

/// 启动器默认共享模型根目录下会预先建好的类型子目录（和 ComfyUI 自带 models 一致）
const DEFAULT_SHARED_SUBDIRS: [&str; 10] = [
    "checkpoints",
    "diffusion_models",
    "text_encoders",
    "clip",
    "clip_vision",
    "vae",
    "loras",
    "controlnet",
    "upscale_models",
    "embeddings",
];

/// 这个实例是否关掉了某个共享目录（按路径比，忽略大小写与结尾斜杠）
/// 路径比对用的键：忽略大小写与结尾斜杠
fn path_key(path: &str) -> String {
    path.trim().trim_end_matches(['\\', '/']).to_lowercase()
}

pub(crate) fn shared_disabled(inst: &InstanceConfig, path: &str) -> bool {
    let key = path_key(path);
    inst.storage
        .disabled_shared_models
        .iter()
        .any(|p| path_key(p) == key)
}

/// 实例自己的某个目录：`<root>\ComfyUI\<child>`
fn own_dir(inst: &InstanceConfig, child: &str) -> PathBuf {
    PathBuf::from(&inst.root_dir).join("ComfyUI").join(child)
}

fn yaml_path(inst: &InstanceConfig) -> PathBuf {
    PathBuf::from(&inst.root_dir).join(YAML_NAME)
}

/// 一个目录要写进 yaml 的类型条目：类型 -> 绝对路径
fn type_entries(dir: &ModelDir, warnings: &mut Vec<String>) -> Vec<(String, String)> {
    let base = Path::new(dir.path.trim());
    if let Some(kind) = dir
        .type_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![(kind.to_string(), dir.path.trim().to_string())];
    }
    // 「整 models 根目录」：把子目录名当作类型名。ComfyUI 自己会把 unet/clip
    // 这类老名字映射过去，未知名字会被静默忽略，所以扫到什么写什么最稳。
    let mut out = Vec::new();
    let mut names: Vec<String> = Vec::new();
    match std::fs::read_dir(base) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
        Err(_) => {
            warnings.push(format!(
                "{} 不存在或读不了，这次启动会跳过它",
                dir.path.trim()
            ));
            return out;
        }
    }
    names.sort();
    for name in names {
        let full = base.join(&name).to_string_lossy().to_string();
        out.push((name, full));
    }
    if out.is_empty() {
        warnings.push(format!(
            "{} 下面没有类型子目录（checkpoints/loras/…），要么它是空目录，要么应该在「类型」里选上具体类型",
            dir.path.trim()
        ));
    }
    out
}

/// yaml 文本。每个目录一段，`is_default` 只写在下载落点那一段。
///
/// 用单引号而不是双引号：Windows 路径里的 `\` 在双引号里是转义符，
/// 单引号里是普通字符，只有 `'` 需要写成两个。
fn yaml_text(dirs: &[ModelDir], warnings: &mut Vec<String>) -> String {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
    let mut body = String::new();
    let mut index = 0;
    // 同一个「类 + 路径」只写一次：共享了一 models 根目录、又单独把它的某     // 子目录按类型加进来时，根目录那一段已经包含了它，再写一遍纯属重。
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for dir in dirs {
        let path = dir.path.trim();
        if path.is_empty() {
            continue;
        }
        let mut entries = type_entries(dir, warnings);
        entries.retain(|(kind, full)| seen.insert((kind.to_lowercase(), path_key(full))));
        if entries.is_empty() {
            // 全被上面的目录覆盖了；下载落点那一段仍然写出来（ComfyUI 会忽略只             // is_default 的段落），避免「设了默认落点却不生效」的错觉
            if !dir.is_default {
                continue;
            }
        }
        index += 1;
        body.push_str(&format!("\ndir_{index}:\n"));
        if dir.is_default {
            body.push_str("  is_default: true\n");
        }
        for (kind, full) in entries {
            body.push_str(&format!("  {}: {}\n", quote(&kind), quote(&full)));
        }
    }
    if body.is_empty() {
        return String::new();
    }
    format!("# Generated by ComfyFleet; manual edits are overwritten on each start.\n# Structure follows ComfyUI extra_model_paths.yaml.example\n{body}")
}

/// 解析某个输入/输出目录的选择
fn resolve_io(
    choice: &DirChoice,
    shared: Option<&str>,
    fallback: &Path,
    label: &str,
    warnings: &mut Vec<String>,
) -> PathBuf {
    let shared = shared.map(str::trim).filter(|s| !s.is_empty());
    match choice.mode.as_str() {
        "shared" => match shared {
            Some(p) => PathBuf::from(p),
            None => {
                warnings.push(format!(
                    "这个实例选了共享{label}目录，但全局设置里还没填，先用实例自己的目录"
                ));
                fallback.to_path_buf()
            }
        },
        "custom" => match choice
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(p) => PathBuf::from(p),
            None => {
                warnings.push(format!("自定义 {label} 目录是空的，先用实例自己的目录。"));
                fallback.to_path_buf()
            }
        },
        _ => fallback.to_path_buf(),
    }
}

/// 只算不写盘：界面预览用的就是这个
pub fn plan(inst: &InstanceConfig, settings: &GlobalSettings) -> StoragePlan {
    let mut warnings: Vec<String> = Vec::new();

    let own_models = own_dir(inst, "models");
    let mut models: Vec<DirEntry> = vec![DirEntry::new(
        own_models.to_string_lossy().to_string(),
        "own",
        false,
        None,
    )];

    // 实例附加目录排前面：越靠前越像是「这个实例自己的东西。
    let mut yaml_dirs: Vec<ModelDir> = Vec::new();
    for dir in &inst.storage.extra_models {
        if dir.path.trim().is_empty() {
            continue;
        }
        models.push(DirEntry::new(
            dir.path.trim(),
            "extra",
            false,
            dir.type_name.clone(),
        ));
        yaml_dirs.push(ModelDir {
            path: dir.path.clone(),
            type_name: dir.type_name.clone(),
            is_default: false,
        });
    }
    let mut shared_options: Vec<SharedModelOption> = Vec::new();
    if inst.storage.use_shared_models {
        let shared: Vec<ModelDir> = settings.storage.shared_models.clone();
        if shared.is_empty() {
            warnings.push("这个实例要用共享模型目录，但全局设置里一个都没配".to_string());
        }
        for dir in &shared {
            let path = dir.path.trim();
            if path.is_empty() {
                continue;
            }
            // 每个实例可以单独关掉某几个共享目录。
            let applies = !shared_disabled(inst, path);
            shared_options.push(SharedModelOption {
                path: path.to_string(),
                type_name: dir.type_name.clone(),
                is_default: dir.is_default,
                applies,
                exists: Path::new(path).is_dir(),
            });
            if !applies {
                continue;
            }
            models.push(DirEntry::new(
                path,
                "shared",
                dir.is_default,
                dir.type_name.clone(),
            ));
            yaml_dirs.push(dir.clone());
        }
        if !shared.is_empty() && !shared_options.iter().any(|o| o.applies) {
            warnings.push(
                "全局共享模型目录被这个实例全部关掉了：只会用实例自己的 models 与附加目录。"
                    .to_string(),
            );
        }
    }

    let text = if yaml_dirs.is_empty() {
        String::new()
    } else {
        yaml_text(&yaml_dirs, &mut warnings)
    };

    // ComfyUI 自己会加 <ComfyUI>\extra_model_paths.yaml（gitignore 里那个）     // 它排在我们传进去的文件前面：存在就提示一声，免得用户以为页面上这些目录是全部
    if own_dir(inst, "extra_model_paths.yaml").is_file() {
        warnings.push(
            "实例的 ComfyUI\\extra_model_paths.yaml 也会被 ComfyUI 自动加载（排在启动器生成的这份前面），两边都生效"
                .to_string(),
        );
    }

    let input = resolve_io(
        &inst.storage.input,
        settings.storage.shared_input.as_deref(),
        &own_dir(inst, "input"),
        "输入",
        &mut warnings,
    );
    let output = resolve_io(
        &inst.storage.output,
        settings.storage.shared_output.as_deref(),
        &own_dir(inst, "output"),
        "输出",
        &mut warnings,
    );

    let mut extra_args: Vec<String> = Vec::new();
    let yaml = yaml_path(inst);
    if !text.is_empty() {
        extra_args.push("--extra-model-paths-config".to_string());
        extra_args.push(yaml.to_string_lossy().to_string());
    }
    let own_input = own_dir(inst, "input");
    let own_output = own_dir(inst, "output");
    if input != own_input {
        extra_args.push("--input-directory".to_string());
        extra_args.push(input.to_string_lossy().to_string());
    }
    if output != own_output {
        extra_args.push("--output-directory".to_string());
        extra_args.push(output.to_string_lossy().to_string());
    }

    let input_source = if input == own_input {
        "own"
    } else if inst.storage.input.mode == "shared" {
        "shared"
    } else {
        "custom"
    };
    let output_source = if output == own_output {
        "own"
    } else if inst.storage.output.mode == "shared" {
        "shared"
    } else {
        "custom"
    };

    StoragePlan {
        models,
        shared_models: shared_options,
        input: DirEntry::new(
            input.to_string_lossy().to_string(),
            input_source,
            false,
            None,
        ),
        output: DirEntry::new(
            output.to_string_lossy().to_string(),
            output_source,
            false,
            None,
        ),
        yaml_path: yaml.to_string_lossy().to_string(),
        yaml_text: text,
        shared_input: settings.storage.shared_input.clone(),
        shared_output: settings.storage.shared_output.clone(),
        extra_args,
        warnings,
    }
}

/// 启动前落地：建目录、写 yaml，返回最终计划（参数以这里返回的为准）。
pub fn prepare(inst: &InstanceConfig, settings: &GlobalSettings) -> Result<StoragePlan, AppError> {
    // 1. 实例自己的目录：ComfyUI 会自己建 input，output 保存时才建，但「打开目录」要能点开
    for child in ["models", "input", "output", "custom_nodes"] {
        let dir = own_dir(inst, child);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).map_err(|e| {
                AppError::new(
                    "STORAGE_DIR_FAILED",
                    format!("创建目录失败 {}：{e}", dir.display()),
                )
            })?;
        }
    }

    // 2. 共享模型目录：不存在就建。建不了只警告，实例照常启动（只是扫不到那边的模型）
    let mut warnings: Vec<String> = Vec::new();
    let default_root = crate::config::default_models_root()
        .to_string_lossy()
        .to_string();
    if inst.storage.use_shared_models {
        for dir in &settings.storage.shared_models {
            let path = dir.path.trim();
            if path.is_empty() || Path::new(path).is_dir() {
                continue;
            }
            match std::fs::create_dir_all(path) {
                Ok(()) => {
                    // 启动器自己的默认共享根目录：顺手铺好标准类型子目录，第一次用就有地方放模。
                    if path.trim_end_matches(['\\', '/'])
                        == default_root.trim_end_matches(['\\', '/'])
                    {
                        for child in DEFAULT_SHARED_SUBDIRS {
                            let _ = std::fs::create_dir_all(Path::new(path).join(child));
                        }
                    }
                }
                Err(e) => warnings.push(format!("共享模型目录建不出来 {path}：{e}")),
            }
        }
    }

    // 3. 目录状态稳定之后再算计划：yaml 文本与参数都以这一次为准。
    let mut base = plan(inst, settings);
    base.warnings.append(&mut warnings);

    // 4. 生效的输入/输出目录（自己的 / 共享 / 自定义的）不存在就建；
    //    建不了只警告，ComfyUI 自己也会按需创建
    for (label, entry) in [("输入", &base.input), ("输出", &base.output)] {
        if !entry.exists {
            match std::fs::create_dir_all(&entry.path) {
                Ok(()) => {}
                Err(e) => base
                    .warnings
                    .push(format!("{label}目录建不出来 {}：{e}", entry.path)),
            }
        }
    }

    // 5.  yaml
    let yaml = yaml_path(inst);
    if base.yaml_text.is_empty() {
        // 没有额外目录：把上次生成的清掉，免得出残留影响下。
        if yaml.exists() {
            let _ = std::fs::remove_file(&yaml);
        }
    } else {
        // 原子写：崩溃/断电时不能留下半截 YAML 让 ComfyUI 解析失败。
        crate::config::atomic_write(&yaml, base.yaml_text.as_bytes()).map_err(|e| {
            AppError::new(
                "STORAGE_YAML_FAILED",
                format!("写入 {} 失败：{}", yaml.display(), e.message),
            )
        })?;
    }

    // 6. 目录都建完之后再算一次， exists 反映最新状。
    let mut fresh = plan(inst, settings);
    fresh.warnings.splice(0..0, base.warnings);
    Ok(fresh)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> InstanceConfig {
        serde_json::from_str(
            r#"{
                "id": "abcdef01",
                "name": "t",
                "rootDir": "C:\\tmp\\comfyfleet-storage-test",
                "engine": "cpu",
                "engineDetail": null,
                "pythonVersion": "3.12",
                "comfy": { "channel": "stable", "ref": "", "commit": "", "version": null },
                "server": { "listen": "127.0.0.1", "port": 8188, "corsOrigin": null, "extraArgs": [] },
                "env": {},
                "overrides": {},
                "imported": false,
                "state": "ready",
                "createdAt": "",
                "lastStartedAt": null
            }"#,
        )
        .expect("instance json")
    }

    fn settings_with_models(paths: &[(&str, bool)]) -> GlobalSettings {
        let mut s = GlobalSettings::default();
        s.storage.shared_models = paths
            .iter()
            .map(|(p, d)| ModelDir {
                path: p.to_string(),
                type_name: None,
                is_default: *d,
            })
            .collect();
        s
    }

    /// 共享目录的「类型」必须序列化 type 字段：前端按 type 读，读不到就会显示成「整 models。
    #[test]
    fn shared_option_uses_type_key() {
        let opt = SharedModelOption {
            path: "D:\\models\\vae".to_string(),
            type_name: Some("vae".to_string()),
            is_default: false,
            applies: true,
            exists: true,
        };
        let json = serde_json::to_string(&opt).unwrap();
        assert!(json.contains("\"type\":\"vae\""), "实际序列化：{json}");
        assert!(!json.contains("typeName"));
    }

    /// 共享 models 根目录、又单独加它 vae 子目录：yaml 里只该出现一。
    #[test]
    fn duplicate_type_path_is_collapsed() {
        let base = std::env::temp_dir().join("comfyfleet-storage-dup");
        let _ = std::fs::remove_dir_all(&base);
        let vae = base.join("vae");
        std::fs::create_dir_all(&vae).unwrap();
        std::fs::create_dir_all(base.join("loras")).unwrap();
        let (base_s, vae_s) = (
            base.to_string_lossy().to_string(),
            vae.to_string_lossy().to_string(),
        );

        let mut settings = GlobalSettings::default();
        settings.storage.shared_models = vec![
            ModelDir {
                path: base_s,
                type_name: None,
                is_default: true,
            },
            ModelDir {
                path: vae_s.clone(),
                type_name: Some("vae".to_string()),
                is_default: false,
            },
        ];

        let plan = plan(&inst(), &settings);

        assert_eq!(
            plan.yaml_text.matches("'vae':").count(),
            1,
            "yaml 里 vae 只能出现一次：{}",
            plan.yaml_text
        );
        // 类型目录本身也不该被当成「根目录」重复扫一遍。
        assert_eq!(plan.yaml_text.matches(&vae_s).count(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 指定了类型的共享目录：整段就是这一类的目录，不再去扫描它的子目录。
    #[test]
    fn typed_shared_dir_is_one_key() {
        let base = std::env::temp_dir().join("comfyfleet-storage-typed");
        let _ = std::fs::remove_dir_all(&base);
        let vae = base.join("vae");
        std::fs::create_dir_all(vae.join("nested")).unwrap();
        let vae_s = vae.to_string_lossy().to_string();

        let mut settings = GlobalSettings::default();
        settings.storage.shared_models = vec![ModelDir {
            path: vae_s.clone(),
            type_name: Some("vae".to_string()),
            is_default: false,
        }];

        let plan = plan(&inst(), &settings);

        assert!(
            plan.yaml_text.contains("'vae':"),
            "yaml 里要 vae 归类{}",
            plan.yaml_text
        );
        assert!(!plan.yaml_text.contains("nested"), "不该把子目录当类 ");
        let shared = plan.shared_models.iter().find(|o| o.path == vae_s).unwrap();
        assert_eq!(shared.type_name.as_deref(), Some("vae"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 实例可以只挑其中几个共享目录：被关掉的不 yaml、也不进生效列表
    #[test]
    fn instance_can_skip_some_shared_dirs() {
        let base = std::env::temp_dir().join("comfyfleet-storage-skip");
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("models-a");
        let b = base.join("models-b");
        std::fs::create_dir_all(a.join("checkpoints")).unwrap();
        std::fs::create_dir_all(b.join("checkpoints")).unwrap();
        let (a_s, b_s) = (
            a.to_string_lossy().to_string(),
            b.to_string_lossy().to_string(),
        );

        let mut inst = inst();
        // 路径结尾多个斜杠也要认出来是同一个
        inst.storage.disabled_shared_models = vec![format!("{b_s}\\")];
        let settings = settings_with_models(&[(&a_s, true), (&b_s, false)]);

        let plan = plan(&inst, &settings);

        let shared: Vec<&DirEntry> = plan
            .models
            .iter()
            .filter(|m| m.source == "shared")
            .collect();
        assert_eq!(shared.len(), 1, "只该挂上没被关掉的那 ");
        assert_eq!(shared[0].path, a_s);
        assert!(plan.yaml_text.contains("checkpoints"));
        assert!(plan.yaml_text.contains(&a_s), "yaml 里要 A 的类型目 ");
        assert!(!plan.yaml_text.contains(&b_s), "B 不该出现 yaml  ");

        // 勾选框要能看到两个，并标出哪个被关掉了
        let options = &plan.shared_models;
        assert_eq!(options.len(), 2);
        assert!(options.iter().find(|o| o.path == a_s).unwrap().applies);
        assert!(!options.iter().find(|o| o.path == b_s).unwrap().applies);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 全关掉也不报错，但要提醒一。
    #[test]
    fn skipping_all_shared_dirs_warns() {
        let mut inst = inst();
        inst.storage.disabled_shared_models = vec!["C:\\models-a".to_string()];
        let settings = settings_with_models(&[("C:\\models-a", true)]);

        let plan = plan(&inst, &settings);

        assert!(plan.models.iter().all(|m| m.source != "shared"));
        assert!(plan.yaml_text.is_empty());
        assert!(plan.warnings.iter().any(|w| w.contains("全部关掉")));
    }

    /// 总开关关掉时，勾选框列表为空（界面就不画了）
    #[test]
    fn master_switch_hides_shared_options() {
        let mut inst = inst();
        inst.storage.use_shared_models = false;
        let settings = settings_with_models(&[("C:\\models-a", true)]);

        let plan = plan(&inst, &settings);

        assert!(plan.shared_models.is_empty());
        assert!(plan.models.iter().all(|m| m.source != "shared"));
    }

    /// 实例里自带的 extra_model_paths.yaml（ComfyUI 自动加载）要被点出来
    #[test]
    fn instance_own_yaml_is_mentioned() {
        let root = std::env::temp_dir().join("comfyfleet-storage-ownyaml");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ComfyUI")).unwrap();
        std::fs::write(
            root.join("ComfyUI").join("extra_model_paths.yaml"),
            b"x: {}\n",
        )
        .unwrap();

        let mut inst = inst();
        inst.root_dir = root.to_string_lossy().to_string();
        let mut settings = GlobalSettings::default();
        settings.storage.shared_models.clear();

        let plan = plan(&inst, &settings);

        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("extra_model_paths.yaml")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 旧实例没 storage 段：默认用共享模型、输入输出各用自己的
    #[test]
    fn old_instance_json_defaults_to_shared_models() {
        let inst = inst();
        assert!(inst.storage.use_shared_models);
        assert_eq!(inst.storage.input.mode, "own");
        assert_eq!(inst.storage.output.mode, "own");
        assert!(inst.storage.extra_models.is_empty());
    }

    /// 只有模型目录会被写进 yaml；输 输出靠命令行参数
    #[test]
    fn yaml_only_carries_model_dirs() {
        let root = std::env::temp_dir().join("comfyfleet-storage-shared");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("checkpoints")).unwrap();

        let mut inst = inst();
        inst.storage.output.mode = "shared".to_string();
        let mut settings = settings_with_models(&[(root.to_string_lossy().as_ref(), true)]);
        settings.storage.shared_output = Some("C:\\definitely\\missing\\out".to_string());

        let plan = plan(&inst, &settings);

        assert!(
            plan.yaml_text.contains("'checkpoints'"),
            "实际{}",
            plan.yaml_text
        );
        assert!(plan.yaml_text.contains("is_default: true"));
        assert!(!plan.yaml_text.contains("output"));
        assert!(plan
            .extra_args
            .windows(2)
            .any(|w| w[0] == "--extra-model-paths-config"));
        assert!(plan
            .extra_args
            .windows(2)
            .any(|w| w[0] == "--output-directory" && w[1] == "C:\\definitely\\missing\\out"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 输入/输出留在自己的目录时，不应该塞多余参。
    #[test]
    fn own_io_needs_no_flags() {
        let inst = inst();
        let mut settings = GlobalSettings::default();
        settings.storage.shared_models.clear();

        let plan = plan(&inst, &settings);

        assert!(!plan.extra_args.iter().any(|a| a == "--input-directory"));
        assert!(!plan.extra_args.iter().any(|a| a == "--output-directory"));
        assert!(plan.yaml_text.is_empty(), "没有额外模型目录就不该生 yaml");
        assert!(plan.extra_args.is_empty());
    }

    /// 单引号里不转义反斜杠，只有单引号要写两个
    #[test]
    fn yaml_quotes_windows_paths() {
        let mut dirs = vec![ModelDir {
            path: "D:\\my models".to_string(),
            type_name: Some("checkpoints".to_string()),
            is_default: true,
        }];
        let mut warnings = Vec::new();
        let text = yaml_text(&mut dirs, &mut warnings);
        assert!(text.contains("is_default: true"));
        assert!(
            text.contains("'checkpoints': 'D:\\my models'"),
            "实际{text}"
        );

        let mut quoted = vec![ModelDir {
            path: "D:\\it's here".to_string(),
            type_name: Some("loras".to_string()),
            is_default: false,
        }];
        let text = yaml_text(&mut quoted, &mut warnings);
        assert!(text.contains("'loras': 'D:\\it''s here'"), "实际{text}");
    }

    /// 模型根目录：扫子目录当类型名，未知名字交 ComfyUI 自己忽略
    #[test]
    fn model_root_scans_subdirs() {
        let root = std::env::temp_dir().join("comfyfleet-storage-scan");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("checkpoints")).unwrap();
        std::fs::create_dir_all(root.join("loras")).unwrap();
        std::fs::write(root.join("stray.safetensors"), b"x").unwrap();

        let mut dirs = vec![ModelDir {
            path: root.to_string_lossy().to_string(),
            type_name: None,
            is_default: false,
        }];
        let mut warnings = Vec::new();
        let text = yaml_text(&mut dirs, &mut warnings);

        assert!(text.contains("'checkpoints'"));
        assert!(text.contains("'loras'"));
        assert!(!text.contains("stray"));
        assert!(warnings.is_empty(), "不该有警告：{warnings:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 空的 models 根目录要明确提示，而不是静默什么都不发。
    #[test]
    fn empty_root_warns() {
        let root = std::env::temp_dir().join("comfyfleet-storage-empty");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut dirs = vec![ModelDir {
            path: root.to_string_lossy().to_string(),
            type_name: None,
            is_default: false,
        }];
        let mut warnings = Vec::new();
        let text = yaml_text(&mut dirs, &mut warnings);

        assert!(text.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("类型子目录"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 选了共享输入但全局没填  退回自己的目录并给提示，不能把空路径传 ComfyUI
    #[test]
    fn shared_io_without_global_falls_back() {
        let mut inst = inst();
        inst.storage.input.mode = "shared".to_string();
        let mut settings = GlobalSettings::default();
        settings.storage.shared_models.clear();

        let plan = plan(&inst, &settings);

        assert_eq!(plan.input.source, "own");
        assert!(plan.input.path.ends_with("input"));
        assert!(plan.warnings.iter().any(|w| w.contains("共享输入目录")));
    }

    /// 启动前的落地：建目录、写 yaml、参数指向真实文。
    #[test]
    fn prepare_writes_yaml_and_dirs() {
        let root = std::env::temp_dir().join("comfyfleet-storage-prepare");
        let models = std::env::temp_dir().join("comfyfleet-storage-prepare-shared");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&models);
        std::fs::create_dir_all(models.join("loras")).unwrap();

        let mut inst = inst();
        inst.root_dir = root.to_string_lossy().to_string();
        inst.storage.output.mode = "custom".to_string();
        inst.storage.output.path = Some(root.join("my-output").to_string_lossy().to_string());
        let mut settings = settings_with_models(&[(models.to_string_lossy().as_ref(), true)]);

        let plan = prepare(&inst, &settings).expect("prepare");

        let yaml = root.join(YAML_NAME);
        assert!(yaml.is_file(), "yaml 应该落在实例根目 ");
        let text = std::fs::read_to_string(&yaml).unwrap();
        assert!(text.contains("'loras': "), "实际{text}");
        assert!(text.contains("is_default: true"));
        assert_eq!(plan.yaml_path, yaml.to_string_lossy());
        assert!(plan
            .extra_args
            .windows(2)
            .any(|w| w[0] == "--extra-model-paths-config" && w[1] == plan.yaml_path));
        // 实例自己的目录（ ComfyUI 下面  自定义输出目录都要建。
        for dir in ["models", "input", "output", "custom_nodes"] {
            assert!(root.join("ComfyUI").join(dir).is_dir(), "{dir} 应该被创 ");
        }
        assert!(root.join("my-output").is_dir(), "自定义输出目录应该被创建");
        // 自定义输出目录存在之后，计划里的 exists 要跟着变成 true
        assert!(plan.output.exists);

        // 关掉共享模型：yaml 被清掉，参数里也不再出现
        inst.storage.use_shared_models = false;
        settings.storage.shared_models.clear();
        let plan = prepare(&inst, &settings).expect("prepare 2");
        assert!(!yaml.exists(), "没有额外目录时应该删掉旧 yaml");
        assert!(!plan
            .extra_args
            .iter()
            .any(|a| a == "--extra-model-paths-config"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&models);
    }

    /// 关掉共享模型后，yaml 里只剩实例附加目录。
    #[test]
    fn disabling_shared_models_keeps_extra_dirs() {
        let mut inst = inst();
        inst.storage.use_shared_models = false;
        inst.storage.extra_models = vec![ModelDir {
            path: "D:\\extra".to_string(),
            type_name: Some("vae".to_string()),
            is_default: false,
        }];
        let settings = settings_with_models(&[("D:\\shared", true)]);

        let plan = plan(&inst, &settings);

        assert!(plan.yaml_text.contains("'vae': 'D:\\extra'"));
        assert!(!plan.yaml_text.contains("D:\\shared"));
        assert!(!plan.models.iter().any(|m| m.source == "shared"));
    }
}
