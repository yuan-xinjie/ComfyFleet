import type {
  ComponentPropsWithoutRef,
  PointerEvent as ReactPointerEvent,
} from 'react'

const INTERACTIVE = 'button, a, input, select, textarea, [role="button"], [contenteditable="true"]'

/** 指针位置换算成很小的 3D 倾角：以卡片中心为轴，鼠标所在那半边向内压、对侧抬起。 */
function setTilt(event: ReactPointerEvent<HTMLElement>) {
  const el = event.currentTarget
  const rect = el.getBoundingClientRect()
  if (rect.width <= 0 || rect.height <= 0) return
  const x = (event.clientX - rect.left) / rect.width - 0.5
  const y = (event.clientY - rect.top) / rect.height - 0.5
  el.style.setProperty('--tilt-y', `${(x * 8).toFixed(2)}deg`)
  el.style.setProperty('--tilt-x', `${(-y * 6).toFixed(2)}deg`)
}

function clearTilt(el: HTMLElement) {
  el.style.removeProperty('--tilt-x')
  el.style.removeProperty('--tilt-y')
}

/**
 * 首页实例卡的悬停/按压外壳：指针位置换算成很小的 3D 倾角（以卡片中心为
 * 旋转轴）；按下时交给无边框模式做压陷。原按钮、链接仍是自己的交互，不抢点击。
 */
export default function PressCard({
  className = '',
  children,
  onPointerMove,
  onPointerDown,
  onPointerUp,
  onPointerCancel,
  onPointerLeave,
  ...rest
}: ComponentPropsWithoutRef<'article'>) {
  const clearPress = (el: HTMLElement) => {
    delete el.dataset.pressing
  }

  return (
    <article
      {...rest}
      className={`press-card ${className}`.trim()}
      onPointerMove={(event) => {
        onPointerMove?.(event)
        if (event.pointerType === 'touch') return
        setTilt(event)
      }}
      onPointerDown={(event) => {
        onPointerDown?.(event)
        if (event.button !== 0) return
        if ((event.target as HTMLElement).closest(INTERACTIVE)) return
        event.currentTarget.dataset.pressing = 'true'
      }}
      onPointerUp={(event) => {
        onPointerUp?.(event)
        clearPress(event.currentTarget)
      }}
      onPointerCancel={(event) => {
        onPointerCancel?.(event)
        clearPress(event.currentTarget)
        clearTilt(event.currentTarget)
      }}
      onPointerLeave={(event) => {
        onPointerLeave?.(event)
        clearPress(event.currentTarget)
        clearTilt(event.currentTarget)
      }}
    >
      {children}
    </article>
  )
}
