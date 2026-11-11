import { useCallback, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { api, errText } from '../lib/api'
import { SOURCE_LABEL, modelTypeLabel } from '../lib/storage'
import type { InstanceConfig, StorageDir } from '../types'
import Icon from './Icon'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
}

interface MenuPos {
  left: number
  top: number
  width: number
  up: boolean
}

/**
 * 模型目录可能不止一个：实例自己的、附加的、全局共享的。
 * 鼠标悬停时先算一遍实际生效列表；只有一个就直接打开，多个才弹选择层。
 */
export default function ModelDirsButton({ inst, notify }: Props) {
  const btnRef = useRef<HTMLButtonElement | null>(null)
  const menuRef = useRef<HTMLDivElement | null>(null)
  const hideTimer = useRef<number | null>(null)
  const hoverRef = useRef(false)
  const loadingRef = useRef<Promise<StorageDir[]> | null>(null)
  const [dirs, setDirs] = useState<StorageDir[] | null>(null)
  const [open, setOpen] = useState(false)
  const [pinned, setPinned] = useState(false)
  const [pos, setPos] = useState<MenuPos | null>(null)

  const clearTimer = () => {
    if (hideTimer.current !== null) {
      window.clearTimeout(hideTimer.current)
      hideTimer.current = null
    }
  }

  const close = useCallback(() => {
    clearTimer()
    setOpen(false)
    setPinned(false)
  }, [])

  useEffect(() => () => clearTimer(), [])

  const loadDirs = (): Promise<StorageDir[]> => {
    // 悬停和点击可能前后脚发生，共用同一请求，避免打开两个菜单。
    if (loadingRef.current) return loadingRef.current
    const request = api
      .instanceStoragePlan(inst.id)
      .then((plan) => {
        const unique: StorageDir[] = []
        const seen = new Set<string>()
        for (const dir of plan.models) {
          const key = dir.path.trim().replace(/[\\/]+$/, '').toLowerCase()
          if (!key || seen.has(key)) continue
          seen.add(key)
          unique.push(dir)
        }
        setDirs(unique)
        return unique
      })
      .catch((e) => {
        notify('err', errText(e))
        return [] as StorageDir[]
      })
      .finally(() => {
        loadingRef.current = null
      })
    loadingRef.current = request
    return request
  }

  const openOne = async (dir: StorageDir) => {
    try {
      await api.openPath(dir.path)
      notify('ok', `已打开「${inst.name}」的模型目录`)
      close()
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const placeMenu = (list: StorageDir[]) => {
    const el = btnRef.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    const width = Math.min(380, Math.max(220, window.innerWidth - 16))
    const estimated = Math.min(360, 52 + list.length * 62)
    const below = rect.bottom + 6
    const up = below + estimated > window.innerHeight - 8 && rect.top - estimated - 6 >= 8
    const top = up ? rect.top - estimated - 6 : below
    const left = Math.max(8, Math.min(rect.left, window.innerWidth - width - 8))
    setPos({ left, top, width, up })
  }

  const reveal = async (pin: boolean) => {
    clearTimer()
    const list = await loadDirs()
    if (list.length === 0) {
      if (pin) notify('err', '这个实例还没有可打开的模型目录')
      return
    }
    if (list.length === 1) {
      if (pin) await openOne(list[0])
      return
    }
    // 悬停请求返回时鼠标已经离开，就别突然弹出来。
    if (!pin && !hoverRef.current) return
    placeMenu(list)
    setOpen(true)
    setPinned(pin)
  }

  const scheduleClose = () => {
    if (pinned) return
    clearTimer()
    hideTimer.current = window.setTimeout(() => setOpen(false), 180)
  }

  const onButtonClick = () => {
    if (open) {
      if (pinned) close()
      else setPinned(true)
      return
    }
    void reveal(true)
  }

  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node
      if (menuRef.current?.contains(target) || btnRef.current?.contains(target)) return
      close()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        close()
        btnRef.current?.focus()
      }
    }
    const onScroll = (event: Event) => {
      const target = event.target
      if (target instanceof Node && menuRef.current?.contains(target)) return
      close()
    }
    const onResize = () => close()
    window.addEventListener('pointerdown', onPointerDown)
    window.addEventListener('keydown', onKeyDown)
    window.addEventListener('scroll', onScroll, true)
    window.addEventListener('resize', onResize)
    return () => {
      window.removeEventListener('pointerdown', onPointerDown)
      window.removeEventListener('keydown', onKeyDown)
      window.removeEventListener('scroll', onScroll, true)
      window.removeEventListener('resize', onResize)
    }
  }, [open, close])

  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className="dir-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        onMouseEnter={() => {
          hoverRef.current = true
          if (!open) void reveal(false)
          else clearTimer()
        }}
        onMouseLeave={() => {
          hoverRef.current = false
          scheduleClose()
        }}
        onClick={onButtonClick}
        onKeyDown={(event) => {
          if (event.key === 'ArrowDown' && !open) {
            event.preventDefault()
            void reveal(true)
          }
        }}
      >
        <Icon name="box" size={14} />
        模型目录
      </button>

      {open &&
        pos &&
        createPortal(
          <div
            ref={menuRef}
            className={`select-menu model-dir-menu${pos.up ? ' up' : ''}`}
            role="menu"
            aria-label="模型目录"
            style={{ position: 'fixed', left: pos.left, top: pos.top, width: pos.width }}
            onMouseEnter={clearTimer}
            onMouseLeave={scheduleClose}
          >
            <div className="model-dir-head">选择要打开的模型目录</div>
            {dirs?.map((dir) => (
              <button
                key={`${dir.path}-${dir.source}`}
                type="button"
                role="menuitem"
                className="model-dir-item"
                title={dir.path}
                onClick={() => void openOne(dir)}
              >
                <span className="model-dir-top">
                  <span className="model-dir-source">{SOURCE_LABEL[dir.source] ?? dir.source}</span>
                  <span className="model-dir-flags">
                    {dir.isDefault && <span className="model-dir-flag">下载落点</span>}
                    {!dir.exists && <span className="model-dir-flag danger">不存在</span>}
                  </span>
                </span>
                <span className="model-dir-path mono">{dir.path}</span>
                {dir.type && <span className="model-dir-type">{modelTypeLabel(dir.type)}</span>}
              </button>
            ))}
          </div>,
          document.body,
        )}
    </>
  )
}
