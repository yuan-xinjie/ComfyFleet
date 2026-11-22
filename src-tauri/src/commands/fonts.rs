use crate::fonts::FontFamily;

/// 外观字体选择用的本机字体族列表；每次打开设置页重新枚举，新装字体不用重启。
/// 枚举 600+ 字体要逐个检查中文字形，放后台线程，别卡界面。
#[tauri::command]
pub async fn fonts_list() -> Vec<FontFamily> {
    tauri::async_runtime::spawn_blocking(crate::fonts::list)
        .await
        .unwrap_or_default()
}
