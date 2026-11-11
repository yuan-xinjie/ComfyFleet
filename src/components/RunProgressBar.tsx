import type { RunProgress } from '../lib/runProgress'

export default function RunProgressBar({
  progress,
  className = '',
}: {
  progress: RunProgress
  className?: string
}) {
  const percent =
    progress.max > 0
      ? Math.round(Math.max(0, Math.min(100, (progress.value / progress.max) * 100)))
      : 0
  return (
    <div className={`run-progress ${className}`.trim()}>
      <div className="run-progress-line">
        <span className="run-progress-what">{progress.what}</span>
        <span className="run-progress-speed">{progress.speed ?? '—'}</span>
      </div>
      <div
        className={`run-progress-track${progress.max > 0 ? '' : ' is-indeterminate'}`}
        role="progressbar"
        aria-label="ComfyUI 任务进度"
        aria-valuemin={0}
        aria-valuemax={progress.max > 0 ? 100 : undefined}
        aria-valuenow={progress.max > 0 ? percent : undefined}
      >
        <span
          className="run-progress-fill"
          style={{ width: progress.max > 0 ? `${percent}%` : undefined }}
        />
      </div>
    </div>
  )
}
