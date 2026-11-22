import type { ButtonHTMLAttributes, ReactNode } from 'react'
import IconTextButton from './IconTextButton'
import type { IconName } from './Icon'

interface Props extends ButtonHTMLAttributes<HTMLButtonElement> {
  icon: IconName
  children: ReactNode
}

/** 节点卡片顶栏里“图标 + 文字”的小按钮，统一图标和文字的垂直对齐。 */
export default function NodeHeadButton({ icon, children, className = '', ...rest }: Props) {
  return (
    <IconTextButton
      variant="ghost"
      size="sm"
      className={`ov-node-head-btn${className ? ` ${className}` : ''}`}
      icon={icon}
      iconSize={13}
      {...rest}
    >
      <span className="ov-bulk-label">{children}</span>
    </IconTextButton>
  )
}
