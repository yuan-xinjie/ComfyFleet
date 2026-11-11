import { useCallback, useEffect, useState } from 'react'
import { api, errText } from '../lib/api'
import type { DepsReport, DiagReport, InstanceConfig } from '../types'
import Icon from './Icon'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
  onClose: () => void
  /** 点「补依赖」时把目标交给概览的节点卡片（null = 全部） */
  onInstall: (names: string[] | null) => void
}

const LEVEL_CLASS: Record<string, string> = { error: 'err', warn: 'warn', info: 'idle' }
const LEVEL_TEXT: Record<string, string> = { error: '错误', warn: '警告', info: '提示' }

/**
 * 诊断窗口：节点依赖是主角（缺什么、点一下就补），环境问题（python/torch/CUDA/pip check）是背景。
 * 入口在「节点」卡片的诊断按钮；概览页的「诊断」也走这里。
 */
export default function DiagModal({ inst, notify, onClose, onInstall }: Props) {
  const [report, setReport] = useState<DiagReport | null>(null)
  const [deps, setDeps] = useState<DepsReport | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const scan = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [diag, dep] = await Promise.all([
        api.diagScan(inst.id),
        api.nodesDepsStatus(inst.id).catch(() => null),
      ])
      setReport(diag)
      setDeps(dep)
    } catch (e) {
      setReport(null)
      setDeps(null)
      setError(errText(e))
    } finally {
      setLoading(false)
    }
  }, [inst.id])

  useEffect(() => {
    void scan()
  }, [scan])

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const errors = report?.issues.filter((i) => i.level === 'error').length ?? 0
  const warns = report?.issues.filter((i) => i.level === 'warn').length ?? 0
  const pendingNodes = deps?.pendingNodes ?? 0
  const depsNodes = (deps?.nodes ?? []).filter((n) => n.pending.length > 0)
  const okNodes = (deps?.nodes ?? []).filter((n) => n.pending.length === 0 && n.hasRequirements).length

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      notify('ok', '命令已复制')
    } catch (e) {
      notify('err', errText(e))
    }
  }

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className="modal diag-modal" role="dialog" aria-modal="true" aria-labelledby="diag-title">
        <div className="modal-head">
          <Icon name="diagnostics" size={15} />
          <h2 id="diag-title">诊断</h2>
          <span className="badge ghost">{inst.name}</span>
          {report && (
            <span className={`badge ${errors > 0 ? 'err' : warns > 0 || pendingNodes > 0 ? 'warn' : 'ok'}`}>
              <span className="dot" />
              {pendingNodes > 0
                ? `${pendingNodes} 个节点缺依赖`
                : errors > 0
                  ? `${errors} 错误 · ${warns} 警告`
                  : warns > 0
                    ? `${warns} 警告`
                    : '没发现问题'}
            </span>
          )}
          <div className="spacer" />
          <button className="ghost sm" onClick={() => void scan()} disabled={loading}>
            <Icon name="refresh" size={13} />
            {loading ? '检测中…' : '重新检测'}
          </button>
          <button className="icon-btn" onClick={onClose} title="关闭" aria-label="关闭">
            <Icon name="close" size={16} />
          </button>
        </div>

        <div className="modal-body">
          {error && (
            <div className="banner err">
              <span className="mark">
                <Icon name="alert" size={13} />
              </span>
              <div>{error}</div>
            </div>
          )}
          {loading && !report && <div className="empty">正在读取环境…（第一次要几秒）</div>}

          {report && (
            <>
              <div className="facts">
                <div className="fact">
                  <span className="metric-label">Python</span>
                  <span className="fact-value mono">{report.pythonVersion || '未知'}</span>
                </div>
                <div className="fact">
                  <span className="metric-label">torch</span>
                  <span className="fact-value mono">{report.torchVersion || '未知'}</span>
                </div>
                <div className="fact">
                  <span className="metric-label">CUDA</span>
                  <span
                    className="fact-value"
                    style={{ color: report.cudaAvailable ? 'var(--green)' : 'var(--red)' }}
                  >
                    {report.cudaAvailable ? '可用' : '不可用'}
                  </span>
                </div>
                <div className="fact">
                  <span className="metric-label">包</span>
                  <span className="fact-value">
                    {report.packageCount}
                    <small>个</small>
                  </span>
                </div>
              </div>

              {/* ---- 节点依赖：这一屏的主角 ---- */}
              <div className="diag-sec no-divider">
                <div className="diag-sec-head">
                  <h3>节点依赖</h3>
                  {deps?.pythonVersion && (
                    <span className="hint faint">Python {deps.pythonVersion}</span>
                  )}
                  <div className="spacer" />
                  {pendingNodes > 0 && (
                    <button className="primary sm" onClick={() => onInstall(null)}>
                      <Icon name="download" size={13} />
                      全部补依赖
                    </button>
                  )}
                </div>

                {deps?.noVenv && (
                  <div className="banner warn">
                    <span className="mark">
                      <Icon name="alert" size={13} />
                    </span>
                    <div>还没装好虚拟环境，无法判断缺什么；先在「设置」里装环境。</div>
                  </div>
                )}

                {!deps && !error && <div className="hint">依赖状态读取失败（不影响上面的环境检查）。</div>}

                {pendingNodes === 0 && deps && !deps.noVenv && (
                  <div className="banner ok">
                    <span className="mark">
                      <Icon name="check" size={13} />
                    </span>
                    <div>
                      {okNodes > 0 ? `${okNodes} 个声明了依赖的节点都齐了。` : '没有节点声明依赖。'}
                    </div>
                  </div>
                )}

                {depsNodes.map((d) => (
                  <div className="deps-node" key={d.name}>
                    <div className="deps-node-head">
                      <span className="mono">{d.name}</span>
                      <span className={`badge ${d.enabled ? 'ok' : 'idle'}`}>{d.enabled ? '启用' : '禁用'}</span>
                      <span className="badge warn">{d.pending.length} 个要装</span>
                      <div className="spacer" />
                      <button className="ghost sm" onClick={() => onInstall([d.name])}>
                        补依赖
                      </button>
                    </div>
                    <ul className="deps-list">
                      {d.pending.map((line) => (
                        <li key={line.line}>
                          <code>{line.line}</code>
                          <span className="deps-reason">{line.reason}</span>
                        </li>
                      ))}
                    </ul>
                  </div>
                ))}

                {deps && deps.conflicts.length > 0 && (
                  <div className="banner warn">
                    <span className="mark">
                      <Icon name="alert" size={13} />
                    </span>
                    <div>
                      pip check 有 {deps.conflicts.length} 条冲突：可能是某个节点要了不兼容的版本，
                      补依赖时会被带着解决；解决不了就点下面的原始输出看是谁。
                    </div>
                  </div>
                )}
              </div>

              {/* ---- 环境问题 ---- */}
              <div className="diag-sec">
                <div className="diag-sec-head">
                  <h3>环境问题</h3>
                  <span className="hint faint">{report.issues.length} 条</span>
                </div>

                {report.issues.length === 0 ? (
                  <div className="banner ok">
                    <span className="mark">
                      <Icon name="check" size={13} />
                    </span>
                    <div>依赖自洽（pip check 无冲突），引擎与实测一致。</div>
                  </div>
                ) : (
                  <div className="issues">
                    {report.issues.map((issue, idx) => (
                      <div className="issue" key={`${issue.title}-${idx}`}>
                        <div className="issue-head">
                          <span className={`badge ${LEVEL_CLASS[issue.level] ?? ''}`}>
                            {LEVEL_TEXT[issue.level] ?? issue.level}
                          </span>
                          <h3>{issue.title}</h3>
                        </div>
                        <pre className="install-log" style={{ maxHeight: 140 }}>
                          {issue.detail}
                        </pre>
                        <div className="hint">
                          <strong>怎么办：</strong>
                          {issue.suggestion}
                          <button className="link-inline" onClick={() => void copy(issue.suggestion)}>
                            复制
                          </button>
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <details>
                <summary>原始 pip check 输出（{report.pipCheck.length} 行）</summary>
                <pre className="install-log" style={{ marginTop: 8 }}>
                  {report.pipCheck.length ? report.pipCheck.join('\n') : '（空）'}
                </pre>
              </details>

            </>
          )}
        </div>

        <div className="modal-foot">
          <div className="spacer" />
          <button className="ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  )
}
