import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import { api, errText } from '../lib/api'
import { startupDepsTargets } from '../lib/deps'
import type {
  AppInfo,
  EnvPlan,
  Health,
  InstanceConfig,
  InstallEvent,
  InstallReport,
  VersionEvent,
} from '../types'
import { TABS, stateText, type InstTab } from '../lib/state'
import { comfyPath } from '../lib/paths'
import { type TabSaveHandle } from '../lib/useTabSave'
import { useInstanceLogStream } from '../lib/useInstanceLogStream'
import { deriveLogTask } from '../lib/runProgress'
import ConsoleDock from '../components/ConsoleDock'
import OperationDialog, {
  operationStageLabel,
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'
import ConsoleView from './Console'
import NodesView from './Nodes'
import ModelExplorer from './ModelExplorer'
import VersionsView from './Versions'
import InstanceSettings from './InstanceSettings'
import LaunchArgs from './LaunchArgs'
import InstanceStorageView from './InstanceStorage'
import SnapshotsView from './Snapshots'
import Activity from './Activity'
import WheelPanel from './WheelPanel'
import DiagModal from '../components/DiagModal'
import Icon, { type IconName } from '../components/Icon'
import TopbarAction from '../components/TopbarAction'
import ThemeToggle from '../components/ThemeToggle'
import WindowControls from '../components/WindowControls'
import type { ThemeMode } from '../lib/theme'

interface Props {
  inst: InstanceConfig
  info: AppInfo | null
  plan: EnvPlan | null
  active: boolean
  tab: InstTab
  setTab: (t: InstTab) => void
  notify: (kind: 'ok' | 'err', msg: string) => void
  onReload: () => Promise<InstanceConfig[]>
  onBack: () => void
  onOpenCluster: () => void
  onOpenSettings: () => void
  /** 当前页签有未保存草稿时通知外层，切实例/回首页前拦截。 */
  onDirtyChange?: (dirty: boolean) => void
  theme: ThemeMode
  onToggleTheme: () => void
  themeBusy?: boolean
}

const STEP_LABEL: Record<string, string> = {
  venv: '创建虚拟环境',
  torch: '安装 torch 三元组',
  triton: '安装 triton',
  comfy: '下载 ComfyUI 本体',
  requirements: '安装 ComfyUI 依赖',
  verify: '校验环境',
}

const OVERVIEW_SPLIT_KEY = 'comfyfleet.overview.split'
const DEFAULT_OVERVIEW_SPLIT = 0.68
const MIN_OVERVIEW_SPLIT = 0.35
const MAX_OVERVIEW_SPLIT = 0.72
const MODEL_MIN_WIDTH = 280
const NODE_MIN_WIDTH = 450

function readOverviewSplit(): number {
  try {
    const saved = Number(localStorage.getItem(OVERVIEW_SPLIT_KEY))
    if (Number.isFinite(saved) && saved >= MIN_OVERVIEW_SPLIT && saved <= MAX_OVERVIEW_SPLIT) {
      return saved
    }
  } catch {
    /* localStorage 不可用时用默认值 */
  }
  return DEFAULT_OVERVIEW_SPLIT
}

function clampOverviewSplit(value: number, width: number): number {
  const byModel =
    width > 0 ? (width - MODEL_MIN_WIDTH - 18) / width : MAX_OVERVIEW_SPLIT
  const byNodes = width > 0 ? NODE_MIN_WIDTH / width : MIN_OVERVIEW_SPLIT
  const max = Math.max(MIN_OVERVIEW_SPLIT, Math.min(MAX_OVERVIEW_SPLIT, byModel))
  const min = Math.min(max, Math.max(MIN_OVERVIEW_SPLIT, Math.min(MAX_OVERVIEW_SPLIT, byNodes)))
  return Math.min(max, Math.max(min, value))
}

const TAB_ICON: Record<InstTab, IconName> = {
  overview: 'overview',
  console: 'console',
  versions: 'versions',
  launch: 'power',
  storage: 'box',
  snapshots: 'clock',
  activity: 'diagnostics',
  settings: 'settings',
}

function gb(mib: number): string {
  return (mib / 1024).toFixed(1)
}

/** 页签只挂载访问过的，切走后隐藏；这样表单、筛选和滚动位置都能保留。 */
function TabPane({
  active,
  mounted,
  children,
}: {
  active: boolean
  mounted: boolean
  children: ReactNode
}) {
  if (!mounted) return null
  return (
    <div
      className={`tab-view${active ? ' is-active' : ''}`}
      style={{ display: active ? 'flex' : 'none' }}
    >
      {children}
    </div>
  )
}

/** 通道的界面说法：stable/dev 是内部值，别直接丢给用户看 */
function channelText(channel: string): string {
  return channel === 'dev' ? '开发版' : '稳定版'
}

/**
 * 模型卡片：选择这个实例生效的模型目录，直接浏览目录内容；拖文件进来即复制。
 */
function ModelCard({ inst, active, notify, onOpenStorage }: {
  inst: InstanceConfig
  active: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
  onOpenStorage: () => void
}) {
  return (
    <ModelExplorer
      inst={inst}
      active={active}
      notify={notify}
      onOpenStorage={onOpenStorage}
    />
  )
}

export default function InstanceDetail(props: Props) {
  const {
    inst,
    plan,
    active,
    tab,
    setTab,
    notify,
    onReload,
    onBack,
    onOpenCluster,
    onOpenSettings,
    onDirtyChange,
    theme,
    onToggleTheme,
    themeBusy,
  } = props

  const running = inst.state === 'starting' || inst.state === 'running'
  // 取消安装/安装失败后 state=error，但 ComfyUI 根本不存在；不能因为状态不是
  // uninitialized 就当成“已安装”，否则重试按钮会消失、启动也放行。
  const installed = Boolean(inst.comfy.version)

  const [busyRun, setBusyRun] = useState(false)
  const [health, setHealth] = useState<Health | null>(null)
  const [readAt, setReadAt] = useState<string | null>(null)
  const { lines: tail, clear: clearTail } = useInstanceLogStream(inst.id, active, 300)
  const [progress, setProgress] = useState<{
    title: string
    logs: string[]
    running: boolean
    failed?: boolean
    cancelled?: boolean
  } | null>(null)
  const [installing, setInstalling] = useState(false)
  const [cancelling, setCancelling] = useState(false)
  const [progressOpen, setProgressOpen] = useState(false)
  const [opEvents, setOpEvents] = useState<OperationLine[]>([])
  const [opPhase, setOpPhase] = useState('准备中')
  const [opPercent, setOpPercent] = useState(0)
  const [opTab, setOpTab] = useState<OperationTab>('detail')
  const [envBusy, setEnvBusy] = useState(false)
  const [copied, setCopied] = useState(false)
  const [tabSaves, setTabSaves] = useState<Partial<Record<InstTab, TabSaveHandle>>>({})
  const [mountedTabs, setMountedTabs] = useState<Record<InstTab, boolean>>(() =>
    Object.fromEntries(TABS.map((t) => [t.id, t.id === 'overview'])) as Record<InstTab, boolean>,
  )
  // 页签过渡方向：去右边的页签从右侧进入，回左边的页签从左侧进入。
  const prevTab = useRef<InstTab>(tab)
  const tabDir = useMemo(() => {
    const from = TABS.findIndex((t) => t.id === prevTab.current)
    const to = TABS.findIndex((t) => t.id === tab)
    return to < from ? 'back' : 'forward'
  }, [tab])
  useEffect(() => {
    prevTab.current = tab
  }, [tab])
  const [diagOpen, setDiagOpen] = useState(false)
  // 诊断弹窗里点「补依赖」→ 关弹窗、回到概览、把目标塞给节点卡片
  const [depsIntent, setDepsIntent] = useState<{ names: string[] | null; key: number } | null>(null)
  // 启动时的依赖提醒只弹一次（每次实例/运行状态变化算一次）
  const reminded = useRef(false)

  const [overviewSplit, setOverviewSplit] = useState(readOverviewSplit)
  const overviewGridRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    try {
      localStorage.setItem(OVERVIEW_SPLIT_KEY, String(overviewSplit))
    } catch {
      /* 忽略存储失败，拖动仍然有效 */
    }
  }, [overviewSplit])

  // 窗口变窄时重新夹紧比例，避免从宽屏保存的窄节点卡把控件挤出去
  useEffect(() => {
    const el = overviewGridRef.current
    if (!el) return
    const apply = () => {
      const width = el.getBoundingClientRect().width
      if (width <= 0) return
      setOverviewSplit((prev) => {
        const next = clampOverviewSplit(prev, width)
        return Math.abs(next - prev) < 0.0001 ? prev : next
      })
    }
    apply()
    const observer = new ResizeObserver(apply)
    observer.observe(el)
    return () => observer.disconnect()
  }, [tab])

  const setOverviewSplitClamped = (value: number) => {
    const width = overviewGridRef.current?.getBoundingClientRect().width ?? 0
    setOverviewSplit(clampOverviewSplit(value, width))
  }

  const beginOverviewSplitDrag = (startX: number) => {
    const el = overviewGridRef.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    if (rect.width <= 0) return
    const startWidth = rect.width * overviewSplit
    const move = (event: PointerEvent) => {
      setOverviewSplit(
        clampOverviewSplit((startWidth + event.clientX - startX) / rect.width, rect.width),
      )
    }
    const end = () => {
      document.body.classList.remove('sync-resizing')
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', end)
      window.removeEventListener('pointercancel', end)
    }
    document.body.classList.add('sync-resizing')
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', end)
    window.addEventListener('pointercancel', end)
  }

  const registerTabSave = useCallback((tab: InstTab, next: TabSaveHandle | null) => {
    setTabSaves((prev) => {
      if (!next) {
        if (!(tab in prev)) return prev
        const copy = { ...prev }
        delete copy[tab]
        return copy
      }
      if (prev[tab] === next) return prev
      return { ...prev, [tab]: next }
    })
  }, [])
  const registerLaunchSave = useCallback(
    (next: TabSaveHandle | null) => registerTabSave('launch', next),
    [registerTabSave],
  )
  const registerStorageSave = useCallback(
    (next: TabSaveHandle | null) => registerTabSave('storage', next),
    [registerTabSave],
  )
  const registerSettingsSave = useCallback(
    (next: TabSaveHandle | null) => registerTabSave('settings', next),
    [registerTabSave],
  )

  const tabSave = tabSaves[tab] ?? null
  const anyTabDirty = Object.values(tabSaves).some((s) => s?.dirty)

  // 当前页签的蓝色底由一条滑动的底片承担：切页签时它从旧位置滑到新位置。
  // 量一次 active 按钮的 offsetLeft/offsetWidth，宽度变化（出现未保存圆点）时重测。
  const tabsRef = useRef<HTMLDivElement>(null)
  const [tabGlider, setTabGlider] = useState<{ x: number; w: number } | null>(null)
  useLayoutEffect(() => {
    const wrap = tabsRef.current
    if (!wrap) return
    const measure = () => {
      const active = wrap.querySelector<HTMLElement>('.tab.active')
      if (!active) return
      const x = active.offsetLeft
      const w = active.offsetWidth
      setTabGlider((prev) => (prev && prev.x === x && prev.w === w ? prev : { x, w }))
    }
    measure()
    const ro = new ResizeObserver(measure)
    ro.observe(wrap)
    return () => ro.disconnect()
  }, [tab, tabSaves])

  useEffect(() => {
    onDirtyChange?.(anyTabDirty)
  }, [onDirtyChange, anyTabDirty])

  // 真实环境：不是读 instance.json 的缓存值，而是执行 venv 里的 python 再探测一次
  const envProbeBusy = useRef(false)
  const lastEnvProbe = useRef(0)
  const refreshEnv = useCallback(
    async (quiet: boolean) => {
      if (inst.kind === 'remote' || envProbeBusy.current) return
      envProbeBusy.current = true
      setEnvBusy(true)
      try {
        const live = await api.instanceEnvRefresh(inst.id)
        if (live.changed) await onReload()
        if (!quiet) {
          const bits = [
            `Python ${live.pythonVersionFull || live.pythonVersion || '未检测到'}`,
            `torch ${live.torchVersion || '未检测到'}`,
            live.cudaAvailable ? `CUDA 可用${live.cudaDevice ? `（${live.cudaDevice}）` : ''}` : 'CUDA 不可用',
          ]
          notify('ok', `已读取真实环境：${bits.join(' · ')}`)
        }
      } catch (e) {
        if (!quiet) notify('err', errText(e))
      } finally {
        envProbeBusy.current = false
        setEnvBusy(false)
      }
    },
    [inst.id, inst.kind, notify, onReload],
  )

  // 进概览 / 设置就校正一次（30 秒内不重复），避免界面一直显示同步前的旧引擎
  useEffect(() => {
    if (!active || (tab !== 'overview' && tab !== 'settings')) return
    if (inst.kind === 'remote' || !inst.comfy.version) return
    if (inst.state === 'starting' || inst.state === 'installing' || inst.state === 'updating') return
    const now = Date.now()
    if (now - lastEnvProbe.current < 30_000) return
    lastEnvProbe.current = now
    void refreshEnv(true)
  }, [active, tab, inst.id, inst.state, inst.kind, refreshEnv])

  useEffect(() => {
    setMountedTabs((prev) =>
      prev[tab] ? prev : { ...prev, [tab]: true },
    )
  }, [tab])

  const editableTab = tab === 'launch' || tab === 'storage' || tab === 'settings'
  const saveStateVisible = !!(
    editableTab &&
    tabSave &&
    (tabSave.busy || tabSave.dirty || tabSave.invalid)
  )
  const saveLabel = tabSave?.busy ? '保存中…' : '保存'
  const startLabel = busyRun
    ? '启动中…'
    : inst.state === 'installing' || inst.state === 'updating'
      ? '忙碌中'
      : '启动'
  const stopLabel = inst.state === 'starting' ? '取消启动' : '停止'

  // 显存/队列：ComfyUI 自己的接口，比状态更能说明“现在忙不忙”
  useEffect(() => {
    if (!active) return
    if (!running) {
      setHealth(null)
      return
    }
    let alive = true
    const pull = () =>
      api
        .instanceHealth()
        .then((m) => {
          if (alive) {
            setHealth(m[inst.id] ?? null)
            setReadAt(
              new Date().toLocaleTimeString('zh-CN', {
                hour: '2-digit',
                minute: '2-digit',
                second: '2-digit',
                hour12: false,
              }),
            )
          }
        })
        .catch(() => {})
    void pull()
    const timer = window.setInterval(pull, 3000)
    return () => {
      alive = false
      window.clearInterval(timer)
    }
  }, [active, running, inst.id])

  // 自动启动 / 切回详情时补一次「缺依赖」提醒（每次挂载只提醒一次）
  useEffect(() => {
    if (!active) return
    if (!running) {
      reminded.current = false
      return
    }
    if (reminded.current) return
    reminded.current = true
    void handleDepsOnStart(false)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, running, inst.id])

  /** 启动时按实例设置处理节点依赖：关闭 / 提醒（默认）/ 自动补 */
  const handleDepsOnStart = async (beforeStart: boolean) => {
    const mode = inst.nodeDepsAuto ?? 'remind'
    if (mode === 'off' || !installed) return
    const report = await api.nodesDepsStatus(inst.id).catch(() => null)
    const targets = startupDepsTargets(report)
    if (targets.length === 0) return

    if (mode === 'remind') {
      if (!beforeStart) notify('err', `${targets.length} 个节点缺依赖：在概览的「节点」卡片点「补依赖」可以补上`)
      return
    }

    // auto：先补依赖再启动（此时实例还没跑，不会被拒）
    if (!beforeStart) return
    setProgressOpen(true)
    setOpEvents([])
    setOpPhase(`补依赖 · ${targets.length} 个节点`)
    setOpPercent(0)
    setOpTab('detail')
    setProgress({ title: `启动前补依赖：${targets.length} 个节点`, logs: [], running: true })
    try {
      const rep = await api.nodesInstallDeps(
        inst.id,
        targets.map((n) => n.name),
        { runInstallPy: false },
        (ev: VersionEvent) => {
          setProgress((p) => (p ? { ...p, logs: [...p.logs, ev.message].slice(-200) } : p))
          setOpEvents((prev) =>
            [
              ...prev,
              {
                at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
                step: operationStageLabel(ev.stage),
                phase: ev.failed ? 'err' : ev.level === 'warn' ? 'warn' : ev.done ? 'done' : 'log',
                message: ev.message,
              },
            ].slice(-800),
          )
        },
      )
      const failed = rep.nodes.reduce((n, r) => n + r.failed.length, 0)
      setOpPhase(failed > 0 ? `依赖有 ${failed} 项失败` : '依赖补齐')
      setOpPercent(100)
      setProgress({
        title: failed > 0 ? `依赖有 ${failed} 项失败，继续启动` : '依赖补齐，启动中…',
        logs: [],
        running: false,
      })
      notify(failed > 0 ? 'err' : 'ok', failed > 0 ? '自动补依赖没全成功，先启动；细节在概览的「节点」卡片' : '依赖已补齐')
    } catch (e) {
      setOpPhase('补依赖失败')
      setProgress((p) =>
        p ? { ...p, title: '补依赖失败', running: false, failed: true } : p,
      )
      notify('err', `自动补依赖失败：${errText(e)}`)
    }
  }

  const start = async () => {
    setBusyRun(true)
    try {
      await handleDepsOnStart(true)
      // 重新启动 = 新的一轮日志：先清空旧画面，之后由进程事件直接追加
      clearTail()
      const out = await api.instanceStart(inst.id)
      notify('ok', `已启动，等待就绪…（${out.url}）`)
      await onReload()
      await handleDepsOnStart(false)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusyRun(false)
    }
  }

  const stop = async () => {
    setBusyRun(true)
    try {
      await api.instanceStop(inst.id)
      notify('ok', '已停止')
      await onReload()
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusyRun(false)
    }
  }

  const openBrowser = async () => {
    try {
      const url = await api.instanceOpenUrl(inst.id)
      notify('ok', `已在浏览器打开 ${url}`)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const openDir = async (dir: string) => {
    try {
      await api.openPath(dir)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const copyUrl = async () => {
    try {
      await navigator.clipboard.writeText(url)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1600)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const install = async () => {
    setInstalling(true)
    setCancelling(false)
    setProgressOpen(true)
    setOpEvents([])
    setOpPhase('准备中')
    setOpPercent(0)
    setOpTab('detail')
    setProgress({ title: '准备中…', logs: [], running: true })
    try {
      const report: InstallReport = await api.envInstall(inst.id, (ev: InstallEvent) => {
        setProgress((cur) => {
          const base = cur ?? { title: '', logs: [], running: true }
          const line = `${ev.phase === 'log' ? '   ' : '▸ '}${ev.message}`
          const logs = [...base.logs, line].slice(-300)
          const title =
            ev.phase === 'start' || ev.phase === 'done' || ev.phase === 'warn'
              ? `[${ev.stepIndex}/${ev.stepTotal}] ${STEP_LABEL[ev.step] ?? ev.step}`
              : base.title
          return { ...base, title, logs }
        })
        setOpEvents((prev) =>
          [
            ...prev,
            {
              at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
              step: STEP_LABEL[ev.step] ?? ev.step,
              phase: ev.phase,
              message: ev.message,
            },
          ].slice(-800),
        )
        if (ev.stepTotal > 0) {
          setOpPercent(Math.round((Math.min(ev.stepIndex, ev.stepTotal) / ev.stepTotal) * 100))
        }
        if (ev.phase === 'start' || ev.phase === 'done' || ev.phase === 'warn') {
          setOpPhase(STEP_LABEL[ev.step] ?? ev.step)
        }
      })
      setProgress((p) => ({ ...(p ?? { logs: [] }), running: false, title: '安装完成' }))
      setOpPhase('安装完成')
      setOpPercent(100)
      notify(
        'ok',
        `安装完成：Python ${report.pythonVersion} · torch ${report.torchVersion}${
          report.cudaAvailable ? ` · CUDA 可用（${report.cudaDevice ?? 'GPU'}）` : ' · 仅 CPU'
        }`,
      )
      await onReload()
    } catch (e) {
      const text = errText(e)
      const wasCancelled = text.includes('INSTALL_CANCELLED')
      // 错误也写进日志：失败卡片可以展开看原因，不用只靠会消失的 toast
      setOpPhase(wasCancelled ? '安装已取消' : '安装失败')
      setProgress((p) => ({
        ...(p ?? { logs: [] }),
        running: false,
        failed: !wasCancelled,
        cancelled: wasCancelled,
        title: wasCancelled ? '安装已取消' : '安装失败',
        logs: [...(p?.logs ?? []), text],
      }))
      notify(wasCancelled ? 'ok' : 'err', wasCancelled ? '已取消环境安装' : text)
      await onReload()
    } finally {
      setInstalling(false)
      setCancelling(false)
    }
  }

  const cancelInstall = async () => {
    setCancelling(true)
    try {
      await api.envCancelInstall(inst.id)
    } catch (e) {
      setCancelling(false)
      notify('err', errText(e))
    }
  }

  const engineLabel = useMemo(() => {
    const e = plan?.engines.find((x) => x.id === inst.engine)
    if (!e) return inst.engine
    // 实测 torch 与矩阵档不一致时，引擎名不再冒充矩阵里的 torch 版本；
    // 真实 torch 由后面的 torch 字段单独显示。
    const measured = inst.engineDetail?.torchVersion
    return measured && measured !== e.torchVersion ? e.label.replace(/ · torch [^·]+$/, '') : e.label
  }, [plan, inst.engine, inst.engineDetail?.torchVersion])

  const url = `http://${inst.server.listen === '0.0.0.0' ? '127.0.0.1' : inst.server.listen}:${inst.server.port}`
  const vramPct =
    health && health.vramTotalMib > 0 ? Math.min(100, (health.vramUsedMib / health.vramTotalMib) * 100) : 0
  const vramTone = vramPct >= 90 ? 'warn' : ''
  const ramPct =
    health && health.ramTotalMib > 0 ? Math.min(100, (health.ramUsedMib / health.ramTotalMib) * 100) : 0
  const gpuUtil = health?.gpuUtilization ?? null
  const logTask = useMemo(() => deriveLogTask(tail), [tail])
  // 日志还没吐出 tqdm、但队列里确实有任务时，先给一条不确定进度
  const fallbackWhat =
    logTask.stage === '加载模型'
      ? '正在加载模型'
      : logTask.stage === '解码'
        ? '正在解码'
        : logTask.stage === '编码'
          ? '正在编码'
          : logTask.stage === '准备'
            ? '正在准备'
            : '正在执行'
  const comfyProgress = running
    ? (logTask.task ??
      (health && health.queueRunning > 0 && !logTask.ended
        ? { what: fallbackWhat, value: 0, max: 0, speed: null }
        : null))
    : null
  // 进度条常驻时给节点列表留出被它压住的高度，最后一行不用和浮层抢位置
  const consoleBar =
    running && (!!comfyProgress || (health?.queueRunning ?? 0) > 0 || (health?.queuePending ?? 0) > 0)

  const overviewSplitter = (
    <div
      className="sync-splitter"
      role="separator"
      aria-label="调整节点与模型宽度"
      aria-orientation="vertical"
      aria-valuemin={Math.round(MIN_OVERVIEW_SPLIT * 100)}
      aria-valuemax={Math.round(MAX_OVERVIEW_SPLIT * 100)}
      aria-valuenow={Math.round(overviewSplit * 100)}
      title="拖动调整左右宽度，双击恢复"
      tabIndex={0}
      onPointerDown={(e) => {
        if (e.button !== 0) return
        e.preventDefault()
        beginOverviewSplitDrag(e.clientX)
      }}
      onDoubleClick={() => setOverviewSplitClamped(DEFAULT_OVERVIEW_SPLIT)}
      onKeyDown={(e) => {
        if (e.key === 'ArrowLeft') {
          e.preventDefault()
          setOverviewSplitClamped(overviewSplit - 0.02)
        } else if (e.key === 'ArrowRight') {
          e.preventDefault()
          setOverviewSplitClamped(overviewSplit + 0.02)
        } else if (e.key === 'Home') {
          e.preventDefault()
          setOverviewSplitClamped(DEFAULT_OVERVIEW_SPLIT)
        }
      }}
    >
      <span className="sync-splitter-grip" aria-hidden="true">
        <i />
        <i />
        <i />
      </span>
    </div>
  )

  return (
    <>
      <header className="topbar" data-tauri-drag-region="deep">
        <div className="topbar-left">
          <button
            className="icon-btn"
            onClick={onBack}
            title="返回实例列表"
            aria-label="返回实例列表"
          >
            <Icon name="back" size={16} />
          </button>
        </div>
        <div className="topbar-heading" title={inst.name}>
          {inst.name}
        </div>
        <div className="topbar-right">
          <TopbarAction
            icon="globe"
            label="同步"
            title="同步到局域网集群"
            onClick={onOpenCluster}
          />
          <TopbarAction
            icon="settings"
            label="设置"
            title="全局设置"
            onClick={onOpenSettings}
          />
          <ThemeToggle theme={theme} busy={themeBusy} onToggle={onToggleTheme} />
          <WindowControls />
        </div>
      </header>

      <div className="tabs-bar" data-dir={tabDir}>
        <div className="tabs" role="tablist" ref={tabsRef}>
          {tabGlider && (
            <span
              className="tab-glider"
              aria-hidden="true"
              style={{ width: tabGlider.w, transform: `translateX(${tabGlider.x}px)` }}
            />
          )}
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              className={`tab${tab === t.id ? ' active' : ''}`}
              onClick={() => setTab(t.id)}
              title={`${tabSaves[t.id]?.dirty ? '有未保存修改 · ' : ''}Ctrl+${t.key}`}
            >
              <Icon name={TAB_ICON[t.id]} size={14} />
              {t.label}
              {tabSaves[t.id]?.dirty && <span className="tab-dirty" aria-hidden />}
            </button>
          ))}
        </div>
      </div>

      <TabPane active={tab === 'overview'} mounted={mountedTabs.overview}>
        <div className={`content overview-fit${consoleBar ? ' has-console-bar' : ''}`}>
          <div className="overview">
            {!installed && !installing && !progress && (
              <div className="banner info">
                <span className="mark">
                  <Icon name="info" size={13} />
                </span>
                <div>
                  <strong>这个实例还没装环境。</strong> 点「安装环境」会建 venv、装 ComfyUI 本体和依赖，最后装 torch；只换 torch 会保留现有 venv。
                </div>
              </div>
            )}

            {/* 状态 + 环境：一张卡。环境那几项不重要，压在下面一条里 */}
            <div className="panel ov-status">
              <div className="panel-head">
                <h2>{running ? '运行指标' : '当前状态'}</h2>
                {running ? (
                  <span className="badge ghost">每 3 秒</span>
                ) : (
                  <span className={`badge ${inst.state === 'error' ? 'err' : 'ghost'}`}>
                    {inst.state === 'uninitialized' ? '等待安装环境' : stateText(inst.state)}
                  </span>
                )}
                <div className="spacer" />
                {running && (
                  <span className="meta" title="最近一次从 ComfyUI 读到显存/队列的时间">
                    读数 {readAt ?? '—'}
                  </span>
                )}
                {!running && (
                  <button
                    className={installed ? 'ghost sm' : 'primary sm'}
                    onClick={() => void install()}
                    disabled={installing || envBusy}
                    title={
                      envBusy
                        ? '正在读取环境信息，读完再安装，避免两个操作同时占住实例'
                        : installed
                          ? '复用现有 venv，按锁定版本重新安装依赖、torch 与 triton；不会删除 ComfyUI 目录'
                          : '创建 venv、安装 ComfyUI 本体和依赖，再装 torch 与匹配的 triton'
                    }
                  >
                    {installing ? '安装中…' : envBusy ? '读取中…' : installed ? '修复环境' : '安装环境'}
                  </button>
                )}
                <button className="ghost sm" onClick={() => void openDir(inst.rootDir)} title="在资源管理器里打开实例目录">
                  <Icon name="folder" size={13} />
                  打开目录
                </button>
                <button
                  className="ghost sm"
                  onClick={() => void openDir(comfyPath(inst, 'custom_nodes'))}
                  disabled={!installed}
                  title="打开 custom_nodes"
                >
                  <Icon name="nodes" size={13} />
                  节点目录
                </button>
                <button className="ghost sm" onClick={() => setTab('storage')} disabled={!installed}>
                  <Icon name="box" size={13} />
                  模型与存储
                </button>
                <button
                  className="ghost sm"
                  onClick={() => void openBrowser()}
                  disabled={!running}
                  title={running ? '在浏览器里打开 ComfyUI' : '启动之后才能打开'}
                >
                  <Icon name="globe" size={13} />
                  打开浏览器
                </button>
                <button className="ghost sm" onClick={() => void copyUrl()} title="复制 ComfyUI 地址">
                  <Icon name={copied ? 'check' : 'copy'} size={13} />
                  {copied ? '已复制' : '复制地址'}
                </button>
                <button className="ghost sm" onClick={() => setDiagOpen(true)}>
                  <Icon name="diagnostics" size={13} />
                  诊断
                </button>
              </div>
              {running ? (
                <div className="metrics">
                  <div className="metric">
                    <span className="metric-label">显存</span>
                    <span className={`metric-value${health ? '' : ' muted'}`}>
                      {health ? (
                        <>
                          {gb(health.vramUsedMib)}
                          <small>/ {gb(health.vramTotalMib)} GB</small>
                        </>
                      ) : (
                        '读取中'
                      )}
                    </span>
                    {health && (
                      <span className={`metric-track${vramTone ? ` ${vramTone}` : ''}`}>
                        <span style={{ width: `${vramPct}%` }} />
                      </span>
                    )}
                  </div>
                  <div className="metric">
                    <span className="metric-label">内存</span>
                    <span className={`metric-value${health && health.ramTotalMib > 0 ? '' : ' muted'}`}>
                      {!health ? (
                        '读取中'
                      ) : health.ramTotalMib > 0 ? (
                        <>
                          {gb(health.ramUsedMib)}
                          <small>/ {gb(health.ramTotalMib)} GB</small>
                        </>
                      ) : (
                        // 老版本 ComfyUI 的 /system_stats 没有 system.ram_*，这里不编数字
                        <span title="这个 ComfyUI 的 /system_stats 没给内存字段">—</span>
                      )}
                    </span>
                    {health && health.ramTotalMib > 0 && (
                      <span className={`metric-track${ramPct >= 90 ? ' warn' : ''}`}>
                        <span style={{ width: `${ramPct}%` }} />
                      </span>
                    )}
                  </div>
                  <div className="metric">
                    <span className="metric-label">GPU 利用率</span>
                    <span className={`metric-value${health && gpuUtil != null ? '' : ' muted'}`}>
                      {!health ? (
                        '读取中'
                      ) : gpuUtil != null ? (
                        <>
                          {gpuUtil}
                          <small>%</small>
                        </>
                      ) : (
                        <span title="nvidia-smi 没有返回 GPU 利用率">—</span>
                      )}
                    </span>
                    {gpuUtil != null && (
                      <span className={`metric-track${gpuUtil >= 90 ? ' warn' : ''}`}>
                        <span style={{ width: `${Math.min(100, gpuUtil)}%` }} />
                      </span>
                    )}
                  </div>
                  <div className="metric">
                    <span className="metric-label">队列</span>
                    <span className={`metric-value${health ? '' : ' muted'}`}>
                      {health ? (
                        <>
                          {health.queueRunning}
                          <small>运行 · {health.queuePending} 排队</small>
                        </>
                      ) : (
                        '读取中'
                      )}
                    </span>
                    {health && (
                      <span className="metric-track">
                        <span style={{ width: health.queueRunning > 0 ? '100%' : '0%' }} />
                      </span>
                    )}
                  </div>
                </div>
              ) : (
                <div className="metrics">
                  <div className="metric">
                    <span className="metric-label">上次启动</span>
                    <span className="metric-value muted">
                      {inst.lastStartedAt
                        ? new Date(inst.lastStartedAt).toLocaleString('zh-CN', {
                            month: '2-digit',
                            day: '2-digit',
                            hour: '2-digit',
                            minute: '2-digit',
                          })
                        : '从未启动'}
                    </span>
                  </div>
                  <div className="metric">
                    <span className="metric-label">ComfyUI</span>
                    <span className="metric-value" style={{ fontSize: 15 }}>
                      {inst.comfy.version ?? '未安装'}
                      <small>{channelText(inst.comfy.channel)}</small>
                    </span>
                  </div>
                  <div className="metric">
                    <span className="metric-label">端口</span>
                    <span className="metric-value mono" style={{ fontSize: 15 }}>
                      {inst.server.port}
                      <small>{inst.server.listen === '0.0.0.0' ? '所有网卡' : '本机'}</small>
                    </span>
                  </div>
                  <div className="metric">
                    <span className="metric-label">引擎</span>
                    <span className="metric-value" style={{ fontSize: 15 }}>
                      {engineLabel}
                    </span>
                  </div>
                </div>
              )}
            </div>

            <div
              className="overview-grid"
              ref={overviewGridRef}
              style={{ '--ov-split': `${overviewSplit * 100}%` } as CSSProperties}
            >
              <NodesView
                inst={inst}
                notify={notify}
                installIntent={depsIntent}
                onDiagnose={() => setDiagOpen(true)}
                onChanged={() => void onReload()}
                onOpenSnapshots={() => setTab('snapshots')}
              />
              {overviewSplitter}
              <ModelCard
                inst={inst}
                active={tab === 'overview'}
                notify={notify}
                onOpenStorage={() => setTab('storage')}
              />
            </div>
          </div>
        </div>

        {progressOpen && (installing || progress) && (
          <OperationDialog
            title={progress?.title || '安装环境'}
            target={inst.name}
            state={
              progress?.running
                ? 'running'
                : progress?.failed
                  ? 'failed'
                  : progress?.cancelled
                    ? 'cancelled'
                    : 'ok'
            }
            phase={opPhase}
            percent={opPercent}
            facts={[
              { label: '实例', value: inst.name },
              { label: '步骤', value: opPhase },
              { label: '进度', value: `${opPercent}%`, mono: true },
            ]}
            events={opEvents}
            tab={opTab}
            onTab={setOpTab}
            detailLabel="安装详情"
            allowHideOnRun={false}
            onClose={() => {
              setProgressOpen(false)
              setProgress(null)
            }}
            footer={
              installing ? (
                <button
                  className="ghost stop"
                  onClick={() => void cancelInstall()}
                  disabled={cancelling}
                  title="终止当前安装步骤并清理子进程"
                >
                  <Icon name="stop" size={13} />
                  {cancelling ? '正在取消…' : '取消安装'}
                </button>
              ) : null
            }
          >
            <div className="op-run-target">
              <div>
                <span>实例</span>
                <strong>{inst.name}</strong>
              </div>
              <div>
                <span>Python</span>
                <strong className="mono">{inst.pythonVersion || '—'}</strong>
              </div>
              <div>
                <span>引擎</span>
                <strong>{engineLabel}</strong>
              </div>
            </div>
            {progress?.failed && (
              <div className="op-run-error">
                {progress.logs[progress.logs.length - 1]?.trim() || '环境安装失败。'}
              </div>
            )}
            {progress?.cancelled && <div className="op-run-warn">已取消环境安装，当前环境保持原样。</div>}
          </OperationDialog>
        )}
      </TabPane>

      <TabPane active={tab === 'console'} mounted={mountedTabs.console}>
        <div className="content flush">
          <ConsoleView
            instanceId={inst.id}
            instanceName={inst.name}
            active={active && tab === 'console'}
            externalProgress={comfyProgress}
          />
        </div>
      </TabPane>
      <TabPane active={tab === 'versions'} mounted={mountedTabs.versions}>
        <div className="content">
          <VersionsView
              instanceId={inst.id}
              instanceName={inst.name}
              canSwitch={!running}
              notify={notify}
              onChanged={() => void onReload()}
            />
        </div>
      </TabPane>

      <TabPane active={tab === 'launch'} mounted={mountedTabs.launch}>
        <div className="content">
          <LaunchArgs
            key={inst.id}
            inst={inst}
            notify={notify}
            onSaveState={registerLaunchSave}
            onSaved={() => {
              void onReload()
            }}
          />
        </div>
      </TabPane>
      <TabPane active={tab === 'storage'} mounted={mountedTabs.storage}>
        <div className="content">
          <InstanceStorageView
            key={inst.id}
            inst={inst}
            notify={notify}
            onSaveState={registerStorageSave}
            onSaved={() => {
              void onReload()
            }}
          />
        </div>
      </TabPane>
      <TabPane active={tab === 'snapshots'} mounted={mountedTabs.snapshots}>
        <div className="content">
          <SnapshotsView
            key={inst.id}
            inst={inst}
            running={running}
            notify={notify}
            onChanged={() => {
              void onReload()
            }}
          />
        </div>
      </TabPane>
      <TabPane active={tab === 'activity'} mounted={mountedTabs.activity}>
        <div className="content">
          <Activity inst={inst} />
        </div>
      </TabPane>
      <TabPane active={tab === 'settings'} mounted={mountedTabs.settings}>
        <div className="content">
          <div className="stack">
            <InstanceSettings
              key={inst.id}
              inst={inst}
              plan={plan}
              onSaveState={registerSettingsSave}
              onSaved={() => {
                void onReload()
              }}
              notify={notify}
            />
            <WheelPanel inst={inst} running={running} notify={notify} />
          </div>
        </div>
      </TabPane>

      {diagOpen && (
        <DiagModal
          inst={inst}
          notify={notify}
          onClose={() => setDiagOpen(false)}
          onInstall={(names) => {
            setDiagOpen(false)
            setTab('overview')
            setDepsIntent({ names, key: Date.now() })
          }}
        />
      )}

      <ConsoleDock
        visible={active && tab !== 'console'}
        running={running}
        installed={installed}
        lines={tail}
        progress={comfyProgress}
        stage={running ? logTask.stage : null}
        queue={health ? { running: health.queueRunning, pending: health.queuePending } : null}
        onOpenTerminal={() => setTab('console')}
      />

      <footer className="bottombar">
        <div className="bottombar-inner">
          <div className="spacer" />
          {editableTab && tabSave && (
            <>
              <span
                className={`instance-save-state${saveStateVisible ? ' is-on' : ''}`}
                aria-hidden={!saveStateVisible}
              >
                <span
                  key={tabSave.busy ? 'busy' : tabSave.invalid ? 'invalid' : 'dirty'}
                  className={`badge ${tabSave.invalid ? 'err' : 'warn'}`}
                >
                  {tabSave.busy ? '保存中…' : tabSave.invalid ? '缺必填项' : '未保存'}
                </span>
              </span>
              <button className="ghost" onClick={tabSave.discard} disabled={!tabSave.dirty || tabSave.busy}>
                放弃
              </button>
              <button
                className="primary bottombar-save"
                onClick={() => void tabSave.save()}
                disabled={!tabSave.dirty || tabSave.busy || tabSave.invalid}
                title={tabSave.invalid ? '先填写必填项' : undefined}
              >
                <Icon name="check" size={14} />
                <span
                  key={`${saveLabel}-${tabSave.dirty ? 'dirty' : 'clean'}`}
                  className="bottombar-label"
                >
                  {saveLabel}
                </span>
              </button>
            </>
          )}
          {running ? (
            <>
              <button
                className={`ghost stop bottombar-stop${busyRun ? '' : ' is-armed'}`}
                onClick={() => void stop()}
                disabled={busyRun}
              >
                <Icon name="stop" size={14} />
                <span
                  key={`${stopLabel}-${busyRun ? 'busy' : 'armed'}`}
                  className="bottombar-label"
                >
                  {stopLabel}
                </span>
              </button>
              <button className="ghost" onClick={() => void onReload()}>
                <Icon name="refresh" size={14} />
                刷新
              </button>
              <button className="primary" onClick={() => void openBrowser()}>
                <Icon name="globe" size={14} />
                打开浏览器
              </button>
            </>
          ) : (
            <>
              <button
                className={`bottombar-start ${anyTabDirty ? 'ghost' : 'primary'}`}
                onClick={() => void start()}
                disabled={
                  busyRun ||
                  !installed ||
                  anyTabDirty ||
                  inst.state === 'installing' ||
                  inst.state === 'updating' ||
                  inst.state === 'stopping'
                }
                title={
                  anyTabDirty
                    ? '有未保存的修改，先保存或放弃再启动'
                    : inst.state === 'installing' || inst.state === 'updating'
                      ? '安装/更新尚未结束，不能启动'
                      : inst.state === 'stopping'
                        ? '正在停止，稍后再启动'
                        : undefined
                }
              >
                <Icon name="play" size={14} />
                <span
                  key={`${startLabel}-${anyTabDirty ? 'dirty' : 'clean'}`}
                  className="bottombar-label"
                >
                  {startLabel}
                </span>
              </button>
              <button className="ghost" onClick={() => void onReload()}>
                <Icon name="refresh" size={14} />
                刷新
              </button>
              <button className="ghost" onClick={() => void openBrowser()} disabled>
                <Icon name="globe" size={14} />
                打开浏览器
              </button>
            </>
          )}
        </div>
      </footer>
    </>
  )
}