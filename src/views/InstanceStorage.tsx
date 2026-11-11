import { useCallback, useEffect, useRef, useState } from 'react'
import { api, errText } from '../lib/api'
import { DIR_MODE_LABEL, SOURCE_LABEL, modelTypeLabel } from '../lib/storage'
import type { DirChoice, InstanceConfig, InstanceStorage, StoragePlan } from '../types'
import { useTabSave, type TabSaveHandle } from '../lib/useTabSave'
import Icon from '../components/Icon'
import ModelDirList from '../components/ModelDirList'
import Segmented from '../components/Segmented'

interface Props {
  inst: InstanceConfig
  notify: (kind: 'ok' | 'err', msg: string) => void
  onSaved: () => void
  onSaveState: (state: TabSaveHandle | null) => void
}

function modeOf(choice: DirChoice): DirChoice['mode'] {
  return choice.mode === 'shared' || choice.mode === 'custom' ? choice.mode : 'own'
}

/** 共享目录按路径比对：忽略大小写与结尾斜杠 */
function samePath(a: string, b: string): boolean {
  const norm = (value: string) => value.trim().replace(/[\\/]+$/, '').toLowerCase()
  return norm(a) === norm(b)
}

/** 输入/输出目录只有一个：三选一 + 自定义路径 */
function IoChoice(props: {
  label: string
  value: DirChoice
  /** 全局设置里填的共享目录（可能是 null：还没填） */
  globalPath: string | null
  onChange: (next: DirChoice) => void
  notify: (kind: 'ok' | 'err', msg: string) => void
}) {
  const { label, value, globalPath, onChange, notify } = props
  const mode = modeOf(value)

  const target = mode === 'custom' ? value.path : mode === 'shared' ? globalPath : null

  const open = async () => {
    if (!target) return
    try {
      await api.openPath(target)
    } catch (e) {
      notify('err', errText(e))
    }
  }

  return (
    <div className="field">
      <label>{label}</label>
      <div className="row tight">
        <Segmented
          value={mode}
          options={(['own', 'shared', 'custom'] as const).map((item) => ({
            value: item,
            label: DIR_MODE_LABEL[item],
          }))}
          onChange={(next) =>
            onChange({ mode: next, path: next === 'custom' ? value.path : null })
          }
        />
        <button
          type="button"
          className="ghost sm icon-btn"
          aria-label={`打开${label}`}
          title={target ? `打开 ${target}` : '还没有可打开的目录'}
          onClick={() => void open()}
          disabled={!target}
        >
          <Icon name="folder" size={13} />
        </button>
      </div>
      {mode === 'custom' && (
        <input
          className="mono"
          aria-label={`自定义${label}`}
          placeholder="D:\\comfyui\\my-input"
          value={value.path ?? ''}
          spellCheck={false}
          onChange={(e) => onChange({ mode: 'custom', path: e.target.value })}
        />
      )}
    </div>
  )
}

/** 一行「生效目录」：来源 + 路径 + 状态 + 打开 */
function EffectiveRow(props: {
  label: string
  path: string
  source: string
  exists: boolean
  isDefault?: boolean
  notify: (kind: 'ok' | 'err', msg: string) => void
}) {
  const { label, path, source, exists, isDefault, notify } = props
  return (
    <div className="eff-row">
      <span className={`badge ${source === 'shared' ? 'ok' : 'ghost'}`}>{label}</span>
      <span className="mono eff-path" title={path}>
        {path}
      </span>
      {isDefault && <span className="badge warn">下载落点</span>}
      {!exists && <span className="badge danger">不存在</span>}
      <button
        className="ghost sm icon-btn"
        aria-label={`打开${label}`}
        title={exists ? '打开目录' : '目录还不存在'}
        disabled={!exists}
        onClick={() => void api.openPath(path).catch((e) => notify('err', errText(e)))}
      >
        <Icon name="folder" size={13} />
      </button>
    </div>
  )
}

/**
 * 实例「存储」页签：决定这个实例的模型 / 输入 / 输出目录打哪来。
 *
 * 与官方桌面版 Manage → Storage 同一套语义：
 * 用不用共享模型目录、额外挂哪些模型目录、输入输出用谁。
 */
export default function InstanceStorageView({ inst, notify, onSaved, onSaveState }: Props) {
  const [form, setForm] = useState<InstanceStorage>(inst.storage)
  const [baseline, setBaseline] = useState<InstanceStorage>(inst.storage)
  const [plan, setPlan] = useState<StoragePlan | null>(null)
  const [busy, setBusy] = useState(false)
  const planSeq = useRef(0)

  const loadPlan = useCallback(
    async (draft?: InstanceStorage) => {
      const seq = ++planSeq.current
      try {
        const next = await api.instanceStoragePlan(inst.id, draft)
        if (seq === planSeq.current) setPlan(next)
      } catch (e) {
        if (seq === planSeq.current) notify('err', errText(e))
      }
    },
    [inst.id, notify],
  )

  // 草稿一变就按草稿重算：勾选框/自定义路径还没保存时，「实际生效」也跟着变
  useEffect(() => {
    const timer = setTimeout(() => {
      const seq = ++planSeq.current
      void api
        .instanceStoragePlan(inst.id, form)
        .then((next) => {
          // 打字很快时旧请求可能后返回，不能让它覆盖新草案的预览。
          if (seq === planSeq.current) setPlan(next)
        })
        .catch(() => {
          /* 草稿里的半截路径不算错误，忽略这次预览 */
        })
    }, 220)
    return () => clearTimeout(timer)
  }, [form, inst.id])

  // 集群同步/快照恢复会改 inst.storage。没有本地草稿时跟随；
  // 有草稿时保留草稿，只更新基线，避免把新配置又写回旧值。
  const storageKey = JSON.stringify(inst.storage)
  const baselineRef = useRef(storageKey)
  const formRef = useRef(JSON.stringify(form))
  formRef.current = JSON.stringify(form)
  useEffect(() => {
    if (baselineRef.current === storageKey) return
    const wasClean = formRef.current === baselineRef.current
    baselineRef.current = storageKey
    if (!wasClean) return
    setForm(inst.storage)
    setBaseline(inst.storage)
    void loadPlan(inst.storage)
  }, [inst.storage, loadPlan, storageKey])

  const dirty = JSON.stringify(form) !== JSON.stringify(baseline)

  /** 这个实例真正用上的共享目录（按草稿算） */
  const sharedOn = (plan?.sharedModels ?? []).filter((opt) => opt.applies)

  const save = async () => {
    if (form.input.mode === 'custom' && !form.input.path?.trim()) {
      notify('err', '自定义输入目录还没填路径')
      return
    }
    if (form.output.mode === 'custom' && !form.output.path?.trim()) {
      notify('err', '自定义输出目录还没填路径')
      return
    }
    setBusy(true)
    try {
      const next = await api.instanceStorageUpdate({
        id: inst.id,
        useSharedModels: form.useSharedModels,
        disabledSharedModels: form.disabledSharedModels,
        extraModels: form.extraModels.filter((dir) => dir.path.trim()),
        input: { ...form.input, path: form.input.path?.trim() || null },
        output: { ...form.output, path: form.output.path?.trim() || null },
      })
      setForm(next.storage)
      setBaseline(next.storage)
      await loadPlan()
      notify('ok', '存储设置已保存，下次启动生效')
      onSaved()
    } catch (e) {
      notify('err', errText(e))
    } finally {
      setBusy(false)
    }
  }

  useTabSave(onSaveState, dirty, busy, save, () => {
    setForm(baseline)
    void loadPlan()
  })

  return (
    <div className="stack">
      <div className="panel">
        <div className="panel-head">
          <h2>模型目录</h2>
        </div>
        <div className="panel-body">
          <label className="check-row">
            <input
              type="checkbox"
              checked={form.useSharedModels}
              onChange={(e) => setForm({ ...form, useSharedModels: e.target.checked })}
            />
            <span>
              使用全局共享模型目录
              <span className="hint">
                {sharedOn.length > 0
                  ? `已挂上 ${sharedOn.length} 个`
                  : '全局没有共享目录'}
              </span>
            </span>
          </label>

          {form.useSharedModels && plan && plan.sharedModels.length > 0 && (
            <div className="share-pick">
              {plan.sharedModels.map((opt) => {
                const off = form.disabledSharedModels.some((path) => samePath(path, opt.path))
                return (
                  <label key={opt.path} className={`share-row${off ? ' off' : ''}`}>
                    <input
                      type="checkbox"
                      checked={!off}
                      onChange={() =>
                        setForm({
                          ...form,
                          disabledSharedModels: off
                            ? form.disabledSharedModels.filter((path) => !samePath(path, opt.path))
                            : [...form.disabledSharedModels, opt.path],
                        })
                      }
                    />
                    <span className="mono share-path" title={opt.path}>
                      {opt.path}
                    </span>
                    {opt.type ? (
                      <span className="badge ghost">{modelTypeLabel(opt.type)}</span>
                    ) : (
                      <span className="badge ghost">整包 models</span>
                    )}
                    {opt.isDefault && <span className="badge warn">下载落点</span>}
                    {!opt.exists && <span className="badge danger">不存在</span>}
                  </label>
                )
              })}
              {form.disabledSharedModels.length > 0 && (
                <span className="hint">
                  已关掉 {form.disabledSharedModels.length} 个，仅影响这个实例
                </span>
              )}
            </div>
          )}

          <div className="field" style={{ marginTop: 14 }}>
            <label>实例附加模型目录</label>
            <ModelDirList
              dirs={form.extraModels}
              onChange={(extraModels) => setForm({ ...form, extraModels })}
              onError={(msg) => notify('err', msg)}
            />
          </div>

        </div>
      </div>

      <div className="panel">
        <div className="panel-head">
          <h2>输入与输出</h2>
        </div>
        <div className="panel-body grid-2">
          <IoChoice
            label="输入目录"
            value={form.input}
            globalPath={plan?.sharedInput ?? null}
            onChange={(input) => setForm({ ...form, input })}
            notify={notify}
          />
          <IoChoice
            label="输出目录"
            value={form.output}
            globalPath={plan?.sharedOutput ?? null}
            onChange={(output) => setForm({ ...form, output })}
            notify={notify}
          />
        </div>
      </div>

      <div className="panel">
        <div className="panel-head">
          <h2>启动时实际生效</h2>
          <div className="spacer" />
          <button
            className="ghost sm"
            onClick={() => void loadPlan(form)}
            title="按当前草稿重新计算"
          >
            <Icon name="refresh" size={13} />
            刷新
          </button>
        </div>
        <div className="panel-body">
          {plan ? (
            <>
              <div className="eff-list">
                {plan.models.map((dir, index) => (
                  <EffectiveRow
                    key={`${dir.path}-${index}`}
                    label={`${SOURCE_LABEL[dir.source] ?? dir.source}${
                      dir.type ? `（${dir.type}）` : ''
                    }`}
                    path={dir.path}
                    source={dir.source}
                    exists={dir.exists}
                    isDefault={dir.isDefault}
                    notify={notify}
                  />
                ))}
                <EffectiveRow
                  label={`输入 · ${SOURCE_LABEL[plan.input.source] ?? plan.input.source}`}
                  path={plan.input.path}
                  source={plan.input.source}
                  exists={plan.input.exists}
                  notify={notify}
                />
                <EffectiveRow
                  label={`输出 · ${SOURCE_LABEL[plan.output.source] ?? plan.output.source}`}
                  path={plan.output.path}
                  source={plan.output.source}
                  exists={plan.output.exists}
                  notify={notify}
                />
              </div>

              {plan.warnings.length > 0 && (
                <div className="launch-warn" style={{ marginTop: 12 }}>
                  {plan.warnings.map((warning) => (
                    <div key={warning}>· {warning}</div>
                  ))}
                </div>
              )}

              <details className="eff-more">
                <summary>
                  启动参数与 yaml（{plan.extraArgs.length} 个参数）
                  <span className="faint"> · {plan.yamlPath}</span>
                </summary>
                <pre className="cmd-preview" style={{ marginTop: 10 }}>
                  {plan.extraArgs.length > 0
                    ? `main.py … ${plan.extraArgs.join(' ')}`
                    : '没有额外的目录参数，全部用默认位置'}
                </pre>
                <pre className="cmd-preview" style={{ marginTop: 8 }}>
                  {plan.yamlText || '（不需要 yaml：没有共享或附加的模型目录）'}
                </pre>
              </details>
            </>
          ) : (
            <div className="faint">计算中…</div>
          )}
        </div>
      </div>
    </div>
  )
}
