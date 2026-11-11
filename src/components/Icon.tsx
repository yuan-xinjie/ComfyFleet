import type { CSSProperties, ReactElement } from 'react'

/**
 * 一套自绘图标：24 网格、1.75 描边、圆头圆角。
 * 只在这里加图标，界面里不要用 emoji / 符号代替。
 */
export type IconName =
  | 'overview'
  | 'console'
  | 'nodes'
  | 'versions'
  | 'diagnostics'
  | 'settings'
  | 'play'
  | 'stop'
  | 'globe'
  | 'refresh'
  | 'folder'
  | 'copy'
  | 'trash'
  | 'plus'
  | 'check'
  | 'alert'
  | 'info'
  | 'chevron'
  | 'box'
  | 'clock'
  | 'chip'
  | 'download'
  | 'send'
  | 'power'
  | 'back'
  | 'close'
  | 'sun'
  | 'moon'
  | 'eye'
  | 'eye-off'
  | 'file'

const PATHS: Record<IconName, ReactElement> = {
  overview: (
    <>
      <rect x="3.5" y="3.5" width="7" height="7" rx="2" />
      <rect x="13.5" y="3.5" width="7" height="7" rx="2" />
      <rect x="3.5" y="13.5" width="7" height="7" rx="2" />
      <rect x="13.5" y="13.5" width="7" height="7" rx="2" />
    </>
  ),
  console: (
    <>
      <rect x="3" y="4.5" width="18" height="15" rx="3" />
      <path d="M7.5 9.5 10 12l-2.5 2.5M12.5 15h4" />
    </>
  ),
  nodes: (
    <>
      <path d="M12 3.5 20 8v8l-8 4.5L4 16V8l8-4.5Z" />
      <path d="M12 12 20 8M12 12v8.5M12 12 4 8" />
    </>
  ),
  versions: (
    <>
      <path d="M20 12.5 12.5 20a2 2 0 0 1-2.8 0L4 14.3V4h10.3l5.7 5.7a2 2 0 0 1 0 2.8Z" />
      <circle cx="8.5" cy="8.5" r="1.3" />
    </>
  ),
  diagnostics: (
    <>
      <path d="M3.5 12h3.2l2-5.2 3.4 10.4 2.2-5.2h6.2" />
    </>
  ),
  settings: (
    <>
      <circle cx="12" cy="12" r="3.2" />
      <path d="M19.4 14.4a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-2.7 1.1v.2a2 2 0 1 1-4 0v-.1a1.6 1.6 0 0 0-2.8-1.1l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.6 1.6 0 0 0-1.1-2.7h-.2a2 2 0 1 1 0-4h.1a1.6 1.6 0 0 0 1.1-2.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.6 1.6 0 0 0 2.7-1.1v-.2a2 2 0 1 1 4 0v.1a1.6 1.6 0 0 0 2.8 1.1l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.6 1.6 0 0 0 1.1 2.7h.2a2 2 0 1 1 0 4h-.1a1.6 1.6 0 0 0-1.5 1Z" />
    </>
  ),
  play: <path d="M8 5.2v13.6a1 1 0 0 0 1.53.85l10.2-6.8a1 1 0 0 0 0-1.7L9.53 4.35A1 1 0 0 0 8 5.2Z" />,
  stop: <rect x="6" y="6" width="12" height="12" rx="2.5" />,
  globe: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M3.5 12h17M12 3.5c2.4 2.3 3.6 5.2 3.6 8.5S14.4 18.2 12 20.5c-2.4-2.3-3.6-5.2-3.6-8.5S9.6 5.8 12 3.5Z" />
    </>
  ),
  refresh: (
    <>
      <path d="M20 11a8 8 0 1 0-2.6 6" />
      <path d="M20 4.5V11h-6.4" />
    </>
  ),
  folder: (
    <>
      <path d="M3.5 7.5a2 2 0 0 1 2-2h3.2l2 2.2h6.8a2 2 0 0 1 2 2v7.8a2 2 0 0 1-2 2h-12a2 2 0 0 1-2-2V7.5Z" />
    </>
  ),
  copy: (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2.5" />
      <path d="M15 6.5V6a2.5 2.5 0 0 0-2.5-2.5H6A2.5 2.5 0 0 0 3.5 6v6.5A2.5 2.5 0 0 0 6 15h.5" />
    </>
  ),
  trash: (
    <>
      <path d="M4.5 7h15M9.5 7V5.5a1.5 1.5 0 0 1 1.5-1.5h2a1.5 1.5 0 0 1 1.5 1.5V7" />
      <path d="M6.5 7l.8 11a2 2 0 0 0 2 1.9h5.4a2 2 0 0 0 2-1.9L17.5 7" />
    </>
  ),
  plus: <path d="M12 5.5v13M5.5 12h13" />,
  check: <path d="m4.8 12.5 4.6 4.5L19.2 7" />,
  alert: (
    <>
      <path d="M12 4.5 21 19.5H3L12 4.5Z" />
      <path d="M12 10v4.2M12 17.2h.01" />
    </>
  ),
  info: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 11v5.2M12 7.8h.01" />
    </>
  ),
  chevron: <path d="m6 9.5 6 6 6-6" />,
  box: (
    <>
      <path d="M20 8.2v7.6a1.6 1.6 0 0 1-.8 1.4l-6.4 3.6a1.6 1.6 0 0 1-1.6 0l-6.4-3.6a1.6 1.6 0 0 1-.8-1.4V8.2" />
      <path d="m4 8.2 8 4.5 8-4.5M12 12.7v7.5" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.5V12l3 2" />
    </>
  ),
  chip: (
    <>
      <rect x="6.5" y="6.5" width="11" height="11" rx="2.5" />
      <path d="M10 3.5v3M14 3.5v3M10 17.5v3M14 17.5v3M3.5 10h3M3.5 14h3M17.5 10h3M17.5 14h3" />
    </>
  ),
  download: (
    <>
      <path d="M12 4v10.5M7.5 10.5 12 15l4.5-4.5" />
      <path d="M4.5 17.5v1a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2v-1" />
    </>
  ),
  send: (
    <>
      <path d="M4.5 11.6 20 4.5l-7.1 15.5-2.7-6.2-5.7-2.2Z" />
      <path d="m9.9 13.8 4.3-4.3" />
    </>
  ),
  power: (
    <>
      <path d="M12 3.5v8" />
      <path d="M7 6.5a7.5 7.5 0 1 0 10 0" />
    </>
  ),
  back: <path d="M14.5 5.5 8 12l6.5 6.5" />,
  close: <path d="m6.5 6.5 11 11M17.5 6.5l-11 11" />,
  sun: (
    <>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M18.5 5.5l-1.7 1.7M7.2 16.8l-1.7 1.7" />
    </>
  ),
  moon: <path d="M20 14.4A8.2 8.2 0 0 1 9.6 4a8.4 8.4 0 1 0 10.4 10.4Z" />,
  eye: (
    <>
      <path d="M2.8 12S6.3 5.8 12 5.8 21.2 12 21.2 12 17.7 18.2 12 18.2 2.8 12 2.8 12Z" />
      <circle cx="12" cy="12" r="3.1" />
    </>
  ),
  'eye-off': (
    <>
      <path d="m4 4 16 16" />
      <path d="M10.4 6.1A10 10 0 0 1 12 6c5.7 0 9.2 6 9.2 6a17 17 0 0 1-2.8 3.6M6.3 7.3A17 17 0 0 0 2.8 12S6.3 18 12 18c1.5 0 2.8-.4 4-1.1" />
      <path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" />
    </>
  ),
  file: (
    <>
      <path d="M6.5 3.5h7l4 4v13h-11z" />
      <path d="M13.5 3.5v4h4" />
    </>
  ),
}

interface Props {
  name: IconName
  size?: number
  style?: CSSProperties
  className?: string
}

export default function Icon({ name, size = 16, style, className }: Props) {
  return (
    <svg
      className={`icon${className ? ` ${className}` : ''}`}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.75}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
      style={style}
    >
      {PATHS[name]}
    </svg>
  )
}
