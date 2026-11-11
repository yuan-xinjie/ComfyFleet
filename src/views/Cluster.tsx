import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { api, errText } from '../lib/api'
import type {
  ClusterMemberView,
  ClusterStatus,
  InstanceConfig,
  MissingWheel,
  SourceChanges,
  SyncCompare,
  SyncJob,
  SyncOptions,
  SyncPrepare,
  SyncScope,
} from '../types'
import Icon from '../components/Icon'
import Select from '../components/Select'
import {
  ALL_SCOPE,
  SCOPE_ITEMS,
  ComparePanel,
  DiffDetails,
  DiffSummary,
  LocalInstances,
  PendingList,
  PreparePanel,
  ScopeGrid,
  SyncDialog,
  TargetList,
  Timeline,
  commit,
  dateClock,
  isWaitingConfirmation,
  stateText,
} from './ClusterUI'

/** Leader 掉线时后端会把心跳请求的 URL 塞进 error；界面只留一句人话。 */
function clusterErrorText(error: string | null | undefined): string | null {
  if (!error) return null
  if (/member\/heartbeat|Connection|Connect error|timed out|请求失败/i.test(error)) {
    return '连接不到 Leader，请确认 Leader 已启动且网络可达。'
  }
  return error
}

const SPLIT_KEY = 'comfyfleet.sync.split'
const DEFAULT_SPLIT = 0.58
const MIN_SPLIT = 0.38
const MAX_SPLIT = 0.78

function readSplit(): number {
  try {
    const saved = Number(localStorage.getItem(SPLIT_KEY))
    if (Number.isFinite(saved) && saved >= MIN_SPLIT && saved <= MAX_SPLIT) return saved
  } catch {
    /* localStorage 不可用时用默认值 */
  }
  return DEFAULT_SPLIT
}

interface Props {
  instances: InstanceConfig[]
  notify: (kind: 'ok' | 'err', msg: string) => void
  onBack: () => void
  initialSourceId?: string | null
  /** 页面常驻后，隐藏时暂停轮询；切回来立即刷新一次。 */
  active?: boolean
}

export default function Cluster({ instances, notify, onBack, initialSourceId, active = true }: Props) {
  const [status, setStatus] = useState<ClusterStatus | null>(null)
  const [loadError, setLoadError] = useState(false)
  const [busy, setBusy] = useState('')

  const [create, setCreate] = useState({ name: '', password: '', port: '47821', allowLan: false })
  const [join, setJoin] = useState({ host: '', port: '47821', password: '', memberName: '' })

  const [sourceId, setSourceId] = useState(initialSourceId ?? '')
  const [selectedMembers, setSelectedMembers] = useState<string[]>([])
  const [expandedMembers, setExpandedMembers] = useState<string[]>([])
  const [scope, setScope] = useState<SyncScope>(ALL_SCOPE)
  const [prep, setPrep] = useState<SyncPrepare | null>(null)
  const [wheelGate, setWheelGate] = useState<{
    missing: MissingWheel[]
    library: string | null
    retried: boolean
  } | null>(null)
  const [changes, setChanges] = useState<SourceChanges | null>(null)
  const [changesBusy, setChangesBusy] = useState(false)
  const [changesOpen, setChangesOpen] = useState(false)
  const [bottomSlot, setBottomSlot] = useState<HTMLElement | null>(null)

  const [compareSourceId, setCompareSourceId] = useState('')
  const [compareTargetId, setCompareTargetId] = useState('')
  const [comparison, setComparison] = useState<SyncCompare | null>(null)
  const [compareOpen, setCompareOpen] = useState(false)

  const [deleteOpen, setDeleteOpen] = useState(false)
  const [deletePassword, setDeletePassword] = useState('')
  const [deleteError, setDeleteError] = useState('')

  const [confirmTargets, setConfirmTargets] = useState<Record<string, string>>({})
  const [confirmAction, setConfirmAction] = useState('')
  const [now, setNow] = useState(Date.now())
  const mainRef = useRef<HTMLDivElement | null>(null)
  const workspaceRef = useRef<HTMLDivElement | null>(null)
  const [mainHeight, setMainHeight] = useState(0)
  const [split, setSplit] = useState(readSplit)

  const [targetOptions, setTargetOptions] = useState<
    Record<
      string,
      Pick<SyncOptions, 'removeExtraNodes' | 'removeExtraModels' | 'allowDirtyTarget'>
    >
  >({})
  // 设置页里的“多余节点/模型”是接收端的默认值；用户在确认卡片里仍可逐次覆盖。
  const [syncDefaults, setSyncDefaults] = useState({
    removeExtraNodes: false,
    removeExtraModels: false,
    // 目标端有本地改动时必须由用户显式勾选，不能默认放过。
    allowDirtyTarget: false,
  })

  useEffect(() => {
    let alive = true
    api
      .settingsGet()
      .then((s) => {
        if (alive) {
          setSyncDefaults({
            removeExtraNodes: !!s.sync?.removeExtraNodes,
            removeExtraModels: !!s.sync?.removeExtraModels,
            allowDirtyTarget: false,
          })
        }
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [])

  // 时间线不抢高度：始终与左侧工作面等高，任务多了在栏内滚动。
  useEffect(() => {
    const el = mainRef.current
    if (!el) return
    const measure = () => {
      const height = el.getBoundingClientRect().height
      if (height > 0) setMainHeight(Math.ceil(height))
    }
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(el)
    return () => observer.disconnect()
  }, [status?.role])

  useEffect(() => {
    try {
      localStorage.setItem(SPLIT_KEY, String(split))
    } catch {
      /* 忽略存储失败，拖动仍然有效 */
    }
  }, [split])

  const setSplitClamped = (value: number) => {
    setSplit(Math.min(MAX_SPLIT, Math.max(MIN_SPLIT, value)))
  }

  const beginSplitDrag = (startX: number) => {
    const el = workspaceRef.current
    if (!el) return
    const rect = el.getBoundingClientRect()
    if (rect.width <= 0) return
    const startWidth = rect.width * split
    const move = (event: PointerEvent) => {
      setSplitClamped((startWidth + event.clientX - startX) / rect.width)
    }
    const end = () => {
      document.body.classList.remove('sync-resizing')
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', end)
      window.removeEventListener('pointercancel', end)
    }
    document.body.classList.add('sync-resizing')
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', end)
    window.addEventListener('pointercancel', end)
  }

  /** 轮询可能重叠，只接受最后一次请求的结果，避免旧状态把新状态盖回去。 */
  const refreshSeq = useRef(0)
  const changesSeq = useRef(0)

  useEffect(() => {
    if (initialSourceId) setSourceId(initialSourceId)
  }, [initialSourceId])

  // 源端动作条由 App 的固定底栏承载；用 portal 复用同一套页面状态。
  useEffect(() => {
    setBottomSlot(document.getElementById('cluster-bottombar-slot'))
  }, [])

  const refresh = useCallback(async () => {
    const seq = ++refreshSeq.current
    try {
      const next = await api.clusterStatus()
      if (seq !== refreshSeq.current) return
      setStatus(next)
      setLoadError(false)
    } catch (e) {
      // Leader 掉线时后台每 2.5 秒都会失败：不要在界面上刷请求 URL / 心跳报错，
      // 只显示一句「连接不到 Leader」，恢复后下一轮自动清掉。
      if (seq !== refreshSeq.current) return
      const text = errText(e)
      if (/heartbeat|Connection|Connect error|timed out|连接|请求失败/i.test(text)) {
        setLoadError(true)
      } else {
        notify('err', text)
      }
    }
  }, [notify])

  useEffect(() => {
    if (!active) return
    void refresh()
    const timer = window.setInterval(() => void refresh(), 2500)
    // 后台窗口的定时器会被浏览器节流；回到前台时补一次，成员离开/加入不必等下一轮。
    const onFocus = () => void refresh()
    const onVisible = () => {
      if (document.visibilityState === 'visible') void refresh()
    }
    window.addEventListener('focus', onFocus)
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      window.clearInterval(timer)
      window.removeEventListener('focus', onFocus)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [active, refresh])

  useEffect(() => {
    if (!active || !sourceId || (busy !== 'start' && !(prep && !prep.done))) return
    let alive = true
    const tick = async () => {
      try {
        const next = await api.syncPrepareStatus(sourceId)
        if (alive && next) setPrep(next)
      } catch {
        // 准备状态只是辅助展示；失败不覆盖主动作错误。
      }
    }
    void tick()
    const timer = window.setInterval(() => void tick(), 350)
    return () => {
      alive = false
      window.clearInterval(timer)
    }
  }, [active, sourceId, busy, prep?.done])

  useEffect(() => {
    if (!active) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [active])

  // 破坏性操作是两步：第一次点击只把按钮切成确认态，4 秒内再点才执行。
  useEffect(() => {
    if (!confirmAction) return
    const timer = window.setTimeout(() => setConfirmAction(''), 4000)
    return () => window.clearTimeout(timer)
  }, [confirmAction])

  const locals = useMemo(() => instances.filter((i) => i.kind !== 'remote'), [instances])
  const members = status?.cluster?.members ?? []
  const awaitingJobs = status?.jobs?.filter((job) => isWaitingConfirmation(job.status)) ?? []
  const compareSources = status?.sources ?? []
  const compareTargets = status?.member?.instances ?? []
  const jobs = status?.jobs ?? []
  const sourceLocal = locals.find((i) => i.id === sourceId)
  const sourceView = compareSources.find((i) => i.id === sourceId)
  const sourceState = sourceView?.state ?? sourceLocal?.state ?? 'stopped'
  const sourceRunning = sourceState === 'running'
  const sourceVersion = sourceLocal?.comfy.version ?? sourceView?.coreVersion ?? '—'
  const sourcePython = sourceLocal?.pythonVersion || sourceView?.pythonVersion || '—'
  const sourceCommit = sourceView?.coreCommit ?? sourceLocal?.comfy.commit ?? null
  const sourceNodes = sourceView?.countsKnown === false ? undefined : sourceView?.nodeCount
  const sourceModels = sourceView?.countsKnown === false ? undefined : sourceView?.modelCount
  const onlineMembers = members.filter((m) => m.online).length

  useEffect(() => {
    if (!sourceId && locals[0]) setSourceId(locals[0].id)
  }, [locals, sourceId])

  useEffect(() => {
    setSelectedMembers((prev) => {
      const next = prev.filter((id) =>
        members.some((m) => m.online && m.instances.length > 0 && m.id === id),
      )
      return next.length === prev.length ? prev : next
    })
  }, [members])

  useEffect(() => {
    if (!compareSourceId && compareSources[0]) setCompareSourceId(compareSources[0].id)
    if (!compareTargetId && compareTargets[0]) setCompareTargetId(compareTargets[0].id)
  }, [compareSourceId, compareSources, compareTargetId, compareTargets])

  useEffect(() => {
    const instances = status?.member?.instances ?? []
    setConfirmTargets((prev) => {
      if (!awaitingJobs.length) {
        return Object.keys(prev).length ? {} : prev
      }
      const next: Record<string, string> = {}
      for (const job of awaitingJobs) {
        const current = prev[job.id]
        if (current && instances.some((i) => i.id === current)) {
          next[job.id] = current
        } else {
          const syncable = instances.filter(
            (i) => i.state === 'ready' || i.state === 'stopped' || i.state === 'running',
          )
          const preferred =
            job.targetInstanceId && syncable.some((i) => i.id === job.targetInstanceId)
              ? job.targetInstanceId
              : ''
          const stopped = syncable.find((i) => i.state === 'stopped')
          next[job.id] = preferred || ((stopped ?? syncable[0])?.id ?? '')
        }
      }
      return next
    })
    // awaitingJobs 每次刷新都是新数组，用 id 串作为依赖，避免无意义的状态回写。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [awaitingJobs.map((j) => j.id).join('|'), status?.member?.instances])

  const loadChanges = useCallback(
    async (silent = false) => {
      if (!sourceId) {
        changesSeq.current += 1
        setChanges(null)
        return
      }
      const seq = ++changesSeq.current
      setChangesBusy(true)
      try {
        const next = await api.clusterSourceChanges(sourceId)
        if (seq !== changesSeq.current) return
        setChanges(next)
      } catch (e) {
        if (seq === changesSeq.current && !silent) notify('err', errText(e))
      } finally {
        if (seq === changesSeq.current) setChangesBusy(false)
      }
    },
    [sourceId, notify],
  )

  useEffect(() => {
    if (!active || !sourceId || status?.role !== 'leader') return
    void loadChanges(true)
    const timer = window.setInterval(() => void loadChanges(true), 10000)
    return () => {
      window.clearInterval(timer)
      changesSeq.current += 1
    }
  }, [active, sourceId, status?.role, loadChanges])

  // 任务进入终态后，基线可能刚更新；下一次轮询刷新变化面板。
  const terminalCount = jobs.filter((j) => j.status === 'success' || j.status === 'partial').length
  useEffect(() => {
    if (active && sourceId && status?.role === 'leader') void loadChanges(true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [terminalCount])

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      notify('ok', `已复制 ${text}`)
    } catch {
      notify('err', `复制失败，请手动选择：${text}`)
    }
  }

  const doCreate = async () => {
    setBusy('create')
    try {
      const next = await api.clusterCreate({
        name: create.name.trim(),
        password: create.password,
        port: Number(create.port) || 47821,
        allowLan: create.allowLan,
      })
      setStatus(next)
      notify('ok', '集群已创建，等待目标客户端加入')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const doJoin = async () => {
    setBusy('join')
    try {
      const next = await api.clusterJoin({
        host: join.host.trim(),
        port: Number(join.port) || 47821,
        password: join.password,
        memberName: join.memberName.trim(),
      })
      setStatus(next)
      notify('ok', '已加入集群，正在上报本机实例')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const toggleMember = (member: ClusterMemberView) => {
    setSelectedMembers((prev) =>
      prev.includes(member.id) ? prev.filter((id) => id !== member.id) : [...prev, member.id],
    )
  }

  const toggleExpanded = (id: string) => {
    setExpandedMembers((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]))
  }

  const toggleAllMembers = () => {
    const selectable = members.filter((m) => m.online && m.instances.length > 0).map((m) => m.id)
    setSelectedMembers((prev) => (prev.length === selectable.length ? [] : selectable))
  }

  const toggleScope = (key: keyof SyncScope) => {
    setScope((prev) => ({ ...prev, [key]: !prev[key] }))
  }

  const doStart = async (skipMissingWheels = false) => {
    if (!sourceId || !selectedMembers.length || sourceRunning || !SCOPE_ITEMS.some((item) => scope[item.key])) return
    const startedAt = new Date().toISOString()
    setPrep({
      sourceInstanceId: sourceId,
      phase: '提交准备',
      percent: 0,
      message: '正在建立准备任务…',
      detail: '等待源端开始扫描',
      logs: [],
      startedAt,
      updatedAt: startedAt,
      done: false,
      cancelled: false,
      cancelling: false,
      error: null,
      generationId: null,
      jobs: 0,
      files: 0,
      bytes: 0,
    })
    setBusy('start')
    let gated = false
    try {
      const result = await api.syncRequest(sourceId, selectedMembers, scope, skipMissingWheels)
      if (result.outcome === 'needsWheels') {
        gated = true
        const retrying = wheelGate !== null
        setWheelGate({ missing: result.missing, library: result.library, retried: retrying })
        setPrep(null)
        if (retrying) {
          notify('err', `仍缺少 ${result.missing.length} 个依赖：补齐后再点「继续」，或确认跳过`)
        }
        return
      }
      setWheelGate(null)
      notify(
        'ok',
        `已向 ${result.jobs.length} 台客户机发送同步请求；目标机选择实例并确认后才会开始`,
      )
      await refresh()
      void loadChanges(true)
    } catch (e) {
      const message = errText(e)
      if (!/SYNC_CANCELLED|准备已取消/.test(message)) notify('err', message)
      // 请求失败时本地构造的面板还没有服务端任务，必须清掉，
      // 否则 preparing=true 会一直禁用源端/范围/发送。
      setPrep(null)
    } finally {
      if (!gated) {
        try {
          const final = await api.syncPrepareStatus(sourceId)
          setPrep(final ?? null)
        } catch {
          // 保留最后一次进度快照（如果请求失败，上面的 catch 已经清空）
        }
      }
      setBusy('')
    }
  }

  /** 用户点「继续」：重新检查一遍；补齐了就开始，还缺就把最新的缺件列表再摆出来。 */
  const retryWheelGate = () => {
    void doStart(false)
  }

  const cancelPrepare = async () => {
    if (!sourceId) return
    try {
      await api.syncPrepareCancel(sourceId)
      const next = await api.syncPrepareStatus(sourceId)
      setPrep(next ?? null)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const closePrepare = async () => {
    if (!sourceId) {
      setPrep(null)
      return
    }
    try {
      await api.syncPrepareClear(sourceId)
    } catch {
      // 已经结束的准备即使清理缓存失败，也不影响再次发送
    }
    setPrep(null)
  }

  const doCompare = async () => {
    if (!compareSourceId || !compareTargetId) return
    setBusy('compare')
    try {
      setComparison(await api.clusterCompare(compareSourceId, compareTargetId))
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const resetLocalCluster = () => {
    // 作废离开前发出的轮询响应，否则界面会又跳回旧集群。
    refreshSeq.current += 1
    changesSeq.current += 1
    // 后端已经删掉本地集群文件；这里立即切回未加入状态，避免轮询间隙仍显示旧成员页。
    setStatus({
      role: 'none',
      cluster: null,
      member: null,
      sources: [],
      jobs: [],
      addresses: [],
      serverRunning: false,
      agentRunning: false,
      error: null,
    })
  }

  const doLeave = async () => {
    if (confirmAction !== 'leave') {
      setConfirmAction('leave')
      return
    }
    setConfirmAction('')
    setBusy('leave')
    try {
      await api.clusterLeave()
      resetLocalCluster()
      notify('ok', '已退出集群')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const doDelete = async () => {
    if (busy === 'delete' || !deletePassword) return
    setDeleteError('')
    setBusy('delete')
    try {
      await api.clusterDelete(deletePassword)
      resetLocalCluster()
      setDeleteOpen(false)
      setDeletePassword('')
      notify('ok', '集群已删除')
    } catch (e) {
      setDeleteError(errText(e))
    } finally {
      setBusy('')
    }
  }

  const toggleLan = async () => {
    if (!status?.cluster) return
    const next = !status.cluster.allowLan
    if (next && confirmAction !== 'lan') {
      setConfirmAction('lan')
      return
    }
    setConfirmAction('')
    setBusy('lan')
    try {
      setStatus(await api.clusterSetLan(next))
      notify('ok', next ? '已允许局域网加入；请确认防火墙只对可信网段开放' : '已改为仅本机监听')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const retryServer = async () => {
    setBusy('retry')
    try {
      setStatus(await api.clusterRetryServer())
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const removeMember = async (m: ClusterMemberView) => {
    if (confirmAction !== `remove:${m.id}`) {
      setConfirmAction(`remove:${m.id}`)
      return
    }
    setConfirmAction('')
    setBusy(`remove-${m.id}`)
    try {
      const next = await api.clusterRemoveMember(m.id)
      setStatus(next)
      setSelectedMembers((prev) => prev.filter((id) => id !== m.id))
      notify('ok', '成员已移除')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const confirmJob = async (job: SyncJob) => {
    const targetId = confirmTargets[job.id]
    if (!targetId) {
      notify('err', '先选择本机要接收同步的实例')
      return
    }
    const target = status?.member?.instances.find((inst) => inst.id === targetId)
    if (
      !target ||
      (target.state !== 'ready' && target.state !== 'stopped' && target.state !== 'running')
    ) {
      notify('err', '目标实例正在安装/启动/停止或未初始化，不能作为同步目标')
      return
    }
    const cleanup = targetOptions[job.id] ?? syncDefaults
    setBusy(`confirm-${job.id}`)
    try {
      const next = await api.clusterConfirmJob(job.id, targetId, {
        ...cleanup,
        allowDirtyTarget: cleanup.allowDirtyTarget ?? false,
      })
      setStatus(next)
      notify(
        'ok',
        target.state === 'running'
          ? '已接受，目标端会先停止该实例再开始同步'
          : '已接受，目标端即将开始同步',
      )
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const rejectJob = async (job: SyncJob) => {
    if (confirmAction !== `reject:${job.id}`) {
      setConfirmAction(`reject:${job.id}`)
      return
    }
    setConfirmAction('')
    setBusy(`reject-${job.id}`)
    try {
      const next = await api.clusterRejectJob(job.id)
      setStatus(next)
      notify('ok', '已拒绝本次同步')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const cancelJob = async (job: SyncJob) => {
    setBusy(`cancel-${job.id}`)
    try {
      const next = await api.clusterCancelJob(job.id)
      setStatus(next)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const deleteJob = async (job: SyncJob) => {
    if (confirmAction !== `delete:${job.id}`) {
      setConfirmAction(`delete:${job.id}`)
      return
    }
    setConfirmAction('')
    setBusy(`delete-${job.id}`)
    try {
      const next = await api.clusterDeleteJob(job.id)
      setStatus(next)
      notify('ok', '已删除同步历史')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const cancelTargetJob = async (job: SyncJob) => {
    setBusy(`cancel-target-${job.id}`)
    try {
      const next = await api.clusterCancelMemberJob(job.id)
      setStatus(next)
      notify(
        'ok',
        job.status === 'pending'
          ? '已取消同步，可在下方重新接受'
          : '已请求停止；停止完成后可重新接受',
      )
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const retryTargetJob = async (job: SyncJob) => {
    setBusy(`retry-${job.id}`)
    try {
      const next = await api.clusterRetryMemberJob(job.id)
      setStatus(next)
      notify('ok', '已重新接受，目标端会按当前状态重新计算差异并执行')
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy('')
    }
  }

  const scopeAny = SCOPE_ITEMS.some((item) => scope[item.key])
  const scopeSelected = SCOPE_ITEMS.filter((item) => scope[item.key]).length
  const preparing = !!prep && !prep.done
  const timelineJobs = jobs.filter((job) => status?.role !== 'member' || !isWaitingConfirmation(job.status))
  const canStart =
    !!sourceId &&
    selectedMembers.length > 0 &&
    !sourceRunning &&
    !busy &&
    !preparing &&
    scopeAny &&
    status?.role === 'leader'

  const sourceOptions = locals.map((i) => ({
    value: i.id,
    label: i.name,
    meta: stateText(i.state),
  }))
  const selectedMemberNames = selectedMembers
    .map((id) => members.find((m) => m.id === id)?.name)
    .filter((name): name is string => !!name)
  const statusError =
    clusterErrorText(status?.error) ??
    (loadError ? '连接不到 Leader，请确认 Leader 已启动且网络可达。' : null)

  const resizeHandle = (
    <div
      className="sync-splitter"
      role="separator"
      aria-label="调整左右宽度"
      aria-orientation="vertical"
      aria-valuemin={Math.round(MIN_SPLIT * 100)}
      aria-valuemax={Math.round(MAX_SPLIT * 100)}
      aria-valuenow={Math.round(split * 100)}
      title="拖动调整左右宽度，双击恢复"
      tabIndex={0}
      onPointerDown={(e) => {
        if (e.button !== 0) return
        e.preventDefault()
        beginSplitDrag(e.clientX)
      }}
      onDoubleClick={() => setSplitClamped(DEFAULT_SPLIT)}
      onKeyDown={(e) => {
        if (e.key === 'ArrowLeft') {
          e.preventDefault()
          setSplitClamped(split - 0.02)
        } else if (e.key === 'ArrowRight') {
          e.preventDefault()
          setSplitClamped(split + 0.02)
        } else if (e.key === 'Home') {
          e.preventDefault()
          setSplitClamped(DEFAULT_SPLIT)
        }
      }}
    >
      <span className="sync-splitter-grip" aria-hidden="true">
        <i />
        <i />
        <i />
      </span>
    </div>
  )

  return (
    <>
      <div className="sync-page">
        {!status ? (
          <div className="sync-loading">
            <span className={`sync-loading-dot ${loadError ? 'is-error' : ''}`} />
            <strong>{loadError ? '无法连接 Leader' : '正在读取集群状态'}</strong>
            {loadError && (
              <button className="ghost sm" onClick={() => void refresh()}>
                重试
              </button>
            )}
          </div>
        ) : status.role === 'none' ? (
          <div className="sync-entry">
            <section className="sync-entry-pane">
              <header className="sync-entry-head">
                <span className="sync-entry-icon">
                  <Icon name="globe" size={20} />
                </span>
                <h2>创建集群</h2>
                <span className="badge ghost">Leader</span>
              </header>
              <form
                id="sync-create-form"
                className="sync-form"
                onSubmit={(e) => {
                  e.preventDefault()
                  if (busy === 'create' || !create.name.trim() || create.password.length < 8) return
                  void doCreate()
                }}
              >
                <label className="field">
                  <span>集群名称</span>
                  <input
                    value={create.name}
                    placeholder="工作室 A"
                    onChange={(e) => setCreate({ ...create, name: e.target.value })}
                  />
                </label>
                <div className="field-row">
                  <label className="field">
                    <span>监听端口</span>
                    <input
                      value={create.port}
                      inputMode="numeric"
                      onChange={(e) => setCreate({ ...create, port: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>集群密码 · 至少 8 位</span>
                    <input
                      type="password"
                      value={create.password}
                      autoComplete="new-password"
                      onChange={(e) => setCreate({ ...create, password: e.target.value })}
                    />
                  </label>
                </div>
                <label className="sync-switch">
                  <input
                    type="checkbox"
                    checked={create.allowLan}
                    onChange={(e) => setCreate({ ...create, allowLan: e.target.checked })}
                  />
                  <span className="sync-switch-track" aria-hidden="true" />
                  <span>允许局域网加入</span>
                  <small className="hint">局域网为明文 HTTP，仅限可信网络</small>
                </label>
              </form>
              <button
                type="submit"
                form="sync-create-form"
                className="primary sync-entry-action"
                disabled={busy === 'create' || !create.name.trim() || create.password.length < 8}
              >
                <Icon name="plus" size={15} />
                {busy === 'create' ? '创建中…' : '创建集群'}
              </button>
            </section>

            <section className="sync-entry-pane">
              <header className="sync-entry-head">
                <span className="sync-entry-icon">
                  <Icon name="download" size={20} />
                </span>
                <h2>加入集群</h2>
                <span className="badge ghost">Follower</span>
              </header>
              <form
                id="sync-join-form"
                className="sync-form"
                onSubmit={(e) => {
                  e.preventDefault()
                  if (busy === 'join' || !join.host.trim() || !join.password) return
                  void doJoin()
                }}
              >
                <div className="field-row">
                  <label className="field">
                    <span>Leader 地址</span>
                    <input
                      value={join.host}
                      placeholder="192.168.1.10"
                      onChange={(e) => setJoin({ ...join, host: e.target.value })}
                    />
                  </label>
                  <label className="field">
                    <span>端口</span>
                    <input
                      value={join.port}
                      inputMode="numeric"
                      onChange={(e) => setJoin({ ...join, port: e.target.value })}
                    />
                  </label>
                </div>
                <label className="field">
                  <span>集群密码</span>
                  <input
                    type="password"
                    value={join.password}
                    autoComplete="current-password"
                    onChange={(e) => setJoin({ ...join, password: e.target.value })}
                  />
                </label>
                <label className="field">
                  <span>成员名</span>
                  <input
                    value={join.memberName}
                    placeholder="留空使用计算机名"
                    onChange={(e) => setJoin({ ...join, memberName: e.target.value })}
                  />
                </label>
              </form>
              <button
                type="submit"
                form="sync-join-form"
                className="primary sync-entry-action"
                disabled={busy === 'join' || !join.host.trim() || !join.password}
              >
                <Icon name="download" size={15} />
                {busy === 'join' ? '连接中…' : '加入集群'}
              </button>
            </section>
          </div>
        ) : status.role === 'leader' && status.cluster ? (
          <>
            <header className="sync-head">
              <div className="sync-head-main">
                <span className="sync-head-mark">
                  <Icon name="globe" size={22} />
                </span>
                <div className="sync-head-id">
                  <div className="sync-head-title">
                    <h1>{status.cluster.name}</h1>
                    <span className="badge ghost">Leader</span>
                  </div>
                  <div className="sync-head-meta">
                    <span className={`sync-status ${status.serverRunning ? 'is-ok' : 'is-err'}`}>
                      <span className="dot" />
                      {status.serverRunning ? '服务运行中' : '服务未运行'}
                    </span>
                    <span>
                      {onlineMembers} / {members.length} 在线
                    </span>
                    <span>{status.cluster.allowLan ? '局域网可加入' : '仅本机'}</span>
                  </div>
                </div>
              </div>
              <div className="sync-head-center">
                <div className="sync-addresses">
                  {status.cluster.addresses.map((address) => (
                    <button
                      className="sync-address"
                      key={address}
                      onClick={() => void copy(address)}
                      title="复制地址"
                    >
                      <Icon name="copy" size={12} />
                      {address}
                    </button>
                  ))}
                </div>
              </div>
              <div className="sync-head-actions">
                <button className="ghost sm" onClick={() => void refresh()} disabled={!!busy}>
                  <Icon name="refresh" size={13} />
                  刷新
                </button>
                <button className="ghost sm" onClick={() => void toggleLan()} disabled={!!busy}>
                  <Icon name="globe" size={13} />
                  {confirmAction === 'lan'
                    ? '确认开放？'
                    : status.cluster.allowLan
                      ? '改为仅本机'
                      : '开放局域网'}
                </button>
                <button
                  className="ghost sm"
                  onClick={() => {
                    setConfirmAction('')
                    setDeletePassword('')
                    setDeleteError('')
                    setDeleteOpen(true)
                  }}
                  disabled={!!busy}
                >
                  <Icon name="close" size={13} />
                  删除集群
                </button>
              </div>
            </header>

            {statusError && (
              <div className="sync-alert">
                <Icon name="alert" size={15} />
                <span>{statusError}</span>
                <button className="ghost sm" onClick={() => void retryServer()} disabled={!!busy}>
                  重试启动
                </button>
              </div>
            )}

            {!status.cluster.allowLan && members.length > 0 && (
              <div className="sync-alert is-warn">
                <Icon name="globe" size={15} />
                <span>仅本机监听，其他成员不可达</span>
                <button className="ghost sm" onClick={() => void toggleLan()} disabled={!!busy}>
                  开放局域网
                </button>
              </div>
            )}

            {prep && (
              <PreparePanel
                prep={prep}
                sourceName={sourceLocal?.name ?? sourceView?.name ?? sourceId}
                onCancel={() => void cancelPrepare()}
                onClose={() => void closePrepare()}
              />
            )}

            <div className="sync-workspace" ref={workspaceRef}>
              <div
                className="sync-panel sync-workspace-main"
                ref={mainRef}
                style={{ flexBasis: `${split * 100}%` }}
              >
                <section className="sync-section">
                  <header className="sync-section-head">
                    <h2>源实例</h2>
                    <span
                      className={`sync-state ${sourceRunning || sourceState === 'ready' ? 'is-running' : ''}`}
                    >
                      {stateText(sourceState)}
                    </span>
                    <div className="spacer" />
                    <button
                      className="ghost sm"
                      onClick={() => setChangesOpen(true)}
                      disabled={!changes || changesBusy}
                    >
                      <Icon name="diagnostics" size={13} />
                      {changesBusy ? '扫描中…' : '变化明细'}
                    </button>
                  </header>
                  {locals.length ? (
                    <Select
                      value={sourceId}
                      options={sourceOptions}
                      onChange={setSourceId}
                      disabled={!!busy || preparing}
                      ariaLabel="选择源实例"
                      className="sync-source-select"
                      placeholder="选择源实例"
                    />
                  ) : (
                    <div className="sync-empty is-inline">
                      <strong>本机还没有实例</strong>
                    </div>
                  )}
                  <div className="sync-facts">
                    <div>
                      <span>版本</span>
                      <strong>{sourceVersion}</strong>
                    </div>
                    <div>
                      <span>commit</span>
                      <strong>{commit(sourceCommit)}</strong>
                    </div>
                    <div>
                      <span>Python</span>
                      <strong>{sourcePython}</strong>
                    </div>
                    <div>
                      <span>节点 / 模型</span>
                      <strong>
                        {sourceNodes === undefined ? '—' : sourceNodes} /{' '}
                        {sourceModels === undefined ? '—' : sourceModels}
                      </strong>
                    </div>
                  </div>
                </section>

                {changes?.diff.python.missingWheels.length ? (
                  <div className="sync-alert is-warn">
                    <Icon name="alert" size={15} />
                    <span>
                      {changes.diff.python.missingWheels.length} 个自定义包缺少可同步的 wheel；
                      请把这些 .whl 放进“设置 → 本地 Wheel 库”，再刷新预览。同步时会跳过它们，结束后统一提示。
                    </span>
                  </div>
                ) : null}

                {changes?.diff.python.incompatibleWheels?.some((w) => w.available) ? (
                  <div className="sync-alert is-warn">
                    <Icon name="alert" size={15} />
                    <span>
                      {
                        changes.diff.python.incompatibleWheels.filter((w) => w.available)
                          .length
                      }{' '}
                      个自定义包与源环境不兼容；按“目标与源完全一致”策略仍会传输并安装，但目标端可能无法运行。
                    </span>
                  </div>
                ) : null}

                <section className="sync-section">
                  <header className="sync-section-head">
                    <h2>同步内容</h2>
                    <span className="badge ghost">{scopeSelected} / 4</span>
                  </header>
                  <ScopeGrid
                    scope={scope}
                    diff={changes?.diff ?? null}
                    scanning={changesBusy}
                    disabled={!!busy || preparing}
                    onToggle={toggleScope}
                  />
                </section>

                <section className="sync-section sync-targets-section">
                  <header className="sync-section-head">
                    <h2>目标机</h2>
                    <span className="badge ghost">{members.length}</span>
                  </header>
                  <TargetList
                    members={members}
                    selected={selectedMembers}
                    expanded={expandedMembers}
                    busy={busy}
                    confirmRemove={confirmAction}
                    onToggle={toggleMember}
                    onExpand={toggleExpanded}
                    onRemove={(member) => void removeMember(member)}
                    onToggleAll={toggleAllMembers}
                  />
                </section>
              </div>

              {resizeHandle}

              <Timeline
                jobs={timelineJobs}
                now={now}
                busy={busy}
                maxHeight={mainHeight}
                confirmAction={confirmAction}
                onCancel={(job) => void cancelJob(job)}
                onDelete={(job) => void deleteJob(job)}
              />
            </div>
          </>
        ) : status.role === 'member' && status.member ? (
          <>
            <header className="sync-head">
              <div className="sync-head-main">
                <span className="sync-head-mark is-member">
                  <Icon name="download" size={22} />
                </span>
                <div className="sync-head-id">
                  <div className="sync-head-title">
                    <h1>{status.member.clusterName}</h1>
                    <span className="badge ghost">Follower</span>
                  </div>
                  <div className="sync-head-meta">
                    <span className={`sync-status ${status.member.online ? 'is-ok' : 'is-err'}`}>
                      <span className="dot" />
                      {status.member.online ? '已连接 Leader' : 'Leader 不可达'}
                    </span>
                    <span>
                      Leader {status.member.leaderHost}:{status.member.leaderPort}
                    </span>
                    <span>{status.member.memberName}</span>
                  </div>
                </div>
              </div>
              <div className="sync-head-center">
                {awaitingJobs.length > 0 && (
                  <span className="sync-head-pending">
                    <Icon name="clock" size={13} />
                    {awaitingJobs.length} 个待接受
                  </span>
                )}
              </div>
              <div className="sync-head-actions">
                <button className="ghost sm" onClick={() => void refresh()} disabled={!!busy}>
                  <Icon name="refresh" size={13} />
                  刷新
                </button>
                <button
                  className="ghost sm"
                  onClick={() => setCompareOpen(true)}
                  disabled={!compareSources.length || !compareTargets.length}
                >
                  <Icon name="versions" size={13} />
                  对比
                </button>
                <button
                  className={confirmAction === 'leave' ? 'danger sm' : 'ghost sm'}
                  onClick={() => void doLeave()}
                  disabled={!!busy}
                >
                  <Icon name="close" size={13} />
                  {confirmAction === 'leave' ? '确认退出？' : '退出集群'}
                </button>
              </div>
            </header>

            {statusError && (
              <div className="sync-alert">
                <Icon name="alert" size={15} />
                <span>{statusError}</span>
              </div>
            )}

            <div className="sync-workspace" ref={workspaceRef}>
              <div
                className="sync-panel sync-workspace-main"
                ref={mainRef}
                style={{ flexBasis: `${split * 100}%` }}
              >
                {awaitingJobs.length > 0 && (
                  <PendingList
                    jobs={awaitingJobs}
                    instances={status.member.instances}
                    memberName={status.member.memberName}
                    confirmTargets={confirmTargets}
                    targetOptions={targetOptions}
                    defaults={syncDefaults}
                    busy={busy}
                    confirmAction={confirmAction}
                    onPick={(jobId, targetId) =>
                      setConfirmTargets((prev) => ({ ...prev, [jobId]: targetId }))
                    }
                    onCleanup={(jobId, patch) =>
                      setTargetOptions((prev) => ({ ...prev, [jobId]: patch }))
                    }
                    onConfirm={(job) => void confirmJob(job)}
                    onReject={(job) => void rejectJob(job)}
                  />
                )}
                <LocalInstances instances={status.member.instances} />
              </div>

              {resizeHandle}

              <Timeline
                jobs={timelineJobs}
                now={now}
                busy={busy}
                maxHeight={mainHeight}
                onCancel={(job) => void cancelTargetJob(job)}
                onRetry={(job) => void retryTargetJob(job)}
              />
            </div>
          </>
        ) : (
          <div className="sync-empty is-page">
            <span className="sync-empty-icon">
              <Icon name="alert" size={22} />
            </span>
            <strong>集群状态异常</strong>
            {statusError && <span className="sync-error-text">{statusError}</span>}
            <button className="ghost sm" onClick={onBack}>
              返回首页
            </button>
          </div>
        )}
      </div>

      {bottomSlot &&
        createPortal(
          !status ? (
            <div className="sync-sendbar">
              <span className="sync-sendbar-names">读取中</span>
            </div>
          ) : status.role === 'leader' && status.cluster ? (
            <div className="sync-sendbar">
              <div className="sync-sendbar-status">
                <span className={`sync-sendbar-count ${selectedMembers.length ? 'is-active' : ''}`}>
                  {selectedMembers.length} 台
                </span>
                <span className="sync-sendbar-names" title={selectedMemberNames.join('、')}>
                  {selectedMemberNames.join('、') || '未选择目标机'}
                </span>
              </div>
              <div className="spacer" />
              {sourceRunning && (
                <span className="sync-sendbar-warn">
                  <Icon name="alert" size={13} />
                  源实例运行中
                </span>
              )}
              <button
                className="primary sync-send"
                disabled={!canStart}
                onClick={() => void doStart()}
              >
                <Icon name="send" size={15} />
                {busy === 'start' || preparing ? '正在准备…' : '发送同步请求'}
              </button>
            </div>
          ) : status.role === 'member' && status.member ? (
            <div className="sync-sendbar">
              <div className="sync-sendbar-status">
                <span className="sync-sendbar-count is-active">{status.member.memberName}</span>
                <span className="sync-sendbar-names">
                  {awaitingJobs.length ? `${awaitingJobs.length} 个待确认` : '已连接'}
                </span>
              </div>
              <div className="spacer" />
              <button className="ghost sm" onClick={() => void refresh()} disabled={!!busy}>
                <Icon name="refresh" size={13} />
                刷新
              </button>
            </div>
          ) : (
            <div className="sync-sendbar">
              <span className="sync-sendbar-names">未加入集群</span>
              <div className="spacer" />
              <button className="ghost sm" onClick={() => void refresh()}>
                <Icon name="refresh" size={13} />
                刷新
              </button>
            </div>
          ),
          bottomSlot,
        )}

      {wheelGate && (
        <SyncDialog
          title="自定义依赖缺少文件"
          wide
          onClose={() => {
            if (busy === 'start') return
            setWheelGate(null)
          }}
        >
          <div className="sync-alert is-warn">
            <Icon name="alert" size={15} />
            <span>
              这些自定义依赖在源端找不到可同步的文件，目标端装不上。把对应的 .whl
              放进「设置 → 本地 Wheel 库」后点「继续」重新检查；也可以确认跳过——跳过意味着这些依赖不会安装，
              目标端 ComfyUI 可能启动失败或缺功能。
            </span>
          </div>
          <div className="sync-dialog-meta">
            <span className={`badge ${wheelGate.library ? 'ghost' : 'warn'}`}>
              {wheelGate.library ? `wheel 库：${wheelGate.library}` : '尚未设置本地 Wheel 库'}
            </span>
            {wheelGate.retried && <span className="badge warn">重新检查后仍缺少</span>}
          </div>
          <ul className="sync-wheel-missing">
            {wheelGate.missing.map((wheel) => (
              <li key={`${wheel.name}-${wheel.version}`}>
                <div className="sync-wheel-missing-name">
                  <span>{wheel.name}</span>
                  <code>{wheel.version}</code>
                  {wheel.filename && <code>{wheel.filename}</code>}
                </div>
                <div className="sync-wheel-missing-reason">{wheel.reason}</div>
              </li>
            ))}
          </ul>
          <div className="sync-modal-actions">
            <button
              className="ghost"
              onClick={() => setWheelGate(null)}
              disabled={busy === 'start'}
            >
              取消
            </button>
            <button
              className="danger"
              onClick={() => {
                setWheelGate(null)
                void doStart(true)
              }}
              disabled={busy === 'start'}
              title="这些依赖不会安装到目标端"
            >
              跳过
            </button>
            <button className="primary" onClick={retryWheelGate} disabled={busy === 'start'}>
              {busy === 'start' ? '检查中…' : '继续'}
            </button>
          </div>
        </SyncDialog>
      )}

      {changesOpen && (
        <SyncDialog title="自上次同步的变化" wide onClose={() => setChangesOpen(false)}>
          {changes ? (
            <>
              <div className="sync-dialog-meta">
                <span className={`badge ${changes.hasBaseline ? 'ghost' : 'warn'}`}>
                  {changes.hasBaseline ? '已有基线' : '尚无基线'}
                </span>
                {changes.lastSyncAt && <span>{dateClock(changes.lastSyncAt)}</span>}
                <div className="spacer" />
                <button className="ghost sm" onClick={() => void loadChanges(false)} disabled={changesBusy}>
                  <Icon name="refresh" size={13} />
                  {changesBusy ? '扫描中…' : '刷新'}
                </button>
              </div>
              <DiffSummary diff={changes.diff} />
              <DiffDetails diff={changes.diff} />
            </>
          ) : (
            <div className="sync-empty is-compact">
              <strong>暂无变化数据</strong>
            </div>
          )}
        </SyncDialog>
      )}

      {compareOpen && status && (status.role === 'leader' || status.role === 'member') && (
        <SyncDialog title="实例对比" wide onClose={() => setCompareOpen(false)}>
          <ComparePanel
            sources={compareSources}
            targets={compareTargets}
            sourceId={compareSourceId}
            targetId={compareTargetId}
            busy={busy}
            comparison={comparison}
            onChangeSource={(id) => {
              setCompareSourceId(id)
              setComparison(null)
            }}
            onChangeTarget={(id) => {
              setCompareTargetId(id)
              setComparison(null)
            }}
            onCompare={() => void doCompare()}
          />
        </SyncDialog>
      )}

      {deleteOpen && (
        <SyncDialog
          title="删除集群"
          narrow
          onClose={() => {
            if (busy === 'delete') return
            setDeleteOpen(false)
          }}
        >
          <form
            className="sync-delete-form"
            onSubmit={(e) => {
              e.preventDefault()
              void doDelete()
            }}
          >
            <label className="field">
              <span>集群密码</span>
              <input
                type="password"
                autoFocus
                autoComplete="current-password"
                value={deletePassword}
                onChange={(e) => {
                  setDeletePassword(e.target.value)
                  setDeleteError('')
                }}
              />
            </label>
            {deleteError && (
              <div className="sync-alert is-error">
                <Icon name="alert" size={15} />
                <span>{deleteError}</span>
              </div>
            )}
            <div className="sync-modal-actions">
              <button
                type="button"
                className="ghost"
                disabled={busy === 'delete'}
                onClick={() => setDeleteOpen(false)}
              >
                取消
              </button>
              <button
                type="submit"
                className="danger"
                disabled={busy === 'delete' || !deletePassword}
              >
                {busy === 'delete' ? '删除中…' : '删除集群'}
              </button>
            </div>
          </form>
        </SyncDialog>
      )}
    </>
  )
}
