import { useEffect, useRef } from 'react'

/** 实例页里可保存页签登记给底栏的动作 */
export interface TabSaveHandle {
  dirty: boolean
  busy: boolean
  /** 表单不完整，底栏显示原因并禁用保存。 */
  invalid?: boolean
  save: () => Promise<void>
  discard: () => void
}

/**
 * 把页面自己的保存/放弃登记到实例底栏。
 * save / discard 每次渲染都会变，用 ref 取最新版本，effect 只跟校验 / dirty / busy 走，
 * 避免父级 setState 反过来触发子级 effect 的死循环。
 *
 * 隐藏的页签也会登记：父级按页签保存句柄，切页签后仍能阻止丢草稿。
 * `active` 不再参与登记，页签显示由父级决定。
 */
export function useTabSave(
  onSaveState: ((state: TabSaveHandle | null) => void) | undefined,
  dirty: boolean,
  busy: boolean,
  save: () => Promise<void>,
  discard: () => void,
  invalid = false,
) {
  const latest = useRef({ save, discard })
  latest.current = { save, discard }

  useEffect(() => {
    if (!onSaveState) return
    onSaveState({
      dirty,
      busy,
      invalid,
      save: () => latest.current.save(),
      discard: () => latest.current.discard(),
    })
    return () => onSaveState(null)
  }, [onSaveState, dirty, busy, invalid])
}
