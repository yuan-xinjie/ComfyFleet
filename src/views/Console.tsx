import { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebglAddon } from '@xterm/addon-webgl'
import { api, decodeBase64, errText } from '../lib/api'
import {
  TQDM_RE,
  isTaskEnd,
  isTaskStart,
  parseTqdm,
  progressFromTqdm,
  stageFromLine,
  stageFromText,
  tqdmStageText,
  type RunProgress,
} from '../lib/runProgress'
import RunProgressBar from '../components/RunProgressBar'
import type { PtyEvent } from '../types'

/** 控制台里成行的 tqdm 更新：从终端输出里摘出来，只喂给上面的进度条。 */
const CONSOLE_PROGRESS_RE = /\r?[^\r\n]*\d{1,3}%\|[^|]*\|\s*\d+\/\d+\s*\[[^\]]*\][^\r\n]*\r?\n?/g

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
  /** 启动器进程的实时进度，控制台自己没跑任务时用它兜底。 */
  externalProgress?: RunProgress | null
}

export default function ConsoleView({
  instanceId,
  instanceName,
  active = true,
  externalProgress = null,
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

  const [running, setRunning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [label, setLabel] = useState('未连接')
  const [focused, setFocused] = useState(true)
  const [consoleProgress, setConsoleProgress] = useState<RunProgress | null>(null)
  const shownProgress = consoleProgress ?? externalProgress

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
      progressStageRef.current = stageFromText(tqdmStageText(info), progressStageRef.current)
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

  // ---- 终端初始化 ----
  useEffect(() => {
    const host = hostRef.current
    if (!host || termRef.current) return
    aliveRef.current = true

    const term = new Terminal({
      fontFamily: '"SF Mono", "Cascadia Mono", Consolas, monospace',
      fontSize: 13.5,
      lineHeight: 1.35,
      cursorBlink: true,
      scrollback: 10_000,
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
    fit.fit()

    termRef.current = term
    fitRef.current = fit

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
    host.addEventListener('mousedown', onMouseDown)
    host.addEventListener('focusin', onFocusIn)
    host.addEventListener('focusout', onFocusOut)

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
      dataSub.dispose()
      resizeSub.dispose()
      const id = sessionRef.current
      if (id) void api.ptyKill(id).catch(() => undefined)
      sessionRef.current = null
      term.dispose()
      termRef.current = null
      fitRef.current = null
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

  // 进控制台就把环境终端连上：少一次点击。
  // 切换实例时必须先收掉旧 PTY，否则新实例的终端永远连不上且旧 cmd 留在后台。
  const autoRef = useRef(false)
  useEffect(() => {
    autoRef.current = false
    // 让正在 spawn、尚未返回的旧实例请求作废（返回后会自行 ptyKill）。
    attachSeqRef.current += 1
    spawningRef.current = false
    pendingRef.current = []
    const old = sessionRef.current
    if (old) {
      sessionRef.current = null
      setRunning(false)
      setLabel('未连接')
      void api.ptyKill(old).catch(() => undefined)
    }
    const t = window.setTimeout(() => {
      if (autoRef.current) return
      autoRef.current = true
      void enterEnv()
    }, 120)
    return () => window.clearTimeout(t)
  }, [enterEnv, instanceId])

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
  }, [active])

  return (
    <div className="console-wrap">
      <div className="console-bar">
        {running ? (
          <span className="badge ok">
            <span className="dot" />
            已连接
          </span>
        ) : (
          <button className="primary" onClick={() => void enterEnv()} title="打开 cmd.exe，venv 已加入 PATH">
            连接终端
          </button>
        )}
        <button
          className="ghost"
          onClick={() => {
            const id = sessionRef.current
            if (id) void api.ptyWrite(id, '\x03').catch(() => undefined)
          }}
          disabled={!running}
        >
          Ctrl+C
        </button>
        <button className="ghost" onClick={() => void endSession()} disabled={!running}>
          结束会话
        </button>
        <button className="ghost" onClick={() => termRef.current?.clear()}>
          清屏
        </button>
        <button className="ghost sm" onClick={selfTest} disabled={running} title="验证中文与管道">
          自检
        </button>

        <div className="spacer" />
        <span className="meta">{running && !focused ? '键盘未聚焦 · 点一下终端' : label}</span>
      </div>

      {error && <div className="banner err" style={{ margin: '10px 16px 0' }}>{error}</div>}

      {/* xterm 直接往这个 div 里 appendChild */}
      <div className="term-host" ref={hostRef} />
      {shownProgress && <RunProgressBar progress={shownProgress} className="console-progress" />}
    </div>
  )
}
