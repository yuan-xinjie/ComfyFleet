/** 日志容器是否停在最后 lines 行内：按实际行高算，字体或缩放入口变化时阈值跟着变。 */
export function isNearLogBottom(el: HTMLElement, lines = 3): boolean {
  const style = getComputedStyle(el)
  const lineHeight = Number.parseFloat(style.lineHeight)
  const fontSize = Number.parseFloat(style.fontSize)
  const row =
    Number.isFinite(lineHeight) && lineHeight > 0
      ? lineHeight
      : (Number.isFinite(fontSize) && fontSize > 0 ? fontSize : 13) * 1.6
  return el.scrollHeight - el.scrollTop - el.clientHeight <= row * lines
}
