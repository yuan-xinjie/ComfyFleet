import { TextInput, TextArea } from '../components/Form'
import IconTextButton from '../components/IconTextButton'
import Switch from '../components/Switch'
import { useEffect, useMemo, useRef, useState } from 'react'
import { api, errText } from '../lib/api'
import {
  ALL_FLAGS,
  BINARY_FLAG_SWITCHES,
  COMMON_FLAGS,
  EXCLUSIVE_LABELS,
  FLAG_GROUPS,
  FLAG_BY_NAME,
  MANAGED_FLAGS,
  buildLine,
  lineFlag,
  lineValue,
  type FlagBlock,
  type FlagGroup,
  type FlagSpec,
} from '../lib/launchFlags'
import type { InstanceConfig } from '../types'
import { useTabSave, type TabSaveHandle } from '../lib/useTabSave'
import Select from '../components/Select'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
  onSaved: (updated: InstanceConfig) => void
  onSaveState: (state: TabSaveHandle | null) => void
}

const DOCS_URL = 'https://docs.comfy.org/zh/development/comfyui-server/startup-flags'

/** 高级参数关闭时直接展示的精选参数，不再要求先选分类。 */
const CURATED_SWITCH_FLAGS = [
  '--disable-pinned-memory',
  '--enable-manager',
  '--models-directory',
  '--user-directory',
  '--max-upload-size',
  // 输出目录/额外模型路径由「存储」页生成，精选开关里不能再手写一份。
].filter((flag) => !MANAGED_FLAGS.includes(flag))

type CatalogEntry =
  | { kind: 'switch'; spec: FlagSpec }
  | { kind: 'choice'; block: FlagBlock }
  | {
      kind: 'boolean'
      exclusive: string
      label: string
      on: FlagSpec
      off: FlagSpec
    }

/** 分类内先放互斥选择，再放开关；两类各自保留原始顺序。 */
function buildEntries(group: FlagGroup, visible: Set<string>): CatalogEntry[] {
  const choices: CatalogEntry[] = []
  const switches: CatalogEntry[] = []
  const seenExclusive = new Set<string>()
  for (const spec of group.flags) {
    if (!spec.exclusive) {
      if (visible.has(spec.flag)) switches.push({ kind: 'switch', spec })
      continue
    }
    if (seenExclusive.has(spec.exclusive)) continue
    seenExclusive.add(spec.exclusive)
    const flags = group.flags.filter((flag) => flag.exclusive === spec.exclusive)
    const binary = BINARY_FLAG_SWITCHES[spec.exclusive]
    if (binary) {
      const on = FLAG_BY_NAME.get(binary.on)
      const off = FLAG_BY_NAME.get(binary.off)
      if (on && off && (visible.has(on.flag) || visible.has(off.flag))) {
        switches.push({
          kind: 'boolean',
          exclusive: spec.exclusive,
          label: EXCLUSIVE_LABELS[spec.exclusive] ?? '开关',
          on,
          off,
        })
      }
      continue
    }
    // 互斥组只要有一个选项命中，就把整组选择保留下来；否则用户会被困在
    // 只看得见部分选项的下拉里。全组仍只占一行，不额外增加视觉噪音。
    if (flags.some((flag) => visible.has(flag.flag))) {
      choices.push({
        kind: 'choice',
        block: {
          exclusive: spec.exclusive,
          label: EXCLUSIVE_LABELS[spec.exclusive] ?? '互斥组',
          flags,
        },
      })
    }
  }
  return [...choices, ...switches]
}

function displayFlag(flag: string): string {
  return flag.replace(/^--/, '')
}

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
  const [advanced, setAdvanced] = useState(false)
  const [activeCategory, setActiveCategory] = useState(FLAG_GROUPS[0]?.id ?? '')
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
        out.push(
          `${displayFlag(flag)} 由页面上的字段生成（服务与网络 / 存储页），写在这里会出现两个同名参数`,
        )
      }
      const spec = FLAG_BY_NAME.get(flag)
      if (spec?.value?.required && !value.trim()) {
        out.push(`${displayFlag(flag)} 少了值，ComfyUI 会直接报错`)
      }
      if (spec?.exclusive) {
        const list = exclusive.get(spec.exclusive) ?? []
        list.push(flag)
        exclusive.set(spec.exclusive, list)
      }
    }
    for (const list of exclusive.values()) {
      if (list.length > 1) {
        out.push(
          `${list.map(displayFlag).join(' 与 ')} 互斥，argparse 会拒绝启动，只能留一个`,
        )
      }
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
    const disablingManager = spec.flag === '--enable-manager' && on
    setLines((prev) => {
      const kept = prev
        .map((line) => {
          const flag = lineFlag(line)
          if (!flag) return line
          if (flag === spec.flag) return stripFlag(line, spec)
          // Manager 界面选项依赖 --enable-manager；关掉总开关时一并清掉，
          // 避免留下 argparse 接受、但界面上无法再操作的组合。
          if (disablingManager) {
            const other = FLAG_BY_NAME.get(flag)
            if (other?.exclusive === 'manager_ui') return stripFlag(line, other)
          }
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

  /** 互斥组用下拉选择：选中一个就摘掉同组其它参数，选“默认”清空整组。 */
  const setExclusive = (exclusive: string, flag: string) => {
    setLines((prev) => {
      const kept = prev
        .map((line) => {
          const current = lineFlag(line)
          if (!current) return line
          const spec = FLAG_BY_NAME.get(current)
          return spec?.exclusive === exclusive ? stripFlag(line, spec) : line
        })
        .map((line) => line.trim())
        .filter(Boolean)
      if (!flag) return kept
      const spec = FLAG_BY_NAME.get(flag)
      return spec ? [...kept, buildLine(spec)] : kept
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
  const categories = FLAG_GROUPS.map((group) => {
    const titleHit = Boolean(q) && group.title.toLowerCase().includes(q)
    const visible = new Set(
      group.flags
        .filter((spec) => {
          if (q) {
            return (
              titleHit ||
              displayFlag(spec.flag).toLowerCase().includes(q) ||
              spec.help.toLowerCase().includes(q)
            )
          }
          return advanced || COMMON_FLAGS.has(spec.flag) || active.has(spec.flag)
        })
        .map((spec) => spec.flag),
    )
    return {
      group,
      entries: buildEntries(group, visible),
      count: group.flags.filter((spec) => active.has(spec.flag)).length,
    }
  }).filter((category) => category.entries.length > 0)

  const renderValue = (spec: FlagSpec) => {
    if (!spec.value) return null
    const value = active.get(spec.flag) ?? ''
    return (
      <span className="flag-value">
        {spec.value.choices ? (
          <Select
            ariaLabel={`${displayFlag(spec.flag)} 的值`}
            value={value || spec.value.def}
            onChange={(next) => setValue(spec, next)}
            options={spec.value.choices.map((choice) => ({
              value: choice,
              label: choice,
            }))}
          />
        ) : (
          <TextInput
            aria-label={`${displayFlag(spec.flag)} 的值`}
            type={spec.value.numeric ? 'number' : 'text'}
            className={spec.value.numeric ? '' : 'mono'}
            value={value}
            placeholder={spec.value.placeholder}
            onChange={(e) => setValue(spec, e.target.value)}
          />
        )}
      </span>
    )
  }

  /**
   * 每类参数统一成一行：左边是名字，右边是控件组。
   * 开关和互斥选择共用同一套行骨架，不再用双列网格拼贴。
   */
  const renderSwitchFlag = (spec: FlagSpec) => {
    const on = active.has(spec.flag)
    return (
      <div className={`flag-row${on ? ' is-on' : ''}`} key={spec.flag}>
        <div className="flag-copy">
          <label
            className={`flag-name mono${spec.risky ? ' risky' : ''}`}
            htmlFor={`flag-${spec.flag}`}
            title={spec.help}
          >
            {displayFlag(spec.flag)}
          </label>
          <p className="flag-help">{spec.help}</p>
        </div>
        <div className="flag-row-control">
          {spec.value && (
            <span
              className={`flag-value-slot${
                spec.value.choices ? ' is-select' : spec.value.numeric ? ' is-number' : ' is-text'
              }`}
            >
              {on && renderValue(spec)}
            </span>
          )}
          <Switch
            className="flag-toggle"
            id={`flag-${spec.flag}`}
            ariaLabel={displayFlag(spec.flag)}
            title={spec.help}
            checked={on}
            onChange={() => toggle(spec)}
          />
        </div>
      </div>
    )
  }

  /** 开 / 关互斥组：开写 on，关写 off，不再暴露「默认」第三态。 */
  const renderBooleanSwitch = (entry: Extract<CatalogEntry, { kind: 'boolean' }>) => {
    const on = active.has(entry.on.flag)
    const help = on ? entry.on.help : entry.off.help
    return (
      <div className={`flag-row${on ? ' is-on' : ''}`} key={`boolean-${entry.exclusive}`}>
        <div className="flag-copy">
          <label className="flag-name" htmlFor={`flag-${entry.exclusive}`} title={help}>
            {entry.label}
          </label>
          <p className="flag-help">{help}</p>
        </div>
        <div className="flag-row-control">
          <Switch
            className="flag-toggle"
            id={`flag-${entry.exclusive}`}
            ariaLabel={entry.label}
            title={help}
            checked={on}
            onChange={() => setExclusive(entry.exclusive, on ? entry.off.flag : entry.on.flag)}
          />
        </div>
      </div>
    )
  }

  /** Manager 界面两个选项的 flag 名太长，收起成短名，完整参数名放在弹层第二行。 */
  const choiceOptions = (block: FlagBlock) => [
    { value: '', label: '默认' },
    ...block.flags.map((flag) => {
      if (block.exclusive === 'manager_ui') {
        const label = flag.flag === '--disable-manager-ui' ? '关闭界面' : '旧界面'
        return {
          value: flag.flag,
          label,
          trigger: label,
          meta: displayFlag(flag.flag),
          title: flag.help,
        }
      }
      return {
        value: flag.flag,
        label: displayFlag(flag.flag),
        trigger: displayFlag(flag.flag),
        title: flag.help,
      }
    }),
  ]

  /** 互斥组：标签在上、选择器在下；带值的选项把值控件并排放在选择器右侧。 */
  const renderChoice = (block: FlagBlock, disabled = false) => {
    if (!block.exclusive) return null
    const selected = block.flags.find((flag) => active.has(flag.flag))
    return (
      <div
        className={`launch-choice${selected ? ' is-on' : ''}`}
        key={`exclusive-${block.exclusive}`}
      >
        <span className="launch-choice-label" title={selected?.help ?? block.label}>
          {block.label}
        </span>
        <div className="launch-choice-controls">
          <Select
            className="launch-choice-select"
            ariaLabel={block.label}
            value={selected?.flag ?? ''}
            menuMinWidth={260}
            disabled={disabled}
            triggerTitle={disabled ? '先启用 ComfyUI Manager' : (selected?.help ?? block.label)}
            onChange={(next) => setExclusive(block.exclusive!, next)}
            options={choiceOptions(block)}
          />
          {selected && renderValue(selected)}
        </div>
      </div>
    )
  }

  /** 精选参数里的互斥组：沿用开关行的左文右控件，避免再出现一排分组框。 */
  const renderInlineChoice = (block: FlagBlock, disabled = false) => {
    if (!block.exclusive) return null
    const selected = block.flags.find((flag) => active.has(flag.flag))
    return (
      <div
        className={`flag-row launch-inline-choice${selected ? ' is-on' : ''}`}
        key={`inline-${block.exclusive}`}
      >
        <div className="flag-copy">
          <span className="flag-name" title={selected?.help ?? block.label}>
            {block.label}
          </span>
        </div>
        <div className="flag-row-control">
          <Select
            className="launch-inline-select"
            ariaLabel={block.label}
            value={selected?.flag ?? ''}
            menuMinWidth={260}
            disabled={disabled}
            triggerTitle={disabled ? '先启用 ComfyUI Manager' : (selected?.help ?? block.label)}
            onChange={(next) => setExclusive(block.exclusive!, next)}
            options={choiceOptions(block)}
          />
        </div>
      </div>
    )
  }

  /** 高级参数关闭时的精选页：不渲染分类，只保留日常会动的参数。 */
  const renderCurated = () => {
    const blockOf = (exclusive: string): FlagBlock | null => {
      const flags = ALL_FLAGS.filter((flag) => flag.exclusive === exclusive)
      if (flags.length === 0) return null
      return {
        exclusive,
        label: EXCLUSIVE_LABELS[exclusive] ?? '互斥组',
        flags,
      }
    }
    const vram = blockOf('vram')
    const attention = blockOf('attn')
    const managerUi = blockOf('manager_ui')
    const managerOn = active.has('--enable-manager')
    const autoPair = BINARY_FLAG_SWITCHES.autolaunch
    const autoOn = autoPair ? FLAG_BY_NAME.get(autoPair.on) : undefined
    const autoOff = autoPair ? FLAG_BY_NAME.get(autoPair.off) : undefined
    const autoEntry: Extract<CatalogEntry, { kind: 'boolean' }> | null =
      autoOn && autoOff
        ? {
            kind: 'boolean',
            exclusive: 'autolaunch',
            label: EXCLUSIVE_LABELS.autolaunch,
            on: autoOn,
            off: autoOff,
          }
        : null

    const represented = new Set<string>()
    for (const block of [vram, attention, managerUi]) {
      block?.flags.forEach((flag) => represented.add(flag.flag))
    }
    if (autoOn) represented.add(autoOn.flag)
    if (autoOff) represented.add(autoOff.flag)
    CURATED_SWITCH_FLAGS.forEach((flag) => represented.add(flag))

    // 已启用的进阶参数不能因为收起分类而消失，否则用户找不到怎么关掉。
    const extras = [...active.keys()]
      .filter((flag) => !represented.has(flag))
      .map((flag) => FLAG_BY_NAME.get(flag))
      .filter((spec): spec is FlagSpec => Boolean(spec))

    const switchRow = (flag: string) => {
      const spec = FLAG_BY_NAME.get(flag)
      return spec ? renderSwitchFlag(spec) : null
    }

    return (
      <div className="launch-param-groups launch-curated">
        {(vram || attention) && (
          <div className="launch-choice-grid">
            {vram && renderChoice(vram)}
            {attention && renderChoice(attention)}
          </div>
        )}
        <div className="flag-list">
          {switchRow('--disable-pinned-memory')}
          {switchRow('--enable-manager')}
          {managerUi && renderInlineChoice(managerUi, !managerOn)}
          {autoEntry && renderBooleanSwitch(autoEntry)}
          {CURATED_SWITCH_FLAGS.filter(
            (flag) => flag !== '--disable-pinned-memory' && flag !== '--enable-manager',
          ).map((flag) => switchRow(flag))}
          {extras.map((spec) => renderSwitchFlag(spec))}
        </div>
      </div>
    )
  }

  const renderParams = (category: (typeof categories)[number]) => {
    const choices = category.entries.filter(
      (entry): entry is Extract<CatalogEntry, { kind: 'choice' }> => entry.kind === 'choice',
    )
    const switches = category.entries.filter(
      (entry): entry is Extract<CatalogEntry, { kind: 'switch' | 'boolean' }> =>
        entry.kind === 'switch' || entry.kind === 'boolean',
    )
    return (
      <div className="launch-param-groups">
        {choices.length > 0 && (
          <div className="launch-choice-grid">
            {choices.map((entry) =>
              entry.kind === 'choice' ? (
                renderChoice(
                  entry.block,
                  entry.block.exclusive === 'manager_ui' && !active.has('--enable-manager'),
                )
              ) : null,
            )}
          </div>
        )}
        {switches.length > 0 && (
          <div className="flag-list">
            {switches.map((entry) => {
              if (entry.kind === 'switch') return renderSwitchFlag(entry.spec)
              if (entry.kind === 'boolean') return renderBooleanSwitch(entry)
              return null
            })}
          </div>
        )}
      </div>
    )
  }

  const renderSearchGroup = (category: (typeof categories)[number]) => (
    <div className="launch-search-group" key={category.group.id}>
      <div className="launch-search-group-head">
        <span>{category.group.title}</span>
        {category.count > 0 && <span className="badge ghost">{category.count}</span>}
      </div>
      {renderParams(category)}
    </div>
  )

  const selectedCategory =
    categories.find((category) => category.group.id === activeCategory) ?? categories[0]

  const enabledCount = ALL_FLAGS.filter((f) => active.has(f.flag)).length

  return (
    <div className="stack launch-args">
      <div className="panel">
        <header className="panel-head">
          <h2>启动参数</h2>
          <span className="badge ghost">{inst.name}</span>
          {dirty && <span className="badge warn">未保存</span>}
          <div className="spacer" />
          <IconTextButton className="ghost sm" onClick={() => void api.openUrl(DOCS_URL)} icon="globe" iconSize={13}>
            官方文档
          </IconTextButton>
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
              <TextArea
                rows={4}
                className="mono"
                aria-label="自定义启动参数"
                value={customText}
                placeholder="一行一个；路径带空格用双引号"
                onChange={(e) => setLines(e.target.value.split('\n'))}
              />
            </div>

          </section>

          <section>
            <h3>
              服务与网络 <span className="badge ghost">下次启动生效</span>
            </h3>
            <div className="launch-network">
              <div className="field launch-listen">
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
              <div className="field launch-port">
                <label>端口</label>
                <TextInput
                  type="number"
                  aria-label="端口"
                  value={port}
                  min={1}
                  max={65535}
                  onChange={(e) => setPort(e.target.value)}
                />
              </div>
              <div className="field launch-cors">
                <label>CORS 允许来源</label>
                <TextInput
                  aria-label="CORS 允许来源"
                  value={corsOrigin}
                  placeholder="https://example.com"
                  onChange={(e) => setCorsOrigin(e.target.value)}
                />
              </div>
            </div>
          </section>

          <section>
            <div className="launch-head launch-optional-head">
              <h3>
                可选参数 <span className="badge ghost">已启用 {enabledCount} 项</span>
                {warnings.length > 0 && (
                  <span
                    className="badge warn launch-warn-badge"
                    title={warnings.join('\n')}
                    aria-label={warnings.join('；')}
                  >
                    警告 {warnings.length}
                  </span>
                )}
              </h3>
              <div className="spacer" />
              <Switch
                className="launch-advanced"
                ariaLabel="高级参数"
                checked={advanced}
                onChange={setAdvanced}
              >
                <span>高级参数</span>
              </Switch>
              <TextInput
                className="launch-search"
                aria-label="搜索参数"
                placeholder="搜索参数"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>

            {q ? (
              categories.length > 0 ? (
                <div className="launch-search-results">{categories.map(renderSearchGroup)}</div>
              ) : (
                <div className="empty">
                  <strong>没有匹配的参数</strong>
                </div>
              )
            ) : !advanced ? (
              renderCurated()
            ) : categories.length > 0 ? (
              <>
                <div className="launch-filters" role="group" aria-label="参数分类">
                  {categories.map((category) => {
                    const on = selectedCategory?.group.id === category.group.id
                    return (
                      <button
                        key={category.group.id}
                        type="button"
                        className={`launch-filter${on ? ' active' : ''}`}
                        aria-pressed={on}
                        onClick={() => setActiveCategory(category.group.id)}
                      >
                        <span>{category.group.title}</span>
                        {category.count > 0 && (
                          <span className="launch-filter-count">{category.count}</span>
                        )}
                      </button>
                    )
                  })}
                </div>
                {selectedCategory && renderParams(selectedCategory)}
              </>
            ) : (
              <div className="empty">
                <strong>没有匹配的参数</strong>
              </div>
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
