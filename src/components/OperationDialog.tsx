import { useEffect, useId, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import Icon from './Icon'
import { api, type NetSpeed } from '../lib/api'

export type OperationState = 'running' | 'ok' | 'failed' | 'cancelled'
export type OperationTab = 'detail' | 'console'

const STAGE_LABELS: Record<string, string> = {
  console: '输出',
  clone: '克隆',
  pull: '拉取',
  deps: '依赖',
  checkout: '切换',
  clean: '清理',
  accel: '轮子',
  install: '安装',
  node: '节点',
  python: 'Python',
}

export function operationStageLabel(stage: string): string {
  return STAGE_LABELS[stage] ?? stage
}

function clampPercent(value: number): number {
  return Math.max(0, Math.min(100, value))
}

/** 本机网络吞吐 → 控制台右上角的 ↓/↑ 指示。 */
function formatRate(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond <= 0) return '0 B/s'
  const units = ['B/s', 'KiB/s', 'MiB/s', 'GiB/s']
  let value = bytesPerSecond
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  const digits = unit === 0 ? 0 : value < 10 ? 1 : value < 100 ? 1 : 0
  return `${value.toFixed(digits)} ${units[unit]}`
}

/** 从 uv / git / 同步日志里提取速度，合并到控制台日志行内显示。 */
export function operationSpeed(message: string): string | null {
  const match = message.match(
    /(\d+(?:\.\d+)?)\s*(B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)\s*\/s/i,
  )
  if (!match) return null
  return `${match[1]} ${match[2]}/s`
}

const BYTE_UNITS: Record<string, number> = {
  B: 1,
  KB: 1024,
  KIB: 1024,
  MB: 1024 ** 2,
  MIB: 1024 ** 2,
  GB: 1024 ** 3,
  GIB: 1024 ** 3,
  TB: 1024 ** 4,
  TIB: 1024 ** 4,
}

/** 从一行日志里找百分比；没有百分比时用“已传/总量”反算。 */
export function operationPercent(message: string): number | null {
  const percent = message.match(/(\d{1,3}(?:\.\d+)?)\s*%(?![0-9A-Fa-f]{2})/)
  if (percent) return clampPercent(Number(percent[1]))
  const fraction = message.match(
    /(\d+(?:\.\d+)?)\s*(B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)\s*\/\s*(\d+(?:\.\d+)?)\s*(B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)/i,
  )
  if (!fraction) return null
  const done = Number(fraction[1]) * (BYTE_UNITS[fraction[2].toUpperCase()] ?? 0)
  const total = Number(fraction[3]) * (BYTE_UNITS[fraction[4].toUpperCase()] ?? 0)
  if (!Number.isFinite(done) || !Number.isFinite(total) || total <= 0) return null
  return clampPercent((done * 100) / total)
}

export interface OperationMeter {
  percent: number | null
  speed: string | null
  /** 已下载 / 总大小；uv 的下载标题里可能带。 */
  size: string | null
}

export function operationMeter(message: string): OperationMeter {
  const fraction = message.match(
    /(\d+(?:\.\d+)?)\s*(B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)\s*\/\s*(\d+(?:\.\d+)?)\s*(B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)/i,
  )
  return {
    percent: operationPercent(message),
    speed: operationSpeed(message),
    size: fraction
      ? `${fraction[1]} ${fraction[2]} / ${fraction[3]} ${fraction[4]}`
      : null,
  }
}

const RATE = String.raw`\d+(?:\.\d+)?\s*(?:B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)\s*\/s`
const SIZE = String.raw`\d+(?:\.\d+)?\s*(?:B|kB|KB|KiB|MB|MiB|GB|GiB|TB|TiB)`

const SPINNER_HEAD_ALL = /^[\s]*[\u2800-\u28FF◐◓◑◒]\s*/

/** 把日志里的文字进度/速度剥掉；它们已经在行首用方块和速度表示。 */
export function operationBody(message: string): string {
  let text = message.replace(SPINNER_HEAD_ALL, '')
  text = text.replace(new RegExp(String.raw`\s*[·•|]\s*${RATE}`, 'gi'), '')
  text = text.replace(new RegExp(String.raw`\s*${RATE}`, 'gi'), '')
  text = text.replace(new RegExp(String.raw`\s*[·•]\s*${SIZE}\s*\/\s*${SIZE}`, 'gi'), '')
  text = text.replace(new RegExp(String.raw`\s*${SIZE}\s*\/\s*${SIZE}`, 'gi'), '')
  text = text.replace(/\s*[·•:：]\s*\d{1,3}(?:\.\d+)?\s*%/g, '')
  text = text.replace(/\s*\d{1,3}(?:\.\d+)?\s*%(?![0-9A-Fa-f]{2})/g, '')
  // pip 的方块条和 ETA 每帧都在变，留在显示里只会让一行来回跳。
  text = text.replace(/\|[^|]*\|/g, '')
  text = text.replace(/\beta\s*[:=]?\s*\d+(?::\d+){0,2}/gi, '')
  text = text.replace(/\d+(?::\d+){1,2}\s*[<]\s*\d+(?::\d+){1,2}/g, '')
  text = text.replace(/\s*[·•|,，;；]\s*$/g, '')
  return text.trim()
}

/** 一条可读的操作输出。at 只负责时间；step 用来把同一阶段的输出归在一起。 */
export interface OperationLine {
  at: string
  step: string
  phase: 'start' | 'log' | 'done' | 'warn' | 'err' | string
  message: string
}

export interface OperationFact {
  label: string
  value: ReactNode
  mono?: boolean
}

interface ConsoleLine {
  ev: OperationLine
  key: string
  /** 进度行按来源分组：同一来源原地刷新，不同阶段各自保留一行。 */
  progressKey: string | null
}

/** 识别“可原地刷新”的进度行，并给出同一进度的稳定分组键。 */
function progressKey(message: string): string | null {
  const text = message.trim()
  if (/^下载轮子(?:完成)?[：:]\s*\d{1,3}(?:\.\d+)?\s*%/.test(text)) return 'wheel-download'

  // git 的 Receiving / Compressing / Counting objects，以及
  // "remote: xxx: 43%" 这种带前缀的进度。
  const colon = text.match(/^(?:remote:\s*)?([^:]{1,60}):\s*\d{1,3}(?:\.\d+)?\s*%/)
  if (colon) return colon[1].trim().toLowerCase()

  // tqdm / pip 的 "0% ComfyUI-Impact-Pack ..."。
  const tqdm = text.match(/^\s*\d{1,3}(?:\.\d+)?\s*%\s*(.+)$/)
  if (tqdm) {
    const tail = tqdm[1].trim()
    if (tail.startsWith('|')) return 'tqdm-bar'
    const label = tail.split(/\s+/)[0]
    return label ? `tqdm:${label.toLowerCase()}` : 'tqdm'
  }

  // 只有百分比或“7.55 MiB | 211.00 KiB/s”这类进度。
  if (/%/.test(text)) return 'percent'
  if (/\d+(?:\.\d+)?\s*[KMGTP]?i?B\s*\/\s*\d+(?:\.\d+)?\s*[KMGTP]?i?B/i.test(text)) {
    return 'size-fraction'
  }
  return null
}

/**
 * 控制台直接显示后端原始输出：时间 + 阶段 + 原始文本。
 * 唯一处理：同一步骤、同一进度来源的连续更新原地替换最后一行，
 * 避免 pip/tqdm/git 把每 1% 都变成一条新日志；不同阶段仍各占一行。
 */
function buildConsoleLines(events: OperationLine[]): ConsoleLine[] {
  const lines: ConsoleLine[] = []
  for (const ev of events) {
    const key = progressKey(ev.message)
    const previous = lines[lines.length - 1]
    if (
      key !== null &&
      previous?.progressKey === key &&
      previous.ev.step === ev.step
    ) {
      lines[lines.length - 1] = { ...previous, ev }
      continue
    }
    lines.push({ ev, key: `line-${lines.length}`, progressKey: key })
  }
  return lines
}

interface Props {
  title: string
  target: ReactNode
  state: OperationState
  phase: string
  percent?: number
  facts?: OperationFact[]
  events: OperationLine[]
  tab: OperationTab
  onTab: (tab: OperationTab) => void
  detailLabel?: string
  consoleLabel?: string
  stepLabel?: (step: string) => string
  closeLabel?: string
  /** 运行中是否允许收起到后台。安装类操作传 false，避免用户以为已经停掉。 */
  allowHideOnRun?: boolean
  /** 显示整机物理网卡吞吐；安装/节点/切版本显示网络，同步显示局域网。 */
  showNet?: boolean
  netLabel?: string
  onClose: () => void
  footer?: ReactNode
  children: ReactNode
}

const STEP_TONE: Record<string, string> = {
  start: 'is-start',
  log: 'is-log',
  done: 'is-done',
  warn: 'is-warn',
  err: 'is-err',
}

/** 所有长操作共用的一块「过程 + 结果」工作台。 */
export default function OperationDialog({
  title,
  target,
  state,
  phase,
  percent,
  facts,
  events,
  tab,
  onTab,
  detailLabel = '详情',
  consoleLabel = '控制台',
  stepLabel = (step) => step,
  closeLabel,
  allowHideOnRun = true,
  showNet = true,
  netLabel = '网络',
  onClose,
  footer,
  children,
}: Props) {
  const titleId = useId()
  const running = state === 'running'
  const canHide = !running || allowHideOnRun
  const modalRef = useRef<HTMLDivElement>(null)
  const consoleRef = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const latestByStep = new Map<string, OperationLine>()
  for (const ev of events) latestByStep.set(ev.step, ev)
  const consoleLines = buildConsoleLines(events)

  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null
    modalRef.current?.focus()
    return () => previous?.focus()
  }, [])

  useEffect(() => {
    if (tab !== 'console' || !stick.current) return
    const el = consoleRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [events.length, tab])

  // 长操作都显示整机物理网卡吞吐；只有同步任务把标签写成“局域网”。
  // 依赖安装不再从目录增长推算磁盘写入速度。
  const [net, setNet] = useState<NetSpeed | null>(null)
  useEffect(() => {
    if (!running || !showNet) {
      setNet(null)
      return
    }
    let alive = true
    let timer = 0
    const tick = async () => {
      try {
        const next = await api.netSpeed()
        if (alive) setNet(next)
      } catch {
        if (alive) setNet(null)
      }
      if (alive) timer = window.setTimeout(tick, 1000)
    }
    void tick()
    return () => {
      alive = false
      window.clearTimeout(timer)
    }
  }, [running, showNet])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && canHide) {
        e.preventDefault()
        onClose()
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [canHide, onClose])

  const tone =
    state === 'running' ? 'is-running' : state === 'ok' ? 'is-ok' : state === 'cancelled' ? 'is-warn' : 'is-err'
  const status =
    state === 'running'
      ? '进行中'
      : state === 'ok'
        ? '已完成'
        : state === 'cancelled'
          ? '已取消'
          : '失败'
  const closeText = closeLabel ?? (running ? (allowHideOnRun ? '收起' : '安装中…') : '关闭')
  const shownFacts =
    facts ??
    [
      { label: '目标', value: target },
      ...(percent !== undefined ? [{ label: '进度', value: `${Math.round(percent)}%`, mono: true }] : []),
      { label: '阶段', value: phase },
    ]

  return createPortal(
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (canHide && e.target === e.currentTarget) onClose()
      }}
    >
      <div
        ref={modalRef}
        className={`modal op-run-modal${running ? '' : ' is-done'}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key !== 'Tab') return
          const focusable = modalRef.current?.querySelectorAll<HTMLElement>(
            'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
          )
          if (!focusable?.length) return
          const first = focusable[0]
          const last = focusable[focusable.length - 1]
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault()
            last.focus()
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault()
            first.focus()
          }
        }}
      >
        <div className="modal-head op-run-head">
          <span className={`op-run-mark ${tone}`} aria-hidden="true">
            <Icon
              name={running ? 'refresh' : state === 'ok' ? 'check' : state === 'cancelled' ? 'close' : 'alert'}
              size={15}
            />
          </span>
          <h2 id={titleId}>{title}</h2>
          <span className={`op-run-status ${tone}`} role="status" aria-live="polite">
            <span className="op-run-status-dot" aria-hidden="true" />
            {status}
          </span>
          <span className="spacer" />
          <button
            className="ghost sm icon-btn"
            onClick={onClose}
            disabled={!canHide}
            aria-label={closeText}
            title={closeText}
          >
            <Icon name="close" size={15} />
          </button>
        </div>

        <div className="op-run-body">
          <div className="op-run-facts">
            {shownFacts.map((fact) => (
              <div
                className={`op-run-fact${fact.mono ? ' is-mono' : ''}`}
                key={fact.label}
                title={typeof fact.value === 'string' ? fact.value : undefined}
              >
                <span>{fact.label}</span>
                <strong>{fact.value}</strong>
              </div>
            ))}
          </div>

          {percent !== undefined && running && (
            <div
              className="op-run-track"
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(percent)}
            >
              <div
                className="op-run-fill"
                style={{ width: `${Math.max(0, Math.min(100, percent))}%` }}
              />
            </div>
          )}

          <div className="op-run-tabs" role="tablist" aria-label={`${title}信息`}>
            <button
              type="button"
              role="tab"
              aria-selected={tab === 'detail'}
              className={`op-run-tab${tab === 'detail' ? ' is-on' : ''}`}
              onClick={() => onTab('detail')}
            >
              {detailLabel}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === 'console'}
              className={`op-run-tab${tab === 'console' ? ' is-on' : ''}`}
              onClick={() => onTab('console')}
            >
              {consoleLabel}
              <span className="op-run-count">{consoleLines.length}</span>
            </button>
            {running && showNet && net && (
              <span
                className="op-run-net"
                title="本机物理网卡实时吞吐（下载 / 上传）；不含虚拟/隧道接口，也不等于单个文件的传输速度"
              >
                <span
                  className={`op-run-net-dot${net.rxBps + net.txBps > 0 ? ' is-on' : ''}`}
                  aria-hidden="true"
                />
                <span className="op-run-net-label">{netLabel}</span>
                <span className="op-run-net-rx">↓ {formatRate(net.rxBps)}</span>
                <span className="op-run-net-tx">↑ {formatRate(net.txBps)}</span>
              </span>
            )}
          </div>

          {tab === 'detail' ? (
            <div className="op-run-pane">
              {children}
              {running && latestByStep.size === 0 && (
                <div className="op-run-live">
                  <span className="op-run-live-mark" aria-hidden="true" />
                  <span>{phase}</span>
                </div>
              )}
              {latestByStep.size > 0 && (
                <div className="op-run-steps">
                  <div className="op-run-sec-head">
                    <span className="op-run-sec-title">执行阶段</span>
                    <span className="op-run-sec-count">{latestByStep.size}</span>
                  </div>
                  {[...latestByStep.entries()].map(([step, ev]) => (
                    <div key={step} className={`op-run-step is-${ev.phase}`}>
                      <span className="op-run-step-mark" aria-hidden="true">
                        {ev.phase === 'done' ? '✓' : ev.phase === 'warn' ? '!' : ev.phase === 'err' ? '×' : '•'}
                      </span>
                      <span className="op-run-step-name">{stepLabel(step)}</span>
                      <span className="op-run-step-msg">{ev.message}</span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <div
              className={`op-run-console${running ? ' is-running' : ''}`}
              ref={consoleRef}
              role="log"
              aria-live="polite"
              aria-label={`${title}控制台`}
              onScroll={(e) => {
                const el = e.currentTarget
                stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 36
              }}
            >
              {consoleLines.length === 0 ? (
                <div className="op-run-console-empty">等待输出…</div>
              ) : (
                consoleLines.map(({ ev, key }) => (
                  <div key={key} className={`op-run-line ${STEP_TONE[ev.phase] ?? 'is-log'}`}>
                    <span className="op-run-time">{ev.at}</span>
                    <span className="op-run-line-step" title={stepLabel(ev.step)}>
                      {stepLabel(ev.step)}
                    </span>
                    <span className="op-run-output">
                      {ev.phase === 'done' && (
                        <span className="op-run-check" aria-hidden="true">
                          ✓
                        </span>
                      )}
                      <span className="op-run-msg" title={ev.message}>
                        {ev.message}
                      </span>
                    </span>
                  </div>
                ))
              )}
            </div>
          )}
        </div>

        <div className="modal-foot">
          <div className="spacer" />
          {footer}
          <button className="ghost" onClick={onClose} disabled={!canHide}>
            {closeText}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  )
}
