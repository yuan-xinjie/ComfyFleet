import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import coreLogo from '../assets/core-logo.svg'
import { api, errText } from '../lib/api'
import { startupDepsTargets } from '../lib/deps'
import type {
  AppInfo,
  CreateInstanceReq,
  EngineId,
  EngineOption,
  EnvPlan,
  EnvReport,
  Health,
  ImportProfilePreview,
  InstanceConfig,
  RemoteTarget,
} from '../types'
import { stateText, stateTone } from '../lib/state'
import { comfyPath } from '../lib/paths'
import type { RunProgress } from '../lib/runProgress'
import { deriveLogTask } from '../lib/runProgress'
import { useInstanceLogStream } from '../lib/useInstanceLogStream'
import Icon from '../components/Icon'
import ModelDirsButton from '../components/ModelDirsButton'
import Select from '../components/Select'
import EnvSummary from '../components/EnvSummary'


interface Props {
  info: AppInfo | null
  plan: EnvPlan | null
  notify: (kind: 'ok' | 'err', msg: string) => void
  instances: InstanceConfig[]
  onCreated: (id: string) => void
  onSelect: (id: string) => void
  onReload: () => Promise<unknown>
  /** 启动自检发现缺运行时时的提示条，由 App 传进来（放在标题上方，不遮挡内容） */
  notice?: ReactNode
}

type Modal = 'create' | 'import' | 'remote' | null

type RemoteForm = {
  id: string | null
  name: string
  host: string
  port: number
  tls: boolean
}

function emptyRemote(): RemoteForm {
  return { id: null, name: '', host: '', port: 8188, tls: false }
}

/** 云端地址预览：IPv6 加方括号，和浏览器里看到的保持一致。 */
function remoteUrl(target: Pick<RemoteTarget, 'host' | 'port' | 'tls'> | null): string {
  if (!target) return ''
  let host = target.host.trim()
  let tls = target.tls
  if (/^https:\/\//i.test(host)) {
    tls = true
    host = host.replace(/^https:\/\//i, '')
  } else if (/^http:\/\//i.test(host)) {
    tls = false
    host = host.replace(/^http:\/\//i, '')
  }
  host = host.replace(/\/+$/, '')
  if (!host) return ''
  const shown = host.includes(':') && !host.startsWith('[') ? `[${host}]` : host
  return `${tls ? 'https' : 'http'}://${shown}:${target.port}`
}

function isRemote(inst: InstanceConfig): boolean {
  return inst.kind === 'remote' || Boolean(inst.remote)
}

function engineLabel(engine: string) {
  if (engine === 'nvidia-cu130') return 'NVIDIA · CUDA 13.0'
  if (engine === 'nvidia-cu129') return 'NVIDIA · CUDA 12.9'
  if (engine === 'cpu') return 'CPU'
  return engine
}

type EngineGroup = { key: string; label: string; engines: EngineOption[] }

/** 把矩阵里的「CUDA 轨道 × torch 版本」拆成两级下拉：先选 CUDA，再选 torch。 */
function groupEngines(engines: EngineOption[]): EngineGroup[] {
  const groups: EngineGroup[] = []
  for (const e of engines) {
    const key = e.cudaVersion ?? 'cpu'
    let g = groups.find((x) => x.key === key)
    if (!g) {
      g = { key, label: e.cudaVersion ? `CUDA ${e.cudaVersion}` : 'CPU', engines: [] }
      groups.push(g)
    }
    g.engines.push(e)
  }
  return groups
}

function address(inst: InstanceConfig) {
  const host = inst.server.listen === '0.0.0.0' ? '127.0.0.1' : inst.server.listen
  return `${host}:${inst.server.port}`
}


function taskStageText(stage: string): string {
  if (stage === '加载模型') return '正在加载模型'
  if (stage === '采样') return '正在采样'
  if (stage === '解码') return '正在解码'
  if (stage === '编码') return '正在编码'
  if (stage === '准备') return '正在准备'
  return '正在执行'
}

/**
 * 卡片中间的任务区：日志里有 tqdm 就按真实步数显示，只有队列信息时给不确定进度；
 * 没有任务（或实例没在跑）就安静地显示空闲。
 */
function InstanceActivity({ inst, health }: { inst: InstanceConfig; health: Health | null }) {
  const running = inst.state === 'running'
  const starting = inst.state === 'starting'
  const { lines } = useInstanceLogStream(inst.id, running, 120)
  const logTask = useMemo(() => deriveLogTask(lines), [lines])
  const progress = useMemo<RunProgress | null>(() => {
    if (!running) return null
    if (logTask.task) return logTask.task
    // 刚提交、还没吐出 tqdm 时，队列接口已经知道有任务了
    if (health && health.queueRunning > 0) {
      // 还没有 tqdm 时，deriveLogTask 的默认阶段是“采样”，但那只是未知；
      // 用“正在执行”避免刚提交就误报正在采样。
      const what = logTask.stage === '采样' ? '正在执行' : taskStageText(logTask.stage)
      return { what, value: 0, max: 0, speed: null }
    }
    return null
  }, [health, logTask.stage, logTask.task, running])

  if (!progress) {
    const pending = health?.queuePending ?? 0
    return (
      <div className="inst-activity idle" aria-label="任务状态">
        <span className="inst-idle-dot" aria-hidden="true" />
        <span className="inst-idle-title">空闲</span>
        <span className="inst-idle-sub">
          {running
            ? pending > 0
              ? `排队 ${pending}`
              : '等待任务'
            : starting
              ? '正在启动'
              : '实例未运行'}
        </span>
      </div>
    )
  }

  const percent =
    progress.max > 0
      ? Math.round(Math.max(0, Math.min(100, (progress.value / progress.max) * 100)))
      : null
  const extras = [
    health && health.queuePending > 0 ? `排队 ${health.queuePending}` : null,
    progress.speed,
  ]
    .filter(Boolean)
    .join(' · ')

  return (
    <div className="inst-activity busy" aria-label={`任务：${progress.what}`}>
      <div className="inst-task-meta">
        <span>{progress.what}</span>
        {extras && <span>{extras}</span>}
      </div>
      <div
        className={`inst-task-track${percent === null ? ' is-indeterminate' : ''}`}
        role="progressbar"
        aria-label="任务进度"
        aria-valuemin={0}
        aria-valuemax={percent === null ? undefined : 100}
        aria-valuenow={percent === null ? undefined : percent}
      >
        <span
          className="inst-task-fill"
          style={percent === null ? undefined : { width: `${percent}%` }}
        />
      </div>
    </div>
  )
}

export default function Home({
  info,
  plan,
  notify,
  instances,
  onCreated,
  onSelect,
  onReload,
  notice,
}: Props) {
  const [modal, setModal] = useState<Modal>(null)
  const [busy, setBusy] = useState(false)
  const [runBusyId, setRunBusyId] = useState<string | null>(null)
  const [form, setForm] = useState<CreateInstanceReq>({
    name: '',
    rootDir: '',
    engine: 'nvidia-cu130',
    pythonVersion: '3.12',
    port: 8188,
    channel: 'stable',
  })

  const [importPath, setImportPath] = useState('')
  const [importName, setImportName] = useState('')
  const [importBusy, setImportBusy] = useState(false)
  const [importPython, setImportPython] = useState('')
  const [importEnv, setImportEnv] = useState<EnvReport | null>(null)
  const [importProfile, setImportProfile] = useState<ImportProfilePreview | null>(null)
  const [profileBusy, setProfileBusy] = useState(false)
  const [envBusy, setEnvBusy] = useState(false)
  const [remoteForm, setRemoteForm] = useState<RemoteForm>(emptyRemote)
  const [removeTarget, setRemoveTarget] = useState<InstanceConfig | null>(null)
  const modalRef = useRef<HTMLDivElement>(null)

  // 探测出来的推荐值直接填好：非技术用户不该被要求先懂引擎矩阵
  useEffect(() => {
    if (!plan) return
    setForm((f) => ({ ...f, engine: plan.recommendedEngine as EngineId, pythonVersion: plan.recommendedPython }))
  }, [plan])

  useEffect(() => {
    if (!info) return
    setForm((f) => (f.rootDir ? f : { ...f, rootDir: `${info.suggestedRoot}\\my-comfy` }))
  }, [info])

  // 路径一改就去读 `.comfyfleet/profile.json`：有就展示将恢复什么，
  // 没有就提前提示“会初始化默认设置”，不用等导入完成才反馈。
  useEffect(() => {
    const path = importPath.trim()
    if (modal !== 'import' || !path) {
      setImportProfile(null)
      setProfileBusy(false)
      return
    }
    let alive = true
    // 路径一变，上一份预览立即作废：否则用户改完路径马上点导入，会拿旧路径的预览去提示。
    setImportProfile(null)
    setProfileBusy(true)
    const timer = window.setTimeout(() => {
      void api
        .instanceImportProfile(path)
        .then((preview) => {
          if (alive) setImportProfile(preview)
        })
        .catch(() => {
          // 路径还没输完整/目录不存在：不抢着报错，交给导入按钮统一提示。
          if (alive) setImportProfile(null)
        })
        .finally(() => {
          if (alive) setProfileBusy(false)
        })
    }, 350)
    return () => {
      alive = false
      window.clearTimeout(timer)
    }
  }, [importPath, modal])

  useEffect(() => {
    if (!modal) return
    const previous = document.activeElement as HTMLElement | null
    const scroller = document.querySelector<HTMLElement>('.content')
    const prevOverflow = scroller?.style.overflowY ?? ''
    if (scroller) scroller.style.overflowY = 'hidden'

    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setModal(null)
        return
      }
      if (e.key !== 'Tab' || !modalRef.current) return
      const focusable = modalRef.current.querySelectorAll<HTMLElement>(
        'button:not(:disabled), input, select, textarea, [tabindex]:not([tabindex="-1"])',
      )
      if (focusable.length === 0) return
      const first = focusable[0]
      const last = focusable[focusable.length - 1]
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault()
        first.focus()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      if (scroller) scroller.style.overflowY = prevOverflow
      previous?.focus()
    }
  }, [modal])

  const engine = plan?.engines.find((e) => e.id === form.engine) ?? null
  const engineGroups = plan ? groupEngines(plan.engines) : []
  const cudaKey = engine?.cudaVersion ?? 'cpu'
  const torchChoices = engineGroups.find((g) => g.key === cudaKey)?.engines ?? []
  const pythons = plan ? plan.pythons.filter((p) => !engine || engine.pythonSupport.includes(p.version)) : []
  const active = instances.filter((i) => i.state === 'running' && !isRemote(i)).length

  // 首页每张卡都要知道队列里有没有任务；一次扫描所有本机实例，避免每张卡各发一轮。
  const [health, setHealth] = useState<Record<string, Health>>({})
  const runningKey = instances
    .filter((i) => i.state === 'running' && !isRemote(i))
    .map((i) => i.id)
    .join(',')
  useEffect(() => {
    if (!runningKey) {
      setHealth({})
      return
    }
    let alive = true
    const pull = () =>
      api
        .instanceHealth()
        .then((map) => {
          if (alive) setHealth(map)
        })
        .catch(() => {})
    void pull()
    const timer = window.setInterval(pull, 3000)
    return () => {
      alive = false
      window.clearInterval(timer)
    }
  }, [runningKey])

  const chooseEngine = (id: string) => {
    const e2 = plan?.engines.find((x) => x.id === id)
    const py =
      e2 && !e2.pythonSupport.includes(form.pythonVersion)
        ? e2.pythonSupport[e2.pythonSupport.length - 1]
        : form.pythonVersion
    setForm((f) => ({ ...f, engine: id as EngineId, pythonVersion: py }))
  }

  const startInstance = async (inst: InstanceConfig) => {
    if (runBusyId) return
    setRunBusyId(inst.id)
    try {
      // 和详情页保持一致：启动前按实例设置处理节点缺依赖。
      const depMode = inst.nodeDepsAuto ?? 'remind'
      if (depMode !== 'off' && inst.comfy.version) {
        const report = await api.nodesDepsStatus(inst.id).catch(() => null)
        const targets = startupDepsTargets(report)
        if (targets.length > 0) {
          if (depMode === 'remind') {
            notify('err', `${targets.length} 个节点缺依赖：在概览的「节点」卡片点「补依赖」可以补上`)
          } else {
            notify('ok', `启动前先补 ${targets.length} 个节点的依赖（不运行 install.py）…`)
            await api.nodesInstallDeps(
              inst.id,
              targets.map((n) => n.name),
              { runInstallPy: false },
              () => undefined,
            )
          }
        }
      }
      const out = await api.instanceStart(inst.id)
      notify('ok', `已启动「${inst.name}」，等待就绪…（${out.url}）`)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setRunBusyId(null)
      await onReload()
    }
  }

  const stopInstance = async (inst: InstanceConfig) => {
    if (runBusyId) return
    setRunBusyId(inst.id)
    try {
      await api.instanceStop(inst.id)
      notify('ok', `已停止「${inst.name}」`)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setRunBusyId(null)
      await onReload()
    }
  }

  const openDir = async (inst: InstanceConfig, child: 'models' | 'custom_nodes' | 'output', label: string) => {
    try {
      await api.openPath(comfyPath(inst, child))
      notify('ok', `已打开「${inst.name}」的${label}`)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  /** 输出目录可能被改到共享目录或自定义位置：按存储设置解析后再打开 */
  const openOutput = async (inst: InstanceConfig) => {
    try {
      const plan = await api.instanceStoragePlan(inst.id)
      await api.openPath(plan.output.path)
      notify('ok', `已打开「${inst.name}」的输出目录`)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const openRemote = async (inst: InstanceConfig) => {
    try {
      const url = await api.instanceOpenUrl(inst.id)
      notify('ok', `已打开「${inst.name}」（${url}）`)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const editRemote = (inst: InstanceConfig) => {
    if (!inst.remote) return
    setRemoteForm({
      id: inst.id,
      name: inst.name,
      host: inst.remote.host,
      port: inst.remote.port,
      tls: inst.remote.tls,
    })
    setModal('remote')
  }

  const openRemoteModal = () => {
    setRemoteForm(emptyRemote())
    setModal('remote')
  }

  const saveRemote = async () => {
    const host = remoteForm.host.trim()
    if (!host) return notify('err', '填云端实例的域名或 IP')
    if (!remoteForm.port || remoteForm.port < 1 || remoteForm.port > 65535) {
      return notify('err', '端口需在 1–65535 之间')
    }
    setBusy(true)
    try {
      const cfg = await api.instanceRemoteSave({
        id: remoteForm.id,
        name: remoteForm.name.trim(),
        host,
        port: remoteForm.port,
        tls: remoteForm.tls,
      })
      setModal(null)
      notify('ok', remoteForm.id ? `已更新「${cfg.name}」` : `已添加云端实例「${cfg.name}」`)
      setRemoteForm(emptyRemote())
      await onReload()
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  const removeNow = async () => {
    if (!removeTarget) return
    const target = removeTarget
    setBusy(true)
    try {
      await api.instanceDelete(target.id, false)
      notify('ok', isRemote(target) ? `已移除云端实例「${target.name}」` : `已移除「${target.name}」`)
      setRemoveTarget(null)
      await onReload()
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  const runAction = (inst: InstanceConfig) => {
    const busy = runBusyId !== null
    const thisBusy = runBusyId === inst.id

    if (inst.state === 'running' || inst.state === 'starting') {
      return {
        kind: 'stop' as const,
        label: thisBusy ? '停止中…' : inst.state === 'starting' ? '取消启动' : '停止',
        disabled: busy,
        title: thisBusy ? '正在停止' : '停止这个实例',
      }
    }
    if (inst.state === 'stopping') {
      return { kind: 'stop' as const, label: '停止中', disabled: true, title: '正在停止' }
    }
    if (!inst.comfy.version) {
      return { kind: 'start' as const, label: '启动', disabled: true, title: '先在实例页安装环境' }
    }
    if (inst.state === 'installing' || inst.state === 'updating') {
      return { kind: 'start' as const, label: '忙碌中', disabled: true, title: '当前任务结束后才能启动' }
    }
    return {
      kind: 'start' as const,
      label: thisBusy ? '启动中…' : '启动',
      disabled: busy,
      title: '启动这个实例',
    }
  }

  const submit = async () => {
    if (!form.name.trim()) return notify('err', '给实例起个名字，比如「出图主力」')
    if (!form.rootDir.trim()) return notify('err', '要一个目录来放 ComfyUI 和环境')
    setBusy(true)
    try {
      const created = await api.instanceCreate({ ...form, name: form.name.trim() })
      setModal(null)
      notify('ok', `已登记「${created.name}」，下一步装环境`)
      setForm((f) => ({ ...f, name: '', rootDir: info ? `${info.suggestedRoot}\\my-comfy` : '' }))
      onCreated(created.id)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  const chooseImportFolder = async (target: 'root' | 'python') => {
    try {
      const path = await api.pickFolder()
      if (!path) return
      if (target === 'root') setImportPath(path)
      else setImportPython(path)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const detectImport = async () => {
    if (!importPath.trim()) return notify('err', '先填已有安装目录，再检测环境')
    setEnvBusy(true)
    setImportEnv(null)
    try {
      setImportEnv(await api.accelScan(importPath.trim(), importPython.trim() || null))
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setEnvBusy(false)
    }
  }

  const doImport = async () => {
    if (!importPath.trim()) return notify('err', '填一个已经装好的 ComfyUI 根目录')
    setImportBusy(true)
    try {
      const cfg = await api.instanceImport(
        importPath.trim(),
        importName.trim() || null,
        importPython.trim() || null,
      )
      setModal(null)
      const profileNote = importProfile?.valid
        ? '，已恢复参数文件'
        : importProfile && !importProfile.found
          ? '，未找到参数文件，已初始化默认设置'
          : importProfile
            ? '，参数文件不可用，已按默认设置导入'
            : ''
      notify('ok', `已导入「${cfg.name}」（${engineLabel(cfg.engine)}）${profileNote}`)
      setImportPath('')
      setImportName('')
      setImportPython('')
      setImportEnv(null)
      setImportProfile(null)
      onCreated(cfg.id)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setImportBusy(false)
    }
  }

  return (
    <>
      <div className="content">
        <div className="home">
          {notice}
          <div className="page-head">
            <div className="page-head-text">
              <h1>实例</h1>
              {instances.length > 0 && (
                <p className="hint">
                  {instances.length} 个实例 · {active} 个运行中
                </p>
              )}
            </div>
            <div className="row">
              <button className="ghost" onClick={openRemoteModal}>
                <Icon name="globe" size={14} />
                添加云端
              </button>
              <button className="ghost" onClick={() => setModal('import')}>
                <Icon name="download" size={14} />
                导入已有安装
              </button>
              <button className="primary" onClick={() => setModal('create')}>
                <Icon name="plus" size={15} />
                新建实例
              </button>
            </div>
          </div>

          {instances.length === 0 ? (
            <div className="empty empty-home">
              <span className="empty-logo-plate">
                <img className="empty-logo" src={coreLogo} alt="ComfyFleet" />
              </span>
              <strong>还没有实例</strong>
              <p className="hint">
                先把 ComfyUI 跑起来：新建一个实例，或者把已经装好的目录登记进来。
              </p>
              <ol className="guide-steps">
                <li className="guide-step">
                  <span className="guide-n">1</span>
                  <div>
                    <strong>建实例</strong>
                    <span className="hint">新建一个空目录，或导入已经装好的 ComfyUI。</span>
                  </div>
                </li>
                <li className="guide-step">
                  <span className="guide-n">2</span>
                  <div>
                    <strong>装环境</strong>
                    <span className="hint">选 NVIDIA · CUDA 引擎，安装 Python、PyTorch 和依赖。</span>
                  </div>
                </li>
                <li className="guide-step">
                  <span className="guide-n">3</span>
                  <div>
                    <strong>启动</strong>
                    <span className="hint">起服务后用浏览器打开；模型和输出目录随时能改。</span>
                  </div>
                </li>
              </ol>
            </div>
          ) : (
            <div className="inst-grid">
              {instances.map((inst) => {
                if (isRemote(inst)) {
                  const url = remoteUrl(inst.remote)
                  return (
                    <article key={inst.id} className="inst-card remote" aria-label={inst.name}>
                      <div className="inst-top">
                        <span className="inst-name">{inst.name}</span>
                        <span className="badge ghost">
                          <Icon name="globe" size={11} />
                          云端
                        </span>
                      </div>
                      <div className="inst-meta">
                        <span className="inst-addr" title={url}>
                          <Icon name="globe" size={12} />
                          <span className="mono">{url}</span>
                        </span>
                      </div>
                      <div className="inst-actions">
                        <button
                          type="button"
                          className="inst-run start"
                          onClick={() => void openRemote(inst)}
                          title={`在浏览器打开 ${url}`}
                          aria-label={`打开「${inst.name}」`}
                        >
                          <Icon name="globe" size={13} />
                          打开
                        </button>
                        <button type="button" className="ghost sm" onClick={() => editRemote(inst)}>
                          编辑
                        </button>
                        <span className="spacer" />
                        <button
                          type="button"
                          className="icon-btn inst-del"
                          onClick={() => setRemoveTarget(inst)}
                          title="移除云端实例"
                          aria-label={`移除「${inst.name}」`}
                        >
                          <Icon name="trash" size={13} />
                        </button>
                      </div>
                    </article>
                  )
                }
                const tone = stateTone(inst.state)
                const run = runAction(inst)
                return (
                  <article
                    key={inst.id}
                    className={`inst-card${inst.state === 'running' ? ' running' : ''}`}
                    aria-label={`${inst.name}，点击打开管理`}
                    tabIndex={0}
                    onClick={(e) => {
                      if ((e.target as HTMLElement).closest('button, a, input, select, textarea')) {
                        return
                      }
                      onSelect(inst.id)
                    }}
                    onKeyDown={(e) => {
                      if (e.target !== e.currentTarget) return
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault()
                        onSelect(inst.id)
                      }
                    }}
                  >
                    <div className="inst-top">
                      <span className="inst-name">{inst.name}</span>
                      <span className={`badge ${tone}`}>
                        <span className="dot" />
                        {stateText(inst.state)}
                      </span>
                    </div>

                    <div className="inst-meta">
                      <span className="inst-addr">
                        <Icon name="globe" size={12} />
                        <span className="mono">{address(inst)}</span>
                      </span>
                    </div>

                    <div className="inst-dirs" role="group" aria-label={`${inst.name} 的快捷目录`}>
                      <ModelDirsButton inst={inst} notify={notify} />
                      <button
                        type="button"
                        className="dir-btn"
                        onClick={() => void openDir(inst, 'custom_nodes', '节点目录')}
                        title={`打开节点目录：${comfyPath(inst, 'custom_nodes')}`}
                      >
                        <Icon name="nodes" size={14} />
                        节点目录
                      </button>
                      <button
                        type="button"
                        className="dir-btn"
                        onClick={() => void openOutput(inst)}
                        title="打开这个实例实际使用的输出目录（可能是共享目录）"
                      >
                        <Icon name="folder" size={14} />
                        输出目录
                      </button>
                    </div>

                    <div className="inst-mid">
                      <InstanceActivity inst={inst} health={health[inst.id] ?? null} />
                    </div>

                    <div className="inst-actions">
                      <button
                        type="button"
                        className={`inst-run ${run.kind}`}
                        onClick={() => (run.kind === 'stop' ? void stopInstance(inst) : void startInstance(inst))}
                        disabled={run.disabled}
                        title={run.title}
                        aria-label={`${run.label}「${inst.name}」`}
                      >
                        <Icon name={run.kind === 'stop' ? 'stop' : 'play'} size={13} />
                        {run.label}
                      </button>
                      <button
                        type="button"
                        className="ghost sm"
                        onClick={() => onSelect(inst.id)}
                        aria-label={`管理「${inst.name}」`}
                      >
                        管理
                        <Icon name="back" size={12} style={{ transform: 'rotate(180deg)' }} />
                      </button>
                      <span className="spacer" />
                      <button
                        type="button"
                        className="icon-btn inst-del"
                        onClick={() => setRemoveTarget(inst)}
                        disabled={
                          inst.state === 'running' ||
                          inst.state === 'starting' ||
                          inst.state === 'stopping' ||
                          inst.state === 'installing' ||
                          inst.state === 'updating'
                        }
                        title={
                          inst.state === 'running' || inst.state === 'starting'
                            ? '先停止实例再移除'
                            : '只从列表移除，不动磁盘上的 ComfyUI、venv 和模型'
                        }
                        aria-label={`移除「${inst.name}」`}
                      >
                        <Icon name="trash" size={13} />
                      </button>
                    </div>
                  </article>
                )
              })}
            </div>
          )}

        </div>
      </div>

      {modal && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setModal(null)
          }}
        >
          <div className="modal" role="dialog" aria-modal="true" aria-labelledby="modal-title" ref={modalRef}>
            <div className="modal-head">
              <h2 id="modal-title">
                {modal === 'remote'
                  ? remoteForm.id
                    ? '编辑云端实例'
                    : '添加云端实例'
                  : modal === 'create'
                    ? '新建实例'
                    : '导入已有安装'}
              </h2>
              <span className="badge ghost">
                {modal === 'remote' ? '只存地址' : modal === 'create' ? '从零装' : '只登记'}
              </span>
              <div className="spacer" />
              <button className="icon-btn" onClick={() => setModal(null)} title="关闭" aria-label="关闭">
                <Icon name="close" size={16} />
              </button>
            </div>

            {modal === 'remote' ? (
              <>
                <div className="modal-body">
                  <label className="field">
                    <span>实例名称</span>
                    <input
                      autoFocus
                      aria-label="云端实例名称"
                      value={remoteForm.name}
                      placeholder="云端 4090"
                      onChange={(e) => setRemoteForm((f) => ({ ...f, name: e.target.value }))}
                    />
                  </label>
                  <div className="field-row">
                    <label className="field">
                      <span>域名 / IP</span>
                      <input
                        aria-label="云端域名或 IP"
                        value={remoteForm.host}
                        placeholder="comfy.example.com"
                        spellCheck={false}
                        onChange={(e) => setRemoteForm((f) => ({ ...f, host: e.target.value }))}
                      />
                    </label>
                    <label className="field">
                      <span>端口</span>
                      <input
                        aria-label="云端端口"
                        value={remoteForm.port || ''}
                        inputMode="numeric"
                        placeholder="8188"
                        onChange={(e) =>
                          setRemoteForm((f) => ({
                            ...f,
                            port: Number(e.target.value.replace(/\D/g, '')) || 0,
                          }))
                        }
                      />
                    </label>
                  </div>
                  <div className="field">
                    <span>协议</span>
                    <Select
                      ariaLabel="云端协议"
                      value={remoteForm.tls ? 'https' : 'http'}
                      onChange={(value) => setRemoteForm((f) => ({ ...f, tls: value === 'https' }))}
                      options={[
                        { value: 'https', label: 'HTTPS（推荐）' },
                        { value: 'http', label: 'HTTP（内网或自签）' },
                      ]}
                    />
                  </div>
                  <div className="banner info" style={{ marginTop: 2 }}>
                    <span className="mark">
                      <Icon name="globe" size={13} />
                    </span>
                    <div>
                      {remoteUrl(remoteForm) ? (
                        <>
                          浏览器会打开 <code>{remoteUrl(remoteForm)}</code>
                        </>
                      ) : (
                        <>填好域名 / IP 后会显示完整地址</>
                      )}
                    </div>
                  </div>
                </div>
                <div className="modal-foot">
                  <div className="spacer" />
                  <button className="ghost" onClick={() => setModal(null)}>
                    取消
                  </button>
                  {remoteForm.id && (
                    <button
                      className="ghost"
                      onClick={() => {
                        const target = instances.find((i) => i.id === remoteForm.id) ?? null
                        setModal(null)
                        setRemoveTarget(target)
                      }}
                    >
                      移除
                    </button>
                  )}
                  <button className="primary" onClick={() => void saveRemote()} disabled={busy}>
                    {busy ? '保存中…' : remoteForm.id ? '保存修改' : '添加云端'}
                  </button>
                </div>
              </>
            ) : modal === 'create' ? (
              <>
                <div className="modal-body">
                  <div className="field-row">
                    <label className="field">
                      <span>实例名称</span>
                      <input
                        autoFocus
                        aria-label="实例名称"
                        value={form.name}
                        placeholder="出图主力"
                        onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
                      />
                    </label>
                    <label className="field">
                      <span>端口</span>
                      <input
                        aria-label="端口"
                        value={form.port}
                        inputMode="numeric"
                        onChange={(e) =>
                          setForm((f) => ({ ...f, port: Number(e.target.value.replace(/\D/g, '')) || 0 }))
                        }
                      />
                    </label>
                  </div>
                  <label className="field">
                    <span>实例目录</span>
                    <input
                      aria-label="实例目录"
                      value={form.rootDir}
                      placeholder="D:\ComfyUI"
                      onChange={(e) => setForm((f) => ({ ...f, rootDir: e.target.value }))}
                    />
                    <span className="hint">本体、venv、日志都放这里；建议非系统盘。</span>
                  </label>
                  <div className="field-row">
                    <div className="field">
                      <span>CUDA 版本</span>
                      <Select
                        ariaLabel="CUDA 版本"
                        value={cudaKey}
                        onChange={(value) => {
                          const group = engineGroups.find((g) => g.key === value)
                          if (group?.engines[0]) chooseEngine(group.engines[0].id)
                        }}
                        options={engineGroups.map((g) => ({
                          value: g.key,
                          label: g.label,
                        }))}
                      />
                    </div>
                    <div className="field">
                      <span>torch 版本</span>
                      <Select
                        ariaLabel="torch 版本"
                        value={form.engine}
                        onChange={chooseEngine}
                        options={torchChoices.map((choice) => ({
                          value: choice.id,
                          label: `torch ${choice.torchVersion}`,
                          badge: choice.id === plan?.recommendedEngine ? '推荐' : undefined,
                          badgeTone:
                            choice.id === plan?.recommendedEngine
                              ? ('accent' as const)
                              : undefined,
                        }))}
                      />
                    </div>
                  </div>
                  <div className="field">
                    <span>Python</span>
                    <Select
                      ariaLabel="Python"
                      value={form.pythonVersion}
                      onChange={(value) => setForm((f) => ({ ...f, pythonVersion: value }))}
                      options={pythons.map((python) => ({
                        value: python.version,
                        label: python.version,
                      }))}
                    />
                    {engine?.torchvision && (
                      <span className="hint">
                        套装锁定：torchvision {engine.torchvision} · torchaudio {engine.torchaudio} · torchcodec{' '}
                        {engine.torchcodec}
                      </span>
                    )}
                  </div>
                  {engine && !engine.driverOk && engine.driverNote && (
                    <div className="banner warn">
                      <span className="mark">
                        <Icon name="alert" size={13} />
                      </span>
                      <div>{engine.driverNote}</div>
                    </div>
                  )}
                </div>
                <div className="modal-foot">
                  <div className="spacer" />
                  <button className="ghost" onClick={() => setModal(null)}>
                    取消
                  </button>
                  <button
                    className="primary"
                    onClick={() => void submit()}
                    disabled={busy || !form.name.trim()}
                    title={!form.name.trim() ? '先填实例名称' : undefined}
                  >
                    {busy ? '登记中…' : '创建'}
                  </button>
                </div>
              </>
            ) : (
              <>
                <div className="modal-body">
                  <p className="hint">选已有 ComfyUI 的<strong>根目录</strong>；只登记，不改文件。</p>
                  <div className="field">
                    <span>已有安装目录</span>
                    <div className="inline-input">
                      <input
                        autoFocus
                        style={{ flex: 1, width: 'auto' }}
                        aria-label="已有安装目录"
                        value={importPath}
                        placeholder="D:\ComfyUI_windows_portable\ComfyUI"
                        onChange={(e) => setImportPath(e.target.value)}
                      />
                      <button
                        type="button"
                        className="ghost sm"
                        onClick={() => void chooseImportFolder('root')}
                      >
                        <Icon name="folder" size={13} />
                        选择
                      </button>
                    </div>
                    <span className="hint">
                      目录里要有 <code>ComfyUI\main.py</code>。
                    </span>
                  </div>
                  <div className="field">
                    <span>Python 环境（可留空）</span>
                    <div className="inline-input">
                      <input
                        style={{ flex: 1, width: 'auto' }}
                        aria-label="Python 环境路径"
                        value={importPython}
                        placeholder="D:\ComfyUI\venv 或 ...\python_embeded\python.exe"
                        onChange={(e) => setImportPython(e.target.value)}
                      />
                      <button
                        type="button"
                        className="ghost sm"
                        onClick={() => void chooseImportFolder('python')}
                      >
                        <Icon name="folder" size={13} />
                        选择
                      </button>
                    </div>
                    <span className="hint">留空会自动查找 venv / python_embeded。</span>
                  </div>
                  <div className="row import-detect">
                    <button
                      className="ghost sm"
                      onClick={() => void detectImport()}
                      disabled={envBusy || !importPath.trim()}
                    >
                      {envBusy ? '检测中…' : '检测环境'}
                    </button>

                  </div>
                  {importEnv && <EnvSummary env={importEnv} />}
                  {profileBusy && <div className="hint">正在读取参数文件…</div>}
                  {!profileBusy && importProfile?.valid && (
                    <div className="banner info">
                      <span className="mark">
                        <Icon name="info" size={13} />
                      </span>
                      <div>
                        <strong>找到参数文件</strong>
                        ：将恢复「{importProfile.name?.trim() || '该目录'}」的
                        {[
                          importProfile.engine ? engineLabel(importProfile.engine) : null,
                          importProfile.pythonVersion ? `Python ${importProfile.pythonVersion}` : null,
                          importProfile.extraArgs > 0 ? `启动参数 ${importProfile.extraArgs} 条` : null,
                          importProfile.envVars > 0 ? `环境变量 ${importProfile.envVars} 项` : null,
                          importProfile.autoRestart ? '自动重启' : null,
                          importProfile.nodeDepsAuto
                            ? `节点依赖 ${importProfile.nodeDepsAuto}`
                            : null,
                        ]
                          .filter(Boolean)
                          .join(' · ')}
                        <div className="hint">
                          存储目录、引擎等实例设置会一起恢复；下面填写的显示名称和 Python 环境会覆盖参数文件里的对应项。
                        </div>
                        {importProfile.warnings.length > 0 && (
                          <div className="banner warn" style={{ marginTop: 8 }}>
                            <span className="mark">
                              <Icon name="alert" size={13} />
                            </span>
                            <div>
                              <strong>导入时会做安全处理</strong>
                              <ul style={{ margin: '4px 0 0', paddingLeft: 18 }}>
                                {importProfile.warnings.map((warning) => (
                                  <li key={warning}>{warning}</li>
                                ))}
                              </ul>
                            </div>
                          </div>
                        )}
                      </div>
                    </div>
                  )}
                  {!profileBusy && importProfile && importProfile.found && !importProfile.valid && (
                    <div className="banner warn">
                      <span className="mark">
                        <Icon name="alert" size={13} />
                      </span>
                      <div>
                        <strong>参数文件读取失败</strong>
                        ：{importProfile.error ?? '文件格式损坏'}
                        <div className="hint">导入后会初始化默认设置，并重新生成参数文件。</div>
                      </div>
                    </div>
                  )}
                  {!profileBusy && importProfile && !importProfile.found && (
                    <div className="banner warn">
                      <span className="mark">
                        <Icon name="alert" size={13} />
                      </span>
                      <div>
                        <strong>没有参数配置文件</strong>
                        ：这个目录里没有 <code>.comfyfleet\profile.json</code>。
                        <div className="hint">
                          导入后将会初始化设置（端口 8189、无额外启动参数、默认存储），之后可以再修改。
                        </div>
                      </div>
                    </div>
                  )}
                  <label className="field">
                    <span>显示名称（可留空）</span>
                    <input
                      aria-label="显示名称"
                      value={importName}
                      placeholder={
                        importProfile?.valid && importProfile.name
                          ? `留空则用「${importProfile.name}」`
                          : '沿用目录名'
                      }
                      onChange={(e) => setImportName(e.target.value)}
                    />
                  </label>
                </div>
                <div className="modal-foot">
                  <div className="spacer" />
                  <button className="ghost" onClick={() => setModal(null)}>
                    取消
                  </button>
                  <button className="primary" onClick={() => void doImport()} disabled={importBusy}>
                    {importBusy ? '识别中…' : '导入'}
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      )}

      {removeTarget && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setRemoveTarget(null)
          }}
        >
          <div className="modal confirm" role="dialog" aria-modal="true" aria-labelledby="remove-instance-title">
            <div className="modal-head">
              <h2 id="remove-instance-title">
                {isRemote(removeTarget) ? '移除云端实例？' : '移除实例？'}
              </h2>
            </div>
            <div className="modal-body">
              <p className="hint">
                {isRemote(removeTarget)
                  ? `只移除「${removeTarget.name}」这条地址，不动远端服务器。`
                  : `只移除「${removeTarget.name}」的登记，磁盘文件不动。`}
              </p>
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <button className="ghost" onClick={() => setRemoveTarget(null)}>
                取消
              </button>
              <button className="danger solid" onClick={() => void removeNow()} disabled={busy}>
                <Icon name="trash" size={14} />
                {busy ? '移除中…' : '确认移除'}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  )
}
