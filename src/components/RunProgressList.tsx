import RunProgressBar from './RunProgressBar'
import type { RunProgress } from '../lib/runProgress'

/** 控制台底部并排显示当前所有活跃进度；没有进度时整块不占高度。 */
export default function RunProgressList({
  progresses,
  className = '',
}: {
  progresses: RunProgress[]
  className?: string
}) {
  if (progresses.length === 0) return null
  return (
    <div className={`run-progress-list ${className}`.trim()}>
      {progresses.map((progress, index) => (
        <RunProgressBar
          key={progress.id ?? `${progress.label ?? progress.what}-${index}`}
          progress={progress}
          className="run-progress-item"
        />
      ))}
    </div>
  )
}
