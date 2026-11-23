import Button from '../components/Button'
import IconTextButton from '../components/IconTextButton'
import Checkbox from '../components/Checkbox'
import { TextInput } from '../components/Form'
import {
  Fragment,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import type { KeyboardEvent, MouseEvent as ReactMouseEvent } from 'react'
import { createPortal } from 'react-dom'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { api, errText } from '../lib/api'
import type {
  InstanceConfig,
  ModelCopyResult,
  ModelEntry,
  ModelListing,
  ModelOpResult,
  ModelSearchHit,
  StorageDir,
  StoragePlan,
} from '../types'
import Icon from '../components/Icon'
import type { IconName } from '../components/Icon'
import Select from '../components/Select'

const SOURCE_LABEL: Record<string, string> = {
  own: '实例目录',
  extra: '实例附加',
  shared: '共享目录',
  launch: '启动参数',
}

type ClipMode = 'copy' | 'cut'

interface ModelClipboard {
  mode: ClipMode
  root: string
  rel: string
  names: string[]
}

function pathSegments(path: string): string[] {
  return path
    .trim()
    .replace(/[\\/]+$/, '')
    .split(/[\\/]+/)
    .filter(Boolean)
}

/**
 * 目录同名时用尽量短的后缀区分，例如两个 models 显示为
 * 「D:\models」和「Comfyui模型\models」；后缀仍相同就继续加一段。
 */
function rootShortName(dir: StorageDir, roots: StorageDir[]): string {
  const segments = pathSegments(dir.path)
  const base = segments[segments.length - 1] ?? dir.path
  const sameBase = roots.filter((other) => {
    const otherSegments = pathSegments(other.path)
    return (otherSegments[otherSegments.length - 1] ?? other.path).toLowerCase() === base.toLowerCase()
  })
  if (sameBase.length <= 1) return base
  for (let count = 2; count <= segments.length; count += 1) {
    const suffix = segments.slice(-count).join('\\').toLowerCase()
    const unique = sameBase.every((other) => {
      if (other.path === dir.path) return true
      const otherSegments = pathSegments(other.path)
      return otherSegments.slice(-count).join('\\').toLowerCase() !== suffix
    })
    if (unique) return segments.slice(-count).join('\\')
  }
  return dir.path
}

function rootSource(dir: StorageDir): string {
  return SOURCE_LABEL[dir.source] ?? '模型'
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

function joinWindowsPath(base: string, name: string): string {
  return `${base.replace(/[\\/]+$/, '')}\\${name}`
}

function scrollKeyFor(root: string | null, segments: string[]): string {
  return `${root ?? ''}\u0000${segments.join('\u0000')}`
}

interface ContextMenuState {
  x: number
  y: number
  entry: ModelEntry | null
  targets: string[]
}

type DirMotion = 'forward' | 'back' | 'none'

function ModelMenuItem({
  icon,
  label,
  onClick,
  disabled = false,
  danger = false,
}: {
  icon: IconName
  label: string
  onClick: () => void
  disabled?: boolean
  danger?: boolean
}) {
  return (
    <button
      type="button"
      role="menuitem"
      className={`model-menu-item${danger ? ' is-danger' : ''}`}
      onClick={onClick}
      disabled={disabled}
    >
      <Icon name={icon} size={13} />
      <span>{label}</span>
    </button>
  )
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
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null)
  const [listingKey, setListingKey] = useState<string | null>(null)
  const [dirMotion, setDirMotion] = useState<DirMotion>('none')
  const [searchOpen, setSearchOpen] = useState(false)
  const [search, setSearch] = useState('')
  const [searchHits, setSearchHits] = useState<ModelSearchHit[]>([])
  const [searching, setSearching] = useState(false)
  const [searchError, setSearchError] = useState<string | null>(null)
  const [reveal, setReveal] = useState<{ rel: string; name: string } | null>(null)
  const cardRef = useRef<HTMLElement>(null)
  const listRef = useRef<HTMLDivElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const searchBoxRef = useRef<HTMLDivElement>(null)
  const searchButtonRef = useRef<HTMLButtonElement>(null)
  const searchResultsRef = useRef<HTMLDivElement>(null)
  const searchInputRef = useRef<HTMLInputElement>(null)
  const searchRequestRef = useRef(0)
  const requestRef = useRef(0)
  const scrollRef = useRef(new Map<string, number>())
  const restoredKeyRef = useRef<string | null>(null)
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
  const scrollKey = scrollKeyFor(root, segments)
  const busy = copying || loadingDir || opBusy

  const loadDir = useCallback(
    async (selectedRoot: string, segs: string[]) => {
      const token = ++requestRef.current
      setLoadingDir(true)
      try {
        const next = await api.modelsList(inst.id, selectedRoot, segs.join('/'))
        if (token !== requestRef.current) return
        setListing(next)
        setListingKey(scrollKeyFor(selectedRoot, segs))
        setError(null)
      } catch (e) {
        if (token !== requestRef.current) return
        setListing(null)
        setListingKey(null)
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

  // 返回上一级时把离开前的滚动位置放回去；同一目录刷新时保留当前滚动。
  useLayoutEffect(() => {
    if (!listing || !listRef.current || listingKey !== scrollKey) return
    if (restoredKeyRef.current === scrollKey) return
    restoredKeyRef.current = scrollKey
    listRef.current.scrollTop = scrollRef.current.get(scrollKey) ?? 0
  }, [listing, listingKey, scrollKey])

  // 搜索框从顶栏展开后先聚焦，行为与节点卡搜索一致。
  useEffect(() => {
    if (!searchOpen) return
    const frame = requestAnimationFrame(() => searchInputRef.current?.focus())
    return () => cancelAnimationFrame(frame)
  }, [searchOpen])

  // 输入停顿一下再递归搜，避免每敲一个字都扫一遍模型目录。
  useEffect(() => {
    if (!searchOpen || !root) {
      setSearching(false)
      setSearchHits([])
      setSearchError(null)
      return
    }
    const needle = search.trim()
    if (!needle) {
      searchRequestRef.current += 1
      setSearching(false)
      setSearchHits([])
      setSearchError(null)
      return
    }
    const token = ++searchRequestRef.current
    const timer = window.setTimeout(() => {
      setSearching(true)
      setSearchError(null)
      api
        .modelsSearch(inst.id, root, needle)
        .then((hits) => {
          if (token === searchRequestRef.current) setSearchHits(hits)
        })
        .catch((e) => {
          if (token === searchRequestRef.current) {
            setSearchHits([])
            setSearchError(errText(e))
          }
        })
        .finally(() => {
          if (token === searchRequestRef.current) setSearching(false)
        })
    }, 180)
    return () => window.clearTimeout(timer)
  }, [searchOpen, search, root, inst.id])

  // 有内容时点卡片外面不收起；空框才自动收。
  useEffect(() => {
    if (!searchOpen) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node
      if (
        searchBoxRef.current?.contains(target) ||
        searchButtonRef.current?.contains(target) ||
        searchResultsRef.current?.contains(target)
      ) {
        return
      }
      if (search.trim()) return
      setSearchOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown, true)
    return () => document.removeEventListener('pointerdown', onPointerDown, true)
  }, [searchOpen, search])

  // 右键菜单：点空白处、Esc、窗口失焦或尺寸变化都收起。
  useEffect(() => {
    if (!contextMenu) return
    const close = () => setContextMenu(null)
    const onPointerDown = (event: PointerEvent) => {
      if (menuRef.current?.contains(event.target as Node)) return
      close()
    }
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') close()
    }
    window.addEventListener('pointerdown', onPointerDown, true)
    window.addEventListener('keydown', onKey)
    window.addEventListener('blur', close)
    window.addEventListener('resize', close)
    window.addEventListener('scroll', close, true)
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true)
      window.removeEventListener('keydown', onKey)
      window.removeEventListener('blur', close)
      window.removeEventListener('resize', close)
      window.removeEventListener('scroll', close, true)
    }
  }, [contextMenu])

  // 菜单贴近视口边缘时先量一次实际尺寸，再夹回可视区。
  useLayoutEffect(() => {
    if (!contextMenu || !menuRef.current) return
    const rect = menuRef.current.getBoundingClientRect()
    const x = Math.max(8, Math.min(contextMenu.x, window.innerWidth - rect.width - 8))
    const y = Math.max(8, Math.min(contextMenu.y, window.innerHeight - rect.height - 8))
    if (x !== contextMenu.x || y !== contextMenu.y) {
      setContextMenu((prev) => (prev ? { ...prev, x, y } : prev))
    }
  }, [contextMenu])

  // 换目录时清掉临时状态，避免重命名/删除确认跑到别的目录里。
  useEffect(() => {
    setSelected(new Set())
    setRenaming(null)
    setCreating(null)
    setConfirmDelete(null)
  }, [root, segments])

  // 搜索结果点到哪个文件，就跳到它所在目录并选中，接着能直接复制/重命名/删除。
  // 放在清空选择之后，避免刚跳过去又被目录切换副作用清掉。
  useEffect(() => {
    if (!reveal || !listing) return
    if ((listing.rel ?? '') !== reveal.rel) return
    const row = [...(listRef.current?.querySelectorAll<HTMLElement>('.model-row') ?? [])].find(
      (el) => el.dataset.modelName === reveal.name,
    )
    if (!row) {
      setReveal(null)
      return
    }
    setSelected(new Set([reveal.name]))
    row.scrollIntoView({ block: 'center', behavior: 'smooth' })
    setReveal(null)
  }, [listing, reveal])

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

  const saveScroll = () => {
    const el = listRef.current
    if (el) scrollRef.current.set(scrollKey, el.scrollTop)
  }

  const goTo = (next: string[]) => {
    saveScroll()
    setContextMenu(null)
    setCopyError(null)
    setDirMotion(
      next.length > segments.length ? 'forward' : next.length < segments.length ? 'back' : 'none',
    )
    setSegments(next)
  }

  const closeSearch = () => {
    setSearchOpen(false)
    setSearch('')
    setSearchHits([])
    setSearchError(null)
    searchButtonRef.current?.focus()
  }

  const toggleSearch = () => {
    if (searchOpen) closeSearch()
    else setSearchOpen(true)
  }

  const revealSearchHit = (hit: ModelSearchHit) => {
    const next = hit.rel ? hit.rel.split('/').filter(Boolean) : []
    saveScroll()
    setSearchOpen(false)
    setSearch('')
    setSearchHits([])
    setSearchError(null)
    setReveal({ rel: hit.rel, name: hit.name })
    setDirMotion(
      next.length > segments.length ? 'forward' : next.length < segments.length ? 'back' : 'none',
    )
    setSegments(next)
    searchButtonRef.current?.focus({ preventScroll: true })
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
  // trigger 直接显示完整路径，让宽度跟着路径字符数变化；超出可用空间时省略。
  const rootOptions = roots.map((m) => {
    const short = `${rootSource(m)} · ${rootShortName(m, roots)}`
    return {
      value: m.path,
      label: short,
      trigger: m.path,
      badge: m.isDefault ? '默认' : m.exists ? undefined : '不存在',
      badgeTone: m.exists ? ('muted' as const) : ('warn' as const),
    }
  })
  const currentLabel = selectedRoot ? selectedRoot.path : '模型'
  const selectedNames = [...selected]
  const selectedCount = selectedNames.length
  const selectedOne = selectedCount === 1 ? selectedNames[0] : null
  const cutKey =
    clipboard?.mode === 'cut' && clipboard.root === root && clipboard.rel === dirRel
      ? new Set(clipboard.names)
      : null

  const openContextMenu = (event: ReactMouseEvent, entry: ModelEntry | null) => {
    const origin = event.target as HTMLElement | null
    if (origin?.closest('textarea, input:not([type="checkbox"]):not([type="radio"])')) {
      return
    }
    event.preventDefault()
    event.stopPropagation()
    if (!root || busy) return
    if (!entry) {
      setSelected(new Set())
      setContextMenu({ x: event.clientX, y: event.clientY, entry: null, targets: [] })
      return
    }
    const targets = selected.has(entry.name) ? selectedNames : [entry.name]
    if (!selected.has(entry.name)) setSelected(new Set([entry.name]))
    setContextMenu({ x: event.clientX, y: event.clientY, entry, targets })
  }

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

  const stageClipboard = (mode: ClipMode, names: string[] = selectedNames) => {
    if (!root || names.length === 0) return
    setClipboard({ mode, root, rel: dirRel, names })
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

  const beginRename = (name: string | null = selectedOne) => {
    if (!name) return
    setCreating(null)
    setConfirmDelete(null)
    setRenaming({ name, value: name })
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

  const armDelete = (names: string[] = selectedNames) => {
    if (names.length === 0) return
    setRenaming(null)
    setCreating(null)
    setConfirmDelete(names)
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

  // Backspace 不依赖某个按钮保持焦点：在模型卡内、或焦点已经回到 body 时都返回上一级。
  useEffect(() => {
    if (!active) return
    const onBackspace = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Backspace' || event.defaultPrevented) return
      if (document.querySelector('.model-menu')) return
      const target = event.target instanceof HTMLElement ? event.target : null
      if (target?.closest('textarea, select, [contenteditable="true"]')) return
      if (
        target instanceof HTMLInputElement &&
        !['checkbox', 'radio', 'button', 'submit'].includes(target.type)
      ) {
        return
      }
      if (document.querySelector('.modal-backdrop')) return
      const el = document.activeElement
      const owns = !!el && !!cardRef.current?.contains(el)
      const idle = !el || el === document.body
      if (!owns && !idle) return
      // 根目录也吃掉 Backspace，避免 WebView 把它当成浏览器后退。
      event.preventDefault()
      if (busy || segments.length === 0) return
      goTo(segments.slice(0, -1))
    }
    window.addEventListener('keydown', onBackspace)
    return () => window.removeEventListener('keydown', onBackspace)
  }, [active, busy, goTo, segments])

  const deletingHasDir =
    confirmDelete?.some(
      (name) => listing?.entries.find((entry) => entry.name === name)?.isDir,
    ) ?? false

  const menuEntry = contextMenu?.entry ?? null
  const menuTargets = contextMenu?.targets ?? []
  const menuCount = menuTargets.length
  const searchMode = searchOpen && search.trim().length > 0

  const openMenuEntry = () => {
    if (!menuEntry?.isDir) return
    setContextMenu(null)
    goTo([...segments, menuEntry.name])
  }

  const revealMenuEntry = () => {
    if (!menuEntry || !listing?.path) return
    setContextMenu(null)
    void api
      .openPath(joinWindowsPath(listing.path, menuEntry.name))
      .catch((e) => notify('err', errText(e)))
  }

  const copyMenuPath = () => {
    if (!menuEntry || !listing?.path) return
    setContextMenu(null)
    const clip = navigator.clipboard
    if (!clip) {
      notify('err', '当前环境不能写剪贴板')
      return
    }
    void clip
      .writeText(joinWindowsPath(listing.path, menuEntry.name))
      .then(() => notify('ok', '路径已复制'))
      .catch((e) => notify('err', errText(e)))
  }

  const renameFromMenu = () => {
    if (menuCount !== 1) return
    setContextMenu(null)
    setSelected(new Set(menuTargets))
    beginRename(menuTargets[0])
  }

  const deleteFromMenu = () => {
    if (menuCount === 0) return
    setContextMenu(null)
    setSelected(new Set(menuTargets))
    armDelete(menuTargets)
  }

  return (
    <section
      className={`panel ov-card ov-models${dropOver ? ' is-drop' : ''}`}
      aria-labelledby="ov-models-title"
      ref={cardRef}
      onKeyDown={onKeyDown}
    >
      <div className={`panel-head ov-models-head${searchOpen ? ' is-searching' : ''}`}>
        <h2 id="ov-models-title">模型</h2>
        {!searchOpen &&
          (roots.length > 1 ? (
            <Select
              value={root ?? ''}
              options={rootOptions}
              onChange={(value) => {
                saveScroll()
                setRoot(value)
                setSegments([])
                setCopyError(null)
                setDirMotion('none')
              }}
              ariaLabel="选择模型目录"
              disabled={busy || loadingPlan}
              className="model-root-select"
              menuClassName="model-root-menu"
              compactMenu
              triggerTitle={selectedRoot?.path}
            />
          ) : selectedRoot ? (
            <span className="model-root-label" title={selectedRoot.path}>
              {selectedRoot.path}
            </span>
          ) : null)}
        <div className="spacer" />
        {!searchOpen && (
          <>
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
          </>
        )}
        <div ref={searchBoxRef} className={`ov-models-search${searchOpen ? ' is-open' : ''}`}>
          <TextInput
            ref={searchInputRef}
            className="model-search-input"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Escape') closeSearch()
            }}
            placeholder="搜索模型"
            aria-label="搜索模型"
            aria-hidden={!searchOpen}
            tabIndex={searchOpen ? 0 : -1}
            autoComplete="off"
            spellCheck={false}
          />
        </div>
        <Button
          ref={searchButtonRef}
          type="button"
          className={`icon-btn model-search-toggle${searchOpen ? ' is-on' : ''}`}
          onClick={toggleSearch}
          title={searchOpen ? '关闭搜索' : '搜索模型'}
          aria-label={searchOpen ? '关闭搜索' : '搜索模型'}
          aria-expanded={searchOpen}
        >
          <Icon name={searchOpen ? 'close' : 'search'} size={14} />
        </Button>
      </div>

      {searchMode && (
        <div className="model-crumbs is-search">
          <span className="model-search-caption">搜索结果 · {search.trim()}</span>
          <div className="spacer" />
          <span className="model-search-count">
            {searching ? '搜索中…' : searchError ? '搜索失败' : `${searchHits.length} 项`}
          </span>
        </div>
      )}

      {!searchMode && roots.length > 0 && (
        <div className="model-crumbs">
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
                {'\\'}
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
                <IconTextButton
                  type="button"
                  className={`ghost sm model-paste${clipboard.mode === 'cut' ? ' is-cut' : ''}`}
                  onClick={() => void pasteClipboard()}
                  title={clipboard.mode === 'cut' ? '粘贴并移动' : '粘贴'}
                  aria-label={clipboard.mode === 'cut' ? '粘贴并移动' : '粘贴'}
                  disabled={busy}
                 icon="paste" iconSize={13}>
                  
                  粘贴
                </IconTextButton>
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
                    onClick={() => beginRename()}
                    title="重命名"
                    aria-label="重命名"
                    disabled={busy || !selectedOne}
                  >
                    <Icon name="edit" size={13} />
                  </Button>
                  <Button
                    type="button"
                    className="icon-btn model-op danger"
                    onClick={() => armDelete()}
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

      <div
        className={`ov-models-body${searchMode ? ' is-searching' : ''}`}
        onContextMenu={(event) => {
          if (searchMode) {
            event.preventDefault()
            return
          }
          openContextMenu(event, null)
        }}
      >
        {searchMode && (
          <div
            ref={searchResultsRef}
            className="model-search-list"
            role="listbox"
            aria-label="模型搜索结果"
          >
            {searching ? (
              <div className="model-search-empty">搜索中…</div>
            ) : searchError ? (
              <div className="model-search-empty">{searchError}</div>
            ) : searchHits.length === 0 ? (
              <div className="model-search-empty">没有匹配的模型</div>
            ) : (
              searchHits.map((hit) => (
                <button
                  key={`${hit.rel}/${hit.name}`}
                  type="button"
                  className="model-search-result"
                  role="option"
                  onClick={() => revealSearchHit(hit)}
                >
                  <Icon name={hit.isDir ? 'folder' : 'file'} size={14} />
                  <span className="model-search-result-main">
                    <span className="model-search-result-name">{hit.name}</span>
                    <span className="model-search-result-dir">
                      {hit.rel ? hit.rel.replace(/\//g, '\\') : '根目录'}
                    </span>
                  </span>
                  <span className="model-search-result-meta">
                    {hit.isDir ? '文件夹' : fmtBytes(hit.size)}
                  </span>
                </button>
              ))
            )}
          </div>
        )}
        {loadingDir && <div className="model-progress" aria-hidden="true" />}
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
          <div
            key={listingKey ?? 'empty'}
            className={`model-list${loadingDir ? ' is-loading' : ''}${
              !loadingDir && listingKey === scrollKey && dirMotion !== 'none'
                ? ` is-nav-${dirMotion}`
                : ''
            }`}
            aria-busy={busy}
            ref={listRef}
            onAnimationEnd={(event) => {
              if (event.target === event.currentTarget) setDirMotion('none')
            }}
          >
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
                  data-model-name={entry.name}
                  onContextMenu={(event) => openContextMenu(event, entry)}
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

      {contextMenu &&
        createPortal(
          <div
            ref={menuRef}
            className="model-menu"
            role="menu"
            aria-label="模型文件操作"
            style={{ left: contextMenu.x, top: contextMenu.y }}
            onContextMenu={(event) => event.preventDefault()}
          >
            {menuEntry ? (
              <>
                {menuEntry.isDir && (
                  <>
                    <ModelMenuItem icon="folder" label="打开" onClick={openMenuEntry} />
                    <ModelMenuItem
                      icon="folder"
                      label="在资源管理器中打开"
                      onClick={revealMenuEntry}
                    />
                    <div className="model-menu-sep" role="separator" />
                  </>
                )}
                <ModelMenuItem
                  icon="copy"
                  label={menuCount > 1 ? `复制 ${menuCount} 项` : '复制'}
                  onClick={() => {
                    setContextMenu(null)
                    stageClipboard('copy', menuTargets)
                  }}
                />
                <ModelMenuItem
                  icon="cut"
                  label={menuCount > 1 ? `剪切 ${menuCount} 项` : '剪切'}
                  onClick={() => {
                    setContextMenu(null)
                    stageClipboard('cut', menuTargets)
                  }}
                />
                <ModelMenuItem
                  icon="edit"
                  label="重命名"
                  onClick={renameFromMenu}
                  disabled={menuCount !== 1}
                />
                <ModelMenuItem
                  icon="link"
                  label="复制路径"
                  onClick={copyMenuPath}
                />
                <div className="model-menu-sep" role="separator" />
                <ModelMenuItem
                  icon="trash"
                  label={menuCount > 1 ? `删除 ${menuCount} 项` : '删除'}
                  onClick={deleteFromMenu}
                  danger
                />
              </>
            ) : (
              <>
                <ModelMenuItem
                  icon="plus"
                  label="新建文件夹"
                  onClick={() => {
                    setContextMenu(null)
                    beginCreate()
                  }}
                />
                <ModelMenuItem
                  icon="paste"
                  label="粘贴"
                  onClick={() => {
                    setContextMenu(null)
                    void pasteClipboard()
                  }}
                  disabled={!clipboard}
                />
                <div className="model-menu-sep" role="separator" />
                <ModelMenuItem
                  icon="refresh"
                  label="刷新"
                  onClick={() => {
                    setContextMenu(null)
                    refresh()
                  }}
                />
                <ModelMenuItem
                  icon="folder"
                  label="在资源管理器中打开"
                  onClick={() => {
                    setContextMenu(null)
                    openCurrent()
                  }}
                  disabled={!listing?.exists}
                />
              </>
            )}
          </div>,
          document.body,
        )}
    </section>
  )
}
