import type { CSSProperties } from 'react'
import type { IslandIdleShape } from '../types'

export const ISLAND_WIDTH_MIN = 64
export const ISLAND_WIDTH_MAX = 200
export const ISLAND_WIDTH_DEFAULT = 84
export const ISLAND_ANIM_SPEED_MIN = 50
export const ISLAND_ANIM_SPEED_MAX = 200
export const ISLAND_ANIM_SPEED_DEFAULT = 100
export const ISLAND_SHAPE_INSET_MIN = 4
export const ISLAND_SHAPE_INSET_MAX = 20
export const ISLAND_SHAPE_INSET_DEFAULT = 11
export const ISLAND_SHAPE_ACTION_GAP_MIN = 0
export const ISLAND_SHAPE_ACTION_GAP_MAX = 30
export const ISLAND_SHAPE_ACTION_GAP_DEFAULT = 15
export const ISLAND_BOTTOM_GAP_MIN = 8
export const ISLAND_BOTTOM_GAP_MAX = 160
export const ISLAND_BOTTOM_GAP_DEFAULT = 56
export const ISLAND_GLOW_COLOR_DEFAULT = '#5b8dff'
export const ISLAND_GLOW_RANGE_MIN = 8
export const ISLAND_GLOW_RANGE_MAX = 80
export const ISLAND_GLOW_RANGE_DEFAULT = 24

const clamp = (value: number, min: number, max: number) =>
  Math.max(min, Math.min(max, value))

/** 外发光的颜色与范围变量。 */
export function islandGlowStyle(color: string, range: number): CSSProperties {
  return {
    '--island-glow-color': color.trim() || ISLAND_GLOW_COLOR_DEFAULT,
    '--island-glow-range': `${clamp(range, ISLAND_GLOW_RANGE_MIN, ISLAND_GLOW_RANGE_MAX)}px`,
  } as CSSProperties & Record<string, string>
}

/** 空闲胶囊的宽度和动画时长变量。 */
export function islandIdleStyle(
  width: number,
  speed: number,
  shapeInset: number,
  exactWidth = false,
): CSSProperties {
  const scale = 100 / clamp(speed, ISLAND_ANIM_SPEED_MIN, ISLAND_ANIM_SPEED_MAX)
  const size = clamp(width, ISLAND_WIDTH_MIN, ISLAND_WIDTH_MAX)
  const inset = clamp(shapeInset, ISLAND_SHAPE_INSET_MIN, ISLAND_SHAPE_INSET_MAX)
  // 只调这四个基值；speed 负责整体缩放，不要在 CSS 里另写固定时长。
  return {
    width: exactWidth ? `${size}px` : undefined,
    minWidth: exactWidth ? undefined : `${size}px`,
    '--island-blink-duration': `${(3.4 * scale).toFixed(2)}s`,
    '--island-idle-duration': `${(3.2 * scale).toFixed(2)}s`,
    '--island-shape-inset': `${inset}px`,
  } as CSSProperties & Record<string, string>
}

/** 空闲形态里单独渲染的一侧箭头；展开显示按钮时仍保留。 */
export function IslandChevron({ side }: { side: 'left' | 'right' }) {
  return (
    <svg
      className={`island-chevron is-${side}`}
      viewBox="0 0 10 16"
      fill="none"
      aria-hidden="true"
    >
      <path d={side === 'left' ? 'M2 2l6 6-6 6' : 'M8 2 2 8l6 6'} />
    </svg>
  )
}

/** 空闲形态里单独渲染的一侧圆环；展开显示按钮时仍保留。 */
export function IslandTarget({ side }: { side: 'left' | 'right' }) {
  return <span className={`island-target is-${side}`} aria-hidden="true" />
}

/** 旧设置里可能还存着已删除的圆点形态；统一落到箭头。 */
export function normalizeIslandShape(shape: unknown): IslandIdleShape {
  return shape === 'targets' ? 'targets' : 'chevrons'
}

/** 无任务灵动岛的两种默认形态。 */
export function IslandShape({ shape }: { shape: IslandIdleShape }) {
  if (shape === 'targets') {
    return (
      <span className="island-shape is-targets" aria-hidden="true">
        <i />
        <i />
      </span>
    )
  }

  return (
    <span className="island-shape is-chevrons" aria-hidden="true">
      <svg viewBox="0 0 10 16" fill="none">
        <path d="M2 2l6 6-6 6" />
      </svg>
      <svg viewBox="0 0 10 16" fill="none">
        <path d="M8 2 2 8l6 6" />
      </svg>
    </span>
  )
}
