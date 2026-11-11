/** 实例页签与状态文案：左栏、详情页、首页共用，避免互相 import */
export type InstTab =
  | 'overview'
  | 'console'
  | 'versions'
  | 'launch'
  | 'storage'
  | 'snapshots'
  | 'activity'
  | 'settings'

export const TABS: { id: InstTab; label: string; key: string }[] = [
  { id: 'overview', label: '概览', key: '1' },
  { id: 'console', label: '终端', key: '2' },
  { id: 'versions', label: '版本', key: '3' },
  { id: 'launch', label: '启动参数', key: '4' },
  { id: 'storage', label: '存储', key: '5' },
  { id: 'snapshots', label: '快照', key: '6' },
  { id: 'activity', label: '日志', key: '7' },
  { id: 'settings', label: '设置', key: '8' },
]

export type Tone = 'ok' | 'warn' | 'err' | 'busy' | 'idle'

/** 状态 → 语义色（只在这里定义一次） */
export function stateTone(state: string): Tone {
  switch (state) {
    case 'running':
    case 'ready':
      return 'ok'
    case 'starting':
    case 'stopping':
    case 'installing':
    case 'updating':
      return 'busy'
    case 'error':
      return 'err'
    case 'stopped':
      return 'idle'
    default:
      return 'warn'
  }
}

const STATE_TEXT: Record<string, string> = {
  running: '运行中',
  ready: '就绪',
  starting: '启动中',
  stopping: '停止中',
  installing: '安装中',
  updating: '更新中',
  stopped: '已停止',
  error: '出错',
  uninitialized: '未装环境',
}

export function stateText(state: string): string {
  return STATE_TEXT[state] ?? state
}
