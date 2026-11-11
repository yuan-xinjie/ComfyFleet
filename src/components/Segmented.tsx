import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from 'react'

export type SegmentedOption<T extends string> = {
  value: T
  label: ReactNode
}

/**
 * 分段选择：选中底片是一块单独的滑轨，换选项时从旧位置滑到新位置。
 * 按下不缩放（文字不会跟着抖小），只让底色平移。
 */
export default function Segmented<T extends string>({
  value,
  options,
  onChange,
  className = '',
  style,
  ariaLabel,
  role,
  buttonRole,
}: {
  value: T
  options: readonly SegmentedOption<T>[]
  onChange: (next: T) => void
  className?: string
  style?: CSSProperties
  ariaLabel?: string
  role?: 'tablist'
  buttonRole?: 'tab'
}) {
  const rootRef = useRef<HTMLSpanElement>(null)
  const [glider, setGlider] = useState({ x: 0, w: 0 })
  const [ready, setReady] = useState(false)
  const armedRef = useRef(false)
  const frameRef = useRef(0)

  const measure = useCallback(() => {
    const root = rootRef.current
    if (!root) return
    const active = root.querySelector<HTMLButtonElement>('button.active')
    if (!active) return
    const next = { x: active.offsetLeft, w: active.offsetWidth }
    setGlider((prev) => (prev.x === next.x && prev.w === next.w ? prev : next))

    if (next.w <= 0) {
      // 页面还在隐藏（例如未切到的页签）：先别武装过渡，等真正可见时直接落位。
      armedRef.current = false
      setReady(false)
      return
    }
    if (!armedRef.current) {
      cancelAnimationFrame(frameRef.current)
      frameRef.current = requestAnimationFrame(() => {
        armedRef.current = true
        setReady(true)
      })
    }
  }, [])

  useLayoutEffect(() => {
    measure()
  }, [measure, value])

  useEffect(() => {
    const root = rootRef.current
    if (!root) return
    const observer = new ResizeObserver(() => measure())
    observer.observe(root)
    for (const button of root.querySelectorAll('button')) observer.observe(button)
    const onResize = () => measure()
    window.addEventListener('resize', onResize)
    return () => {
      observer.disconnect()
      window.removeEventListener('resize', onResize)
      cancelAnimationFrame(frameRef.current)
    }
  }, [measure])

  return (
    <span
      ref={rootRef}
      className={className ? `seg ${className}` : 'seg'}
      style={style}
      role={role}
      aria-label={ariaLabel}
    >
      <span
        className={ready ? 'seg-glider is-ready' : 'seg-glider'}
        style={{ width: glider.w, transform: `translateX(${glider.x}px)` }}
        aria-hidden="true"
      />
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          role={buttonRole}
          aria-selected={buttonRole === 'tab' ? value === option.value : undefined}
          className={value === option.value ? 'active' : ''}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </span>
  )
}
