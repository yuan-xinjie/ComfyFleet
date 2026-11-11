import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { api, errText } from '../lib/api'
import type {
  InstanceConfig,
  ModelCopyResult,
  ModelListing,
  StorageDir,
  StoragePlan,
} from '../types'
import Icon from '../components/Icon'
import Select from '../components/Select'

const SOURCE_LABEL: Record<string, string> = {
  own: '实例',
  extra: '附加',
  shared: '共享',
}

function baseName(path: string): string {
  const parts = path.trim().replace(/[\\/]+$/, '').split(/[\\/]/)
  return parts[parts.length - 1] || path
}

function rootLabel(dir: StorageDir): string {
  const kind = SOURCE_LABEL[dir.source] ?? '模型'
  const base = baseName(dir.path)
  const type = dir.type?.trim()
  if (type && type.toLowerCase() !== base.toLowerCase()) return `${kind} · ${type}`
  return `${kind} · ${base}`
}

function pickDefault(models: StorageDir[]): string | null {
  const shared = models.filter((m) => m.source === 'shared')
  return (
    shared.find((m) => m.isDefault)?.path ??
    shared[0]?.path ??
    models.find((m) => m.exists)?.path ??
    models[0]?.path ??
    null
  )
}

function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let value = n
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`
}

function fmtDate(ms: number | null): string {
  if (!ms) return ''
  const d = new Date(ms)
  if (Number.isNaN(d.getTime())) return ''
  const pad = (v: number) => String(v).padStart(2, '0')
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

interface Props {
  inst: InstanceConfig
  /** 概览页在前台时才拉目录；切走再回来会刷新 */
  active: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
  onOpenStorage: () => void
}

/**
 * 模型资源管理器：选择这个实例生效的某个模型目录，像文件管理器一样进出子目录。
 * 从系统里拖文件/文件夹到卡片上，直接复制进当前所在目录。
 */
export default function ModelExplorer({ inst, active, notify, onOpenStorage }: Props) {
  const [plan, setPlan] = useState<StoragePlan | null>(null)
  const [root, setRoot] = useState<string | null>(null)
  const [segments, setSegments] = useState<string[]>([])
  const [listing, setListing] = useState<ModelListing | null>(null)
  const [loadingPlan, setLoadingPlan] = useState(true)
  const [loadingDir, setLoadingDir] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [copyError, setCopyError] = useState<string | null>(null)
  const [reloadTick, setReloadTick] = useState(0)
  const [copying, setCopying] = useState(false)
  const [dropOver, setDropOver] = useState(false)
  const cardRef = useRef<HTMLElement>(null)
  const requestRef = useRef(0)
  const stateRef = useRef({ root, segments, active, copying })
  stateRef.current = { root, segments, active, copying }
  const copyRef = useRef<(paths: string[]) => void>(() => {})

  const storageKey = useMemo(() => JSON.stringify(inst.storage), [inst.storage])

  const loadDir = useCallback(
    async (selectedRoot: string, segs: string[]) => {
      const token = ++requestRef.current
      setLoadingDir(true)
      try {
        const next = await api.modelsList(inst.id, selectedRoot, segs.join('/'))
        if (token !== requestRef.current) return
        setListing(next)
        setError(null)
      } catch (e) {
        if (token !== requestRef.current) return
        setListing(null)
        setError(errText(e))
      } finally {
        if (token === requestRef.current) setLoadingDir(false)
      }
    },
    [inst.id],
  )

  // 目录列表和全局共享目录都可能被别处改掉：概览重新出现、实例配置变化时重拉一次。
  useEffect(() => {
    if (!active) return
    let cancelled = false
    setLoadingPlan(true)
    api
      .instanceStoragePlan(inst.id)
      .then((next) => {
        if (cancelled) return
        setPlan(next)
        const previous = stateRef.current.root
        const keep = previous && next.models.some((m) => m.path === previous)
        const selected = keep ? previous : pickDefault(next.models)
        if (selected !== previous) setSegments([])
        setRoot(selected)
        setError(null)
        setCopyError(null)
        setLoadingPlan(false)
      })
      .catch((e) => {
        if (cancelled) return
        setPlan(null)
        setListing(null)
        setError(errText(e))
        setLoadingPlan(false)
      })
    return () => {
      cancelled = true
    }
  }, [active, inst.id, storageKey, reloadTick])

  useEffect(() => {
    if (!active || !root) return
    void loadDir(root, segments)
  }, [active, root, segments, plan, loadDir])

  const refresh = () => {
    setReloadTick((v) => v + 1)
  }

  const goTo = (next: string[]) => {
    setCopyError(null)
    setSegments(next)
  }

  const copyDropped = useCallback(
    async (paths: string[]) => {
      const { root: currentRoot, segments: currentSegments, copying: busy } = stateRef.current
      if (!currentRoot || busy || paths.length === 0) return
      setCopying(true)
      setDropOver(false)
      setCopyError(null)
      try {
        const result: ModelCopyResult = await api.modelsCopy(
          inst.id,
          currentRoot,
          currentSegments.join('/'),
          paths,
        )
        if (result.failed > 0) {
          setCopyError(result.errors[0] ?? `${result.failed} 个文件复制失败`)
          notify(
            'err',
            result.copied > 0
              ? `${result.copied} 个已复制，${result.failed} 个失败`
              : `${result.failed} 个文件复制失败`,
          )
        } else if (result.copied === 0 && result.skipped > 0) {
          notify('ok', `${result.skipped} 个同名文件已存在`)
        } else if (result.skipped > 0) {
          notify('ok', `已复制 ${result.copied} 个文件，跳过 ${result.skipped} 个已存在`)
        } else {
          notify('ok', `已复制 ${result.copied} 个文件 · ${fmtBytes(result.bytes)}`)
        }
        await loadDir(currentRoot, currentSegments)
      } catch (e) {
        const text = errText(e)
        setCopyError(text)
        notify('err', text)
      } finally {
        setCopying(false)
      }
    },
    [inst.id, loadDir, notify],
  )
  copyRef.current = (paths) => {
    void copyDropped(paths)
  }

  // 系统文件拖进来时，只有落点在模型卡片内才接管；Tauri 的坐标是物理像素。
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let disposed = false
    const overCard = (x: number, y: number): boolean => {
      const el = cardRef.current
      if (!el || !stateRef.current.active) return false
      const r = el.getBoundingClientRect()
      if (x < r.left || x > r.right || y < r.top || y > r.bottom) return false
      const hit = document.elementFromPoint(x, y)
      return !!hit && el.contains(hit)
    }
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload
        if (payload.type === 'leave') {
          setDropOver(false)
          return
        }
        const scale = window.devicePixelRatio || 1
        const x = payload.position.x / scale
        const y = payload.position.y / scale
        const inside = overCard(x, y)
        if (payload.type === 'enter' || payload.type === 'over') {
          setDropOver(inside && !stateRef.current.copying)
          return
        }
        if (payload.type === 'drop') {
          setDropOver(false)
          if (!inside || stateRef.current.copying) return
          copyRef.current(payload.paths)
        }
      })
      .then((fn) => {
        if (disposed) fn()
        else unlisten = fn
      })
      .catch(() => {
        // 没有拖放权限时不影响目录浏览
      })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])

  const roots = plan?.models ?? []
  const selected = roots.find((m) => m.path === root) ?? null
  const rootOptions = roots.map((m) => {
    const label = rootLabel(m)
    const duplicate = roots.filter((other) => rootLabel(other) === label).length > 1
    return {
      value: m.path,
      label,
      meta: duplicate ? m.path : undefined,
      badge: m.isDefault ? '默认' : m.exists ? undefined : '不存在',
      badgeTone: m.exists ? ('muted' as const) : ('warn' as const),
    }
  })
  const currentLabel = selected ? (SOURCE_LABEL[selected.source] ?? '模型') : '模型'
  const busy = copying || loadingDir

  const openCurrent = () => {
    if (!listing?.path) return
    void api.openPath(listing.path).catch((e) => notify('err', errText(e)))
  }

  return (
    <section
      className={`panel ov-card ov-models${dropOver ? ' is-drop' : ''}`}
      aria-labelledby="ov-models-title"
      ref={cardRef}
    >
      <div className="panel-head ov-models-head">
        <h2 id="ov-models-title">模型</h2>
        <div className="spacer" />
        {roots.length > 1 ? (
          <div className="model-root-select">
            <Select
              value={root ?? ''}
              options={rootOptions}
              onChange={(value) => {
                setRoot(value)
                setSegments([])
                setCopyError(null)
              }}
              ariaLabel="选择模型目录"
              disabled={copying || loadingPlan}
            />
          </div>
        ) : selected ? (
          <span className="model-root-label" title={selected.path}>
            {rootLabel(selected)}
          </span>
        ) : null}
        <button
          type="button"
          className={`icon-btn${loadingPlan || loadingDir ? ' is-busy' : ''}`}
          onClick={refresh}
          title="刷新模型目录"
          aria-label="刷新模型目录"
          disabled={copying}
        >
          <Icon name="refresh" size={14} />
        </button>
        <button
          type="button"
          className="icon-btn"
          onClick={openCurrent}
          title="在资源管理器中打开"
          aria-label="在资源管理器中打开"
          disabled={!listing?.exists || copying}
        >
          <Icon name="folder" size={14} />
        </button>
      </div>

      {roots.length > 0 && (
        <div className="model-crumbs">
          {segments.length > 0 && (
            <button
              type="button"
              className="icon-btn model-up"
              onClick={() => goTo(segments.slice(0, -1))}
              title="上一级"
              aria-label="上一级"
              disabled={copying}
            >
              <Icon name="back" size={13} />
            </button>
          )}
          <button
            type="button"
            className="model-crumb"
            onClick={() => goTo([])}
            disabled={copying}
            title={root ?? undefined}
          >
            {currentLabel}
          </button>
          {segments.map((seg, index) => (
            <Fragment key={`${index}:${seg}`}>
              <span className="model-crumb-sep" aria-hidden="true">
                /
              </span>
              <button
                type="button"
                className={`model-crumb${index === segments.length - 1 ? ' is-current' : ''}`}
                onClick={() => goTo(segments.slice(0, index + 1))}
                disabled={copying}
              >
                {seg}
              </button>
            </Fragment>
          ))}
          <div className="spacer" />
          {loadingDir && <span className="model-loading" aria-label="读取中" />}
        </div>
      )}

      <div className="ov-models-body">
        {copyError && (
          <div className="model-copy-error" role="alert">
            <span>{copyError}</span>
            <button
              type="button"
              className="icon-btn"
              onClick={() => setCopyError(null)}
              title="关闭"
              aria-label="关闭复制错误"
            >
              <Icon name="close" size={12} />
            </button>
          </div>
        )}
        {!loadingPlan && !error && roots.length === 0 ? (
          <div className="model-empty">
            <span className="ov-models-glyph" aria-hidden="true">
              <Icon name="box" size={22} />
            </span>
            <span className="ov-models-label">没有模型目录</span>
            <button type="button" className="ghost sm" onClick={onOpenStorage}>
              存储设置
            </button>
          </div>
        ) : loadingPlan && !plan ? (
          <div className="model-empty">
            <span className="ov-models-label">读取中…</span>
          </div>
        ) : error ? (
          <div className="model-empty">
            <span className="model-error">{error}</span>
            <button type="button" className="ghost sm" onClick={refresh}>
              重试
            </button>
          </div>
        ) : listing && !listing.exists ? (
          <div className="model-empty">
            <span className="ov-models-label">目录不存在</span>
            <span className="ov-models-path mono">{listing.path}</span>
          </div>
        ) : listing && listing.entries.length === 0 ? (
          <div className="model-empty">
            <span className="ov-models-label">空目录</span>
          </div>
        ) : (
          <div className="model-list" aria-busy={busy}>
            {(listing?.entries ?? []).map((entry) =>
              entry.isDir ? (
                <button
                  type="button"
                  className="model-row"
                  key={`d:${entry.name}`}
                  onClick={() => goTo([...segments, entry.name])}
                  disabled={copying}
                >
                  <span className="model-row-icon is-dir" aria-hidden="true">
                    <Icon name="folder" size={15} />
                  </span>
                  <span className="model-row-main">
                    <span className="model-row-name">{entry.name}</span>
                    <span className="model-row-meta">
                      <span>文件夹</span>
                      {entry.modified ? <span>{fmtDate(entry.modified)}</span> : null}
                    </span>
                  </span>
                  <Icon name="chevron" size={13} className="model-row-chev" />
                </button>
              ) : (
                <div className="model-row is-file" key={`f:${entry.name}`}>
                  <span className="model-row-icon" aria-hidden="true">
                    <Icon name="file" size={15} />
                  </span>
                  <span className="model-row-main">
                    <span className="model-row-name">{entry.name}</span>
                    <span className="model-row-meta">
                      <span>{fmtBytes(entry.size)}</span>
                      {entry.modified ? <span>{fmtDate(entry.modified)}</span> : null}
                    </span>
                  </span>
                </div>
              ),
            )}
          </div>
        )}

        {dropOver && (
          <div className="model-drop" aria-hidden="true">
            <Icon name="copy" size={20} />
            <span>释放以复制到当前目录</span>
          </div>
        )}
        {copying && (
          <div className="model-drop is-busy" aria-live="polite">
            <span className="model-spinner" aria-hidden="true" />
            <span>复制中…</span>
          </div>
        )}
      </div>
    </section>
  )
}
