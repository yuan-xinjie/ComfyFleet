import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react'
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent } from 'react'
import { createPortal } from 'react-dom'
import Icon from './Icon'

export type SelectOptionTone = 'accent' | 'ok' | 'warn' | 'muted'

export interface SelectOption {
  value: string
  label: string
  /** 选项右侧的短提示，如「推荐」「已验证」 */
  badge?: string
  badgeTone?: SelectOptionTone
  /** 选项的第二行说明，用于目录类型这类「名字 · 解释」的长文本 */
  meta?: string
  disabled?: boolean
}

interface Props {
  value: string
  options: SelectOption[]
  onChange: (value: string) => void
  ariaLabel: string
  disabled?: boolean
  className?: string
  placeholder?: string
  /** 弹层按选项文字自适应宽度，而不是保证 200px 最小宽度 */
  compactMenu?: boolean
}

const MENU_GAP = 6
const MENU_MAX = 288
const MENU_MIN_WIDTH = 200

/**
 * 自绘下拉：原生 select 的弹层在 Windows 上无法跟随深色界面，
 * 这里把 trigger 做成按钮、弹层挂到 body，保证列表也能用同一套 token。
 *
 * 交互约定与原生一致：点击 / ↑↓ / Home / End / Enter / Esc；
 * 弹层不抢焦点，键盘始终留在 trigger 上，由 aria-activedescendant 指出当前项。
 */
export default function Select({
  value,
  options,
  onChange,
  ariaLabel,
  disabled = false,
  className = '',
  placeholder = '请选择',
  compactMenu = false,
}: Props) {
  const id = useId().replace(/:/g, '')
  const rootRef = useRef<HTMLDivElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const [open, setOpen] = useState(false)
  const [activeIndex, setActiveIndex] = useState(-1)
  const [menuStyle, setMenuStyle] = useState<CSSProperties | null>(null)
  const [dropUp, setDropUp] = useState(false)

  const selectedIndex = options.findIndex((option) => option.value === value)
  const selected = selectedIndex >= 0 ? options[selectedIndex] : null
  const selectedText = selected
    ? `${selected.label}${selected.meta ? ` · ${selected.meta}` : ''}`
    : placeholder
  const listId = `${id}-listbox`

  const firstEnabled = () => options.findIndex((option) => !option.disabled)
  const lastEnabled = () => {
    for (let index = options.length - 1; index >= 0; index -= 1) {
      if (!options[index].disabled) return index
    }
    return -1
  }

  const openMenu = (index?: number) => {
    if (disabled) return
    setMenuStyle(null)
    setActiveIndex(
      index ??
        (selected && !selected.disabled ? selectedIndex : firstEnabled()),
    )
    setOpen(true)
  }

  const closeMenu = (restoreFocus = false) => {
    setOpen(false)
    setActiveIndex(-1)
    if (restoreFocus) triggerRef.current?.focus()
  }

  const commit = (option: SelectOption) => {
    if (option.disabled) return
    if (option.value !== value) onChange(option.value)
    closeMenu(true)
  }

  const step = (direction: 1 | -1) => {
    if (options.length === 0) return
    let start = activeIndex
    if (start < 0) start = direction > 0 ? -1 : 0
    for (let hop = 1; hop <= options.length; hop += 1) {
      const index = (start + direction * hop + options.length * 2) % options.length
      if (!options[index].disabled) {
        setActiveIndex(index)
        return
      }
    }
  }

  // 点击弹层和 trigger 之外的地方收起；弹层在 portal 里，所以两个 ref 都要看。
  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node
      if (rootRef.current?.contains(target) || menuRef.current?.contains(target)) return
      setOpen(false)
      setActiveIndex(-1)
    }
    document.addEventListener('pointerdown', onPointerDown, true)
    return () => document.removeEventListener('pointerdown', onPointerDown, true)
  }, [open])

  // 弹层位置由 trigger 的 rect 决定；向上还是向下，取决于下面够不够。
  useLayoutEffect(() => {
    if (!open) return
    const place = () => {
      const trigger = triggerRef.current
      const menu = menuRef.current
      if (!trigger || !menu) return
      const rect = trigger.getBoundingClientRect()
      const viewportWidth = window.innerWidth
      const viewportHeight = window.innerHeight
      const dialog = trigger.closest('.modal')
      const dialogRect = dialog?.getBoundingClientRect()
      const footerRect = dialog?.querySelector('.modal-foot')?.getBoundingClientRect()
      const boundTop = Math.max(8, dialogRect?.top ?? 8)
      const boundBottom = Math.min(
        viewportHeight - 8,
        footerRect?.top ?? dialogRect?.bottom ?? viewportHeight - 8,
      )
      const boundLeft = Math.max(8, (dialogRect?.left ?? 0) + 8)
      const boundRight = Math.min(
        viewportWidth - 8,
        (dialogRect?.right ?? viewportWidth) - 8,
      )
      const below = boundBottom - rect.bottom - MENU_GAP - 8
      const above = rect.top - boundTop - MENU_GAP - 8
      const measured = Math.min(menu.scrollHeight + 10, MENU_MAX)
      const up = below < measured && above > below
      const contentWidth = compactMenu
        ? Math.max(menu.scrollWidth, menu.getBoundingClientRect().width)
        : 0
      const width = Math.min(
        Math.max(rect.width, compactMenu ? 0 : MENU_MIN_WIDTH, contentWidth),
        Math.max(160, boundRight - boundLeft),
      )
      const left = Math.min(
        Math.max(rect.left, boundLeft),
        Math.max(boundLeft, boundRight - width),
      )
      const maxHeight = Math.max(96, Math.min(MENU_MAX, up ? above : below))
      setDropUp(up)
      setMenuStyle({
        position: 'fixed',
        left,
        width,
        maxHeight,
        top: up ? undefined : rect.bottom + MENU_GAP,
        bottom: up ? viewportHeight - rect.top + MENU_GAP : undefined,
        visibility: 'visible',
      })
    }
    place()
    const onMove = () => place()
    window.addEventListener('scroll', onMove, true)
    window.addEventListener('resize', onMove)
    return () => {
      window.removeEventListener('scroll', onMove, true)
      window.removeEventListener('resize', onMove)
    }
  }, [open, options.length, compactMenu])

  // 键盘移动时只滚弹层，不带着整页跑。
  useLayoutEffect(() => {
    if (!open || activeIndex < 0) return
    const menu = menuRef.current
    const item = menu?.querySelector<HTMLElement>(`[data-option-index="${activeIndex}"]`)
    if (!menu || !item) return
    const top = item.offsetTop
    const bottom = top + item.offsetHeight
    if (top < menu.scrollTop) menu.scrollTop = top
    else if (bottom > menu.scrollTop + menu.clientHeight) {
      menu.scrollTop = bottom - menu.clientHeight
    }
  }, [open, activeIndex])

  const onTriggerKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault()
        if (!open) openMenu(selected && !selected.disabled ? selectedIndex : firstEnabled())
        else step(1)
        break
      case 'ArrowUp':
        event.preventDefault()
        if (!open) openMenu(selected && !selected.disabled ? selectedIndex : lastEnabled())
        else step(-1)
        break
      case 'Home':
        if (!open) break
        event.preventDefault()
        setActiveIndex(firstEnabled())
        break
      case 'End':
        if (!open) break
        event.preventDefault()
        setActiveIndex(lastEnabled())
        break
      case 'Enter':
      case ' ':
        event.preventDefault()
        if (open && activeIndex >= 0 && options[activeIndex]) commit(options[activeIndex])
        else openMenu()
        break
      case 'Escape':
        if (open) {
          event.preventDefault()
          event.stopPropagation()
          closeMenu(true)
        }
        break
      case 'Tab':
        if (open) closeMenu()
        break
      default:
        break
    }
  }

  return (
    <div ref={rootRef} className={`select ${className}`.trim()}>
      <button
        ref={triggerRef}
        type="button"
        className="select-trigger"
        role="combobox"
        aria-label={ariaLabel}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-activedescendant={
          open && activeIndex >= 0 ? `${id}-option-${activeIndex}` : undefined
        }
        disabled={disabled}
        onClick={() => (open ? closeMenu() : openMenu())}
        onKeyDown={onTriggerKeyDown}
      >
        <span className={`select-value${selected ? '' : ' placeholder'}`}>
          {selectedText}
        </span>
        <Icon name="chevron" size={14} className="select-caret" />
      </button>

      {open &&
        createPortal(
          <div
            ref={menuRef}
            id={listId}
            role="listbox"
            aria-label={ariaLabel}
            className={`select-menu${dropUp ? ' up' : ''}`}
            style={menuStyle ?? { position: 'fixed', visibility: 'hidden' }}
          >
            {options.length === 0 ? (
              <div className="select-empty">没有可选项</div>
            ) : (
              options.map((option, index) => (
                <div
                  key={option.value}
                  id={`${id}-option-${index}`}
                  data-option-index={index}
                  role="option"
                  aria-selected={option.value === value}
                  aria-disabled={option.disabled || undefined}
                  className={`select-option${index === activeIndex ? ' active' : ''}${
                    option.disabled ? ' disabled' : ''
                  }`}
                  onPointerDown={(event) => event.preventDefault()}
                  onMouseEnter={() => {
                    if (!option.disabled) setActiveIndex(index)
                  }}
                  onClick={() => commit(option)}
                >
                  <span className="select-option-main">
                    <span className="select-option-label">{option.label}</span>
                    {option.meta && <span className="select-option-meta">{option.meta}</span>}
                  </span>
                  {option.badge && (
                    <span className={`select-option-badge ${option.badgeTone ?? 'muted'}`}>
                      {option.badge}
                    </span>
                  )}
                  <Icon name="check" size={14} className="select-option-check" />
                </div>
              ))
            )}
          </div>,
          document.body,
        )}
    </div>
  )
}
