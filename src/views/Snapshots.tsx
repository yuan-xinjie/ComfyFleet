import { useCallback, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { api, errText } from '../lib/api'
import type {
  InstanceConfig,
  MissingWheel,
  NodeRef,
  RestoreEvent,
  RestoreOptions,
  RestorePreview,
  RestoreReport,
  ServerOpts,
  SnapshotDetail,
  SnapshotDiff,
  SnapshotSummary,
} from '../types'
import Icon from '../components/Icon'
import OperationDialog, {
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'

/**
 * 快照页：布局与交互对齐 ComfyUI Desktop 的 Snapshots（时间线轨道 + 可展开的行）。
 *
 * - 轨道第一个节点是「创建快照」；恢复中 / 恢复完成 / 恢复失败也显示在这个槽位
 *   （Desktop 同样把 restore 的状态卡放在 Save 里）。
 * - 每行两段：触发来源 + 最新徽标 / 相对时间 + 展开箭头；第二段是结论胶囊
 *   （版本、节点差异、包差异）。
 * - 展开后是一张卡片：节点/包计数、备注、快照清单、两个折叠差异
 *   （这条改了什么 / 恢复它会改动什么，后者按需算）、右下角恢复与删除。
 */

interface Props {
  inst: InstanceConfig
  /** 实例在跑的时候不允许恢复（文件被占用） */
  running: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
  onChanged: () => void
}

interface OpState {
  state: 'running' | 'ok' | 'error'
  /** 目标行的显示名：备注 > 版本 */
  label: string
  percent: number
  phase: string
  last: string
  error: string | null
}

const TRIGGER_LABEL: Record<string, string> = {
  manual: '手动',
  boot: '启动',
  'pre-update': '更新前',
  'pre-deps': '装依赖前',
  'post-update': '更新后',
  'pre-restore': '恢复前',
  'post-restore': '恢复后',
}

/** 一次真实的状态切换（Desktop 用橙色点强调这两种） */
function isStateTrigger(trigger: string): boolean {
  return trigger === 'post-update' || trigger === 'post-restore'
}

const STEP_LABEL: Record<string, string> = {
  core: 'ComfyUI 版本',
  nodes: '自定义节点',
  packages: 'Python 包',
  launch: '启动参数',
  snapshot: '快照',
}

function pad(n: number): string {
  return n < 10 ? `0${n}` : String(n)
}

function absTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(
    d.getMinutes(),
  )}:${pad(d.getSeconds())}`
}

/** 列表行里显示到分钟，避免右对齐区域被秒数撑得太宽 */
function absTimeShort(iso: string): string {
  return absTime(iso).slice(0, 16)
}

/** Desktop 的「Latest: 5 minutes ago」风格 */
function relTime(iso: string): string {
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return iso
  const diff = Date.now() - t
  if (diff < 45_000) return '刚刚'
  const minutes = Math.round(diff / 60_000)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.round(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.round(hours / 24)
  if (days < 30) return `${days} 天前`
  return absTime(iso).slice(0, 10)
}

function short(sha: string | null): string {
  if (!sha) return '—'
  return sha.length > 7 ? sha.slice(0, 7) : sha
}

/** 行标题：备注 > 版本变化 > 当前版本（和 Desktop 的 title pill 一致） */
function titlePill(item: SnapshotSummary): string {
  if (item.label && item.label.trim()) return item.label
  if (item.diff.comfyuiChanged && item.previousVersion) {
    return `${item.previousVersion} → ${item.comfyuiVersion}`
  }
  return item.comfyuiVersion
}

/** 把差异拆成 Desktop 那种 +/−/~ 行 */
interface DiffLine {
  kind: 'add' | 'rm' | 'chg'
  text: string
}

function nodeLines(diff: SnapshotDiff): DiffLine[] {
  // 读不到 commit（节点不是 git 仓库）就不写哈希，免得整行都是 `—`
  const withHash = (n: NodeRef) => (n.commit ? ` ${short(n.commit)}` : '')
  return [
    ...diff.nodesAdded.map((n) => ({ kind: 'add' as const, text: `+ ${n.name}${withHash(n)}` })),
    ...diff.nodesRemoved.map((n) => ({ kind: 'rm' as const, text: `− ${n.name}${withHash(n)}` })),
    ...diff.nodesChanged.map((n) => {
      const toggle =
        n.fromEnabled === n.toEnabled
          ? ''
          : `${n.fromEnabled ? '启用' : '禁用'} → ${n.toEnabled ? '启用' : '禁用'}`
      const commitMoved = !!n.fromCommit && !!n.toCommit && n.fromCommit !== n.toCommit
      const local = n.contentChanged
        ? n.fromDirty || n.toDirty
          ? '未提交改动'
          : '本地内容'
        : ''
      const parts = [
        commitMoved ? `${short(n.fromCommit)} → ${short(n.toCommit)}` : '',
        toggle,
        local,
      ].filter(Boolean)
      return { kind: 'chg' as const, text: `~ ${n.name}: ${parts.join(', ')}` }
    }),
  ]
}

function pkgLines(diff: SnapshotDiff): DiffLine[] {
  return [
    ...diff.packagesAdded.map((p) => ({ kind: 'add' as const, text: `+ ${p.name} ${p.to}` })),
    ...diff.packagesRemoved.map((p) => ({ kind: 'rm' as const, text: `− ${p.name} ${p.from}` })),
    ...diff.packagesChanged.map((p) => ({
      kind: 'chg' as const,
      text: `~ ${p.name}: ${p.from} → ${p.to}`,
    })),
  ]
}

function serverText(server: ServerOpts | null): string {
  if (!server) return '未记录'
  const listen = server.listen || '—'
  const cors = server.corsOrigin ? ` · CORS ${server.corsOrigin}` : ''
  const extra = server.extraArgs.length > 0 ? ` · ${server.extraArgs.length} 个自定义参数` : ''
  return `${listen}:${server.port}${cors}${extra}`
}

function launchLines(diff: SnapshotDiff): DiffLine[] {
  if (!diff.launchChanged) return []
  return [{ kind: 'chg' as const, text: `${serverText(diff.launchFrom)} → ${serverText(diff.launchTo)}` }]
}

function DiffLines({ lines, cap }: { lines: DiffLine[]; cap: number }) {
  const shown = lines.slice(0, cap)
  return (
    <>
      {shown.map((line, index) => (
        <div key={`${line.kind}-${index}`} className={`snap-diff-line is-${line.kind}`}>
          {line.text}
        </div>
      ))}
      {lines.length > cap && <div className="snap-diff-more">…还有 {lines.length - cap} 项</div>}
    </>
  )
}

function SnapshotDiffView({ diff, cap = 40 }: { diff: SnapshotDiff; cap?: number }) {
  const nodes = nodeLines(diff)
  const pkgs = pkgLines(diff)
  const launch = launchLines(diff)
  if (!diff.comfyuiChanged && launch.length === 0 && nodes.length === 0 && pkgs.length === 0) {
    return <div className="snap-diff-empty">没有差异</div>
  }
  return (
    <div className="snap-diff">
      {diff.comfyuiChanged && (
        <div className="snap-diff-sec">
          <div className="snap-diff-title">ComfyUI 版本</div>
          <div className="snap-diff-line is-chg">
            {(diff.comfyuiFrom ?? '?') + ' → ' + (diff.comfyuiTo ?? '?')}
          </div>
        </div>
      )}
      {launch.length > 0 && (
        <div className="snap-diff-sec">
          <div className="snap-diff-title">启动参数</div>
          <DiffLines lines={launch} cap={cap} />
        </div>
      )}
      {nodes.length > 0 && (
        <div className="snap-diff-sec">
          <div className="snap-diff-title">自定义节点 ({nodes.length})</div>
          <DiffLines lines={nodes} cap={cap} />
        </div>
      )}
      {pkgs.length > 0 && (
        <div className="snap-diff-sec">
          <div className="snap-diff-title">Python 包 ({pkgs.length})</div>
          <DiffLines lines={pkgs} cap={cap} />
        </div>
      )}
    </div>
  )
}

/** 折叠的差异块：展开时才去算（Desktop 也只在展开时拉 diff） */
function DiffAccordion(props: {
  title: string
  diff: SnapshotDiff | null
  busy: boolean
  onOpen: () => void
}) {
  const { title, diff, busy, onOpen } = props
  const [open, setOpen] = useState(false)
  return (
    <div className={`snap-acc${open ? ' is-open' : ''}`}>
      <button
        type="button"
        className="snap-acc-trigger"
        aria-expanded={open}
        onClick={() => {
          const next = !open
          setOpen(next)
          if (next && !diff) onOpen()
        }}
      >
        <span className="snap-acc-caret">
          <Icon name="chevron" size={11} />
        </span>
        <span>{title}</span>
      </button>
      {open &&
        (busy ? (
          <div className="snap-diff-loading">读取差异…</div>
        ) : diff ? (
          <SnapshotDiffView diff={diff} />
        ) : (
          <div className="snap-diff-loading">没有差异</div>
        ))}
    </div>
  )
}

function ReportPanel({ report, open = false }: { report: RestoreReport; open?: boolean }) {
  const failed = report.nodes.failed.length + report.packages.failed.length
  const leftover = report.packages.leftover ?? []
  const changed =
    report.nodes.installed.length +
    report.nodes.switched.length +
    report.nodes.contentRestored.length +
    report.nodes.enabled.length +
    report.nodes.disabled.length +
    report.nodes.removed.length +
    report.packages.installed.length +
    report.packages.changed.length +
    report.packages.removed.length +
    (report.launch.changed ? 1 : 0)
  return (
    <details className="snap-report" open={open || failed > 0 || leftover.length > 0}>
      <summary>
        恢复详情 · {changed} 项已执行{failed > 0 ? ` · ${failed} 项失败` : ''}
        {leftover.length > 0 ? ` · 仍多 ${leftover.length} 个包` : ''}
      </summary>
      <div className="snap-report-body">
        <div className="snap-diff-line is-chg">
          ComfyUI：
          {report.core.changed
            ? `${short(report.core.from)} → ${short(report.core.to)}`
            : '未变'}
        </div>
        <div className="snap-diff-line is-chg">
          节点：装回 {report.nodes.installed.length} · 切换 {report.nodes.switched.length} · 还原内容{' '}
          {report.nodes.contentRestored.length} · 启用 {report.nodes.enabled.length} · 禁用{' '}
          {report.nodes.disabled.length} · 删除 {report.nodes.removed.length}
        </div>
        <div className="snap-diff-line is-chg">
          包：装/换 {report.packages.installed.length + report.packages.changed.length} · 删除{' '}
          {report.packages.removed.length}
        </div>
        <div className="snap-diff-line is-chg">
          启动参数：
          {report.launch.changed
            ? `${serverText(report.launch.from)} → ${serverText(report.launch.to)}`
            : report.launch.to
              ? '未变'
              : '老快照未记录，保持当前设置'}
        </div>
        {report.core.error && <div className="snap-report-warn">版本没切成：{report.core.error}</div>}
        {leftover.length > 0 && (
          <div className="snap-report-warn">
            这些包在卸载后仍然存在：{leftover.join('、')}。可先检查包是否被其他进程占用，再重试恢复。
          </div>
        )}
        {[...report.nodes.failed, ...report.packages.failed].map((f) => (
          <div key={`${f.name}-${f.error}`} className="snap-report-warn">
            {f.name}：{f.error}
          </div>
        ))}
        {report.packages.protectedSkipped.length > 0 && (
          <div className="snap-report-note">
            torch 系与打包工具按约定不动：
            {report.packages.protectedSkipped
              .map((p) => `${p.name} ${p.current ?? '—'}→${p.target ?? '—'}`)
              .join('、')}
          </div>
        )}
        {report.warnings.map((w) => (
          <div key={w} className="snap-report-warn">
            {w}
          </div>
        ))}
      </div>
    </details>
  )
}

export default function SnapshotsView({ inst, running, notify, onChanged }: Props) {
  const [items, setItems] = useState<SnapshotSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [label, setLabel] = useState('')
  const [creating, setCreating] = useState(false)
  const [open, setOpen] = useState<string | null>(null)
  const [details, setDetails] = useState<Record<string, SnapshotDetail>>({})
  const [detailBusy, setDetailBusy] = useState<string | null>(null)
  const [rowDiffs, setRowDiffs] = useState<Record<string, RestorePreview>>({})
  const [diffBusy, setDiffBusy] = useState<string | null>(null)
  const [confirm, setConfirm] = useState<{ id: string; preview: RestorePreview } | null>(null)
  const [wheelGate, setWheelGate] = useState<{
    id: string
    missing: MissingWheel[]
    retried: boolean
  } | null>(null)
  const [gateBusy, setGateBusy] = useState(false)
  const [previewing, setPreviewing] = useState<string | null>(null)
  const [arming, setArming] = useState<string | null>(null)
  const [opts, setOpts] = useState<RestoreOptions>({
    removeExtraNodes: true,
    removeExtraPackages: true,
  })
  const [op, setOp] = useState<OpState | null>(null)
  const [report, setReport] = useState<RestoreReport | null>(null)
  const [restoreOpen, setRestoreOpen] = useState(false)
  const [restoreTab, setOperationTab] = useState<OperationTab>('detail')
  const [restoreEvents, setRestoreEvents] = useState<OperationLine[]>([])
  const [restoreTarget, setRestoreTarget] = useState<SnapshotSummary | null>(null)
  const loadSeq = useRef(0)
  const previewSeq = useRef(0)

  const load = useCallback(async () => {
    const seq = ++loadSeq.current
    try {
      const list = await api.snapshotList(inst.id)
      if (seq !== loadSeq.current) return
      setItems(list)
    } catch (e) {
      if (seq === loadSeq.current) notify('err', errText(e))
    } finally {
      if (seq === loadSeq.current) setLoading(false)
    }
  }, [inst.id, notify])

  useEffect(() => {
    // 换实例时先把上一个实例的列表/确认框清掉，避免误点“恢复”到另一实例。
    previewSeq.current += 1
    setItems([])
    setLoading(true)
    setOpen(null)
    setDetails({})
    setRowDiffs({})
    setConfirm(null)
    setReport(null)
    setOp(null)
    setRestoreOpen(false)
    setRestoreEvents([])
    setRestoreTarget(null)
    void load()
  }, [load, inst.id])

  const busy = op?.state === 'running'

  useEffect(() => {
    if (!confirm) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !busy) setConfirm(null)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [confirm, busy])

  useEffect(() => {
    if (!restoreOpen) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setRestoreOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [restoreOpen])

  const create = async () => {
    if (creating || busy) return
    setCreating(true)
    try {
      await api.snapshotCreate(inst.id, label.trim() ? label.trim() : null)
      setLabel('')
      notify('ok', '已记录当前状态')
      await load()
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setCreating(false)
    }
  }

  /** 详情按需加载；已缓存就不重复请求 */
  const ensureDetail = async (id: string) => {
    if (details[id]) return
    setDetailBusy(id)
    try {
      const detail = await api.snapshotDetail(inst.id, id)
      setDetails((prev) => ({ ...prev, [id]: detail }))
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setDetailBusy(null)
    }
  }

  const toggle = async (id: string) => {
    if (open === id) {
      setOpen(null)
      return
    }
    setOpen(id)
    setArming(null)
    await ensureDetail(id)
  }

  /** 「恢复它会改动什么」按需计算（要现场采一次当前状态） */
  const ensureRowDiff = async (id: string) => {
    if (rowDiffs[id] || diffBusy === id) return
    setDiffBusy(id)
    try {
      const preview = await api.snapshotRestorePreview(inst.id, id)
      setRowDiffs((prev) => ({ ...prev, [id]: preview }))
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setDiffBusy(null)
    }
  }

  /** 删除两段式确认：第一下变成「确认删除」，4 秒后自动收回 */
  const remove = async (item: SnapshotSummary) => {
    if (arming !== item.id) {
      setArming(item.id)
      window.setTimeout(() => setArming((cur) => (cur === item.id ? null : cur)), 4000)
      return
    }
    setArming(null)
    try {
      await api.snapshotDelete(inst.id, item.id)
      if (open === item.id) setOpen(null)
      setDetails((prev) => {
        const next = { ...prev }
        delete next[item.id]
        return next
      })
      await load()
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const askRestore = async (id: string) => {
    const seq = ++previewSeq.current
    setPreviewing(id)
    try {
      // 确认框必须现场算：缓存的行差异可能来自恢复/装包/改节点之前，
      // 用它展示“会删哪些东西”会把用户带偏。
      const preview = await api.snapshotRestorePreview(inst.id, id)
      if (seq !== previewSeq.current) return
      setRowDiffs((prev) => ({ ...prev, [id]: preview }))
      // 要装的包里缺 wheel：先让用户补文件，别等恢复跑到一半才报失败
      if (preview.missingWheels.length) {
        setWheelGate({ id, missing: preview.missingWheels, retried: false })
        return
      }
      setConfirm({ id, preview })
    } catch (e) {
      if (seq === previewSeq.current) notify('err', errText(e))
    } finally {
      if (seq === previewSeq.current) setPreviewing(null)
    }
  }

  /** 补齐后重新检查：通过了就直接开始恢复，还缺就把最新列表再摆出来。 */
  const retryWheelGate = async () => {
    if (!wheelGate || gateBusy) return
    const gate = wheelGate
    setGateBusy(true)
    try {
      const preview = await api.snapshotRestorePreview(inst.id, gate.id)
      setRowDiffs((prev) => ({ ...prev, [gate.id]: preview }))
      if (preview.missingWheels.length) {
        setWheelGate({ ...gate, missing: preview.missingWheels, retried: true })
        notify(
          'err',
          `仍缺少 ${preview.missingWheels.length} 个 wheel：补齐后再点「继续」，或确认跳过`,
        )
        return
      }
      setWheelGate(null)
      // 补检查通过：回到正常的恢复确认框，恢复选项还能改。
      setConfirm({ id: gate.id, preview })
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setGateBusy(false)
    }
  }

  /** 用户确认跳过：不装这些包，恢复继续，报告里会列出它们。 */
  const skipWheelGate = () => {
    if (!wheelGate) return
    const { id, missing } = wheelGate
    setWheelGate(null)
    notify('err', `已跳过 ${missing.length} 个缺少 wheel 的包：恢复后它们不会被安装`)
    void startRestore(id)
  }

  const startRestore = async (id: string) => {
    const target = items.find((it) => it.id === id)
    const targetLabel = target ? titlePill(target) : short(id)
    setOp({
      state: 'running',
      label: targetLabel,
      percent: 0,
      phase: '准备中…',
      last: '',
      error: null,
    })
    setRestoreTarget(target ?? null)
    setRestoreEvents([])
    setOperationTab('detail')
    setRestoreOpen(true)
    setConfirm(null)
    setReport(null)
    try {
      const result = await api.snapshotRestore(inst.id, id, opts, (ev: RestoreEvent) => {
        const phaseText =
          ev.phase === 'done' ? '完成' : ev.phase === 'warn' ? '警告' : ev.phase === 'log' ? '进行中' : '开始'
        setOp((prev) =>
          prev && prev.state === 'running'
            ? {
                ...prev,
                percent: Math.max(prev.percent, ev.percent),
                phase: `${STEP_LABEL[ev.step] ?? ev.step} · ${phaseText}`,
                last: ev.message,
              }
            : prev,
        )
        const at = new Date().toLocaleTimeString('zh-CN', { hour12: false })
        setRestoreEvents((prev) => {
          const next = [...prev, { ...ev, at }]
          return next.length > 800 ? next.slice(next.length - 800) : next
        })
      })
      setReport(result)
      const failed = result.nodes.failed.length + result.packages.failed.length
      const leftover = result.packages.leftover?.length ?? 0
      notify(
        failed + leftover > 0 ? 'err' : 'ok',
        failed + leftover > 0
          ? `恢复完成，但还有 ${failed + leftover} 项没对齐，展开「恢复详情」查看`
          : '快照已恢复',
      )
      setOp({
        state: 'ok',
        label: targetLabel,
        percent: 100,
        phase: '已完成',
        last: '',
        error: null,
      })
      // 状态变了：收起展开的行，丢掉缓存的详情/差异，重新拉列表
      setOpen(null)
      setDetails({})
      setRowDiffs({})
      await load()
      onChanged()
    } catch (e) {
      setOp({
        state: 'error',
        label: targetLabel,
        percent: 0,
        phase: '失败',
        last: '',
        error: errText(e),
      })
    }
  }

  const runRestore = async () => {
    if (confirm) await startRestore(confirm.id)
  }

  const dismissOp = () => {
    setOp(null)
    setReport(null)
    setRestoreOpen(false)
    setRestoreEvents([])
    setRestoreTarget(null)
  }

  const rollbackRestore = (id: string) => {
    setRestoreOpen(false)
    setOp(null)
    setReport(null)
    setRestoreEvents([])
    void askRestore(id)
  }

  const latestRel = items[0] ? relTime(items[0].createdAt) : null
  const confirmTarget = confirm ? (items.find((it) => it.id === confirm.id) ?? null) : null
  const opTone =
    op?.state === 'running'
      ? 'is-running'
      : op?.state === 'ok'
        ? 'is-ok'
        : op?.state === 'error'
          ? 'is-err'
          : ''
  const dotTone =
    op?.state === 'running'
      ? 'is-spin'
      : op?.state === 'ok'
        ? 'is-ok'
        : op?.state === 'error'
          ? 'is-err'
          : 'is-pending'

  return (
    <div className="stack snap-page">
      <div className="panel">
        <div className="panel-body">
        <header className="snap-page-head">
          <span className="snap-latest">
            {loading ? '读取中…' : latestRel ? `最新：${latestRel}` : '还没有快照'}
          </span>
          <div className="row snap-tools">
            <button
              className="ghost sm"
              onClick={() => void load()}
              title="刷新快照列表"
            >
              <Icon name="refresh" size={13} />
              刷新
            </button>
          </div>
        </header>

        <ul className="snap-rail">
          <li className={`snap-node is-save${op ? ` ${opTone}` : ''}`}>
            <span className={`snap-dot ${dotTone}`} aria-hidden="true" />
            <div className="snap-node-body">
              <span className="snap-save-label">
                {op?.state === 'running'
                  ? '正在恢复快照'
                  : op?.state === 'ok'
                    ? '快照已恢复'
                    : op?.state === 'error'
                      ? '恢复失败'
                      : '创建快照'}
              </span>
              <div className={`snap-save-box${op ? ` ${opTone}` : ''}`}>
                {op ? (
                  <div className="snap-op">
                    {op.state === 'running' ? (
                      <>
                        <p className="snap-op-target">正在恢复到「{op.label}」</p>
                        <div className="snap-op-barhead">
                          <span>{op.phase}</span>
                          <span>{op.percent}%</span>
                        </div>
                        <div className="snap-op-track" role="progressbar" aria-valuenow={op.percent}>
                          <div className="snap-op-fill" style={{ width: `${op.percent}%` }} />
                        </div>
                        {op.last && <div className="snap-op-last">{op.last}</div>}
                        <div className="row">
                          <button className="ghost sm" onClick={() => setRestoreOpen(true)}>
                            查看详情
                          </button>
                        </div>
                      </>
                    ) : (
                      <>
                        <p className="snap-op-target">
                          {op.state === 'ok'
                            ? `已恢复到「${op.label}」`
                            : `恢复失败：${op.error ?? '未知错误'}`}
                        </p>
                        {report && <ReportPanel report={report} />}
                        <div className="row">
                          <button className="ghost sm" onClick={() => setRestoreOpen(true)}>
                            恢复详情
                          </button>
                          {report?.preSnapshot && (
                            <button
                              className="ghost sm"
                              onClick={() => {
                                const pre = report.preSnapshot
                                setOp(null)
                                if (pre) void askRestore(pre)
                              }}
                            >
                              回滚到恢复前
                            </button>
                          )}
                          <button className="ghost sm" onClick={dismissOp}>
                            关闭
                          </button>
                        </div>
                      </>
                    )}
                  </div>
                ) : (
                  <div className="snap-save-row">
                    <input
                      className="snap-label"
                      type="text"
                      placeholder="备注（可选），比如「装 LCM 之前」"
                      value={label}
                      maxLength={40}
                      onChange={(e) => setLabel(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' && !creating) void create()
                      }}
                    />
                    <button className="snap-cta" onClick={() => void create()} disabled={creating}>
                      <Icon name="plus" size={13} />
                      {creating ? '记录中…' : '记录当前状态'}
                    </button>
                  </div>
                )}
              </div>
            </div>
          </li>

          {items.map((item, index) => {
            const isOpen = open === item.id
            const detail = details[item.id]
            const diff = rowDiffs[item.id]?.diff ?? null
            const isLatest = index === 0
            const nodeDelta = item.diff.nodesAdded + item.diff.nodesRemoved + item.diff.nodesChanged
            const pkgDelta =
              item.diff.packagesAdded + item.diff.packagesRemoved + item.diff.packagesChanged
            return (
              <li key={item.id} className={`snap-node${isOpen ? ' is-open' : ''}`}>
                <span
                  className={`snap-dot${isStateTrigger(item.trigger) ? ' is-state' : ''}`}
                  aria-hidden="true"
                />
                <div className="snap-node-body">
                  <button
                    type="button"
                    className={`snap-row-head${isOpen ? ' is-open' : ''}`}
                    aria-expanded={isOpen}
                    onClick={() => void toggle(item.id)}
                  >
                    <span className="snap-row-top">
                      <span className="snap-row-side">
                        <span
                          className={`snap-trigger${isStateTrigger(item.trigger) ? ' is-state' : ''}`}
                        >
                          {TRIGGER_LABEL[item.trigger] ?? item.trigger}
                        </span>
                        {isLatest && <span className="snap-latest-badge">最新</span>}
                      </span>
                      <span className="snap-row-side">
                        <span className="snap-time" title={absTime(item.createdAt)}>
                          {absTimeShort(item.createdAt)}
                          <span className="snap-time-rel">{relTime(item.createdAt)}</span>
                        </span>
                        <span className="snap-caret">
                          <Icon name="chevron" size={12} />
                        </span>
                      </span>
                    </span>
                    <span className="snap-row-pills">
                      <span className="snap-pill is-version" title={titlePill(item)}>
                        <Icon name="versions" size={11} />
                        <span className="snap-pill-text">{titlePill(item)}</span>
                      </span>
                      {item.diff.launchChanged && (
                        <span className="snap-pill">
                          <Icon name="power" size={11} />
                          <span className="snap-pill-label">启动参数</span>
                        </span>
                      )}
                      {nodeDelta > 0 && (
                        <span className="snap-pill">
                          <Icon name="box" size={11} />
                          {item.diff.nodesAdded > 0 && (
                            <span className="d-add">+{item.diff.nodesAdded}</span>
                          )}
                          {item.diff.nodesRemoved > 0 && (
                            <span className="d-rm">−{item.diff.nodesRemoved}</span>
                          )}
                          {item.diff.nodesChanged > 0 && (
                            <span className="d-chg">~{item.diff.nodesChanged}</span>
                          )}
                          <span className="snap-pill-label">节点</span>
                        </span>
                      )}
                      {pkgDelta > 0 && (
                        <span className="snap-pill">
                          <Icon name="chip" size={11} />
                          {item.diff.packagesAdded > 0 && (
                            <span className="d-add">+{item.diff.packagesAdded}</span>
                          )}
                          {item.diff.packagesRemoved > 0 && (
                            <span className="d-rm">−{item.diff.packagesRemoved}</span>
                          )}
                          {item.diff.packagesChanged > 0 && (
                            <span className="d-chg">~{item.diff.packagesChanged}</span>
                          )}
                          <span className="snap-pill-label">包</span>
                        </span>
                      )}
                    </span>
                  </button>

                  {isOpen && (
                    <div className="snap-card">
                      <div className="snap-meta">
                        <span>{detail?.snapshot.nodes.length ?? item.nodeCount} 个节点</span>
                        <span className="sep-dot">·</span>
                        <span>{item.packageCount} 个包</span>
                        {item.note && (
                          <>
                            <span className="sep-dot">·</span>
                            <span className="snap-note">{item.note}</span>
                          </>
                        )}
                      </div>

                      {item.label && <p className="snap-label-text">{item.label}</p>}
                      {detailBusy === item.id && <div className="snap-diff-loading">读取快照内容…</div>}

                      {index < items.length - 1 && (
                        <DiffAccordion
                          title="这条快照改了什么"
                          diff={detail?.diffPrevious ?? null}
                          busy={detailBusy === item.id}
                          onOpen={() => void ensureDetail(item.id)}
                        />
                      )}

                      {detail && detail.snapshot.nodes.length > 0 && (
                        <details className="snap-acc snap-inventory">
                          <summary>
                            <span className="snap-acc-caret">
                              <Icon name="chevron" size={11} />
                            </span>
                            快照里的节点 {detail.snapshot.nodes.length} · 包{' '}
                            {Object.keys(detail.snapshot.packages).length}
                          </summary>
                          <div className="snap-inventory-body">
                            {detail.snapshot.nodes.map((node) => (
                              <span key={node.name} className="snap-tag">
                                {node.name} {short(node.commit)}
                                {node.url ? '' : ' · 手工'}
                                {node.dirty ? ' · 未提交' : ''}
                                {node.content === 'full' && node.url ? ' · 本地归档' : ''}
                                {node.content === 'missing' ? ' · 归档缺失' : ''}
                                {node.enabled ? '' : ' · 禁用'}
                              </span>
                            ))}
                          </div>
                        </details>
                      )}

                      <DiffAccordion
                        title="恢复它会改动什么"
                        diff={diff}
                        busy={diffBusy === item.id}
                        onOpen={() => void ensureRowDiff(item.id)}
                      />

                      <div className="snap-detail-actions">
                        <button
                          className="primary sm"
                          onClick={() => void askRestore(item.id)}
                          disabled={running || busy || previewing === item.id}
                        >
                          <Icon name="refresh" size={13} />
                          {previewing === item.id ? '读取差异…' : '恢复到这一刻'}
                        </button>
                        <button
                          className={`ghost sm${arming === item.id ? ' danger' : ''}`}
                          onClick={() => void remove(item)}
                          disabled={busy}
                        >
                          <Icon name="trash" size={13} />
                          {arming === item.id ? '确认删除' : '删除'}
                        </button>
                      </div>
                    </div>
                  )}
                </div>
              </li>
            )
          })}
        </ul>
        </div>
      </div>

      {wheelGate &&
        createPortal(
          <div
            className="modal-backdrop"
            onMouseDown={(e) => {
              if (e.target === e.currentTarget && !gateBusy) setWheelGate(null)
            }}
          >
            <div
              className="modal snap-restore-modal"
              role="dialog"
              aria-modal="true"
              aria-labelledby="snap-wheels-title"
            >
              <div className="modal-head">
                <Icon name="alert" size={15} />
                <h2 id="snap-wheels-title">缺少本地 wheel</h2>
                <span className="badge warn">{wheelGate.missing.length} 个包</span>
                <div className="spacer" />
                <button
                  className="icon-btn"
                  onClick={() => setWheelGate(null)}
                  disabled={gateBusy}
                  title="关闭"
                  aria-label="关闭缺少 wheel 提示"
                >
                  <Icon name="close" size={16} />
                </button>
              </div>
              <div className="modal-body">
                <p className="hint">
                  这次恢复要装的包里有 {wheelGate.missing.length}{' '}
                  个的 wheel 在本地库里找不到文件。把对应的 .whl 放进「设置 → 本地
                  Wheel 库」后点「继续」重新检查；也可以确认跳过——这些包不会被安装，环境可能不完整。
                </p>
                {wheelGate.retried && (
                  <div className="launch-warn">重新检查后仍然缺少，补齐文件再点「继续」。</div>
                )}
                <ul className="snap-missing-wheels">
                  {wheelGate.missing.map((wheel) => (
                    <li key={`${wheel.name}-${wheel.version}`}>
                      <div className="snap-missing-name">
                        <span>{wheel.name}</span>
                        <code>{wheel.version}</code>
                        {wheel.filename && <code>{wheel.filename}</code>}
                      </div>
                      <div className="snap-missing-reason">{wheel.reason}</div>
                    </li>
                  ))}
                </ul>
              </div>
              <div className="modal-foot">
                <div className="spacer" />
                <button
                  className="ghost"
                  onClick={() => setWheelGate(null)}
                  disabled={gateBusy}
                  autoFocus
                >
                  取消
                </button>
                <button className="danger" onClick={skipWheelGate} disabled={gateBusy}>
                  跳过（不安装这些包）
                </button>
                <button className="primary" onClick={() => void retryWheelGate()} disabled={gateBusy}>
                  {gateBusy ? '检查中…' : '继续'}
                </button>
              </div>
            </div>
          </div>,
          document.body,
        )}

      {confirm &&
        createPortal(
          <div
            className="modal-backdrop"
            onMouseDown={(e) => {
              if (e.target === e.currentTarget && !busy) setConfirm(null)
            }}
          >
          <div
            className="modal snap-restore-modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="snap-restore-title"
          >
            <div className="modal-head">
              <Icon name="clock" size={15} />
              <h2 id="snap-restore-title">恢复到这条快照？</h2>
              <span className="badge warn">不可撤销</span>
              <div className="spacer" />
              <button
                className="icon-btn"
                onClick={() => setConfirm(null)}
                disabled={busy}
                title="关闭"
                aria-label="关闭恢复确认"
              >
                <Icon name="close" size={16} />
              </button>
            </div>
            <div className="modal-body">
              <div className="snap-restore-target">
                <span className="snap-restore-when">
                  {confirmTarget ? absTime(confirmTarget.createdAt) : ''}
                </span>
                <strong>{confirmTarget ? titlePill(confirmTarget) : short(confirm.id)}</strong>
              </div>

              {confirm.preview.running || running ? (
                <div className="launch-warn">
                  实例正在运行，先停止再恢复：文件被占用会装不回去，包也会装完又被覆盖。
                </div>
              ) : (
                <p className="hint">
                  恢复会切换版本、还原节点与包，并恢复启动参数。目标节点的本地改动会被覆盖；恢复前自动存一条「恢复前」快照。
                </p>
              )}

              {confirm.preview.targets.nodesContent > 0 && (
                <div className="hint">
                  会还原 {confirm.preview.targets.nodesContent} 个节点的本地内容（手工节点目录 / git
                  未提交改动），目标节点当前的本地改动会被覆盖。
                </div>
              )}

              <SnapshotDiffView diff={confirm.preview.diff} cap={12} />

              <div className="snap-opts">
                <label className="check-row">
                  <input
                    type="checkbox"
                    checked={opts.removeExtraNodes}
                    onChange={(e) =>
                      setOpts((prev) => ({ ...prev, removeExtraNodes: e.target.checked }))
                    }
                  />
                  删掉快照里没有的自定义节点（{confirm.preview.targets.nodesRemove} 个）
                </label>
                <label className="check-row">
                  <input
                    type="checkbox"
                    checked={opts.removeExtraPackages}
                    onChange={(e) =>
                      setOpts((prev) => ({ ...prev, removeExtraPackages: e.target.checked }))
                    }
                  />
                  卸掉快照里没有的包（{confirm.preview.targets.packagesRemove} 个）
                </label>
              </div>

              {confirm.preview.targets.packagesProtected.length > 0 && (
                <div className="hint">
                  torch 系与打包工具不动，只报告差异：
                  {confirm.preview.targets.packagesProtected
                    .map((p) => `${p.name} ${p.current ?? '—'}→${p.target ?? '—'}`)
                    .join('、')}
                </div>
              )}
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <button className="ghost" onClick={() => setConfirm(null)} autoFocus>
                取消
              </button>
              <button
                className="primary"
                onClick={() => void runRestore()}
                disabled={confirm.preview.running || running}
              >
                开始恢复
              </button>
            </div>
          </div>
        </div>,
          document.body,
        )}

      {restoreOpen && op && (
        <OperationDialog
          title="恢复快照"
          target={op.label}
          state={op.state === 'error' ? 'failed' : op.state}
          phase={op.phase}
          percent={op.percent}
          events={restoreEvents}
          tab={restoreTab}
          onTab={setOperationTab}
          detailLabel="恢复详情"
          stepLabel={(step) => STEP_LABEL[step] ?? step}
          onClose={() => setRestoreOpen(false)}
          footer={
            report?.preSnapshot && op.state !== 'running' ? (
              <button className="ghost" onClick={() => rollbackRestore(report.preSnapshot!)}>
                回滚到恢复前
              </button>
            ) : null
          }
        >
          {op.error && <div className="op-run-error">{op.error}</div>}
          {restoreTarget && (
            <div className="op-run-target">
              <div>
                <span>快照</span>
                <strong>{absTime(restoreTarget.createdAt)}</strong>
              </div>
              <div>
                <span>ComfyUI</span>
                <strong className="mono">{restoreTarget.comfyuiVersion}</strong>
              </div>
              <div>
                <span>节点 / Python 包</span>
                <strong className="mono">
                  {restoreTarget.nodeCount} / {restoreTarget.packageCount}
                </strong>
              </div>
            </div>
          )}
          {report ? (
            <ReportPanel report={report} open />
          ) : (
            !op.error && (
              <div className="op-run-live">
                <span className="op-run-live-mark" aria-hidden="true" />
                <span>{op.last || '正在开始…'}</span>
              </div>
            )
          )}
          {report && report.warnings.length > 0 && (
            <div className="op-run-warn">{report.warnings.join('；')}</div>
          )}
        </OperationDialog>
      )}
    </div>
  )
}
