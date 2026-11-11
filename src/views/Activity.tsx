import { useCallback, useEffect, useMemo, useState } from 'react'
import { api, errText } from '../lib/api'
import type { ActivityEntry, InstanceConfig } from '../types'
import Icon from '../components/Icon'

/**
 * 操作日志页：和「快照」同一条时间线轨道，最新在最上面。
 *
 * 每行是「什么操作 + 一句话结论 + 时间」，点开看细节（改前改后、命令输出、失败原因）。
 * 记录由后端在所有改实例的命令里写，这一页只负责读和展示。
 */
interface Props {
  inst: InstanceConfig
}

const KIND_LABEL: Record<string, string> = {
  start: '启动',
  stop: '停止',
  'auto-restart': '重启',
  'node-install': '节点',
  'node-update': '节点',
  'node-remove': '节点',
  'node-toggle': '节点',
  'node-deps': '依赖',
  'node-deps-mode': '依赖',
  'version-switch': '版本',
  'version-update': '版本',
  'launch-args': '参数',
  storage: '存储',
  'instance-settings': '设置',
  relocate: '重定位',
  'env-install': '环境',
  'snapshot-create': '快照',
  'snapshot-restore': '快照',
  'snapshot-delete': '快照',
  import: '导入',
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

/** 和快照页同一套相对时间口径 */
function relTime(iso: string): string {
  const t = new Date(iso).getTime()
  if (Number.isNaN(t)) return iso
  const diff = Date.now() - t
  if (diff < 45_000) return '刚刚'
  const minutes = Math.round(diff / 60_000)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.round(diff / 3_600_000)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.round(diff / 86_400_000)
  if (days < 30) return `${days} 天前`
  return absTime(iso).slice(0, 10)
}

function dotClass(level: ActivityEntry['level']): string {
  switch (level) {
    case 'ok':
      return ' is-ok'
    case 'warn':
      return ' is-warn'
    case 'error':
      return ' is-err'
    default:
      return ''
  }
}

export default function Activity({ inst }: Props) {
  const [items, setItems] = useState<ActivityEntry[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [open, setOpen] = useState<string | null>(null)

  const load = useCallback(async () => {
    setBusy(true)
    setError(null)
    try {
      setItems(await api.activityList(inst.id))
    } catch (e) {
      setError(errText(e))
      setItems([])
    } finally {
      setBusy(false)
    }
  }, [inst.id])

  useEffect(() => {
    void load()
  }, [load])

  const counts = useMemo(() => {
    const by = new Map<string, number>()
    for (const item of items ?? []) {
      const label = KIND_LABEL[item.kind] ?? item.kind
      by.set(label, (by.get(label) ?? 0) + 1)
    }
    return [...by.entries()].sort((a, b) => b[1] - a[1])
  }, [items])

  const toggle = (id: string) => setOpen((prev) => (prev === id ? null : id))

  return (
    <div className="stack snap-page">
      <div className="panel">
        <div className="panel-head">
          <h2>操作日志</h2>
          <span className="badge ghost">{items ? `${items.length} 条` : '读取中'}</span>
          {counts.length > 0 && (
            <span className="act-counts" aria-hidden="true">
              {counts.slice(0, 4).map(([label, n]) => (
                <span key={label} className="act-count">
                  {label} {n}
                </span>
              ))}
            </span>
          )}
          <div className="spacer" />
          <button
            className="ghost sm"
            onClick={() => void load()}
            disabled={busy}
            title="重新读取日志"
          >
            <Icon name="refresh" size={13} />
            {busy ? '刷新中…' : '刷新'}
          </button>
        </div>
        <div className="panel-body">
          {error && <div className="banner err">{error}</div>}
          {items === null && <div className="empty">读取日志…</div>}
          {items !== null && items.length === 0 && !error && (
            <div className="empty">还没有记录。操作之后这里会留下时间线。</div>
          )}
          {items !== null && items.length > 0 && (
            <ul className="snap-rail act-rail">
              {items.map((item) => {
                const isOpen = open === item.id
                return (
                  <li key={item.id} className={`snap-node${isOpen ? ' is-open' : ''}`}>
                    <span className={`snap-dot${dotClass(item.level)}`} aria-hidden="true" />
                    <div className="snap-node-body">
                      <button
                        type="button"
                        className={`snap-row-head${isOpen ? ' is-open' : ''}`}
                        aria-expanded={isOpen}
                        onClick={() => toggle(item.id)}
                      >
                        <span className="snap-row-top">
                          <span className="snap-row-side">
                            <span className="act-kind">{KIND_LABEL[item.kind] ?? item.kind}</span>
                            <span className="act-title" title={item.action}>
                              {item.action}
                            </span>
                          </span>
                          <span className="snap-row-side">
                            <span className="snap-time" title={absTime(item.at)}>
                              {relTime(item.at)}
                            </span>
                            <span className="snap-caret">
                              <Icon name="chevron" size={12} />
                            </span>
                          </span>
                        </span>
                      </button>

                      {isOpen && (
                        <div className="snap-card">
                          <div className="snap-meta">
                            <span className="mono">{absTime(item.at)}</span>
                            <span className="sep-dot">·</span>
                            <span>{KIND_LABEL[item.kind] ?? item.kind}</span>
                            {item.level === 'error' && <span className="badge err">失败</span>}
                            {item.level === 'warn' && <span className="badge warn">注意</span>}
                          </div>
                          {item.detail && <pre className="act-detail">{item.detail}</pre>}
                        </div>
                      )}
                    </div>
                  </li>
                )
              })}
            </ul>
          )}
        </div>
      </div>
    </div>
  )
}
