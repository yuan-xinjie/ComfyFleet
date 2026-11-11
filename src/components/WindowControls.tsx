import { useEffect, useState } from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'

const iconProps = {
  viewBox: '0 0 12 12',
  width: 12,
  height: 12,
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 1.2,
  strokeLinecap: 'round' as const,
  strokeLinejoin: 'round' as const,
  'aria-hidden': true,
}

function MinimizeIcon() {
  return (
    <svg {...iconProps}>
      <path d="M2.5 6h7" />
    </svg>
  )
}

function MaximizeIcon() {
  return (
    <svg {...iconProps}>
      <rect x="2.5" y="2.5" width="7" height="7" rx="1" />
    </svg>
  )
}

function RestoreIcon() {
  return (
    <svg {...iconProps}>
      <path d="M4.4 3.4h4.2v4.2" />
      <rect x="2.6" y="4.6" width="4.8" height="4.8" rx="1" />
    </svg>
  )
}

function CloseIcon() {
  return (
    <svg {...iconProps}>
      <path d="M3.2 3.2l5.6 5.6M8.8 3.2l-5.6 5.6" />
    </svg>
  )
}

/** 自绘窗口按钮：贴在页头右端，替代系统标题栏 */
export default function WindowControls() {
  const win = getCurrentWindow()
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    let alive = true
    const sync = () => {
      void win
        .isMaximized()
        .then((next) => {
          if (alive) setMaximized(next)
        })
        .catch(() => {})
    }
    sync()
    const unlisten = win.onResized(() => sync())
    return () => {
      alive = false
      void unlisten.then((off) => off()).catch(() => {})
    }
  }, [win])

  return (
    <div className="win-controls" data-tauri-drag-region="false">
      <button
        className="win-btn"
        type="button"
        title="最小化"
        aria-label="最小化"
        onClick={() => void win.minimize()}
      >
        <MinimizeIcon />
      </button>
      <button
        className="win-btn"
        type="button"
        title={maximized ? '还原' : '最大化'}
        aria-label={maximized ? '还原' : '最大化'}
        onClick={() => void win.toggleMaximize()}
      >
        {maximized ? <RestoreIcon /> : <MaximizeIcon />}
      </button>
      <button
        className="win-btn close"
        type="button"
        title="关闭到托盘"
        aria-label="关闭到托盘"
        onClick={() => void win.close()}
      >
        <CloseIcon />
      </button>
    </div>
  )
}
