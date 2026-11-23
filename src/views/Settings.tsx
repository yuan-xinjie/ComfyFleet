import { TextInput, Checkbox, Range, DirectoryInput } from '../components/Form'
import Button from '../components/Button'
import IconTextButton from '../components/IconTextButton'
import { useCallback, useEffect, useRef, useState, type CSSProperties } from 'react'
import { api, errText } from '../lib/api'
import type {
  AppInfo,
  AppUpdate,
  FontFamily,
  GitInfo,
  GlobalSettings,
  IslandIdleShape,
  RuntimeEvent,
  RuntimeStatus,
  StorageSettings,
  SyncCacheInfo,
  ThemePalette,
  UiOpts,
  UpdateEvent,
  UvCacheInfo,
  WheelsInfo,
} from '../types'
import Icon, { type IconName } from '../components/Icon'
import ModelDirList from '../components/ModelDirList'
import Segmented from '../components/Segmented'
import Select from '../components/Select'
import {
  IslandShape,
  islandGlowStyle,
  islandIdleStyle,
  normalizeIslandShape,
  ISLAND_ANIM_SPEED_MAX,
  ISLAND_ANIM_SPEED_MIN,
  ISLAND_BOTTOM_GAP_MAX,
  ISLAND_BOTTOM_GAP_MIN,
  ISLAND_GLOW_RANGE_MAX,
  ISLAND_GLOW_RANGE_MIN,
  ISLAND_SHAPE_ACTION_GAP_MAX,
  ISLAND_SHAPE_ACTION_GAP_MIN,
  ISLAND_SHAPE_INSET_MAX,
  ISLAND_SHAPE_INSET_MIN,
  ISLAND_WIDTH_MAX,
  ISLAND_WIDTH_MIN,
} from '../components/IslandShape'
import coreLogo from '../assets/core-logo.svg'
import {
  DEFAULT_BACKGROUND_COLOR,
  DEFAULT_BACKGROUND_MODE,
  DEFAULT_CARD_SHADOW,
  DEFAULT_CARD_SHADOW_SIZE,
  DEFAULT_DARK_PALETTE,
  DEFAULT_LIGHT_PALETTE,
  MAX_BAR_TRANSPARENCY,
  MAX_CARD_SHADOW_SIZE,
  MAX_CARD_TRANSPARENCY,
  applyTheme,
  luminance,
  mixHex,
  normalizeHex,
  onAccent,
  paletteFor,
  rgbaHex,
  type ThemeMode,
} from '../lib/theme'
import { DEFAULT_FONTS, FALLBACK_FONTS, fontOptions } from '../lib/fonts'

const IDLE_SHAPES: { value: IslandIdleShape; label: string }[] = [
  { value: 'chevrons', label: '箭头' },
  { value: 'targets', label: '圆环' },
  { value: 'eyes', label: '眼睛' },
]

interface MirrorPreset {
  value: string
  label: string
}

const PYPI_MIRRORS: MirrorPreset[] = [
  { value: '', label: '官方源' },
  { value: 'https://pypi.tuna.tsinghua.edu.cn/simple', label: '清华' },
  { value: 'https://mirrors.aliyun.com/pypi/simple/', label: '阿里云' },
  { value: 'https://pypi.mirrors.ustc.edu.cn/simple', label: '中科大' },
  { value: 'https://mirrors.cloud.tencent.com/pypi/simple', label: '腾讯云' },
  { value: 'https://repo.huaweicloud.com/repository/pypi/simple', label: '华为云' },
]

const TORCH_MIRRORS: MirrorPreset[] = [
  { value: '', label: '官方源' },
  { value: 'https://mirrors.aliyun.com/pytorch-wheels', label: '阿里云' },
  { value: 'https://mirror.sjtu.edu.cn/pytorch-wheels', label: '上海交大' },
]

/**
 * 镜像源：候选直接点在输入框里填，也可以自己敲地址。
 * 留空 = 官方源，所以占位文字就写「官方源」。
 */
function MirrorField(props: {
  label: string
  ariaLabel: string
  value: string | null
  presets: MirrorPreset[]
  placeholder: string
  onChange: (value: string) => void
}) {
  const { label, ariaLabel, value, presets, placeholder, onChange } = props
  return (
    <div className="field">
      <label>{label}</label>
      <Select
        ariaLabel={ariaLabel}
        editable
        className="mirror-select"
        value={(value ?? '').trim()}
        editableValue={value ?? ''}
        editablePlaceholder={placeholder}
        options={presets}
        onChange={onChange}
        onEditableChange={onChange}
      />
    </div>
  )
}

function previewClockText(): string {
  const now = new Date()
  return `${String(now.getHours()).padStart(2, '0')}:${String(now.getMinutes()).padStart(2, '0')}`
}

export type SettingsSection =
  | 'appearance'
  | 'island'
  | 'network'
  | 'storage'
  | 'runtime'
  | 'sync'
  | 'logs'
  | 'update'

const SETTINGS_SECTIONS: { id: SettingsSection; label: string; icon: IconName }[] = [
  { id: 'appearance', label: '外观', icon: 'overview' },
  { id: 'island', label: '灵动岛', icon: 'console' },
  { id: 'network', label: '网络与镜像', icon: 'globe' },
  { id: 'storage', label: '存储', icon: 'box' },
  { id: 'runtime', label: '运行', icon: 'power' },
  { id: 'sync', label: '同步与快照', icon: 'nodes' },
  { id: 'logs', label: '操作日志', icon: 'console' },
  { id: 'update', label: '更新', icon: 'download' },
]

export interface SettingsActions {
  save: () => Promise<void>
  discard: () => void
  reload: () => void
  /** 顶栏切换主题后，把已落盘的 theme 同步进设置草稿，避免保存时覆盖回去。 */
  syncTheme: (theme: ThemeMode) => void
}

interface Props {
  info: AppInfo | null
  notify: (kind: 'ok' | 'err', msg: string) => void
  onDirty: (dirty: boolean) => void
  onBusy: (busy: boolean) => void
  onActions: (actions: SettingsActions | null) => void
  active?: boolean
  /** 保存后把新的明暗状态同步给顶栏切换按钮 */
  onThemeChange?: (theme: ThemeMode) => void
  /** 切换配色页签或改颜色时临时应用到整个界面，保存后才写盘 */
  onPreviewTheme?: (ui: UiOpts) => void
  /** 外部（例如启动时的运行时提示）要求直接打开某个分区 */
  focusSection?: SettingsSection | null
}

function ColorField(props: {
  label: string
  value: string
  hint?: string
  onChange: (value: string) => void
}) {
  const { label, value, hint, onChange } = props
  const picker = normalizeHex(value) ?? '#000000'
  return (
    <div className="field color-field">
      <label>{label}</label>
      <div className="color-row">
        <TextInput
          className="color-chip"
          type="color"
          aria-label={`${label}取色`}
          value={picker}
          onChange={(e) => onChange(e.target.value)}
        />
        <TextInput
          className="mono color-hex"
          aria-label={label}
          value={value}
          spellCheck={false}
          onChange={(e) => onChange(e.target.value)}
        />
      </div>
      {hint && <span className="hint">{hint}</span>}
    </div>
  )
}

/** 操作日志上限：输入 MB、存字节；失焦时收敛到 64 KB – 64 MB */
function LogSizeField({ bytes, onChange }: { bytes: number; onChange: (bytes: number) => void }) {
  const format = (value: number) => String(Number((value / 1024 / 1024).toFixed(2)))
  const [text, setText] = useState(format(bytes))
  useEffect(() => setText(format(bytes)), [bytes])
  const commit = (raw: string) => {
    const mb = Number(raw)
    if (!Number.isFinite(mb) || raw.trim() === '') {
      setText(format(bytes))
      return
    }
    const clamped = Math.min(64, Math.max(0.0625, mb))
    onChange(Math.round(clamped * 1024 * 1024))
    setText(format(clamped * 1024 * 1024))
  }
  return (
    <TextInput
      aria-label="操作日志文件上限（MB）"
      type="number"
      min={0.0625}
      max={64}
      step={0.25}
      value={text}
      onChange={(e) => {
        const raw = e.target.value
        setText(raw)
        // 把合法输入即时同步给上层，否则 Ctrl+S 会绕过 blur 保存旧上限。
        const mb = Number(raw)
        if (raw.trim() !== '' && Number.isFinite(mb)) {
          const clamped = Math.min(64, Math.max(0.0625, mb))
          onChange(Math.round(clamped * 1024 * 1024))
        }
      }}
      onBlur={(e) => commit(e.target.value)}
    />
  )
}

function cacheSize(bytes: number): string {
  if (!bytes) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  const fixed = unit >= 3 ? 1 : value >= 10 || unit === 0 ? 0 : 1
  return `${value.toFixed(fixed)} ${units[unit]}`
}

function cacheBreakdown(info: SyncCacheInfo): string {
  return [
    `代次 ${info.generationCount} 个 / ${cacheSize(info.generationBytes)}`,
    `收件箱 ${info.inboxCount} 个 / ${cacheSize(info.inboxBytes)}`,
    `备份 ${info.backupCount} 个 / ${cacheSize(info.backupBytes)}`,
    `节点缓存 ${cacheSize(info.nodeCacheBytes)}`,
  ].join(' · ')
}

export default function SettingsView({
  info,
  notify,
  onDirty,
  onBusy,
  onActions,
  active = true,
  onThemeChange,
  onPreviewTheme,
  focusSection,
}: Props) {
  const [section, setSection] = useState<SettingsSection>(focusSection ?? 'appearance')

  // 启动检测发现缺运行时后，首页横幅会带着 focusSection 把用户送进来
  useEffect(() => {
    if (focusSection) setSection(focusSection)
  }, [focusSection])
  const [settings, setSettings] = useState<GlobalSettings | null>(null)
  const [fonts, setFonts] = useState<FontFamily[]>(FALLBACK_FONTS)
  const [previewClock, setPreviewClock] = useState(previewClockText)
  const [baseline, setBaseline] = useState<string | null>(null)
  const [editMode, setEditMode] = useState<ThemeMode>('dark')
  const [busy, setBusy] = useState(false)
  const [cacheInfo, setCacheInfo] = useState<SyncCacheInfo | null>(null)
  const [cacheBusy, setCacheBusy] = useState(false)
  const [cacheError, setCacheError] = useState<string | null>(null)
  const [uvInfo, setUvInfo] = useState<UvCacheInfo | null>(null)
  const [uvBusy, setUvBusy] = useState(false)
  const [uvError, setUvError] = useState<string | null>(null)
  const [uvConfirm, setUvConfirm] = useState(false)
  const [wheelsInfo, setWheelsInfo] = useState<WheelsInfo | null>(null)
  const [wheelsBusy, setWheelsBusy] = useState(false)
  const loaded = useRef(false)
  const actions = useRef<SettingsActions | null>(null)
  const layoutRef = useRef<HTMLDivElement>(null)

  const load = useCallback(async () => {
    try {
      const next = await api.settingsGet()
      const ui: UiOpts = {
        ...next.ui,
        islandIdleShape: normalizeIslandShape(next.ui.islandIdleShape),
        backgroundMode: next.ui.backgroundMode === 'color' ? 'color' : DEFAULT_BACKGROUND_MODE,
      }
      // 旧版本配置里留存的图片/毛玻璃字段不再使用，丢掉避免继续写回配置文件。
      for (const key of [
        'backgroundImage',
        'backgroundImageOpacity',
        'backgroundImageBlur',
        'cardGlass',
        'cardBlur',
      ]) {
        delete (ui as unknown as Record<string, unknown>)[key]
      }
      const normalized = { ...next, ui }
      setSettings(normalized)
      setEditMode(ui.theme)
      setBaseline(JSON.stringify(normalized))
      onPreviewTheme?.(ui)
    } catch (e) {
      notify('err', errText(e))
    }
  }, [notify, onPreviewTheme])

  const loadCacheInfo = useCallback(async () => {
    setCacheBusy(true)
    setCacheError(null)
    try {
      setCacheInfo(await api.syncCacheInfo())
    } catch (e) {
      setCacheError(errText(e))
    } finally {
      setCacheBusy(false)
    }
  }, [])

  const loadUvCacheInfo = useCallback(async () => {
    setUvBusy(true)
    setUvError(null)
    try {
      setUvInfo(await api.uvCacheInfo())
    } catch (e) {
      setUvError(errText(e))
    } finally {
      setUvBusy(false)
    }
  }, [])

  const loadWheelsInfo = useCallback(async () => {
    setWheelsBusy(true)
    try {
      setWheelsInfo(await api.wheelsInfo())
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setWheelsBusy(false)
    }
  }, [notify])

  // 本机字体可能在两次打开设置之间装了新的；每次进设置页重新取一遍。
  useEffect(() => {
    if (!active) return
    api.fontsList().then(setFonts).catch(() => undefined)
  }, [active])

  useEffect(() => {
    if (loaded.current) return
    loaded.current = true
    void load()
  }, [load])

  useEffect(() => {
    if (section === 'sync') void loadCacheInfo()
    if (section === 'storage') {
      void loadUvCacheInfo()
      void loadWheelsInfo()
    }
  }, [loadCacheInfo, loadUvCacheInfo, loadWheelsInfo, section])

  useEffect(() => {
    setUvConfirm(false)
  }, [section])

  const dirty = baseline !== null && settings !== null && JSON.stringify(settings) !== baseline
  useEffect(() => onDirty(dirty), [dirty, onDirty])

  // 顶栏可能已在别处切换主题；设置页重新打开时若没有草稿就从磁盘同步一次。
  const wasActive = useRef(false)
  useEffect(() => {
    if (active && !wasActive.current && !dirty) void load()
    wasActive.current = active
  }, [active, dirty, load])

  useEffect(() => {
    const content = layoutRef.current?.closest('.content')
    if (content instanceof HTMLElement) content.scrollTo({ top: 0 })
  }, [section])

  const save = async () => {
    if (!settings || busy) return
    const { ui } = settings
    const palettes = [ui.dark, ui.light]
    const colors = palettes.flatMap((palette) => [
      palette.accent,
      palette.bgTop,
      palette.bgMid,
      palette.bgBottom,
      palette.cardColor,
      palette.barColor,
    ])
    if (colors.some((value) => normalizeHex(value) === null)) {
      notify('err', '颜色要写成 #RRGGBB，例如 #2b63ff')
      return
    }
    const cardTransparency = Math.min(
      MAX_CARD_TRANSPARENCY,
      Math.max(0, Math.round(Number(ui.cardTransparency) || 0)),
    )
    const barTransparency = Math.min(
      MAX_BAR_TRANSPARENCY,
      Math.max(0, Math.round(Number(ui.barTransparency) || 0)),
    )
    const cardShadowSize = Math.min(
      MAX_CARD_SHADOW_SIZE,
      Math.max(0, Math.round(Number(ui.cardShadowSize) || 0)),
    )
    const normalizePalette = (palette: ThemePalette): ThemePalette => ({
      accent: normalizeHex(palette.accent)!,
      bgTop: normalizeHex(palette.bgTop)!,
      bgMid: normalizeHex(palette.bgMid)!,
      bgBottom: normalizeHex(palette.bgBottom)!,
      cardColor: normalizeHex(palette.cardColor)!,
      barColor: normalizeHex(palette.barColor)!,
    })
    const normalized: GlobalSettings = {
      ...settings,
      ui: {
        ...ui,
        dark: normalizePalette(ui.dark),
        light: normalizePalette(ui.light),
        cardTransparency,
        barTransparency,
        barFollowCard: Boolean(ui.barFollowCard),
        cardShadow: Boolean(ui.cardShadow),
        cardShadowSize,
        backgroundMode: ui.backgroundMode === 'color' ? 'color' : 'gradient',
        backgroundColor: normalizeHex(ui.backgroundColor) ?? DEFAULT_BACKGROUND_COLOR,
      },
    }
    setBusy(true)
    try {
      const saved = await api.settingsSave(normalized)
      setSettings(saved)
      setBaseline(JSON.stringify(saved))
      applyTheme(saved.ui)
      onPreviewTheme?.(saved.ui)
      onThemeChange?.(saved.ui.theme)
      void loadCacheInfo()
      void loadUvCacheInfo()
      void loadWheelsInfo()
      notify('ok', '设置已保存，主题已应用')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  // 顶栏的开关已经写盘，设置页草稿跟着走；baseline 也要一起改，
  // 否则只切主题就会让设置页一直显示“未保存”，保存时还会把旧主题写回去。
  const syncTheme = (theme: ThemeMode) => {
    setSettings((current) =>
      current && current.ui.theme !== theme
        ? { ...current, ui: { ...current.ui, theme } }
        : current,
    )
    setEditMode(theme)
    setBaseline((raw) => {
      if (!raw) return raw
      try {
        const parsed = JSON.parse(raw) as GlobalSettings
        parsed.ui.theme = theme
        return JSON.stringify(parsed)
      } catch {
        return raw
      }
    })
  }

  // 底栏通过这个句柄触发当前设置值的保存/放弃/重读；动作始终指向最新一次渲染
  actions.current = {
    save,
    discard: () => void load(),
    reload: () => void load(),
    syncTheme,
  }
  useEffect(() => {
    onActions({
      save: async () => {
        await actions.current?.save()
      },
      discard: () => actions.current?.discard(),
      reload: () => actions.current?.reload(),
      syncTheme: (theme) => actions.current?.syncTheme(theme),
    })
    return () => onActions(null)
  }, [onActions])

  useEffect(() => onBusy(busy), [busy, onBusy])

  useEffect(() => {
    if (!active || section !== 'island') return
    setPreviewClock(previewClockText())
    if (settings?.ui.islandIdleContent !== 'time') return
    const timer = window.setInterval(
      () => setPreviewClock(previewClockText()),
      1000,
    )
    return () => window.clearInterval(timer)
  }, [active, section, settings?.ui.islandIdleContent])

  useEffect(() => {
    // 设置页常驻后，隐藏时别抢全局 Ctrl+S。
    if (!active) return
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
        e.preventDefault()
        if (dirty && !busy) void actions.current?.save()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [active, dirty, busy])

  if (!settings) return <div className="faint">加载设置…</div>

  const islandPreviewClock = previewClock
  const islandPreviewStyle = {
    ...islandIdleStyle(
      settings.ui.islandWidth,
      settings.ui.islandAnimationSpeed,
      settings.ui.islandShapeInset,
      settings.ui.islandIdleContent === 'animation',
    ),
    ...islandGlowStyle(settings.ui.islandGlowColor, settings.ui.islandGlowRange),
  } as CSSProperties
  const islandPreviewText =
    settings.ui.islandIdleContent === 'time'
      ? islandPreviewClock
      : settings.ui.islandIdleContent === 'custom'
        ? settings.ui.islandIdleText.trim() || '控制台'
        : '控制台'
  const islandPreviewVisual =
    settings.ui.islandIdleContent === 'animation' ? (
      <IslandShape shape={settings.ui.islandIdleShape} />
    ) : (
      <>
        <Icon
          name={settings.ui.islandIdleContent === 'time' ? 'clock' : 'console'}
          size={13}
        />
        <span
          className={
            settings.ui.islandIdleContent === 'time'
              ? 'mono'
              : settings.ui.islandIdleContent === 'custom'
                ? 'island-idle-custom'
                : undefined
          }
        >
          {islandPreviewText}
        </span>
      </>
    )

  const setMirror = (key: keyof GlobalSettings['mirrors'], value: string) =>
    setSettings({ ...settings, mirrors: { ...settings.mirrors, [key]: value.trim() || null } })

  const setWheelRepo = (value: string) =>
    setSettings({ ...settings, wheelRepo: value.trim() || null })

  const applyDraft = (ui: UiOpts) => {
    const next = { ...ui, islandIdleShape: normalizeIslandShape(ui.islandIdleShape) }
    setSettings({ ...settings, ui: next })
    onPreviewTheme?.(next)
  }

  const setFont = (patch: Partial<Pick<UiOpts, 'fontLatin' | 'fontCjk' | 'fontMono'>>) =>
    applyDraft({ ...settings.ui, ...patch })

  const resetFonts = () =>
    applyDraft({
      ...settings.ui,
      fontLatin: DEFAULT_FONTS.latin,
      fontCjk: DEFAULT_FONTS.cjk,
      fontMono: DEFAULT_FONTS.mono,
    })

  const setUiStyle = (uiStyle: UiOpts['uiStyle']) =>
    applyDraft({ ...settings.ui, uiStyle })

  const setPalette = (key: keyof ThemePalette, value: string) => {
    const nextPalette = { ...settings.ui[editMode], [key]: value }
    const ui: UiOpts = {
      ...settings.ui,
      theme: editMode,
      [editMode]: nextPalette,
    }
    setSettings({ ...settings, ui })
    // 输入到一半的 #2b6 不做全屏回退，避免亮度闪一下。
    if (normalizeHex(value)) onPreviewTheme?.(ui)
  }

  const setCardTransparency = (value: number) =>
    applyDraft({
      ...settings.ui,
      theme: editMode,
      cardTransparency: Math.min(MAX_CARD_TRANSPARENCY, Math.max(0, value)),
    })

  const setBarTransparency = (value: number) =>
    applyDraft({
      ...settings.ui,
      theme: editMode,
      barTransparency: Math.min(MAX_BAR_TRANSPARENCY, Math.max(0, value)),
    })

  const setBarFollowCard = (value: boolean) =>
    applyDraft({
      ...settings.ui,
      theme: editMode,
      barFollowCard: value,
    })

  const setCardShadow = (value: boolean) =>
    applyDraft({ ...settings.ui, cardShadow: value })

  const setCardShadowSize = (value: number) =>
    applyDraft({
      ...settings.ui,
      cardShadowSize: Math.min(MAX_CARD_SHADOW_SIZE, Math.max(0, value)),
    })

  const setBackgroundMode = (value: UiOpts['backgroundMode']) =>
    applyDraft({ ...settings.ui, backgroundMode: value })

  const resetEffects = () =>
    applyDraft({
      ...settings.ui,
      cardShadow: DEFAULT_CARD_SHADOW,
      cardShadowSize: DEFAULT_CARD_SHADOW_SIZE,
      backgroundMode: DEFAULT_BACKGROUND_MODE,
      backgroundColor: DEFAULT_BACKGROUND_COLOR,
    })

  const resetTheme = () =>
    applyDraft({
      ...settings.ui,
      theme: editMode,
      dark: { ...DEFAULT_DARK_PALETTE },
      light: { ...DEFAULT_LIGHT_PALETTE },
    })

  /** 页签就是明暗切换：先实时应用草稿，保存动作再写盘。 */
  const switchMode = (mode: ThemeMode) => {
    setEditMode(mode)
    applyDraft({ ...settings.ui, theme: mode })
  }

  const setStorage = (patch: Partial<StorageSettings>) =>
    setSettings({ ...settings, storage: { ...settings.storage, ...patch } })

  const setNetwork = (patch: Partial<GlobalSettings['network']>) =>
    setSettings({ ...settings, network: { ...settings.network, ...patch } })

  const setActivityMaxBytes = (maxBytes: number) =>
    setSettings({ ...settings, activity: { ...settings.activity, maxBytes } })

  const setSync = (patch: Partial<GlobalSettings['sync']>) =>
    setSettings({ ...settings, sync: { ...settings.sync, ...patch } })

  const clearUvCache = async () => {
    if (uvBusy) return
    setUvBusy(true)
    setUvError(null)
    try {
      const result = await api.uvCacheClear()
      setUvConfirm(false)
      notify(
        'ok',
        result.freedBytes > 0
          ? `已清理 UV 缓存，释放 ${cacheSize(result.freedBytes)}`
          : 'UV 缓存已经是空的',
      )
      await loadUvCacheInfo()
    } catch (e) {
      const message = errText(e)
      setUvError(message)
      notify('err', message)
    } finally {
      setUvBusy(false)
    }
  }

  /** 共享目录的默认位置：数据目录下的 shared\input | shared\output */
  const sharedExample = (child: 'input' | 'output') =>
    info ? `${info.dataDir}\\shared\\${child}` : `shared\\${child}`

  const openShared = async (path: string | null) => {
    if (!path) return
    try {
      await api.openPath(path)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  /** 共享输入 / 输出目录：选一个文件夹填进对应字段。 */
  const chooseSharedDir = async (child: 'input' | 'output') => {
    try {
      const dir = await api.pickFolder()
      if (!dir) return
      setStorage(child === 'input' ? { sharedInput: dir } : { sharedOutput: dir })
    } catch (e) {
      notify('err', errText(e))
    }
  }

  /** 选一个目录做 wheel 库；保存设置后生效，统计等重新加载。 */
  const chooseWheelsDir = async () => {
    try {
      const dir = await api.pickFolder()
      if (!dir) return
      setSettings({ ...settings, wheels: dir })
      setWheelsInfo(null)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const editPalette = settings.ui[editMode]
  const previewPalette = paletteFor(settings.ui, editMode)
  const previewAccent = previewPalette.accent
  const previewTop = previewPalette.bgTop
  const previewMid = previewPalette.bgMid
  const previewBottom = previewPalette.bgBottom
  const previewAlpha = 1 - settings.ui.cardTransparency / 100
  const shownBarTransparency = settings.ui.barFollowCard
    ? settings.ui.cardTransparency
    : settings.ui.barTransparency
  const previewIsColor = settings.ui.backgroundMode === 'color'
  const previewBgColor =
    (previewIsColor ? normalizeHex(settings.ui.backgroundColor) : null) ?? previewMid
  // 预览里“卡片”文字跟着卡片亮度，“设置”按钮跟着实际背景亮度。
  const previewCardInk =
    luminance(mixHex(previewBgColor, previewPalette.cardColor, previewAlpha)) > 0.22
      ? '#141824'
      : '#f5f6fb'
  const previewBgInk = luminance(previewBgColor) > 0.4 ? '#141824' : '#f5f6fb'
  const previewCardLine = rgbaHex(previewCardInk, 0.24)
  const previewBgLine = rgbaHex(previewBgInk, 0.24)

  return (
    <div className="settings-layout" ref={layoutRef}>
      <nav className="settings-rail" aria-label="设置分类">
        <span
          className="settings-nav-glider"
          aria-hidden="true"
          style={{ transform: `translateY(${Math.max(0, SETTINGS_SECTIONS.findIndex((i) => i.id === section)) * 36}px)` }}
        />
        {SETTINGS_SECTIONS.map((item) => (
          <IconTextButton
            key={item.id}
            type="button"
            className={`settings-nav${section === item.id ? ' active' : ''}`}
            aria-current={section === item.id ? 'page' : undefined}
            onClick={() => setSection(item.id)}
           icon={item.icon} iconSize={14}>
            <span>{item.label}</span>
          </IconTextButton>
        ))}
      </nav>

      <div className="settings-main">
        {section === 'appearance' && (
          <div className="panel">
            <div className="panel-head">
              <h2>外观</h2>
              <span className="badge ghost">实时预览</span>
              <div className="spacer" />
              <Button className="ghost sm" onClick={resetFonts}>恢复默认字体</Button>
              <Button className="ghost sm" onClick={resetTheme}>恢复默认配色</Button>
              <Button className="ghost sm" onClick={resetEffects}>恢复默认效果</Button>
            </div>
            <div className="panel-body">
              <div className="appearance-options-row">
                <div className="appearance-option">
                  <span className="appearance-style-label">界面样式</span>
                  <Segmented
                    ariaLabel="界面样式"
                    value={settings.ui.uiStyle}
                    options={[
                      { value: 'borderless', label: '无边框' },
                      { value: 'outlined', label: '描边' },
                    ]}
                    onChange={setUiStyle}
                  />
                </div>
                <div className="appearance-option">
                  <span className="appearance-style-label">背景颜色</span>
                  <Segmented
                    ariaLabel="背景颜色"
                    value={settings.ui.backgroundMode}
                    options={[
                      { value: 'gradient', label: '渐变' },
                      { value: 'color', label: '纯色' },
                    ]}
                    onChange={(value) => setBackgroundMode(value as UiOpts['backgroundMode'])}
                  />
                </div>
                <div className="appearance-option">
                  <span className="appearance-style-label">颜色模式</span>
                  <Segmented
                    ariaLabel="选择要编辑的配色"
                    role="tablist"
                    buttonRole="tab"
                    value={editMode}
                    options={[
                      { value: 'dark', label: '暗色配色' },
                      { value: 'light', label: '亮色配色' },
                    ]}
                    onChange={switchMode}
                  />
                </div>
              </div>

              <div className="grid-2 appearance-colors">
                <ColorField
                  label="主题色"
                  value={editPalette.accent}
                  onChange={(value) => setPalette('accent', value)}
                />
                {settings.ui.backgroundMode === 'gradient' ? (
                  <>
                    <ColorField
                      label="渐变顶部"
                      value={editPalette.bgTop}
                      onChange={(value) => setPalette('bgTop', value)}
                    />
                    <ColorField
                      label="渐变中部"
                      value={editPalette.bgMid}
                      onChange={(value) => setPalette('bgMid', value)}
                    />
                    <ColorField
                      label="渐变底部"
                      value={editPalette.bgBottom}
                      onChange={(value) => setPalette('bgBottom', value)}
                    />
                  </>
                ) : (
                  <ColorField
                    label="背景颜色"
                    value={settings.ui.backgroundColor}
                    onChange={(value) => applyDraft({ ...settings.ui, backgroundColor: value })}
                  />
                )}
              </div>

              <div className="appearance-card-row">
                <ColorField
                  label="卡片颜色"
                  value={editPalette.cardColor}
                  onChange={(value) => setPalette('cardColor', value)}
                />
                <label className="field card-alpha-field">
                  <span>
                    卡片透明度
                    <span className="mono card-alpha-value">{settings.ui.cardTransparency}%</span>
                  </span>
                  <Range
                    type="range"
                    min={0}
                    max={MAX_CARD_TRANSPARENCY}
                    step={1}
                    value={settings.ui.cardTransparency}
                    aria-label="卡片透明度"
                    onChange={(e) => setCardTransparency(Number(e.target.value))}
                  />
                </label>
              </div>

              <div className="appearance-card-row">
                <ColorField
                  label="底栏颜色"
                  value={editPalette.barColor}
                  onChange={(value) => setPalette('barColor', value)}
                />
                <label className="field card-alpha-field">
                  <span>
                    底栏透明度
                    <span className="mono card-alpha-value">{shownBarTransparency}%</span>
                  </span>
                  <Range
                    type="range"
                    min={0}
                    max={MAX_BAR_TRANSPARENCY}
                    step={1}
                    value={shownBarTransparency}
                    aria-label="底栏透明度"
                    disabled={settings.ui.barFollowCard}
                    onChange={(e) => setBarTransparency(Number(e.target.value))}
                  />
                </label>
              </div>

              <label className="toggle appearance-bar-follow">
                <Checkbox
                  type="checkbox"
                  checked={settings.ui.barFollowCard}
                  onChange={(e) => setBarFollowCard(e.target.checked)}
                />
                跟随卡片透明度
              </label>

              <div className="appearance-effect-grid">
                <div className="appearance-effect-block">
                  <label className="toggle">
                    <Checkbox
                      type="checkbox"
                      checked={settings.ui.cardShadow}
                      onChange={(e) => setCardShadow(e.target.checked)}
                    />
                    卡片阴影
                  </label>
                  <label className="field card-alpha-field">
                    <span>
                      阴影大小
                      <span className="mono card-alpha-value">
                        {settings.ui.cardShadowSize}px
                      </span>
                    </span>
                    <Range
                      type="range"
                      min={0}
                      max={MAX_CARD_SHADOW_SIZE}
                      step={1}
                      value={settings.ui.cardShadowSize}
                      aria-label="阴影大小"
                      disabled={!settings.ui.cardShadow}
                      onChange={(e) => setCardShadowSize(Number(e.target.value))}
                    />
                  </label>
                </div>
              </div>

              <div className="appearance-fonts">
                <label className="field">
                  <span>英文字体</span>
                  <Select
                    ariaLabel="英文字体"
                    value={settings.ui.fontLatin}
                    options={fontOptions(fonts, 'latin', settings.ui.fontLatin)}
                    searchable
                    searchPlaceholder="搜索字体"
                    onChange={(value) => setFont({ fontLatin: value })}
                  />
                </label>
                <label className="field">
                  <span>中文字体</span>
                  <Select
                    ariaLabel="中文字体"
                    value={settings.ui.fontCjk}
                    options={fontOptions(fonts, 'cjk', settings.ui.fontCjk)}
                    searchable
                    searchPlaceholder="搜索字体"
                    onChange={(value) => setFont({ fontCjk: value })}
                  />
                </label>
                <label className="field">
                  <span>等宽字体</span>
                  <Select
                    ariaLabel="等宽字体"
                    value={settings.ui.fontMono}
                    options={fontOptions(fonts, 'mono', settings.ui.fontMono)}
                    searchable
                    searchPlaceholder="搜索字体"
                    onChange={(value) => setFont({ fontMono: value })}
                  />
                </label>
              </div>

              <div
                className="theme-preview"
                style={{
                  background: previewIsColor
                    ? previewBgColor
                    : `linear-gradient(150deg, ${previewTop} 0%, ${previewMid} 52%, ${previewBottom} 100%)`,
                }}
              >
                <span className="theme-preview-label" style={{ color: rgbaHex(previewBgInk, 0.62) }}>预览</span>
                <span
                  className="theme-preview-alpha"
                  style={{
                    background: rgbaHex(previewPalette.cardColor, previewAlpha),
                    borderColor: previewCardLine,
                    color: previewCardInk,
                  }}
                >
                  卡片
                </span>
                <span className="theme-preview-btn" style={{ background: previewAccent, color: onAccent(previewAccent) }}>
                  启动
                </span>
                <span className="theme-preview-ghost" style={{ borderColor: previewBgLine, color: previewBgInk }}>
                  设置
                </span>
                <span className="theme-preview-logo">
                  <img src={coreLogo} alt="" />
                </span>
              </div>
            </div>
          </div>
        )}

        {section === 'island' && (
          <div className="panel island-panel">
            <div className="panel-head">
              <h2>灵动岛</h2>
              <span className="badge ghost">实时预览</span>
              <span className="island-head-preview" aria-hidden="true">
                <span
                  className={`island-idle${settings.ui.islandIdleContent === 'animation' ? ' island-idle-anim' : ''}${settings.ui.islandGlow ? ' is-glowing' : ''}`}
                  style={islandPreviewStyle}
                >
                  {islandPreviewVisual}
                </span>
              </span>
            </div>
            <div className="panel-body island-settings-body">
              <div className="island-settings-groups">
                <section className="island-group">
                  <div className="island-card-row is-choosers">
                    <label className="field">
                      <span>空闲任务条</span>
                      <Segmented
                        ariaLabel="空闲任务条"
                        value={settings.ui.taskBarIdle}
                        options={[
                          { value: 'auto', label: '自动隐藏' },
                          { value: 'always', label: '常驻' },
                        ]}
                        onChange={(value) =>
                          applyDraft({ ...settings.ui, taskBarIdle: value })
                        }
                      />
                    </label>

                    <label className="field">
                      <span>空闲内容</span>
                      <Segmented
                        ariaLabel="空闲内容"
                        value={settings.ui.islandIdleContent}
                        options={[
                          { value: 'animation', label: '动画' },
                          { value: 'time', label: '时间' },
                          { value: 'custom', label: '自定义' },
                        ]}
                        onChange={(value) =>
                          applyDraft({ ...settings.ui, islandIdleContent: value })
                        }
                      />
                    </label>

                    <label className="field">
                      <span>通知停留</span>
                      <Segmented
                        ariaLabel="通知停留"
                        value={String(settings.ui.noticeSeconds)}
                        options={[1, 2, 3, 4, 5].map((seconds) => ({
                          value: String(seconds),
                          label: `${seconds} 秒`,
                        }))}
                        onChange={(value) =>
                          applyDraft({ ...settings.ui, noticeSeconds: Number(value) })
                        }
                      />
                    </label>

                    <label className="field">
                      <span>外发光</span>
                      <Segmented
                        ariaLabel="外发光"
                        value={settings.ui.islandGlow ? 'on' : 'off'}
                        options={[
                          { value: 'on', label: '开启' },
                          { value: 'off', label: '关闭' },
                        ]}
                        onChange={(value) =>
                          applyDraft({ ...settings.ui, islandGlow: value === 'on' })
                        }
                      />
                    </label>
                  </div>

                  {settings.ui.islandIdleContent === 'custom' && (
                    <label className="field">
                      <span>自定义内容</span>
                      <TextInput
                        aria-label="自定义内容"
                        maxLength={24}
                        value={settings.ui.islandIdleText}
                        onChange={(e) =>
                          applyDraft({
                            ...settings.ui,
                            islandIdleText: e.target.value,
                          })
                        }
                      />
                    </label>
                  )}

                  {settings.ui.islandGlow && (
                    <div className="island-card-row is-two">
                      <ColorField
                        label="默认发光颜色"
                        value={settings.ui.islandGlowColor}
                        onChange={(value) =>
                          applyDraft({ ...settings.ui, islandGlowColor: value })
                        }
                      />
                      <label className="field card-alpha-field">
                        <span>
                          发光范围
                          <span className="mono card-alpha-value">
                            {settings.ui.islandGlowRange}px
                          </span>
                        </span>
                        <Range
                          aria-label="发光范围"
                          min={ISLAND_GLOW_RANGE_MIN}
                          max={ISLAND_GLOW_RANGE_MAX}
                          step={2}
                          value={settings.ui.islandGlowRange}
                          onChange={(e) =>
                            applyDraft({
                              ...settings.ui,
                              islandGlowRange: Number(e.target.value),
                            })
                          }
                        />
                      </label>
                    </div>
                  )}
                </section>

                <section className="island-group">
                  <div className="island-card-row is-three">
                    <label className="field card-alpha-field">
                      <span>
                        空闲宽度
                        <span className="mono card-alpha-value">
                          {settings.ui.islandWidth}px
                        </span>
                      </span>
                      <Range
                        aria-label="空闲宽度"
                        min={ISLAND_WIDTH_MIN}
                        max={ISLAND_WIDTH_MAX}
                        step={2}
                        value={settings.ui.islandWidth}
                        onChange={(e) =>
                          applyDraft({
                            ...settings.ui,
                            islandWidth: Number(e.target.value),
                          })
                        }
                      />
                    </label>

                    <label className="field card-alpha-field">
                      <span>
                        底部距离
                        <span className="mono card-alpha-value">
                          {settings.ui.islandBottomGap}px
                        </span>
                      </span>
                      <Range
                        aria-label="底部距离"
                        min={ISLAND_BOTTOM_GAP_MIN}
                        max={ISLAND_BOTTOM_GAP_MAX}
                        step={2}
                        value={settings.ui.islandBottomGap}
                        onChange={(e) =>
                          applyDraft({
                            ...settings.ui,
                            islandBottomGap: Number(e.target.value),
                          })
                        }
                      />
                    </label>

                    <label className="field card-alpha-field">
                      <span>
                        动画速度
                        <span className="mono card-alpha-value">
                          {(settings.ui.islandAnimationSpeed / 100).toFixed(1)}×
                        </span>
                      </span>
                      <Range
                        aria-label="动画速度"
                        min={ISLAND_ANIM_SPEED_MIN}
                        max={ISLAND_ANIM_SPEED_MAX}
                        step={10}
                        value={settings.ui.islandAnimationSpeed}
                        onChange={(e) =>
                          applyDraft({
                            ...settings.ui,
                            islandAnimationSpeed: Number(e.target.value),
                          })
                        }
                      />
                    </label>
                  </div>

                  {settings.ui.islandIdleContent === 'animation' && (
                    <div className="island-shape-block">
                      <span className="island-card-label">默认形态</span>
                      <div
                        className="island-shape-options"
                        role="radiogroup"
                        aria-label="默认形态"
                      >
                        {IDLE_SHAPES.map(({ value, label }) => {
                          const active = settings.ui.islandIdleShape === value
                          return (
                            <button
                              key={value}
                              type="button"
                              role="radio"
                              aria-checked={active}
                              className={`island-shape-option${active ? ' is-active' : ''}`}
                              onClick={() =>
                                applyDraft({ ...settings.ui, islandIdleShape: value })
                              }
                            >
                              <span className="island-shape-tile">
                                <IslandShape shape={value} />
                              </span>
                              <span className="island-shape-option-label">{label}</span>
                            </button>
                          )
                        })}
                      </div>

                      <div className="island-card-row is-two">
                        <label className="field card-alpha-field">
                          <span>
                            两侧距离
                            <span className="mono card-alpha-value">
                              {settings.ui.islandShapeInset}px
                            </span>
                          </span>
                          <Range
                            aria-label="两侧距离"
                            min={ISLAND_SHAPE_INSET_MIN}
                            max={ISLAND_SHAPE_INSET_MAX}
                            step={1}
                            value={settings.ui.islandShapeInset}
                            onChange={(e) =>
                              applyDraft({
                                ...settings.ui,
                                islandShapeInset: Number(e.target.value),
                              })
                            }
                          />
                        </label>

                        <label className="field card-alpha-field">
                          <span>
                            按钮间距
                            <span className="mono card-alpha-value">
                              {settings.ui.islandShapeActionGap}px
                            </span>
                          </span>
                          <Range
                            aria-label="按钮间距"
                            min={ISLAND_SHAPE_ACTION_GAP_MIN}
                            max={ISLAND_SHAPE_ACTION_GAP_MAX}
                            step={1}
                            value={settings.ui.islandShapeActionGap}
                            onChange={(e) =>
                              applyDraft({
                                ...settings.ui,
                                islandShapeActionGap: Number(e.target.value),
                              })
                            }
                          />
                        </label>
                      </div>
                    </div>
                  )}
                </section>
              </div>
            </div>
          </div>
        )}

        {section === 'network' && (
          <div className="panel">
            <div className="panel-head">
              <h2>网络与镜像</h2>
              <span className="badge ghost">所有实例共用</span>
            </div>
            <div className="panel-body">
              <div className="grid-2">
                <MirrorField
                  label="PyPI 镜像"
                  ariaLabel="PyPI 镜像"
                  value={settings.mirrors.pypi}
                  presets={PYPI_MIRRORS}
                  placeholder="官方源"
                  onChange={(value) => setMirror('pypi', value)}
                />
                <div className="field">
                  <label>HuggingFace 镜像</label>
                  <TextInput
                    aria-label="HuggingFace 镜像" value={settings.mirrors.hf ?? ''}
                    placeholder="https://hf-mirror.com"
                    onChange={(e) => setMirror('hf', e.target.value)}
                  />
                </div>
                <MirrorField
                  label="PyTorch 镜像"
                  ariaLabel="PyTorch 镜像"
                  value={settings.mirrors.torch}
                  presets={TORCH_MIRRORS}
                  placeholder="官方源"
                  onChange={(value) => setMirror('torch', value)}
                />
                <div className="field">
                  <label>GitHub 加速</label>
                  <TextInput
                    aria-label="GitHub 加速" value={settings.mirrors.githubProxy ?? ''}
                    placeholder="https://gh-proxy.com/https://github.com"
                    onChange={(e) => setMirror('githubProxy', e.target.value)}
                  />
                </div>
                <div className="field" style={{ gridColumn: '1 / -1' }}>
                  <label>云端轮子仓库</label>
                  <TextInput
                    className="mono"
                    aria-label="云端轮子仓库"
                    value={settings.wheelRepo ?? ''}
                    placeholder="https://www.modelscope.cn/models/用户名/仓库名"
                    spellCheck={false}
                    onChange={(e) => setWheelRepo(e.target.value)}
                  />
                  <span className="hint">
                    目前支持 ModelScope 模型仓库。布局为「包名 / 版本 / xxx.whl」；实例「其他轮子」首次打开时自动读取并缓存，之后点面板里的刷新按钮才重新查询。
                  </span>
                </div>
              </div>
              <label className="toggle">
                <Checkbox
                  type="checkbox"
                  checked={settings.network.hfOffline}
                  onChange={(e) => setNetwork({ hfOffline: e.target.checked })}
                />
                HuggingFace 离线模式
                <span className="hint">只用本地缓存，不发起下载。</span>
              </label>
            </div>
          </div>
        )}

        {section === 'storage' && (
          <div className="panel">
            <div className="panel-head">
              <h2>存储</h2>
              <span className="badge ghost">所有实例共用</span>
            </div>
            <div className="panel-body">
              <div className="field">
                <label>共享模型目录</label>
                <ModelDirList
                  dirs={settings.storage.sharedModels}
                  onChange={(sharedModels) => setStorage({ sharedModels })}
                  showDefault
                  onError={(msg) => notify('err', msg)}
                />
              </div>
              <div className="stack" style={{ marginTop: 14, gap: 14 }}>
                <div className="field">
                  <label>共享输入目录</label>
                  <DirectoryInput
                    wrapperClassName="dir-single"
                    className="mono"
                    aria-label="共享输入目录"
                    placeholder={sharedExample('input')}
                    value={settings.storage.sharedInput ?? ''}
                    spellCheck={false}
                    onChange={(e) => setStorage({ sharedInput: e.target.value.trim() || null })}
                    onPick={() => void chooseSharedDir('input')}
                    pickTitle="选择共享输入目录"
                    actions={
                      <Button
                        type="button"
                        className="ghost sm"
                        onClick={() => setStorage({ sharedInput: sharedExample('input') })}
                      >
                        用默认位置
                      </Button>
                    }
                    onOpen={() => void openShared(settings.storage.sharedInput)}
                    openTitle="打开共享输入目录"
                    openDisabled={!settings.storage.sharedInput}
                  />
                </div>
                <div className="field">
                  <label>共享输出目录</label>
                  <DirectoryInput
                    wrapperClassName="dir-single"
                    className="mono"
                    aria-label="共享输出目录"
                    placeholder={sharedExample('output')}
                    value={settings.storage.sharedOutput ?? ''}
                    spellCheck={false}
                    onChange={(e) => setStorage({ sharedOutput: e.target.value.trim() || null })}
                    onPick={() => void chooseSharedDir('output')}
                    pickTitle="选择共享输出目录"
                    actions={
                      <Button
                        type="button"
                        className="ghost sm"
                        onClick={() => setStorage({ sharedOutput: sharedExample('output') })}
                      >
                        用默认位置
                      </Button>
                    }
                    onOpen={() => void openShared(settings.storage.sharedOutput)}
                    openTitle="打开共享输出目录"
                    openDisabled={!settings.storage.sharedOutput}
                  />
                </div>
              </div>

              <div className="field" style={{ maxWidth: 560, marginTop: 16 }}>
                <label>UV 缓存自动清理阈值</label>
                <div className="dir-single" style={{ maxWidth: 150 }}>
                  <TextInput
                    aria-label="UV 缓存自动清理阈值（GB）"
                    type="number"
                    min={0}
                    max={1024}
                    step={1}
                    value={settings.storage.uvCacheMaxGb ?? 10}
                    onChange={(e) => {
                      const raw = Number(e.target.value)
                      const next = Number.isFinite(raw)
                        ? Math.min(1024, Math.max(0, Math.round(raw)))
                        : 0
                      setStorage({ uvCacheMaxGb: next })
                    }}
                  />
                  <span className="hint hint-unit" style={{ alignSelf: 'center' }}>
                    GB
                  </span>
                </div>
                <span className="hint">
                  超过这个大小就自动清空 UV 缓存；0 = 关闭自动清理。默认 10 GB。
                </span>
              </div>

              <div className="field" style={{ maxWidth: 560, marginTop: 14 }}>
                <label>当前 UV 缓存</label>
                <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
                  <strong style={{ fontSize: 20, fontVariantNumeric: 'tabular-nums' }}>
                    {uvInfo ? cacheSize(uvInfo.totalBytes) : uvBusy ? '统计中…' : '—'}
                  </strong>
                  <Button
                    type="button"
                    className="ghost sm icon-btn"
                    aria-label="刷新 UV 缓存大小"
                    title="重新统计"
                    disabled={uvBusy}
                    onClick={() => void loadUvCacheInfo()}
                  >
                    <Icon name="refresh" size={13} />
                  </Button>
                  {uvConfirm ? (
                    <>
                      <Button
                        type="button"
                        className="danger sm"
                        disabled={uvBusy}
                        onClick={() => void clearUvCache()}
                      >
                        {uvBusy ? '清理中…' : '确认清理'}
                      </Button>
                      <Button
                        type="button"
                        className="ghost sm"
                        disabled={uvBusy}
                        onClick={() => setUvConfirm(false)}
                      >
                        取消
                      </Button>
                    </>
                  ) : (
                    <Button
                      type="button"
                      className="ghost sm"
                      disabled={uvBusy || !uvInfo || uvInfo.totalBytes === 0}
                      onClick={() => setUvConfirm(true)}
                    >
                      清理
                    </Button>
                  )}
                </div>
                <span className="hint is-status">
                  {uvError
                    ? `读取失败：${uvError}`
                    : uvInfo
                      ? `${uvInfo.path} · ${cacheSize(uvInfo.totalBytes)}`
                      : ''}
                </span>
              </div>

              <div className="field" style={{ marginTop: 18 }}>
                <label>本地 Wheel 库</label>
                <DirectoryInput
                  wrapperClassName="dir-single"
                  className="mono"
                  aria-label="本地 Wheel 库目录"
                  value={settings.wheels ?? ''}
                  spellCheck={false}
                  onChange={(e) =>
                    setSettings({ ...settings, wheels: e.target.value.trim() || null })
                  }
                  onPick={() => void chooseWheelsDir()}
                  pickTitle="选择本地 Wheel 库文件夹"
                  onOpen={() => void openShared(settings.wheels ?? null)}
                  openTitle="打开本地 Wheel 库"
                  openDisabled={!settings.wheels}
                />
                <div
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 10,
                    marginTop: 8,
                    flexWrap: 'wrap',
                  }}
                >
                  <strong style={{ fontSize: 15, fontVariantNumeric: 'tabular-nums' }}>
                    {wheelsInfo
                      ? `${wheelsInfo.count} 个 · ${cacheSize(wheelsInfo.bytes)}`
                      : wheelsBusy
                        ? '统计中…'
                        : '—'}
                  </strong>
                  <Button
                    type="button"
                    className="ghost sm icon-btn"
                    aria-label="刷新本地 Wheel 库"
                    title="重新统计"
                    disabled={wheelsBusy}
                    onClick={() => void loadWheelsInfo()}
                  >
                    <Icon name="refresh" size={13} />
                  </Button>
                  {wheelsInfo && wheelsInfo.configured && !wheelsInfo.exists ? (
                    <span className="hint is-status">目录不存在</span>
                  ) : null}
                </div>
              </div>
            </div>
          </div>
        )}

        {section === 'runtime' && (
          <>
            <div className="panel">
              <div className="panel-head">
                <h2>运行</h2>
              </div>
              <div className="panel-body grid-2">
                <div className="field">
                  <label>自定义 Git（可选）</label>
                  <TextInput
                    aria-label="自定义 Git 路径" value={settings.paths.git ?? ''}
                    placeholder="留空使用内置 MinGit"
                    onChange={(e) =>
                      setSettings({
                        ...settings,
                        paths: { ...settings.paths, git: e.target.value.trim() || null },
                      })
                    }
                  />
                </div>
                <div className="field">
                  <label>端口区间</label>
                  <div className="inline-input">
                    <TextInput
                      type="number"
                      aria-label="端口区间起始" value={settings.network.portRange[0]}
                      onChange={(e) =>
                        setNetwork({
                          portRange: [Number(e.target.value) || 8188, settings.network.portRange[1]],
                        })
                      }
                    />
                    <span className="faint">至</span>
                    <TextInput
                      type="number"
                      aria-label="端口区间结束" value={settings.network.portRange[1]}
                      onChange={(e) =>
                        setNetwork({
                          portRange: [settings.network.portRange[0], Number(e.target.value) || 8288],
                        })
                      }
                    />
                  </div>
                </div>
                <label className="toggle" style={{ gridColumn: '1 / -1' }}>
                  <Checkbox
                    type="checkbox"
                    checked={settings.install.allowInstallScript}
                    onChange={(e) =>
                      setSettings({
                        ...settings,
                        install: { ...settings.install, allowInstallScript: e.target.checked },
                      })
                    }
                  />
                  允许节点运行 install.py
                </label>
              </div>
            </div>
            <RuntimePanel notify={notify} />
            <ToolchainPanel />
          </>
        )}

        {section === 'sync' && (
          <div className="panel">
            <div className="panel-head">
              <h2>同步与快照</h2>
            </div>
            <div className="panel-body">
              <label className="toggle">
                <Checkbox
                  type="checkbox"
                  checked={settings.sync.removeExtraNodes}
                  onChange={(e) => setSync({ removeExtraNodes: e.target.checked })}
                />
                默认删除目标多余节点
                <span className="hint">成员确认页里仍可逐次取消；删除前会备份。</span>
              </label>
              <label className="toggle">
                <Checkbox
                  type="checkbox"
                  checked={settings.sync.removeExtraModels}
                  onChange={(e) => setSync({ removeExtraModels: e.target.checked })}
                />
                默认删除目标多余模型
                <span className="hint">占空间较大时再开。</span>
              </label>
              <div className="field" style={{ maxWidth: 340, marginTop: 14 }}>
                <label>旧 venv 保留天数</label>
                <TextInput
                  type="number"
                  min={0}
                  max={30}
                  aria-label="旧 venv 保留天数"
                  value={settings.sync.keepOldVenvDays}
                  onChange={(e) =>
                    setSync({
                      keepOldVenvDays: Math.min(30, Math.max(0, Number(e.target.value) || 0)),
                    })
                  }
                />
                <span className="hint">仅在重建 venv 时产生；0 = 成功后立即删除。</span>
              </div>
              <div className="field" style={{ maxWidth: 340, marginTop: 14 }}>
                <label>最多保留快照数量</label>
                <TextInput
                  type="number"
                  min={1}
                  max={2000}
                  step={1}
                  aria-label="最多保留快照数量"
                  value={settings.sync.snapshotMax ?? 20}
                  onChange={(e) =>
                    setSync({
                      snapshotMax: Math.min(
                        2000,
                        Math.max(1, Math.floor(Number(e.target.value) || 1)),
                      ),
                    })
                  }
                />
                <span className="hint">只清理自动快照；手动快照不自动删除。</span>
              </div>
              <div className="field" style={{ maxWidth: 340, marginTop: 14 }}>
                <label>同步缓存保留次数</label>
                <TextInput
                  type="number"
                  min={1}
                  max={100}
                  step={1}
                  aria-label="同步缓存保留次数"
                  value={settings.sync.syncCacheKeep ?? 5}
                  onChange={(e) =>
                    setSync({
                      syncCacheKeep: Math.min(
                        100,
                        Math.max(1, Math.floor(Number(e.target.value) || 1)),
                      ),
                    })
                  }
                />
                <span className="hint">
                  保留最近几次同步产生的代次、目标收件箱和覆盖备份，超出的自动删除；节点压缩缓存和哈希索引不按次数清理。
                </span>
              </div>
              <div className="field" style={{ maxWidth: 520, marginTop: 14 }}>
                <label>当前同步缓存</label>
                <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                  <strong style={{ fontSize: 20 }}>
                    {cacheInfo ? cacheSize(cacheInfo.totalBytes) : cacheBusy ? '统计中…' : '—'}
                  </strong>
                  <Button
                    type="button"
                    className="ghost sm"
                    disabled={cacheBusy}
                    onClick={() => void loadCacheInfo()}
                  >
                    刷新
                  </Button>
                </div>
                <span className="hint is-status">
                  {cacheError
                    ? `读取失败：${cacheError}`
                    : cacheInfo
                      ? cacheBreakdown(cacheInfo)
                      : ''}
                </span>
              </div>
            </div>
          </div>
        )}

        {section === 'update' && <UpdatePanel info={info} notify={notify} />}

        {section === 'logs' && (
          <div className="panel">
            <div className="panel-head">
              <h2>操作日志</h2>
              <span className="badge ghost">每个实例一份</span>
            </div>
            <div className="panel-body">
              <div className="field" style={{ maxWidth: 340 }}>
                <label>日志文件上限（MB）</label>
                <LogSizeField bytes={settings.activity.maxBytes} onChange={setActivityMaxBytes} />
                <span className="hint">超过上限从最旧记录开始丢，保留最新的。</span>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

const GIT_SOURCE: Record<string, string> = {
  bundled: '内置 MinGit（按需下载）',
  custom: '自定义路径',
  system: '系统 PATH',
}

/** uv / Git 两个可选运行时：不再随安装包分发，默认一个都不装，点了才下载。 */
function RuntimePanel({ notify }: { notify: (kind: 'ok' | 'err', msg: string) => void }) {
  const [status, setStatus] = useState<RuntimeStatus | null>(null)
  const [busyKind, setBusyKind] = useState<'uv' | 'git' | null>(null)
  const [progress, setProgress] = useState<RuntimeEvent | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.toolchainStatus())
    } catch (e) {
      setError(errText(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const install = useCallback(
    async (kind: 'uv' | 'git') => {
      setBusyKind(kind)
      setError(null)
      setProgress(null)
      try {
        await api.toolchainInstall(kind, (ev) => setProgress(ev))
        notify('ok', `${kind === 'uv' ? 'uv' : 'Git'} 运行时安装完成`)
      } catch (e) {
        setError(errText(e))
      } finally {
        setBusyKind(null)
        setProgress(null)
        void refresh()
      }
    },
    [notify, refresh],
  )

  const remove = useCallback(
    async (kind: 'uv' | 'git') => {
      setBusyKind(kind)
      setError(null)
      try {
        await api.toolchainRemove(kind)
        notify('ok', `${kind === 'uv' ? 'uv' : 'Git'} 运行时已删除`)
      } catch (e) {
        setError(errText(e))
      } finally {
        setBusyKind(null)
        void refresh()
      }
    },
    [notify, refresh],
  )

  // 「一键装齐」：缺哪个装哪个，已经装好的跳过
  const installAll = useCallback(async () => {
    const need: ('uv' | 'git')[] = []
    if (!status?.uv.installed || !status.uv.version) need.push('uv')
    if (!status?.git.installed || !status.git.version) need.push('git')
    if (need.length === 0) return
    for (const kind of need) {
      await install(kind)
    }
  }, [install, status])

  const rows = [
    {
      kind: 'uv' as const,
      title: 'uv',
      desc: 'Python 环境与依赖安装器',
      size: '18.7 MB',
      part: status?.uv,
      note: null as string | null,
    },
    {
      kind: 'git' as const,
      title: 'Git + git-lfs',
      desc: '拉取 ComfyUI / 节点、切换版本；含大文件（LFS）支持',
      size: '43.7 MB',
      part: status?.git,
      note: status && status.git.installed && !status.gitLfs ? '缺少 git-lfs（可点「修复」补装）' : null,
    },
  ]

  return (
    <div className="panel">
      <div className="panel-head">
        <h2>运行时（可选）</h2>
        <div className="spacer" />
        <Button
          className="primary sm"
          onClick={() => void installAll()}
          disabled={busyKind !== null || (!status ? true : status.uv.installed && status.git.installed && status.gitLfs)}
          title="缺什么装什么，已经装好的会跳过"
        >
          一键安装全部（{status?.downloadMb ?? 62.4} MB）
        </Button>
        <Button className="ghost" onClick={() => void refresh()} disabled={busyKind !== null}>
          刷新
        </Button>
      </div>
      <div className="panel-body" style={{ display: 'grid', gap: 10 }}>
        <div className="hint">
          这两个组件不再随安装包分发（安装包因此从 55 MB 降到约 10 MB），默认都不安装，
          只有你点了「下载」才会联网获取并校验 SHA256。全部装齐约 {status?.downloadMb ?? 62.4} MB。
        </div>

        {rows.map((row) => {
          const installed = row.part?.installed ?? false
          const busy = busyKind === row.kind
          return (
            <div
              key={row.kind}
              style={{
                display: 'grid',
                gap: 6,
                padding: '8px 0',
                borderTop: '1px solid var(--line, rgba(128,128,128,0.18))',
              }}
            >
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                <strong>{row.title}</strong>
                <span className={`badge ${installed ? 'ok' : 'ghost'}`}>
                  {installed ? (row.part?.version ?? '已安装') : '未安装'}
                </span>
                <span className="hint">{row.desc}</span>
                <div className="spacer" />
                {installed ? (
                  <>
                    <span className="faint mono" style={{ fontSize: 11.5 }}>
                      {row.part?.sizeMb?.toFixed(1) ?? '—'} MB
                    </span>
                    <Button
                      className="ghost sm"
                      onClick={() => void remove(row.kind)}
                      disabled={busyKind !== null}
                    >
                      {busy ? '处理中…' : '删除'}
                    </Button>
                  </>
                ) : (
                  <Button
                    className="ghost sm"
                    onClick={() => void install(row.kind)}
                    disabled={busyKind !== null}
                  >
                    {busy ? '下载中…' : `下载（${row.size}）`}
                  </Button>
                )}
                {installed && row.note && (
                  <Button
                    className="ghost sm"
                    onClick={() => void install(row.kind)}
                    disabled={busyKind !== null}
                  >
                    修复
                  </Button>
                )}
              </div>
              {row.note && <div className="hint is-status">{row.note}</div>}
              <div className="faint mono" style={{ fontSize: 11, wordBreak: 'break-all' }}>
                {row.part?.path ?? ''}
              </div>
              {busy && progress && <div className="hint is-status">{progress.message}</div>}
            </div>
          )
        })}

        {error && (
          <div className="banner">
            <div>
              <strong>运行时操作失败：</strong>
              {error}
            </div>
          </div>
        )}

        {status && (
          <div className="hint">
            存放位置：<span className="mono">{status.root}</span>（删掉这里不会影响系统里的 git）
          </div>
        )}
      </div>
    </div>
  )
}

/** 工具链面板：当前实际会用哪个 git（M1/M6 的 clone 与切版本都靠它） */
function ToolchainPanel() {
  const [git, setGit] = useState<GitInfo | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const probe = useCallback(async () => {
    setBusy(true)
    setError(null)
    try {
      setGit(await api.gitInfo())
    } catch (e) {
      setGit(null)
      setError(errText(e))
    } finally {
      setBusy(false)
    }
  }, [])

  useEffect(() => {
    void probe()
  }, [probe])

  return (
    <div className="panel">
      <div className="panel-head">
        <h2>工具链</h2>
        <div className="spacer" />
        <Button className="ghost" onClick={() => void probe()} disabled={busy}>
          {busy ? '检测中…' : '重新检测'}
        </Button>
      </div>
      <div className="panel-body">
        {error && (
          <div className="banner">
            <div>
              <strong>没有可用的 git：</strong>
              {error}
            </div>
          </div>
        )}
        {git && (
          <div className="stack" style={{ gap: 6 }}>
            <div>
              <span className="badge ok">git</span> <span className="mono">{git.version}</span>
            </div>
            <div className="faint mono" style={{ fontSize: 11.5, wordBreak: 'break-all' }}>
              {git.path}
            </div>
            <div className="hint">
              来源：{GIT_SOURCE[git.source] ?? git.source} · 内置版本 {git.bundledVersion}
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

/**
 * 应用自更新面板：只提示 + 用户点确认，绝不自动安装。
 * 安装前必须停掉所有实例——后端 `app_update_install` 会兜底拒绝，
 * 这里提前把「哪个实例还在跑」列出来，省得用户点一次才知道。
 */
function UpdatePanel({
  info,
  notify,
}: {
  info: AppInfo | null
  notify: (kind: 'ok' | 'err', msg: string) => void
}) {
  const [checking, setChecking] = useState(false)
  const [checkError, setCheckError] = useState<string | null>(null)
  const [update, setUpdate] = useState<AppUpdate | null>(null)
  const [installing, setInstalling] = useState(false)
  const [progress, setProgress] = useState<UpdateEvent | null>(null)
  const [installError, setInstallError] = useState<string | null>(null)

  const check = useCallback(
    async (silent = false) => {
      setChecking(true)
      setCheckError(null)
      setInstallError(null)
      try {
        const result = await api.appUpdateCheck()
        setUpdate(result)
        if (result.available && !silent) notify('ok', `发现新版本 ${result.latest}`)
      } catch (e) {
        setUpdate(null)
        setCheckError(errText(e))
      } finally {
        setChecking(false)
      }
    },
    [notify],
  )

  // 打开「更新」页时先静默查一次：用户不必先点按钮才知道有没有新版本。
  // 仍然只有点「检查更新」才弹提示，所以不会打扰。
  useEffect(() => {
    void check(true)
  }, [check])

  const install = useCallback(async () => {
    setInstalling(true)
    setInstallError(null)
    setProgress(null)
    try {
      await api.appUpdateInstall((ev) => setProgress(ev))
      // 成功时后端会重启进程，这里通常执行不到
    } catch (e) {
      setInstallError(errText(e))
      setInstalling(false)
    }
  }, [])

  const running = update?.running ?? []
  const blocked = running.length > 0

  return (
    <div className="panel">
      <div className="panel-head">
        <h2>版本与更新</h2>
        <span className="badge ghost mono">v{info?.version ?? '—'}</span>
        <div className="spacer" />
        <Button className="ghost" onClick={() => void check()} disabled={checking || installing}>
          {checking ? '检查中…' : '检查更新'}
        </Button>
      </div>
      <div className="panel-body" style={{ display: 'grid', gap: 10 }}>
        <div className="hint">
          更新包从 ModelScope 下载，安装前会校验签名。检查只提示，不会自动安装；点「立即更新」后程序会重启。
        </div>

        {checkError && (
          <div className="banner">
            <div>
              <strong>检查更新失败：</strong>
              {checkError}
            </div>
          </div>
        )}

        {update && !update.available && (
          <div className="hint is-status">已是最新版本（v{update.current}）。</div>
        )}

        {blocked && (
          <div className="banner">
            <div>
              <strong>有 {running.length} 个实例在运行：</strong>
              {running.join('、')}
              <div className="hint">更新会重启本程序，请先在首页把实例全部停止再更新。</div>
            </div>
          </div>
        )}

        {update?.available && (
          <div style={{ display: 'grid', gap: 8 }}>
            <div>
              发现新版本 <strong>v{update.latest}</strong>
              <span className="faint"> （当前 v{update.current}）</span>
            </div>
            {update.pubDate && (
              <div className="hint mono">发布于 {new Date(update.pubDate).toLocaleString()}</div>
            )}
            {update.notes && (
              <div className="hint mono" style={{ whiteSpace: 'pre-wrap' }}>
                {update.notes}
              </div>
            )}
            <div>
              <Button
                className="ghost sm"
                onClick={() => void install()}
                disabled={installing || blocked}
              >
                {installing ? '正在更新…' : '立即更新并重启'}
              </Button>
            </div>
            {progress && <div className="hint is-status">{progress.message}</div>}
            {installError && (
              <div className="banner">
                <div>
                  <strong>更新失败：</strong>
                  {installError}
                </div>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
