import { forwardRef, type ButtonHTMLAttributes } from 'react'

export type ButtonVariant = 'primary' | 'ghost' | 'danger' | 'quiet'
export type ButtonSize = 'sm' | 'md'

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** 外观变体；不传时沿用调用方已有的类名。 */
  variant?: ButtonVariant
  /** md 为默认高度，sm 对应 28px 小按钮。 */
  size?: ButtonSize
  /** 仅图标按钮：补上统一的 `.icon-btn` 尺寸和命中区。 */
  icon?: boolean
}

/**
 * 所有按钮的统一入口。
 *
 * 样式仍由 base.css 里的 `.primary / .ghost / .danger / .quiet / .sm / .icon-btn`
 * 承载，组件只负责把 variant / size / icon 映射到这些类，避免每个调用点重复拼类名。
 * 现有调用仍可直接传 className，方便渐进迁移。
 */
const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { variant, size = 'md', icon = false, className = '', children, ...rest },
  ref,
) {
  const classes = [
    variant,
    size === 'sm' ? 'sm' : '',
    icon ? 'icon-btn' : '',
    className,
  ]
    .filter(Boolean)
    .join(' ')
  // 纯文字按钮统一套一层标签，方便做中文墨迹的光学校正；带图标的按钮结构不动。
  const label =
    typeof children === 'string' || typeof children === 'number' ? (
      <span className="btn-label">{children}</span>
    ) : (
      children
    )
  return (
    <button ref={ref} className={classes} {...rest}>
      {label}
    </button>
  )
})

export default Button
