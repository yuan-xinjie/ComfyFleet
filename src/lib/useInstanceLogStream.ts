import { useEffect, useState } from 'react'
import { api, errText } from './api'
import type { InstanceLogEvent } from '../types'

/**
 * 直接订阅 ComfyUI 进程（python main.py）的输出。
 *
 * 只在一开始取一次「尾部 + 序号」快照，之后靠 `instance-log` 事件逐行追加；
 * 不再按秒拉日志文件。事件序号是全局递增的，重启实例也不会归零。
 */
export function useInstanceLogStream(instanceId: string, active: boolean, limit = 300) {
  const [lines, setLines] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!active) return
    let alive = true
    let unlisten: (() => void) | null = null
    let ready = false
    let baseSeq = 0
    let pending: InstanceLogEvent[] = []

    setLines([])
    setError(null)

    const append = (incoming: string[]) => {
      if (!incoming.length) return
      setLines((prev) => {
        const next = [...prev, ...incoming]
        return next.length > limit ? next.slice(next.length - limit) : next
      })
    }

    const onEvent = (event: InstanceLogEvent) => {
      if (!alive || event.instanceId !== instanceId) return
      if (!ready) {
        pending.push(event)
        return
      }
      if (event.seq <= baseSeq) return
      baseSeq = event.seq
      append([event.line])
    }

    // 先注册监听，再取快照：中间到达的行先放进 pending，不会漏。
    void api
      .onInstanceLog(onEvent)
      .then(async (un) => {
        if (!alive) {
          un()
          return
        }
        unlisten = un
        try {
          const snapshot = await api.instanceLogState(instanceId, limit)
          if (!alive) return
          setLines(snapshot.lines)
          baseSeq = snapshot.seq
          ready = true
          const fresh = pending.filter((event) => event.seq > baseSeq)
          pending = []
          if (fresh.length) {
            baseSeq = fresh[fresh.length - 1].seq
            append(fresh.map((event) => event.line))
          }
        } catch (e) {
          if (!alive) return
          setError(errText(e))
          // 快照失败也继续用事件流，不能把后到的行一直压在 pending 里。
          ready = true
          const fresh = pending
          pending = []
          if (fresh.length) {
            baseSeq = fresh[fresh.length - 1].seq
            append(fresh.map((event) => event.line))
          }
        }
      })
      .catch((e) => {
        if (alive) setError(errText(e))
      })

    return () => {
      alive = false
      unlisten?.()
    }
  }, [active, instanceId, limit])

  return {
    lines,
    error,
    clear: () => setLines([]),
  }
}
