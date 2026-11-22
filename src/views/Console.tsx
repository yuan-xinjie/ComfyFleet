import Button from '../components/Button'
import Icon from '../components/Icon'
import { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebglAddon } from '@xterm/addon-webgl'
import { SearchAddon } from '@xterm/addon-search'
import { api, decodeBase64, errText } from '../lib/api'
import {
  TQDM_RE,
  TQDM_LINE_RE,
  isTaskEnd,
  isTaskStart,
  parseTqdm,
  progressFromTqdm,
  stageFromLine,
  stageFromText,
  stageFromTqdm,
  tqdmStageText,
  type RunProgress,
} from '../lib/runProgress'
import RunProgressList from '../components/RunProgressList'
import type { PtyEvent } from '../types'

/** 终端跟随外观里的等宽字体；CSS 还没就绪时退回系统等宽字体。 */
function terminalFontFamily(): string {
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue('--font-mono')
    .trim()
  return value || "'Cascadia Mono', Consolas, monospace"
}

/** 控制台里成行的 tqdm 更新：从终端输出里摘出来，只喂给上面的进度条。 */
const CONSOLE_PROGRESS_RE = new RegExp(TQDM_LINE_RE.source, 'gi')

const SEARCH_DECORATIONS = {
  matchBackground: '#5a4b00',
  matchBorder: '#8a7300',
  matchOverviewRuler: '#8a7300',
  activeMatchBackground: '#2b63ff',
  activeMatchBorder: '#8fb0ff',
  activeMatchColorOverviewRuler: '#2b63ff',
}

/** WebView 的 Clipboard API 可能被权限挡住；复制失败时退回 execCommand。 */
async function copyText(text: string): Promise<void> {
  if (!text) return
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return
    }
  } catch {
    // 继续走旧接口
  }
  const area = document.createElement('textarea')
  area.value = text
  area.setAttribute('readonly', '')
  area.style.position = 'fixed'
  area.style.left = '-9999px'
  area.style.top = '0'
  document.body.appendChild(area)
  area.select()
  const ok = document.execCommand('copy')
  area.remove()
  if (!ok) throw new Error('系统剪贴板不可用')
}

/** 自检脚本：验证中文、管道吞吐与流式解码（纯 cmd 语法）。 */
const SELF_TEST = [
  'chcp 65001>nul',
  'echo.',
  'echo PTY 自检开始（cmd）',
  '(for /L %i in (1,1,20) do @echo 第 %i 行：中文与管道正常)',
  'echo.',
  'echo PTY 自检完成',
].join(' & ')

interface Props {
  instanceId: string
  instanceName: string
  /** 隐藏的常驻页签没有布局，切回来时要重新适配终端尺寸。 */
  active?: boolean
}

export default function ConsoleView({
  instanceId,
  instanceName,
  active = true,
}: Props) {
  const hostRef = useRef<HTMLDivElement | null>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const sessionRef = useRef<string | null>(null)
  const decoderRef = useRef(new TextDecoder('utf-8'))
  /** 会话 id 还没拿到时产生的输入（典型：ConPTY 的 ESC[6n 光标查询应答），
   *  必须先攒着，拿到 id 立刻补发，否则 cmd 会永久卡在等 CPR。 */
  const pendingRef = useRef<string[]>([])
  const spawningRef = useRef(false)
  const writeErrRef = useRef(false)
  /** 组件是否还活着；异步 spawn 返回时用它决定要不要立刻杀掉新会话。 */
  const aliveRef = useRef(true)
  /** 每次 attach 递增；旧请求返回后直接作废，避免卸载后写已 dispose 的终端。 */
  const attachSeqRef = useRef(0)
  /** 跨 chunk 的半截 tqdm 行；不加缓冲会在拆分处漏到终端里。 */
  const progressCarryRef = useRef('')
  const progressStageRef = useRef('采样')
  const searchAddonRef = useRef<SearchAddon | null>(null)
  const searchInputRef = useRef<HTMLInputElement | null>(null)

  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [label, setLabel] = useState('未连接')
  const [focused, setFocused] = useState(true)
  const [consoleProgress, setConsoleProgress] = useState<RunProgress | null>(null)
  const [searchOpen, setSearchOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState('')
  const [searchResult, setSearchResult] = useState({ index: -1, count: 0 })
  const [searchReady, setSearchReady] = useState(false)
  const [hasSelection, setHasSelection] = useState(false)
  const shownProgresses = consoleProgress ? [consoleProgress] : []

  /**
   * 把 tqdm 进度从终端输出里摘掉：进度更新只改进度条，终端保持一行一条。
   * 其他字节原样交给 xterm，所以命令、提示符和交互都不受影响。
   */
  const consumeTerminalText = useCallback((text: string): string => {
    let input = progressCarryRef.current + text
    progressCarryRef.current = ''

    // 最后一段还没收到分隔符，如果看起来是进度条就先存着，等下一块补全
    const lastBreak = Math.max(input.lastIndexOf('\r'), input.lastIndexOf('\n'))
    const tail = input.slice(lastBreak + 1)
    if (/\d{1,3}%\|/.test(tail) && !TQDM_RE.test(tail)) {
      progressCarryRef.current = tail
      input = input.slice(0, lastBreak + 1)
    }

    CONSOLE_PROGRESS_RE.lastIndex = 0
    for (const match of input.matchAll(CONSOLE_PROGRESS_RE)) {
      const info = parseTqdm(match[0])
      if (!info) continue
      progressStageRef.current =
        stageFromTqdm(info) ||
        stageFromText(tqdmStageText(info), progressStageRef.current)
      setConsoleProgress(progressFromTqdm(info, progressStageRef.current))
    }

    CONSOLE_PROGRESS_RE.lastIndex = 0
    const normal = input.replace(CONSOLE_PROGRESS_RE, '')
    for (const line of normal.split(/\r?\n/)) {
      if (!line.trim()) continue
      if (isTaskEnd(line)) {
        setConsoleProgress(null)
        progressStageRef.current = '采样'
        continue
      }
      if (isTaskStart(line)) {
        setConsoleProgress(null)
        progressStageRef.current = '准备'
        continue
      }
      progressStageRef.current = stageFromLine(line, progressStageRef.current)
    }
    return normal
  }, [])

  const copySelection = useCallback(async () => {
    const term = termRef.current
    const text = term?.getSelection() ?? ''
    if (!text) return false
    try {
      await copyText(text)
      termRef.current?.focus()
      setError(null)
      return true
    } catch (e) {
      setError(`复制失败：${errText(e)}`)
      return false
    }
  }, [])

  const pasteClipboard = useCallback(async () => {
    const term = termRef.current
    if (!term) return false
    try {
      const text = await api.clipboardReadText()
      if (!text) return false
      term.paste(text)
      term.focus()
      setError(null)
      return true
    } catch (e) {
      setError(`粘贴失败：${errText(e)}`)
      return false
    }
  }, [])

  // ---- 终端初始化 ----
  useEffect(() => {
    const host = hostRef.current
    if (!host || termRef.current) return
    aliveRef.current = true

    const term = new Terminal({
      fontFamily: terminalFontFamily(),
      fontSize: 13.5,
      lineHeight: 1.35,
      cursorBlink: true,
      scrollback: 10_000,
      // 搜索匹配高亮走 xterm 的 proposed API；桌面端固定版本，可以接受。
      allowProposedApi: true,
      theme: {
        background: '#030307',
        foreground: '#e7e9f2',
        cursor: '#22d3ee',
        cursorAccent: '#030307',
        selectionBackground: 'rgba(43, 99, 255, 0.42)',
        black: '#0b0b12',
        brightBlack: '#8e95aa',
        green: '#3ddc97',
        yellow: '#ffb454',
        red: '#ff5c6c',
        blue: '#6d7cff',
        cyan: '#22d3ee',
        magenta: '#ff6a4d',
        white: '#e7e9f2',
        brightWhite: '#f5f6fb',
      },
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(host)
    try {
      term.loadAddon(new WebglAddon())
    } catch {
      // WebGL 不可用就退回 DOM 渲染
    }
    const search = new SearchAddon()
    term.loadAddon(search)
    searchAddonRef.current = search
    setSearchReady(true)
    const searchSub = search.onDidChangeResults(({ resultIndex, resultCount }) => {
      setSearchResult({ index: resultIndex, count: resultCount })
    })
    fit.fit()

    termRef.current = term
    fitRef.current = fit

    const selectionSub = term.onSelectionChange(() => {
      setHasSelection(term.hasSelection())
    })
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== 'keydown') return true
      const ctrl = event.ctrlKey || event.metaKey
      const key = event.key.toLowerCase()
      if (ctrl && key === 'c') {
        if (term.hasSelection()) {
          event.preventDefault()
          event.stopPropagation()
          void copySelection()
          return false
        }
        if (event.shiftKey) {
          event.preventDefault()
          event.stopPropagation()
          return false
        }
        return true
      }
      if (ctrl && event.key === 'Insert' && term.hasSelection()) {
        event.preventDefault()
        event.stopPropagation()
        void copySelection()
        return false
      }
      if (ctrl && key === 'v') {
        event.preventDefault()
        event.stopPropagation()
        void pasteClipboard()
        return false
      }
      if (event.shiftKey && event.key === 'Insert') {
        event.preventDefault()
        event.stopPropagation()
        void pasteClipboard()
        return false
      }
      return true
    })

    const dataSub = term.onData((data) => {
      const id = sessionRef.current
      if (id) {
        void api.ptyWrite(id, data).catch((e) => {
          if (writeErrRef.current) return
          writeErrRef.current = true
          term.writeln(`\r\n\x1b[38;2;255;69;58m[写入失败] ${errText(e)}\x1b[0m`)
        })
      } else if (spawningRef.current) {
        pendingRef.current.push(data)
      }
    })

    const onMouseDown = () => term.focus()
    const onFocusIn = () => setFocused(true)
    const onFocusOut = () => setFocused(false)
    const onContextMenu = (event: MouseEvent) => {
      event.preventDefault()
      event.stopPropagation()
      if (term.hasSelection()) void copySelection()
      else void pasteClipboard()
    }
    host.addEventListener('mousedown', onMouseDown)
    host.addEventListener('focusin', onFocusIn)
    host.addEventListener('focusout', onFocusOut)
    host.addEventListener('contextmenu', onContextMenu, true)

    const resizeSub = term.onResize(({ cols, rows }) => {
      const id = sessionRef.current
      if (id) void api.ptyResize(id, cols, rows).catch(() => undefined)
    })

    const observer = new ResizeObserver(() => {
      try {
        fit.fit()
      } catch {
        // 容器尚未布局完成
      }
    })
    observer.observe(host)

    return () => {
      aliveRef.current = false
      attachSeqRef.current += 1
      observer.disconnect()
      host.removeEventListener('mousedown', onMouseDown)
      host.removeEventListener('focusin', onFocusIn)
      host.removeEventListener('focusout', onFocusOut)
      host.removeEventListener('contextmenu', onContextMenu, true)
      dataSub.dispose()
      resizeSub.dispose()
      searchSub.dispose()
      selectionSub.dispose()
      setHasSelection(false)
      searchAddonRef.current = null
      setSearchReady(false)
      const id = sessionRef.current
      if (id) void api.ptyKill(id).catch(() => undefined)
      sessionRef.current = null
      term.dispose()
      termRef.current = null
      fitRef.current = null
    }
  }, [])

  // 外观里换等宽字体时，xterm 要跟着换并重新适配列宽。
  useEffect(() => {
    const term = termRef.current
    if (!term) return
    const sync = () => {
      const family = terminalFontFamily()
      if (!family || term.options.fontFamily === family) return
      term.options.fontFamily = family
      try {
        fitRef.current?.fit()
      } catch {
        // 容器尚未布局完成
      }
    }
    sync()
    window.addEventListener('comfyfleet:fonts', sync)
    document.fonts?.addEventListener('loadingdone', sync)
    return () => {
      window.removeEventListener('comfyfleet:fonts', sync)
      document.fonts?.removeEventListener('loadingdone', sync)
    }
  }, [])

  // ---- 统一的会话挂载 ----
  const attach = useCallback(
    async (factory: (onEvent: (ev: PtyEvent) => void) => Promise<string>, labelText: string) => {
      const term = termRef.current
      if (!term) return
      if (sessionRef.current || spawningRef.current) {
        term.writeln('\r\n\x1b[38;2;255;159;10m已有会话在运行，先点「结束会话」。\x1b[0m')
        return
      }
      const seq = ++attachSeqRef.current
      let exitedBeforeReady = false
      decoderRef.current = new TextDecoder('utf-8')
      progressCarryRef.current = ''
      progressStageRef.current = '采样'
      setConsoleProgress(null)
      setError(null)
      try {
        spawningRef.current = true
        const id = await factory((ev) => {
          if (seq !== attachSeqRef.current || !aliveRef.current) return
          if (ev.type === 'data') {
            const text = consumeTerminalText(decoderRef.current.decode(decodeBase64(ev.b64), { stream: true }))
            if (text) term.write(text)
          } else {
            if (!sessionRef.current) exitedBeforeReady = true
            term.write(`\r\n\x1b[38;2;152;152;157m[会话结束 code=${ev.code}]\x1b[0m\r\n`)
            decoderRef.current = new TextDecoder('utf-8')
            progressCarryRef.current = ''
            progressStageRef.current = '采样'
            setConsoleProgress(null)
            sessionRef.current = null
            setRunning(false)
            setLabel('未连接')
          }
        })
        spawningRef.current = false
        if (seq !== attachSeqRef.current || !aliveRef.current) {
          // 组件已卸载/切换实例：新会话没人接收了，必须立刻回收，不能留孤儿 cmd。
          void api.ptyKill(id).catch(() => undefined)
          return
        }
        if (exitedBeforeReady) {
          void api.ptyKill(id).catch(() => undefined)
          return
        }
        sessionRef.current = id
        setRunning(true)
        setLabel(labelText)
        writeErrRef.current = false

        const pending = pendingRef.current
        pendingRef.current = []
        for (const chunk of pending) {
          void api.ptyWrite(id, chunk).catch(() => undefined)
        }
        term.focus()
      } catch (e) {
        spawningRef.current = false
        pendingRef.current = []
        if (seq !== attachSeqRef.current || !aliveRef.current) return
        const text = errText(e)
        setError(text)
        term.writeln(`\r\n\x1b[38;2;255;69;58m${text}\x1b[0m`)
      }
    },
    [consumeTerminalText],
  )

  // 名称只用于终端标题：重命名实例不应该重建 PTY、杀掉正在跑的 cmd。
  const instanceNameRef = useRef(instanceName)
  instanceNameRef.current = instanceName
  const enterEnv = useCallback(async () => {
    await attach(
      (onEvent) =>
        api.ptySpawnInstance(instanceId, termRef.current?.cols ?? 120, termRef.current?.rows ?? 30, onEvent),
      `CMD · ${instanceNameRef.current}`,
    )
  }, [attach, instanceId])

  useEffect(() => {
    if (sessionRef.current) setLabel(`CMD · ${instanceName}`)
  }, [instanceName])

  // 终端默认不连接，等用户点「连接终端」。
  // 切换实例时先收掉旧 PTY；正在 spawn 的旧请求作废后会自行 ptyKill。
  useEffect(() => {
    attachSeqRef.current += 1
    spawningRef.current = false
    pendingRef.current = []
    const old = sessionRef.current
    sessionRef.current = null
    setRunning(false)
    setLabel('未连接')
    if (old) void api.ptyKill(old).catch(() => undefined)
  }, [instanceId])

  const endSession = useCallback(async () => {
    const id = sessionRef.current
    if (!id) return
    try {
      await api.ptyKill(id)
    } catch {
      // 会话可能已自行退出
    }
    sessionRef.current = null
    progressCarryRef.current = ''
    progressStageRef.current = '采样'
    setConsoleProgress(null)
    setRunning(false)
    setLabel('未连接')
  }, [])

  const selfTest = useCallback(() => {
    if (sessionRef.current) {
      termRef.current?.writeln('\r\n\x1b[38;2;255;159;10m自检需要干净会话：先「结束会话」。\x1b[0m')
      return
    }
    void attach(
      (onEvent) =>
        api.ptySpawn(
          {
            program: 'cmd.exe',
            args: ['/C', SELF_TEST],
            cwd: null,
            cols: termRef.current?.cols ?? 120,
            rows: termRef.current?.rows ?? 30,
          },
          onEvent,
        ),
      'PTY 自检 · cmd',
    )
  }, [attach])

  // 从别的页面切回终端：隐藏期间没有布局，必须重新 fit
  useEffect(() => {
    if (!active) return
    const id = window.requestAnimationFrame(() => {
      try {
        fitRef.current?.fit()
      } catch {
        // 终端还没建好
      }
    })
    return () => window.cancelAnimationFrame(id)
  }, [active, searchOpen])

  const openSearch = useCallback(() => {
    setSearchOpen(true)
    window.requestAnimationFrame(() => {
      searchInputRef.current?.focus()
      searchInputRef.current?.select()
    })
  }, [])

  const closeSearch = useCallback(() => {
    setSearchOpen(false)
    setSearchQuery('')
    setSearchResult({ index: -1, count: 0 })
    searchAddonRef.current?.clearDecorations()
    termRef.current?.focus()
  }, [])

  const searchNavigate = useCallback(
    (direction: 1 | -1) => {
      const search = searchAddonRef.current
      if (!search) return
      if (!searchQuery) {
        searchInputRef.current?.focus()
        return
      }
      const options = { decorations: SEARCH_DECORATIONS, incremental: false }
      if (direction === 1) search.findNext(searchQuery, options)
      else search.findPrevious(searchQuery, options)
    },
    [searchQuery],
  )

  useEffect(() => {
    if (!searchReady) return
    const search = searchAddonRef.current
    if (!search) return
    if (!searchOpen || !searchQuery) {
      search.clearDecorations()
      setSearchResult({ index: -1, count: 0 })
      return
    }
    search.findNext(searchQuery, { decorations: SEARCH_DECORATIONS, incremental: true })
  }, [searchReady, searchOpen, searchQuery])

  useEffect(() => {
    if (!active) return
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.altKey) return
      if (event.key.toLowerCase() !== 'f') return
      event.preventDefault()
      event.stopPropagation()
      openSearch()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [active, openSearch])

  const searchCount = searchQuery
    ? searchResult.count === 0
      ? '无匹配'
      : `${searchResult.index >= 0 ? searchResult.index + 1 : 0}/${searchResult.count}`
    : ''

  return (
    <div className="console-wrap">
      <div className="console-bar">
        {running ? (
          <span className="badge ok">
            <span className="dot" />
            已连接
          </span>
        ) : (
          <Button className="primary" onClick={() => void enterEnv()} title="打开 cmd.exe，venv 已加入 PATH">
            连接终端
          </Button>
        )}
        <Button
          className="ghost"
          onClick={() => {
            const id = sessionRef.current
            if (id) void api.ptyWrite(id, '\x03').catch(() => undefined)
          }}
          disabled={!running}
        >
          Ctrl+C
        </Button>
        <Button className="ghost" onClick={() => void copySelection()} disabled={!hasSelection}>
          复制
        </Button>
        <Button className="ghost" onClick={() => void pasteClipboard()}>
          粘贴
        </Button>
        <Button className="ghost" onClick={() => void endSession()} disabled={!running}>
          结束会话
        </Button>
        <Button className="ghost" onClick={() => termRef.current?.clear()}>
          清屏
        </Button>
        <Button className="ghost sm" onClick={selfTest} disabled={running} title="验证中文与管道">
          自检
        </Button>
        <Button
          className={`ghost sm console-search-toggle${searchOpen ? ' is-on' : ''}`}
          icon
          onClick={() => (searchOpen ? closeSearch() : openSearch())}
          aria-label="搜索控制台"
          aria-expanded={searchOpen}
          title="搜索控制台（Ctrl+F）"
        >
          <Icon name="search" size={14} />
        </Button>

        <div className="spacer" />
        <span className="meta">{running && !focused ? '键盘未聚焦 · 点一下终端' : label}</span>
      </div>

      {searchOpen && (
        <div className="console-search" role="search">
          <Icon name="search" size={13} />
          <input
            ref={searchInputRef}
            className="console-search-input"
            type="text"
            value={searchQuery}
            onChange={(event) => setSearchQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Escape') {
                event.preventDefault()
                closeSearch()
              } else if (event.key === 'Enter') {
                event.preventDefault()
                searchNavigate(event.shiftKey ? -1 : 1)
              }
            }}
            placeholder="搜索"
            aria-label="在控制台中搜索"
            autoComplete="off"
            spellCheck={false}
          />
          <span className="console-search-count" aria-live="polite">
            {searchCount}
          </span>
          <button
            type="button"
            className="console-search-btn"
            onClick={() => searchNavigate(-1)}
            aria-label="上一个匹配"
            title="上一个匹配"
          >
            <Icon name="up" size={13} />
          </button>
          <button
            type="button"
            className="console-search-btn"
            onClick={() => searchNavigate(1)}
            aria-label="下一个匹配"
            title="下一个匹配"
          >
            <Icon name="down" size={13} />
          </button>
          <button
            type="button"
            className="console-search-btn"
            onClick={closeSearch}
            aria-label="关闭搜索"
            title="关闭搜索"
          >
            <Icon name="close" size={13} />
          </button>
        </div>
      )}

      {error && <div className="banner err" style={{ margin: '10px 16px 0' }}>{error}</div>}

      {/* xterm 直接往这个 div 里 appendChild */}
      <div className="term-host" ref={hostRef} />
      <RunProgressList progresses={shownProgresses} className="console-progress" />
    </div>
  )
}
