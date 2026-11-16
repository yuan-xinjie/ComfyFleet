import {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import Icon, { type IconName } from './Icon'
import {
  IslandChevron,
  IslandShape,
  IslandTarget,
  ISLAND_BOTTOM_GAP_DEFAULT,
  ISLAND_BOTTOM_GAP_MAX,
  ISLAND_BOTTOM_GAP_MIN,
  ISLAND_SHAPE_ACTION_GAP_DEFAULT,
  ISLAND_SHAPE_ACTION_GAP_MAX,
  ISLAND_SHAPE_ACTION_GAP_MIN,
  islandGlowStyle,
  islandIdleStyle,
} from './IslandShape'
import type { IslandIdleShape } from '../types'
import {
  useTaskList,
  type BackgroundTask,
  type TaskKind,
  type TaskRecord,
} from './BackgroundTasks'
import type { OperationState } from './OperationDialog'

export type TaskBarIdleMode = 'auto' | 'always'
export type TaskBarIdleContent = 'animation' | 'time' | 'custom'

export interface TaskBarNotice {
  kind: 'ok' | 'err'
  msg: string
}

export type IslandActionVariant = 'primary' | 'danger' | 'ghost'

export interface IslandAction {
  id: string
  label: string
  variant?: IslandActionVariant
  /** 只显示状态，不可点击。 */
  status?: boolean
  onClick?: () => void
}

function clockText(): string {
  const now = new Date()
  return `${String(now.getHours()).padStart(2, '0')}:${String(now.getMinutes()).padStart(2, '0')}`
}

const STATE_LABEL: Record<OperationState, string> = {
  running: '进行中',
  ok: '已完成',
  failed: '失败',
  cancelled: '已取消',
}

const STATE_TONE: Record<OperationState, string> = {
  running: 'run',
  ok: 'ok',
  failed: 'fail',
  cancelled: 'cancel',
}

const KIND_ICON: Record<TaskKind, IconName> = {
  generate: 'diagnostics',
  start: 'power',
  install: 'download',
  wheel: 'box',
  version: 'versions',
  node: 'nodes',
  snapshot: 'clock',
  sync: 'globe',
  other: 'console',
}

const MAX_POPOVER_TASKS = 5
const HOVER_CLOSE_MS = 180
const MIN_ISLAND_WIDTH = 300
const MAX_ISLAND_WIDTH = 680
const EXIT_MS = 220
const RESIDENT_EXIT_MS = 260
/** 悬浮展开时按钮左右留白，以及相对空闲宽度多展开的宽度。 */
const IDLE_ACTION_PAD = 14
const IDLE_HOVER_GROW = 10
/** 展开高度比收起多 8px，底边同步下移一半才能保持中心不动。 */
const IDLE_EXPAND_CENTER_DROP = 4

function clampPercent(value: number): number {
  return Math.max(0, Math.min(100, Math.round(value)))
}

function choosePrimary(tasks: TaskRecord[]): TaskRecord | null {
  const running = tasks.filter((task) => task.state === 'running')
  if (running.length > 0) {
    const generate = running.find((task) => task.kind === 'generate')
    if (generate) return generate
    return [...running].sort((a, b) => a.enteredAt - b.enteredAt)[0] ?? null
  }
  const terminal = tasks.filter((task) => task.state === 'failed' || task.state === 'cancelled')
  if (terminal.length > 0) {
    return [...terminal].sort((a, b) => b.enteredAt - a.enteredAt)[0] ?? null
  }
  return tasks[0] ?? null
}

function secondaryText(task: BackgroundTask): string {
  const parts: string[] = []
  if (task.phase && !task.title.includes(task.phase)) parts.push(task.phase)
  if (task.detail) parts.push(task.detail)
  if (task.speed) parts.push(task.speed)
  return parts.join(' · ')
}

function taskPercent(task: BackgroundTask): number | null {
  return typeof task.percent === 'number' && Number.isFinite(task.percent)
    ? clampPercent(task.percent)
    : null
}

/** 生成任务运行中优先显示采样速度；其他任务仍显示进度百分比。 */
function taskValue(task: BackgroundTask): string {
  if (task.state !== 'running') return STATE_LABEL[task.state]
  if (task.kind === 'generate' && task.speed) return task.speed
  const percent = taskPercent(task)
  return percent !== null ? `${percent}%` : STATE_LABEL.running
}

function taskAriaLabel(task: BackgroundTask): string {
  return [task.title, task.phase, taskValue(task)].filter(Boolean).join('，')
}

interface IslandView {
  key: string
  title: string
  value: string
  tone: string
  icon: IconName
  percent: number | null
  indeterminate: boolean
  task: BackgroundTask | null
}

interface IslandProps {
  /** 实例页没有任务时，留一个可打开控制台的入口。 */
  idle?: { onOpen: () => void } | null
  /** 空闲时自动隐藏还是常驻。 */
  idleMode?: TaskBarIdleMode
  /** 无任务时显示动画、时间或自定义内容。 */
  idleContent?: TaskBarIdleContent
  /** 无任务动画的默认形态。 */
  idleShape?: IslandIdleShape
  /** 空闲胶囊最小宽度。 */
  idleMinWidth?: number
  /** 空闲动画速度（50–200%）。 */
  animationSpeed?: number
  /** 默认形态与胶囊两侧的距离。 */
  shapeInset?: number
  /** 两侧形态与操作按钮的间距。 */
  shapeActionGap?: number
  /** 岛底边距离窗口底部的距离。 */
  bottomGap?: number
  /** 外发光。 */
  glow?: boolean
  /** 默认发光颜色，状态色会覆盖它。 */
  glowColor?: string
  /** 发光范围。 */
  glowRange?: number
  /** 自定义空闲内容。 */
  idleText?: string
  /** 操作反馈也走任务岛，不再单独弹 toast。 */
  notice?: TaskBarNotice | null
  onNoticeDismiss?: () => void
  /** 控制台打开时保留岛的布局，只把它藏起来，供展开动画取形状。 */
  concealed?: boolean
  /** 实例页底栏操作，悬浮岛时展开。 */
  actions?: IslandAction[]
}

/**
 * 灵动岛式全局任务条。
 *
 * 固定两行：标题 + 副标题（阶段/信息/速度，悬浮时换成最近一行控制台输出）。
 * 左侧状态图标外圈就是进度环；宽度按内容自适应，最长 680px。
 * 没有任务时，操作反馈（成功/失败）也在这一个岛里显示。
 */
export default function TaskBar({
  idle,
  idleMode = 'auto',
  idleContent = 'animation',
  idleShape = 'chevrons',
  idleMinWidth = 84,
  animationSpeed = 100,
  shapeInset = 11,
  shapeActionGap = ISLAND_SHAPE_ACTION_GAP_DEFAULT,
  bottomGap = ISLAND_BOTTOM_GAP_DEFAULT,
  glow = true,
  glowColor = '#5b8dff',
  glowRange = 24,
  idleText = '控制台',
  notice = null,
  onNoticeDismiss,
  concealed = false,
  actions = [],
}: IslandProps) {
  const tasks = useTaskList()
  const [clock, setClock] = useState(clockText)
  const [primaryId, setPrimaryId] = useState<string | null>(null)
  const [hovered, setHovered] = useState(false)
  const [focused, setFocused] = useState(false)
  const [listOpen, setListOpen] = useState(false)
  const [contentWidth, setContentWidth] = useState(MIN_ISLAND_WIDTH)
  const [actionsWidth, setActionsWidth] = useState(0)
  const [idleWidth, setIdleWidth] = useState(0)
  const [painted, setPainted] = useState<IslandView | null>(null)
  const [leaving, setLeaving] = useState(false)
  const [leavingToIdle, setLeavingToIdle] = useState(false)
  const [noticeIn, setNoticeIn] = useState(false)
  const [noticeFromIdle, setNoticeFromIdle] = useState(false)
  const [overlayOpen, setOverlayOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)
  const mainRef = useRef<HTMLButtonElement | null>(null)
  const measureRef = useRef<HTMLDivElement | null>(null)
  const idleMeasureRef = useRef<HTMLButtonElement | null>(null)
  const actionsRef = useRef<HTMLDivElement | null>(null)
  const idleSnapshot = useRef({ width: MIN_ISLAND_WIDTH, height: 30 })
  const lastShownKey = useRef<string | null>(null)
  const closeTimer = useRef<number | null>(null)
  const leaveTimer = useRef<number | null>(null)
  const suppressFocus = useRef(false)
  const listId = useId()
  const hidden = concealed || overlayOpen

  // 控制台和操作弹窗都会盖住岛；两者都出现时岛必须整体失效，而不只是透明。
  useEffect(() => {
    const update = () =>
      setOverlayOpen(Boolean(document.querySelector('.modal-backdrop, .op-dock-layer')))
    update()
    const observer = new MutationObserver(update)
    observer.observe(document.body, { childList: true, subtree: true })
    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    if (!hidden) return
    setHovered(false)
    setFocused(false)
    setListOpen(false)
    const active = document.activeElement
    if (active instanceof HTMLElement && active.closest('.island-root')) active.blur()
  }, [hidden])

  const current = tasks.find((task) => task.id === primaryId) ?? null
  const running = tasks.filter((task) => task.state === 'running')
  const keepCurrent = current !== null && (current.state === 'running' || running.length === 0)
  const primary = keepCurrent ? current : choosePrimary(tasks)
  const others = primary ? tasks.filter((task) => task.id !== primary.id) : tasks
  const isListOpen = listOpen && others.length > 0

  const expanded = hovered || focused || isListOpen
  const persistentActions = actions.some(
    (action) =>
      action.id === 'save' ||
      action.id === 'discard' ||
      action.id === 'save-invalid' ||
      action.id === 'save-busy',
  )
  const showActions = actions.length > 0 && (expanded || persistentActions)
  const shapeActionGapSafe = Number.isFinite(shapeActionGap)
    ? Math.max(
        ISLAND_SHAPE_ACTION_GAP_MIN,
        Math.min(ISLAND_SHAPE_ACTION_GAP_MAX, shapeActionGap),
      )
    : ISLAND_SHAPE_ACTION_GAP_DEFAULT
  const bottomGapSafe = Number.isFinite(bottomGap)
    ? Math.max(ISLAND_BOTTOM_GAP_MIN, Math.min(ISLAND_BOTTOM_GAP_MAX, bottomGap))
    : ISLAND_BOTTOM_GAP_DEFAULT
  const idleBottom = showActions
    ? Math.max(0, bottomGapSafe - IDLE_EXPAND_CENTER_DROP)
    : bottomGapSafe
  const actionsSignature = actions
    .map((action) => `${action.id}\u0000${action.label}\u0000${action.variant ?? ''}\u0000${action.status ? 1 : 0}`)
    .join('\u0001')
  const width = Math.min(
    MAX_ISLAND_WIDTH,
    Math.max(MIN_ISLAND_WIDTH, contentWidth + (showActions ? actionsWidth + 8 : 0)),
  )
  const percent = primary ? taskPercent(primary) : null
  const primaryIcon: IconName = primary
    ? primary.state === 'failed'
      ? 'alert'
      : primary.state === 'cancelled'
        ? 'close'
        : primary.state === 'ok'
          ? 'check'
          : KIND_ICON[primary.kind ?? 'other']
    : 'check'
  const primaryValue = primary ? taskValue(primary) : ''

  const view: IslandView | null = primary
    ? {
        key: primary.id,
        title: primary.title,
        value: primaryValue,
        tone: STATE_TONE[primary.state],
        icon: primaryIcon,
        percent:
          primary.state === 'running' ? percent : 100,
        indeterminate: primary.state === 'running' && percent === null,
        task: primary,
      }
    : notice
      ? {
          key: 'notice',
          title: notice.msg,
          value: notice.kind === 'err' ? STATE_LABEL.failed : STATE_LABEL.ok,
          tone: notice.kind === 'err' ? 'fail' : 'ok',
          icon: notice.kind === 'err' ? 'alert' : 'check',
          percent: 100,
          indeterminate: false,
          task: null,
        }
      : null

  const viewSignature = view
    ? [
        view.key,
        view.title,
        view.value,
        view.tone,
        view.icon,
        view.percent ?? '',
        view.indeterminate ? 1 : 0,
      ].join('\u0000')
    : ''
  const shownView = view ?? painted
  const resident = Boolean(idle) && idleMode === 'always'
  const displayWidth = leavingToIdle ? idleWidth : width
  const noticeEnter =
    noticeIn && !leaving ? (noticeFromIdle ? ' is-from-idle' : ' is-notice-in') : ''

  // 任务/通知消失后先留一小段退场动画，再把岛收掉或让位给空闲入口。
  // 常驻时直接缩成空闲胶囊的宽度和高度，避免淡出后另起一个形状。
  useEffect(() => {
    if (leaveTimer.current !== null) {
      window.clearTimeout(leaveTimer.current)
      leaveTimer.current = null
    }
    if (view) {
      setPainted(view)
      setLeaving(false)
      setLeavingToIdle(false)
      return
    }
    if (!painted) return
    const toIdle = resident && idleWidth > 0
    setLeaving(true)
    setLeavingToIdle(toIdle)
    leaveTimer.current = window.setTimeout(
      () => {
        setPainted(null)
        setLeaving(false)
        setLeavingToIdle(false)
        leaveTimer.current = null
      },
      toIdle ? RESIDENT_EXIT_MS : EXIT_MS,
    )
    return () => {
      if (leaveTimer.current !== null) {
        window.clearTimeout(leaveTimer.current)
        leaveTimer.current = null
      }
    }
  }, [viewSignature, resident, idleWidth])

  // 通知首次出现时放大；常驻状态下从空闲胶囊的尺寸展开。
  useLayoutEffect(() => {
    if (!view) {
      if (lastShownKey.current !== null) {
        lastShownKey.current = null
        setNoticeIn(false)
        setNoticeFromIdle(false)
        setFocused(false)
      }
      return
    }
    if (lastShownKey.current === view.key) return
    const previous = lastShownKey.current
    lastShownKey.current = view.key
    setFocused(false)
    if (previous === null && idle) {
      // 从空闲胶囊直接长成任务，避免先缩放淡入再跳成另一个尺寸。
      setNoticeIn(true)
      setNoticeFromIdle(true)
    } else if (view.key === 'notice' && previous === null) {
      setNoticeIn(true)
      setNoticeFromIdle(false)
    } else {
      setNoticeIn(false)
      setNoticeFromIdle(false)
    }
  }, [viewSignature])

  const visible = shownView !== null || Boolean(idle)
  const idleAuto = shownView === null && idleMode === 'auto' && !focused
  const heading = `另有 ${others.length} 个任务`
  const idleLabel =
    idleContent === 'time'
      ? clock
      : idleContent === 'custom'
        ? idleText.trim() || '控制台'
        : '控制台'
  const idleIcon: IconName = idleContent === 'time' ? 'clock' : 'console'
  const idleBaseWidth = idleWidth || idleMinWidth
  const shapeWidth = idleShape === 'targets' ? 11 : 8
  const expandedMinWidth =
    idleContent === 'animation'
      ? actionsWidth + (shapeInset + shapeWidth + shapeActionGapSafe) * 2
      : actionsWidth + IDLE_ACTION_PAD * 2
  const idleActionWidth = Math.min(
    MAX_ISLAND_WIDTH,
    Math.max(idleBaseWidth + IDLE_HOVER_GROW, expandedMinWidth),
  )
  const idleDisplayWidth =
    actions.length > 0 && showActions ? idleActionWidth : idleBaseWidth
  if (shownView === null && idle) {
    idleSnapshot.current = {
      width: idleDisplayWidth,
      height: showActions ? 38 : 30,
    }
  }

  const actionStrip =
    actions.length > 0 ? (
      <div
        ref={actionsRef}
        className={`island-actions${showActions ? ' is-on' : ''}`}
        style={{ width: showActions ? actionsWidth : 0 }}
      >
        <span className="island-actions-lead" aria-hidden="true" />
        {actions.map((action) =>
          action.status ? (
            <span
              key={action.id}
              className={`island-action is-status${action.variant ? ` is-${action.variant}` : ''}`}
            >
              <span className="island-action-label">{action.label}</span>
            </span>
          ) : (
            <button
              key={action.id}
              type="button"
              className={`island-action${action.variant ? ` is-${action.variant}` : ''}`}
              onClick={() => action.onClick?.()}
            >
              <span className="island-action-label">{action.label}</span>
            </button>
          ),
        )}
      </div>
    ) : null
  const glowStyle = islandGlowStyle(glowColor, glowRange)
  const idleSurfaceStyle = {
    ...islandIdleStyle(
      idleMinWidth,
      animationSpeed,
      shapeInset,
      idleContent === 'animation',
    ),
    ...glowStyle,
  } as CSSProperties
  const SideShape = idleShape === 'targets' ? IslandTarget : IslandChevron
  const idleVisual =
    idleContent === 'animation' ? (
      <IslandShape shape={idleShape} />
    ) : (
      <>
        <Icon name={idleIcon} size={13} />
        <span
          className={
            idleContent === 'time'
              ? 'mono'
              : idleContent === 'custom'
                ? 'island-idle-custom'
                : undefined
          }
        >
          {idleLabel}
        </span>
      </>
    )

  useEffect(() => {
    if (idleContent !== 'time') return
    const tick = () => setClock(clockText())
    tick()
    const timer = window.setInterval(tick, 1000)
    return () => window.clearInterval(timer)
  }, [idleContent])

  useEffect(() => {
    if (!primary) {
      if (primaryId !== null) setPrimaryId(null)
      return
    }
    if (primary.id !== primaryId) setPrimaryId(primary.id)
  }, [primary, primaryId])

  // 宽度按隐藏的量具取自然宽度；量具复用了真实行的内边距和字号。
  useLayoutEffect(() => {
    if (!shownView) return
    const el = measureRef.current
    if (!el) return
    const next = Math.min(
      MAX_ISLAND_WIDTH,
      Math.max(MIN_ISLAND_WIDTH, Math.ceil(el.getBoundingClientRect().width + 2)),
    )
    setContentWidth((prev) => (Math.abs(prev - next) > 0.5 ? next : prev))
  }, [shownView, others.length])

  // 收起时容器宽度是 0，不能拿 scrollWidth（会被当前展开宽度撑大）；按子项真实宽度算。
  useLayoutEffect(() => {
    const el = actionsRef.current
    if (!el || actions.length === 0) {
      setActionsWidth(0)
      return
    }
    const measure = () => {
      const kids = Array.from(el.children) as HTMLElement[]
      const gap = Number.parseFloat(getComputedStyle(el).columnGap) || 0
      const content = kids.reduce((sum, kid) => sum + kid.getBoundingClientRect().width, 0)
      const next = Math.ceil(content + gap * Math.max(0, kids.length - 1))
      setActionsWidth((prev) => (Math.abs(prev - next) > 0.5 ? next : prev))
    }
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(el)
    for (const kid of Array.from(el.children)) observer.observe(kid)
    return () => observer.disconnect()
  }, [actionsSignature, actions.length, Boolean(shownView)])

  // 常驻入口的真实宽度；退场时缩到同宽，随后切换到同一个胶囊形状。
  useLayoutEffect(() => {
    const el = idleMeasureRef.current
    if (!el) return
    const next = Math.ceil(el.getBoundingClientRect().width)
    setIdleWidth((prev) => (Math.abs(prev - next) > 0.5 ? next : prev))
  }, [shownView, idle, clock, idleContent, idleShape, idleMinWidth, shapeInset, idleText])

  useEffect(() => {
    if (!visible) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      setHovered(false)
      setFocused(false)
      setListOpen(false)
      if (document.activeElement !== mainRef.current) {
        suppressFocus.current = true
        mainRef.current?.focus({ preventScroll: true })
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [visible])

  useEffect(() => {
    if (others.length === 0 && listOpen) setListOpen(false)
  }, [others.length, listOpen])

  useEffect(() => {
    if (!isListOpen) return
    const onPointerDown = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) {
        setHovered(false)
        setListOpen(false)
      }
    }
    document.addEventListener('pointerdown', onPointerDown)
    return () => document.removeEventListener('pointerdown', onPointerDown)
  }, [isListOpen])

  useEffect(() => {
    return () => {
      if (closeTimer.current !== null) window.clearTimeout(closeTimer.current)
    }
  }, [])

  const openIsland = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current)
      closeTimer.current = null
    }
    setHovered(true)
  }

  const scheduleClose = () => {
    if (closeTimer.current !== null) window.clearTimeout(closeTimer.current)
    closeTimer.current = window.setTimeout(() => {
      setHovered(false)
      setListOpen(false)
      closeTimer.current = null
    }, HOVER_CLOSE_MS)
  }

  const onPointerEnter = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === 'touch') return
    openIsland()
  }

  const onPointerLeave = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.pointerType === 'touch') return
    scheduleClose()
  }

  const openTask = (task: BackgroundTask) => {
    setHovered(false)
    setFocused(false)
    setListOpen(false)
    task.onOpen()
  }

  const activateView = () => {
    if (leaving || !view) return
    if (view.task) openTask(view.task)
    else onNoticeDismiss?.()
  }

  const toggleList = () => {
    setHovered(true)
    setListOpen((value) => !value)
  }

  if (!visible) return null

  if (!shownView) {
    if (!idle) return null
    return (
      <div
        className={`island-root${idleAuto ? ' is-idle-auto' : ''}${showActions ? ' is-idle-expanded' : ''}${hidden ? ' is-concealed' : ''}`}
        style={{ bottom: idleBottom }}
        aria-hidden={hidden || undefined}
        inert={hidden || undefined}
        onPointerEnter={onPointerEnter}
        onPointerLeave={onPointerLeave}
        onFocusCapture={(event) => {
          if ((event.target as HTMLElement).matches(':focus-visible')) setFocused(true)
        }}
        onBlurCapture={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
            setFocused(false)
          }
        }}
      >
        <div
          className={`island-idle has-actions${glow ? ' is-glowing' : ''}${showActions ? ' is-expanded' : ''}`}
          style={{ ...idleSurfaceStyle, width: idleDisplayWidth }}
        >
          <button
            type="button"
            className="island-idle-hit"
            onClick={idle.onOpen}
            title="打开控制台"
            aria-label="打开控制台"
          />
          {idleContent === 'animation' ? (
            <span className="island-idle-sides">
              <SideShape side="left" />
              <SideShape side="right" />
            </span>
          ) : (
            <span className={`island-idle-content${showActions ? ' is-hidden' : ''}`}>
              {idleVisual}
            </span>
          )}
          {actionStrip}
          <button
            ref={idleMeasureRef}
            type="button"
            className={`island-idle island-idle-measure${idleContent === 'animation' ? ' island-idle-anim' : ''}`}
            style={idleSurfaceStyle}
            tabIndex={-1}
            aria-hidden="true"
          >
            {idleVisual}
          </button>
        </div>
      </div>
    )
  }

  const ringStyle = { '--p': shownView.percent ?? 0 } as CSSProperties
  const ariaLabel = shownView.task
    ? taskAriaLabel(shownView.task)
    : `${shownView.title}，${shownView.value}`

  return (
    <div
      ref={rootRef}
      className={`island-root${hidden ? ' is-concealed' : ''}`}
      style={{ bottom: bottomGapSafe }}
      aria-hidden={hidden || undefined}
      inert={hidden || undefined}
      onPointerEnter={onPointerEnter}
      onPointerLeave={onPointerLeave}
      onFocusCapture={(event) => {
        if (suppressFocus.current) {
          suppressFocus.current = false
          return
        }
        if ((event.target as HTMLElement).matches(':focus-visible')) setFocused(true)
      }}
      onBlurCapture={(event) => {
        if (!rootRef.current?.contains(event.relatedTarget as Node | null)) {
          setFocused(false)
          setListOpen(false)
        }
      }}
    >
      <div
        className={`island is-${shownView.tone} kind-${shownView.task?.kind ?? 'other'}${glow ? ' is-glowing' : ''}${expanded ? ' is-expanded' : ''}${leaving ? ' is-leaving' : ''}${leavingToIdle ? ' is-to-idle' : ''}${noticeEnter}`}
        style={
          {
            width: displayWidth,
            '--idle-w': idleSnapshot.current.width,
            '--idle-h': `${idleSnapshot.current.height}px`,
            ...glowStyle,
          } as CSSProperties
        }
      >
        <div className="island-row">
          <button
            ref={mainRef}
            type="button"
            className="island-main"
            onClick={activateView}
            aria-label={ariaLabel}
            aria-disabled={leaving || undefined}
          >
            <span
              className={`island-mark${shownView.indeterminate ? ' is-indeterminate' : ''}`}
              style={ringStyle}
              role="progressbar"
              aria-label={`${shownView.title}进度`}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={shownView.indeterminate ? undefined : (shownView.percent ?? 0)}
            >
              <span className="island-ring" aria-hidden="true" />
              <span className="island-mark-disc" aria-hidden="true">
                <Icon name={shownView.icon} size={13} />
              </span>
            </span>

            <span className="island-copy">
              <span className="island-title">{shownView.title}</span>
            </span>
          </button>

          {actionStrip}

          <span className="island-value mono">
            <span className="island-value-label">{shownView.value}</span>
          </span>

          {others.length > 0 && (
            <button
              type="button"
              className="island-count mono"
              onClick={toggleList}
              aria-label={`${isListOpen ? '收起' : '展开'} ${others.length} 个其他任务`}
              aria-expanded={isListOpen}
              aria-controls={listId}
            >
              +{others.length}
            </button>
          )}
        </div>
      </div>

      <button
        ref={idleMeasureRef}
        type="button"
        className={`island-idle island-idle-measure${idleContent === 'animation' ? ' island-idle-anim' : ''}`}
        style={idleSurfaceStyle}
        tabIndex={-1}
        aria-hidden="true"
      >
        {idleVisual}
      </button>

      <div ref={measureRef} className="island-measure" aria-hidden="true">
        <div className="island-row">
          <span className="island-main">
            <span className="island-mark" />
            <span className="island-copy">
              <span className="island-title">{shownView.title}</span>
            </span>
          </span>
          <span className="island-value mono">
            <span className="island-value-label">{shownView.value}</span>
          </span>
          {others.length > 0 && <span className="island-count mono">+{others.length}</span>}
        </div>
      </div>

      {isListOpen && (
        <div id={listId} className="island-popover" role="group" aria-label="其他任务">
          <div className="island-popover-head mono">{heading}</div>
          {others.slice(0, MAX_POPOVER_TASKS).map((task) => {
            const rowIcon: IconName =
              task.state === 'failed'
                ? 'alert'
                : task.state === 'cancelled'
                  ? 'close'
                  : task.state === 'ok'
                    ? 'check'
                    : KIND_ICON[task.kind ?? 'other']
            const rowValue = taskValue(task)
            return (
              <button
                key={task.id}
                type="button"
                className={`island-popover-row is-${STATE_TONE[task.state]}`}
                onClick={() => openTask(task)}
                aria-label={taskAriaLabel(task)}
              >
                <span className="island-popover-mark" aria-hidden="true">
                  <Icon name={rowIcon} size={13} />
                </span>
                <span className="island-popover-copy">
                  <span className="island-popover-title">{task.title}</span>
                  <span className="island-popover-meta">
                    {secondaryText(task) || STATE_LABEL[task.state]}
                  </span>
                </span>
                <span className="island-popover-value mono">{rowValue}</span>
              </button>
            )
          })}
          {others.length > MAX_POPOVER_TASKS && (
            <div className="island-popover-more mono">
              另有 {others.length - MAX_POPOVER_TASKS} 个
            </div>
          )}
        </div>
      )}
    </div>
  )
}
