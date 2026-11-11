use std::collections::BTreeMap;
use std::path::PathBuf;

use tauri::ipc::Channel;
use tauri::State;

use crate::commands::instance::load_instance;
use crate::error::AppError;
use crate::pty::{self, PtyEvent, SpawnOptions};
use crate::state::AppState;

fn check_program(program: &str) -> Result<(), AppError> {
    // 只接受字面 cmd.exe（由系统 PATH/System32 解析），不接受任何带目录的
    // “cmd.exe”：否则恶意前端可以把任意程序改名后启动。
    if !program.trim().eq_ignore_ascii_case("cmd.exe") {
        return Err(AppError::new(
            "PTY_PROGRAM_DENIED",
            format!("只允许启动系统 cmd.exe，收到：{program}"),
        ));
    }
    Ok(())
}

/// 实例名会进入 cmd 命令行，必须干掉 cmd 元字符（`&` `|` `%` 等），
/// 否则一个带 `&` 的实例名就能注入命令。
fn cmd_safe(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' | '|' | '<' | '>' | '^' | '"' | '%' | '(' | ')' | '\r' | '\n' | '\0' => '_',
            c => c,
        })
        .collect()
}

fn cmd_init(name: &str) -> String {
    // 只保留UTF-8 和窗口标题，不再往终端里塞启动提示
    format!("chcp 65001>nul & title ComfyUI启动器[{}]", cmd_safe(name))
}

#[tauri::command]
pub fn pty_spawn(
    state: State<'_, AppState>,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    on_event: Channel<PtyEvent>,
) -> Result<String, AppError> {
    check_program(&program)?;
    let (id, session) = pty::spawn(
        SpawnOptions {
            program,
            args,
            cwd,
            cols,
            rows,
            env: BTreeMap::new(),
            path_prepend: None,
        },
        on_event,
    )?;
    state.ptys.lock().insert(id.clone(), session);
    Ok(id)
}

/// 打开某个实例的「环境终端」：注入 VIRTUAL_ENV / PATH / 镜像变量，
/// 让用户能在该虚拟环境里手动执行 pip、python 等操作（cmd）。
#[tauri::command]
pub fn pty_spawn_instance(
    state: State<'_, AppState>,
    instance_id: String,
    cols: u16,
    rows: u16,
    on_event: Channel<PtyEvent>,
) -> Result<String, AppError> {
    let inst = load_instance(&instance_id)?;
    let root = PathBuf::from(&inst.root_dir);
    let py = crate::runtime::resolve_python(&inst).ok_or_else(|| {
        AppError::new(
            "VENV_MISSING",
            if inst.python.is_existing() {
                format!(
                    "实例「{}」指向的 Python 环境不可用：{}。到实例设置 → Python 环境里改路径。",
                    inst.name,
                    inst.python.path.clone().unwrap_or_else(|| "（留空）".to_string())
                )
            } else {
                format!(
                    "实例。{}」还没有虚拟环境（期望{}）。先点「初始化 venv」，或到实例设置里改用已有Python 环境。",
                    inst.name,
                    root.join(".venv").display()
                )
            },
        )
    })?;
    let scripts = py.bin_dir();

    let settings = state.settings.lock().clone();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if py.venv_style {
        env.insert(
            "VIRTUAL_ENV".to_string(),
            py.root.to_string_lossy().to_string(),
        );
    }
    env.insert("COMFYFLEET_INSTANCE_ID".to_string(), inst.id.clone());
    env.insert("COMFYFLEET_INSTANCE_NAME".to_string(), inst.name.clone());
    env.insert(
        "COMFYFLEET_INSTANCE_ROOT".to_string(),
        root.to_string_lossy().to_string(),
    );
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".to_string(), "1".to_string());

    // 镜像：让手动 pip 安装也走同一套源，避免启动器装得上、手动装不上"
    // 取源优先级：实例覆盖 > 全局设置
    let pypi = inst
        .overrides
        .pypi
        .as_ref()
        .or(settings.mirrors.pypi.as_ref());
    if let Some(pypi) = pypi {
        env.insert("PIP_INDEX_URL".to_string(), pypi.clone());
        env.insert("UV_INDEX_URL".to_string(), pypi.clone());
    }
    let hf = inst.overrides.hf.as_ref().or(settings.mirrors.hf.as_ref());
    if let Some(hf) = hf {
        env.insert("HF_ENDPOINT".to_string(), hf.clone());
    }
    let hf_offline = inst
        .overrides
        .hf_offline
        .unwrap_or(settings.network.hf_offline);
    if hf_offline {
        env.insert("HF_HUB_OFFLINE".to_string(), "1".to_string());
        env.insert("TRANSFORMERS_OFFLINE".to_string(), "1".to_string());
    }
    // 实例级环境变量（如PYTORCH_CUDA_ALLOC_CONF）优先级最高
    for (k, v) in &inst.env {
        env.insert(k.clone(), v.clone());
    }

    let cwd = {
        let comfy = root.join("ComfyUI");
        if comfy.exists() {
            comfy
        } else {
            root.clone()
        }
    };

    log::info!(
        "打开实例环境终端：instance={} python={}",
        inst.id,
        py.python.display()
    );

    let (id, session) = pty::spawn(
        SpawnOptions {
            program: "cmd.exe".to_string(),
            args: vec!["/K".to_string(), cmd_init(&inst.name)],
            cwd: Some(cwd.to_string_lossy().to_string()),
            cols,
            rows,
            env,
            path_prepend: Some(scripts.to_string_lossy().to_string()),
        },
        on_event,
    )?;
    state.ptys.lock().insert(id.clone(), session);
    Ok(id)
}

#[tauri::command]
pub fn pty_write(state: State<'_, AppState>, id: String, data: String) -> Result<(), AppError> {
    let mut guard = state.ptys.lock();
    let session = guard
        .get_mut(&id)
        .ok_or_else(|| AppError::new("PTY_NOT_FOUND", "PTY session not found"))?;
    session.write(&data)
}

#[tauri::command]
pub fn pty_resize(
    state: State<'_, AppState>,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), AppError> {
    let guard = state.ptys.lock();
    match guard.get(&id) {
        Some(session) => {
            session.resize(cols, rows);
            Ok(())
        }
        None => Err(AppError::new("PTY_NOT_FOUND", "PTY 会话不存在")),
    }
}

#[tauri::command]
pub fn pty_kill(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    let mut guard = state.ptys.lock();
    if let Some(mut session) = guard.remove(&id) {
        session.kill();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bare_cmd_is_allowed() {
        assert!(check_program("cmd.exe").is_ok());
        assert!(check_program("CMD.EXE").is_ok());
        assert!(check_program(r"C:\Windows\System32\cmd.exe").is_err());
        assert!(check_program(r"\\server\share\cmd.exe").is_err());
        assert!(check_program("evil.exe").is_err());
    }

    #[test]
    fn title_cannot_break_out_of_cmd() {
        let safe = cmd_safe("a\r\nb&c|d");
        assert!(!safe.contains('\r'));
        assert!(!safe.contains('\n'));
        assert!(!safe.contains('&'));
        assert!(!safe.contains('|'));
    }
}
