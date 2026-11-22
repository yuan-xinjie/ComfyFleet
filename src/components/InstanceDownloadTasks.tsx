import { useMemo } from 'react'
import type { InstanceConfig } from '../types'
import {
  deriveLogTask,
  isDownloadStage,
  latestProgress,
  progressFromStructured,
  stageLabel,
} from '../lib/runProgress'
import { useInstanceLogStream } from '../lib/useInstanceLogStream'
import { useInstanceProgress } from '../lib/useInstanceProgress'
import { useTask } from './BackgroundTasks'

function InstanceDownloadTask({
  inst,
  onOpen,
}: {
  inst: InstanceConfig
  onOpen: (id: string) => void
}) {
  const remote = inst.kind === 'remote' || Boolean(inst.remote)
  const running = inst.state === 'running' && !remote
  const { lines } = useInstanceLogStream(inst.id, running, 200)
  const { progress: structuredProgress } = useInstanceProgress(inst.id, running)
  const logTask = useMemo(() => deriveLogTask(lines), [lines])
  const structured = useMemo(() => {
    const latest = latestProgress(structuredProgress)
    return latest ? progressFromStructured(latest) : null
  }, [structuredProgress])
  const stage = structured?.stage ?? logTask.stage
  const downloading = running && isDownloadStage(stage)
  const task = downloading ? (structured?.task ?? logTask.task) : null
  const percent =
    task && task.max > 0
      ? Math.round(Math.max(0, Math.min(100, (task.value / task.max) * 100)))
      : undefined

  useTask(
    downloading
      ? {
          id: `download-${inst.id}`,
          title: stageLabel(stage),
          phase: task?.what ?? stageLabel(stage),
          state: 'running',
          percent,
          speed: task?.speed ?? null,
          log: lines[lines.length - 1] ?? '',
          kind: 'download',
          onOpen: () => onOpen(inst.id),
        }
      : null,
  )

  return null
}

/**
 * ComfyUI 执行工作流时可能自动下载缺失模型。这个监听只负责把“下载中”登记到
 * 全局灵动岛；实例卡片自己用同一份日志解析，避免两套判断。
 */
export default function InstanceDownloadTasks({
  instances,
  onOpen,
}: {
  instances: InstanceConfig[]
  onOpen: (id: string) => void
}) {
  return (
    <>
      {instances.map((inst) => (
        <InstanceDownloadTask key={inst.id} inst={inst} onOpen={onOpen} />
      ))}
    </>
  )
}
