import Button from './Button'
import IconTextButton from './IconTextButton'
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
import RunProgressList from './RunProgressList'
import { TQDM_RE, type RunProgress } from '../lib/runProgress'
import { isNearLogBottom } from '../lib/logScroll'
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
  /** 当前 ComfyUI 所有活跃进度；控制台展开时显示在底部。 */
  progresses?: RunProgress[]
  /** 原始进程输出，含 tqdm 行；组件自己过滤进度行 */
  lines: string[]
  onOpenFullLog: () => void
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
  progresses = [],
  lines,
  onOpenFullLog,
  onOpenTerminal,
}: Props) {
  const [stick, setStick] = useState(true)
  const [font, setFont] = useState(loadLogFontSize)
  const [size, setSize] = useState<ConsoleSize | null>(loadSize)
  const [drag, setDrag] = useState<ResizeDrag | null>(null)
  const [searchOpen, setSearchOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [matchIndex, setMatchIndex] = useState(0)
  const panelRef = useRef<HTMLElement | null>(null)
  const logRef = useRef<HTMLPreElement | null>(null)
  const searchInputRef = useRef<HTMLInputElement | null>(null)
  const lineRefs = useRef(new Map<number, HTMLSpanElement>())
  const stickRef = useRef(true)
  /** 最近一次内容变高的时间：内容变动引起的瞬时滚动不能当成用户上翻。 */
  const contentAtRef = useRef(0)
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
  const normalizedQuery = searchQuery.trim().toLocaleLowerCase()
  const matches = useMemo(() => {
    if (!normalizedQuery) return []
    const hits: number[] = []
    visibleTail.forEach((line, index) => {
      if (line.toLocaleLowerCase().includes(normalizedQuery)) hits.push(index)
    })
    return hits
  }, [visibleTail, normalizedQuery])
  const hitSet = useMemo(() => new Set(matches), [matches])
  const activeMatch = matches.length ? Math.min(matchIndex, matches.length - 1) : -1
  const searchCount = searchQuery
    ? matches.length
      ? `${activeMatch + 1}/${matches.length}`
      : '无匹配'
    : ''

  const openSearch = () => {
    setSearchOpen(true)
    window.requestAnimationFrame(() => {
      searchInputRef.current?.focus()
      searchInputRef.current?.select()
    })
  }

  const closeSearch = () => {
    setSearchOpen(false)
    setSearchQuery('')
    setMatchIndex(0)
    panelRef.current?.focus({ preventScroll: true })
  }

  const gotoMatch = (direction: 1 | -1) => {
    if (!matches.length) return
    setMatchIndex((Math.max(0, activeMatch) + direction + matches.length) % matches.length)
  }

  useEffect(() => {
    setMatchIndex(0)
  }, [normalizedQuery])

  useEffect(() => {
    if (!visible || !open) return
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.altKey) return
      if (event.key.toLowerCase() !== 'f') return
      event.preventDefault()
      event.stopPropagation()
      openSearch()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [visible, open])

  useEffect(() => {
    if (!searchOpen || !normalizedQuery || activeMatch < 0) return
    const pre = logRef.current
    const lineIndex = matches[activeMatch]
    const el = lineRefs.current.get(lineIndex)
    if (!pre || !el) return
    const preRect = pre.getBoundingClientRect()
    const lineRect = el.getBoundingClientRect()
    pre.scrollTop += lineRect.top - preRect.top - pre.clientHeight / 2 + lineRect.height / 2
  }, [searchOpen, normalizedQuery, activeMatch, matches])

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

  useEffect(() => {
    if (!open) return
    stickRef.current = true
    setStick(true)
  }, [open])

  // 日志贴底就继续贴底；用户翻上去看旧输出时不抢滚动位置。
  useEffect(() => {
    if (!visible) return
    const el = logRef.current
    if (!el) return
    const onScroll = () => {
      // 停在最后 3 行内就继续跟随；再往上翻是在看旧输出，保持当前位置。
      const atBottom = isNearLogBottom(el)
      if (atBottom) {
        stickRef.current = true
        setStick(true)
        return
      }
      // 追加/裁掉顶部行会让滚动位置瞬间离开底部，这不是用户上翻。
      if (performance.now() - contentAtRef.current < 150) return
      stickRef.current = false
      setStick(false)
    }
    el.addEventListener('scroll', onScroll, { passive: true })
    return () => el.removeEventListener('scroll', onScroll)
  }, [visible])

  useLayoutEffect(() => {
    contentAtRef.current = performance.now()
    if (searchOpen && normalizedQuery) return
    const el = logRef.current
    if (el && stickRef.current) el.scrollTop = el.scrollHeight
    // 日志只保留尾部固定行数，追加后 length 不再变化；必须依赖实际内容。
  }, [visibleTail, open, font, searchOpen, normalizedQuery])

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
          <Button
            className={`icon-btn cdock-search-toggle${searchOpen ? ' is-on' : ''}`}
            onClick={() => (searchOpen ? closeSearch() : openSearch())}
            title="搜索控制台（Ctrl+F）"
            aria-label="搜索控制台"
            aria-expanded={searchOpen}
          >
            <Icon name="search" size={14} />
          </Button>
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
          <IconTextButton
            className="ghost sm cdock-term"
            onClick={onOpenFullLog}
            title="打开本轮完整日志"
            icon="file"
            iconSize={13}
          >
            完整日志
          </IconTextButton>
          <IconTextButton className="ghost sm cdock-term" onClick={onOpenTerminal} title="切到终端页签" icon="console" iconSize={13}>
            
            终端
          </IconTextButton>
          <Button className="icon-btn cdock-close" onClick={requestClose} title="收起控制台" aria-label="收起控制台">
            <Icon name="close" size={14} />
          </Button>
        </header>

        {searchOpen && (
          <div className="cdock-search" role="search">
            <Icon name="search" size={13} />
            <input
              ref={searchInputRef}
              className="cdock-search-input"
              type="text"
              value={searchQuery}
              onChange={(event) => setSearchQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Escape') {
                  event.preventDefault()
                  event.stopPropagation()
                  closeSearch()
                } else if (event.key === 'Enter') {
                  event.preventDefault()
                  gotoMatch(event.shiftKey ? -1 : 1)
                }
              }}
              placeholder="搜索"
              aria-label="在控制台中搜索"
              autoComplete="off"
              spellCheck={false}
            />
            <span className="cdock-search-count" aria-live="polite">
              {searchCount}
            </span>
            <button
              type="button"
              className="cdock-search-btn"
              onClick={() => gotoMatch(-1)}
              aria-label="上一个匹配"
              title="上一个匹配"
            >
              <Icon name="up" size={13} />
            </button>
            <button
              type="button"
              className="cdock-search-btn"
              onClick={() => gotoMatch(1)}
              aria-label="下一个匹配"
              title="下一个匹配"
            >
              <Icon name="down" size={13} />
            </button>
            <button
              type="button"
              className="cdock-search-btn"
              onClick={closeSearch}
              aria-label="关闭搜索"
              title="关闭搜索"
            >
              <Icon name="close" size={13} />
            </button>
          </div>
        )}

        <pre
          className="cdock-log"
          ref={logRef}
          aria-live="off"
          onWheel={(event) => {
            if (event.deltaY < 0) {
              stickRef.current = false
              setStick(false)
            }
          }}
        >
          {visibleTail.length ? (
            visibleTail.map((line, i) => {
              const tone = logTone(line)
              const info = INFO_TAG_RE.exec(line)
              const current = searchOpen && normalizedQuery !== '' && matches[activeMatch] === i
              return (
                <span
                  className={`log-line${tone ? ` ${tone}` : ''}${hitSet.has(i) ? ' is-search-hit' : ''}${current ? ' is-search-current' : ''}`}
                  key={i}
                  ref={(el) => {
                    if (el) lineRefs.current.set(i, el)
                    else lineRefs.current.delete(i)
                  }}
                >
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

        <RunProgressList progresses={progresses} className="cdock-run-progress" />

        {!stick && (
          <IconTextButton
            className="cdock-latest"
            onClick={() => {
              const el = logRef.current
              if (!el) return
              el.scrollTop = el.scrollHeight
              stickRef.current = true
              setStick(true)
            }}
           icon="chevron" iconSize={12}>
            
            最新
          </IconTextButton>
        )}
      </section>
    </div>,
    document.body,
  )
}
