import { useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent } from 'react'
import { createPortal } from 'react-dom'
import Icon from './Icon'
import { TQDM_RE, type RunProgress } from '../lib/runProgress'

type LogTone = '' | 'err' | 'warn' | 'ok' | 'dim' | 'progress'

const LOG_FONT_KEY = 'comfyfleet.console.fontSize'
const LOG_FONT_DEFAULT = 13.5
const LOG_FONT_MIN = 11
const LOG_FONT_MAX = 20
const AUTO_KEY = 'comfyfleet.console.autoReveal'
const RAIL_KEY = 'comfyfleet.console.rail'
const RAIL_INSET = 12
const RAIL_SNAP = 56
const LONG_PRESS_MS = 300

type RailSide = 'left' | 'right' | 'free'
type RailAnchor = { side: RailSide; fraction: number }
const RAIL_DEFAULT: RailAnchor = { side: 'free', fraction: 0.5 }

/** 开头的 [ ... INFO ... ]，只给方括号本身单独上色 */
const INFO_TAG_RE = /^(\s*)(\[[^\]]*INFO[^\]]*\])([\s\S]*)$/i

function loadLogFontSize(): number {
  try {
    const raw = localStorage.getItem(LOG_FONT_KEY)
    const n = raw === null ? NaN : Number(raw)
    if (Number.isFinite(n)) return Math.min(LOG_FONT_MAX, Math.max(LOG_FONT_MIN, n))
  } catch {
    // 隐私模式 / 存储被禁用时用默认值，不影响日志显示
  }
  return LOG_FONT_DEFAULT
}

function loadAutoReveal(): boolean {
  try {
    return localStorage.getItem(AUTO_KEY) !== '0'
  } catch {
    return true
  }
}

function loadRailAnchor(): RailAnchor {
  try {
    const raw = localStorage.getItem(RAIL_KEY)
    if (!raw) return RAIL_DEFAULT
    const parsed = JSON.parse(raw) as Partial<RailAnchor> | null
    const side = parsed?.side
    if (side !== 'left' && side !== 'right' && side !== 'free') return RAIL_DEFAULT
    const fraction =
      typeof parsed?.fraction === 'number' && Number.isFinite(parsed.fraction)
        ? Math.min(1, Math.max(0, parsed.fraction))
        : 0.5
    return { side, fraction }
  } catch {
    return RAIL_DEFAULT
  }
}

/**
 * 给日志行判个等级：错误红、警告黄、成功绿、堆栈续行压暗，其余保持原色。
 * ComfyUI 的日志没有统一前缀，只能按关键词 + 缩进猜。
 */
function logTone(line: string): LogTone {
  if (!line.trim()) return 'dim'
  if (TQDM_RE.test(line)) return 'progress'
  if (/^\s{2,}\S/.test(line)) return 'dim'
  if (
    /\b(error|exception|traceback|critical|fatal|failed|failure|refused|aborted|out of memory|oom|killed)\b/i.test(
      line,
    )
  ) {
    return 'err'
  }
  if (
    /\b(warn|warning|warnings|deprecated|missing|not found|could not|cannot|unable|unsupported|fallback|ignored)\b/i.test(
      line,
    )
  ) {
    return 'warn'
  }
  if (/prompt executed|loaded completely|to see the gui|server started|starting server|model loaded/i.test(line)) {
    return 'ok'
  }
  return ''
}

export type DockQueue = { running: number; pending: number }

interface Props {
  /** 当前实例页是否在前台；切到终端页签或离开实例页时整个悬浮窗收起 */
  visible: boolean
  running: boolean
  installed: boolean
  /** 原始进程输出，含 tqdm 行；组件自己过滤进度行 */
  lines: string[]
  progress: RunProgress | null
  /** 当前阶段（加载模型 / 采样 / 解码……），只在控制台底部展示 */
  stage?: string | null
  queue: DockQueue | null
  onOpenTerminal: () => void
}

/**
 * 实例页底部的悬浮控制台。
 *
 * 收起时是一条贴着底栏的进度条；没有任务时缩成一枚小把手。
 * 鼠标进入把手或进度条本身，半屏浮起实时日志；点击则整块展开。
 * 日志与终端页签共用同一份进程输出，但这里保留「按等级上色」的可读版本。
 */
export default function ConsoleDock({
  visible,
  running,
  installed,
  lines,
  progress,
  stage,
  queue,
  onOpenTerminal,
}: Props) {
  const [full, setFull] = useState(false)
  const [near, setNear] = useState(false)
  const [stick, setStick] = useState(true)
  const [font, setFont] = useState(loadLogFontSize)
  const [autoReveal, setAutoReveal] = useState(loadAutoReveal)
  const [railAnchor, setRailAnchor] = useState<RailAnchor>(loadRailAnchor)
  const [vw, setVw] = useState(() => (typeof window === 'undefined' ? 1280 : window.innerWidth))
  const [dragging, setDragging] = useState(false)
  const [dragCenter, setDragCenter] = useState<number | null>(null)
  const panelRef = useRef<HTMLElement | null>(null)
  const logRef = useRef<HTMLPreElement | null>(null)
  const railRef = useRef<HTMLButtonElement | null>(null)
  const stickRef = useRef(true)
  const fullRef = useRef(false)
  const autoRef = useRef(autoReveal)
  const dragCenterRef = useRef<number | null>(null)
  const suppressClickRef = useRef(false)
  const pressStartRef = useRef<{ x: number; y: number } | null>(null)
  const dragRef = useRef<{
    pointerId: number
    startX: number
    startCenter: number
    min: number
    max: number
    timer: number | null
    active: boolean
  } | null>(null)
  fullRef.current = full
  autoRef.current = autoReveal

  const pending = queue?.pending ?? 0
  const queueRunning = queue?.running ?? 0
  const busy = running && (!!progress || queueRunning > 0 || pending > 0)
  const what =
    progress?.what ?? (pending > 0 ? `排队 ${pending}` : queueRunning > 0 ? '正在执行' : null)
  const pct =
    progress && progress.max > 0
      ? Math.round(Math.max(0, Math.min(100, (progress.value / progress.max) * 100)))
      : null
  const open = near || full
  // 任务运行时也保持那枚小把手；进度细节放到展开后的控制台底部
  const barShown = open
  const railW = barShown ? Math.min(680, vw - 56) : 88
  const anchorCenter =
    railAnchor.side === 'left'
      ? railW / 2 + RAIL_INSET
      : railAnchor.side === 'right'
        ? vw - railW / 2 - RAIL_INSET
        : Math.min(vw - railW / 2 - RAIL_INSET, Math.max(railW / 2 + RAIL_INSET, railAnchor.fraction * vw))
  const railCenterPx = dragCenter ?? anchorCenter
  const panelW = vw <= 1100 ? vw - 40 : Math.min(920, vw - 48)
  const peekLimit = Math.max(0, (vw - panelW) / 2 - RAIL_INSET)
  const peekShift = full ? 0 : Math.max(-peekLimit, Math.min(peekLimit, railCenterPx - vw / 2))
  const railStyle: CSSProperties =
    dragging && dragCenter !== null
      ? { left: `${dragCenter}px`, right: 'auto', transform: 'translateX(-50%)' }
      : railAnchor.side === 'left'
        ? { left: `${RAIL_INSET}px`, right: 'auto', transform: 'none' }
        : railAnchor.side === 'right'
          ? { right: `${RAIL_INSET}px`, left: 'auto', transform: 'none' }
          : { left: `${anchorCenter}px`, right: 'auto', transform: 'translateX(-50%)' }
  const visibleTail = useMemo(() => lines.filter((line) => !TQDM_RE.test(line)), [lines])

  // 切走（终端页签 / 回首页）时把展开状态收回，回来不弹一个盖住页面的旧窗口。
  useEffect(() => {
    if (visible) return
    setFull(false)
    setNear(false)
    const d = dragRef.current
    if (d?.timer) window.clearTimeout(d.timer)
    dragRef.current = null
    dragCenterRef.current = null
    setDragging(false)
    setDragCenter(null)
    document.body.classList.remove('cdock-dragging')
  }, [visible])

  useEffect(() => {
    const onResize = () => setVw(window.innerWidth)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  useEffect(() => {
    try {
      localStorage.setItem(AUTO_KEY, autoReveal ? '1' : '0')
    } catch {
      // 记忆失败只影响下次打开，当前开关仍然生效
    }
  }, [autoReveal])

  useEffect(() => {
    try {
      localStorage.setItem(RAIL_KEY, JSON.stringify(railAnchor))
    } catch {
      // 记忆失败只影响下次打开，当前位置仍然生效
    }
  }, [railAnchor])

  // 悬浮控制台出现时，把 toast 抬到它上面，避免两条浮层叠在一起互相盖住。
  useEffect(() => {
    if (!visible) return
    document.body.classList.add('console-dock-on')
    return () => document.body.classList.remove('console-dock-on')
  }, [visible])

  // 靠近控制台本身才浮起：只有那枚小把手算触发区；任务运行时把手也不变大
  useEffect(() => {
    if (!visible) return
    let raf = 0
    const onMove = (e: PointerEvent) => {
      if (fullRef.current) return
      const x = e.clientX
      const y = e.clientY
      if (raf) cancelAnimationFrame(raf)
      raf = requestAnimationFrame(() => {
        raf = 0
        const pad = 2
        const inside = (el: Element | null, openOnly = false) => {
          if (!el) return false
          if (openOnly && !el.classList.contains('is-open')) return false
          const r = el.getBoundingClientRect()
          return x >= r.left - pad && x <= r.right + pad && y >= r.top - pad && y <= r.bottom + pad
        }
        setNear(inside(panelRef.current, true) || (autoRef.current && inside(railRef.current)))
      })
    }
    const onLeave = () => setNear(false)
    window.addEventListener('pointermove', onMove, { passive: true })
    document.addEventListener('pointerleave', onLeave)
    window.addEventListener('blur', onLeave)
    return () => {
      if (raf) cancelAnimationFrame(raf)
      window.removeEventListener('pointermove', onMove)
      document.removeEventListener('pointerleave', onLeave)
      window.removeEventListener('blur', onLeave)
    }
  }, [visible])

  // 全屏时 Esc 收回；打开时把焦点交给面板，收回时还给进度条按钮。
  useEffect(() => {
    if (!visible) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || !fullRef.current) return
      setFull(false)
      setNear(false)
      railRef.current?.focus({ preventScroll: true })
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [visible])

  const wasFull = useRef(false)
  useEffect(() => {
    if (full) panelRef.current?.focus({ preventScroll: true })
    else if (wasFull.current) railRef.current?.focus({ preventScroll: true })
    wasFull.current = full
  }, [full])

  useEffect(() => {
    try {
      localStorage.setItem(LOG_FONT_KEY, String(font))
    } catch {
      // 记忆失败只影响下次打开，当前字号仍然生效
    }
  }, [font])

  // 日志贴底就继续贴底；用户翻上去看旧输出时不抢滚动位置。
  useEffect(() => {
    if (!visible) return
    const el = logRef.current
    if (!el) return
    const onScroll = () => {
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40
      stickRef.current = atBottom
      setStick(atBottom)
    }
    el.addEventListener('scroll', onScroll, { passive: true })
    return () => el.removeEventListener('scroll', onScroll)
  }, [visible])

  useEffect(() => {
    const el = logRef.current
    if (el && stickRef.current) el.scrollTop = el.scrollHeight
  }, [visibleTail.length, open, full, font])

  if (!visible) return null

  const dismiss = () => {
    setFull(false)
    setNear(false)
    railRef.current?.focus({ preventScroll: true })
  }

  const finishRailDrag = (e?: ReactPointerEvent<HTMLButtonElement>) => {
    const d = dragRef.current
    if (!d) return
    if (d.timer !== null) window.clearTimeout(d.timer)
    if (d.active) {
      const center = dragCenterRef.current ?? d.startCenter
      setRailAnchor(
        center <= d.min + RAIL_SNAP
          ? { side: 'left', fraction: 0 }
          : center >= d.max - RAIL_SNAP
            ? { side: 'right', fraction: 1 }
            : { side: 'free', fraction: Math.min(1, Math.max(0, center / vw)) },
      )
      suppressClickRef.current = true
      document.body.classList.remove('cdock-dragging')
      setDragging(false)
      setDragCenter(null)
      dragCenterRef.current = null
    }
    dragRef.current = null
    const el = e?.currentTarget ?? railRef.current
    if (el) {
      try {
        if (el.hasPointerCapture(d.pointerId)) el.releasePointerCapture(d.pointerId)
      } catch {
        // 拖动结束时指针可能已经离开窗口，capture 已由系统释放
      }
    }
  }

  const onRailPointerDown = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (e.button !== 0 || !e.isPrimary) return
    pressStartRef.current = { x: e.clientX, y: e.clientY }
    suppressClickRef.current = false
    if (autoReveal) return
    const el = e.currentTarget
    const r = el.getBoundingClientRect()
    const startCenter = r.left + r.width / 2
    const min = r.width / 2 + RAIL_INSET
    const max = window.innerWidth - r.width / 2 - RAIL_INSET
    const timer = window.setTimeout(() => {
      const d = dragRef.current
      if (!d) return
      d.active = true
      d.timer = null
      suppressClickRef.current = true
      dragCenterRef.current = d.startCenter
      setDragCenter(d.startCenter)
      setDragging(true)
      document.body.classList.add('cdock-dragging')
    }, LONG_PRESS_MS)
    dragRef.current = {
      pointerId: e.pointerId,
      startX: e.clientX,
      startCenter,
      min,
      max,
      timer,
      active: false,
    }
    try {
      el.setPointerCapture(e.pointerId)
    } catch {
      // capture 失败时仍允许点击展开，只是拖出窗口会中断
    }
  }

  const onRailPointerMove = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (autoReveal) {
      const start = pressStartRef.current
      if (start && Math.hypot(e.clientX - start.x, e.clientY - start.y) > 8) {
        suppressClickRef.current = true
      }
      return
    }
    const d = dragRef.current
    if (!d || d.pointerId !== e.pointerId || !d.active) return
    const next = Math.min(d.max, Math.max(d.min, d.startCenter + (e.clientX - d.startX)))
    dragCenterRef.current = next
    setDragCenter(next)
  }

  const onRailPointerUp = (e: ReactPointerEvent<HTMLButtonElement>) => {
    const d = dragRef.current
    if (!d || d.pointerId !== e.pointerId) return
    finishRailDrag(e)
  }

  const onRailClick = () => {
    if (suppressClickRef.current) {
      suppressClickRef.current = false
      pressStartRef.current = null
      return
    }
    setNear(false)
    setFull(true)
  }

  const meter = (
    <>
      <span className={`cdock-dot${busy ? ' is-live' : ''}`} aria-hidden="true" />
      {what ? (
        <span className="cdock-what">{what}</span>
      ) : (
        <span className="cdock-what cdock-what-idle">控制台</span>
      )}
      {progress?.speed && <span className="cdock-speed mono">{progress.speed}</span>}
      <span className="cdock-spacer" />
      {!what && <span className="cdock-state">{running ? '等待任务' : installed ? '已停止' : '未装环境'}</span>}
      {pending > 0 && what !== `排队 ${pending}` && <span className="cdock-queue mono">排队 {pending}</span>}
      {pct !== null && <span className="cdock-pct mono">{pct}%</span>}
      <Icon name="chevron" size={13} className={`cdock-chev${full ? ' is-down' : ''}`} />
    </>
  )

  const track = (
    <span
      className={`cdock-track${busy ? (pct === null ? ' is-indeterminate' : '') : ' is-idle'}`}
      role="progressbar"
      aria-label="ComfyUI 任务进度"
      aria-valuemin={0}
      aria-valuemax={busy && pct !== null ? 100 : undefined}
      aria-valuenow={busy && pct !== null ? pct : undefined}
    >
      {busy && (
        <span className="cdock-fill" style={pct !== null ? { width: `${pct}%` } : undefined} />
      )}
    </span>
  )

  const stepText =
    progress && progress.max > 0
      ? `${Math.round(Math.max(0, progress.value))} / ${Math.round(progress.max)}`
      : null

  const footMeter = (
    <>
      <span className="cdock-foot-line">
        <span className={`cdock-dot${busy ? ' is-live' : ''}`} aria-hidden="true" />
        {what ? (
          <span className="cdock-what">{what}</span>
        ) : (
          <span className="cdock-what cdock-what-idle">控制台</span>
        )}
        {busy && progress && stage ? <span className="cdock-stage">{stage}</span> : null}
        <span className="cdock-spacer" />
        {!what && (
          <span className="cdock-state">{running ? '等待任务' : installed ? '已停止' : '未装环境'}</span>
        )}
        {busy && progress?.speed ? <span className="cdock-speed mono">{progress.speed}</span> : null}
        <Icon name="chevron" size={13} className={`cdock-chev${full ? ' is-down' : ''}`} />
      </span>
      {busy && (
        <span className="cdock-detail">
          <span className="cdock-steps mono">{stepText ?? '进行中'}</span>
          <span className="cdock-spacer" />
          {pending > 0 && what !== `排队 ${pending}` ? (
            <span className="cdock-queue mono">排队 {pending}</span>
          ) : null}
          {pct !== null ? <span className="cdock-pct mono">{pct}%</span> : null}
        </span>
      )}
    </>
  )

  return createPortal(
    <div className="cdock-layer">
      {full && (
        <div
          className="cdock-scrim"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) dismiss()
          }}
        />
      )}

      <button
        type="button"
        ref={railRef}
        className={`cdock-rail${barShown ? '' : ' is-hidden'}${busy ? ' is-running' : ''}${autoReveal ? '' : ' is-manual'}${dragging ? ' is-dragging' : ''}${open ? ' is-covered' : ''}`}
        style={railStyle}
        aria-label={
          busy
            ? `展开控制台：${what ?? '任务进行中'}`
            : autoReveal
              ? '展开控制台'
              : '展开控制台（可长按拖动）'
        }
        aria-expanded={full}
        title={autoReveal ? '展开控制台' : '点击展开，长按拖动'}
        onClick={onRailClick}
        onPointerDown={onRailPointerDown}
        onPointerMove={onRailPointerMove}
        onPointerUp={onRailPointerUp}
        onPointerCancel={onRailPointerUp}
        onLostPointerCapture={() => finishRailDrag()}
      >
        <span className="cdock-meter">{meter}</span>
        {track}
      </button>

      <section
        ref={panelRef}
        className={`cdock-panel${open ? ' is-open' : ''}${full ? ' is-full' : ''}`}
        aria-label="控制台"
        aria-hidden={!open}
        inert={!open}
        tabIndex={-1}
        style={
          {
            '--cdock-font': `${font}px`,
            '--cdock-shift': `${peekShift}px`,
          } as CSSProperties
        }
      >
        <header className="cdock-head">
          <h2>控制台</h2>
          <span className={`badge ${running ? 'ok' : installed ? 'idle' : 'warn'}`}>
            <span className="dot" />
            {running ? '运行中' : installed ? '已停止' : '未装环境'}
          </span>
          <span className="cdock-spacer" />
          <div className="log-font-actions" role="group" aria-label="控制台字号">
            <button
              className="ghost sm log-font-btn"
              onClick={() => setFont((n) => Math.max(LOG_FONT_MIN, n - 1))}
              disabled={font <= LOG_FONT_MIN}
              title={`减小字号（当前 ${font}px）`}
              aria-label="减小控制台字号"
            >
              A−
            </button>
            <button
              className="ghost sm log-font-btn"
              onClick={() => setFont((n) => Math.min(LOG_FONT_MAX, n + 1))}
              disabled={font >= LOG_FONT_MAX}
              title={`增大字号（当前 ${font}px）`}
              aria-label="增大控制台字号"
            >
              A+
            </button>
          </div>
          <button
            type="button"
            className={`icon-btn cdock-auto${autoReveal ? ' is-on' : ''}`}
            onClick={() => setAutoReveal((v) => !v)}
            title={autoReveal ? '悬浮自动展开：开' : '悬浮自动展开：关'}
            aria-label={autoReveal ? '关闭悬浮自动展开' : '开启悬浮自动展开'}
            aria-pressed={autoReveal}
          >
            <Icon name={autoReveal ? 'eye' : 'eye-off'} size={14} />
          </button>
          <button className="ghost sm cdock-term" onClick={onOpenTerminal} title="切到终端页签">
            <Icon name="console" size={13} />
            终端
          </button>
          <button className="icon-btn cdock-close" onClick={dismiss} title="收起控制台" aria-label="收起控制台">
            <Icon name="close" size={14} />
          </button>
        </header>

        <pre className="cdock-log" ref={logRef} aria-live="off">
          {visibleTail.length ? (
            visibleTail.map((line, i) => {
              const tone = logTone(line)
              const info = INFO_TAG_RE.exec(line)
              return (
                <span className={`log-line${tone ? ` ${tone}` : ''}`} key={i}>
                  {info ? (
                    <>
                      {info[1]}
                      <span className="log-tag-info">{info[2]}</span>
                      {info[3]}
                    </>
                  ) : (
                    line || ' '
                  )}
                </span>
              )
            })
          ) : (
            <span className="cdock-empty">
              <Icon name="console" size={18} />
              <span>{!installed ? '未装环境' : running ? '等待 ComfyUI 输出' : '实例已停止'}</span>
            </span>
          )}
        </pre>

        {!stick && (
          <button
            className="cdock-latest"
            onClick={() => {
              const el = logRef.current
              if (!el) return
              el.scrollTop = el.scrollHeight
              stickRef.current = true
              setStick(true)
            }}
          >
            <Icon name="chevron" size={12} />
            最新
          </button>
        )}

        <button
          type="button"
          className={`cdock-foot${busy ? ' is-rich' : ''}`}
          aria-label={
            full
              ? autoReveal
                ? '收起到半屏'
                : '收起控制台'
              : busy
                ? `展开控制台：${what ?? '任务进行中'}`
                : '展开控制台'
          }
          aria-expanded={full}
          onClick={() => {
            if (full) {
              setFull(false)
              setNear(autoReveal)
            } else {
              setFull(true)
            }
          }}
        >
          <span className="cdock-meter cdock-foot-meter">{footMeter}</span>
          {track}
        </button>
      </section>
    </div>,
    document.body,
  )
}
