import { useEffect, useRef, useState } from 'react'
import { api, errText } from './api'
import type { InstanceProgressEvent, PythonProgress } from '../types'

/**
 * 订阅 Python 进度桥的结构化进度。
 *
 * 和日志流一样：先注册监听，再取一次快照；事件带 rev，旧事件不会覆盖新快照。
 * 实例不在运行时直接清空，避免停止后遗留上一轮的进度条。
 */
export function useInstanceProgress(instanceId: string, active: boolean) {
  const [progress, setProgress] = useState<PythonProgress[]>([])
  const [error, setError] = useState<string | null>(null)
  const revRef = useRef(0)

  useEffect(() => {
    if (!active) {
      revRef.current = 0
      setProgress([])
      setError(null)
      return
    }

    let alive = true
    let unlisten: (() => void) | null = null
    let ready = false
    let pending: InstanceProgressEvent[] = []

    const apply = (event: InstanceProgressEvent) => {
      if (event.instanceId !== instanceId) return
      if (event.rev < revRef.current) return
      revRef.current = event.rev
      setProgress(event.progress)
    }

    const onEvent = (event: InstanceProgressEvent) => {
      if (!alive) return
      if (!ready) {
        pending.push(event)
        return
      }
      apply(event)
    }

    void api
      .onInstanceProgress(onEvent)
      .then(async (un) => {
        if (!alive) {
          un()
          return
        }
        unlisten = un
        try {
          const snapshot = await api.instanceProgressState(instanceId)
          if (!alive) return
          ready = true
          if (snapshot.rev >= revRef.current) {
            revRef.current = snapshot.rev
            setProgress(snapshot.progress)
          }
          for (const event of pending) apply(event)
          pending = []
        } catch (e) {
          if (!alive) return
          ready = true
          setError(errText(e))
          for (const event of pending) apply(event)
          pending = []
        }
      })
      .catch((e) => {
        if (alive) setError(errText(e))
      })

    return () => {
      alive = false
      unlisten?.()
    }
  }, [active, instanceId])

  return { progress, error }
}
