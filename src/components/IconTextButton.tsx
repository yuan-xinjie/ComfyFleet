import {
  forwardRef,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
} from 'react'
import Button, { type ButtonProps } from './Button'
import Icon, { type IconName } from './Icon'

export interface IconTextButtonProps extends Omit<ButtonProps, 'icon'> {
  icon: IconName
  iconSize?: number
  iconClassName?: string
  iconStyle?: CSSProperties
}

/**
 * 左侧图标、右侧文字的按钮。
 *
 * 只负责图标与文字的结构和对齐；高度、宽度、底色、边框、颜色、
 * 字号、圆角全部由调用方的 className / variant 决定。
 */
const IconTextButton = forwardRef<HTMLButtonElement, IconTextButtonProps>(function IconTextButton(
  { icon, iconSize = 14, iconClassName, iconStyle, className = '', style, children, ...rest },
  ref,
) {
  const innerRef = useRef<HTMLButtonElement | null>(null)
  const [labelShift, setLabelShift] = useState(0)

  useImperativeHandle(ref, () => innerRef.current as HTMLButtonElement)

  // 图标已在 Icon 组件里按墨迹包围盒居中；这里只补偿中文文字：
  // 中文墨迹比行盒中心偏上，字号越小越明显，按实际字号和字重下移。
  useLayoutEffect(() => {
    const button = innerRef.current
    const label = button?.querySelector('.icon-text-btn-label')
    if (!button || !label) return
    const measure = () => {
      const cs = getComputedStyle(label)
      const fontSize = Number.parseFloat(cs.fontSize)
      const fontWeight = Number.parseFloat(cs.fontWeight)
      if (!Number.isFinite(fontSize) || !Number.isFinite(fontWeight)) return
      const base = Math.min(1.2, Math.max(0, (13 - fontSize) * 0.7))
      const shift = base * (fontWeight >= 500 ? 0.5 : 1)
      setLabelShift((prev) => (Math.abs(prev - shift) < 0.01 ? prev : shift))
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(measure)
    observer.observe(button)
    return () => observer.disconnect()
  }, [className])

  return (
    <Button
      ref={innerRef}
      className={`icon-text-btn${className ? ` ${className}` : ''}`}
      style={
        {
          ...style,
          '--icon-text-size': `${iconSize}px`,
          '--icon-label-optical-y': `${labelShift}px`,
        } as CSSProperties
      }
      {...rest}
    >
      <Icon name={icon} size={iconSize} className={iconClassName} style={iconStyle} />
      <span className="icon-text-btn-label">{children}</span>
    </Button>
  )
})

export default IconTextButton
