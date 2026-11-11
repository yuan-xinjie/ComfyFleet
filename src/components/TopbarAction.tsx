import Icon, { type IconName } from './Icon'

/**
 * 顶栏右侧的带文字操作：图标只做辅助，文字把“同步 / 设置”说清楚。
 * 仍保留完整 title 和 aria-label，窄窗口下也不丢语义。
 */
export default function TopbarAction({
  icon,
  label,
  title,
  onClick,
}: {
  icon: IconName
  label: string
  title: string
  onClick: () => void
}) {
  return (
    <button type="button" className="topbar-action" onClick={onClick} title={title} aria-label={title}>
      <Icon name={icon} size={14} />
      <span>{label}</span>
    </button>
  )
}
