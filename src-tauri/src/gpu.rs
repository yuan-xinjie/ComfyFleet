//! 显卡与驱动探测。
//!
//! 只依赖 `nvidia-smi`（NVIDIA 驱动自带），不引入任何额外依赖。
//! 关键信息是 `CUDA Version`：它是**驱动支持的最高 CUDA 运行时版本**，
//! 用它来判断某个引擎档能不能用，比维护"驱动号 → CUDA 版本"的映射表可靠得多。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub name: String,
    pub driver_version: String,
    /// 驱动支持的最高 CUDA 版本，如 "13.0"；拿不到则为 None
    pub cuda_version: Option<String>,
    pub memory_mib: Option<u64>,
}

fn run(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("nvidia-smi");
    for a in args {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    // 驱动卡死/初始化慢时不能把创建页永远挂住。
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let buf = rx.recv_timeout(Duration::from_secs(1)).ok()?;
                return Some(String::from_utf8_lossy(&buf).to_string());
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 解析 `nvidia-smi --query-gpu=utilization.gpu` 的输出（第一块 GPU）。
fn parse_utilization(text: &str) -> Option<u32> {
    let value = text.lines().next()?.trim().parse::<u32>().ok()?;
    (value <= 100).then_some(value)
}

/// 最近一次读到的 GPU 利用率，缓存 1 秒：健康检查每 3 秒一轮，
/// 多实例同时轮询时不必反复拉起 `nvidia-smi`。
static UTIL_CACHE: Mutex<Option<(Instant, u32)>> = Mutex::new(None);

/// 当前 GPU 利用率（0–100）。拿不到就返回 None，界面显示“—”，不编数字。
pub fn utilization() -> Option<u32> {
    if let Ok(cache) = UTIL_CACHE.lock() {
        if let Some((at, value)) = *cache {
            if at.elapsed() < Duration::from_secs(1) {
                return Some(value);
            }
        }
    }
    let out = run(&[
        "--query-gpu=utilization.gpu",
        "--format=csv,noheader,nounits",
    ])?;
    let value = parse_utilization(&out)?;
    if let Ok(mut cache) = UTIL_CACHE.lock() {
        *cache = Some((Instant::now(), value));
    }
    Some(value)
}

fn parse_cuda_version(text: &str) -> Option<String> {
    // 表头格式在变：
    //   旧驱动：| NVIDIA-SMI 550.54.14  Driver Version: 550.54.14  CUDA Version: 12.4 |
    //   610.x ：| NVIDIA-SMI 610.88  KMD Version: 610.88  CUDA UMD Version: 13.3 |
    // 所以不能写死 "CUDA Version:"，改成「先找 cuda，再在就近窗口里找 version:」。
    // 注意 "KMD Version:" 出现在 "CUDA UMD Version" 之前，必须从 cuda 之后开始找。
    let lower = text.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("cuda") {
        let after_cuda = from + rel + "cuda".len();
        // 本地化输出可能含非 ASCII 字符：按字符窗口取，不能按字节切片。
        let tail = lower.get(after_cuda..)?;
        let window: String = tail.chars().take(48).collect();
        if let Some(vrel) = window.find("version:") {
            let after_ver = vrel + "version:".len();
            let token: String = window[after_ver..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !token.is_empty() {
                return Some(token);
            }
        }
        from = after_cuda;
    }
    None
}

/// 探测结果缓存：`nvidia-smi` 每次约 100–300 ms，检测页会多次调用。
/// 只缓存成功结果；驱动未就绪/调用失败时下次重试，避免一次失败后整个会话都按 CPU。
static CACHE: Mutex<Option<GpuInfo>> = Mutex::new(None);

pub fn detect() -> Option<GpuInfo> {
    if let Some(cached) = CACHE.lock().ok().and_then(|c| c.clone()) {
        return Some(cached);
    }
    let query = run(&[
        "--query-gpu=name,driver_version,memory.total",
        "--format=csv,noheader,nounits",
    ])?;
    let first = query.lines().next()?.trim();
    let mut parts = first.split(',').map(|s| s.trim().to_string());
    let name = parts.next()?;
    let driver_version = parts.next()?;
    let memory_mib = parts.next().and_then(|s| s.parse::<u64>().ok());
    if name.is_empty() || driver_version.is_empty() {
        return None;
    }
    let cuda_version = run(&[]).and_then(|full| parse_cuda_version(&full));
    let info = GpuInfo {
        name,
        driver_version,
        cuda_version,
        memory_mib,
    };
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some(info.clone());
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::{parse_cuda_version, parse_utilization};

    /// 610.88 真实表头（2026-10-28 在 RTX 5090 机器上抓到）。
    /// 注意这里没有 "CUDA Version:"，只有 "CUDA UMD Version:"。
    #[test]
    fn parses_new_driver_header_cuda_umd_version() {
        let sample = "| NVIDIA-SMI 610.88                 KMD Version: 610.88        CUDA UMD Version: 13.3     |";
        assert_eq!(parse_cuda_version(sample).as_deref(), Some("13.3"));
    }

    /// 老驱动的经典表头，必须继续支持
    #[test]
    fn parses_legacy_driver_header() {
        let sample = "| NVIDIA-SMI 550.54.14              Driver Version: 550.54.14         CUDA Version: 12.4     |";
        assert_eq!(parse_cuda_version(sample).as_deref(), Some("12.4"));
    }

    /// KMD Version 在 CUDA 之前，不能被误取成 CUDA 版本
    #[test]
    fn does_not_confuse_kmd_version_with_cuda() {
        let sample = "NVIDIA-SMI 610.88   KMD Version: 610.88   CUDA UMD Version: 13.3";
        let parsed = parse_cuda_version(sample);
        let got = parsed.as_deref();
        assert_eq!(got, Some("13.3"), "实际拿到 {got:?}");
        assert_ne!(got, Some("610.88"));
    }

    #[test]
    fn parses_gpu_utilization() {
        assert_eq!(parse_utilization("87\n"), Some(87));
        assert_eq!(parse_utilization(" 0 "), Some(0));
        assert_eq!(parse_utilization("100"), Some(100));
        assert_eq!(parse_utilization("[N/A]"), None);
        assert_eq!(parse_utilization("101"), None);
    }

    #[test]
    fn returns_none_without_header() {
        assert!(parse_cuda_version("no driver here").is_none());
        assert!(parse_cuda_version("CUDA Version: unknown").is_none());
    }

    #[test]
    fn non_ascii_header_does_not_panic() {
        let sample = "显卡 CUDA UMD Version: 13.3";
        assert_eq!(parse_cuda_version(sample).as_deref(), Some("13.3"));
    }
}
