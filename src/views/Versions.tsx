import { useCallback, useEffect, useState } from 'react'
import { api, errText } from '../lib/api'
import type {
  DevCommit,
  TagInfo,
  VersionDirtyFile,
  VersionEvent,
  VersionInfo,
} from '../types'
import Icon from '../components/Icon'
import Segmented from '../components/Segmented'
import OperationDialog, {
  operationStageLabel,
  type OperationLine,
  type OperationTab,
} from '../components/OperationDialog'

interface Props {
  instanceId: string
  instanceName: string
  /** 运行中的实例不允许切换 / 更新，界面上就要灰掉，而不是等后端拒绝 */
  canSwitch: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
  /** 通道 / 版本变了：让父级重读 instance.json，概览页的通道别停在旧值 */
  onChanged?: () => void
}

/** 开发版固定跟随 master；稳定版 = 停在某个发版 tag 上 */
const DEV_BRANCH = 'master'
/** 只在名字像版本号时才当"最新稳定版"，远端还有会移动的 latest tag */
const VERSION_TAG = /^v\d+(\.\d+)+$/

/** git 的 iso-local 带时区；列表里只显示到秒，拿不到时间就给占位 */
function tagDateText(date: string): string {
  return date ? date.replace(/ ([+-]\d{4})$/, '') : '—'
}

type Channel = 'stable' | 'dev'
/** tag = 稳定版某个发版；branch = 跟随开发版；commit = 开发版历史里的某一刻 */
type SwitchKind = 'tag' | 'branch' | 'commit'
interface Pending {
  target: string
  kind: SwitchKind
  label: string
  clean: boolean
}
type ConsoleState = 'running' | 'ok' | 'failed' | 'cancelled'

/** 把事件变成日志行：控制台输出原样保留，自己的结论加标记 */
function logLine(ev: VersionEvent): string[] {
  if (ev.stage === 'console') return [ev.message]
  const mark = ev.failed ? '✗' : ev.level === 'warn' ? '!' : ev.done ? '✓' : '»'
  return ev.message.split('\n').map((line) => `${mark} [${ev.stage}] ${line}`)
}

const DIRTY_STATUS: Record<string, string> = {
  '??': '未跟踪',
  M: '修改',
  A: '新增',
  D: '删除',
  R: '重命名',
  C: '复制',
  U: '冲突',
  T: '类型变更',
}

function dirtyStatusLabel(status: string): string {
  return DIRTY_STATUS[status] ?? DIRTY_STATUS[status.slice(0, 1)] ?? status
}

/** 确认前复查用：路径/状态任一变化都要求用户重新看一遍清单。 */
function dirtyFilesKey(files: VersionDirtyFile[]): string {
  return files
    .map((file) => `${file.status}\u0000${file.path}`)
    .sort()
    .join('\u0001')
}

function DirtyFileList({ files }: { files: VersionDirtyFile[] }) {
  if (files.length === 0) {
    return <div className="hint" style={{ marginTop: 6 }}>没有可清理的文件</div>
  }
  const shown = files.slice(0, 120)
  return (
    <div className="version-dirty-files">
      <ul className="version-dirty-list">
        {shown.map((file) => (
          <li key={`${file.status}:${file.path}`}>
            <span className="version-dirty-status">{dirtyStatusLabel(file.status)}</span>
            <code>{file.path}</code>
          </li>
        ))}
      </ul>
      {files.length > shown.length && (
        <div className="hint">还有 {files.length - shown.length} 个文件未展开</div>
      )}
    </div>
  )
}

/** 读取版本信息时占住记录行的位置，别让面板先塌成一句「读取中…」。 */
function VersionInfoSkeleton() {
  const rows = [
    [56, 168],
    [56, 240],
    [56, 128],
    [56, 208],
  ]
  return (
    <div className="records" aria-hidden="true">
      {rows.map(([label, value], index) => (
        <div className="record" key={index}>
          <span className="skeleton" style={{ width: label, height: 11 }} />
          <span className="skeleton" style={{ width: value, height: 12 }} />
        </div>
      ))}
    </div>
  )
}

/** 标签 / 提交列表的读取骨架：五行，右侧保留切换按钮的位置。 */
function TagListSkeleton() {
  return (
    <div className="tag-list" aria-hidden="true">
      {[0, 1, 2, 3, 4].map((index) => (
        <div className="tag-row tag-skeleton" key={index}>
          <span className="skeleton" style={{ width: 108, height: 12 }} />
          <span className="skeleton" style={{ width: 86, height: 11 }} />
          <div className="tag-spacer" />
          <span className="skeleton" style={{ width: 46, height: 24, borderRadius: 8 }} />
        </div>
      ))}
    </div>
  )
}

/** 清理清单用浮层卡片展示，不挤占版本面板的布局 */function DirtyFilesDialog({
  files,
  onClose,
}: {
  files: VersionDirtyFile[]
  onClose: () => void
}) {
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div
        className="modal version-dirty-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="dirty-files-title"
      >
        <div className="modal-head">
          <Icon name="alert" size={15} />
          <h2 id="dirty-files-title">切版本前会清理这些文件</h2>
          <span className="badge warn">{files.length} 个</span>
          <div className="spacer" />
          <button className="icon-btn" onClick={onClose} title="关闭" aria-label="关闭">
            <Icon name="close" size={16} />
          </button>
        </div>
        <div className="modal-body">
          <div className="hint">
            切版本前会先做快照；下面这些未提交文件会被清理，未列出的文件不会动。
          </div>
          <DirtyFileList files={files} />
        </div>
        <div className="modal-foot">
          <button className="primary sm" onClick={onClose}>
            知道了
          </button>
        </div>
      </div>
    </div>
  )
}

/** 版本管理：切稳定版 / 开发版、看当前版本、看切换的控制台输出 */
export default function Versions({ instanceId, instanceName, canSwitch, notify, onChanged }: Props) {
  const [info, setInfo] = useState<VersionInfo | null>(null)
  const [tags, setTags] = useState<TagInfo[]>([])
  /** null = 还没取过（第一次切到开发版时才联网拉） */
  const [commits, setCommits] = useState<DevCommit[] | null>(null)
  const [commitsLoading, setCommitsLoading] = useState(false)
  const [commitsError, setCommitsError] = useState<string | null>(null)
  const [behind, setBehind] = useState<number | null>(null)
  const [checked, setChecked] = useState(false)
  const [tab, setTab] = useState<Channel>('stable')
  const [pending, setPending] = useState<Pending | null>(null)
  const [pendingUpdate, setPendingUpdate] = useState(false)
  const [dirtyFiles, setDirtyFiles] = useState<VersionDirtyFile[]>([])
  const [dirtyOpen, setDirtyOpen] = useState(false)
  const [dirtyLoading, setDirtyLoading] = useState(false)
  const [busy, setBusy] = useState(false)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  // 切换 / 更新的控制台：弹窗里看原文
  const [logs, setLogs] = useState<string[]>([])
  const [consoleOpen, setConsoleOpen] = useState(false)
  const [consoleTitle, setConsoleTitle] = useState('')
  const [consoleState, setConsoleState] = useState<ConsoleState>('running')
  const [consoleTarget, setConsoleTarget] = useState('')
  const [opEvents, setOpEvents] = useState<OperationLine[]>([])
  const [opTab, setOpTab] = useState<OperationTab>('detail')
  const [cancelling, setCancelling] = useState(false)

  const load = useCallback(async () => {
    setError(null)
    try {
      const [v, t] = await Promise.all([api.versionInfo(instanceId), api.versionTags(instanceId)])
      setInfo(v)
      setTags(t)
      setBehind(null)
      setChecked(false)
      setDirtyFiles([])
      setDirtyOpen(false)
      setPendingUpdate(false)
      setPending(null)
      // 切完自动回到"当前通道"那份列表，别让人接着看另一边的历史
      setTab(v.channel === 'dev' ? 'dev' : 'stable')
    } catch (e) {
      setInfo(null)
      setTags([])
      setError(errText(e))
    } finally {
      setLoading(false)
    }
  }, [instanceId])

  useEffect(() => {
    void load()
  }, [load])

  const loadCommits = useCallback(
    async (force = false) => {
      if (!force && (commits !== null || commitsLoading)) return
      setCommitsLoading(true)
      setCommitsError(null)
      try {
        setCommits(await api.versionCommits(instanceId, 30))
      } catch (e) {
        setCommitsError(errText(e))
        setCommits([])
      } finally {
        setCommitsLoading(false)
      }
    },
    [commits, commitsLoading, instanceId],
  )

  useEffect(() => {
    if (tab === 'dev') void loadCommits()
  }, [tab, loadCommits])

  const pushEvent = (ev: VersionEvent) => {
    setLogs((prev) => [...prev, ...logLine(ev)].slice(-3000))
    const phase = ev.failed ? 'err' : ev.level === 'warn' ? 'warn' : ev.done ? 'done' : 'log'
    const lines = ev.message.split('\n').filter((line) => line.length > 0)
    setOpEvents((prev) =>
      [
        ...prev,
        ...lines.map((message) => ({
          at: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
          step: operationStageLabel(ev.stage),
          phase,
          message,
        })),
      ].slice(-1200),
    )
  }

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(logs.join('\n'))
      notify('ok', '日志已复制')
    } catch {
      notify('err', '复制失败，请在弹窗里手动选中')
    }
  }

  const check = async () => {
    setBusy(true)
    try {
      const n = await api.versionCheck(instanceId)
      setBehind(n)
      setChecked(true)
      notify('ok', n === 0 ? '已是最新' : n === null ? '远端有更新的提交' : `落后 ${n} 个提交`)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  const loadDirtyFiles = async (): Promise<VersionDirtyFile[] | null> => {
    setDirtyLoading(true)
    try {
      const files = await api.versionDirtyFiles(instanceId)
      setDirtyFiles(files)
      return files
    } catch (e) {
      notify('err', errText(e))
      return null
    } finally {
      setDirtyLoading(false)
    }
  }

  const showDirty = async () => {
    setDirtyOpen(true)
    const files = await loadDirtyFiles()
    if (!files) setDirtyOpen(false)
  }

  const requestSwitch = async (p: Omit<Pending, 'clean'>) => {
    if (!info?.dirty) {
      setDirtyOpen(false)
      setDirtyFiles([])
      setPending({ ...p, clean: false })
      return
    }
    const files = await loadDirtyFiles()
    if (files) {
      setDirtyOpen(false)
      setPending({ ...p, clean: true })
    }
  }

  const requestUpdate = async () => {
    if (!info?.dirty) {
      await update(false)
      return
    }
    const files = await loadDirtyFiles()
    if (files) {
      setDirtyOpen(false)
      setPendingUpdate(true)
    }
  }

  const update = async (clean = false) => {
    if (clean) {
      // 清单一度停留在页面上，期间工作区可能又被改了；确认前必须复查。
      const expected = dirtyFiles
      const latest = await loadDirtyFiles()
      if (!latest) return
      if (dirtyFilesKey(latest) !== dirtyFilesKey(expected)) {
        notify('err', '工作区改动已变化，请重新确认清理清单')
        return
      }
    }
    setPendingUpdate(false)
    setDirtyOpen(false)
    setBusy(true)
    setCancelling(false)
    setLogs([])
    setOpEvents([])
    setOpTab('detail')
    setConsoleTarget(`更新 ${DEV_BRANCH} 到最新`)
    setConsoleTitle(`更新 ${DEV_BRANCH} 到最新`)
    setConsoleState('running')
    setConsoleOpen(true)
    try {
      const v = await api.versionUpdate(instanceId, pushEvent, clean)
      setConsoleState('ok')
      notify('ok', `已更新到 ${v.current}`)
      onChanged?.()
    } catch (e) {
      const text = errText(e)
      const wasCancelled = text.includes('VERSION_CANCELLED') || text.includes('CMD_CANCELLED')
      setConsoleState(wasCancelled ? 'cancelled' : 'failed')
      notify(wasCancelled ? 'ok' : 'err', wasCancelled ? '已取消依赖安装' : text)
      // 可能已经拉到新提交、只是依赖没装完：同样刷新
      onChanged?.()
    } finally {
      await load()
      setCommits(null)
      setBusy(false)
    }
  }

  const doSwitch = async (p: Pending) => {
    if (p.clean) {
      const expected = dirtyFiles
      const latest = await loadDirtyFiles()
      if (!latest) return
      if (dirtyFilesKey(latest) !== dirtyFilesKey(expected)) {
        notify('err', '工作区改动已变化，请重新确认清理清单')
        return
      }
    }
    setPending(null)
    setDirtyFiles([])
    setDirtyOpen(false)
    setBusy(true)
    setCancelling(false)
    setLogs([])
    setOpEvents([])
    setOpTab('detail')
    setConsoleTarget(`切换到 ${p.label}`)
    setConsoleTitle(`切换到 ${p.label}`)
    setConsoleState('running')
    setConsoleOpen(true)
    try {
      const v = await api.versionSwitch(instanceId, p.target, p.kind, pushEvent, p.clean)
      setConsoleState('ok')
      notify('ok', `已切换到 ${v.current}`)
      onChanged?.()
    } catch (e) {
      const text = errText(e)
      const wasCancelled = text.includes('VERSION_CANCELLED') || text.includes('CMD_CANCELLED')
      setConsoleState(wasCancelled ? 'cancelled' : 'failed')
      notify(wasCancelled ? 'ok' : 'err', wasCancelled ? '已取消依赖安装' : text)
      // checkout 可能已经成功、只是依赖重装失败：刷新到磁盘上的真实状态，别让人以为没切
      onChanged?.()
    } finally {
      await load()
      setCommits(null)
      setBusy(false)
    }
  }

  const cancelVersion = async () => {
    setCancelling(true)
    try {
      await api.versionCancel(instanceId)
    } catch (e) {
      setCancelling(false)
      notify('err', errText(e))
    }
  }

  /**
   * 最新稳定版 = 版本 tag 列表按版本号倒序的第一项（由后端排好序）。
   * 远端还有一个会移动的 `latest` tag，它不是某个具体版本，不能当稳定版锚点。
   */
  const latestTag = tags.find((t) => VERSION_TAG.test(t.name))?.name ?? null
  const dev = info?.channel === 'dev'
  const detached = info?.branch === 'HEAD'
  const onDevBranch = info?.branch === DEV_BRANCH
  const wrongBranch = !!info && !detached && info.branch !== DEV_BRANCH
  const channelText = info
    ? dev
      ? `开发版 · ${detached ? info.commit : info.branch}`
      : `稳定版 · ${info.current}`
    : ''
  const blocked = busy || loading || dirtyLoading || !canSwitch || !info
  /** 只在真的被挡住时给原因，别在按钮可点时挂一个「处理中…」的提示 */
  const blockedWhy = busy || loading
    ? '处理中…'
    : dirtyLoading
      ? '正在读取清理清单…'
      : !info
        ? '读不到版本状态（不是 git 仓库？）'
        : !canSwitch
          ? '先停止实例'
          : undefined

  const tip = commits && commits.length > 0 ? commits[0] : null
  const pinned = (commits ?? []).filter((c) => !tip || c.sha !== tip.sha)

  return (
    <div className="stack">
      <div className="panel">
        <div className="panel-head">
          <h2>当前版本</h2>
          <span className="badge ghost">{instanceName}</span>
          {info && <span className={`badge ${dev ? 'ghost' : 'ok'}`}>{channelText}</span>}
          <div className="spacer" />
          {logs.length > 0 && (
            <button
              className="ghost sm"
              onClick={() => setConsoleOpen(true)}
              title="重看上一次切换 / 更新的输出"
            >
              <Icon name="console" size={13} />
              查看日志
            </button>
          )}
          <button
            className="ghost sm"
            onClick={() => void check()}
            disabled={busy || detached || loading}
            title={
              detached
                ? '没有跟随分支，没有"落后几个提交"的概念；先切到开发版 master'
                : !canSwitch
                  ? '先停止实例'
                  : '与远端比对提交数'
            }
          >
            <Icon name="refresh" size={13} />
            检查更新
          </button>
          <button
            className="primary sm"
            onClick={() => void requestUpdate()}
            disabled={busy || dirtyLoading || detached || loading || !canSwitch || wrongBranch}
            title={
              detached
                ? `没有跟随分支；先切到开发版 ${DEV_BRANCH}，或直接选一个提交`
                : !canSwitch
                  ? '先停止实例'
                  : wrongBranch
                    ? `版本更新固定跟随 ${DEV_BRANCH}；先切到开发版 ${DEV_BRANCH}`
                    : `拉取 ${DEV_BRANCH} 的最新提交；工作区有改动时会先确认清理清单`
            }
          >
            <Icon name="download" size={13} />
            更新到最新
          </button>
        </div>
        <div className="panel-body" style={{ paddingTop: 6 }}>
          {error && <div className="banner err">{error}</div>}
          {loading && <VersionInfoSkeleton />}
          {info && (
            <>
              <div className="records">
                <div className="record">
                  <span className="k">版本</span>
                  <span className="v mono" style={{ fontSize: 13.5 }}>
                    {info.current}
                  </span>
                </div>
                <div className="record">
                  <span className="k">提交</span>
                  <span className="v mono">{info.commit}</span>
                </div>
                <div className="record">
                  <span className="k">工作区</span>
                  <span className="v">
                    {info.dirty ? (
                      <span className="row" style={{ gap: 8 }}>
                        <span className="badge warn">有未提交改动，切换前会清理</span>
                        <button
                          type="button"
                          className="ghost sm"
                          onClick={() => void showDirty()}
                          disabled={dirtyLoading}
                        >
                          {dirtyLoading ? '读取中…' : '查看清理清单'}
                        </button>
                      </span>
                    ) : (
                      <span className="badge ok">干净</span>
                    )}
                  </span>
                </div>
                {info.remote && (
                  <div className="record">
                    <span className="k">远端</span>
                    <span className="v mono">{info.remote.replace(/\.git$/, '')}</span>
                  </div>
                )}
                {checked && (
                  <div className="record">
                    <span className="k">落后</span>
                    <span className="v">
                      {behind === 0
                        ? '已是最新'
                        : behind === null
                          ? '有更新（浅克隆数不清几个提交）'
                          : `${behind} 个提交`}
                    </span>
                  </div>
                )}
              </div>
            </>
          )}
        </div>
      </div>

      <div className="panel">
        <div className="panel-head">
          <h2>可用版本</h2>
          <span className="badge ghost">
            {tab === 'stable' ? (loading ? '…' : tags.length) : (commits?.length ?? '…')}
          </span>
          <Segmented
            style={{ marginLeft: 6 }}
            value={tab}
            options={[
              { value: 'stable', label: '稳定版' },
              { value: 'dev', label: '开发版' },
            ]}
            onChange={setTab}
          />
          <div className="spacer" />
          {busy && <span className="hint">处理中…</span>}
          <span className="hint nowrap">
            {tab === 'stable' ? '旧版本按需下载' : '开发版提交列表'}
          </span>
          <button
            className="ghost sm"
            onClick={() => void (tab === 'stable' ? load() : loadCommits(true))}
            disabled={tab === 'stable' ? loading : commitsLoading}
            title={tab === 'stable' ? '重新拉取稳定版 tag 列表' : '重新拉取 master 的提交列表'}
          >
            <Icon name="refresh" size={13} />
            刷新
          </button>
        </div>

        <div className="panel-body" style={{ padding: 0 }}>
          {pending && (
            <div className="banner warn" style={{ margin: 12 }}>
              <div>
                <strong>切到 {pending.label}？</strong>
                会 checkout 代码并按新版本的 <code>requirements.txt</code> 重装依赖，可能需要几分钟。
                {pending.clean && (
                  <div style={{ marginTop: 6 }}>
                    确认后会在切换前清理工作区里的 {dirtyFiles.length} 个未提交文件：
                  </div>
                )}
              </div>
              {pending.clean && <DirtyFileList files={dirtyFiles} />}
              <div className="row" style={{ marginTop: 8 }}>
                <button className="primary sm" disabled={busy} onClick={() => void doSwitch(pending)}>
                  {pending.clean ? '清理并切换' : '确认切换'}
                </button>
                <button
                  className="ghost sm"
                  onClick={() => {
                    setPending(null)
                    setDirtyFiles([])
                    setDirtyOpen(false)
                  }}
                >
                  取消
                </button>
              </div>
            </div>
          )}

          {pendingUpdate && (
            <div className="banner warn" style={{ margin: 12 }}>
              <div>
                <strong>更新 {DEV_BRANCH} 到最新？</strong>
                更新前会清理工作区里的 {dirtyFiles.length} 个未提交文件，然后拉取 {DEV_BRANCH}{' '}
                并重装依赖。
              </div>
              <DirtyFileList files={dirtyFiles} />
              <div className="row" style={{ marginTop: 8 }}>
                <button className="primary sm" disabled={busy} onClick={() => void update(true)}>
                  清理并更新
                </button>
                <button
                  className="ghost sm"
                  onClick={() => {
                    setPendingUpdate(false)
                    setDirtyFiles([])
                    setDirtyOpen(false)
                  }}
                >
                  取消
                </button>
              </div>
            </div>
          )}

          {tab === 'stable' ? (
            loading ? (
              <TagListSkeleton />
            ) : tags.length === 0 ? (
              <div className="empty">
                <span className="empty-icon">
                  <Icon name="alert" size={16} />
                </span>
                <strong>没读到版本列表</strong>
                <span className="hint">检查网络或镜像设置，然后重试；已安装的版本不受影响。</span>
                <button className="ghost sm" onClick={() => void load()} disabled={loading}>
                  <Icon name="refresh" size={13} />
                  重试
                </button>
              </div>
            ) : (
              <div className="tag-list">
                {tags.map((t) => (
                  <div className={`tag-row${t.current ? ' current' : ''}`} key={t.name}>
                    <span className="tag-name">{t.name}</span>
                    <span className="tag-date mono" title={t.date || undefined}>
                      {tagDateText(t.date)}
                    </span>
                    {t.name === latestTag && !t.current && <span className="badge ghost">最新稳定</span>}
                    <div className="tag-spacer" />
                    {t.current ? (
                      <span className="badge ok">当前</span>
                    ) : (
                      <button
                        className="ghost sm"
                        disabled={blocked}
                        title={blocked ? blockedWhy : `切到 ${t.name} 并重装依赖`}
                        onClick={() =>
                          void requestSwitch({ target: t.name, kind: 'tag', label: `稳定版 ${t.name}` })
                        }
                      >
                        切换
                      </button>
                    )}
                  </div>
                ))}
              </div>
            )
          ) : (
            <div className="tag-list">
              <div className={`tag-row${onDevBranch ? ' current' : ''}`}>
                <span className="tag-name">开发版 · {DEV_BRANCH}</span>
                {tip && (
                  <span className="tag-date mono" title={tip.date}>
                    {tip.short} · {tagDateText(tip.date)}
                  </span>
                )}
                <span className="tag-subject" title={tip?.subject}>
                  {tip?.subject ?? '跟随 master 的最新提交'}
                </span>
                <div className="tag-spacer" />
                {onDevBranch ? (
                  <span className="badge ok">当前</span>
                ) : (
                  <button
                    className="ghost sm"
                    disabled={blocked}
                    title={blocked ? blockedWhy : `跟随 ${DEV_BRANCH} 最新提交`}
                    onClick={() =>
                      void requestSwitch({
                        target: DEV_BRANCH,
                        kind: 'branch',
                        label: `开发版（跟随 ${DEV_BRANCH} 最新）`,
                      })
                    }
                  >
                    跟随最新
                  </button>
                )}
              </div>

              {commitsLoading && <TagListSkeleton />}
              {!commitsLoading && commitsError && <div className="banner err">{commitsError}</div>}
              {!commitsLoading && !commitsError && pinned.length === 0 && (
                <div className="empty">
                  <span className="empty-icon">
                    <Icon name="alert" size={16} />
                  </span>
                  <strong>没读到提交列表</strong>
                  <span className="hint">
                    离线或没有 origin 时读不到历史；仍可直接跟随 {DEV_BRANCH}。
                  </span>
                  <button className="ghost sm" onClick={() => void loadCommits(true)} disabled={commitsLoading}>
                    <Icon name="refresh" size={13} />
                    重试
                  </button>
                </div>
              )}
              {!commitsLoading &&
                pinned.map((c) => (
                  <div className={`tag-row${c.current ? ' current' : ''}`} key={c.sha}>
                    <span className="tag-name">{c.short}</span>
                    <span className="tag-date mono" title={c.date}>
                      {tagDateText(c.date)}
                    </span>
                    <span className="tag-subject" title={c.subject}>
                      {c.subject}
                    </span>
                    <div className="tag-spacer" />
                    {c.current ? (
                      <span className="badge ok">当前</span>
                    ) : (
                      <button
                        className="ghost sm"
                        disabled={blocked}
                        title={blocked ? blockedWhy : `切到提交 ${c.short} 并重装依赖`}
                        onClick={() =>
                          void requestSwitch({
                            target: c.sha,
                            kind: 'commit',
                            label: `开发版提交 ${c.short}（${c.subject}）`,
                          })
                        }
                      >
                        切换
                      </button>
                    )}
                  </div>
                ))}
            </div>
          )}
        </div>
      </div>

      {consoleOpen && (
        <OperationDialog
          title={consoleTitle}
          target={consoleTarget}
          state={consoleState}
          phase={opEvents[opEvents.length - 1]?.step ?? '准备中'}
          facts={[
            { label: '操作', value: consoleTarget },
            { label: '当前版本', value: info?.current ?? '—', mono: true },
          ]}
          events={opEvents}
          tab={opTab}
          onTab={setOpTab}
          detailLabel="版本详情"
          closeLabel={consoleState === 'running' ? '后台继续' : '关闭'}
          onClose={() => setConsoleOpen(false)}
          footer={
            <>
              <button className="ghost" onClick={() => void copyLogs()} disabled={logs.length === 0}>
                复制日志
              </button>
              {consoleState === 'running' && (
                <button
                  className="ghost stop"
                  onClick={() => void cancelVersion()}
                  disabled={cancelling}
                  title="终止依赖安装"
                >
                  <Icon name="stop" size={13} />
                  {cancelling ? '正在取消…' : '取消依赖安装'}
                </button>
              )}
            </>
          }
        >
          {consoleState === 'failed' && (
            <div className="op-run-error">依赖重装失败。代码可能已切到新版本。</div>
          )}
          {consoleState === 'cancelled' && (
            <div className="op-run-warn">依赖安装已取消。代码可能已切到新版本。</div>
          )}
        </OperationDialog>
      )}

      {dirtyOpen && <DirtyFilesDialog files={dirtyFiles} onClose={() => setDirtyOpen(false)} />}
    </div>
  )
}
