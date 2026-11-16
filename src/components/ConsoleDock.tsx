import Button from './Button'
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import { createPortal } from 'react-dom'
import Icon from './Icon'
import { TQDM_RE } from '../lib/runProgress'
import { playIslandWindowCollapse, playIslandWindowMorph } from '../lib/windowMorph'

type LogTone = '' | 'err' | 'warn' | 'ok' | 'dim' | 'progress'
type ResizeMode = 'top' | 'left' | 'corner'

interface ConsoleSize {
  width: number
  height: number
}

interface ResizeDrag extends ConsoleSize {
  mode: ResizeMode
  x: number
  y: number
}

const LOG_FONT_KEY = 'comfyfleet.console.fontSize'
const LOG_FONT_DEFAULT = 13.5
const LOG_FONT_MIN = 11
const LOG_FONT_MAX = 20
const SIZE_KEY = 'comfyfleet.console.size'
const SIZE_EDGE = 12
const SIZE_TOP = 100
const SIZE_MIN_WIDTH = 420
const SIZE_MIN_HEIGHT = 240

function sizeBounds() {
  const vw = Math.max(320, window.innerWidth)
  const vh = Math.max(320, window.innerHeight)
  return {
    maxWidth: Math.max(320, vw - SIZE_EDGE * 2),
    maxHeight: Math.max(180, vh - SIZE_TOP - SIZE_EDGE),
  }
}

function clampSize(width: number, height: number): ConsoleSize {
  const { maxWidth, maxHeight } = sizeBounds()
  return {
    width: Math.min(maxWidth, Math.max(Math.min(SIZE_MIN_WIDTH, maxWidth), width)),
    height: Math.min(maxHeight, Math.max(Math.min(SIZE_MIN_HEIGHT, maxHeight), height)),
  }
}

function loadSize(): ConsoleSize | null {
  try {
    const raw = localStorage.getItem(SIZE_KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw) as Partial<ConsoleSize>
    if (!Number.isFinite(parsed.width) || !Number.isFinite(parsed.height)) return null
    return clampSize(parsed.width as number, parsed.height as number)
  } catch {
    return null
  }
}

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

interface Props {
  /** 当前实例页是否在前台；切到终端页签或离开实例页时整个面板不挂载。 */
  visible: boolean
  /** 由全局任务条控制；true 时显示完整控制台。 */
  open: boolean
  onClose: () => void
  running: boolean
  installed: boolean
  /** 原始进程输出，含 tqdm 行；组件自己过滤进度行 */
  lines: string[]
  onOpenTerminal: () => void
}

/**
 * 实例控制台详情面板。
 *
 * 小长条已经提升为全局任务条；这里只负责点击后展开的日志面板。
 * 不铺背景遮罩、不做半屏预览，保留干净的日志阅读区。
 */
export default function ConsoleDock({
  visible,
  open,
  onClose,
  running,
  installed,
  lines,
  onOpenTerminal,
}: Props) {
  const [stick, setStick] = useState(true)
  const [font, setFont] = useState(loadLogFontSize)
  const [size, setSize] = useState<ConsoleSize | null>(loadSize)
  const [drag, setDrag] = useState<ResizeDrag | null>(null)
  const panelRef = useRef<HTMLElement | null>(null)
  const logRef = useRef<HTMLPreElement | null>(null)
  const stickRef = useRef(true)
  const closeRef = useRef(onClose)
  closeRef.current = onClose
  const closingRef = useRef(false)
  if (open) closingRef.current = false
  const requestClose = () => {
    if (closingRef.current) return
    closingRef.current = true
    playIslandWindowCollapse(panelRef.current)
    closeRef.current()
  }
  const visibleTail = useMemo(() => lines.filter((line) => !TQDM_RE.test(line)), [lines])

  useEffect(() => {
    try {
      localStorage.setItem(LOG_FONT_KEY, String(font))
    } catch {
      // 记忆失败只影响下次打开，当前字号仍然生效
    }
  }, [font])

  useEffect(() => {
    try {
      if (!size) {
        localStorage.removeItem(SIZE_KEY)
        return
      }
      const timer = window.setTimeout(() => {
        try {
          localStorage.setItem(SIZE_KEY, JSON.stringify(size))
        } catch {
          // 尺寸记忆失败不影响当前布局
        }
      }, 120)
      return () => window.clearTimeout(timer)
    } catch {
      return
    }
  }, [size])

  useEffect(() => {
    const onResize = () =>
      setSize((prev) => (prev ? clampSize(prev.width, prev.height) : prev))
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  useEffect(() => {
    if (!drag) return
    document.body.classList.add('cdock-resizing')
    const onMove = (event: PointerEvent) => {
      const dx = event.clientX - drag.x
      const dy = event.clientY - drag.y
      const width = drag.mode === 'top' ? drag.width : drag.width - dx * 2
      const height = drag.mode === 'left' ? drag.height : drag.height - dy
      setSize(clampSize(width, height))
    }
    const onUp = () => setDrag(null)
    window.addEventListener('pointermove', onMove)
    window.addEventListener('pointerup', onUp)
    window.addEventListener('pointercancel', onUp)
    return () => {
      document.body.classList.remove('cdock-resizing')
      window.removeEventListener('pointermove', onMove)
      window.removeEventListener('pointerup', onUp)
      window.removeEventListener('pointercancel', onUp)
    }
  }, [drag])

  const beginResize = (mode: ResizeMode, event: ReactPointerEvent<HTMLElement>) => {
    event.preventDefault()
    event.stopPropagation()
    const rect = panelRef.current?.getBoundingClientRect()
    if (!rect) return
    setDrag({
      mode,
      x: event.clientX,
      y: event.clientY,
      width: rect.width,
      height: rect.height,
    })
  }

  const resetSize = () => {
    setSize(null)
    try {
      localStorage.removeItem(SIZE_KEY)
    } catch {
      // 忽略存储不可用
    }
  }

  useLayoutEffect(() => {
    if (!visible || !open) return
    const el = panelRef.current
    if (!el) return
    return playIslandWindowMorph(el)
  }, [visible, open])

  useEffect(() => {
    if (!visible || !open) return
    panelRef.current?.focus({ preventScroll: true })
  }, [visible, open])

  useEffect(() => {
    if (!visible || !open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      requestClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [visible, open])

  useEffect(() => {
    if (!visible || !open) return
    document.body.classList.add('console-dock-on')
    return () => document.body.classList.remove('console-dock-on')
  }, [visible, open])

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
  }, [visibleTail.length, open, font])

  if (!visible) return null

  return createPortal(
    <div className="cdock-layer">
      {open && (
        <div
          className="cdock-scrim"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) requestClose()
          }}
        />
      )}

      <section
        ref={panelRef}
        className={`cdock-panel${open ? ' is-open' : ''}${drag ? ' is-resizing' : ''}`}
        aria-label="控制台"
        aria-hidden={!open}
        inert={!open}
        tabIndex={-1}
        style={
          {
            '--cdock-font': `${font}px`,
            ...(size ? { width: size.width, height: size.height } : {}),
          } as CSSProperties
        }
      >
        <div
          className="cdock-resize cdock-resize-top"
          role="separator"
          aria-orientation="horizontal"
          aria-label="调整控制台高度"
          title="拖动调整高度"
          onPointerDown={(event) => beginResize('top', event)}
          onDoubleClick={resetSize}
        />
        <div
          className="cdock-resize cdock-resize-left"
          role="separator"
          aria-orientation="vertical"
          aria-label="调整控制台宽度"
          title="拖动调整宽度"
          onPointerDown={(event) => beginResize('left', event)}
        />
        <div
          className="cdock-resize cdock-resize-corner"
          role="separator"
          aria-label="调整控制台大小"
          title="拖动调整大小"
          onPointerDown={(event) => beginResize('corner', event)}
        />
        <header className="cdock-head">
          <h2>控制台</h2>
          <span className={`badge ${running ? 'ok' : installed ? 'idle' : 'warn'}`}>
            <span className="dot" />
            {running ? '运行中' : installed ? '已停止' : '未装环境'}
          </span>
          <span className="cdock-spacer" />
          <div className="log-font-actions" role="group" aria-label="控制台字号">
            <Button
              className="ghost sm log-font-btn"
              onClick={() => setFont((n) => Math.max(LOG_FONT_MIN, n - 1))}
              disabled={font <= LOG_FONT_MIN}
              title={`减小字号（当前 ${font}px）`}
              aria-label="减小控制台字号"
            >
              A−
            </Button>
            <Button
              className="ghost sm log-font-btn"
              onClick={() => setFont((n) => Math.min(LOG_FONT_MAX, n + 1))}
              disabled={font >= LOG_FONT_MAX}
              title={`增大字号（当前 ${font}px）`}
              aria-label="增大控制台字号"
            >
              A+
            </Button>
          </div>
          <Button className="ghost sm cdock-term" onClick={onOpenTerminal} title="切到终端页签">
            <Icon name="console" size={13} />
            终端
          </Button>
          <Button className="icon-btn cdock-close" onClick={requestClose} title="收起控制台" aria-label="收起控制台">
            <Icon name="close" size={14} />
          </Button>
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
          <Button
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
          </Button>
        )}
      </section>
    </div>,
    document.body,
  )
}
