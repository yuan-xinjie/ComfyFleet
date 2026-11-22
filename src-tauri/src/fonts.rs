//! 本机字体枚举：外观里的字体选择直接使用系统已安装的字体族。
//!
//! Windows 用 GDI 按字符集枚举字体族，不读注册表，也不依赖字体文件布局；
//! 用户后来安装的字体重新打开设置页就能看到。

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FontFamily {
    pub name: String,
    /// 字体是否声明支持简体中文字符集。
    pub cjk: bool,
    /// 字体是否为等宽字体。
    pub mono: bool,
}

pub fn list() -> Vec<FontFamily> {
    let mut families = platform_list();
    families.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    families
}

#[cfg(windows)]
fn platform_list() -> Vec<FontFamily> {
    use std::collections::BTreeMap;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontIndirectW, DEFAULT_CHARSET, DeleteDC, DeleteObject,
        EnumFontFamiliesExW, GGI_MARK_NONEXISTING_GLYPHS, GetGlyphIndicesW, HDC, HGDIOBJ, LOGFONTW,
        SelectObject, TMPF_FIXED_PITCH, TEXTMETRICW,
    };
    use windows::core::PCWSTR;

    struct Collector {
        fonts: BTreeMap<String, FontFamily>,
    }

    unsafe extern "system" fn collect(
        logfont: *const LOGFONTW,
        metric: *const TEXTMETRICW,
        _font_type: u32,
        lparam: LPARAM,
    ) -> i32 {
        let collector = &mut *(lparam.0 as *mut Collector);
        if logfont.is_null() || metric.is_null() {
            return 1;
        }
        let logfont = &*logfont;
        let metric = &*metric;
        let name = wide_to_string(&logfont.lfFaceName);
        let name = name.trim();
        if name.is_empty() || name.starts_with('@') {
            return 1;
        }
        // 这个位反着来：置位表示比例字体，未置位才是等宽字体。
        let mono = !metric.tmPitchAndFamily.contains(TMPF_FIXED_PITCH);
        let key = name.to_lowercase();
        let entry = collector
            .fonts
            .entry(key)
            .or_insert_with(|| FontFamily {
                name: name.to_string(),
                cjk: looks_cjk(name),
                mono,
            });
        entry.cjk |= looks_cjk(name);
        entry.mono |= mono;
        1
    }

    fn run(collector: &mut Collector) {
        unsafe {
            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                return;
            }
            let logfont = LOGFONTW {
                lfCharSet: DEFAULT_CHARSET,
                ..Default::default()
            };
            let _ = EnumFontFamiliesExW(
                hdc,
                &logfont,
                Some(collect),
                LPARAM(collector as *mut Collector as isize),
                0,
            );
            let _ = DeleteDC(hdc);
        }
    }

    /// 用字体自身的字形表判断是否真的有中文字形；字体别名/链接不会混进来。
    unsafe fn supports_cjk(hdc: HDC, name: &str) -> bool {
        let mut wide: Vec<u16> = name.encode_utf16().take(31).collect();
        wide.push(0);
        let mut logfont = LOGFONTW {
            lfHeight: 16,
            lfWeight: 400,
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        logfont.lfFaceName[..wide.len()].copy_from_slice(&wide);
        let font = CreateFontIndirectW(&logfont);
        if font.is_invalid() {
            return false;
        }
        let previous = SelectObject(hdc, HGDIOBJ(font.0));
        let mut glyph: u16 = 0xffff;
        let sample = [0x4e2du16];
        let _ = GetGlyphIndicesW(
            hdc,
            PCWSTR(sample.as_ptr()),
            1,
            &mut glyph,
            GGI_MARK_NONEXISTING_GLYPHS,
        );
        let _ = SelectObject(hdc, previous);
        let _ = DeleteObject(HGDIOBJ(font.0));
        glyph != 0xffff
    }

    let mut collector = Collector {
        fonts: BTreeMap::new(),
    };
    run(&mut collector);
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if !hdc.is_invalid() {
            for font in collector.fonts.values_mut() {
                if !font.cjk {
                    font.cjk = supports_cjk(hdc, &font.name);
                }
            }
            let _ = DeleteDC(hdc);
        }
    }
    let fonts: Vec<_> = collector.fonts.into_values().collect();
    if fonts.is_empty() {
        fallback_fonts()
    } else {
        fonts
    }
}

#[cfg(not(windows))]
fn platform_list() -> Vec<FontFamily> {
    fallback_fonts()
}

fn fallback_fonts() -> Vec<FontFamily> {
    [
        ("Segoe UI", false, false),
        ("Arial", false, false),
        ("Microsoft YaHei UI", true, false),
        ("Microsoft YaHei", true, false),
        ("DengXian", true, false),
        ("SimSun", true, false),
        ("SimHei", true, false),
        ("Cascadia Mono", false, true),
        ("Consolas", false, true),
    ]
    .into_iter()
    .map(|(name, cjk, mono)| FontFamily {
        name: name.to_string(),
        cjk,
        mono,
    })
    .collect()
}

#[cfg(windows)]
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn looks_cjk(name: &str) -> bool {
    let name = name.to_lowercase();
    [
        "yahei",
        "simsun",
        "nsimsun",
        "simhei",
        "dengxian",
        "fangsong",
        "kaiti",
        "mingliu",
        "jhenghei",
        "noto sans cjk",
        "noto serif cjk",
        "source han",
        "songti",
        "heiti",
        "song",
        "hei",
        "kai",
        "ming",
        "yu gothic",
        "meiryo",
        "malgun gothic",
        "microsoft jhenghei",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_fonts_cover_three_roles() {
        let fonts = fallback_fonts();
        assert!(fonts.iter().any(|f| !f.cjk && !f.mono));
        assert!(fonts.iter().any(|f| f.cjk));
        assert!(fonts.iter().any(|f| f.mono));
    }

    #[test]
    fn installed_fonts_cover_three_roles() {
        let fonts = list();
        assert!(!fonts.is_empty(), "至少应枚举到系统字体");
        let cjk = fonts.iter().filter(|f| f.cjk).count();
        let mono = fonts.iter().filter(|f| f.mono).count();
        assert!(fonts.len() > cjk, "英文字体不应被全部标成中文");
        assert!(cjk > 0, "应有中文字体");
        assert!(mono > 0, "应有等宽字体");
    }

    #[test]
    fn cjk_name_heuristics_are_case_insensitive() {
        assert!(looks_cjk("Microsoft YaHei UI"));
        assert!(looks_cjk("Noto Sans CJK SC"));
        assert!(looks_cjk("Source Han Sans SC"));
        assert!(!looks_cjk("Segoe UI"));
        assert!(!looks_cjk("Cascadia Mono"));
    }
}
