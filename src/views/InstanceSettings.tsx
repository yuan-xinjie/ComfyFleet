import { TextInput, TextArea, Checkbox, DirectoryInput } from '../components/Form'
import Button from '../components/Button'
import IconTextButton from '../components/IconTextButton'
import { useEffect, useMemo, useRef, useState } from 'react'
import Select from '../components/Select'
import Icon from '../components/Icon'
import Modal from '../components/Modal'
import Segmented from '../components/Segmented'
import OperationDialog, {
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'
import { speedFromLog, useBackgroundTask } from '../components/BackgroundTasks'
import { api, errText } from '../lib/api'
import type {
  EngineOption,
  EnvPlan,
  InstallEvent,
  InstallReport,
  InstanceConfig,
  InstanceOverrides,
  PythonProbe,
} from '../types'
import { useTabSave, type TabSaveHandle } from '../lib/useTabSave'

interface Props {
  inst: InstanceConfig
  plan: EnvPlan | null
  onSaved: (updated: InstanceConfig) => void
  notify: (kind: 'ok' | 'err', msg: string) => void
  onSaveState: (state: TabSaveHandle | null) => void
  /** 更换 torch/CUDA/Python 时正在重装环境，灵动岛隐藏启动按钮。 */
  onBusyChange?: (busy: boolean) => void
}

const STEP_LABEL: Record<string, string> = {
  venv: '虚拟环境',
  torch: 'torch 全套',
  triton: 'triton',
  comfy: 'ComfyUI 本体',
  requirements: '依赖 requirements',
  verify: '环境校验',
}

/** 实例里统一用「主.次」；导入时探到的 3.13.6 要显示成 3.13，否则下拉对不上。 */
function pythonMinor(version: string): string {
  const parts = version.trim().split('.')
  return parts.length >= 2 ? `${parts[0]}.${parts[1]}` : version.trim()
}

function parseKv(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const at = line.indexOf('=')
    if (at <= 0) continue
    out[line.slice(0, at).trim()] = line.slice(at + 1).trim()
  }
  return out
}

/** 返回第一条无法解析成 KEY=VALUE 的行号（从 1 开始）。 */
function envInvalidLine(text: string): number | null {
  const lines = text.split('\n')
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim()
    if (!line || line.startsWith('#')) continue
    if (line.indexOf('=') <= 0) return i + 1
  }
  return null
}

function kvToText(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([k, v]) => `${k}=${v}`)
    .join('\n')
}

/** 后端用 BTreeMap 保存 env，读回来按 key 排序；比较时也要用同一顺序。 */
function envTextKey(text: string): string {
  return Object.entries(parseKv(text))
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${k}=${v}`)
    .join('\n')
}

type EngineGroup = { key: string; label: string; engines: EngineOption[] }

/** 和创建向导一致：CUDA 轨道 + 该轨道下的 torch 版本两级选择。 */
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

export default function InstanceSettings({
  inst,
  plan,
  onSaved,
  notify,
  onSaveState,
  onBusyChange,
}: Props) {
  const [name, setName] = useState(inst.name)
  const [engine, setEngine] = useState(inst.engine)
  const [pythonVersion, setPythonVersion] = useState(pythonMinor(inst.pythonVersion))
  const [envText, setEnvText] = useState(kvToText(inst.env))
  const [pypi, setPypi] = useState(inst.overrides?.pypi ?? '')
  const [git, setGit] = useState(inst.overrides?.git ?? '')
  const [hf, setHf] = useState(inst.overrides?.hf ?? '')
  const [hfOffline, setHfOffline] = useState<'inherit' | 'on' | 'off'>(
    inst.overrides?.hfOffline === true ? 'on' : inst.overrides?.hfOffline === false ? 'off' : 'inherit',
  )
  const [autoRestart, setAutoRestart] = useState(inst.autoRestart)
  const [pyMode, setPyMode] = useState<'managed' | 'existing'>(
    inst.python?.mode === 'existing' ? 'existing' : 'managed',
  )
  const [pyPath, setPyPath] = useState(inst.python?.path ?? '')
  const [probe, setProbe] = useState<PythonProbe | null>(null)
  const [probing, setProbing] = useState(false)
  const [relocateOpen, setRelocateOpen] = useState(false)
  const [relocatePath, setRelocatePath] = useState('')
  const [relocateBusy, setRelocateBusy] = useState(false)
  const [relocateError, setRelocateError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [needsConfirm, setNeedsConfirm] = useState(false)
  /** 保存后自动跑安装：控制台里保留完整步骤和原始输出，失败了停在这里方便排查 */
  const [switchLog, setSwitchLog] = useState<{
    title: string
    logs: string[]
    running: boolean
    failed?: boolean
    cancelled?: boolean
  } | null>(null)
  const [cancelling, setCancelling] = useState(false)
  const [switchOpen, setSwitchOpen] = useState(false)
  const [savedConfig, setSavedConfig] = useState<InstanceConfig | null>(null)
  const [opEvents, setOpEvents] = useState<OperationLine[]>([])
  const [opPhase, setOpPhase] = useState('准备中')
  const [opPercent, setOpPercent] = useState(0)
  const [opTab, setOpTab] = useState<OperationTab>('detail')

  useEffect(() => {
    onBusyChange?.(Boolean(switchLog?.running))
  }, [switchLog?.running, onBusyChange])

  useEffect(() => {
    return () => onBusyChange?.(false)
  }, [onBusyChange])

  const currentEngine = plan?.engines.find((e) => e.id === engine) ?? null
  // 矩阵外的 torch 版本没有对应的 engine id：选中的仍是同 CUDA 轨道的通用档，
  // 但下拉必须显示实测值，不能继续显示矩阵推荐值。
  const measuredTorch = engine === inst.engine ? (inst.engineDetail?.torchVersion ?? null) : null
  const engineGroups = plan ? groupEngines(plan.engines) : []
  const cudaKey = currentEngine?.cudaVersion ?? 'cpu'
  const torchChoices = engineGroups.find((g) => g.key === cudaKey)?.engines ?? []
  const pythonChoices = useMemo(() => {
    if (!plan) return []
    return plan.pythons.filter((p) => !currentEngine || currentEngine.pythonSupport.includes(p.version))
  }, [plan, currentEngine])

  const chooseEngine = (id: string) => {
    const e2 = plan?.engines.find((x) => x.id === id)
    setEngine(id)
    if (e2 && !e2.pythonSupport.includes(pythonVersion)) {
      setPythonVersion(e2.pythonSupport[e2.pythonSupport.length - 1])
    }
  }

  const pythonChanged = pythonMinor(inst.pythonVersion) !== pythonVersion
  const envChanged = engine !== inst.engine || pythonChanged

  // 草稿是否动过：关闭前提醒，避免静默丢改动
  const snapshot = useMemo(
    () =>
      JSON.stringify({
        name: inst.name.trim(),
        engine: inst.engine,
        pythonVersion: pythonMinor(inst.pythonVersion),
        pyMode: inst.python?.mode === 'existing' ? 'existing' : 'managed',
        pyPath: (inst.python?.path ?? '').trim(),
        envText: envTextKey(kvToText(inst.env)),
        pypi: (inst.overrides?.pypi ?? '').trim(),
        git: (inst.overrides?.git ?? '').trim(),
        hf: (inst.overrides?.hf ?? '').trim(),
        hfOffline:
          inst.overrides?.hfOffline === true ? 'on' : inst.overrides?.hfOffline === false ? 'off' : 'inherit',
        autoRestart: inst.autoRestart,
      }),
    [inst],
  )
  const missingEnvPath = pyMode === 'existing' && !pyPath.trim()
  const envBadLine = useMemo(() => envInvalidLine(envText), [envText])
  // 与保存时相同的归一化再比较：服务端会 trim 名称/路径、重排 env，
  // 不归一化会导致“保存完还是未保存”。
  const dirty =
    envBadLine !== null ||
    JSON.stringify({
      name: name.trim(),
      engine,
      pythonVersion,
      pyMode,
      pyPath: pyPath.trim(),
      envText: envTextKey(envText),
      pypi: pypi.trim(),
      git: git.trim(),
      hf: hf.trim(),
      hfOffline,
      autoRestart,
    }) !== snapshot

  // 集群同步 / 概览页重读环境会改 instance.json 的 engine 和 pythonVersion。
  // 设置页是常驻挂载的，本地草稿不会自己更新，必须跟着外部值走；
  // 但用户手动改过的字段不能被覆盖（只有本地还等于上次外部值时才同步）。
  const syncedMeta = useRef({
    id: inst.id,
    rootDir: inst.rootDir,
    engine: inst.engine,
    pythonVersion: pythonMinor(inst.pythonVersion),
  })
  useEffect(() => {
    const prev = syncedMeta.current
    const next = {
      id: inst.id,
      rootDir: inst.rootDir,
      engine: inst.engine,
      pythonVersion: pythonMinor(inst.pythonVersion),
    }
    syncedMeta.current = next
    // 重定位会改写 Python/环境变量/启动参数里的路径；只换实例 id 不足以触发同步。
    if (prev.id !== next.id || prev.rootDir !== next.rootDir) {
      setName(inst.name)
      setEngine(inst.engine)
      setPythonVersion(next.pythonVersion)
      setEnvText(kvToText(inst.env))
      setPyMode(inst.python?.mode === 'existing' ? 'existing' : 'managed')
      setPyPath(inst.python?.path ?? '')
      setAutoRestart(inst.autoRestart)
      setProbe(null)
      return
    }
    if (prev.engine !== next.engine && engine === prev.engine) setEngine(next.engine)
    if (prev.pythonVersion !== next.pythonVersion && pythonVersion === prev.pythonVersion) {
      setPythonVersion(next.pythonVersion)
    }
  }, [inst, engine, pythonVersion])

  const chooseExistingFolder = async () => {
    try {
      const path = await api.pickFolder()
      if (!path) return
      setPyPath(path)
      setProbe(null)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const openRelocate = () => {
    if (dirty) {
      notify('err', '先保存当前设置，再重新定位实例目录')
      return
    }
    if (inst.state === 'running' || inst.state === 'starting' || inst.state === 'stopping') {
      notify('err', '先停止实例，再重新定位实例目录')
      return
    }
    setRelocatePath(inst.rootDir)
    setRelocateError(null)
    setRelocateOpen(true)
  }

  const chooseRelocateFolder = async () => {
    try {
      const path = await api.pickFolder()
      if (path) {
        setRelocatePath(path)
        setRelocateError(null)
      }
    } catch (e) {
      setRelocateError(errText(e))
    }
  }

  const doRelocate = async () => {
    const path = relocatePath.trim()
    if (!path) {
      setRelocateError('请填写新的实例目录')
      return
    }
    setRelocateBusy(true)
    setRelocateError(null)
    try {
      const updated = await api.instanceRelocate(inst.id, path)
      setRelocateOpen(false)
      setProbe(null)
      notify('ok', `实例目录已重新定位到 ${updated.rootDir}`)
      onSaved(updated)
    } catch (e) {
      setRelocateError(errText(e))
    } finally {
      setRelocateBusy(false)
    }
  }

  const detect = async () => {
    setProbing(true)
    try {
      const found = await api.instancePythonProbe(inst.id, pyPath.trim() || null)
      setProbe(found)
      if (!found) {
        notify('err', '这个路径下没找到 Python（venv 目录、python.exe、便携版 python_embeded 都行）')
      }
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setProbing(false)
    }
  }

  const finishSwitch = () => {
    const cfg = savedConfig
    setSwitchLog(null)
    setSavedConfig(null)
    if (cfg) onSaved(cfg)
  }

  /**
   * 保存引擎 / Python 后直接跑安装，不再让用户回概览页自己找按钮。
   * 用现有的环境安装链路：venv 存在就复用、torch 按锁定版本替换、ComfyUI 已存在就跳过。
   * 控制台保留每一步和 uv 原始输出，失败时停在这里可复制、可重试。
   */
  const runInstall = async (updated: InstanceConfig) => {
    setBusy(true)
    setCancelling(false)
    setSavedConfig(updated)
    setSwitchOpen(false)
    setSwitchLog({ title: '准备更新环境…', logs: [], running: true })
    setOpEvents([])
    setOpPhase('准备中')
    setOpPercent(0)
    setOpTab('detail')
    try {
      const report: InstallReport = await api.envInstall(inst.id, (ev: InstallEvent) => {
        setSwitchLog((cur) => {
          const base = cur ?? { title: '', logs: [], running: true }
          const line = `${ev.phase === 'log' ? '   ' : '▸ '}${ev.message}`
          const logs = [...base.logs, line].slice(-2000)
          const title =
            ev.phase === 'start' || ev.phase === 'done'
              ? `[${ev.stepIndex}/${ev.stepTotal}] ${STEP_LABEL[ev.step] ?? ev.step}`
              : ev.phase === 'warn'
                ? '提示'
                : base.title
          return { ...base, title, logs }
        })
        setOpEvents((prev) =>
          [
            ...prev,
            {
              at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
              step: STEP_LABEL[ev.step] ?? ev.step,
              phase: ev.phase,
              message: ev.message,
            },
          ].slice(-1200),
        )
        if (ev.stepTotal > 0) {
          setOpPercent(Math.round((Math.min(ev.stepIndex, ev.stepTotal) / ev.stepTotal) * 100))
        }
        if (ev.phase === 'start' || ev.phase === 'done' || ev.phase === 'warn') {
          setOpPhase(STEP_LABEL[ev.step] ?? ev.step)
        }
      })
      setOpPhase(`torch ${report.torchVersion} 已替换`)
      setOpPercent(100)
      setSwitchLog((p) => ({
        ...(p ?? { logs: [] }),
        running: false,
        title: `torch ${report.torchVersion} 已替换`,
        logs: [
          ...(p?.logs ?? []),
          '',
          `完成：Python ${report.pythonVersion} · torch ${report.torchVersion}`,
          report.cudaAvailable ? `CUDA 可用：${report.cudaDevice ?? 'GPU'}` : 'CUDA 不可用（按 CPU 运行）',
        ],
      }))
      notify('ok', `环境更新完成：torch ${report.torchVersion}`)
    } catch (e) {
      const text = errText(e)
      const wasCancelled = text.includes('INSTALL_CANCELLED')
      setOpPhase(wasCancelled ? '环境更新已取消' : '环境更新失败')
      setSwitchLog((p) => ({
        ...(p ?? { logs: [] }),
        running: false,
        failed: !wasCancelled,
        cancelled: wasCancelled,
        title: wasCancelled ? '环境更新已取消' : '环境更新失败',
        logs: [...(p?.logs ?? []), '', text],
      }))
      notify(wasCancelled ? 'ok' : 'err', wasCancelled ? '已取消环境更新' : text)
    } finally {
      setBusy(false)
      setCancelling(false)
    }
  }

  const cancelInstall = async () => {
    setCancelling(true)
    try {
      await api.envCancelInstall(inst.id)
    } catch (e) {
      setCancelling(false)
      notify('err', errText(e))
    }
  }

  const save = async (allowEnvChange: boolean) => {
    if (missingEnvPath) {
      notify('err', '选择已有环境时，必须先填环境路径')
      return
    }
    if (envBadLine !== null) {
      notify('err', `环境变量第 ${envBadLine} 行缺少 KEY=`)
      return
    }
    if (
      envChanged &&
      pyMode === 'managed' &&
      (inst.state === 'running' || inst.state === 'starting' || inst.state === 'stopping')
    ) {
      notify('err', '实例正在运行：先停止，再换 CUDA / torch 或 Python')
      return
    }
    setBusy(true)
    try {
      const overrides: InstanceOverrides = {
        pypi: pypi.trim() || null,
        git: git.trim() || null,
        hf: hf.trim() || null,
        hfOffline: hfOffline === 'inherit' ? null : hfOffline === 'on',
      }
      const updated = await api.instanceUpdate({
        id: inst.id,
        name: name.trim(),
        engine,
        pythonVersion,
        python: { mode: pyMode, path: pyMode === 'existing' ? pyPath.trim() || null : null },
        // 通道、服务与网络、启动参数由各自页面保存；这里不传，后端保持现有值。
        env: parseKv(envText),
        overrides,
        autoRestart,
        allowEnvChange,
      })
      // 后端会把 env 重新排序、名称/路径 trim；用返回值重建草稿基线。
      setName(updated.name)
      setEngine(updated.engine)
      setPythonVersion(pythonMinor(updated.pythonVersion))
      setEnvText(kvToText(updated.env))
      // 引擎 / Python 变了：保存后直接跑安装，弹出完整控制台，不再跳回概览让用户自己找按钮
      if (pyMode === 'managed' && (engine !== inst.engine || pythonChanged)) {
        setNeedsConfirm(false)
        notify('ok', '设置已保存，开始更新环境…')
        await runInstall(updated)
        return
      }
      notify('ok', '实例设置已保存')
      onSaved(updated)
    } catch (e) {
      const text = errText(e)
      if (text.includes('ENV_CHANGE_NEEDS_CONFIRM')) {
        setNeedsConfirm(true)
      } else {
        notify('err', text)
      }
    } finally {
      setBusy(false)
    }
  }

  // 放弃只把草稿恢复成已保存的实例配置，留在当前页；不要切页签。
  const resetDraft = () => {
    setName(inst.name)
    setEngine(inst.engine)
    setPythonVersion(pythonMinor(inst.pythonVersion))
    setEnvText(kvToText(inst.env))
    setPypi(inst.overrides?.pypi ?? '')
    setGit(inst.overrides?.git ?? '')
    setHf(inst.overrides?.hf ?? '')
    setHfOffline(inst.overrides?.hfOffline === true ? 'on' : inst.overrides?.hfOffline === false ? 'off' : 'inherit')
    setAutoRestart(inst.autoRestart)
    setPyMode(inst.python?.mode === 'existing' ? 'existing' : 'managed')
    setPyPath(inst.python?.path ?? '')
    setProbe(null)
    setNeedsConfirm(false)
  }

  useTabSave(
    onSaveState,
    dirty,
    busy,
    () => save(false),
    resetDraft,
    missingEnvPath || envBadLine !== null,
  )

  const copySwitchLog = async () => {
    if (!switchLog) return
    try {
      await navigator.clipboard.writeText(switchLog.logs.join('\n'))
      notify('ok', '安装日志已复制')
    } catch (e) {
      notify('err', errText(e))
    }
  }

  const switchState = switchLog?.running
    ? 'running'
    : switchLog?.failed
      ? 'failed'
      : switchLog?.cancelled
        ? 'cancelled'
        : 'ok'

  useBackgroundTask(
    switchLog
      ? {
          id: `env-settings-${inst.id}`,
          title: switchLog.title || '环境更新',
          phase: opPhase,
          state: switchState,
          percent: opPercent,
          log: opEvents[opEvents.length - 1]?.message,
          speed: speedFromLog(opEvents[opEvents.length - 1]?.message),
          kind: 'install',
          onOpen: () => setSwitchOpen(true),
        }
      : null,
  )

  return (
    <div className="stack">
      <div className="panel settings-clean">
        <header className="panel-head">
          <h2>实例设置</h2>
          <span className="badge ghost">{inst.name}</span>
          {dirty && <span className="badge warn">未保存</span>}
        </header>

        <div className="panel-body settings-body">
          <div className="settings-identity">
            <div className="field settings-name">
              <div className="field-label">
                <label>实例名称</label>
              </div>
              <TextInput aria-label="实例名称" value={name} onChange={(e) => setName(e.target.value)} />
            </div>

            <div className="field settings-directory">
              <div className="field-label">
                <label>实例目录</label>
              </div>
              <div className="inline-input wide">
                <TextInput
                  className="mono"
                  aria-label="实例目录"
                  value={inst.rootDir}
                  readOnly
                  title={inst.rootDir}
                />
                <Button
                  type="button"
                  className="ghost settings-field-action"
                  onClick={() => void openRelocate()}
                  disabled={
                    busy ||
                    relocateBusy ||
                    inst.state === 'running' ||
                    inst.state === 'starting' ||
                    inst.state === 'stopping'
                  }
                  title={
                    inst.state === 'running' ||
                    inst.state === 'starting' ||
                    inst.state === 'stopping'
                      ? '先停止实例，再重新定位目录'
                      : '只改登记路径，不移动磁盘文件'
                  }
                >
                  <Icon name="link" size={13} />
                  重新定位
                </Button>
              </div>
            </div>
          </div>

          <section>
            <h3>
              计算引擎与 Python
              {envChanged && (
                <span className="badge warn">{pyMode === 'managed' ? '保存后自动安装' : '仅记录配置'}</span>
              )}
            </h3>
            <div className="engine-grid">
              <div className="field">
                <div className="field-label">
                  <label>CUDA 版本</label>
                </div>
                <Select
                  ariaLabel="CUDA 版本"
                  value={cudaKey}
                  onChange={(value) => {
                    const group = engineGroups.find((g) => g.key === value)
                    if (group?.engines[0]) chooseEngine(group.engines[0].id)
                  }}
                  disabled={!plan}
                  options={[
                    ...engineGroups.map((g) => ({ value: g.key, label: g.label })),
                    ...(!plan && !engineGroups.some((g) => g.key === cudaKey)
                      ? [{ value: cudaKey, label: cudaKey === 'cpu' ? 'CPU' : `CUDA ${cudaKey}` }]
                      : []),
                  ]}
                />
              </div>
              <div className="field">
                <div className="field-label">
                  <label>torch 版本</label>
                </div>
                <Select
                  ariaLabel="torch 版本"
                  value={engine}
                  onChange={chooseEngine}
                  disabled={!plan}
                  options={[
                    ...torchChoices.map((choice) => ({
                      value: choice.id,
                      label:
                        choice.id === engine && measuredTorch
                          ? `torch ${measuredTorch}`
                          : `torch ${choice.torchVersion}`,
                      badge: choice.driverOk ? undefined : '驱动不匹配',
                      badgeTone: choice.driverOk ? undefined : ('warn' as const),
                    })),
                    ...(!plan && !torchChoices.some((choice) => choice.id === engine)
                      ? [
                          {
                            value: engine,
                            label: measuredTorch ? `torch ${measuredTorch}` : engine,
                          },
                        ]
                      : []),
                  ]}
                />
              </div>
              <div className="field">
                <div className="field-label">
                  <label>Python</label>
                </div>
                <Select
                  ariaLabel="Python 版本"
                  value={pythonVersion}
                  onChange={setPythonVersion}
                  options={[
                    ...(pythonChoices.length > 0
                      ? pythonChoices.map((p) => ({
                          value: p.version,
                          label: p.version,
                        }))
                      : [{ value: pythonVersion, label: pythonVersion }]),
                    ...(pythonChoices.length > 0 &&
                    !pythonChoices.some((p) => p.version === pythonVersion)
                      ? [{ value: pythonVersion, label: pythonVersion, badge: '当前登记' }]
                      : []),
                  ]}
                />
              </div>
              {currentEngine?.torchvision &&
                (!measuredTorch || measuredTorch === currentEngine.torchVersion) && (
                  <div className="engine-lock">
                    <span className="engine-lock-key">套装锁定</span>
                    <span className="mono">torchvision {currentEngine.torchvision}</span>
                    <span className="mono">torchaudio {currentEngine.torchaudio}</span>
                    <span className="mono">torchcodec {currentEngine.torchcodec}</span>
                  </div>
                )}
            </div>
          </section>

          <section>
            <h3>
              Python 环境
              <span className="badge">{pyMode === 'existing' ? '用已有环境' : '启动器自建'}</span>
            </h3>
            <div className="field">
              <Segmented
                value={pyMode}
                options={[
                  { value: 'managed', label: '自建 venv（推荐）' },
                  { value: 'existing', label: '已有环境' },
                ]}
                onChange={(next) => {
                  setPyMode(next)
                  setProbe(null)
                }}
              />
            </div>

            {pyMode === 'existing' && (
              <div className="field">
                <div className="field-label">
                  <label>环境路径</label>
                </div>
                <DirectoryInput
                  className="mono"
                  style={{ flex: 1, width: 'auto' }}
                  aria-label="已有 Python 环境路径"
                  aria-invalid={missingEnvPath}
                  value={pyPath}
                  placeholder="必填，例如 D:\ComfyUI_windows_portable\python_embeded"
                  onChange={(e) => {
                    setPyPath(e.target.value)
                    setProbe(null)
                  }}
                  onPick={() => void chooseExistingFolder()}
                  pickTitle="选择已有环境文件夹"
                  actions={
                    <Button
                      type="button"
                      className="ghost settings-field-action"
                      disabled={probing}
                      onClick={() => void detect()}
                    >
                      {probing ? '检测中…' : '检测'}
                    </Button>
                  }
                />
                {probe && (
                  <span className="hint is-status" style={{ color: 'var(--green)' }}>
                    找到 Python {probe.version || '（版本未知）'}
                  </span>
                )}
              </div>
            )}
          </section>

          <section>
            <h3>环境变量</h3>
            <div className="field">
              <TextArea
                rows={3}
                className="mono"
                aria-label="环境变量" value={envText}
                placeholder={'每行 KEY=VALUE，例如\nPYTORCH_CUDA_ALLOC_CONF=expandable_segments:True'}
                onChange={(e) => setEnvText(e.target.value)}
              />
              {envBadLine !== null && (
                <div style={{ color: 'var(--red)', fontSize: 12, marginTop: 6 }}>
                  第 {envBadLine} 行缺少 =
                </div>
              )}
            </div>
          </section>

          <section>
            <h3>运行</h3>
            <label className="check-row">
              <Checkbox
                type="checkbox"
                checked={autoRestart}
                onChange={(e) => setAutoRestart(e.target.checked)}
              />
              <span>进程退出后自动重启</span>
            </label>
          </section>

          <section>
            <h3>镜像覆盖</h3>
            <div className="settings-mirror">
              <div className="field settings-mirror-wide">
                <div className="field-label">
                  <label>PyPI 索引</label>
                </div>
                <TextInput
                  className="mono"
                  aria-label="PyPI 索引" value={pypi}
                  placeholder="继承全局"
                  onChange={(e) => setPypi(e.target.value)}
                />
              </div>
              <div className="field">
                <div className="field-label">
                  <label>Git 加速前缀</label>
                </div>
                <TextInput
                  className="mono"
                  aria-label="Git 加速前缀" value={git}
                  placeholder="继承全局"
                  onChange={(e) => setGit(e.target.value)}
                />
              </div>
              <div className="field settings-mirror-wide">
                <div className="field-label">
                  <label>HuggingFace 端点</label>
                </div>
                <TextInput
                  className="mono"
                  aria-label="HuggingFace 端点" value={hf}
                  placeholder="继承全局"
                  onChange={(e) => setHf(e.target.value)}
                />
              </div>
              <div className="field">
                <div className="field-label">
                  <label>HuggingFace 离线</label>
                </div>
                <Select
                  ariaLabel="HuggingFace 离线模式"
                  value={hfOffline}
                  onChange={(value) => setHfOffline(value as 'inherit' | 'on' | 'off')}
                  options={[
                    { value: 'inherit', label: '继承全局' },
                    { value: 'on', label: '强制离线（只用本地缓存）' },
                    { value: 'off', label: '强制联网' },
                  ]}
                />
              </div>
            </div>
          </section>
        </div>
      </div>

      {relocateOpen && (
        <Modal
          className="confirm"
          labelledBy="relocate-title"
          dismissible={!relocateBusy}
          onClose={() => setRelocateOpen(false)}
        >
            <div className="modal-head">
              <h2 id="relocate-title">重新定位实例目录？</h2>
            </div>
            <div className="modal-body">
              <div className="field">
                <label>新实例目录</label>
                <DirectoryInput
                  wrapperClassName="inline-input wide"
                  className="mono"
                  aria-label="新实例目录"
                  value={relocatePath}
                  onChange={(e) => {
                    setRelocatePath(e.target.value)
                    setRelocateError(null)
                  }}
                  placeholder="D:\ComfyUI"
                  onPick={() => void chooseRelocateFolder()}
                  pickDisabled={relocateBusy}
                />
              </div>
              <div className="banner warn">
                <span className="mark">
                  <Icon name="alert" size={13} />
                </span>
                <div>
                  新目录必须已有 <code>ComfyUI\main.py</code>；不能与其它实例目录共用或嵌套。
                </div>
              </div>
              {relocateError && (
                <div className="banner err">
                  <span className="mark">
                    <Icon name="alert" size={13} />
                  </span>
                  <div>{relocateError}</div>
                </div>
              )}
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <Button
                className="ghost"
                onClick={() => setRelocateOpen(false)}
                disabled={relocateBusy}
              >
                取消
              </Button>
              <Button className="primary" onClick={() => void doRelocate()} disabled={relocateBusy}>
                {relocateBusy ? '重定位中…' : '确认重定位'}
              </Button>
            </div>
        </Modal>
      )}

      {needsConfirm && (
        <Modal
          className="confirm"
          labelledBy="env-confirm-title"
          dismissible={!busy}
          onClose={() => setNeedsConfirm(false)}
        >
            <div className="modal-head">
              <h2 id="env-confirm-title">{pythonChanged ? '确认更换 Python 版本？' : '确认更换 CUDA / torch？'}</h2>
            </div>
            <div className="modal-body">
              <div className="banner warn">
                <span className="mark">
                  <Icon name="alert" size={13} />
                </span>
                <div>
                  {pyMode === 'existing'
                    ? '这个实例用的是已有环境：只记录配置，不动 venv。'
                    : pythonChanged
                      ? 'Python 版本变了：旧 venv 会备份成 .venv.bak-时间戳，保存后自动建新环境并弹出安装控制台。'
                      : '只换 CUDA / torch 版本：现有 venv 会保留，不会新建环境；保存后自动替换 torch 全套并弹出安装控制台。'}
                </div>
              </div>
            </div>
            <div className="modal-foot">
              <div className="spacer" />
              <Button className="ghost" onClick={() => setNeedsConfirm(false)}>
                取消
              </Button>
              <Button
                className="primary"
                disabled={busy}
                onClick={() => {
                  setNeedsConfirm(false)
                  void save(true)
                }}
              >
                确认并保存
              </Button>
            </div>
        </Modal>
      )}

      {switchLog && switchOpen && (
        <OperationDialog
          title={switchLog.title || '环境更新'}
          target={inst.name}
          state={switchState}
          phase={opPhase}
          percent={opPercent}
          facts={[
            { label: '实例', value: inst.name },
            { label: '步骤', value: opPhase },
            { label: '进度', value: `${opPercent}%`, mono: true },
          ]}
          events={opEvents}
          tab={opTab}
          onTab={setOpTab}
          detailLabel="安装详情"
          closeLabel={switchLog.running ? undefined : '关闭'}
          onClose={() => {
            if (switchLog.running) {
              setSwitchOpen(false)
              return
            }
            finishSwitch()
          }}
          footer={
            <>
              <Button
                className="ghost"
                onClick={() => void copySwitchLog()}
                disabled={switchLog.logs.length === 0}
              >
                复制日志
              </Button>
              {switchLog.failed && !switchLog.running && (
                <Button className="ghost" onClick={() => void runInstall(savedConfig ?? inst)}>
                  重试
                </Button>
              )}
              {switchLog.running && (
                <IconTextButton
                  className="ghost stop"
                  onClick={() => void cancelInstall()}
                  disabled={cancelling}
                  title="终止当前安装步骤并清理子进程"
                 icon="stop" iconSize={13}>
                  
                  {cancelling ? '正在取消…' : '取消安装'}
                </IconTextButton>
              )}
            </>
          }
        >
          <div className="op-run-target">
            <div>
              <span>实例</span>
              <strong>{inst.name}</strong>
            </div>
            <div>
              <span>Python</span>
              <strong className="mono">{savedConfig?.pythonVersion ?? inst.pythonVersion ?? '—'}</strong>
            </div>
            <div>
              <span>torch</span>
              <strong className="mono">{savedConfig?.engineDetail?.torchVersion ?? '—'}</strong>
            </div>
          </div>
          {switchLog.failed && (
            <div className="op-run-error">环境更新失败。</div>
          )}
          {switchLog.cancelled && <div className="op-run-warn">已取消环境更新，当前环境保持原样。</div>}
        </OperationDialog>
      )}
    </div>
  )
}
