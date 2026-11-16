import Button from '../components/Button'
import Checkbox from '../components/Checkbox'
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { KeyboardEvent } from 'react'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { api, errText } from '../lib/api'
import type {
  InstanceConfig,
  ModelCopyResult,
  ModelListing,
  ModelOpResult,
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

type ClipMode = 'copy' | 'cut'

interface ModelClipboard {
  mode: ClipMode
  root: string
  rel: string
  names: string[]
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
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [clipboard, setClipboard] = useState<ModelClipboard | null>(null)
  const [renaming, setRenaming] = useState<{ name: string; value: string } | null>(null)
  const [creating, setCreating] = useState<string | null>(null)
  const [confirmDelete, setConfirmDelete] = useState<string[] | null>(null)
  const [opBusy, setOpBusy] = useState(false)
  const [opLabel, setOpLabel] = useState<string | null>(null)
  const cardRef = useRef<HTMLElement>(null)
  const requestRef = useRef(0)
  const stateRef = useRef({ root, segments, active, busy: false })
  stateRef.current = {
    root,
    segments,
    active,
    busy: copying || loadingDir || opBusy,
  }
  const copyRef = useRef<(paths: string[]) => void>(() => {})

  const storageKey = useMemo(() => JSON.stringify(inst.storage), [inst.storage])
  const dirRel = segments.join('/')
  const busy = copying || loadingDir || opBusy

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
        const selectedRoot = keep ? previous : pickDefault(next.models)
        if (selectedRoot !== previous) setSegments([])
        setRoot(selectedRoot)
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

  // 换目录时清掉临时状态，避免重命名/删除确认跑到别的目录里。
  useEffect(() => {
    setSelected(new Set())
    setRenaming(null)
    setCreating(null)
    setConfirmDelete(null)
  }, [root, segments])

  // 外部改动导致某个选中项消失时，把它从选择里剔掉。
  useEffect(() => {
    if (!listing) return
    const names = new Set(listing.entries.map((entry) => entry.name))
    setSelected((prev) => {
      const next = new Set([...prev].filter((name) => names.has(name)))
      return next.size === prev.size ? prev : next
    })
  }, [listing])

  const refresh = () => {
    setReloadTick((v) => v + 1)
  }

  const goTo = (next: string[]) => {
    setCopyError(null)
    setSegments(next)
  }

  const reportResult = (result: ModelOpResult, label: string) => {
    if (result.failed > 0) {
      const message = result.errors[0] ?? `${result.failed} 项失败`
      setCopyError(message)
      notify(
        'err',
        result.done > 0
          ? `${label} ${result.done} 项，${result.failed} 项失败`
          : `${label}失败：${message}`,
      )
      return
    }
    if (result.done === 0 && result.skipped > 0) {
      notify('ok', `${result.skipped} 个同名项已存在`)
      return
    }
    if (result.done > 0 && result.skipped > 0) {
      notify('ok', `${label} ${result.done} 项，跳过 ${result.skipped} 个同名项`)
      return
    }
    if (result.done > 0) notify('ok', `${label} ${result.done} 项`)
  }

  const copyDropped = useCallback(
    async (paths: string[]) => {
      const { root: currentRoot, segments: currentSegments, busy: busyNow } = stateRef.current
      if (!currentRoot || busyNow || paths.length === 0) return
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
          setDropOver(inside && !stateRef.current.busy)
          return
        }
        if (payload.type === 'drop') {
          setDropOver(false)
          if (!inside || stateRef.current.busy) return
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
  const selectedRoot = roots.find((m) => m.path === root) ?? null
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
  const currentLabel = selectedRoot ? (SOURCE_LABEL[selectedRoot.source] ?? '模型') : '模型'
  const selectedNames = [...selected]
  const selectedCount = selectedNames.length
  const selectedOne = selectedCount === 1 ? selectedNames[0] : null
  const cutKey =
    clipboard?.mode === 'cut' && clipboard.root === root && clipboard.rel === dirRel
      ? new Set(clipboard.names)
      : null

  const openCurrent = () => {
    if (!listing?.path) return
    void api.openPath(listing.path).catch((e) => notify('err', errText(e)))
  }

  const toggleSelected = (name: string) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(name)) next.delete(name)
      else next.add(name)
      return next
    })
  }

  const stageClipboard = (mode: ClipMode) => {
    if (!root || selectedCount === 0) return
    setClipboard({ mode, root, rel: dirRel, names: selectedNames })
    setSelected(new Set())
    setRenaming(null)
    setCreating(null)
    setConfirmDelete(null)
  }

  const pasteClipboard = async () => {
    if (!clipboard || !root || busy) return
    setOpBusy(true)
    setOpLabel(clipboard.mode === 'cut' ? '移动中…' : '粘贴中…')
    setCopyError(null)
    try {
      const result = await api.modelsPaste(
        inst.id,
        clipboard.root,
        clipboard.rel,
        clipboard.names,
        root,
        dirRel,
        clipboard.mode === 'cut',
      )
      reportResult(result, clipboard.mode === 'cut' ? '移动' : '粘贴')
      if (clipboard.mode === 'cut') setClipboard(null)
      setSelected(new Set())
      await loadDir(root, segments)
    } catch (e) {
      const text = errText(e)
      setCopyError(text)
      notify('err', text)
    } finally {
      setOpBusy(false)
      setOpLabel(null)
    }
  }

  const beginRename = () => {
    if (!selectedOne) return
    setCreating(null)
    setConfirmDelete(null)
    setRenaming({ name: selectedOne, value: selectedOne })
  }

  const commitRename = async () => {
    if (!renaming || !root) return
    const nextName = renaming.value.trim()
    if (!nextName || nextName === renaming.name) {
      setRenaming(null)
      return
    }
    setOpBusy(true)
    setOpLabel('重命名中…')
    setCopyError(null)
    try {
      await api.modelsRename(inst.id, root, dirRel, renaming.name, nextName)
      notify('ok', `已重命名为 ${nextName}`)
      setRenaming(null)
      setSelected(new Set())
      await loadDir(root, segments)
    } catch (e) {
      const text = errText(e)
      setCopyError(text)
      notify('err', text)
    } finally {
      setOpBusy(false)
      setOpLabel(null)
    }
  }

  const beginCreate = () => {
    if (!root || busy) return
    setSelected(new Set())
    setRenaming(null)
    setConfirmDelete(null)
    setCreating('')
  }

  const commitCreate = async () => {
    if (creating === null || !root) return
    const name = creating.trim()
    if (!name) {
      setCreating(null)
      return
    }
    setOpBusy(true)
    setOpLabel('新建中…')
    setCopyError(null)
    try {
      await api.modelsMkdir(inst.id, root, dirRel, name)
      notify('ok', `已新建文件夹 ${name}`)
      setCreating(null)
      await loadDir(root, segments)
    } catch (e) {
      const text = errText(e)
      setCopyError(text)
      notify('err', text)
    } finally {
      setOpBusy(false)
      setOpLabel(null)
    }
  }

  const armDelete = () => {
    if (selectedCount === 0) return
    setRenaming(null)
    setCreating(null)
    setConfirmDelete(selectedNames)
  }

  const deleteSelected = async () => {
    if (!confirmDelete || !root) return
    setOpBusy(true)
    setOpLabel('删除中…')
    setCopyError(null)
    try {
      const result = await api.modelsDelete(inst.id, root, dirRel, confirmDelete)
      reportResult(result, '删除')
      setConfirmDelete(null)
      setSelected(new Set())
      await loadDir(root, segments)
    } catch (e) {
      const text = errText(e)
      setCopyError(text)
      notify('err', text)
    } finally {
      setOpBusy(false)
      setOpLabel(null)
    }
  }

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    const target = event.target as HTMLElement | null
    if (target?.closest('input, textarea, select, [contenteditable="true"]')) return
    const mod = event.ctrlKey || event.metaKey
    const key = event.key.toLowerCase()
    if (mod && key === 'c' && selectedCount > 0) {
      event.preventDefault()
      stageClipboard('copy')
      return
    }
    if (mod && key === 'x' && selectedCount > 0) {
      event.preventDefault()
      stageClipboard('cut')
      return
    }
    if (mod && key === 'v' && clipboard) {
      event.preventDefault()
      void pasteClipboard()
      return
    }
    if (mod && key === 'a' && listing) {
      event.preventDefault()
      setSelected(new Set(listing.entries.map((entry) => entry.name)))
      return
    }
    if (event.key === 'F2' && selectedOne) {
      event.preventDefault()
      beginRename()
      return
    }
    if (event.key === 'Delete' && selectedCount > 0) {
      event.preventDefault()
      armDelete()
      return
    }
    if (event.key === 'Escape') {
      setRenaming(null)
      setCreating(null)
      setConfirmDelete(null)
    }
  }

  const deletingHasDir =
    confirmDelete?.some(
      (name) => listing?.entries.find((entry) => entry.name === name)?.isDir,
    ) ?? false

  return (
    <section
      className={`panel ov-card ov-models${dropOver ? ' is-drop' : ''}`}
      aria-labelledby="ov-models-title"
      ref={cardRef}
      onKeyDown={onKeyDown}
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
              disabled={busy || loadingPlan}
            />
          </div>
        ) : selectedRoot ? (
          <span className="model-root-label" title={selectedRoot.path}>
            {rootLabel(selectedRoot)}
          </span>
        ) : null}
        <Button
          type="button"
          className={`icon-btn${loadingPlan || loadingDir ? ' is-busy' : ''}`}
          onClick={refresh}
          title="刷新模型目录"
          aria-label="刷新模型目录"
          disabled={busy}
        >
          <Icon name="refresh" size={14} />
        </Button>
        <Button
          type="button"
          className="icon-btn"
          onClick={openCurrent}
          title="在资源管理器中打开"
          aria-label="在资源管理器中打开"
          disabled={!listing?.exists || busy}
        >
          <Icon name="folder" size={14} />
        </Button>
      </div>

      {roots.length > 0 && (
        <div className="model-crumbs">
          {segments.length > 0 && (
            <Button
              type="button"
              className="icon-btn model-up"
              onClick={() => goTo(segments.slice(0, -1))}
              title="上一级"
              aria-label="上一级"
              disabled={busy}
            >
              <Icon name="back" size={13} />
            </Button>
          )}
          <Button
            type="button"
            className="model-crumb"
            onClick={() => goTo([])}
            disabled={busy}
            title={root ?? undefined}
          >
            {currentLabel}
          </Button>
          {segments.map((seg, index) => (
            <Fragment key={`${index}:${seg}`}>
              <span className="model-crumb-sep" aria-hidden="true">
                /
              </span>
              <Button
                type="button"
                className={`model-crumb${index === segments.length - 1 ? ' is-current' : ''}`}
                onClick={() => goTo(segments.slice(0, index + 1))}
                disabled={busy}
              >
                {seg}
              </Button>
            </Fragment>
          ))}
          <div className="spacer" />
          {(selectedCount > 0 || clipboard) && (
            <div className="model-action-group">
              <span className="model-select-count mono">
                {selectedCount > 0
                  ? `${selectedCount} 项`
                  : clipboard
                    ? `${clipboard.mode === 'cut' ? '剪切' : '复制'} ${clipboard.names.length} 项`
                    : ''}
              </span>
              {clipboard && (
                <Button
                  type="button"
                  className={`ghost sm model-paste${clipboard.mode === 'cut' ? ' is-cut' : ''}`}
                  onClick={() => void pasteClipboard()}
                  title={clipboard.mode === 'cut' ? '粘贴并移动' : '粘贴'}
                  aria-label={clipboard.mode === 'cut' ? '粘贴并移动' : '粘贴'}
                  disabled={busy}
                >
                  <Icon name="paste" size={13} />
                  粘贴
                </Button>
              )}
              {selectedCount > 0 && (
                <>
                  <Button
                    type="button"
                    className="icon-btn model-op"
                    onClick={() => stageClipboard('copy')}
                    title="复制"
                    aria-label="复制"
                    disabled={busy}
                  >
                    <Icon name="copy" size={13} />
                  </Button>
                  <Button
                    type="button"
                    className="icon-btn model-op"
                    onClick={() => stageClipboard('cut')}
                    title="剪切"
                    aria-label="剪切"
                    disabled={busy}
                  >
                    <Icon name="cut" size={13} />
                  </Button>
                  <Button
                    type="button"
                    className="icon-btn model-op"
                    onClick={beginRename}
                    title="重命名"
                    aria-label="重命名"
                    disabled={busy || !selectedOne}
                  >
                    <Icon name="edit" size={13} />
                  </Button>
                  <Button
                    type="button"
                    className="icon-btn model-op danger"
                    onClick={armDelete}
                    title="删除"
                    aria-label="删除"
                    disabled={busy}
                  >
                    <Icon name="trash" size={13} />
                  </Button>
                </>
              )}
            </div>
          )}
          <Button
            type="button"
            className="icon-btn model-op"
            onClick={beginCreate}
            title="新建文件夹"
            aria-label="新建文件夹"
            disabled={busy || creating !== null}
          >
            <Icon name="plus" size={13} />
          </Button>
          {loadingDir && <span className="model-loading" aria-label="读取中" />}
        </div>
      )}

      <div className="ov-models-body">
        {copyError && (
          <div className="model-copy-error" role="alert">
            <span>{copyError}</span>
            <Button
              type="button"
              className="icon-btn"
              onClick={() => setCopyError(null)}
              title="关闭"
              aria-label="关闭操作错误"
            >
              <Icon name="close" size={12} />
            </Button>
          </div>
        )}
        {!loadingPlan && !error && roots.length === 0 ? (
          <div className="model-empty">
            <span className="ov-models-glyph" aria-hidden="true">
              <Icon name="box" size={22} />
            </span>
            <span className="ov-models-label">没有模型目录</span>
            <Button type="button" className="ghost sm" onClick={onOpenStorage}>
              存储设置
            </Button>
          </div>
        ) : loadingPlan && !plan ? (
          <div className="model-empty">
            <span className="ov-models-label">读取中…</span>
          </div>
        ) : error ? (
          <div className="model-empty">
            <span className="model-error">{error}</span>
            <Button type="button" className="ghost sm" onClick={refresh}>
              重试
            </Button>
          </div>
        ) : listing && !listing.exists ? (
          <div className="model-empty">
            <span className="ov-models-label">目录不存在</span>
            <span className="ov-models-path mono">{listing.path}</span>
          </div>
        ) : listing && listing.entries.length === 0 && creating === null ? (
          <div className="model-empty">
            <span className="ov-models-label">空目录</span>
          </div>
        ) : (
          <div className="model-list" aria-busy={busy}>
            {creating !== null && (
              <div className="model-row model-new-row">
                <span className="model-row-icon is-dir" aria-hidden="true">
                  <Icon name="folder" size={15} />
                </span>
                <span className="model-row-main">
                  <input
                    className="model-rename-input"
                    aria-label="新文件夹名称"
                    autoFocus
                    value={creating}
                    spellCheck={false}
                    onChange={(e) => setCreating(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') {
                        e.preventDefault()
                        void commitCreate()
                      } else if (e.key === 'Escape') {
                        e.preventDefault()
                        setCreating(null)
                      }
                    }}
                    onBlur={() => setCreating(null)}
                  />
                </span>
              </div>
            )}
            {(listing?.entries ?? []).map((entry) => {
              const isRenaming = renaming?.name === entry.name
              const isCut = cutKey?.has(entry.name) ?? false
              return (
                <div
                  className={`model-row${entry.isDir ? ' is-dir' : ' is-file'}${
                    selected.has(entry.name) ? ' is-selected' : ''
                  }${isCut ? ' is-cut' : ''}`}
                  key={`${entry.isDir ? 'd' : 'f'}:${entry.name}`}
                >
                  <Checkbox
                    className="model-row-check"
                    checked={selected.has(entry.name)}
                    onChange={() => toggleSelected(entry.name)}
                    aria-label={`选择 ${entry.name}`}
                    disabled={busy || isRenaming}
                  />
                  {isRenaming ? (
                    <>
                      <span
                        className={`model-row-icon${entry.isDir ? ' is-dir' : ''}`}
                        aria-hidden="true"
                      >
                        <Icon name={entry.isDir ? 'folder' : 'file'} size={15} />
                      </span>
                      <span className="model-row-main">
                        <input
                          className="model-rename-input"
                          aria-label={`重命名 ${entry.name}`}
                          autoFocus
                          value={renaming.value}
                          spellCheck={false}
                          onChange={(e) =>
                            setRenaming((prev) =>
                              prev ? { ...prev, value: e.target.value } : prev,
                            )
                          }
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') {
                              e.preventDefault()
                              void commitRename()
                            } else if (e.key === 'Escape') {
                              e.preventDefault()
                              setRenaming(null)
                            }
                          }}
                          onBlur={() => setRenaming(null)}
                        />
                        <span className="model-row-meta">
                          {entry.isDir ? <span>文件夹</span> : <span>{fmtBytes(entry.size)}</span>}
                        </span>
                      </span>
                    </>
                  ) : entry.isDir ? (
                    <button
                      type="button"
                      className="model-row-open"
                      onClick={() => goTo([...segments, entry.name])}
                      disabled={busy}
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
                    <button
                      type="button"
                      className="model-row-open"
                      onClick={() => toggleSelected(entry.name)}
                      disabled={busy}
                    >
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
                    </button>
                  )}
                </div>
              )
            })}
          </div>
        )}

        {confirmDelete && (
          <div className="model-confirm" role="alertdialog" aria-label="确认删除">
            <span className="model-confirm-text">
              删除 {confirmDelete.length} 项？
              {deletingHasDir ? ' 文件夹会连同内容一起删除。' : ''}
            </span>
            <Button
              type="button"
              className="danger sm"
              onClick={() => void deleteSelected()}
              disabled={opBusy}
            >
              删除
            </Button>
            <Button
              type="button"
              className="ghost sm"
              onClick={() => setConfirmDelete(null)}
              disabled={opBusy}
            >
              取消
            </Button>
          </div>
        )}

        {dropOver && (
          <div className="model-drop" aria-hidden="true">
            <Icon name="copy" size={20} />
            <span>释放以复制到当前目录</span>
          </div>
        )}
        {(copying || opBusy) && (
          <div className="model-drop is-busy" aria-live="polite">
            <span className="model-spinner" aria-hidden="true" />
            <span>{opLabel ?? '复制中…'}</span>
          </div>
        )}
      </div>
    </section>
  )
}
