import {
  useEffect,
  useId,
  useState,
  type CSSProperties,
  type FocusEvent,
  type MouseEvent,
} from 'react'
import { createPortal } from 'react-dom'

interface Props {
  message: string | null
  /** 传入后变成可点击按钮；不传时只负责悬浮摘要。 */
  onClick?: () => void
  label?: string
}

function errorSummary(message: string | null): string {
  const text = (message ?? '未记录错误详情').replace(/\s+/g, ' ').trim()
  return text.length > 180 ? `${text.slice(0, 180)}…` : text
}

/** 出错徽标：悬浮显示摘要，详情页可点击打开完整错误。 */
export default function ErrorBadge({ message, onClick, label = '出错' }: Props) {
  const [tip, setTip] = useState<{ left: number; top: number; above: boolean } | null>(null)
  const tipId = useId()
  const summary = errorSummary(message)

  const show = (event: MouseEvent<HTMLElement> | FocusEvent<HTMLElement>) => {
    const rect = event.currentTarget.getBoundingClientRect()
    const above = rect.bottom + 79 > window.innerHeight
    setTip({
      left: Math.min(Math.max(12, rect.left), Math.max(12, window.innerWidth - 312)),
      top: above ? rect.top - 7 : rect.bottom + 7,
      above,
    })
  }
  const hide = () => setTip(null)

  useEffect(() => {
    if (!tip) return
    const dismiss = () => setTip(null)
    window.addEventListener('pointerdown', dismiss, true)
    window.addEventListener('scroll', dismiss, true)
    window.addEventListener('resize', dismiss)
    return () => {
      window.removeEventListener('pointerdown', dismiss, true)
      window.removeEventListener('scroll', dismiss, true)
      window.removeEventListener('resize', dismiss)
    }
  }, [tip])

  const common = {
    className: `badge err${onClick ? ' error-badge-action' : ''}`,
    'aria-label': message ? `出错：${summary}` : '出错：未记录错误详情',
    'aria-describedby': tip ? tipId : undefined,
    onMouseEnter: show,
    onMouseLeave: hide,
    onPointerDown: hide,
    onFocus: (event: FocusEvent<HTMLElement>) => {
      if (event.currentTarget.matches(':focus-visible')) show(event)
    },
    onBlur: hide,
  }

  return (
    <>
      {onClick ? (
        <button
          {...common}
          type="button"
          onClick={() => {
            hide()
            onClick()
          }}
        >
          <span className="dot" />
          {label}
        </button>
      ) : (
        <span {...common}>
          <span className="dot" />
          {label}
        </span>
      )}
      {tip &&
        createPortal(
          <div
            className={`error-tip${tip.above ? ' is-above' : ''}`}
            id={tipId}
            role="tooltip"
            style={{ left: tip.left, top: tip.top } as CSSProperties}
          >
            {summary}
          </div>,
          document.body,
        )}
    </>
  )
}
