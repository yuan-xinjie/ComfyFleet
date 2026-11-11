import { useCallback, useEffect, useMemo, useState } from 'react'
import { api, errText } from '../lib/api'
import type { InstanceConfig, RepoWheel, VersionEvent, WheelRepoList } from '../types'
import Icon from '../components/Icon'
import OperationDialog, {
  operationPercent,
  operationStageLabel,
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'
import WheelPackageDialog, { type WheelGroup } from '../components/WheelPackageDialog'

/**
 * 设置页里的「其他轮子」：云端仓库按包名聚合成卡片。
 * 卡片只展示可用版本和数量；点开卡片才进入版本选择、下载和安装。
 */
interface Props {
  inst: InstanceConfig
  running: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
}

interface CardProgress {
  state: 'running' | 'ok' | 'failed'
  message: string
  percent: number | null
}

interface PendingInstall {
  wheel: RepoWheel
  force: boolean
}

function sortWheels(a: RepoWheel, b: RepoWheel): number {
  return b.version.localeCompare(a.version, undefined, { numeric: true })
}

function groupProgress(
  group: WheelGroup,
  progress: Record<string, CardProgress>,
): CardProgress | null {
  const states = group.wheels
    .map((wheel) => progress[wheel.path])
    .filter((state): state is CardProgress => Boolean(state))
  return states.find((state) => state.state === 'running') ?? states[states.length - 1] ?? null
}

export default function WheelPanel({ inst, running, notify }: Props) {
  const [repo, setRepo] = useState<WheelRepoList | null>(null)
  const [repoBusy, setRepoBusy] = useState(false)
  const [repoError, setRepoError] = useState<string | null>(null)
  const [openGroupName, setOpenGroupName] = useState<string | null>(null)
  const [downloading, setDownloading] = useState<string | null>(null)
  const [downloadError, setDownloadError] = useState<string | null>(null)
  const [progress, setProgress] = useState<Record<string, CardProgress>>({})

  /** 安装弹窗：一次只装一个 */
  const [pick, setPick] = useState<PendingInstall | null>(null)
  const [phase, setPhase] = useState<'idle' | 'running' | 'ok' | 'failed'>('idle')
  const [failMsg, setFailMsg] = useState<string | null>(null)
  const [opEvents, setOpEvents] = useState<OperationLine[]>([])
  const [opPhase, setOpPhase] = useState('准备中')
  const [opTab, setOpTab] = useState<OperationTab>('detail')

  const loadRepo = useCallback(
    async (refresh: boolean) => {
      setRepoBusy(true)
      setRepoError(null)
      try {
        const next = await api.wheelRepoList(inst.id, refresh)
        setRepo(next)
        if (next.error) notify('err', next.error)
      } catch (e) {
        setRepoError(errText(e))
      } finally {
        setRepoBusy(false)
      }
    },
    // 通知函数变化不应该触发一次仓库网络请求。
    [inst.id],
  )

  useEffect(() => {
    void loadRepo(false)
  }, [loadRepo])

  const closeGroup = useCallback(() => {
    setOpenGroupName(null)
    setDownloadError(null)
  }, [])

  const groups = useMemo<WheelGroup[]>(() => {
    const buckets = new Map<string, RepoWheel[]>()
    for (const wheel of repo?.wheels ?? []) {
      const name = wheel.group.trim() || '未分组'
      const list = buckets.get(name)
      if (list) list.push(wheel)
      else buckets.set(name, [wheel])
    }
    return [...buckets.entries()]
      .map(([name, wheels]) => {
        const compatible = wheels.filter((wheel) => wheel.compatible).sort(sortWheels)
        const incompatible = wheels.filter((wheel) => !wheel.compatible).sort(sortWheels)
        return { name, wheels, compatible, incompatible }
      })
      .sort((a, b) => a.name.localeCompare(b.name, 'zh-CN'))
  }, [repo])

  const openGroup = openGroupName
    ? groups.find((group) => group.name === openGroupName) ?? null
    : null

  const startInstall = async (nextPick: PendingInstall) => {
    const { wheel, force } = nextPick
    setPick(nextPick)
    setOpenGroupName(null)
    setFailMsg(null)
    setDownloadError(null)
    setOpEvents([])
    setOpPhase(force ? '强制安装' : '准备安装')
    setOpTab('detail')
    setPhase('running')
    setProgress((prev) => ({
      ...prev,
      [wheel.path]: { state: 'running', message: '准备安装…', percent: null },
    }))
    try {
      const onEvent = (ev: VersionEvent) => {
        setOpEvents((prev) =>
          [
            ...prev,
            {
              at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
              step: operationStageLabel(ev.stage),
              phase: ev.failed ? 'err' : ev.level === 'warn' ? 'warn' : ev.done ? 'done' : 'log',
              message: ev.message,
            },
          ].slice(-600),
        )
        setOpPhase(ev.stage === 'console' ? '安装轮子' : operationStageLabel(ev.stage))
        setProgress((prev) => ({
          ...prev,
          [wheel.path]: {
            state: ev.failed ? 'failed' : ev.done ? 'ok' : 'running',
            message: ev.message,
            percent: operationPercent(ev.message) ?? prev[wheel.path]?.percent ?? null,
          },
        }))
      }
      await api.wheelRepoInstall(inst.id, wheel.path, onEvent)
      setPhase('ok')
      setOpPhase('安装完成')
      setProgress((prev) => ({
        ...prev,
        [wheel.path]: { state: 'ok', message: '安装完成，重启 ComfyUI 后生效', percent: 100 },
      }))
      notify('ok', `${wheel.group || wheel.name} 装好了，重启 ComfyUI 生效`)
      // 安装可能改动了环境；重新按最新环境标兼容性，但不再访问仓库。
      void loadRepo(false)
    } catch (e) {
      const message = errText(e)
      setFailMsg(message)
      setOpPhase('安装失败')
      setPhase('failed')
      setProgress((prev) => ({
        ...prev,
        [wheel.path]: { state: 'failed', message, percent: null },
      }))
    }
  }

  const download = async (wheel: RepoWheel) => {
    let dir: string | null = null
    try {
      dir = await api.pickFolder()
    } catch (e) {
      const message = errText(e)
      setDownloadError(message)
      notify('err', message)
      return
    }
    if (!dir) return
    setDownloading(wheel.path)
    setDownloadError(null)
    try {
      const saved = await api.wheelRepoDownload(wheel.path, dir)
      notify('ok', `已保存到 ${saved}`)
    } catch (e) {
      const message = errText(e)
      setDownloadError(message)
      notify('err', message)
    } finally {
      setDownloading(null)
    }
  }

  const installBlocked = running
    ? '实例正在运行：先停止再安装，装完重启 ComfyUI 生效。'
    : phase === 'running'
      ? '另一个轮子正在安装，等它结束再装。'
      : null

  const totalCompatible = groups.reduce((sum, group) => sum + group.compatible.length, 0)

  return (
    <div className="panel settings-clean wheel-panel">
      <div className="panel-head">
        <h2>其他轮子</h2>
        <div className="spacer" />
        {repo?.fetchedAt ? (
          <span className="hint">
            {repo.fromCache ? '缓存' : '已更新'} ·{' '}
            {new Date(repo.fetchedAt * 1000).toLocaleString('zh-CN', { hour12: false })}
          </span>
        ) : null}
        <button className="ghost" disabled={repoBusy} onClick={() => void loadRepo(true)}>
          <Icon name="refresh" size={13} />
          {repoBusy ? '刷新中…' : '刷新仓库'}
        </button>
      </div>

      <div className="panel-body">
        {running && (
          <div className="banner warn">
            <span className="mark">
              <Icon name="alert" size={13} />
            </span>
            <div>实例正在运行：先停止再安装，装完重启 ComfyUI 生效。</div>
          </div>
        )}

        {repoError && <div className="banner err">{repoError}</div>}
        {repo?.error && (
          <div className="banner warn">
            <span className="mark">
              <Icon name="alert" size={13} />
            </span>
            <div>刷新失败，仍显示上次缓存：{repo.error}</div>
          </div>
        )}

        {repo?.repoUrl && (
          <div className="wheel-panel-source">
            <span className="mono" title={repo.repoUrl}>
              {repo.repoUrl}
            </span>
            <span className="wheel-panel-count">
              {totalCompatible} 个可用于当前环境 · 共 {repo.total} 个 wheel
            </span>
          </div>
        )}

        {repoBusy && !repo && (
          <div className="wheel-package-grid" aria-label="正在读取云端轮子仓库">
            {Array.from({ length: 6 }).map((_, index) => (
              <div className="wheel-package-skeleton" key={index}>
                <span />
                <span />
                <span />
              </div>
            ))}
          </div>
        )}

        {repo && !repo.repoUrl && repo.hint && (
          <div className="banner info">
            <span className="mark">
              <Icon name="info" size={13} />
            </span>
            <div>{repo.hint}</div>
          </div>
        )}

        {repo?.repoUrl && groups.length > 0 && (
          <div className="wheel-package-grid">
            {groups.map((group) => {
              const versions = [
                ...new Set(group.compatible.map((wheel) => wheel.version).filter(Boolean)),
              ]
              const state = groupProgress(group, progress)
              return (
                <article
                  className={`wheel-package-card${group.compatible.length ? '' : ' is-empty'}${
                    state ? ` is-${state.state}` : ''
                  }`}
                  key={group.name}
                  role="button"
                  tabIndex={0}
                  aria-label={`打开 ${group.name} 的轮子详情`}
                  onClick={() => {
                    setDownloadError(null)
                    setOpenGroupName(group.name)
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== 'Enter' && event.key !== ' ') return
                    event.preventDefault()
                    setDownloadError(null)
                    setOpenGroupName(group.name)
                  }}
                >
                  <div className="wheel-package-top">
                    <h3 title={group.name}>{group.name}</h3>
                    <span className={`badge ${group.compatible.length ? 'ok' : 'idle'}`}>
                      <span className="dot" />
                      {group.compatible.length ? '有兼容' : '无兼容'}
                    </span>
                  </div>

                  <div className="wheel-package-versions">
                    {versions.length > 0 ? (
                      <span className="wheel-version-list">
                        {versions.slice(0, 3).map((version) => (
                          <span className="wheel-version-pill mono" key={version}>
                            {version}
                          </span>
                        ))}
                        {versions.length > 3 && (
                          <span className="wheel-version-more">+{versions.length - 3}</span>
                        )}
                      </span>
                    ) : (
                      <span className="wheel-package-none">当前环境无兼容版本</span>
                    )}
                  </div>

                  <div className="wheel-package-meta">
                    <span>共 {group.wheels.length} 个轮子</span>
                    {group.compatible.length > 0 && (
                      <span className="wheel-package-compat">
                        {group.compatible.length} 个兼容
                      </span>
                    )}
                    <span className="wheel-package-chevron" aria-hidden="true">
                      <Icon name="chevron" size={12} />
                    </span>
                  </div>

                  {state && (
                    <div className={`wheel-package-progress is-${state.state}`}>
                      <span className="wheel-package-progress-dot" />
                      <span title={state.message}>{state.message}</span>
                      {state.state === 'running' && state.percent !== null && (
                        <em>{state.percent}%</em>
                      )}
                    </div>
                  )}
                </article>
              )
            })}
          </div>
        )}

        {repo?.repoUrl && !repoBusy && groups.length === 0 && repo.hint && (
          <div className="banner info">
            <span className="mark">
              <Icon name="info" size={13} />
            </span>
            <div>{repo.hint}</div>
          </div>
        )}
      </div>

      {openGroup && (
        <WheelPackageDialog
          key={openGroup.name}
          group={openGroup}
          env={repo?.env ?? null}
          installBlocked={installBlocked}
          downloading={downloading}
          downloadError={downloadError}
          onClose={closeGroup}
          onDownload={(wheel) => void download(wheel)}
          onInstall={(wheel, force) => void startInstall({ wheel, force })}
        />
      )}

      {pick && phase !== 'idle' && (
        <OperationDialog
          title={
            phase === 'running'
              ? `${pick.force ? '强制安装' : '安装'} ${pick.wheel.group || pick.wheel.name}`
              : phase === 'ok'
                ? '安装完成'
                : '安装失败'
          }
          target={pick.wheel.name}
          state={phase}
          phase={opPhase}
          facts={[
            { label: '包', value: pick.wheel.group || pick.wheel.name, mono: true },
            { label: '版本', value: pick.wheel.version || '—', mono: true },
            { label: '文件', value: pick.wheel.name, mono: true },
            { label: '兼容性', value: pick.force ? '不兼容 · 强制安装' : '兼容' },
          ]}
          events={opEvents}
          tab={opTab}
          onTab={setOpTab}
          detailLabel="安装详情"
          allowHideOnRun={false}
          onClose={() => {
            setPick(null)
            setPhase('idle')
          }}
        >
          {phase === 'ok' && <div className="op-run-live">安装完成，重启 ComfyUI 后生效。</div>}
          {phase === 'failed' && failMsg && <div className="op-run-error">{failMsg}</div>}
        </OperationDialog>
      )}
    </div>
  )
}
