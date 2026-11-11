import type { ThemeMode } from '../lib/theme'
import Icon from './Icon'

/**
 * 顶栏的明暗切换：图标显示“切过去之后”的样子，点击只切当前主题。
 * 两套配色已经分开保存，所以这里不会覆盖用户调好的另一套。
 */
export default function ThemeToggle({
  theme,
  onToggle,
  busy = false,
}: {
  theme: ThemeMode
  onToggle: () => void
  busy?: boolean
}) {
  const next = theme === 'light' ? 'dark' : 'light'
  const label = next === 'light' ? '切换到亮色主题' : '切换到暗色主题'
  return (
    <button
      type="button"
      className="icon-btn theme-toggle"
      onClick={onToggle}
      disabled={busy}
      title={label}
      aria-label={label}
    >
      <Icon name={next === 'light' ? 'sun' : 'moon'} size={15} />
    </button>
  )
}
