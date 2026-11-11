import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errText } from '../lib/api'
import type {
  AppInfo,
  AppUpdate,
  GitInfo,
  GlobalSettings,
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
import coreLogo from '../assets/core-logo.svg'
import {
  DEFAULT_DARK_PALETTE,
  DEFAULT_LIGHT_PALETTE,
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

type SettingsSection = 'appearance' | 'network' | 'storage' | 'runtime' | 'sync' | 'logs' | 'update'

const SETTINGS_SECTIONS: { id: SettingsSection; label: string; icon: IconName }[] = [
  { id: 'appearance', label: '外观', icon: 'overview' },
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
        <input
          className="color-chip"
          type="color"
          aria-label={`${label}取色`}
          value={picker}
          onChange={(e) => onChange(e.target.value)}
        />
        <input
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
    <input
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
}: Props) {
  const [section, setSection] = useState<SettingsSection>('appearance')
  const [settings, setSettings] = useState<GlobalSettings | null>(null)
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
      setSettings(next)
      setEditMode(next.ui.theme)
      setBaseline(JSON.stringify(next))
      onPreviewTheme?.(next.ui)
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
    ])
    if (colors.some((value) => normalizeHex(value) === null)) {
      notify('err', '颜色要写成 #RRGGBB，例如 #2b63ff')
      return
    }
    const cardTransparency = Math.min(
      MAX_CARD_TRANSPARENCY,
      Math.max(0, Math.round(Number(ui.cardTransparency) || 0)),
    )
    const normalizePalette = (palette: ThemePalette): ThemePalette => ({
      accent: normalizeHex(palette.accent)!,
      bgTop: normalizeHex(palette.bgTop)!,
      bgMid: normalizeHex(palette.bgMid)!,
      bgBottom: normalizeHex(palette.bgBottom)!,
      cardColor: normalizeHex(palette.cardColor)!,
    })
    const normalized: GlobalSettings = {
      ...settings,
      ui: {
        ...ui,
        dark: normalizePalette(ui.dark),
        light: normalizePalette(ui.light),
        cardTransparency,
      },
    }
    setBusy(true)
    try {
      const saved = await api.settingsSave(normalized)
      setSettings(saved)
      setBaseline(JSON.stringify(saved))
      applyTheme(saved.ui)
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

  const setMirror = (key: keyof GlobalSettings['mirrors'], value: string) =>
    setSettings({ ...settings, mirrors: { ...settings.mirrors, [key]: value.trim() || null } })

  const setWheelRepo = (value: string) =>
    setSettings({ ...settings, wheelRepo: value.trim() || null })

  const applyDraft = (ui: UiOpts) => {
    setSettings({ ...settings, ui })
    onPreviewTheme?.(ui)
  }

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
  // 预览里“卡片”文字跟着卡片亮度，“设置”按钮跟着渐变背景亮度。
  const previewCardInk =
    luminance(mixHex(previewMid, previewPalette.cardColor, previewAlpha)) > 0.22 ? '#141824' : '#f5f6fb'
  const previewBgInk = luminance(previewMid) > 0.4 ? '#141824' : '#f5f6fb'
  const previewCardLine = rgbaHex(previewCardInk, 0.24)
  const previewBgLine = rgbaHex(previewBgInk, 0.24)

  return (
    <div className="settings-layout" ref={layoutRef}>
      <nav className="settings-rail" aria-label="设置分类">
        {SETTINGS_SECTIONS.map((item) => (
          <button
            key={item.id}
            type="button"
            className={`settings-nav${section === item.id ? ' active' : ''}`}
            aria-current={section === item.id ? 'page' : undefined}
            onClick={() => setSection(item.id)}
          >
            <Icon name={item.icon} size={14} />
            <span>{item.label}</span>
          </button>
        ))}
      </nav>

      <div className="settings-main">
        {section === 'appearance' && (
          <div className="panel">
            <div className="panel-head">
              <h2>外观</h2>
              <span className="badge ghost">实时预览</span>
              <div className="spacer" />
              <button className="ghost sm" onClick={resetTheme}>恢复默认配色</button>
            </div>
            <div className="panel-body">
              <div className="appearance-theme-row">
                <Segmented
                  className="appearance-theme-tabs"
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

              <div className="grid-2 appearance-colors">
                <ColorField
                  label="主题色"
                  value={editPalette.accent}
                  hint="按钮、链接与焦点环。"
                  onChange={(value) => setPalette('accent', value)}
                />
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
                  <input
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

              <div
                className="theme-preview"
                style={{
                  background: `linear-gradient(150deg, ${previewTop} 0%, ${previewMid} 52%, ${previewBottom} 100%)`,
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

        {section === 'network' && (
          <div className="panel">
            <div className="panel-head">
              <h2>网络与镜像</h2>
              <span className="badge ghost">所有实例共用</span>
            </div>
            <div className="panel-body">
              <div className="grid-2">
                <div className="field">
                  <label>PyPI 镜像</label>
                  <input
                    aria-label="PyPI 镜像" value={settings.mirrors.pypi ?? ''}
                    onChange={(e) => setMirror('pypi', e.target.value)}
                  />
                </div>
                <div className="field">
                  <label>HuggingFace 镜像</label>
                  <input
                    aria-label="HuggingFace 镜像" value={settings.mirrors.hf ?? ''}
                    placeholder="https://hf-mirror.com"
                    onChange={(e) => setMirror('hf', e.target.value)}
                  />
                </div>
                <div className="field">
                  <label>PyTorch 镜像</label>
                  <input
                    aria-label="PyTorch 镜像" value={settings.mirrors.torch ?? ''}
                    onChange={(e) => setMirror('torch', e.target.value)}
                  />
                  <span className="hint">
                    填镜像根地址即可，安装时自动补当前引擎的 cu130 / cpu 目录。留空用官方源。
                    国内 PyTorch 镜像可能缺少对应 CUDA 目录，或对大 wheel 限速；下载超时请清空此项。
                  </span>
                </div>
                <div className="field">
                  <label>GitHub 加速</label>
                  <input
                    aria-label="GitHub 加速" value={settings.mirrors.githubProxy ?? ''}
                    placeholder="https://gh-proxy.com/https://github.com"
                    onChange={(e) => setMirror('githubProxy', e.target.value)}
                  />
                </div>
                <div className="field" style={{ gridColumn: '1 / -1' }}>
                  <label>云端轮子仓库</label>
                  <input
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
                <input
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
                <span className="hint">不复制文件；新模型存到「下载落点」。</span>
              </div>
              <div className="stack" style={{ marginTop: 14, gap: 14 }}>
                <div className="field">
                  <label>共享输入目录</label>
                  <div className="dir-single">
                    <input
                      className="mono"
                      aria-label="共享输入目录"
                      placeholder={sharedExample('input')}
                      value={settings.storage.sharedInput ?? ''}
                      spellCheck={false}
                      onChange={(e) => setStorage({ sharedInput: e.target.value.trim() || null })}
                    />
                    <button
                      type="button"
                      className="ghost sm"
                      onClick={() => setStorage({ sharedInput: sharedExample('input') })}
                    >
                      用默认位置
                    </button>
                    <button
                      type="button"
                      className="ghost sm icon-btn"
                      aria-label="打开共享输入目录"
                      title="打开目录"
                      disabled={!settings.storage.sharedInput}
                      onClick={() => void openShared(settings.storage.sharedInput)}
                    >
                      <Icon name="folder" size={13} />
                    </button>
                  </div>
                  <span className="hint">留空就用实例自己的目录。</span>
                </div>
                <div className="field">
                  <label>共享输出目录</label>
                  <div className="dir-single">
                    <input
                      className="mono"
                      aria-label="共享输出目录"
                      placeholder={sharedExample('output')}
                      value={settings.storage.sharedOutput ?? ''}
                      spellCheck={false}
                      onChange={(e) => setStorage({ sharedOutput: e.target.value.trim() || null })}
                    />
                    <button
                      type="button"
                      className="ghost sm"
                      onClick={() => setStorage({ sharedOutput: sharedExample('output') })}
                    >
                      用默认位置
                    </button>
                    <button
                      type="button"
                      className="ghost sm icon-btn"
                      aria-label="打开共享输出目录"
                      title="打开目录"
                      disabled={!settings.storage.sharedOutput}
                      onClick={() => void openShared(settings.storage.sharedOutput)}
                    >
                      <Icon name="folder" size={13} />
                    </button>
                  </div>
                  <span className="hint">多人共用时文件会混在一起。</span>
                </div>
              </div>

              <div className="field" style={{ maxWidth: 560, marginTop: 16 }}>
                <label>UV 缓存自动清理阈值</label>
                <div className="dir-single" style={{ maxWidth: 150 }}>
                  <input
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
                  <button
                    type="button"
                    className="ghost sm icon-btn"
                    aria-label="刷新 UV 缓存大小"
                    title="重新统计"
                    disabled={uvBusy}
                    onClick={() => void loadUvCacheInfo()}
                  >
                    <Icon name="refresh" size={13} />
                  </button>
                  {uvConfirm ? (
                    <>
                      <button
                        type="button"
                        className="danger sm"
                        disabled={uvBusy}
                        onClick={() => void clearUvCache()}
                      >
                        {uvBusy ? '清理中…' : '确认清理'}
                      </button>
                      <button
                        type="button"
                        className="ghost sm"
                        disabled={uvBusy}
                        onClick={() => setUvConfirm(false)}
                      >
                        取消
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      className="ghost sm"
                      disabled={uvBusy || !uvInfo || uvInfo.totalBytes === 0}
                      onClick={() => setUvConfirm(true)}
                    >
                      清理
                    </button>
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

              <div className="field" style={{ maxWidth: 560, marginTop: 18 }}>
                <label>本地 Wheel 库</label>
                <div className="dir-single">
                  <input
                    className="mono"
                    aria-label="本地 Wheel 库目录"
                    value={settings.wheels ?? ''}
                    spellCheck={false}
                    onChange={(e) =>
                      setSettings({ ...settings, wheels: e.target.value.trim() || null })
                    }
                  />
                  <button type="button" className="ghost sm" onClick={() => void chooseWheelsDir()}>
                    选择目录
                  </button>
                  <button
                    type="button"
                    className="ghost sm icon-btn"
                    aria-label="打开本地 Wheel 库"
                    title="打开目录"
                    disabled={!settings.wheels}
                    onClick={() => void openShared(settings.wheels ?? null)}
                  >
                    <Icon name="folder" size={13} />
                  </button>
                </div>
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
                  <button
                    type="button"
                    className="ghost sm icon-btn"
                    aria-label="刷新本地 Wheel 库"
                    title="重新统计"
                    disabled={wheelsBusy}
                    onClick={() => void loadWheelsInfo()}
                  >
                    <Icon name="refresh" size={13} />
                  </button>
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
                  <input
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
                    <input
                      type="number"
                      aria-label="端口区间起始" value={settings.network.portRange[0]}
                      onChange={(e) =>
                        setNetwork({
                          portRange: [Number(e.target.value) || 8188, settings.network.portRange[1]],
                        })
                      }
                    />
                    <span className="faint">至</span>
                    <input
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
                  <input
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
                <input
                  type="checkbox"
                  checked={settings.sync.removeExtraNodes}
                  onChange={(e) => setSync({ removeExtraNodes: e.target.checked })}
                />
                默认删除目标多余节点
                <span className="hint">成员确认页里仍可逐次取消；删除前会备份。</span>
              </label>
              <label className="toggle">
                <input
                  type="checkbox"
                  checked={settings.sync.removeExtraModels}
                  onChange={(e) => setSync({ removeExtraModels: e.target.checked })}
                />
                默认删除目标多余模型
                <span className="hint">占空间较大时再开。</span>
              </label>
              <div className="field" style={{ maxWidth: 340, marginTop: 14 }}>
                <label>旧 venv 保留天数</label>
                <input
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
                <input
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
                <input
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
                  <button
                    type="button"
                    className="ghost sm"
                    disabled={cacheBusy}
                    onClick={() => void loadCacheInfo()}
                  >
                    刷新
                  </button>
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
  bundled: '内置 MinGit（随安装包分发）',
  custom: '自定义路径',
  system: '系统 PATH',
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
        <button className="ghost" onClick={() => void probe()} disabled={busy}>
          {busy ? '检测中…' : '重新检测'}
        </button>
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
        <button className="ghost" onClick={() => void check()} disabled={checking || installing}>
          {checking ? '检查中…' : '检查更新'}
        </button>
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
              <button
                className="ghost sm"
                onClick={() => void install()}
                disabled={installing || blocked}
              >
                {installing ? '正在更新…' : '立即更新并重启'}
              </button>
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
