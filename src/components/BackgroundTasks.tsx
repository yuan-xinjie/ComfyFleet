import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import type { OperationState } from './OperationDialog'

export type TaskKind =
  | 'generate'
  | 'download'
  | 'start'
  | 'install'
  | 'wheel'
  | 'version'
  | 'node'
  | 'snapshot'
  | 'sync'
  | 'other'

export interface BackgroundTask {
  id: string
  title: string
  phase: string
  state: OperationState
  percent?: number
  /** 步数、下载量这类不占主标题的进度信息。 */
  detail?: string
  speed?: string | null
  /** 最近一行控制台输出；任务条悬浮展开时显示。 */
  log?: string
  kind?: TaskKind
  onOpen: () => void
}

export interface TaskRecord extends BackgroundTask {
  /** 任务第一次进入任务中心的时间；更新进度不会改变它。 */
  enteredAt: number
}

/** 从安装/下载日志里取人类可读的传输速度。 */
export function speedFromLog(line?: string): string | undefined {
  const match = line?.match(/([\d.]+)\s*(GiB|MiB|KiB|GB|MB|KB)\/s/i)
  return match ? `${match[1]} ${match[2]}/s` : undefined
}

interface TaskRegistry {
  upsert: (task: BackgroundTask) => void
  remove: (id: string) => void
}

const RegistryContext = createContext<TaskRegistry | null>(null)
const TasksContext = createContext<TaskRecord[]>([])

/** 成功和取消只短暂停留；失败保留，等用户打开对应的详情。 */
const DEFAULT_SUCCESS_LINGER_MS = 3000

export function BackgroundTasksProvider({
  children,
  successLingerMs = DEFAULT_SUCCESS_LINGER_MS,
}: {
  children: ReactNode
  successLingerMs?: number
}) {
  const [tasks, setTasks] = useState<TaskRecord[]>([])
  const timers = useRef(new Map<string, number>())

  const lingerRef = useRef(successLingerMs)
  lingerRef.current = successLingerMs

  const clearTimer = useCallback((id: string) => {
    const timer = timers.current.get(id)
    if (timer !== undefined) {
      window.clearTimeout(timer)
      timers.current.delete(id)
    }
  }, [])

  const remove = useCallback(
    (id: string) => {
      clearTimer(id)
      setTasks((prev) => prev.filter((task) => task.id !== id))
    },
    [clearTimer],
  )

  const upsert = useCallback(
    (task: BackgroundTask) => {
      setTasks((prev) => {
        const index = prev.findIndex((item) => item.id === task.id)
        if (index < 0) return [...prev, { ...task, enteredAt: Date.now() }]
        const next = prev.slice()
        next[index] = { ...next[index], ...task }
        return next
      })

      clearTimer(task.id)
      if (task.state === 'ok' || task.state === 'cancelled') {
        timers.current.set(
          task.id,
          window.setTimeout(() => {
            timers.current.delete(task.id)
            setTasks((prev) => prev.filter((item) => item.id !== task.id))
          }, lingerRef.current),
        )
      }
    },
    [clearTimer],
  )

  useEffect(() => {
    const current = timers.current
    return () => {
      for (const timer of current.values()) window.clearTimeout(timer)
      current.clear()
    }
  }, [])

  const registry = useMemo(() => ({ upsert, remove }), [upsert, remove])

  return (
    <RegistryContext.Provider value={registry}>
      <TasksContext.Provider value={tasks}>{children}</TasksContext.Provider>
    </RegistryContext.Provider>
  )
}

/**
 * 把长操作登记到全局任务条。
 * 后台操作只在弹窗隐藏时登记；实例生成任务在实例页持续登记。
 */
export function useTask(task: BackgroundTask | null) {
  const registry = useContext(RegistryContext)
  const openRef = useRef(task?.onOpen)
  openRef.current = task?.onOpen
  const id = task?.id
  const title = task?.title
  const phase = task?.phase
  const state = task?.state
  const percent = task?.percent
  const detail = task?.detail
  const speed = task?.speed
  const log = task?.log
  const kind = task?.kind

  useEffect(() => {
    if (!registry || !id || !title || !state) return
    registry.upsert({
      id,
      title,
      phase: phase ?? '',
      state,
      percent,
      detail,
      speed,
      log,
      kind,
      onOpen: () => openRef.current?.(),
    })
  }, [registry, id, title, phase, state, percent, detail, speed, log, kind])

  useEffect(() => {
    if (!registry || !id) return
    return () => registry.remove(id)
  }, [registry, id])
}

/** 保留旧名字，避免每个后台任务调用点都改一遍。 */
export const useBackgroundTask = useTask

export function useTaskList(): TaskRecord[] {
  return useContext(TasksContext)
}
