import { useCallback, useEffect, useState } from 'react'
import { createPortal } from 'react-dom'
import { api, errText } from '../lib/api'
import type {
  DepsInstallReport,
  NodeUpdate,
  DepsReport,
  InstanceConfig,
  NodeDeps,
  NodeInfo,
  VersionEvent,
} from '../types'
import Icon from '../components/Icon'
import Select from '../components/Select'
import OperationDialog, {
  operationStageLabel,
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
  /** 诊断弹窗里点「补依赖」时传进来的目标（null = 全部） */
  installIntent: { names: string[] | null; key: number } | null
  onDiagnose: () => void
  onChanged?: () => void
  onOpenSnapshots?: () => void
}

const AUTO_LABEL: Record<string, string> = {
  remind: '提醒我',
  auto: '自动补',
  off: '关闭',
}

interface NodeOperation {
  title: string
  target: string
  source: string
  state: 'running' | 'ok' | 'failed'
  phase: string
  events: OperationLine[]
  tab: OperationTab
  error: string | null
}

/**
 * 节点管理：安装 / 启停用 / 更新 / 卸载；以及「依赖」这条线——
 * 检测缺什么、一键补上（自己拷进 custom_nodes 的节点也能补）。
 */
export default function Nodes({ inst, notify, installIntent, onDiagnose, onChanged, onOpenSnapshots }: Props) {
  const instanceId = inst.id
  const running = inst.state === 'starting' || inst.state === 'running'

  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [deps, setDeps] = useState<DepsReport | null>(null)
  const [depsBusy, setDepsBusy] = useState(false)
  const [updates, setUpdates] = useState<Record<string, NodeUpdate>>({})
  const [checking, setChecking] = useState(false)
  const [busy, setBusy] = useState<string | null>(null)
  const [nodeOp, setNodeOp] = useState<NodeOperation | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)

  const [url, setUrl] = useState('')
  const [withReq, setWithReq] = useState(true)
  const [confirmDel, setConfirmDel] = useState<string | null>(null)
  const [addOpen, setAddOpen] = useState(false)

  // 补依赖弹层
  const [confirm, setConfirm] = useState<{ names: string[] | null } | null>(null)
  const [runInstallPy, setRunInstallPy] = useState(true)
  const [installing, setInstalling] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [depEvents, setDepEvents] = useState<OperationLine[]>([])
  const [depPhase, setDepPhase] = useState('准备中')
  const [depTab, setDepTab] = useState<OperationTab>('detail')
  const [report, setReport] = useState<DepsInstallReport | null>(null)
  const [depsError, setDepsError] = useState<string | null>(null)

  const refreshDeps = useCallback(async (): Promise<DepsReport | null> => {
    setDepsBusy(true)
    try {
      const next = await api.nodesDepsStatus(instanceId)
      setDeps(next)
      return next
    } catch {
      // 检测失败不该盖住节点列表本身（比如没有 venv）
      setDeps(null)
      return null
    } finally {
      setDepsBusy(false)
    }
  }, [instanceId])

  /** 检查 git 节点在远端有没有新提交：只有「有新版本」才允许点更新 */
  const refreshUpdates = useCallback(
    async (list: NodeInfo[], names?: string[]): Promise<Record<string, NodeUpdate>> => {
      const wanted = names ? new Set(names) : null
      const targets = list.filter(
        (n) => n.kind === 'git' && (!wanted || wanted.has(n.name)),
      )
      if (targets.length === 0) {
        if (!wanted) setUpdates({})
        return {}
      }
      setChecking(true)
      try {
        const rows = await api.nodesCheckUpdates(instanceId, names ?? null)
        const map: Record<string, NodeUpdate> = {}
        rows.forEach((r) => {
          map[r.name] = r
        })
        setUpdates((prev) => (wanted ? { ...prev, ...map } : map))
        return map
      } catch {
        // 检查失败不打扰，但按钮别锁死：给每个 git 节点标一条「检查失败」，仍可手动更新
        const failed: Record<string, NodeUpdate> = {}
        targets.forEach((n) => {
          failed[n.name] = {
            name: n.name,
            behind: 0,
            local: null,
            remote: null,
            error: '检查更新失败（网络或权限）',
          }
        })
        setUpdates((prev) => (wanted ? { ...prev, ...failed } : failed))
        return failed
      } finally {
        setChecking(false)
      }
    },
    [instanceId],
  )

  const reloadAll = useCallback(
    async (checkNames?: string[], checkUpdates = true) => {
      let list: NodeInfo[] = []
      try {
        list = await api.nodesList(instanceId)
        setNodes(list)
        setError(null)
      } catch (e) {
        setError(errText(e))
        setNodes([])
      } finally {
        setLoading(false)
      }
      await Promise.all([
        refreshDeps(),
        checkUpdates ? refreshUpdates(list, checkNames) : Promise.resolve({}),
      ])
    },
    [instanceId, refreshDeps, refreshUpdates],
  )

  useEffect(() => {
    void reloadAll()
  }, [reloadAll])

  useEffect(() => {
    if (installIntent) void openInstall(installIntent.names)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [installIntent])

  const pushNodeEvent = (ev: VersionEvent) => {
    setNodeOp((prev) =>
      prev
        ? {
            ...prev,
            phase: ev.stage === 'console' ? prev.phase : operationStageLabel(ev.stage),
            events: [
              ...prev.events,
              {
                at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
                step: operationStageLabel(ev.stage),
                phase: ev.failed ? 'err' : ev.level === 'warn' ? 'warn' : ev.done ? 'done' : 'log',
                message: ev.message,
              },
            ].slice(-600),
          }
        : prev,
    )
  }
  const pushDepEvent = (ev: VersionEvent) => {
    setDepEvents((prev) =>
      [
        ...prev,
        {
          at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
          step: operationStageLabel(ev.stage),
          phase: ev.failed ? 'err' : ev.level === 'warn' ? 'warn' : ev.done ? 'done' : 'log',
          message: ev.message,
        },
      ].slice(-1200),
    )
    if (ev.stage !== 'console') setDepPhase(operationStageLabel(ev.stage))
  }

  const install = async () => {
    if (!url.trim()) return
    setBusy('install')
    setNodeOp({
      title: '安装节点',
      target: url.trim(),
      source: url.trim(),
      state: 'running',
      phase: '准备中',
      events: [],
      tab: 'detail',
      error: null,
    })
    try {
      const node = await api.nodesInstall(instanceId, url.trim(), null, withReq, pushNodeEvent)
      setNodeOp((prev) =>
        prev ? { ...prev, target: node.name, state: 'ok', phase: '安装完成' } : prev,
      )
      notify('ok', `已安装 ${node.name}，重启 ComfyUI 生效`)
      setUrl('')
      setAddOpen(false)
      // 只检查新装的这个节点，别为了它把已有节点全部重新 fetch 一遍。
      await reloadAll([node.name])
    } catch (e) {
      const text = errText(e)
      setNodeOp((prev) =>
        prev ? { ...prev, state: 'failed', phase: '安装失败', error: text } : prev,
      )
      notify('err', text)
    } finally {
      setBusy(null)
    }
  }

  const toggle = async (node: NodeInfo) => {
    setBusy(node.name)
    try {
      await api.nodesToggle(instanceId, node.name, !node.enabled)
      notify('ok', `${node.name} 已${node.enabled ? '禁用' : '启用'}（重启 ComfyUI 生效）`)
      await reloadAll(undefined, false)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(null)
    }
  }

  const applyFreshUpdate = (fresh: NodeUpdate) => {
    setUpdates((prev) => ({ ...prev, [fresh.name]: fresh }))
    const local = fresh.local
    if (local) {
      setNodes((prev) =>
        prev.map((n) =>
          n.name === fresh.name ? { ...n, commit: local.slice(0, 7) } : n,
        ),
      )
    }
  }

  const update = async (node: NodeInfo) => {
    setBusy(node.name)
    setNodeOp({
      title: '更新节点',
      target: node.name,
      source: node.url ?? node.name,
      state: 'running',
      phase: '准备中',
      events: [],
      tab: 'detail',
      error: null,
    })
    try {
      // 后端 pull 成功后直接返回「更新后的最新状态」，不再等下一次远端检查；
      // 先把「可更新」改成「已最新」，再刷新依赖，避免界面短暂停在旧结论。
      const fresh = await api.nodesUpdate(instanceId, node.name, pushNodeEvent)
      applyFreshUpdate(fresh)
      setNodeOp((prev) => (prev ? { ...prev, state: 'ok', phase: '更新完成' } : prev))
      notify('ok', `${node.name} 已更新（重启 ComfyUI 生效）`)
      await reloadAll(undefined, false)
    } catch (e) {
      const text = errText(e)
      setNodeOp((prev) => (prev ? { ...prev, state: 'failed', phase: '更新失败', error: text } : prev))
      notify('err', text)
    } finally {
      setBusy(null)
    }
  }

  const updateAll = async () => {
    const targets = nodes.filter(
      (n) => n.enabled && n.kind === 'git' && (updates[n.name]?.behind ?? 0) > 0,
    )
    if (targets.length === 0) {
      notify('ok', '所有节点已是最新')
      return
    }
    setBusy('all')
    setNodeOp({
      title: '更新全部节点',
      target: `${targets.length} 个节点`,
      source: '按顺序 git pull',
      state: 'running',
      phase: '准备中',
      events: [],
      tab: 'detail',
      error: null,
    })
    const failed: string[] = []
    try {
      for (let i = 0; i < targets.length; i += 1) {
        const node = targets[i]
        setNodeOp((prev) =>
          prev
            ? { ...prev, target: `${i + 1}/${targets.length} · ${node.name}`, phase: `更新 ${node.name}` }
            : prev,
        )
        pushNodeEvent({
          stage: 'pull',
          message: `▶ ${node.name}（${i + 1}/${targets.length}）`,
          level: 'info',
          done: false,
          failed: false,
        })
        try {
          const fresh = await api.nodesUpdate(instanceId, node.name, pushNodeEvent)
          applyFreshUpdate(fresh)
        } catch (e) {
          const text = errText(e)
          failed.push(node.name)
          pushNodeEvent({
            stage: 'pull',
            message: `${node.name} 更新失败：${text}`,
            level: 'error',
            done: false,
            failed: true,
          })
        }
      }
      const allOk = failed.length === 0
      setNodeOp((prev) =>
        prev
          ? {
              ...prev,
              state: allOk ? 'ok' : 'failed',
              phase: allOk ? '全部更新完成' : `完成，${failed.length} 个失败`,
              error: allOk ? null : `失败：${failed.join('、')}`,
            }
          : prev,
      )
      if (allOk) notify('ok', `${targets.length} 个节点已更新（重启 ComfyUI 生效）`)
      else notify('err', `${failed.length} 个节点更新失败，看弹窗里的输出`)
      await reloadAll(undefined, false)
    } finally {
      setBusy(null)
    }
  }

  const remove = async (node: NodeInfo) => {
    if (confirmDel !== node.name) {
      setConfirmDel(node.name)
      return
    }
    setConfirmDel(null)
    setBusy(node.name)
    try {
      await api.nodesRemove(instanceId, node.name, node.kind !== 'git')
      notify('ok', `已删除 ${node.name}`)
      await reloadAll(undefined, false)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(null)
    }
  }

  const openInstall = async (names: string[] | null) => {
    setReport(null)
    setDepsError(null)
    setDepEvents([])
    setDepPhase('准备中')
    setDepTab('detail')
    setRunInstallPy(true)
    setConfirm({ names })
    if (!deps) await refreshDeps()
  }

  const closeConfirm = () => {
    if (installing) return
    setConfirm(null)
    setReport(null)
    setDepsError(null)
  }

  const installDeps = async (stopFirst: boolean) => {
    if (!confirm) return
    setDepsError(null)
    if (stopFirst) {
      setStopping(true)
      try {
        await api.instanceStop(instanceId)
        onChanged?.()
      } catch (e) {
        setDepsError(errText(e))
        setStopping(false)
        return
      } finally {
        setStopping(false)
      }
    }
    setInstalling(true)
    setDepEvents([])
    setDepPhase('准备中')
    setDepTab('detail')
    setReport(null)
    try {
      // `names=null` 在后端表示“所有节点”，会把几十个本来就不缺依赖的节点
      // 也重新 freeze/plan 一遍，甚至跑它们的 install.py。弹窗里已经筛出
      // 真正要处理的 target，就显式传这些名字，避免空转。
      const names = confirm.names ?? targets.map((d) => d.name)
      const rep = await api.nodesInstallDeps(instanceId, names, { runInstallPy }, pushDepEvent)
      setReport(rep)
      setDepPhase('依赖处理完成')
      const failed = rep.nodes.reduce((n, r) => n + r.failed.length, 0)
      if (failed > 0) notify('err', `依赖处理完成，但有 ${failed} 项没装上，看弹窗里的输出`)
      else notify('ok', '依赖处理完成，重启 ComfyUI 生效')
      // 依赖装完只需要重读节点和包表，不再发一轮 git fetch。
      await reloadAll(undefined, false)
    } catch (e) {
      setDepsError(errText(e))
      setDepPhase('依赖处理失败')
    } finally {
      setInstalling(false)
    }
  }

  const autoMode = inst.nodeDepsAuto ?? 'remind'
  const setAutoMode = async (mode: 'off' | 'remind' | 'auto') => {
    try {
      await api.instanceSetNodeAuto(instanceId, mode)
      notify('ok', `启动时的依赖处理：${AUTO_LABEL[mode] ?? mode}`)
      onChanged?.()
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const enabled = nodes.filter((n) => n.enabled).length
  const updatableCount = nodes.filter(
    (n) => n.enabled && n.kind === 'git' && (updates[n.name]?.behind ?? 0) > 0,
  ).length
  const depsOf = (nodeName: string): NodeDeps | undefined => deps?.nodes.find((d) => d.name === nodeName)

  /** 弹层里要展示的节点：指定名字就按名字，否则只列「有事要做」的 */
  const targets: NodeDeps[] = confirm
    ? confirm.names
      ? (deps?.nodes ?? []).filter((d) => confirm.names!.includes(d.name))
      : (deps?.nodes ?? []).filter((d) => d.pending.length > 0 || d.manual.length > 0)
    : []

  const targetCount = confirm ? (confirm.names ? confirm.names.length : targets.length) : 0

  return (
    <>
      <section className="panel ov-card ov-nodes" aria-labelledby="ov-nodes-title">
        <div className="panel-head ov-nodes-head">
          <div className="ov-nodes-head-main">
            <h2 id="ov-nodes-title">节点</h2>
            {nodes.length > 0 && (
              <span className="badge ok">
                <span className="dot" />
                {enabled === nodes.length ? `${nodes.length} 启用` : `${enabled} / ${nodes.length} 启用`}
              </span>
            )}
            <div className="inline-field ov-nodes-startup" title="启动 ComfyUI 时怎么处理节点依赖">
              <span>启动时</span>
              <Select
                ariaLabel="启动时依赖处理"
                compactMenu
                value={autoMode}
                onChange={(value) => void setAutoMode(value as 'off' | 'remind' | 'auto')}
                options={[
                  { value: 'remind', label: '提醒我' },
                  { value: 'auto', label: '自动补' },
                  { value: 'off', label: '关闭' },
                ]}
              />
            </div>
          </div>
          <div className="ov-nodes-head-actions">
            {updatableCount > 0 && (
              <button
                className="ghost sm"
                onClick={() => void updateAll()}
                disabled={loading || busy !== null || installing || depsBusy || checking}
                aria-label={`更新 ${updatableCount} 个节点`}
                title={
                  busy === 'all'
                    ? '正在按顺序更新节点…'
                    : installing
                      ? '先完成当前依赖处理'
                      : checking
                        ? '正在检查远端最新版本…'
                        : depsBusy
                          ? '正在检测依赖…'
                          : `按顺序更新 ${updatableCount} 个有远端新提交的节点`
                }
              >
                <Icon name="refresh" size={13} />
                <span className="ov-bulk-label">更新 ({updatableCount})</span>
              </button>
            )}
            {deps && deps.pendingNodes > 0 && (
              <button
                className="ghost sm"
                onClick={() => void openInstall(null)}
                disabled={installing || depsBusy}
                aria-label={`补全依赖，${deps.pendingNodes} 个节点`}
                title={`补全 ${deps.pendingNodes} 个节点的缺失依赖`}
              >
                <Icon name="download" size={13} />
                <span className="ov-bulk-label">补全依赖</span>
              </button>
            )}
          </div>
          <div className="ov-nodes-head-icons">
            <button
              className={`icon-btn${addOpen ? ' is-on' : ''}`}
              onClick={() => setAddOpen((v) => !v)}
              disabled={installing}
              title={addOpen ? '收起安装' : '安装节点'}
              aria-label={addOpen ? '收起安装' : '安装节点'}
              aria-expanded={addOpen}
            >
              <Icon name={addOpen ? 'close' : 'plus'} size={14} />
            </button>
            <button
              className={`icon-btn${depsBusy || checking ? ' is-busy' : ''}`}
              onClick={() => void reloadAll()}
              disabled={loading || depsBusy || installing || checking}
              title="刷新节点列表和依赖状态"
              aria-label="刷新节点"
            >
              <Icon name="refresh" size={14} />
            </button>
            <button
              className="icon-btn"
              onClick={onDiagnose}
              disabled={installing}
              title="检查节点依赖与环境问题"
              aria-label="诊断节点"
            >
              <Icon name="diagnostics" size={14} />
            </button>
          </div>
        </div>

        {addOpen && (
          <div className="ov-node-install">
            <label className="field ov-node-url">
              仓库地址
              <input
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                autoFocus
              />
            </label>
            <label className="check">
              <input type="checkbox" checked={withReq} onChange={(e) => setWithReq(e.target.checked)} />
              装依赖
            </label>
            <button
              className="primary"
              onClick={() => void install()}
              disabled={!url.trim() || busy !== null}
            >
              <Icon name="plus" size={13} />
              {busy === 'install' ? '安装中…' : '安装'}
            </button>
          </div>
        )}

        {error && (
          <div className="banner err ov-node-banner">
            <span className="mark">
              <Icon name="alert" size={13} />
            </span>
            <div>{error}</div>
          </div>
        )}

        <div className="ov-node-scroll">
          {loading ? (
            <div className="empty">读取中…</div>
          ) : nodes.length === 0 ? (
            <div className="empty">
              <span className="empty-icon">
                <Icon name="nodes" size={20} />
              </span>
              <strong>还没有自定义节点</strong>
            </div>
          ) : (
            <>
              {deps?.noVenv && (
                <div className="banner warn" style={{ margin: 12 }}>
                  <span className="mark">
                    <Icon name="alert" size={13} />
                  </span>
                  <div>还没装好虚拟环境，检测不到已装包；先在「设置」里装环境，再回来补依赖。</div>
                </div>
              )}
              {deps && deps.conflicts.length > 0 && (
                <div className="banner warn" style={{ margin: 12 }}>
                  <span className="mark">
                    <Icon name="alert" size={13} />
                  </span>
                  <div>
                    pip check 报 {deps.conflicts.length} 条冲突，点「诊断」看细节。
                  </div>
                </div>
              )}
              <div className="node-list">
                {nodes.map((n) => {
                  const d = depsOf(n.name)
                  const pending = d?.pending.length ?? 0
                  const u = updates[n.name]
                  const updateLocked = busy !== null || installing || depsBusy
                  const depsLocked = busy !== null || installing || depsBusy
                  return (
                    <div className={`node-row${n.enabled ? '' : ' off'}`} key={n.name}>
                      <div className="node-title">
                        <span className={`node-state${n.enabled ? ' is-on' : ''}`} aria-hidden="true" />
                        <span
                          className="node-name mono"
                          title={[
                            n.name,
                            n.url ?? (n.kind === 'manual' ? '手动放入的节点' : ''),
                            n.commit ? `提交 ${n.commit.slice(0, 7)}` : '',
                            n.sizeMib < 1 ? '<1 MB' : `${n.sizeMib} MB`,
                          ]
                            .filter(Boolean)
                            .join(' · ')}
                        >
                          {n.name}
                        </span>
                        {n.kind === 'manual' && <span className="badge ghost">手动</span>}
                        {u && !u.error && u.behind > 0 && (
                          <button
                            type="button"
                            className="badge warn badge-action"
                            disabled={updateLocked || !n.enabled}
                            title={
                              !n.enabled
                                ? '先启用节点'
                                : updateLocked
                                  ? '等待当前操作完成'
                                  : u.local && u.remote
                                    ? `远端有新提交：${u.local.slice(0, 7)} → ${u.remote.slice(0, 7)}`
                                    : '远端有新提交'
                            }
                            aria-label={`更新 ${n.name}`}
                            onClick={() => void update(n)}
                          >
                            可更新
                          </button>
                        )}
                        {u?.error && (
                          <button
                            type="button"
                            className="badge badge-action"
                            disabled={updateLocked || !n.enabled}
                            title={
                              !n.enabled
                                ? '先启用节点'
                                : `检查更新失败：${u.error}；仍可手动更新`
                            }
                            aria-label={`更新 ${n.name}`}
                            onClick={() => void update(n)}
                          >
                            检查失败
                          </button>
                        )}
                        {pending > 0 ? (
                          <button
                            type="button"
                            className="badge warn badge-action"
                            disabled={depsLocked}
                            title={`补上 ${pending} 个缺失的依赖`}
                            aria-label={`补 ${n.name} 的 ${pending} 个依赖`}
                            onClick={() => void openInstall([n.name])}
                          >
                            缺 {pending} 个依赖
                          </button>
                        ) : d?.manual.length ? (
                          <button
                            type="button"
                            className="badge badge-action"
                            disabled={depsLocked}
                            title={`查看 ${d.manual.length} 条手动依赖项`}
                            aria-label={`查看 ${n.name} 的依赖`}
                            onClick={() => void openInstall([n.name])}
                          >
                            {d.manual.length} 条手动项
                          </button>
                        ) : d?.hasInstallPy ? (
                          <button
                            type="button"
                            className="badge badge-action"
                            disabled={depsLocked}
                            title="运行节点自带的 install.py"
                            aria-label={`运行 ${n.name} 的 install.py`}
                            onClick={() => void openInstall([n.name])}
                          >
                            装依赖
                          </button>
                        ) : null}
                      </div>
                      <div className="node-actions">
                        {confirmDel === n.name ? (
                          <>
                            <button
                              className="danger sm"
                              disabled={busy !== null}
                              title="删除节点目录，不可恢复；不会删除模型和输出"
                              onClick={() => void remove(n)}
                            >
                              确认删除
                            </button>
                            <button className="ghost sm" onClick={() => setConfirmDel(null)}>
                              取消
                            </button>
                          </>
                        ) : (
                          <button
                            className="icon-btn node-del"
                            disabled={busy !== null}
                            onClick={() => void remove(n)}
                            title="删除节点目录"
                            aria-label={`删除 ${n.name}`}
                          >
                            <Icon name="trash" size={14} />
                          </button>
                        )}
                        <button
                          type="button"
                          className={`node-switch${n.enabled ? ' is-on' : ''}`}
                          role="switch"
                          aria-checked={n.enabled}
                          aria-label={n.enabled ? `禁用 ${n.name}` : `启用 ${n.name}`}
                          title={n.enabled ? '禁用节点' : '启用节点'}
                          disabled={busy !== null}
                          onClick={() => void toggle(n)}
                        >
                          <span aria-hidden="true" />
                        </button>
                      </div>
                    </div>
                  )
                })}
              </div>
            </>
          )}
        </div>
      </section>

      {nodeOp && (
        <OperationDialog
          title={nodeOp.title}
          target={nodeOp.target}
          state={nodeOp.state}
          phase={nodeOp.phase}
          facts={[
            { label: '节点', value: nodeOp.target, mono: true },
            { label: '来源', value: nodeOp.source, mono: true },
            { label: '阶段', value: nodeOp.phase },
          ]}
          events={nodeOp.events}
          tab={nodeOp.tab}
          onTab={(tab) => setNodeOp((prev) => (prev ? { ...prev, tab } : prev))}
          detailLabel="安装详情"
          closeLabel={
            nodeOp.state !== 'running' ? '关闭' : nodeOp.title === '更新节点' ? '更新中…' : '安装中…'
          }
          allowHideOnRun={false}
          onClose={() => setNodeOp(null)}
        >
          {nodeOp.error && <div className="op-run-error">{nodeOp.error}</div>}
        </OperationDialog>
      )}

      {confirm && (installing || report || depsError) && (
        <OperationDialog
          title={depsError ? '依赖处理失败' : report ? '依赖处理结果' : '安装节点依赖'}
          target={targetCount ? `${targetCount} 个节点` : inst.name}
          state={installing ? 'running' : depsError ? 'failed' : 'ok'}
          phase={depPhase}
          facts={[
            { label: '节点', value: `${targetCount || targets.length} 个` },
            { label: '阶段', value: depPhase },
            { label: '输出', value: `${depEvents.length} 行`, mono: true },
          ]}
          events={depEvents}
          tab={depTab}
          onTab={setDepTab}
          detailLabel="依赖详情"
          allowHideOnRun={false}
          onClose={closeConfirm}
          footer={
            report?.snapshotId && onOpenSnapshots ? (
              <button
                className="ghost"
                onClick={() => {
                  closeConfirm()
                  onOpenSnapshots()
                }}
              >
                <Icon name="clock" size={13} />
                去快照页
              </button>
            ) : null
          }
        >
          {depsError ? (
            <div className="op-run-error">{depsError}</div>
          ) : report ? (
            <ReportBody report={report} />
          ) : (
            <>
              <div className="op-run-target">
                <div>
                  <span>节点</span>
                  <strong>{targetCount || targets.length} 个</strong>
                </div>
                <div>
                  <span>install.py</span>
                  <strong>
                    {targets.some((d) => d.hasInstallPy)
                      ? runInstallPy
                        ? '会运行'
                        : '不运行'
                      : '这些节点没有'}
                  </strong>
                </div>
                <div>
                  <span>快照</span>
                  <strong>装前自动保存</strong>
                </div>
              </div>
              <div className="op-run-live">
                <span className="op-run-live-mark" aria-hidden="true" />
                <span>{depEvents.length ? depPhase : '正在比对 requirements…'}</span>
              </div>
            </>
          )}
        </OperationDialog>
      )}

      {confirm && !installing && !report && !depsError &&
        createPortal(
          <div
            className="modal-backdrop"
            onMouseDown={(e) => {
              if (e.target === e.currentTarget) closeConfirm()
            }}
          >
            <div className="modal deps-modal" role="dialog" aria-modal="true" aria-labelledby="deps-title">
            <div className="modal-head">
              <h2 id="deps-title">补节点依赖</h2>
              <span className="badge ghost">{targetCount ? `${targetCount} 个节点` : inst.name}</span>
              <div className="spacer" />
              <button className="icon-btn" onClick={closeConfirm} title="关闭" aria-label="关闭">
                <Icon name="close" size={16} />
              </button>
            </div>

            <div className="modal-body">
                  {running && (
                    <div className="banner warn">
                      <span className="mark">
                        <Icon name="alert" size={13} />
                      </span>
                      <div>先停止实例；装完重启才生效。</div>
                    </div>
                  )}

                  {!deps && <div className="empty">正在读取依赖状态…</div>}

                  {deps && targets.length === 0 && (
                    <div className="banner ok">
                      <span className="mark">
                        <Icon name="check" size={13} />
                      </span>
                      <div>没有要装的包，也没有 install.py；这些节点的依赖已经齐了。</div>
                    </div>
                  )}

                  {targets.map((d) => (
                    <div className="deps-node" key={d.name}>
                      <div className="deps-node-head">
                        <span className="mono">{d.name}</span>
                        <span className={`badge ${d.enabled ? 'ok' : 'idle'}`}>{d.enabled ? '启用' : '禁用'}</span>
                        {d.pending.length > 0 && <span className="badge warn">{d.pending.length} 个要装</span>}
                        {d.hasInstallPy && <span className="badge ghost">有 install.py</span>}
                      </div>

                      {d.pending.length > 0 && (
                        <ul className="deps-list">
                          {d.pending.map((line) => (
                            <li key={line.line}>
                              <code>{line.line}</code>
                              <span className="deps-reason">{line.reason}</span>
                            </li>
                          ))}
                        </ul>
                      )}

                      {d.manual.map((line) => (
                        <ul className="deps-list" key={line}>
                          <li>
                            <code>{line}</code>
                          </li>
                        </ul>
                      ))}

                      {d.skipped.length > 0 && (
                        <details className="deps-skip">
                          <summary>跳过 {d.skipped.length} 条（含原因）</summary>
                          <ul className="deps-list">
                            {d.skipped.map((line) => (
                              <li key={line.line}>
                                <code>{line.line}</code>
                                <span className="deps-reason">{line.reason}</span>
                              </li>
                            ))}
                          </ul>
                        </details>
                      )}
                    </div>
                  ))}

                  {targets.some((d) => d.hasInstallPy) && (
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={runInstallPy}
                        onChange={(e) => setRunInstallPy(e.target.checked)}
                      />
                      运行节点自带的 install.py
                    </label>
                  )}

            </div>

            <div className="modal-foot">
              <div className="spacer" />
              <button className="ghost" onClick={closeConfirm}>
                取消
              </button>
              <button
                className="primary"
                onClick={() => void installDeps(running)}
                disabled={stopping || targets.length === 0}
              >
                {stopping
                  ? '停止中…'
                  : targets.length === 0
                    ? '没有要处理的依赖'
                    : running
                      ? '停止实例并再装'
                      : '开始装依赖'}
              </button>
            </div>
            </div>
          </div>,
          document.body,
        )}
    </>
  )
}

/** 装完之后的账本：谁装上了、谁跳过了、torch 有没有被带动 */
function ReportBody({ report }: { report: DepsInstallReport }) {
  const failed = report.nodes.reduce((n, r) => n + r.failed.length, 0)
  const installed = report.nodes.reduce((n, r) => n + r.installed.length, 0)
  const torchChanged = report.torchBefore !== report.torchAfter

  return (
    <div className="stack" style={{ gap: 12 }}>
      {failed === 0 ? (
        <div className="banner ok">
          <span className="mark">
            <Icon name="check" size={13} />
          </span>
          <div>
            装了 <strong>{installed}</strong> 条依赖
            {report.packagesAfter !== report.packagesBefore
              ? `（包清单 ${report.packagesBefore} → ${report.packagesAfter}）`
              : ''}
            。重启 ComfyUI 生效。
          </div>
        </div>
      ) : (
        <div className="banner err">
          <span className="mark">
            <Icon name="alert" size={13} />
          </span>
          <div>
            {failed} 项没装上，其余 {installed} 项已装。可以看下面的输出，或手动装。
          </div>
        </div>
      )}

      {torchChanged && (
        <div className="banner warn">
          <span className="mark">
            <Icon name="alert" size={13} />
          </span>
          <div>
            torch 被带动了：{report.torchBefore ?? '未装'} → {report.torchAfter ?? '未装'}
            。如果这不是你要的，回滚到「装依赖前」的快照。
          </div>
        </div>
      )}

      {report.warnings.map((w) => (
        <div className="banner warn" key={w}>
          <span className="mark">
            <Icon name="info" size={13} />
          </span>
          <div>{w}</div>
        </div>
      ))}

      {report.nodes.map((n) => (
        <div className="deps-node" key={n.name}>
          <div className="deps-node-head">
            <span className="mono">{n.name}</span>
            {n.installed.length > 0 && <span className="badge ok">装 {n.installed.length}</span>}
            {n.failed.length > 0 && <span className="badge err">{n.failed.length} 失败</span>}
            {n.skipped.length > 0 && <span className="badge idle">跳过 {n.skipped.length}</span>}
            {n.installPy?.ran && (
              <span className={`badge ${n.installPy.ok ? 'ok' : 'warn'}`}>
                install.py {n.installPy.ok ? '完成' : '失败'}
              </span>
            )}
          </div>
          {n.installed.length > 0 && (
            <div className="deps-installed mono">{n.installed.join('  ·  ')}</div>
          )}
          {n.failed.length > 0 && (
            <ul className="deps-list">
              {n.failed.map((f) => (
                <li key={f.line}>
                  <code>{f.line}</code>
                  <span className="deps-reason err-text">{f.error}</span>
                </li>
              ))}
            </ul>
          )}
          {n.skipped.length > 0 && (
            <details className="deps-skip">
              <summary>跳过 {n.skipped.length} 条（含原因）</summary>
              <ul className="deps-list">
                {n.skipped.map((line) => (
                  <li key={line.line}>
                    <code>{line.line}</code>
                    <span className="deps-reason">{line.reason}</span>
                  </li>
                ))}
              </ul>
            </details>
          )}
          {n.installPy && !n.installPy.ok && n.installPy.message && (
            <pre className="install-log" style={{ maxHeight: 160 }}>
              {n.installPy.message}
            </pre>
          )}
        </div>
      ))}

      {report.conflicts.length > 0 && (
        <details open>
          <summary>pip check：{report.conflicts.length} 条冲突</summary>
          <pre className="install-log" style={{ marginTop: 8 }}>
            {report.conflicts.join('\n')}
          </pre>
        </details>
      )}
    </div>
  )
}
