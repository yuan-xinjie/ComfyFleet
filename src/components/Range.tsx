import { forwardRef, type CSSProperties, type InputHTMLAttributes } from 'react'

export interface RangeProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> {
  type?: 'range'
}

/**
 * 滑动条：按当前值算出已选比例，交给 CSS 画蓝色填充段。
 * 轨道外观和聚焦样式统一在 pages.css 的 .card-alpha-field 里。
 */
const Range = forwardRef<HTMLInputElement, RangeProps>(function Range(
  { className = '', style, min, max, value, defaultValue, ...rest },
  ref,
) {
  const low = Number(min ?? 0)
  const high = Number(max ?? 100)
  const current = Number(value ?? defaultValue ?? low)
  const fill =
    high > low && Number.isFinite(current)
      ? Math.min(100, Math.max(0, ((current - low) / (high - low)) * 100))
      : 0
  const fillStyle = {
    ...style,
    '--range-fill': `${fill.toFixed(2)}%`,
  } as CSSProperties
  return (
    <input
      ref={ref}
      className={className || undefined}
      style={fillStyle}
      value={value}
      defaultValue={defaultValue}
      min={min}
      max={max}
      {...rest}
      type="range"
    />
  )
})

export default Range
