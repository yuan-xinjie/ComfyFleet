import { useEffect, useRef, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

interface Props {
  children: ReactNode
  onClose: () => void
  /** 追加在基础 `.modal` 后面的尺寸/结构类名。 */
  className?: string
  label?: string
  labelledBy?: string
  dismissible?: boolean
}

/**
 * 标准弹层外壳：遮罩、Esc、焦点归还和 Tab 焦点循环都只在这里实现。
 * 业务弹窗只负责内容与底部动作，避免每个弹层各写一套背景/快捷键。
 */
export default function Modal({
  children,
  onClose,
  className,
  label,
  labelledBy,
  dismissible = true,
}: Props) {
  const modalRef = useRef<HTMLDivElement>(null)
  const previousRef = useRef<HTMLElement | null>(
    typeof document !== 'undefined' && document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null,
  )

  useEffect(() => {
    const node = modalRef.current
    if (node && !node.contains(document.activeElement)) node.focus()
    return () => previousRef.current?.focus()
  }, [])

  useEffect(() => {
    if (!dismissible) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        onClose()
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [dismissible, onClose])

  return createPortal(
    <div
      className="modal-backdrop"
      onMouseDown={(event) => {
        if (dismissible && event.target === event.currentTarget) onClose()
      }}
    >
      <div
        ref={modalRef}
        className={`modal${className ? ` ${className}` : ''}`}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        aria-labelledby={labelledBy}
        tabIndex={-1}
        onKeyDown={(event) => {
          if (event.key !== 'Tab') return
          const focusable = modalRef.current?.querySelectorAll<HTMLElement>(
            'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
          )
          if (!focusable?.length) return
          const first = focusable[0]
          const last = focusable[focusable.length - 1]
          if (event.shiftKey && document.activeElement === first) {
            event.preventDefault()
            last.focus()
          } else if (!event.shiftKey && document.activeElement === last) {
            event.preventDefault()
            first.focus()
          }
        }}
      >
        {children}
      </div>
    </div>,
    document.body,
  )
}
