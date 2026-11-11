import type { ThemePalette, UiOpts } from '../types'

export type ThemeMode = UiOpts['theme']

/** 暗色默认配色：保持现有观感，卡片底色单独拆出来。 */
export const DEFAULT_DARK_PALETTE: ThemePalette = {
  accent: '#2b63ff',
  bgTop: '#1a2036',
  bgMid: '#111525',
  bgBottom: '#020203',
  cardColor: '#161b2a',
}

/** 亮色默认配色：浅灰蓝渐变 + 白卡片，文字由亮色 token 自动转深色。 */
export const DEFAULT_LIGHT_PALETTE: ThemePalette = {
  accent: '#2b63ff',
  bgTop: '#f7f9ff',
  bgMid: '#eef2fa',
  bgBottom: '#e3e9f4',
  cardColor: '#ffffff',
}

/** 兼容旧导入名；暗色仍是默认。 */
export const DEFAULT_THEME = DEFAULT_DARK_PALETTE

export const MAX_CARD_TRANSPARENCY = 80

export function normalizeHex(input: string): string | null {
  let value = input.trim().toLowerCase()
  if (/^#[0-9a-f]{3}$/.test(value)) {
    value = `#${value[1]}${value[1]}${value[2]}${value[2]}${value[3]}${value[3]}`
  }
  return /^#[0-9a-f]{6}$/.test(value) ? value : null
}

function rgb(hex: string): [number, number, number] {
  const value = normalizeHex(hex) ?? '#000000'
  return [
    Number.parseInt(value.slice(1, 3), 16),
    Number.parseInt(value.slice(3, 5), 16),
    Number.parseInt(value.slice(5, 7), 16),
  ]
}

function channel(value: number): string {
  return Math.round(Math.min(255, Math.max(0, value)))
    .toString(16)
    .padStart(2, '0')
}

/** 把 a 向 b 混合 weight（0..1），用于从主题色派生 hover / press / ink。 */
export function mixHex(a: string, b: string, weight: number): string {
  const [ar, ag, ab] = rgb(a)
  const [br, bg, bb] = rgb(b)
  return `#${channel(ar + (br - ar) * weight)}${channel(ag + (bg - ag) * weight)}${channel(
    ab + (bb - ab) * weight,
  )}`
}

export function rgbaHex(hex: string, value: number): string {
  const [r, g, b] = rgb(hex)
  return `rgba(${r}, ${g}, ${b}, ${value})`
}

/** 相对亮度：决定主按钮上应该用白字还是深色字。 */
export function luminance(hex: string): number {
  const [r, g, b] = rgb(hex).map((value) => {
    const c = value / 255
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4
  })
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

export function onAccent(hex: string): string {
  return luminance(hex) > 0.19 ? '#0b0b12' : '#ffffff'
}

export function paletteFor(ui: UiOpts, mode: ThemeMode): ThemePalette {
  const fallback = mode === 'light' ? DEFAULT_LIGHT_PALETTE : DEFAULT_DARK_PALETTE
  const palette = mode === 'light' ? ui.light : ui.dark
  if (!palette) return fallback
  return {
    accent: normalizeHex(palette.accent) ?? fallback.accent,
    bgTop: normalizeHex(palette.bgTop) ?? fallback.bgTop,
    bgMid: normalizeHex(palette.bgMid) ?? fallback.bgMid,
    bgBottom: normalizeHex(palette.bgBottom) ?? fallback.bgBottom,
    cardColor: normalizeHex(palette.cardColor) ?? fallback.cardColor,
  }
}

/** 把当前主题的配色写到 :root；所有组件引用 --blue* / --bg-* / --card 即可。 */
export function applyTheme(ui: UiOpts) {
  const mode: ThemeMode = ui.theme === 'light' ? 'light' : 'dark'
  const isLight = mode === 'light'
  const theme = paletteFor(ui, mode)
  const accent = theme.accent
  const rawTransparency = Number(ui.cardTransparency)
  const transparency = Number.isFinite(rawTransparency)
    ? Math.min(MAX_CARD_TRANSPARENCY, Math.max(0, rawTransparency))
    : 12
  const cardAlpha = 1 - transparency / 100

  const root = document.documentElement
  root.dataset.theme = mode
  root.classList.toggle('theme-light', isLight)

  root.style.setProperty('--blue', accent)
  root.style.setProperty('--blue-hover', mixHex(accent, isLight ? '#000000' : '#ffffff', 0.1))
  root.style.setProperty('--blue-press', mixHex(accent, '#000000', 0.22))
  // 亮色背景上的链接要往黑里压，暗色背景上的往白里提，才不会和底同色。
  const ink = isLight
    ? luminance(accent) > 0.55
      ? mixHex(accent, '#000000', 0.42)
      : accent
    : luminance(accent) > 0.6
      ? accent
      : mixHex(accent, '#ffffff', 0.38)
  root.style.setProperty('--blue-ink', ink)
  root.style.setProperty('--on-accent', onAccent(accent))
  root.style.setProperty('--blue-wash', rgbaHex(accent, isLight ? 0.12 : 0.16))
  root.style.setProperty('--blue-ring', rgbaHex(accent, isLight ? 0.36 : 0.48))
  root.style.setProperty('--accent-select', rgbaHex(accent, isLight ? 0.24 : 0.42))
  root.style.setProperty('--selection-ink', isLight ? '#141824' : '#ffffff')

  root.style.setProperty('--card-color', theme.cardColor)
  root.style.setProperty('--card-alpha', String(cardAlpha))
  root.style.setProperty('--card', rgbaHex(theme.cardColor, cardAlpha))
  root.style.setProperty(
    '--card-hover',
    rgbaHex(mixHex(theme.cardColor, isLight ? '#000000' : '#ffffff', isLight ? 0.035 : 0.055), cardAlpha),
  )
  root.style.setProperty('--card-press', rgbaHex(mixHex(theme.cardColor, '#000000', 0.06), cardAlpha))

  // 卡片文字跟着卡片实际亮度走：暗色主题里选了亮色卡片也不会白字糊成一片。
  const cardSurface = mixHex(theme.bgMid, theme.cardColor, cardAlpha)
  const cardIsLight = luminance(cardSurface) > 0.22
  const cardInk = cardIsLight ? '#141824' : '#f5f6fb'
  root.style.setProperty('--card-ink', cardInk)
  root.style.setProperty('--card-coral-ink', cardIsLight ? '#b83b24' : '#ff8a9a')
  root.style.setProperty('--card-green', cardIsLight ? '#0a7a53' : '#3ddc97')
  root.style.setProperty('--card-orange', cardIsLight ? '#a85f00' : '#ffb454')
  root.style.setProperty('--card-red', cardIsLight ? '#d12d48' : '#ff5c6c')
  root.style.setProperty(
    '--card-blue-ink',
    cardIsLight
      ? luminance(accent) > 0.55
        ? mixHex(accent, '#000000', 0.42)
        : accent
      : luminance(accent) > 0.6
        ? accent
        : mixHex(accent, '#ffffff', 0.38),
  )
  root.style.setProperty('--card-ink-2', rgbaHex(cardInk, 0.82))
  root.style.setProperty('--card-ink-3', rgbaHex(cardInk, 0.74))
  root.style.setProperty('--card-ink-4', rgbaHex(cardInk, 0.66))

  root.style.setProperty('--bg-top', theme.bgTop)
  root.style.setProperty('--bg-mid', theme.bgMid)
  root.style.setProperty('--bg-bottom', theme.bgBottom)
}
