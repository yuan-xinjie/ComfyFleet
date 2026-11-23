import type { FontFamily, UiOpts } from '../types'

export type FontRole = 'latin' | 'cjk' | 'mono'

/** 与 Rust 端的默认字体保持一致；外观里的「恢复默认字体」回到这一组。 */
export const DEFAULT_FONTS = {
  latin: 'Segoe UI',
  cjk: 'Microsoft YaHei UI',
  mono: 'Cascadia Mono',
} as const

/** 系统字体枚举失败时至少还能选这些 Windows 自带字体。 */
export const FALLBACK_FONTS: FontFamily[] = [
  { name: DEFAULT_FONTS.latin, cjk: false, mono: false },
  { name: 'Arial', cjk: false, mono: false },
  { name: 'Tahoma', cjk: false, mono: false },
  { name: DEFAULT_FONTS.cjk, cjk: true, mono: false },
  { name: 'Microsoft YaHei', cjk: true, mono: false },
  { name: 'DengXian', cjk: true, mono: false },
  { name: 'SimSun', cjk: true, mono: false },
  { name: 'SimHei', cjk: true, mono: false },
  { name: DEFAULT_FONTS.mono, cjk: false, mono: true },
  { name: 'Consolas', cjk: false, mono: true },
]

export function fontOptions(
  fonts: FontFamily[],
  role: FontRole,
  current: string,
): { value: string; label: string }[] {
  const list = fonts.length > 0 ? fonts : FALLBACK_FONTS
  const filtered = list.filter((font) =>
    role === 'cjk' ? font.cjk : role === 'mono' ? font.mono : !font.cjk,
  )
  const options = filtered.map((font) => ({ value: font.name, label: font.name }))
  if (current && !options.some((option) => option.value === current)) {
    options.unshift({ value: current, label: current })
  }
  return options
}

/**
 * 字体族名称来自本机（或旧配置），进 CSS 前去掉能拆开声明的字符；
 * 名字本身用双引号包住，逗号分隔的各段再分别兜底。
 */
function family(value: string, fallback: string): string {
  const clean = value
    .replace(/["'\\;{},]/g, '')
    .replace(/[\u0000-\u001f]/g, '')
    .trim()
  return clean ? `"${clean.replace(/"/g, '')}"` : fallback
}

function stack(families: string[]): string {
  const seen = new Set<string>()
  return families
    .filter((item) => {
      const key = item.replace(/"/g, '').toLowerCase()
      if (seen.has(key)) return false
      seen.add(key)
      return true
    })
    .join(', ')
}

export function uiFontStacks(
  ui: Pick<UiOpts, 'fontLatin' | 'fontCjk' | 'fontMono'>,
): { sans: string; mono: string } {
  const latin = family(ui.fontLatin, '"Segoe UI"')
  const cjk = family(ui.fontCjk, '"Microsoft YaHei UI"')
  const mono = family(ui.fontMono, '"Cascadia Mono"')
  return {
    sans: `${stack([latin, cjk, '"Segoe UI"', '"Microsoft YaHei UI"', 'sans-serif'])}`,
    mono: `${stack([mono, cjk, '"Cascadia Mono"', '"Consolas"', 'monospace'])}`,
  }
}
