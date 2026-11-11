//! 原生文件夹选择框（Windows IFileOpenDialog）。
//!
//! 只在前端点“选择”时调用，返回绝对路径；取消返回 None。
#[cfg(target_os = "windows")]
pub fn pick_folder() -> Result<Option<String>, String> {
    // COM 的 STA 是线程级的；命令可能落在已经初始化成 MTA 的异步线程上，
    // 直接 CoInitializeEx 会拿到 RPC_E_CHANGED_MODE。专用线程最稳。
    std::thread::spawn(pick_folder_blocking)
        .join()
        .map_err(|_| "文件夹选择框线程异常退出".to_string())?
}

#[cfg(target_os = "windows")]
fn pick_folder_blocking() -> Result<Option<String>, String> {
    use windows::core::{HRESULT, HSTRING};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
        SIGDN_FILESYSPATH,
    };

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let initialized = hr.is_ok();
        let result: Result<Option<String>, String> = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| e.to_string())?;
            dialog
                .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
                .map_err(|e| e.to_string())?;
            dialog
                .SetTitle(&HSTRING::from("选择文件夹"))
                .map_err(|e| e.to_string())?;
            match dialog.Show(None) {
                Ok(()) => {}
                // ERROR_CANCELLED：用户点了取消
                Err(e) if e.code() == HRESULT(0x8007_04C7u32 as i32) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            }
            let item = dialog.GetResult().map_err(|e| e.to_string())?;
            let path = item
                .GetDisplayName(SIGDN_FILESYSPATH)
                .map_err(|e| e.to_string())?;
            let text = path.to_string().map_err(|e| e.to_string())?;
            CoTaskMemFree(Some(path.0 as _));
            Ok(Some(text))
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}

#[cfg(not(target_os = "windows"))]
pub fn pick_folder() -> Result<Option<String>, String> {
    Err("当前平台不支持原生文件夹选择".to_string())
}
