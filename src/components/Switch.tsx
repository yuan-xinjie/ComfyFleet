import type { ReactNode } from 'react'

interface Props {
  checked: boolean
  onChange: (checked: boolean) => void
  id?: string
  disabled?: boolean
  className?: string
  ariaLabel?: string
  title?: string
  children?: ReactNode
}

/** 全局统一的拨动开关：轨道 + 圆点，点击整行生效。 */
export default function Switch({
  checked,
  onChange,
  id,
  disabled,
  className,
  ariaLabel,
  title,
  children,
}: Props) {
  return (
    <label
      className={`switch${checked ? ' is-on' : ''}${disabled ? ' is-disabled' : ''}${
        className ? ` ${className}` : ''
      }`}
      title={title}
    >
      <input
        className="switch-input"
        id={id}
        type="checkbox"
        role="switch"
        checked={checked}
        disabled={disabled}
        aria-checked={checked}
        aria-label={ariaLabel}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="switch-track" aria-hidden="true" />
      {children}
    </label>
  )
}
