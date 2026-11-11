import { useEffect, useMemo, useState, type CSSProperties, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import type {
  ClusterInstanceView,
  ClusterMemberView,
  SyncCompare,
  SyncDiff,
  SyncJob,
  SyncOptions,
  SyncPrepare,
  SyncScope,
} from '../types'
import Icon, { type IconName } from '../components/Icon'
import Select from '../components/Select'
import OperationDialog, {
  type OperationLine,
  type OperationState,
  type OperationTab,
} from '../components/OperationDialog'

export const SCOPE_ITEMS: Array<{ key: keyof SyncScope; label: string; icon: IconName }> = [
  { key: 'comfyui', label: 'ComfyUI', icon: 'versions' },
  { key: 'nodes', label: '节点', icon: 'nodes' },
  { key: 'python', label: 'Python', icon: 'chip' },
  { key: 'models', label: '模型', icon: 'box' },
]

export const ALL_SCOPE: SyncScope = { comfyui: true, nodes: true, python: true, models: true }

export function bytes(n: number): string {
  if (!n) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let v = n
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i += 1
  }
  return `${v >= 10 || i === 0 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`
}

export function commit(s: string | null | undefined): string {
  if (!s) return '—'
  return s.length > 10 ? s.slice(0, 10) : s
}

export function when(s: string | null | undefined): string {
  if (!s) return '—'
  const t = Date.parse(s)
  if (Number.isNaN(t)) return s
  const sec = Math.max(0, Math.round((Date.now() - t) / 1000))
  if (sec < 60) return `${sec} 秒前`
  if (sec < 3600) return `${Math.round(sec / 60)} 分钟前`
  if (sec < 86400) return `${Math.round(sec / 3600)} 小时前`
  return `${Math.round(sec / 86400)} 天前`
}

export function clock(s: string | null | undefined): string {
  if (!s) return '—'
  const d = new Date(s)
  if (Number.isNaN(d.getTime())) return s
  return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}:${d
    .getSeconds()
    .toString()
    .padStart(2, '0')}`
}

export function dayClock(s: string | null | undefined): string {
  if (!s) return '—'
  const d = new Date(s)
  if (Number.isNaN(d.getTime())) return s
  const sameDay = d.toDateString() === new Date().toDateString()
  return sameDay ? clock(s) : `${d.getMonth() + 1}/${d.getDate()} ${clock(s)}`
}

export function dateClock(s: string | null | undefined): string {
  if (!s) return '—'
  const d = new Date(s)
  if (Number.isNaN(d.getTime())) return s
  return `${d.getFullYear()}-${(d.getMonth() + 1).toString().padStart(2, '0')}-${d
    .getDate()
    .toString()
    .padStart(2, '0')} ${clock(s)}`
}

export function span(start: string | null | undefined, end: string | null | undefined): string {
  if (!start) return '—'
  const a = Date.parse(start)
  const b = end ? Date.parse(end) : Date.now()
  if (Number.isNaN(a) || Number.isNaN(b) || b < a) return '—'
  const sec = Math.round((b - a) / 1000)
  if (sec < 60) return `${sec} 秒`
  const min = Math.floor(sec / 60)
  if (min < 60) return `${min} 分 ${sec % 60} 秒`
  const hour = Math.floor(min / 60)
  return `${hour} 小时 ${min % 60} 分`
}

export function stateText(state: string): string {
  return (
    {
      running: '运行中',
      stopped: '已停止',
      starting: '启动中',
      stopping: '停止中',
      installing: '安装中',
      updating: '更新中',
      ready: '就绪',
      error: '错误',
      uninitialized: '未初始化',
    }[state] ?? state
  )
}

export function jobActive(job: SyncJob): boolean {
  return job.status === 'awaiting_confirmation' || job.status === 'pending' || job.status === 'running'
}

export function jobStart(job: SyncJob): string {
  return job.startedAt || job.createdAt
}

export function jobEnd(job: SyncJob, now: number): string | null {
  if (job.finishedAt) return job.finishedAt
  if (jobActive(job)) return new Date(now).toISOString()
  return job.updatedAt || null
}

export function jobDuration(job: SyncJob, now: number): string {
  return span(jobStart(job), jobEnd(job, now))
}

export function targetLabel(job: SyncJob): string {
  if (job.targetInstanceName) {
    return job.targetMemberName
      ? `${job.targetMemberName} · ${job.targetInstanceName}`
      : job.targetInstanceName
  }
  return job.targetMemberName ? `${job.targetMemberName} · 待选实例` : '目标客户机 · 待选实例'
}

export function jobBadge(status: string): string {
  if (status === 'success') return 'ok'
  if (status === 'partial') return 'warn'
  if (status === 'failed') return 'err'
  if (status === 'running') return 'busy'
  if (status === 'cancelled') return 'idle'
  return 'warn'
}

export function isWaitingConfirmation(status: string): boolean {
  return status === 'awaiting_confirmation'
}

export function jobText(status: string): string {
  return (
    {
      awaiting_confirmation: '等待目标接受',
      pending: '已接受 / 排队中',
      running: '执行中',
      success: '已完成',
      partial: '部分完成',
      failed: '失败',
      cancelled: '已取消',
    }[status] ?? status
  )
}

export function pyChangeCount(d: SyncDiff): number {
  return (
    d.python.packagesAdded.length +
    d.python.packagesRemoved.length +
    d.python.packagesChanged.length +
    d.python.wheelsAdded.length +
    d.python.wheelsChanged.length +
    d.python.wheelsRemoved.length
  )
}

export function ScopeBadges({ scope }: { scope: SyncScope | null | undefined }) {
  if (!scope) return null
  return (
    <span className="sync-scope-badges">
      {SCOPE_ITEMS.filter((item) => scope[item.key]).map((item) => (
        <span className="sync-scope-badge" key={item.key}>
          {item.label}
        </span>
      ))}
    </span>
  )
}

function scopeState(key: keyof SyncScope, diff: SyncDiff | null, scanning = false): string {
  if (scanning) return '扫描中'
  if (!diff) return '待扫描'
  if (key === 'comfyui') return diff.core.changed || diff.launch.changed ? '有变化' : '一致'
  if (key === 'nodes') {
    const n = diff.nodes.added.length + diff.nodes.changed.length
    return n ? `${n} 个变化` : '一致'
  }
  if (key === 'python') {
    const n = pyChangeCount(diff)
    return n ? `${n} 项变化` : '一致'
  }
  const n = diff.models.added.length + diff.models.modified.length
  return n ? `${n} 个 · ${bytes(diff.models.transferBytes)}` : '一致'
}

export function ScopeGrid({
  scope,
  diff,
  scanning,
  disabled,
  onToggle,
}: {
  scope: SyncScope
  diff: SyncDiff | null
  scanning?: boolean
  disabled?: boolean
  onToggle: (key: keyof SyncScope) => void
}) {
  return (
    <div className="sync-scope-grid" role="group" aria-label="同步内容">
      {SCOPE_ITEMS.map((item) => {
        const on = scope[item.key]
        return (
          <button
            key={item.key}
            type="button"
            className={`sync-scope ${on ? 'is-on' : ''}`}
            aria-pressed={on}
            onClick={() => onToggle(item.key)}
            disabled={disabled}
          >
            <span className="sync-scope-icon">
              <Icon name={item.icon} size={14} />
            </span>
            <span className="sync-scope-copy">
              <span className="sync-scope-label">{item.label}</span>
              <span className="sync-scope-state">{scopeState(item.key, diff, scanning)}</span>
            </span>
            <span className="sync-scope-mark" aria-hidden="true">
              {on && <Icon name="check" size={10} />}
            </span>
          </button>
        )
      })}
    </div>
  )
}

function instancesOf(member: ClusterMemberView) {
  return member.instances ?? []
}

function lastAppliedOf(member: ClusterMemberView): string | null {
  return instancesOf(member).reduce<string | null>((latest, i) => {
    const at = i.lastApplied?.appliedAt
    return at && (!latest || Date.parse(at) > Date.parse(latest)) ? at : latest
  }, null)
}

export function TargetList({
  members,
  selected,
  expanded,
  busy,
  confirmRemove,
  onToggle,
  onExpand,
  onRemove,
  onToggleAll,
}: {
  members: ClusterMemberView[]
  selected: string[]
  expanded: string[]
  busy: string
  confirmRemove: string
  onToggle: (member: ClusterMemberView) => void
  onExpand: (id: string) => void
  onRemove: (member: ClusterMemberView) => void
  onToggleAll: () => void
}) {
  const selectable = members.filter((m) => m.online && m.instances.length > 0)
  const allSelected = selectable.length > 0 && selectable.every((m) => selected.includes(m.id))

  if (!members.length) {
    return (
      <div className="sync-targets-empty">
        <span className="sync-empty-icon">
          <Icon name="globe" size={20} />
        </span>
        <strong>等待目标机加入</strong>
      </div>
    )
  }

  return (
    <>
      <div className="sync-targets-toolbar">
        <span className="sync-targets-count">
          已选 <strong>{selected.length}</strong> / {members.length}
        </span>
        <button
          className="ghost sm"
          type="button"
          onClick={onToggleAll}
          disabled={!selectable.length || !!busy}
        >
          {allSelected ? '清空' : '全选'}
        </button>
      </div>
      <div className="sync-targets">
        {members.map((member) => {
          const isSelected = selected.includes(member.id)
          const isExpanded = expanded.includes(member.id)
          const disabled = !member.online || !member.instances.length
          const lastApplied = lastAppliedOf(member)
          return (
            <article
              className={`sync-target ${isSelected ? 'is-selected' : ''} ${!member.online ? 'is-offline' : ''}`}
              key={member.id}
            >
              <div className="sync-target-row">
                <label className={`sync-target-pick ${disabled ? 'is-disabled' : ''}`}>
                  <input
                    type="checkbox"
                    checked={isSelected}
                    disabled={disabled || !!busy}
                    onChange={() => onToggle(member)}
                    aria-label={`选择 ${member.name}`}
                  />
                  <span className="sync-target-check" aria-hidden="true">
                    {isSelected && <Icon name="check" size={11} />}
                  </span>
                  <span className="sync-target-copy">
                    <span className="sync-target-name">
                      <strong>{member.name}</strong>
                      <span className={`badge ${member.online ? 'ok' : 'idle'}`}>
                        <span className="dot" />
                        {member.online ? '在线' : '离线'}
                      </span>
                    </span>
                    <span className="sync-target-meta">
                      <span>{member.hostname}</span>
                    </span>
                  </span>
                  <span className="sync-target-side">
                    <strong>{member.instances.length} 实例</strong>
                    <span>{lastApplied ? `上次 ${when(lastApplied)}` : '未同步'}</span>
                  </span>
                </label>
                <button
                  type="button"
                  className={`sync-icon-button ${isExpanded ? 'is-open' : ''}`}
                  onClick={() => onExpand(member.id)}
                  aria-label={isExpanded ? '收起目标详情' : '展开目标详情'}
                  aria-expanded={isExpanded}
                  title={isExpanded ? '收起详情' : '展开详情'}
                >
                  <Icon name="chevron" size={14} />
                </button>
              </div>
              {isExpanded && (
                <div className="sync-target-detail">
                  <div className="sync-target-facts">
                    <div>
                      <span>系统</span>
                      <strong>{member.platform}</strong>
                    </div>
                    <div>
                      <span>版本</span>
                      <strong>v{member.appVersion}</strong>
                    </div>
                    <div>
                      <span>模型写入</span>
                      <strong title={member.modelStore}>{member.modelStore || '—'}</strong>
                    </div>
                  </div>
                  {member.instances.length ? (
                    <div className="sync-instance-list">
                      {member.instances.map((inst) => (
                        <div className="sync-instance-row" key={inst.id}>
                          <div className="sync-instance-main">
                            <strong>{inst.name}</strong>
                            <span className={`sync-state ${inst.state === 'running' ? 'is-running' : ''}`}>
                              {stateText(inst.state)}
                            </span>
                          </div>
                          <div className="sync-instance-meta">
                            <span>
                              {inst.countsKnown === false ? '节点 —' : `${inst.nodeCount} 节点`}
                            </span>
                            <span>
                              {inst.countsKnown === false ? '模型 —' : `${inst.modelCount} 模型`}
                            </span>
                            <span>{inst.countsKnown === false ? '—' : bytes(inst.modelBytes)}</span>
                            {inst.diskFree > 0 && <span>余量 {bytes(inst.diskFree)}</span>}
                            <span>{inst.lastApplied ? `上次 ${when(inst.lastApplied.appliedAt)}` : '未同步'}</span>
                          </div>
                        </div>
                      ))}
                    </div>
                  ) : (
                    <div className="sync-targets-empty is-inline">
                      <strong>尚未上报实例</strong>
                    </div>
                  )}
                  <div className="sync-target-footer">
                    <span>{member.online ? `最后心跳 ${when(member.lastSeen)}` : `离线 ${when(member.lastSeen)}`}</span>
                    <button
                      className={confirmRemove === `remove:${member.id}` ? 'danger sm' : 'ghost sm'}
                      type="button"
                      onClick={() => onRemove(member)}
                      disabled={!!busy}
                    >
                      {confirmRemove === `remove:${member.id}` ? '确认移除？' : '移除成员'}
                    </button>
                  </div>
                </div>
              )}
            </article>
          )
        })}
      </div>
    </>
  )
}

function syncOperationState(status: string): OperationState {
  if (status === 'success' || status === 'partial') return 'ok'
  if (status === 'failed') return 'failed'
  if (status === 'cancelled') return 'cancelled'
  return 'running'
}

function syncLogStep(message: string): string {
  if (/^同步/.test(message)) return '同步'
  if (/模型/.test(message)) return '模型'
  if (/节点|插件/.test(message)) return '节点'
  if (/依赖|pip|uv |wheel|安装包/.test(message)) return '依赖'
  if (/Python|环境|venv|解释器/.test(message)) return '环境'
  if (/快照/.test(message)) return '快照'
  if (/ComfyUI|版本|git|分支|提交/.test(message)) return '核心'
  if (/下载/.test(message)) return '下载'
  return '同步'
}

function syncLogPhase(message: string): OperationLine['phase'] {
  if (/失败|错误|无法|拒绝|异常/.test(message)) return 'err'
  if (/警告|跳过/.test(message)) return 'warn'
  if (/完成|成功|就绪/.test(message)) return 'done'
  return 'log'
}

function parseSyncLog(log: string, fallback: string): OperationLine {
  const spaced = log.match(/^(\d{2}:\d{2}:\d{2})\s+(.*)$/)
  const legacy = log.match(/^\[([^\]]+)\]\s*(.*)$/)
  const at = spaced?.[1] ?? legacy?.[1] ?? fallback
  const message = spaced?.[2] ?? legacy?.[2] ?? log
  return { at, step: syncLogStep(message), phase: syncLogPhase(message), message }
}

function syncOperationEvents(job: SyncJob): OperationLine[] {
  const fallback = clock(jobStart(job))
  return (job.logs ?? []).slice(-1200).map((log) => parseSyncLog(log, fallback))
}

function SyncDetails({
  job,
  busy = '',
  onCancel,
}: {
  job: SyncJob
  busy?: string
  onCancel?: (job: SyncJob) => void
}) {
  const [open, setOpen] = useState(false)
  const [tab, setTab] = useState<OperationTab>('detail')
  const events = useMemo(() => syncOperationEvents(job), [job])
  const active = jobActive(job)
  const percent = Math.max(0, Math.min(100, job.progress))
  const scopeText = SCOPE_ITEMS.filter((item) => job.scope?.[item.key])
    .map((item) => item.label)
    .join(' · ')
  const warnings = job.report?.warnings ?? []
  const cancelling =
    job.cancelRequested ||
    job.targetCancelRequested ||
    busy === `cancel-${job.id}` ||
    busy === `cancel-target-${job.id}`

  return (
    <>
      <div className="sync-log">
        <button
          type="button"
          className="sync-log-toggle"
          onClick={() => {
            setTab('detail')
            setOpen(true)
          }}
          title="打开同步任务详情"
        >
          <Icon name="console" size={13} />
          <span>详情</span>
          <span className="sync-log-count">{events.length}</span>
        </button>
      </div>
      {open && (
        <OperationDialog
          title={job.status === 'awaiting_confirmation' ? '同步请求' : '同步任务'}
          target={`${job.sourceInstanceName} → ${targetLabel(job)}`}
          state={syncOperationState(job.status)}
          phase={job.message || jobText(job.status)}
          percent={percent}
          facts={[
            { label: '来源', value: job.sourceInstanceName },
            { label: '目标', value: targetLabel(job) },
            { label: '进度', value: `${percent}%`, mono: true },
            ...(job.acceptedAt
              ? [
                  { label: '接受', value: dayClock(job.acceptedAt) },
                  {
                    label: '目标清理',
                    value:
                      job.options.removeExtraNodes || job.options.removeExtraModels
                        ? [
                            job.options.removeExtraNodes ? '节点' : '',
                            job.options.removeExtraModels ? '模型' : '',
                          ]
                            .filter(Boolean)
                            .join('、')
                        : '保留多余内容',
                  },
                ]
              : []),
          ]}
          events={events}
          tab={tab}
          onTab={setTab}
          detailLabel="同步详情"
          showNet
          netLabel="局域网"
          onClose={() => setOpen(false)}
          footer={
            active && onCancel ? (
              <button
                className="ghost stop"
                type="button"
                onClick={() => onCancel(job)}
                disabled={cancelling}
                title="取消当前同步"
              >
                <Icon name="stop" size={13} />
                {cancelling ? '取消中…' : '取消同步'}
              </button>
            ) : null
          }
        >
          <div className="op-run-target">
            <div>
              <span>范围</span>
              <strong>{scopeText || '—'}</strong>
            </div>
            <div>
              <span>开始</span>
              <strong className="mono">{dayClock(jobStart(job))}</strong>
            </div>
            <div>
              <span>耗时</span>
              <strong className="mono">{jobDuration(job, Date.now())}</strong>
            </div>
          </div>
          {job.error && <div className="op-run-error">{job.error}</div>}
          {job.status === 'awaiting_confirmation' && (job.targetCancelCount ?? 0) > 0 && (
            <div className="op-run-warn">
              目标端已取消 {job.targetCancelCount} 次；源端停止前仍可再次接受。
            </div>
          )}
          {job.report && (
            <div className="op-run-target">
              <div>
                <span>节点</span>
                <strong>{job.report.nodes}</strong>
              </div>
              <div>
                <span>模型</span>
                <strong>{job.report.models}</strong>
              </div>
              <div>
                <span>快照</span>
                <strong className="mono">
                  {job.report.preSnapshot ? job.report.preSnapshot.slice(-6) : '—'}
                </strong>
              </div>
              <div>
                <span>ComfyUI</span>
                <strong className="mono">{commit(job.report.core)}</strong>
              </div>
              <div>
                <span>Python</span>
                <strong className="mono">{job.report.python || '—'}</strong>
              </div>
              <div>
                <span>警告</span>
                <strong>{warnings.length}</strong>
              </div>
            </div>
          )}
          {job.report?.partial && job.report.issues.length > 0 && (
            <div className="op-run-warn">
              目标环境未完成：
              {job.report.issues
                .map((issue) => `${issue.name}（${issue.kind === 'incompatible' ? '不兼容' : issue.kind === 'install' ? '安装失败' : '缺失'}：${issue.reason}）`)
                .join('；')}
            </div>
          )}
          {warnings.length > 0 && <div className="op-run-warn">{warnings.join('；')}</div>}
        </OperationDialog>
      )}
    </>
  )
}

function SyncEvent({
  job,
  now,
  busy,
  confirmAction,
  onCancel,
  onDelete,
  onRetry,
}: {
  job: SyncJob
  now: number
  busy: string
  confirmAction?: string
  onCancel?: (job: SyncJob) => void
  onDelete?: (job: SyncJob) => void
  onRetry?: (job: SyncJob) => void
}) {
  const active = jobActive(job)
  const percent = Math.max(0, Math.min(100, job.progress))
  const scopeText = SCOPE_ITEMS.filter((item) => job.scope?.[item.key])
    .map((item) => item.label)
    .join(' · ')
  const pausedByTarget = job.status === 'awaiting_confirmation' && !!job.targetCancelledAt
  const cancelling =
    job.cancelRequested ||
    job.targetCancelRequested ||
    busy === `cancel-${job.id}` ||
    busy === `cancel-target-${job.id}`
  return (
    <li className={`sync-event ${job.status} ${active ? 'is-active' : ''}`}>
      <span className="sync-event-dot" aria-hidden="true" />
      <div className="sync-event-body">
        <div className="sync-event-top">
          <span className={`badge ${jobBadge(job.status)}`}>
            <span className="dot" />
            {pausedByTarget ? '目标已取消 / 待再次接受' : jobText(job.status)}
          </span>
          <span className="sync-event-time">{dayClock(jobStart(job))}</span>
          <span className="sync-event-duration">{jobDuration(job, now)}</span>
          <SyncDetails job={job} busy={busy} onCancel={onCancel} />
        </div>
        <div className="sync-event-route">
          <strong>{job.sourceInstanceName}</strong>
          <span className="sync-event-arrow">→</span>
          <strong>{targetLabel(job)}</strong>
        </div>
        <div className="sync-event-meta">
          <span>{scopeText || '—'}</span>
          <span>{percent}%</span>
        </div>
        {job.error && <div className="sync-event-error">{job.error}</div>}
        {(active || percent > 0) && (
          <div className="sync-progress" aria-label={`进度 ${percent}%`}>
            <span style={{ width: `${percent}%` }} />
          </div>
        )}
        {job.message && !job.error && <div className="sync-event-message">{job.message}</div>}
        {job.report && (
          <div className="sync-event-report">
            <span>节点 {job.report.nodes}</span>
            <span>模型 {job.report.models}</span>
            {job.report.warnings.length > 0 && <span className="is-warn">警告 {job.report.warnings.length}</span>}
          </div>
        )}
        {((onCancel && active) ||
          (onDelete && !active) ||
          (onRetry && (job.status === 'failed' || job.status === 'partial'))) && (
          <div className="sync-event-actions">
            {onRetry && (job.status === 'failed' || job.status === 'partial') && (
              <button
                type="button"
                className="primary sm"
                onClick={() => onRetry(job)}
                disabled={busy === `retry-${job.id}`}
              >
                {busy === `retry-${job.id}` ? '重试中…' : '重试'}
              </button>
            )}
            {onCancel && active && (
              <button
                type="button"
                className="ghost sm"
                onClick={() => onCancel(job)}
                disabled={cancelling}
              >
                {cancelling ? '取消中…' : '取消'}
              </button>
            )}
            {onDelete && !active && (
              <button
                type="button"
                className={confirmAction === `delete:${job.id}` ? 'danger sm' : 'ghost sm'}
                onClick={() => onDelete(job)}
                disabled={busy === `delete-${job.id}`}
              >
                {busy === `delete-${job.id}`
                  ? '删除中…'
                  : confirmAction === `delete:${job.id}`
                    ? '确认删除？'
                    : '删除'}
              </button>
            )}
          </div>
        )}
      </div>
    </li>
  )
}

export function Timeline({
  jobs,
  now,
  busy,
  maxHeight,
  confirmAction,
  onCancel,
  onDelete,
  onRetry,
}: {
  jobs: SyncJob[]
  now: number
  busy: string
  maxHeight?: number
  confirmAction?: string
  onCancel?: (job: SyncJob) => void
  onDelete?: (job: SyncJob) => void
  onRetry?: (job: SyncJob) => void
}) {
  const sorted = useMemo(
    () => jobs.slice().sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt)),
    [jobs],
  )
  const active = sorted.filter(jobActive).length
  return (
    <aside
      className="sync-timeline"
      style={{ '--sync-timeline-max': maxHeight ? `${maxHeight}px` : '0px' } as CSSProperties}
    >
      <header className="sync-timeline-head">
        <h2>任务</h2>
        <span className="badge ghost">{sorted.length}</span>
        {active > 0 && <span className="sync-timeline-active">{active} 进行中</span>}
      </header>
      <div className="sync-timeline-scroll">
        {sorted.length ? (
          <ol className="sync-events">
            {sorted.map((job) => (
              <SyncEvent
                key={job.id}
                job={job}
                now={now}
                busy={busy}
                confirmAction={confirmAction}
                onCancel={onCancel}
                onDelete={onDelete}
                onRetry={onRetry}
              />
            ))}
          </ol>
        ) : (
          <div className="sync-empty">
            <span className="sync-empty-icon">
              <Icon name="clock" size={20} />
            </span>
            <strong>还没有任务</strong>
          </div>
        )}
      </div>
    </aside>
  )
}

export function PreparePanel({
  prep,
  sourceName,
  onCancel,
  onClose,
}: {
  prep: SyncPrepare
  sourceName: string
  onCancel: () => void
  onClose: () => void
}) {
  const [tab, setTab] = useState<OperationTab>('detail')
  const percent = Math.max(0, Math.min(100, prep.percent))
  const state: OperationState = prep.cancelled
    ? 'cancelled'
    : prep.error
      ? 'failed'
      : prep.done
        ? 'ok'
        : 'running'
  const title = prep.cancelled
    ? '同步准备已取消'
    : prep.error
      ? '同步准备失败'
      : prep.done
        ? '同步准备完成'
        : '准备同步'
  const events = useMemo(
    () => (prep.logs ?? []).slice(-1200).map((log) => parseSyncLog(log, clock(prep.startedAt))),
    [prep.logs, prep.startedAt],
  )
  const phase = prep.message || prep.phase || (prep.done ? '准备完成' : '准备中')

  return (
    <OperationDialog
      title={title}
      target={sourceName}
      state={state}
      phase={phase}
      percent={percent}
      facts={[
        { label: '源实例', value: sourceName },
        { label: '文件', value: prep.files > 0 ? `${prep.files} 个` : '—', mono: true },
        { label: '传输', value: bytes(prep.bytes), mono: true },
      ]}
      events={events}
      tab={tab}
      onTab={setTab}
      detailLabel="准备详情"
      closeLabel={prep.done ? '关闭' : '准备中…'}
      allowHideOnRun={false}
      onClose={onClose}
      footer={
        !prep.done ? (
          <button
            className="ghost stop"
            type="button"
            onClick={onCancel}
            disabled={prep.cancelling}
            title="停止准备"
          >
            <Icon name="stop" size={13} />
            {prep.cancelling ? '停止中…' : '停止准备'}
          </button>
        ) : null
      }
    >
      <div className="op-run-target">
        <div>
          <span>开始</span>
          <strong className="mono">{dayClock(prep.startedAt)}</strong>
        </div>
        <div>
          <span>耗时</span>
          <strong className="mono">{span(prep.startedAt, prep.done ? prep.updatedAt : null)}</strong>
        </div>
        <div>
          <span>任务</span>
          <strong className="mono">{prep.jobs}</strong>
        </div>
      </div>
      {prep.message && events.length > 0 && (
        <div className="op-run-live">
          <span className="op-run-live-mark" aria-hidden="true" />
          <span>{prep.message}</span>
        </div>
      )}
      {prep.detail && (
        <div className="sync-prepare-detail" title={prep.detail}>
          {prep.detail}
        </div>
      )}
      {prep.error && <div className="op-run-error">{prep.error}</div>}
    </OperationDialog>
  )
}

export function SyncDialog({
  title,
  onClose,
  children,
  wide,
  narrow,
}: {
  title: string
  onClose: () => void
  children: ReactNode
  wide?: boolean
  narrow?: boolean
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  return createPortal(
    <div className="sync-modal-backdrop" onMouseDown={onClose}>
      <section
        className={`sync-modal ${wide ? 'is-wide' : ''} ${narrow ? 'is-narrow' : ''}`}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <header className="sync-modal-head">
          <h2>{title}</h2>
          <button className="sync-icon-button" onClick={onClose} aria-label="关闭">
            <Icon name="close" size={16} />
          </button>
        </header>
        <div className="sync-modal-body">{children}</div>
      </section>
    </div>,
    document.body,
  )
}

export function DiffSummary({ diff }: { diff: SyncDiff }) {
  const py = pyChangeCount(diff)
  const nodes = diff.nodes.added.length + diff.nodes.changed.length
  const models = diff.models.added.length + diff.models.modified.length
  return (
    <div className="sync-diff-summary">
      <div>
        <span>本体</span>
        <strong>{diff.core.changed || diff.launch.changed ? '有变化' : '一致'}</strong>
      </div>
      <div>
        <span>节点</span>
        <strong>{nodes ? `${nodes} 个` : '一致'}</strong>
      </div>
      <div>
        <span>Python</span>
        <strong>{py ? `${py} 项` : '一致'}</strong>
      </div>
      <div>
        <span>模型</span>
        <strong>{models ? `${models} 个` : '一致'}</strong>
      </div>
      <div>
        <span>传输</span>
        <strong>{bytes(diff.models.transferBytes)}</strong>
      </div>
    </div>
  )
}

function DiffList({
  title,
  changed,
  children,
}: {
  title: string
  changed?: boolean
  children: ReactNode
}) {
  return (
    <section className={`sync-diff-group ${changed ? 'is-changed' : ''}`}>
      <header>
        <h3>{title}</h3>
        <span className={`sync-diff-mark ${changed ? 'is-changed' : ''}`}>
          {changed ? '有变化' : '一致'}
        </span>
      </header>
      {children}
    </section>
  )
}

function DiffStats({ items }: { items: Array<{ label: string; value: number }> }) {
  return (
    <div className="sync-diff-stats">
      {items.map((item) => (
        <span className={item.value ? 'is-hot' : ''} key={item.label}>
          {item.label}
          <strong>{item.value}</strong>
        </span>
      ))}
    </div>
  )
}

export function DiffDetails({ diff }: { diff: SyncDiff }) {
  const pyItems: { name: string; from?: string; to?: string; kind: string }[] = [
    ...diff.python.packagesAdded.map((p) => ({ ...p, kind: '新增' })),
    ...diff.python.packagesRemoved.map((p) => ({ ...p, kind: '删除' })),
    ...diff.python.packagesChanged.map((p) => ({ ...p, kind: '变更' })),
    ...diff.python.wheelsAdded.map((p) => ({ name: p.name, to: p.to ?? undefined, kind: 'wheel 新增' })),
    ...diff.python.wheelsRemoved.map((p) => ({ name: p.name, from: p.from ?? undefined, kind: 'wheel 删除' })),
    ...diff.python.wheelsChanged.map((p) => ({
      name: p.name,
      from: p.from ?? undefined,
      to: p.to ?? undefined,
      kind: 'wheel 变更',
    })),
  ]
  return (
    <div className="sync-diff-grid">
      <DiffList title="ComfyUI / 启动参数" changed={diff.core.changed || diff.launch.changed}>
        <div className="sync-kv">
          <span>版本</span>
          <strong>
            {diff.core.fromVersion ?? '—'} → {diff.core.toVersion ?? '—'}
          </strong>
          <span>commit</span>
          <strong>
            {commit(diff.core.fromCommit)} → {commit(diff.core.toCommit)}
          </strong>
          <span>监听</span>
          <strong>
            {diff.launch.fromListen}:{diff.launch.fromPort} → {diff.launch.toListen}:{diff.launch.toPort}
          </strong>
        </div>
        {(diff.core.sourceDirty || diff.launch.toLocalExtraArgs.length > 0) && (
          <div className="sync-diff-flags">
            {diff.core.sourceDirty && <span>工作区有修改</span>}
            {diff.launch.toLocalExtraArgs.length > 0 && (
              <span>本机参数 {diff.launch.toLocalExtraArgs.length} 项不下发</span>
            )}
          </div>
        )}
        {diff.launch.toExtraArgs.length > 0 && (
          <ul className="sync-diff-list">
            {diff.launch.toExtraArgs.slice(0, 8).map((arg, i) => (
              <li key={`${i}-${arg}`}>
                <code>{arg}</code>
              </li>
            ))}
          </ul>
        )}
      </DiffList>
      <DiffList
        title="Python"
        changed={pyItems.length > 0 || diff.python.fromVersion !== diff.python.toVersion}
      >
        <div className="sync-kv">
          <span>版本</span>
          <strong>
            {diff.python.fromVersion || '—'} → {diff.python.toVersion || '—'}
          </strong>
          <span>torch</span>
          <strong>
            {diff.python.fromTorch || '—'} → {diff.python.toTorch || '—'}
          </strong>
        </div>
        {pyItems.length > 0 && (
          <ul className="sync-diff-list">
            {pyItems.slice(0, 16).map((p, i) => (
              <li key={`${p.kind}-${p.name}-${i}`}>
                <code>{p.name}</code>
                <span>{p.kind}</span>
                <span>{p.from && p.to ? `${p.from} → ${p.to}` : p.to || p.from || ''}</span>
              </li>
            ))}
          </ul>
        )}
      </DiffList>
      <DiffList
        title="节点"
        changed={diff.nodes.added.length + diff.nodes.changed.length + diff.nodes.removed.length > 0}
      >
        <DiffStats
          items={[
            { label: '新增', value: diff.nodes.added.length },
            { label: '更新', value: diff.nodes.changed.length },
            { label: '删除', value: diff.nodes.removed.length },
          ]}
        />
        {diff.nodes.added.length + diff.nodes.changed.length + diff.nodes.removed.length > 0 && (
          <ul className="sync-diff-list">
            {diff.nodes.added.slice(0, 8).map((n) => (
              <li key={`na-${n.name}`}>
                <code>{n.name}</code>
                <span>新增</span>
              </li>
            ))}
            {diff.nodes.changed.slice(0, 8).map((n) => (
              <li key={`nc-${n.name}`}>
                <code>{n.name}</code>
                <span>{n.reason}</span>
              </li>
            ))}
            {diff.nodes.removed.slice(0, 8).map((n) => (
              <li key={`nr-${n.name}`}>
                <code>{n.name}</code>
                <span>删除</span>
              </li>
            ))}
          </ul>
        )}
      </DiffList>
      <DiffList
        title="共享模型"
        changed={diff.models.added.length + diff.models.modified.length + diff.models.removed.length > 0}
      >
        <DiffStats
          items={[
            { label: '新增', value: diff.models.added.length },
            { label: '覆盖', value: diff.models.modified.length },
            { label: '删除', value: diff.models.removed.length },
          ]}
        />
        <div className="sync-diff-transfer">传输 {bytes(diff.models.transferBytes)}</div>
        {diff.models.added.length + diff.models.modified.length + diff.models.removed.length > 0 && (
          <ul className="sync-diff-list">
            {diff.models.added.slice(0, 8).map((m) => (
              <li key={`ma-${m.kind}-${m.relativePath}`}>
                <code>
                  {m.kind}/{m.relativePath}
                </code>
                <span>{bytes(m.size)}</span>
              </li>
            ))}
            {diff.models.modified.slice(0, 8).map((m) => (
              <li key={`mm-${m.kind}-${m.relativePath}`}>
                <code>
                  {m.kind}/{m.relativePath}
                </code>
                <span>
                  {bytes(m.fromSize)} → {bytes(m.size)}
                </span>
              </li>
            ))}
            {diff.models.removed.slice(0, 8).map((m) => (
              <li key={`mr-${m.kind}-${m.relativePath}`}>
                <code>
                  {m.kind}/{m.relativePath}
                </code>
                <span>删除</span>
              </li>
            ))}
          </ul>
        )}
      </DiffList>
      {diff.warnings.length > 0 && (
        <div className="sync-diff-warnings">
          {diff.warnings.map((warning, i) => (
            <p key={`${i}-${warning}`}>
              <Icon name="alert" size={13} />
              {warning}
            </p>
          ))}
        </div>
      )}
    </div>
  )
}

export function ComparePanel({
  sources,
  targets,
  sourceId,
  targetId,
  busy,
  comparison,
  onChangeSource,
  onChangeTarget,
  onCompare,
}: {
  sources: ClusterInstanceView[]
  targets: ClusterInstanceView[]
  sourceId: string
  targetId: string
  busy: string
  comparison: SyncCompare | null
  onChangeSource: (id: string) => void
  onChangeTarget: (id: string) => void
  onCompare: () => void
}) {
  return (
    <div className="sync-compare">
      <div className="sync-compare-controls">
        <label className="sync-field">
          <span>源端实例</span>
          <Select
            value={sourceId}
            options={sources.map((i) => ({ value: i.id, label: i.name, meta: stateText(i.state) }))}
            onChange={onChangeSource}
            ariaLabel="选择源端实例"
            placeholder="选择源端实例"
          />
        </label>
        <label className="sync-field">
          <span>本机实例</span>
          <Select
            value={targetId}
            options={targets.map((i) => ({ value: i.id, label: i.name, meta: stateText(i.state) }))}
            onChange={onChangeTarget}
            ariaLabel="选择本机实例"
            placeholder="选择本机实例"
          />
        </label>
        <button
          className="primary"
          disabled={!sourceId || !targetId || busy === 'compare'}
          onClick={onCompare}
        >
          <Icon name="refresh" size={14} />
          {busy === 'compare' ? '比较中…' : '比较'}
        </button>
      </div>
      {comparison ? (
        <div className="sync-compare-result">
          <div className="sync-compare-head">
            <strong>{comparison.sourceInstanceName}</strong>
            <span>→</span>
            <strong>{comparison.targetInstanceName}</strong>
            <span className={`badge ${comparison.diff.needsApply ? 'warn' : 'ok'}`}>
              {comparison.diff.needsApply ? '仍有差异' : '已一致'}
            </span>
            {comparison.sourceRunning && <span className="badge ghost">源实例运行中</span>}
          </div>
          <DiffSummary diff={comparison.diff} />
          <DiffDetails diff={comparison.diff} />
        </div>
      ) : (
        <div className="sync-empty is-compact">
          <strong>选择两个实例后比较</strong>
        </div>
      )}
    </div>
  )
}

export function PendingList({
  jobs,
  instances,
  memberName,
  confirmTargets,
  targetOptions,
  defaults,
  busy,
  confirmAction,
  onPick,
  onCleanup,
  onConfirm,
  onReject,
}: {
  jobs: SyncJob[]
  instances: ClusterInstanceView[]
  memberName: string
  confirmTargets: Record<string, string>
  targetOptions: Record<
    string,
    Pick<SyncOptions, 'removeExtraNodes' | 'removeExtraModels' | 'allowDirtyTarget'>
  >
  defaults: Pick<SyncOptions, 'removeExtraNodes' | 'removeExtraModels' | 'allowDirtyTarget'>
  busy: string
  confirmAction: string
  onPick: (jobId: string, targetId: string) => void
  onCleanup: (
    jobId: string,
    patch: Pick<SyncOptions, 'removeExtraNodes' | 'removeExtraModels' | 'allowDirtyTarget'>,
  ) => void
  onConfirm: (job: SyncJob) => void
  onReject: (job: SyncJob) => void
}) {
  const syncable = instances.filter(
    (i) => i.state === 'ready' || i.state === 'stopped' || i.state === 'running',
  )
  return (
    <section className="sync-section sync-pending">
      <header className="sync-section-head">
        <h2>等待接受</h2>
        <span className="badge warn">{jobs.length}</span>
      </header>
      <div className="sync-pending-list">
        {jobs.map((job) => {
          const targetId = confirmTargets[job.id] ?? ''
          const target = instances.find((inst) => inst.id === targetId)
          const blocked =
            !!target &&
            target.state !== 'ready' &&
            target.state !== 'stopped' &&
            target.state !== 'running'
          const cleanup = targetOptions[job.id] ?? defaults
          const jobScope = job.scope ?? ALL_SCOPE
          return (
            <article className={`sync-confirm ${blocked ? 'is-blocked' : ''}`} key={job.id}>
              <div className="sync-confirm-head">
                <div>
                  <strong>{job.sourceInstanceName}</strong>
                  <span className="sync-confirm-arrow">→</span>
                  <span>{memberName}</span>
                </div>
                <span className="sync-confirm-time">{when(job.createdAt)}</span>
              </div>
              <ScopeBadges scope={jobScope} />
              <label className="sync-field">
                <span>接收实例</span>
                <Select
                  value={targetId}
                  options={syncable.map((i) => ({
                    value: i.id,
                    label: i.name,
                    meta: stateText(i.state),
                  }))}
                  onChange={(value) => onPick(job.id, value)}
                  ariaLabel="选择本机接收实例"
                  placeholder="选择本机实例"
                />
              </label>
              {blocked && (
                <div className="sync-error-text">该实例当前不能接收同步</div>
              )}
              {target?.state === 'running' && (
                <div className="sync-note">
                  该实例正在运行；接受后会先停止它，再开始同步。
                </div>
              )}
              {(job.targetCancelCount ?? 0) > 0 && (
                <div className="sync-note">
                  你上次取消了这次同步；源端没有停止任务时可以重新接受。
                </div>
              )}
              {job.diff.targetDrift && (
                <div className="sync-alert is-inline">
                  <Icon name="alert" size={14} />
                  <span>目标实例有本地改动，确认后按源端覆盖</span>
                </div>
              )}
              <label className="sync-check">
                <input
                  type="checkbox"
                  checked={!!cleanup.allowDirtyTarget}
                  onChange={(e) =>
                    onCleanup(job.id, { ...cleanup, allowDirtyTarget: e.target.checked })
                  }
                />
                <span>允许覆盖目标实例自上次同步后的本地改动</span>
              </label>
              {(jobScope.nodes || jobScope.models) && (
                <div className="sync-confirm-cleanup">
                  {jobScope.nodes && (
                    <label className="sync-check">
                      <input
                        type="checkbox"
                        checked={cleanup.removeExtraNodes}
                        onChange={(e) => onCleanup(job.id, { ...cleanup, removeExtraNodes: e.target.checked })}
                      />
                      <span>删除源端没有的节点</span>
                    </label>
                  )}
                  {jobScope.models && (
                    <label className="sync-check">
                      <input
                        type="checkbox"
                        checked={cleanup.removeExtraModels}
                        onChange={(e) =>
                          onCleanup(job.id, { ...cleanup, removeExtraModels: e.target.checked })
                        }
                      />
                      <span>删除源端没有的模型</span>
                    </label>
                  )}
                </div>
              )}
              <div className="sync-confirm-actions">
                <button
                  className="primary"
                  disabled={!!busy || !targetId || blocked}
                  onClick={() => onConfirm(job)}
                >
                  <Icon name="check" size={14} />
                  {busy === `confirm-${job.id}` ? '接受中…' : '接受并开始'}
                </button>
                <button
                  className={confirmAction === `reject:${job.id}` ? 'danger' : 'ghost'}
                  disabled={!!busy}
                  onClick={() => onReject(job)}
                >
                  {confirmAction === `reject:${job.id}` ? '确认拒绝？' : '拒绝'}
                </button>
              </div>
              <SyncDetails job={job} />
            </article>
          )
        })}
      </div>
    </section>
  )
}

export function LocalInstances({ instances }: { instances: ClusterInstanceView[] }) {
  return (
    <section className="sync-section">
      <header className="sync-section-head">
        <h2>本机实例</h2>
        <span className="badge ghost">{instances.length}</span>
      </header>
      {instances.length ? (
        <div className="sync-local-list">
          {instances.map((inst) => (
            <div className="sync-local-row" key={inst.id}>
              <div className="sync-local-main">
                <strong>{inst.name}</strong>
                <span className={`sync-state ${inst.state === 'running' ? 'is-running' : ''}`}>
                  {stateText(inst.state)}
                </span>
              </div>
              <div className="sync-local-meta">
                <span>{inst.countsKnown === false ? '节点 —' : `${inst.nodeCount} 节点`}</span>
                <span>{inst.countsKnown === false ? '模型 —' : `${inst.modelCount} 模型`}</span>
                <span>{inst.countsKnown === false ? '—' : bytes(inst.modelBytes)}</span>
                {inst.diskFree > 0 && <span>余量 {bytes(inst.diskFree)}</span>}
                <span>{inst.lastApplied ? `上次 ${when(inst.lastApplied.appliedAt)}` : '未同步'}</span>
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="sync-empty is-compact">
          <strong>本机还没有实例</strong>
        </div>
      )}
    </section>
  )
}
