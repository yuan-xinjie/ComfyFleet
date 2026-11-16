const MORPH_MS = 260
const COLLAPSE_MS = 220
const REVEAL_AT = 0.64
const FADE_MS = 80
const EASE = 'cubic-bezier(0.4, 0, 0.2, 1)'

interface Shape {
  rect: DOMRect
  radius: string
  border: string
  background: string
}

function shapeOf(el: HTMLElement): Shape {
  const rect = el.getBoundingClientRect()
  const style = getComputedStyle(el)
  return {
    rect,
    radius: style.borderRadius,
    border: style.borderColor,
    background: style.backgroundColor,
  }
}

function islandShape(): Shape | null {
  const candidates = document.querySelectorAll<HTMLElement>(
    '.island, .island-idle:not(.island-idle-measure)',
  )
  for (const el of candidates) {
    const rect = el.getBoundingClientRect()
    if (rect.width <= 2 || rect.height <= 2) continue
    const concealed = el.closest('.island-root')?.classList.contains('is-concealed') ?? false
    const style = getComputedStyle(el)
    if (!concealed && (style.visibility === 'hidden' || Number(style.opacity) < 0.05)) continue
    return {
      rect,
      radius: style.borderRadius,
      border: style.borderColor,
      background: style.backgroundColor,
    }
  }
  return null
}

function frame(shape: Shape, offset: number) {
  return {
    left: `${shape.rect.left}px`,
    top: `${shape.rect.top}px`,
    width: `${shape.rect.width}px`,
    height: `${shape.rect.height}px`,
    borderRadius: shape.radius,
    borderColor: shape.border,
    backgroundColor: shape.background,
    offset,
  }
}

function mount(from: Shape, to: Shape, duration: number): HTMLElement {
  const node = document.createElement('div')
  node.className = 'window-morph'
  node.setAttribute('aria-hidden', 'true')
  node.style.left = `${from.rect.left}px`
  node.style.top = `${from.rect.top}px`
  node.style.width = `${from.rect.width}px`
  node.style.height = `${from.rect.height}px`
  node.style.borderRadius = from.radius
  node.style.borderColor = from.border
  node.style.backgroundColor = from.background
  document.body.appendChild(node)
  node.animate([frame(from, 0), frame(to, 1)], { duration, easing: EASE })
  return node
}

/**
 * 展开：窗口先隐藏，形变层接近目标尺寸后再交叉淡入。
 * 否则窗口内容在形变早期就已经画出来，会出现两层重叠的残影。
 */
export function playIslandWindowMorph(target: HTMLElement): (() => void) | undefined {
  if (typeof window === 'undefined' || typeof target.animate !== 'function') return
  if (window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return

  const source = islandShape()
  if (!source) return

  target.classList.add('is-window-morphing')
  const to = shapeOf(target)
  if (source.rect.width < 2 || source.rect.height < 2 || to.rect.width < 2 || to.rect.height < 2) {
    target.classList.remove('is-window-morphing')
    return
  }

  const node = mount(source, to, MORPH_MS)
  const animation = node.getAnimations()[0]
  let revealed = false
  let finished = false
  const reveal = () => {
    if (revealed) return
    revealed = true
    target.classList.remove('is-window-morphing')
    target.classList.add('is-window-revealing')
    node.style.transition = `opacity ${FADE_MS}ms ease-out`
    node.style.opacity = '0'
  }
  const revealTimer = window.setTimeout(reveal, Math.round(MORPH_MS * REVEAL_AT))
  const finish = () => {
    if (finished) return
    finished = true
    window.clearTimeout(revealTimer)
    node.remove()
    target.classList.remove('is-window-morphing')
    target.classList.remove('is-window-revealing')
  }
  const cleanup = () => {
    if (finished) return
    finished = true
    window.clearTimeout(revealTimer)
    animation?.cancel()
    node.remove()
    target.classList.remove('is-window-morphing')
    target.classList.remove('is-window-revealing')
  }
  if (animation) {
    animation.addEventListener('finish', finish)
    animation.addEventListener('cancel', cleanup)
  } else {
    cleanup()
  }
  return cleanup
}

/** 收起：窗口立刻隐藏，形变层独立缩回任务条，再让调用方关闭窗口。 */
export function playIslandWindowCollapse(target: HTMLElement | null): void {
  if (!target || typeof window === 'undefined' || typeof target.animate !== 'function') return
  if (window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return

  const to = islandShape()
  if (!to) return
  const from = shapeOf(target)
  if (from.rect.width < 2 || from.rect.height < 2) return

  target.classList.add('is-window-morphing')
  const node = mount(from, to, COLLAPSE_MS)
  const animation = node.getAnimations()[0]
  const finish = () => {
    node.remove()
    target.classList.remove('is-window-morphing')
  }
  if (animation) {
    animation.addEventListener('finish', finish)
    animation.addEventListener('cancel', finish)
  } else {
    finish()
  }
}
