import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { api, errText } from './lib/api'
import type { AppInfo, EnvPlan, InstanceConfig, UiOpts } from './types'
import { TABS, type InstTab } from './lib/state'
import Cluster from './views/Cluster'
import Home from './views/Home'
import InstanceDetail from './views/InstanceDetail'
import TopbarAction from './components/TopbarAction'
import ThemeToggle from './components/ThemeToggle'
import WindowControls from './components/WindowControls'
import SettingsView, { type SettingsActions, type SettingsSection } from './views/Settings'
import Icon from './components/Icon'
import { applyTheme, type ThemeMode } from './lib/theme'
import { syncTrayIcon } from './lib/tray'

type Screen = 'home' | 'instance' | 'global' | 'cluster'
type NavDir = 'none' | 'forward' | 'back'

/**
 * 页面常驻容器：切换顶层页面时只隐藏，不卸载。
 * 同步预览、实例页各标签的表单/滚动位置因此不会因为去了一趟首页就丢掉。
 */
function Page({ active, children }: { active: boolean; children: ReactNode }) {
  return (
    <div
      className={`view${active ? ' is-active' : ''}`}
      style={{ display: active ? 'flex' : 'none' }}
      aria-hidden={!active}
    >
      {children}
    </div>
  )
}

export default function App() {
  const [info, setInfo] = useState<AppInfo | null>(null)
  const [plan, setPlan] = useState<EnvPlan | null>(null)
  const [instances, setInstances] = useState<InstanceConfig[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [tab, setTab] = useState<InstTab>('overview')
  const [screen, setScreen] = useState<Screen>('home')
  const [navDir, setNavDir] = useState<NavDir>('none')
  const [settingsFrom, setSettingsFrom] = useState<Exclude<Screen, 'global'>>('home')
  const [clusterSourceId, setClusterSourceId] = useState<string | null>(null)
  const [settingsDirty, setSettingsDirty] = useState(false)
  const [settingsBusy, setSettingsBusy] = useState(false)
  const [themeMode, setThemeMode] = useState<ThemeMode>('dark')
  const [themeBusy, setThemeBusy] = useState(false)
  const settingsActions = useRef<SettingsActions | null>(null)
  const [confirmLeave, setConfirmLeave] = useState(false)
  const [toast, setToast] = useState<{ kind: 'ok' | 'err'; msg: string } | null>(null)
  /** 启动自检发现的缺失运行时（uv / Git / git-lfs），非空时显示顶部提示条 */
  const [runtimeIssues, setRuntimeIssues] = useState<string[]>([])
  const [runtimeBannerClosed, setRuntimeBannerClosed] = useState(false)
  const [focusSection, setFocusSection] = useState<SettingsSection | null>(null)

  const notify = useCallback((kind: 'ok' | 'err', msg: string) => setToast({ kind, msg }), [])

  // 设置页把保存/放弃/重读句柄登记给底栏；底栏常驻，不随 dirty 出现或消失
  const registerSettingsActions = useCallback((actions: SettingsActions | null) => {
    settingsActions.current = actions
  }, [])

  // 页面/页签常驻后不再统一清零滚动位置；切回来时保留上次看到的位置。

  // 每次启动都自检运行时：缺 uv / Git 时「安装环境」「拉节点」都会失败。
  // 与其让用户操作到一半才报错，不如启动就说清楚并给个一键入口。
  const checkRuntime = useCallback(async () => {
    try {
      const s = await api.toolchainStatus()
      const issues: string[] = []
      if (!s.uv.installed || !s.uv.version) issues.push('uv')
      if (!s.git.installed || !s.git.version) issues.push('Git')
      else if (!s.gitLfs) issues.push('git-lfs')
      setRuntimeIssues(issues)
    } catch {
      // 自检失败不该挡住界面
    }
  }, [])

  // 回到首页/实例页时重新自检，装完运行时回来提示条会自动消失
  useEffect(() => {
    void checkRuntime()
  }, [checkRuntime, screen])

  useEffect(() => {
    api.appInfo().then(setInfo).catch((e) => notify('err', errText(e)))
    api.envPlan().then(setPlan).catch(() => {})
  }, [notify])

  // 主题在首屏就应用；托盘使用透明的原版 logomark
  useEffect(() => {
    api.settingsGet().then((s) => {
      applyTheme(s.ui)
      setThemeMode(s.ui.theme)
      void syncTrayIcon().catch((e) => console.warn('tray icon', e))
    }).catch(() => {})
  }, [])

  const toggleTheme = useCallback(async () => {
    // 设置页保存中时不要并发写 theme：后端虽已串行，前端两个响应的
    // 先后顺序仍可能让界面停在旧主题。
    if (themeBusy || settingsBusy) return
    setThemeBusy(true)
    try {
      const saved = await api.settingsSetTheme(themeMode === 'light' ? 'dark' : 'light')
      applyTheme(saved.ui)
      setThemeMode(saved.ui.theme)
      // 设置页可能正拿着一份旧草稿；不同步的话，点保存会把这次切换覆盖掉。
      settingsActions.current?.syncTheme(saved.ui.theme)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setThemeBusy(false)
    }
  }, [themeBusy, themeMode, settingsBusy, notify])

  // 设置页的配色页签/颜色改动只在内存里预览；点保存才写盘。
  const previewTheme = useCallback((ui: UiOpts) => {
    applyTheme(ui)
    setThemeMode(ui.theme)
  }, [])

  const reload = useCallback(async () => {
    try {
      const list = await api.instanceList()
      setInstances(list)
      return list
    } catch (e) {
      notify('err', errText(e))
      return []
    }
  }, [notify])

  useEffect(() => {
    void reload()
  }, [reload])

  // 有实例在动（启动/停止/安装）时 1.2 s 轮询，否则 5 s 一次就够
  const moving = instances.some((i) =>
    ['starting', 'stopping', 'installing', 'updating', 'running'].includes(i.state),
  )
  useEffect(() => {
    const timer = window.setInterval(() => void reload(), moving ? 1200 : 5000)
    return () => window.clearInterval(timer)
  }, [moving, reload])

  // 状态变化只提醒一次：就绪 / 失败 / 被停掉
  const prevStates = useRef<Record<string, string>>({})
  useEffect(() => {
    const prev = prevStates.current
    for (const inst of instances) {
      const before = prev[inst.id]
      if (before && before !== inst.state) {
        if (inst.state === 'running') notify('ok', `「${inst.name}」已就绪`)
        else if (inst.state === 'error') notify('err', `「${inst.name}」启动失败，去看日志或诊断`)
        else if (before === 'running' && inst.state === 'stopped') notify('ok', `「${inst.name}」已停止`)
      }
      prev[inst.id] = inst.state
    }
  }, [instances, notify])

  useEffect(() => {
    if (!toast) return
    const t = window.setTimeout(() => setToast(null), 3200)
    return () => window.clearTimeout(t)
  }, [toast])

  const selected = useMemo(() => instances.find((i) => i.id === selectedId) ?? null, [instances, selectedId])
  const [detailDirty, setDetailDirty] = useState(false)

  const goHome = useCallback(() => {
    // 保留 selectedId 和页签：实例页只是隐藏，回来时连同标签页状态一起还在。
    setNavDir('back')
    setScreen('home')
  }, [])

  const leaveInstance = useCallback(() => {
    if (
      detailDirty &&
      !window.confirm('当前页签还有未保存的修改，返回首页会丢弃这些修改。继续？')
    ) {
      return
    }
    setDetailDirty(false)
    goHome()
  }, [detailDirty, goHome])

  // 选中的实例消失（被移除）就回到首页
  useEffect(() => {
    if (screen === 'instance' && selectedId && !selected && instances.length > 0) goHome()
  }, [screen, selected, selectedId, instances, goHome])

  const select = useCallback(
    (id: string) => {
      if (
        id !== selectedId &&
        detailDirty &&
        !window.confirm('当前实例的页签还有未保存的修改，切换实例会丢弃这些修改。继续？')
      ) {
        return
      }
      setDetailDirty(false)
      setSelectedId(id)
      setNavDir('forward')
      setScreen('instance')
    },
    [detailDirty, selectedId],
  )

  const openSettings = useCallback(() => {
    setSettingsFrom(screen === 'instance' ? 'instance' : screen === 'cluster' ? 'cluster' : 'home')
    setSettingsBusy(false)
    setNavDir('forward')
    setScreen('global')
  }, [screen])

  const openCluster = useCallback((sourceId?: string) => {
    setClusterSourceId(sourceId ?? null)
    setNavDir('forward')
    setScreen('cluster')
  }, [])

  const closeSettings = useCallback(() => {
    setSettingsDirty(false)
    setSettingsBusy(false)
    setFocusSection(null)
    setNavDir('back')
    setScreen(
      settingsFrom === 'instance' && selected
        ? 'instance'
        : settingsFrom === 'cluster'
          ? 'cluster'
          : 'home',
    )
  }, [settingsFrom, selected])

  const requestCloseSettings = useCallback(() => {
    if (settingsDirty) setConfirmLeave(true)
    else closeSettings()
  }, [settingsDirty, closeSettings])

  const openDataDir = useCallback(() => {
    const dir = info?.dataDir
    if (!dir) return
    api.openPath(dir).catch((e) => notify('err', errText(e)))
  }, [info, notify])

  // Ctrl+1..8 切页签；Esc 关提示
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setToast(null)
        setConfirmLeave(false)
        return
      }
      if (!e.ctrlKey || e.altKey || e.metaKey) return
      const n = Number(e.key)
      if (Number.isInteger(n) && n >= 1 && n <= TABS.length && selected && screen === 'instance') {
        e.preventDefault()
        setTab(TABS[n - 1].id)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selected, screen])

  // 启动自检缺运行时：做成页面内的一条提示（放在「实例」标题上方），不用浮层，
  // 免得压住标题和右侧按钮。
  const runtimeNotice =
    runtimeIssues.length > 0 && !runtimeBannerClosed ? (
      <div className="banner warn" role="status" style={{ alignItems: 'center' }}>
        <span className="mark">
          <Icon name="download" size={13} />
        </span>
        <div style={{ flex: 1, minWidth: 0 }}>
          <strong>缺少运行时：{runtimeIssues.join('、')}</strong>
          <div className="hint" style={{ marginTop: 2 }}>
            安装环境、拉取 ComfyUI 与节点都要用到这些组件；到 设置 → 运行 里按需下载，不点就不会联网。
          </div>
        </div>
        <button
          className="ghost sm"
          onClick={() => {
            setFocusSection('runtime')
            openSettings()
          }}
        >
          去下载
        </button>
        <button
          className="icon-btn"
          onClick={() => setRuntimeBannerClosed(true)}
          aria-label="关闭提示"
          title="本次不再提示"
        >
          ×
        </button>
      </div>
    ) : null

  return (
    <div className="shell">
      <div className="main" data-nav-dir={navDir}>
        <Page active={screen === 'global'}>
          <>
            <header className="topbar" data-tauri-drag-region="deep">
              <div className="topbar-left">
                <button
                  className="icon-btn"
                  onClick={requestCloseSettings}
                  title="返回"
                  aria-label="返回"
                >
                  <Icon name="back" size={16} />
                </button>
              </div>
              <div className="topbar-heading" title="全局设置">
                全局设置
              </div>
              <div className="topbar-right">
                <WindowControls />
              </div>
            </header>
            <div className="content narrow">
              <SettingsView
                info={info}
                notify={notify}
                onDirty={setSettingsDirty}
                onBusy={setSettingsBusy}
                onActions={registerSettingsActions}
                active={screen === 'global'}
                focusSection={focusSection}
                onThemeChange={setThemeMode}
                onPreviewTheme={previewTheme}
              />
            </div>
            <footer className="bottombar">
              <div className="bottombar-inner">
                <button
                  className="icon-btn"
                  onClick={() => settingsActions.current?.reload()}
                  disabled={settingsDirty || settingsBusy}
                  title={settingsDirty ? '先保存或放弃，再从磁盘重读' : '从磁盘重新读取'}
                  aria-label="重新加载设置"
                >
                  <Icon name="refresh" size={15} />
                </button>
                <button
                  className="icon-btn"
                  onClick={openDataDir}
                  disabled={!info}
                  title="打开数据目录"
                  aria-label="打开数据目录"
                >
                  <Icon name="folder" size={15} />
                </button>
                <div className="spacer" />
                <span className={`badge ${settingsDirty ? 'warn' : 'ghost'}`}>
                  {settingsBusy ? '保存中…' : settingsDirty ? '未保存' : '已保存'}
                </span>
                <button
                  className="ghost"
                  onClick={() => settingsActions.current?.discard()}
                  disabled={!settingsDirty || settingsBusy}
                >
                  放弃
                </button>
                <button
                  className="primary"
                  onClick={() => void settingsActions.current?.save()}
                  disabled={!settingsDirty || settingsBusy}
                  title="保存设置（Ctrl+S）"
                >
                  <Icon name="check" size={14} />
                  {settingsBusy ? '保存中…' : '保存设置'}
                </button>
              </div>
            </footer>
          </>
        </Page>

        <Page active={screen === 'cluster'}>
          <>
            <header className="topbar" data-tauri-drag-region="deep">
              <div className="topbar-left">
                <button
                  className="icon-btn"
                  onClick={goHome}
                  title="返回"
                  aria-label="返回"
                >
                  <Icon name="back" size={16} />
                </button>
              </div>
              <div className="topbar-heading" title="局域网同步">
                局域网同步
              </div>
              <div className="topbar-right">
                <TopbarAction
                  icon="settings"
                  label="设置"
                  title="全局设置"
                  onClick={openSettings}
                />
                <ThemeToggle
                  theme={themeMode}
                  busy={themeBusy}
                  onToggle={() => void toggleTheme()}
                />
                <WindowControls />
              </div>
            </header>
            <div className="content">
              <Cluster
                instances={instances}
                notify={notify}
                onBack={goHome}
                initialSourceId={clusterSourceId}
                active={screen === 'cluster'}
              />
            </div>
            <footer className="bottombar cluster-bottombar">
              <div className="bottombar-inner cluster-bottombar-inner">
                <div id="cluster-bottombar-slot" className="cluster-bottombar-slot" />
              </div>
            </footer>
          </>
        </Page>

        {selected && (
          <Page active={screen === 'instance'}>
            <InstanceDetail
              key={selected.id}
              inst={selected}
              info={info}
              plan={plan}
              active={screen === 'instance'}
              tab={tab}
              setTab={setTab}
              notify={notify}
              onReload={reload}
              onBack={leaveInstance}
              onOpenCluster={() => openCluster(selected.id)}
              onOpenSettings={openSettings}
              onDirtyChange={setDetailDirty}
              theme={themeMode}
              onToggleTheme={() => void toggleTheme()}
              themeBusy={themeBusy}
            />
          </Page>
        )}

        <Page active={screen === 'home' || (screen === 'instance' && !selected)}>
          <>
            <header className="topbar" data-tauri-drag-region="deep">
              <div className="topbar-left" />
              <div className="topbar-heading" title="ComfyFleet">
                ComfyFleet
              </div>
              <div className="topbar-right">
                <TopbarAction
                  icon="globe"
                  label="同步"
                  title="局域网同步"
                  onClick={() => openCluster()}
                />
                <TopbarAction
                  icon="settings"
                  label="设置"
                  title="全局设置"
                  onClick={openSettings}
                />
                <ThemeToggle
                  theme={themeMode}
                  busy={themeBusy}
                  onToggle={() => void toggleTheme()}
                />
                <WindowControls />
              </div>
            </header>
            <Home
              info={info}
              plan={plan}
              notify={notify}
              instances={instances}
              onCreated={(id) => {
                void reload().then(() => select(id))
              }}
              onSelect={select}
              onReload={reload}
              notice={runtimeNotice}
            />
            <footer className="bottombar home-bottombar">
              <div className="bottombar-inner home-status">
                <span className="home-status-live">
                  <span className="dot" aria-hidden="true" />
                  本机
                </span>
                {plan?.gpu ? (
                  <>
                    <span className="home-status-fact">{plan.gpu.name}</span>
                    <span className="home-status-sep" aria-hidden="true">
                      ·
                    </span>
                    <span className="home-status-fact is-extra">驱动 {plan.gpu.driverVersion}</span>
                    {plan.gpu.cudaVersion && (
                      <>
                        <span className="home-status-sep" aria-hidden="true">
                          ·
                        </span>
                        <span className="home-status-fact is-extra">CUDA {plan.gpu.cudaVersion}</span>
                      </>
                    )}
                  </>
                ) : (
                  <span className="home-status-fact">
                    {plan ? '未检测到 NVIDIA 显卡' : '正在检测本机硬件…'}
                  </span>
                )}
                <span className="spacer" />
                <button
                  className="icon-btn"
                  onClick={openDataDir}
                  disabled={!info}
                  title={info ? `打开数据目录：${info.dataDir}` : '打开数据目录'}
                  aria-label="打开数据目录"
                >
                  <Icon name="folder" size={15} />
                </button>
                {info && <span className="home-status-version mono">v{info.version}</span>}
              </div>
            </footer>
          </>
        </Page>
      </div>

      {confirmLeave && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setConfirmLeave(false)
          }}
        >
          <div className="modal confirm" role="dialog" aria-modal="true" aria-labelledby="leave-title">
            <div className="modal-head">
              <h2 id="leave-title">放弃未保存的修改？</h2>
            </div>
            <div className="modal-body">
              <p className="hint">全局设置还没保存，返回会丢掉这次改动。可以先取消，回去点「保存设置」。</p>
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <button className="ghost" onClick={() => setConfirmLeave(false)}>
                取消
              </button>
              <button
                className="danger"
                onClick={() => {
                  setConfirmLeave(false)
                  // 常驻的设置页不会卸载，确认放弃后要把草稿真正读回已保存值。
                  settingsActions.current?.discard()
                  closeSettings()
                }}
              >
                放弃并返回
              </button>
            </div>
          </div>
        </div>
      )}

      {toast && (
        <div className={`toast ${toast.kind}`} role="status">
          <span className="dot" />
          {toast.msg}
        </div>
      )}
    </div>
  )
}
