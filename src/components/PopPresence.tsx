import { useEffect, useRef, useState, type ReactNode } from 'react'

/** 底栏操作按钮的出现/收起共用一个短弹出，避免布局突然跳一下。 */
const EXIT_MS = 140

export function PopPresence({
  show,
  children,
}: {
  show: boolean
  children: ReactNode
}) {
  const [mounted, setMounted] = useState(show)
  const [leaving, setLeaving] = useState(false)
  const timer = useRef<number | null>(null)

  useEffect(() => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current)
      timer.current = null
    }
    if (show) {
      setMounted(true)
      setLeaving(false)
      return
    }
    if (!mounted) return
    setLeaving(true)
    timer.current = window.setTimeout(() => {
      timer.current = null
      setLeaving(false)
      setMounted(false)
    }, EXIT_MS)
    return () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current)
        timer.current = null
      }
    }
  }, [show, mounted])

  if (!mounted) return null
  return (
    <span className={`pop-presence${leaving ? ' is-leaving' : ''}`}>{children}</span>
  )
}
