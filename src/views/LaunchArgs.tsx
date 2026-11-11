import { useEffect, useMemo, useRef, useState } from 'react'
import { api, errText } from '../lib/api'
import {
  ALL_FLAGS,
  FLAG_GROUPS,
  FLAG_BY_NAME,
  MANAGED_FLAGS,
  blocksOf,
  buildLine,
  lineFlag,
  lineValue,
  type FlagSpec,
} from '../lib/launchFlags'
import type { InstanceConfig } from '../types'
import { useTabSave, type TabSaveHandle } from '../lib/useTabSave'
import Icon from '../components/Icon'
import Select from '../components/Select'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
  onSaved: (updated: InstanceConfig) => void
  onSaveState: (state: TabSaveHandle | null) => void
}

const DOCS_URL = 'https://docs.comfy.org/zh/development/comfyui-server/startup-flags'

/** 一行里可能写多个参数，勾选框只认第一个；展示时原样保留用户写的那行 */
function textOf(lines: string[]): string {
  return lines.join('\n')
}

/** 保留引号中的空格，避免编辑一个参数时把同一行的其它参数拆坏。 */
function lineTokens(line: string): string[] {
  return line.match(/"[^"]*"|'[^']*'|\S+/g) ?? []
}

function quoteValue(value: string): string {
  return /[\s"]/.test(value) ? `"${value.replace(/"/g, '\\"')}"` : value
}

/** 只移除目标参数及其对应的值，保留同一行的其它参数。 */
function stripFlag(line: string, spec: FlagSpec): string {
  const tokens = lineTokens(line)
  const index = tokens.findIndex((token) => lineFlag(token) === spec.flag)
  if (index < 0) return line
  const hasInlineValue = tokens[index].includes('=')
  tokens.splice(index, 1)
  if (!hasInlineValue && spec.value && tokens[index] && !tokens[index].startsWith('--')) {
    tokens.splice(index, 1)
  }
  return tokens.join(' ')
}

function formKey(server: InstanceConfig['server']): string {
  return JSON.stringify({
    listen: server.listen,
    port: String(server.port),
    corsOrigin: server.corsOrigin ?? '',
    args: textOf(server.extraArgs),
  })
}

export default function LaunchArgs({ inst, notify, onSaved, onSaveState }: Props) {
  const [listen, setListen] = useState(inst.server.listen)
  const [port, setPort] = useState(String(inst.server.port))
  const [corsOrigin, setCorsOrigin] = useState(inst.server.corsOrigin ?? '')
  /** 参数行：唯一数据源。勾选框、自定义输入、命令预览都从这一份派生 */
  const [lines, setLines] = useState<string[]>(inst.server.extraArgs)
  const [query, setQuery] = useState('')
  const [busy, setBusy] = useState(false)

  /** 参数名 → 值文本（值为空就是空串） */
  const active = useMemo(() => {
    const map = new Map<string, string>()
    for (const line of lines) {
      const flag = lineFlag(line)
      if (flag) map.set(flag, lineValue(line))
    }
    return map
  }, [lines])

  const warnings = useMemo(() => {
    const out: string[] = []
    const exclusive = new Map<string, string[]>()
    for (const [flag, value] of active) {
      if (MANAGED_FLAGS.includes(flag)) {
        out.push(`${flag} 由页面上的字段生成（服务与网络 / 存储页），写在这里会出现两个同名参数`)
      }
      const spec = FLAG_BY_NAME.get(flag)
      if (spec?.value?.required && !value.trim()) {
        out.push(`${flag} 少了值，ComfyUI 会直接报错`)
      }
      if (spec?.exclusive) {
        const list = exclusive.get(spec.exclusive) ?? []
        list.push(flag)
        exclusive.set(spec.exclusive, list)
      }
    }
    for (const list of exclusive.values()) {
      if (list.length > 1) out.push(`${list.join(' 与 ')} 互斥，argparse 会拒绝启动，只能留一个`)
    }
    return out
  }, [active])

  const customText = textOf(lines)

  const preview = useMemo(() => {
    const parts = ['main.py']
    if (listen.trim()) parts.push('--listen', listen.trim())
    parts.push('--port', port.trim() || '8188')
    if (corsOrigin.trim()) parts.push('--enable-cors-header', corsOrigin.trim())
    for (const line of lines) {
      const text = line.trim()
      if (text) parts.push(text)
    }
    return parts.join(' ')
  }, [listen, port, corsOrigin, lines])

  const snapshot = useMemo(
    () =>
      JSON.stringify({
        listen: inst.server.listen,
        port: String(inst.server.port),
        corsOrigin: inst.server.corsOrigin ?? '',
        args: textOf(inst.server.extraArgs),
      }),
    [inst],
  )
  const dirty =
    JSON.stringify({
      listen,
      port,
      corsOrigin,
      args: customText,
    }) !== snapshot

  // 快照恢复/集群同步会改 inst.server；本地没有草稿时跟随新值，
  // 有草稿时保留用户输入，只把基线更新到最新磁盘配置。
  const serverKey = formKey(inst.server)
  const baselineRef = useRef(serverKey)
  const draftRef = useRef(
    JSON.stringify({ listen, port, corsOrigin, args: customText }),
  )
  draftRef.current = JSON.stringify({ listen, port, corsOrigin, args: customText })
  useEffect(() => {
    if (baselineRef.current === serverKey) return
    const wasClean = draftRef.current === baselineRef.current
    baselineRef.current = serverKey
    if (!wasClean) return
    setListen(inst.server.listen)
    setPort(String(inst.server.port))
    setCorsOrigin(inst.server.corsOrigin ?? '')
    setLines(inst.server.extraArgs)
  }, [inst.server, serverKey])

  const toggle = (spec: FlagSpec) => {
    const on = active.has(spec.flag)
    setLines((prev) => {
      const kept = prev
        .map((line) => {
          const flag = lineFlag(line)
          if (!flag) return line
          if (flag === spec.flag) return stripFlag(line, spec)
          // 勾新的之前，先把互斥组里的兄弟摘掉（否则 argparse 直接报错）
          if (!on && spec.exclusive) {
            const other = FLAG_BY_NAME.get(flag)
            if (other?.exclusive === spec.exclusive) return stripFlag(line, other)
          }
          return line
        })
        .map((line) => line.trim())
        .filter(Boolean)
      return on ? kept : [...kept, buildLine(spec)]
    })
  }

  const setValue = (spec: FlagSpec, value: string) => {
    setLines((prev) =>
      prev.map((line) => {
        if (lineFlag(line) !== spec.flag) return line
        const tokens = lineTokens(line)
        const index = tokens.findIndex((token) => lineFlag(token) === spec.flag)
        if (index < 0) return line
        const trimmed = value.trim()
        if (tokens[index].includes('=')) {
          tokens[index] = trimmed ? `${spec.flag}=${quoteValue(trimmed)}` : spec.flag
        } else {
          tokens[index] = spec.flag
          if (trimmed) {
            if (tokens[index + 1] && !tokens[index + 1].startsWith('--')) {
              tokens[index + 1] = quoteValue(trimmed)
            } else {
              tokens.splice(index + 1, 0, quoteValue(trimmed))
            }
          } else if (tokens[index + 1] && !tokens[index + 1].startsWith('--')) {
            tokens.splice(index + 1, 1)
          }
        }
        return tokens.join(' ')
      }),
    )
  }

  const portNumber = Number(port.trim())
  const invalidPort =
    !/^\d+$/.test(port.trim()) || !Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535

  const save = async () => {
    if (invalidPort) {
      notify('err', '端口必须是 1–65535 的整数')
      return
    }
    setBusy(true)
    try {
      const updated = await api.instanceLaunchUpdate({
        id: inst.id,
        listen: listen.trim(),
        port: portNumber,
        corsOrigin: corsOrigin.trim() || null,
        extraArgs: lines.map((l) => l.trim()).filter(Boolean),
      })
      // 以服务端归一化后的值为新基线，避免保存后本地还留着旧输入。
      setListen(updated.server.listen)
      setPort(String(updated.server.port))
      setCorsOrigin(updated.server.corsOrigin ?? '')
      setLines(updated.server.extraArgs)
      notify('ok', '启动参数已保存，下次启动生效')
      onSaved(updated)
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  useTabSave(onSaveState, dirty, busy, save, reset, invalidPort)

  const q = query.trim().toLowerCase()
  const groups = FLAG_GROUPS.map((g) => ({
    ...g,
    flags: q
      ? g.flags.filter((f) => f.flag.toLowerCase().includes(q) || f.help.includes(q))
      : g.flags,
  })).filter((g) => g.flags.length > 0)

  /** 一行参数：勾选框 + 参数名 + 说明（+ 需要值时给输入控件） */
  const renderFlag = (spec: FlagSpec) => {
    const on = active.has(spec.flag)
    const value = active.get(spec.flag) ?? ''
    return (
      <div className={`flag-row${on ? ' on' : ''}`} key={spec.flag}>
        <input
          type="checkbox"
          id={`flag-${spec.flag}`}
          aria-label={spec.flag}
          checked={on}
          onChange={() => toggle(spec)}
        />
        <label className="flag-body" htmlFor={`flag-${spec.flag}`} title={spec.help}>
          <span className={`flag-name mono${spec.risky ? ' risky' : ''}`}>{spec.flag}</span>
          <span className="flag-help">{spec.help}</span>
        </label>
        {on && spec.value && (
          <span className="flag-value">
            {spec.value.choices ? (
              <Select
                ariaLabel={`${spec.flag} 的值`}
                value={value || spec.value.def}
                onChange={(next) => setValue(spec, next)}
                options={spec.value.choices.map((choice) => ({
                  value: choice,
                  label: choice,
                }))}
              />
            ) : (
              <input
                aria-label={`${spec.flag} 的值`}
                type={spec.value.numeric ? 'number' : 'text'}
                className={spec.value.numeric ? '' : 'mono'}
                value={value}
                placeholder={spec.value.placeholder}
                onChange={(e) => setValue(spec, e.target.value)}
              />
            )}
          </span>
        )}
      </div>
    )
  }
  const enabledCount = ALL_FLAGS.filter((f) => active.has(f.flag)).length

  return (
    <div className="stack launch-args">
      <div className="panel">
        <header className="panel-head">
          <h2>启动参数</h2>
          <span className="badge ghost">{inst.name}</span>
          {dirty && <span className="badge warn">未保存</span>}
          <div className="spacer" />
          <button className="ghost sm" onClick={() => void api.openUrl(DOCS_URL)}>
            <Icon name="globe" size={13} />
            官方文档
          </button>
        </header>

        <div className="panel-body settings-body">
          {/* 启动命令在最上面：命令贴在标题行，自定义参数占满整行 */}
          <section>
            <div className="launch-head launch-command-head">
              <h3>启动命令</h3>
              <code className="launch-command" title={preview}>
                {preview}
              </code>
            </div>
            <div className="field launch-custom">
              <label>自定义参数</label>
              <textarea
                rows={6}
                className="mono"
                aria-label="自定义启动参数"
                value={customText}
                placeholder="--enable-manager"
                onChange={(e) => setLines(e.target.value.split('\n'))}
              />
              <p className="hint hint-show launch-args-hint">
                一行一个；一行写多个参数时按空格切开，路径带空格用双引号。删掉一行，对应勾选也会取消。
              </p>
            </div>

            {warnings.length > 0 && (
              <div className="banner warn launch-warn">
                {warnings.map((w) => (
                  <div key={w}>· {w}</div>
                ))}
              </div>
            )}
          </section>

          <section>
            <h3>
              服务与网络 <span className="badge ghost">下次启动生效</span>
            </h3>
            <div className="grid-2">
              <div className="field">
                <label>监听地址</label>
                <Select
                  ariaLabel="监听地址"
                  value={listen}
                  onChange={setListen}
                  options={[
                    { value: '127.0.0.1', label: '127.0.0.1（仅本机）' },
                    { value: '0.0.0.0', label: '0.0.0.0（开放局域网访问）' },
                  ]}
                />
              </div>
              <div className="field">
                <label>端口</label>
                <input
                  type="number"
                  aria-label="端口"
                  value={port}
                  min={1}
                  max={65535}
                  onChange={(e) => setPort(e.target.value)}
                />

              </div>
              <div className="field" style={{ gridColumn: '1 / -1' }}>
                <label>CORS 允许来源（可选）</label>
                <input
                  aria-label="CORS 允许来源"
                  value={corsOrigin}
                  placeholder="留空 = 不启用跨域"
                  onChange={(e) => setCorsOrigin(e.target.value)}
                />
              </div>
            </div>
          </section>

          <section>
            <div className="launch-head">
              <h3>
                可选参数 <span className="badge ghost">已启用 {enabledCount} 项</span>
              </h3>
              <div className="spacer" />
              <input
                className="launch-search"
                aria-label="搜索参数"
                placeholder="搜参数名或说明"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>

            {groups.map((group) => (
              <div className="flag-group" key={group.id}>
                <div className="flag-group-head">
                  <span className="flag-group-title" title={group.note}>{group.title}</span>
                  {group.note && <span className="hint">{group.note}</span>}
                </div>
                {blocksOf(group).map((block) => (
                  <div
                    className={`flag-block${block.exclusive ? ' cluster' : ''}`}
                    key={`${group.id}-${block.exclusive ?? 'plain'}`}
                  >
                    {block.label && (
                      <div className="flag-block-head">
                        <span>{block.label}</span>
                        {block.exclusive && <span className="hint">只能选一个</span>}
                      </div>
                    )}
                    {block.flags.map((spec) => renderFlag(spec))}
                  </div>
                ))}
              </div>
            ))}
            {groups.length === 0 && (
              <p className="hint">没有匹配「{query}」的参数，直接写到上面的自定义参数里就行。</p>
            )}
          </section>
        </div>
      </div>
    </div>
  )

  function reset() {
    setListen(inst.server.listen)
    setPort(String(inst.server.port))
    setCorsOrigin(inst.server.corsOrigin ?? '')
    setLines(inst.server.extraArgs)
  }
}
