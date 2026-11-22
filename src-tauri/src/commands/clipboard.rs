use crate::error::AppError;

const CF_UNICODETEXT: u32 = 13;

/// 读取系统剪贴板的纯文本。
///
/// 浏览器 Clipboard API 在 WebView2 里读剪贴板会弹权限并可能一直挂起；
/// 右键粘贴走这条 Windows API，和终端自己的粘贴一样直接。
#[tauri::command]
pub fn clipboard_read_text() -> Result<String, AppError> {
    #[cfg(windows)]
    {
        read_windows()
    }
    #[cfg(not(windows))]
    {
        Err(AppError::new(
            "CLIPBOARD_UNSUPPORTED",
            "仅 Windows 支持读取剪贴板",
        ))
    }
}

#[cfg(windows)]
fn read_windows() -> Result<String, AppError> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // 剪贴板可能被别的进程短暂占用，重试几次，失败就不要把输入卡住。
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    if !opened {
        return Err(AppError::new("CLIPBOARD_BUSY", "剪贴板正被其他程序占用"));
    }

    let result = (|| {
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
            return Ok(String::new());
        }
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }
            .map_err(|e| AppError::new("CLIPBOARD_READ", e.to_string()))?;
        let memory = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(memory) };
        if ptr.is_null() {
            return Err(AppError::new("CLIPBOARD_READ", "无法锁定剪贴板内存"));
        }
        let size = unsafe { GlobalSize(memory) };
        let units = if size >= 2 {
            unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), size / 2) }
        } else {
            &[]
        };
        let end = units
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(units.len());
        let text = String::from_utf16_lossy(&units[..end]);
        let _ = unsafe { GlobalUnlock(memory) };
        Ok(text)
    })();

    let _ = unsafe { CloseClipboard() };
    result
}
