import Button from './components/Button'
import IconTextButton from './components/IconTextButton'
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { api, errText } from './lib/api'
import type {
  AppInfo,
  EnvPlan,
  InstanceConfig,
  IslandIdleShape,
  UiOpts,
} from './types'
import { TABS, type InstTab } from './lib/state'
import Cluster from './views/Cluster'
import Home from './views/Home'
import InstanceDetail from './views/InstanceDetail'
import TopbarAction from './components/TopbarAction'
import ThemeToggle from './components/ThemeToggle'
import WindowControls from './components/WindowControls'
import TaskBar, {
  type IslandAction,
  type TaskBarIdleContent,
  type TaskBarIdleMode,
  type TaskBarNotice,
} from './components/TaskBar'
import {
  BackgroundTasksProvider,
  useTask,
  type BackgroundTask,
} from './components/BackgroundTasks'
import InstanceDownloadTasks from './components/InstanceDownloadTasks'
import SettingsView, { type SettingsActions, type SettingsSection } from './views/Settings'
import Icon from './components/Icon'
import {
  ISLAND_BOTTOM_GAP_DEFAULT,
  ISLAND_SHAPE_ACTION_GAP_DEFAULT,
  normalizeIslandShape,
} from './components/IslandShape'
import Modal from './components/Modal'
import { PopPresence } from './components/PopPresence'
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

type RunStage = 'idle' | 'running' | 'ok' | 'failed' | 'cancelled'

/**
 * 把实例的启动过程投到全局任务岛。
 * 状态由 App 的实例轮询驱动：starting → 启动中，running / error / stopped → 结束态。
 */
function InstanceRunTask({
  inst,
  onOpen,
  lingerMs,
}: {
  inst: InstanceConfig
  onOpen: (id: string) => void
  lingerMs: number
}) {
  const [stage, setStage] = useState<RunStage>(
    inst.kind === 'local' && inst.state === 'starting' ? 'running' : 'idle',
  )
  const prev = useRef({ state: inst.state, lastStartedAt: inst.lastStartedAt })
  const settleTimer = useRef<number | null>(null)
  const runShownAt = useRef<number | null>(null)
  const stageRef = useRef<RunStage>(stage)
  stageRef.current = stage

  useEffect(() => {
    return () => {
      if (settleTimer.current !== null) window.clearTimeout(settleTimer.current)
    }
  }, [])

  // 启动取消不是错误；短暂显示后让任务岛回到空闲状态，不把取消态一直挂着。
  useEffect(() => {
    if (stage !== 'cancelled') return
    const timer = window.setTimeout(() => setStage('idle'), lingerMs)
    return () => window.clearTimeout(timer)
  }, [stage, lingerMs])

  useEffect(() => {
    const before = prev.current
    prev.current = { state: inst.state, lastStartedAt: inst.lastStartedAt }
    if (inst.kind !== 'local') return

    const finish = (next: RunStage) => {
      if (settleTimer.current !== null) {
        window.clearTimeout(settleTimer.current)
        settleTimer.current = null
      }
      if (next !== 'ok') {
        setStage(next)
        return
      }
      // 启动太快时也把发射动效完整放完，不直接跳到完成。
      const shown = runShownAt.current
      const delay = shown === null ? 0 : Math.max(0, 900 - (Date.now() - shown))
      if (delay === 0) {
        setStage('ok')
        return
      }
      settleTimer.current = window.setTimeout(() => {
        settleTimer.current = null
        setStage('ok')
      }, delay)
    }

    // 状态可能在没有被 1.2 s 轮询看到的间隙里直接冲到 running；按启动时间补一条。
    if (settleTimer.current !== null && inst.state !== 'running') {
      window.clearTimeout(settleTimer.current)
      settleTimer.current = null
    }

    if (inst.state === 'starting') {
      if (before.state !== 'starting') runShownAt.current = Date.now()
      setStage('running')
      return
    }
    // 停止过程可能被轮询看到（starting → stopping → stopped），不能只在
    // “刚好从 starting 跳到终态”时收尾，否则任务会一直停在启动中。
    if (
      stageRef.current === 'running' &&
      (inst.state === 'stopping' || inst.state === 'stopped')
    ) {
      finish('cancelled')
      return
    }
    if (stageRef.current === 'running' && inst.state === 'error') {
      finish('failed')
      return
    }
    if (before.state === 'starting') {
      if (inst.state === 'running') finish('ok')
      else if (inst.state === 'error') finish('failed')
      else if (inst.state === 'stopped') finish('cancelled')
      return
    }
    if (before.lastStartedAt !== inst.lastStartedAt && inst.state === 'error') {
      setStage('failed')
      return
    }
    if (inst.state === 'running' && before.lastStartedAt !== inst.lastStartedAt) {
      runShownAt.current = Date.now()
      setStage('running')
      finish('ok')
    }
  }, [inst.kind, inst.state, inst.lastStartedAt])

  const state: BackgroundTask['state'] | null =
    stage === 'running' || stage === 'ok' || stage === 'failed' || stage === 'cancelled'
      ? stage
      : null
  const title =
    stage === 'running'
      ? `正在启动 ${inst.name}`
      : stage === 'ok'
        ? `已启动 ${inst.name}`
        : stage === 'failed'
          ? `启动失败 ${inst.name}`
          : stage === 'cancelled'
            ? `启动已取消 ${inst.name}`
            : ''
  const phase =
    stage === 'running'
      ? '等待就绪'
      : stage === 'ok'
        ? '已就绪'
        : stage === 'failed'
          ? '启动失败'
          : stage === 'cancelled'
            ? '已取消'
            : ''
  const log =
    stage === 'running'
      ? '等待 ComfyUI 健康检查'
      : stage === 'ok'
        ? 'ComfyUI 已就绪'
        : stage === 'failed'
          ? '启动未通过健康检查'
          : stage === 'cancelled'
            ? '启动已取消'
            : ''

  useTask(
    stage !== 'idle' && state
      ? {
          id: `start-${inst.id}`,
          title,
          phase,
          state,
          percent: stage === 'ok' ? 100 : undefined,
          detail: `端口 ${inst.server.port}`,
          log,
          kind: 'start',
          onOpen: () => onOpen(inst.id),
        }
      : null,
  )

  return null
}

export default function App() {
  const [info, setInfo] = useState<AppInfo | null>(null)
  const [plan, setPlan] = useState<EnvPlan | null>(null)
  const [instances, setInstances] = useState<InstanceConfig[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [tab, setTab] = useState<InstTab>('overview')
  const [consoleOpen, setConsoleOpen] = useState(false)
  const [screen, setScreen] = useState<Screen>('home')
  const [navDir, setNavDir] = useState<NavDir>('none')
  const [settingsFrom, setSettingsFrom] = useState<Exclude<Screen, 'global'>>('home')
  const [clusterSourceId, setClusterSourceId] = useState<string | null>(null)
  const [settingsDirty, setSettingsDirty] = useState(false)
  const [settingsBusy, setSettingsBusy] = useState(false)
  const [themeMode, setThemeMode] = useState<ThemeMode>('dark')
  const [themeBusy, setThemeBusy] = useState(false)
  const [taskBarIdle, setTaskBarIdle] = useState<TaskBarIdleMode>('auto')
  const [noticeSeconds, setNoticeSeconds] = useState(3)
  const [islandIdleContent, setIslandIdleContent] = useState<TaskBarIdleContent>('animation')
  const [islandIdleShape, setIslandIdleShape] = useState<IslandIdleShape>('chevrons')
  const [islandWidth, setIslandWidth] = useState(84)
  const [islandAnimationSpeed, setIslandAnimationSpeed] = useState(100)
  const [islandShapeInset, setIslandShapeInset] = useState(11)
  const [islandShapeActionGap, setIslandShapeActionGap] = useState(
    ISLAND_SHAPE_ACTION_GAP_DEFAULT,
  )
  const [islandBottomGap, setIslandBottomGap] = useState(ISLAND_BOTTOM_GAP_DEFAULT)
  const [islandGlow, setIslandGlow] = useState(true)
  const [islandGlowColor, setIslandGlowColor] = useState('#5b8dff')
  const [islandGlowRange, setIslandGlowRange] = useState(24)
  const [islandIdleText, setIslandIdleText] = useState('控制台')
  const settingsActions = useRef<SettingsActions | null>(null)
  const [confirmLeave, setConfirmLeave] = useState(false)
  const [notice, setNotice] = useState<TaskBarNotice | null>(null)
  const [islandActions, setIslandActions] = useState<IslandAction[]>([])
  const registerIslandActions = useCallback((next: IslandAction[] | null) => {
    setIslandActions(next ?? [])
  }, [])
  /** 启动自检发现的缺失运行时（uv / Git / git-lfs），非空时显示顶部提示条 */
  const [runtimeIssues, setRuntimeIssues] = useState<string[]>([])
  const [runtimeBannerClosed, setRuntimeBannerClosed] = useState(false)
  const [focusSection, setFocusSection] = useState<SettingsSection | null>(null)

  const notify = useCallback((kind: 'ok' | 'err', msg: string) => setNotice({ kind, msg }), [])

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

  // 这是桌面应用，右键菜单没有可用项；全局关掉，避免在终端/标题栏/按钮上误弹。
  useEffect(() => {
    const block = (event: MouseEvent) => event.preventDefault()
    document.addEventListener('contextmenu', block, true)
    return () => document.removeEventListener('contextmenu', block, true)
  }, [])

  // 主题在首屏就应用；托盘使用透明的原版 logomark
  useEffect(() => {
    api.settingsGet().then((s) => {
      applyTheme(s.ui)
      setThemeMode(s.ui.theme)
      setTaskBarIdle(s.ui.taskBarIdle)
      setNoticeSeconds(s.ui.noticeSeconds)
      setIslandIdleContent(s.ui.islandIdleContent)
      setIslandIdleShape(normalizeIslandShape(s.ui.islandIdleShape))
      setIslandWidth(s.ui.islandWidth)
      setIslandAnimationSpeed(s.ui.islandAnimationSpeed)
      setIslandShapeInset(s.ui.islandShapeInset)
      setIslandShapeActionGap(s.ui.islandShapeActionGap)
      setIslandBottomGap(s.ui.islandBottomGap)
      setIslandGlow(s.ui.islandGlow)
      setIslandGlowColor(s.ui.islandGlowColor)
      setIslandGlowRange(s.ui.islandGlowRange)
      setIslandIdleText(s.ui.islandIdleText)
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
      setTaskBarIdle(saved.ui.taskBarIdle)
      setNoticeSeconds(saved.ui.noticeSeconds)
      setIslandIdleContent(saved.ui.islandIdleContent)
      setIslandIdleShape(normalizeIslandShape(saved.ui.islandIdleShape))
      setIslandWidth(saved.ui.islandWidth)
      setIslandAnimationSpeed(saved.ui.islandAnimationSpeed)
      setIslandShapeInset(saved.ui.islandShapeInset)
      setIslandShapeActionGap(saved.ui.islandShapeActionGap)
      setIslandBottomGap(saved.ui.islandBottomGap)
      setIslandGlow(saved.ui.islandGlow)
      setIslandGlowColor(saved.ui.islandGlowColor)
      setIslandGlowRange(saved.ui.islandGlowRange)
      setIslandIdleText(saved.ui.islandIdleText)
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
    setTaskBarIdle(ui.taskBarIdle)
    setNoticeSeconds(ui.noticeSeconds)
    setIslandIdleContent(ui.islandIdleContent)
    setIslandIdleShape(normalizeIslandShape(ui.islandIdleShape))
    setIslandWidth(ui.islandWidth)
    setIslandAnimationSpeed(ui.islandAnimationSpeed)
    setIslandShapeInset(ui.islandShapeInset)
    setIslandShapeActionGap(ui.islandShapeActionGap)
    setIslandBottomGap(ui.islandBottomGap)
    setIslandGlow(ui.islandGlow)
    setIslandGlowColor(ui.islandGlowColor)
    setIslandGlowRange(ui.islandGlowRange)
    setIslandIdleText(ui.islandIdleText)
  }, [])

  const reloadSeq = useRef(0)
  const reloadFailed = useRef(false)
  const reload = useCallback(async () => {
    const seq = ++reloadSeq.current
    try {
      const list = await api.instanceList()
      if (seq !== reloadSeq.current) return list
      setInstances(list)
      reloadFailed.current = false
      return list
    } catch (e) {
      // 轮询失败时只提示一次；否则每次轮询都会重置提示计时器，错误永远不消失。
      if (seq === reloadSeq.current && !reloadFailed.current) {
        reloadFailed.current = true
        notify('err', errText(e))
      }
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

  // 启停状态由灵动岛和实例任务自己展示；这里只额外提醒失败。
  const prevStates = useRef<Record<string, string>>({})
  useEffect(() => {
    const prev = prevStates.current
    for (const inst of instances) {
      const before = prev[inst.id]
      if (before && before !== inst.state && inst.state === 'error') {
        notify('err', `「${inst.name}」启动失败，去看日志或诊断`)
      }
      prev[inst.id] = inst.state
    }
  }, [instances, notify])

  useEffect(() => {
    if (!notice) return
    const t = window.setTimeout(() => setNotice(null), noticeSeconds * 1000)
    return () => window.clearTimeout(t)
  }, [notice, noticeSeconds])

  const selected = useMemo(() => instances.find((i) => i.id === selectedId) ?? null, [instances, selectedId])
  const [detailDirty, setDetailDirty] = useState(false)
  // 离开实例/切换实例前先确认；确认后必须真正卸载旧实例页，否则它的草稿还留在缓存里。
  const [pendingDetail, setPendingDetail] = useState<
    null | { kind: 'home' } | { kind: 'select'; id: string; task: boolean }
  >(null)

  useEffect(() => {
    if (screen !== 'instance' || tab === 'console') setConsoleOpen(false)
  }, [screen, tab])

  const goHome = useCallback(() => {
    // 保留 selectedId 和页签：实例页只是隐藏，回来时连同标签页状态一起还在。
    setNavDir('back')
    setScreen('home')
  }, [])

  const leaveInstance = useCallback(() => {
    if (detailDirty) {
      setPendingDetail({ kind: 'home' })
      return
    }
    goHome()
  }, [detailDirty, goHome])

  // 选中的实例消失（被移除）就回到首页
  useEffect(() => {
    if (screen === 'instance' && selectedId && !selected && instances.length > 0) goHome()
  }, [screen, selected, selectedId, instances, goHome])

  const selectNow = useCallback((id: string, openConsole = false) => {
    setSelectedId(id)
    setNavDir('forward')
    setScreen('instance')
    setConsoleOpen(openConsole)
  }, [])

  const select = useCallback(
    (id: string, openConsole = false) => {
      if (id === selectedId) {
        // 已经缓存的实例页：保留未保存草稿，不要用一次重复点击把它清掉。
        selectNow(id, openConsole)
        return
      }
      if (detailDirty) {
        setPendingDetail({ kind: 'select', id, task: openConsole })
        return
      }
      selectNow(id, openConsole)
    },
    [detailDirty, selectedId, selectNow],
  )

  const openStartTask = useCallback(
    (id: string) => {
      if (id !== selectedId && detailDirty) {
        setPendingDetail({ kind: 'select', id, task: true })
        return
      }
      select(id, true)
      setTab('overview')
    },
    [detailDirty, selectedId, select],
  )

  // 下载任务在灵动岛上点开时，直接打开该实例的控制台，而不是只切到实例页。
  const openInstanceConsole = useCallback(
    (id: string) => select(id, true),
    [select],
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
        setNotice(null)
        setConfirmLeave(false)
        return
      }
      if (!e.ctrlKey || e.altKey || e.metaKey) return
      const target = e.target as HTMLElement | null
      // 输入框和弹层里不抢数字键：Ctrl+1 在文本里可能被用户当编辑快捷键。
      if (
        target?.closest('input, textarea, select, [contenteditable="true"]') ||
        document.querySelector('.modal-backdrop')
      ) {
        return
      }
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
        <Button
          className="ghost sm"
          onClick={() => {
            setFocusSection('runtime')
            openSettings()
          }}
        >
          去下载
        </Button>
        <Button
          className="icon-btn"
          onClick={() => setRuntimeBannerClosed(true)}
          aria-label="关闭提示"
          title="本次不再提示"
        >
          ×
        </Button>
      </div>
    ) : null

  return (
    <BackgroundTasksProvider successLingerMs={noticeSeconds * 1000}>
    <InstanceDownloadTasks instances={instances} onOpen={openInstanceConsole} />
    <div className="shell">
      <div className="main" data-nav-dir={navDir}>
        <Page active={screen === 'global'}>
          <>
            <header className="topbar" data-tauri-drag-region="deep">
              <div className="topbar-left">
                <Button
                  className="icon-btn"
                  onClick={requestCloseSettings}
                  title="返回"
                  aria-label="返回"
                >
                  <Icon name="back" size={16} />
                </Button>
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
                <Button
                  className="icon-btn"
                  onClick={() => settingsActions.current?.reload()}
                  disabled={settingsDirty || settingsBusy}
                  title={settingsDirty ? '先保存或放弃，再从磁盘重读' : '从磁盘重新读取'}
                  aria-label="重新加载设置"
                >
                  <Icon name="refresh" size={15} />
                </Button>
                <Button
                  className="icon-btn"
                  onClick={openDataDir}
                  disabled={!info}
                  title="打开数据目录"
                  aria-label="打开数据目录"
                >
                  <Icon name="folder" size={15} />
                </Button>
                <div className="spacer" />
                <span className={`badge ${settingsDirty ? 'warn' : 'ghost'}`}>
                  {settingsBusy ? '保存中…' : settingsDirty ? '未保存' : '已保存'}
                </span>
                <PopPresence show={settingsDirty && !settingsBusy}>
                  <Button
                    className="ghost"
                    onClick={() => settingsActions.current?.discard()}
                    disabled={!settingsDirty || settingsBusy}
                  >
                    放弃
                  </Button>
                </PopPresence>
                <PopPresence show={settingsDirty && !settingsBusy}>
                  <IconTextButton
                    className="primary"
                    onClick={() => void settingsActions.current?.save()}
                    disabled={!settingsDirty || settingsBusy}
                    title="保存设置（Ctrl+S）"
                   icon="check" iconSize={14}>
                    
                    {settingsBusy ? '保存中…' : '保存设置'}
                  </IconTextButton>
                </PopPresence>
              </div>
            </footer>
          </>
        </Page>

        <Page active={screen === 'cluster'}>
          <>
            <header className="topbar" data-tauri-drag-region="deep">
              <div className="topbar-left">
                <Button
                  className="icon-btn"
                  onClick={goHome}
                  title="返回"
                  aria-label="返回"
                >
                  <Icon name="back" size={16} />
                </Button>
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
              consoleOpen={consoleOpen}
              onConsoleOpenChange={setConsoleOpen}
              onIslandActions={registerIslandActions}
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
                  <span className="home-status-live-label">本机</span>
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
                <Button
                  className="icon-btn"
                  onClick={openDataDir}
                  disabled={!info}
                  title={info ? `打开数据目录：${info.dataDir}` : '打开数据目录'}
                  aria-label="打开数据目录"
                >
                  <Icon name="folder" size={15} />
                </Button>
                {info && <span className="home-status-version mono">v{info.version}</span>}
              </div>
            </footer>
          </>
        </Page>
      </div>

      {instances.map((inst) => (
        <InstanceRunTask
          key={inst.id}
          inst={inst}
          onOpen={openStartTask}
          lingerMs={noticeSeconds * 1000}
        />
      ))}

      <TaskBar
        idleMode={taskBarIdle}
        idleContent={islandIdleContent}
        idleShape={islandIdleShape}
        idleMinWidth={islandWidth}
        animationSpeed={islandAnimationSpeed}
        shapeInset={islandShapeInset}
        shapeActionGap={islandShapeActionGap}
        bottomGap={islandBottomGap}
        glow={islandGlow}
        glowColor={islandGlowColor}
        glowRange={islandGlowRange}
        idleText={islandIdleText}
        notice={notice}
        onNoticeDismiss={() => setNotice(null)}
        concealed={consoleOpen}
        actions={islandActions}
        idle={
            screen === 'instance' && selected && tab !== 'console'
              ? { onOpen: () => setConsoleOpen(true) }
              : null
        }
      />

      {pendingDetail && (
        <Modal
          className="confirm"
          labelledBy="discard-detail-title"
          onClose={() => setPendingDetail(null)}
        >
          <div className="modal-head">
            <h2 id="discard-detail-title">放弃未保存的修改？</h2>
          </div>
          <div className="modal-foot">
            <div className="spacer" />
            <Button className="ghost" onClick={() => setPendingDetail(null)}>
              取消
            </Button>
            <Button
              className="danger"
              onClick={() => {
                const pending = pendingDetail
                setPendingDetail(null)
                if (!pending) return
                setDetailDirty(false)
                if (pending.kind === 'home') {
                  // 卸载常驻的实例页，草稿才会真正丢弃。
                  setSelectedId(null)
                  setConsoleOpen(false)
                  goHome()
                  return
                }
                selectNow(pending.id, pending.task)
                if (pending.task) setTab('overview')
              }}
            >
              {pendingDetail.kind === 'home' ? '放弃并返回' : '放弃并切换'}
            </Button>
          </div>
        </Modal>
      )}

      {confirmLeave && (
        <Modal
          className="confirm"
          labelledBy="leave-title"
          onClose={() => setConfirmLeave(false)}
        >
            <div className="modal-head">
              <h2 id="leave-title">放弃未保存的修改？</h2>
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <Button className="ghost" onClick={() => setConfirmLeave(false)}>
                取消
              </Button>
              <Button
                className="danger"
                onClick={() => {
                  setConfirmLeave(false)
                  // 常驻的设置页不会卸载，确认放弃后要把草稿真正读回已保存值。
                  settingsActions.current?.discard()
                  closeSettings()
                }}
              >
                放弃并返回
              </Button>
            </div>
        </Modal>
      )}

    </div>
    </BackgroundTasksProvider>
  )
}
